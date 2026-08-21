//! Telemetry platform crate (`plan/10-observability-ops.md`).
//!
//! Owns the `tracing` subscriber setup (JSON or pretty records, env filter,
//! redaction layer, bounded sizes, non-blocking writer), the span/field and
//! event-name conventions, the metrics registry with its Prometheus exporter,
//! and the redaction function used by every sink.
//!
//! Dependency direction: depends on `kevin-domain` and `kevin-config`; used by
//! every crate that logs or records metrics.
//!
//! ```no_run
//! use kevin_telemetry::{TelemetryConfig, events, fields};
//!
//! # #[tokio::main] async fn main() -> Result<(), kevin_telemetry::InitError> {
//! let cfg = TelemetryConfig::default();
//! let _guard = kevin_telemetry::init(&cfg)?; // keep alive for the process lifetime
//! tracing::info!({ fields::EVENT } = events::startup::READY, "ready");
//! # Ok(()) }
//! ```

pub mod config;
pub mod events;
pub mod fields;
pub mod layer;
pub mod metrics;
pub mod nonblocking;
#[cfg(feature = "otlp")]
pub mod otlp;
pub mod redact;
pub mod testing;

use std::io;
use std::net::SocketAddr;

use nonblocking::{ErrorCounter, NonBlocking, WorkerGuard};
use tokio::task::JoinHandle;
use tracing::{Level, Metadata, Subscriber};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{EnvFilter, Registry};

pub use config::{InstanceInfo, LogFormat, TelemetryConfig};
pub use layer::KevinLayer;
pub use metrics::{MetricsHandle, serve_metrics};
pub use redact::Redactor;

/// Errors from [`init`].
#[derive(Debug, thiserror::Error)]
pub enum InitError {
    /// `telemetry.log_level` is not a valid `EnvFilter` directive.
    #[error("invalid telemetry.log_level `{directive}`: {source}")]
    InvalidFilter {
        /// The offending directive.
        directive: String,
        /// Parser error.
        #[source]
        source: tracing_subscriber::filter::ParseError,
    },
    /// `telemetry.metrics_bind` is not a socket address.
    #[error("invalid telemetry.metrics_bind `{bind}`: {source}")]
    InvalidMetricsBind {
        /// The offending value.
        bind: String,
        /// Parser error.
        #[source]
        source: std::net::AddrParseError,
    },
    /// Binding the metrics listener failed.
    #[error("cannot bind metrics listener on {bind}: {source}")]
    MetricsBind {
        /// Address.
        bind: SocketAddr,
        /// IO error.
        #[source]
        source: io::Error,
    },
    /// `metrics_bind` is set but `init` was called outside a tokio runtime.
    #[error("telemetry.metrics_bind requires a tokio runtime to be entered before init")]
    NoRuntime,
    /// The Prometheus recorder could not be installed.
    #[error("metrics recorder: {0}")]
    Metrics(String),
    /// A global subscriber is already installed.
    #[error("telemetry already initialised")]
    AlreadyInitialized,
    /// OTLP requested but the crate was built without the `otlp` feature, or
    /// the exporter failed to build.
    #[error("otlp: {0}")]
    Otlp(String),
}

/// Keeps the telemetry pipeline alive: non-blocking writer threads, the
/// metrics listener task, the OTLP tracer provider. Drop it last, at process
/// exit (it flushes within a bounded time).
#[derive(Debug)]
pub struct Guard {
    _writers: Vec<WorkerGuard>,
    metrics: MetricsHandle,
    metrics_addr: Option<SocketAddr>,
    metrics_task: Option<JoinHandle<()>>,
    #[cfg(feature = "otlp")]
    tracer_provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
}

impl Guard {
    /// Handle to render the metrics registry (`GET /metrics`).
    #[must_use]
    pub fn metrics(&self) -> &MetricsHandle {
        &self.metrics
    }

    /// Address of the metrics listener when one was started.
    #[must_use]
    pub fn metrics_addr(&self) -> Option<SocketAddr> {
        self.metrics_addr
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(task) = self.metrics_task.take() {
            task.abort();
        }
        #[cfg(feature = "otlp")]
        if let Some(provider) = self.tracer_provider.take() {
            let _ = provider.shutdown();
        }
    }
}

/// Routes debug/trace records to a lossy non-blocking queue and info+ records
/// to a lossless one (both on the same sink), per plan/10.
#[derive(Debug, Clone)]
pub struct SplitWriter {
    lossy: NonBlocking,
    lossless: NonBlocking,
}

impl SplitWriter {
    /// Builds both queues over `sink` with `queue_lines` capacity each.
    pub fn new<T: io::Write + Send + 'static>(
        sink_factory: impl Fn() -> T,
        queue_lines: usize,
    ) -> (Self, Vec<WorkerGuard>) {
        let (lossy, g1) = NonBlocking::new(sink_factory(), queue_lines, true, "kevin-log-debug");
        let (lossless, g2) = NonBlocking::new(sink_factory(), queue_lines, false, "kevin-log");
        (Self { lossy, lossless }, vec![g1, g2])
    }

    /// Dropped-line counter of the lossy queue.
    #[must_use]
    pub fn lossy_counter(&self) -> ErrorCounter {
        self.lossy.error_counter()
    }
}

impl<'a> MakeWriter<'a> for SplitWriter {
    type Writer = NonBlocking;

    fn make_writer(&'a self) -> Self::Writer {
        self.lossless.clone()
    }

    fn make_writer_for(&'a self, meta: &Metadata<'_>) -> Self::Writer {
        if *meta.level() >= Level::DEBUG {
            self.lossy.clone()
        } else {
            self.lossless.clone()
        }
    }
}

/// The env filter for `cfg`: `RUST_LOG` wins when set and valid, otherwise
/// `telemetry.log_level`.
pub fn env_filter(cfg: &TelemetryConfig) -> Result<EnvFilter, InitError> {
    if let Ok(filter) = EnvFilter::try_from_default_env() {
        return Ok(filter);
    }
    EnvFilter::try_new(&cfg.log_level).map_err(|source| InitError::InvalidFilter {
        directive: cfg.log_level.clone(),
        source,
    })
}

/// Builds the subscriber (registry + env filter + [`KevinLayer`]) writing to
/// `writer`, without installing it. Used by [`init`] and by tests that want to
/// capture records (`tracing::subscriber::with_default`).
pub fn build_subscriber<W>(
    cfg: &TelemetryConfig,
    writer: W,
) -> Result<impl Subscriber + Send + Sync + use<W>, InitError>
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    let layer = KevinLayer::new(
        writer,
        cfg.log_format,
        cfg.instance.clone(),
        Redactor::global().clone(),
    );
    Ok(Registry::default().with(env_filter(cfg)?).with(layer))
}

/// Initialises logging, metrics and (optionally) OTLP from `cfg`; returns the
/// [`Guard`] that must live as long as the process.
///
/// - Records go to stdout, JSON or pretty, through bounded non-blocking queues.
/// - The Prometheus recorder is always installed (so `metrics::counter!` works);
///   a listener is started on `metrics_bind` when non-empty (requires a tokio
///   runtime; the bind happens synchronously so port conflicts fail startup).
/// - `kevin_build_info{version,commit,profile} = 1` is emitted.
pub fn init(cfg: &TelemetryConfig) -> Result<Guard, InitError> {
    let filter = env_filter(cfg)?;
    let metrics_addr = cfg
        .metrics_addr()
        .map_err(|source| InitError::InvalidMetricsBind {
            bind: cfg.metrics_bind.clone(),
            source,
        })?;
    let handle = metrics::install().map_err(|e| InitError::Metrics(e.to_string()))?;

    let (split, guards) = SplitWriter::new(io::stdout, cfg.writer_queue_lines.max(1));
    let lossy_counter = split.lossy_counter();
    let layer = KevinLayer::new(
        split,
        cfg.log_format,
        cfg.instance.clone(),
        Redactor::global().clone(),
    )
    .with_lossy_counter(lossy_counter);

    #[cfg(feature = "otlp")]
    let (otlp_layer, tracer_provider) = if cfg.otlp_endpoint.trim().is_empty() {
        (None, None)
    } else {
        let (layer, provider) = otlp::layer(cfg.otlp_endpoint.trim(), &cfg.instance)?;
        (Some(layer), Some(provider))
    };
    #[cfg(not(feature = "otlp"))]
    if !cfg.otlp_endpoint.trim().is_empty() {
        return Err(InitError::Otlp(
            "telemetry.otlp_endpoint set but kevin-telemetry was built without the `otlp` feature"
                .to_owned(),
        ));
    }

    let subscriber = Registry::default().with(filter).with(layer);
    #[cfg(feature = "otlp")]
    let subscriber = subscriber.with(otlp_layer);
    tracing::subscriber::set_global_default(subscriber)
        .map_err(|_| InitError::AlreadyInitialized)?;

    let (metrics_addr, metrics_task) = match metrics_addr {
        None => (None, None),
        Some(bind) => {
            let runtime =
                tokio::runtime::Handle::try_current().map_err(|_| InitError::NoRuntime)?;
            let listener = std::net::TcpListener::bind(bind)
                .map_err(|source| InitError::MetricsBind { bind, source })?;
            listener
                .set_nonblocking(true)
                .map_err(|source| InitError::MetricsBind { bind, source })?;
            let local = listener
                .local_addr()
                .map_err(|source| InitError::MetricsBind { bind, source })?;
            let handle = handle.clone();
            let task = runtime.spawn(async move {
                match tokio::net::TcpListener::from_std(listener) {
                    Ok(listener) => metrics::serve_on(listener, handle).await,
                    Err(err) => tracing::error!(error = %err, "metrics listener failed"),
                }
            });
            (Some(local), Some(task))
        }
    };

    ::metrics::gauge!(
        metrics::BUILD_INFO,
        "version" => cfg.instance.version.clone(),
        "commit" => option_env!("KEVIN_COMMIT").unwrap_or("unknown"),
        "profile" => cfg.instance.profile.clone(),
    )
    .set(1.0);

    Ok(Guard {
        _writers: guards,
        metrics: handle,
        metrics_addr,
        metrics_task,
        #[cfg(feature = "otlp")]
        tracer_provider,
    })
}
