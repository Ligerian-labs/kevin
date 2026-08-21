//! Telemetry configuration accepted by [`crate::init`].
//!
//! Mirrors the `[telemetry]` table of `plan/03-config-schema.md`
//! (`log_format`, `log_level`, `metrics_bind`, `otlp_endpoint`) plus the
//! instance identity every record carries (`service`, `version`, `instance`,
//! `profile`). `kevin-config` (WS-02) converts its `Telemetry` sub-struct into
//! this type; until it lands this is the only input.

use std::net::SocketAddr;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// `telemetry.log_format`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogFormat {
    /// One JSON object per line on stdout (default for daemons / Kohral).
    #[default]
    Json,
    /// Human-readable single-line records (default for a tty / `laptop` profile).
    Pretty,
}

impl FromStr for LogFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "json" => Ok(LogFormat::Json),
            "pretty" => Ok(LogFormat::Pretty),
            other => Err(format!(
                "unknown log_format `{other}` (expected json | pretty)"
            )),
        }
    }
}

/// Identity stamped on every record: `service`, `version`, `instance`, `profile`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceInfo {
    /// Always `kevin` unless a sidecar binary sets otherwise.
    pub service: String,
    /// `CARGO_PKG_VERSION` of the binary.
    pub version: String,
    /// `kevin.instance_name`.
    pub instance: String,
    /// `profile` (`laptop` | `server` | `kohral`).
    pub profile: String,
}

impl Default for InstanceInfo {
    fn default() -> Self {
        Self {
            service: "kevin".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            instance: "kevin".to_owned(),
            profile: "laptop".to_owned(),
        }
    }
}

/// Input of [`crate::init`]. Field names follow `[telemetry]` in plan/03.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TelemetryConfig {
    /// `json` | `pretty`.
    pub log_format: LogFormat,
    /// `tracing_subscriber::EnvFilter` directive(s); `RUST_LOG` overrides it.
    pub log_level: String,
    /// Prometheus listener address; empty disables the exporter.
    pub metrics_bind: String,
    /// OTLP gRPC endpoint; empty disables trace export (needs the `otlp` feature).
    pub otlp_endpoint: String,
    /// Identity fields stamped on every record.
    pub instance: InstanceInfo,
    /// Capacity of the non-blocking writer queue (lines); default 65 536.
    pub writer_queue_lines: usize,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            log_format: LogFormat::Json,
            log_level: "info".to_owned(),
            metrics_bind: String::new(),
            otlp_endpoint: String::new(),
            instance: InstanceInfo::default(),
            writer_queue_lines: 65_536,
        }
    }
}

impl TelemetryConfig {
    /// Parsed `metrics_bind`, `None` when empty.
    pub fn metrics_addr(&self) -> Result<Option<SocketAddr>, std::net::AddrParseError> {
        if self.metrics_bind.trim().is_empty() {
            return Ok(None);
        }
        self.metrics_bind.trim().parse().map(Some)
    }
}

impl From<kevin_config::LogFormat> for LogFormat {
    fn from(value: kevin_config::LogFormat) -> Self {
        match value {
            kevin_config::LogFormat::Json => LogFormat::Json,
            kevin_config::LogFormat::Pretty => LogFormat::Pretty,
        }
    }
}

impl From<&kevin_config::Telemetry> for TelemetryConfig {
    /// `[telemetry]` only; instance identity stays at its defaults (use
    /// [`TelemetryConfig::from_kevin_config`] for the full picture).
    fn from(value: &kevin_config::Telemetry) -> Self {
        Self {
            log_format: value.log_format.into(),
            log_level: value.log_level.clone(),
            metrics_bind: value.metrics_bind.clone(),
            otlp_endpoint: value.otlp_endpoint.clone(),
            ..Self::default()
        }
    }
}

impl TelemetryConfig {
    /// `[telemetry]` plus `kevin.instance_name` and `profile` from the whole config.
    #[must_use]
    pub fn from_kevin_config(cfg: &kevin_config::KevinConfig) -> Self {
        Self {
            instance: InstanceInfo {
                instance: cfg.kevin.instance_name.clone(),
                profile: cfg.kevin.profile.as_str().to_owned(),
                ..InstanceInfo::default()
            },
            ..Self::from(&cfg.telemetry)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_from_kevin_config() {
        let mut cfg = kevin_config::KevinConfig::default();
        cfg.telemetry.log_format = kevin_config::LogFormat::Pretty;
        cfg.telemetry.metrics_bind = "127.0.0.1:9464".into();
        cfg.kevin.instance_name = "box-1".into();
        let t = TelemetryConfig::from_kevin_config(&cfg);
        assert_eq!(t.log_format, LogFormat::Pretty);
        assert_eq!(t.metrics_addr().unwrap().unwrap().port(), 9464);
        assert_eq!(t.instance.instance, "box-1");
        assert_eq!(t.instance.profile, "laptop");
        assert_eq!(t.log_level, "info");
    }
}
