//! Telemetry platform crate (`plan/10-observability-ops.md`).
//!
//! Owns the `tracing` subscriber setup (JSON logs, env filter, redaction
//! layer), the metrics registry and `/metrics` exporter wiring, correlation-id
//! propagation helpers and span field conventions.
//!
//! Dependency direction: depends on `kevin-domain` and `kevin-config`; used by
//! every crate that logs or records metrics. Implemented by WS-04.
