//! The Kevin `tracing` layer: redaction + bounded sizes + JSON / pretty output.
//!
//! One layer does three things the plan requires of every record: it masks
//! secrets in every field and message **before** formatting (see
//! [`crate::redact`]), it caps field and record sizes, and it emits the stable
//! envelope (`ts`, `level`, `event`, `service`, `version`, `instance`,
//! `profile`, span fields, event fields). Span fields are captured when spans
//! are created (`on_new_span`/`on_record`) and flattened into every record
//! emitted inside them, including from `tokio::spawn`ed tasks instrumented
//! with the span.

use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::Arc;

use crate::nonblocking::ErrorCounter;
use chrono::{SecondsFormat, Utc};
use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Metadata, Subscriber};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

use crate::config::{InstanceInfo, LogFormat};
use crate::metrics::TELEMETRY_DROPPED_RECORDS_TOTAL;
use crate::redact::{FIELD_CAP_BYTES, RECORD_CAP_BYTES, Redactor, STACK_TRACE_CAP_BYTES, truncate};

/// Field name that carries the stable event name (`kevin.run.started`).
const EVENT_FIELD: &str = "event";
/// tracing's name for the formatted message.
const MESSAGE_FIELD: &str = "message";
/// When a record exceeds [`RECORD_CAP_BYTES`], string fields are re-capped to this.
const OVERFLOW_FIELD_CAP: usize = 1024;

/// Redacted span fields kept in the span's extensions.
#[derive(Debug, Default, Clone)]
pub struct SpanFields(pub Vec<(&'static str, Value)>);

/// The layer. Build it with [`KevinLayer::new`] and stack it on a
/// `tracing_subscriber::Registry` (usually through [`crate::build_subscriber`]).
pub struct KevinLayer<W> {
    make_writer: W,
    format: LogFormat,
    info: Arc<InstanceInfo>,
    redactor: Redactor,
    lossy_counter: Option<ErrorCounter>,
}

impl<W> std::fmt::Debug for KevinLayer<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KevinLayer")
            .field("format", &self.format)
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

impl<W> KevinLayer<W> {
    /// A layer writing `format` records to `make_writer`, stamped with `info`.
    pub fn new(make_writer: W, format: LogFormat, info: InstanceInfo, redactor: Redactor) -> Self {
        Self {
            make_writer,
            format,
            info: Arc::new(info),
            redactor,
            lossy_counter: None,
        }
    }

    /// Exports `kevin_telemetry_dropped_records_total{level="debug"}` from the
    /// error counter of the lossy (debug/trace) non-blocking writer.
    #[must_use]
    pub fn with_lossy_counter(mut self, counter: ErrorCounter) -> Self {
        self.lossy_counter = Some(counter);
        self
    }

    fn collect_span_fields<S>(
        event: &Event<'_>,
        ctx: &Context<'_, S>,
    ) -> (Vec<(&'static str, Value)>, Option<&'static str>)
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        let mut fields: Vec<(&'static str, Value)> = Vec::new();
        let mut innermost = None;
        if let Some(scope) = ctx.event_scope(event) {
            for span in scope.from_root() {
                innermost = Some(span.name());
                if let Some(stored) = span.extensions().get::<SpanFields>() {
                    for (k, v) in &stored.0 {
                        upsert(&mut fields, k, v.clone());
                    }
                }
            }
        }
        (fields, innermost)
    }

    fn render(
        &self,
        meta: &Metadata<'_>,
        span_name: Option<&str>,
        span_fields: &[(&'static str, Value)],
        event_fields: &[(&'static str, Value)],
    ) -> String {
        let ts = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        match self.format {
            LogFormat::Json => self.render_json(&ts, meta, span_name, span_fields, event_fields),
            LogFormat::Pretty => render_pretty(&ts, meta, span_name, span_fields, event_fields),
        }
    }

    fn render_json(
        &self,
        ts: &str,
        meta: &Metadata<'_>,
        span_name: Option<&str>,
        span_fields: &[(&'static str, Value)],
        event_fields: &[(&'static str, Value)],
    ) -> String {
        let mut map = Map::new();
        map.insert("ts".into(), Value::String(ts.to_owned()));
        map.insert(
            "level".into(),
            Value::String(level_name(*meta.level()).to_owned()),
        );
        map.insert("service".into(), Value::String(self.info.service.clone()));
        map.insert("version".into(), Value::String(self.info.version.clone()));
        map.insert("instance".into(), Value::String(self.info.instance.clone()));
        map.insert("profile".into(), Value::String(self.info.profile.clone()));
        map.insert("target".into(), Value::String(meta.target().to_owned()));
        if let Some(name) = span_name {
            map.insert("span".into(), Value::String(name.to_owned()));
        }
        for (k, v) in span_fields.iter().chain(event_fields) {
            map.insert((*k).to_owned(), v.clone());
        }
        if !map.contains_key(EVENT_FIELD) {
            map.insert(EVENT_FIELD.into(), Value::String(meta.target().to_owned()));
        }
        let mut line = Value::Object(map).to_string();
        if line.len() > RECORD_CAP_BYTES {
            let overflow = line.len() - RECORD_CAP_BYTES;
            let Value::Object(mut map) =
                serde_json::from_str::<Value>(&line).unwrap_or(Value::Null)
            else {
                return line;
            };
            for value in map.values_mut() {
                if let Value::String(s) = value
                    && s.len() > OVERFLOW_FIELD_CAP
                {
                    *s = truncate(s, OVERFLOW_FIELD_CAP);
                }
            }
            map.insert("truncated_bytes".into(), Value::from(overflow));
            line = Value::Object(map).to_string();
        }
        line.push('\n');
        line
    }
}

fn render_pretty(
    ts: &str,
    meta: &Metadata<'_>,
    span_name: Option<&str>,
    span_fields: &[(&'static str, Value)],
    event_fields: &[(&'static str, Value)],
) -> String {
    let mut line = String::new();
    let event = event_fields
        .iter()
        .find(|(k, _)| *k == EVENT_FIELD)
        .and_then(|(_, v)| v.as_str())
        .unwrap_or(meta.target());
    let _ = write!(line, "{ts} {:>5} {event}", level_name(*meta.level()));
    if let Some(name) = span_name {
        let _ = write!(line, " [{name}]");
    }
    if let Some((_, msg)) = event_fields.iter().find(|(k, _)| *k == MESSAGE_FIELD) {
        let _ = write!(
            line,
            ": {}",
            msg.as_str().map_or_else(|| msg.to_string(), str::to_owned)
        );
    }
    for (k, v) in span_fields.iter().chain(event_fields) {
        if *k == EVENT_FIELD || *k == MESSAGE_FIELD {
            continue;
        }
        let _ = write!(line, " {k}={v}");
    }
    if line.len() > RECORD_CAP_BYTES {
        line = truncate(&line, RECORD_CAP_BYTES);
    }
    line.push('\n');
    line
}

fn level_name(level: Level) -> &'static str {
    match level {
        Level::TRACE => "trace",
        Level::DEBUG => "debug",
        Level::INFO => "info",
        Level::WARN => "warn",
        Level::ERROR => "error",
    }
}

fn upsert(fields: &mut Vec<(&'static str, Value)>, key: &'static str, value: Value) {
    if let Some(slot) = fields.iter_mut().find(|(k, _)| *k == key) {
        slot.1 = value;
    } else {
        fields.push((key, value));
    }
}

impl<S, W> Layer<S> for KevinLayer<W>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    W: for<'w> MakeWriter<'w> + 'static,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut fields = SpanFields::default();
        attrs.record(&mut FieldVisitor::new(&self.redactor, &mut fields.0));
        span.extensions_mut().insert(fields);
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut ext = span.extensions_mut();
        let fields = ext.get_mut::<SpanFields>();
        if let Some(fields) = fields {
            values.record(&mut FieldVisitor::new(&self.redactor, &mut fields.0));
        } else {
            let mut fresh = SpanFields::default();
            values.record(&mut FieldVisitor::new(&self.redactor, &mut fresh.0));
            ext.insert(fresh);
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let meta = event.metadata();
        let (span_fields, span_name) = Self::collect_span_fields(event, &ctx);
        let mut event_fields = Vec::new();
        event.record(&mut FieldVisitor::new(&self.redactor, &mut event_fields));
        let line = self.render(meta, span_name, &span_fields, &event_fields);
        let mut writer = self.make_writer.make_writer_for(meta);
        let _ = writer.write_all(line.as_bytes());
        if let Some(counter) = &self.lossy_counter
            && *meta.level() >= Level::DEBUG
        {
            metrics::counter!(TELEMETRY_DROPPED_RECORDS_TOTAL, "level" => "debug")
                .absolute(counter.dropped_lines());
        }
    }
}

/// Visits fields, redacting and capping string-like values.
struct FieldVisitor<'a> {
    redactor: &'a Redactor,
    out: &'a mut Vec<(&'static str, Value)>,
}

impl<'a> FieldVisitor<'a> {
    fn new(redactor: &'a Redactor, out: &'a mut Vec<(&'static str, Value)>) -> Self {
        Self { redactor, out }
    }

    fn push(&mut self, field: &Field, value: Value) {
        upsert(self.out, field.name(), value);
    }

    fn text(&self, text: &str, cap: usize) -> Value {
        Value::String(truncate(&self.redactor.redact_str(text), cap))
    }
}

impl Visit for FieldVisitor<'_> {
    fn record_f64(&mut self, field: &Field, value: f64) {
        let v = serde_json::Number::from_f64(value)
            .map_or_else(|| Value::String(value.to_string()), Value::Number);
        self.push(field, v);
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.push(field, Value::from(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.push(field, Value::from(value));
    }

    fn record_i128(&mut self, field: &Field, value: i128) {
        self.push(field, Value::String(value.to_string()));
    }

    fn record_u128(&mut self, field: &Field, value: u128) {
        self.push(field, Value::String(value.to_string()));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.push(field, Value::Bool(value));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        let v = self.text(value, FIELD_CAP_BYTES);
        self.push(field, v);
    }

    fn record_bytes(&mut self, field: &Field, value: &[u8]) {
        self.push(field, Value::String(format!("<{} bytes>", value.len())));
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        let mut chain = value.to_string();
        let mut source = value.source();
        while let Some(err) = source {
            let _ = write!(chain, ": {err}");
            source = err.source();
        }
        let v = self.text(&chain, STACK_TRACE_CAP_BYTES);
        self.push(field, v);
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let v = self.text(&format!("{value:?}"), FIELD_CAP_BYTES);
        self.push(field, v);
    }
}
