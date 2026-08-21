//! Optional OTLP trace export (`telemetry.otlp_endpoint`), behind the `otlp`
//! feature. Spans carry the same fields as the logs; sampling is
//! `parent-based, ratio 1.0` (plan/10).

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig as _;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};
use tracing::Subscriber;
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::registry::LookupSpan;

use crate::InitError;
use crate::config::InstanceInfo;

/// Builds the OTLP exporter + tracer provider and the tracing layer on top.
pub fn layer<S>(
    endpoint: &str,
    info: &InstanceInfo,
) -> Result<
    (
        OpenTelemetryLayer<S, opentelemetry_sdk::trace::Tracer>,
        SdkTracerProvider,
    ),
    InitError,
>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .map_err(|e| InitError::Otlp(e.to_string()))?;
    let resource = Resource::builder()
        .with_service_name(info.service.clone())
        .with_attributes([
            opentelemetry::KeyValue::new("service.version", info.version.clone()),
            opentelemetry::KeyValue::new("service.instance.id", info.instance.clone()),
            opentelemetry::KeyValue::new("deployment.environment.name", info.profile.clone()),
        ])
        .build();
    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_sampler(Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
            1.0,
        ))))
        .with_resource(resource)
        .build();
    let tracer = provider.tracer(info.service.clone());
    Ok((tracing_opentelemetry::layer().with_tracer(tracer), provider))
}
