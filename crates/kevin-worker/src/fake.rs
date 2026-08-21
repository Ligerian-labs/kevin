//! The in-process `fake` worker (`plan/04-workers.md` §Adapter: `fake`).
//!
//! Driven by a scenario (`workers.fake.script`, YAML or JSON):
//!
//! ```yaml
//! default: { reply: "done", usage: { input_tokens: 10, output_tokens: 5 } }
//! rules:
//!   - match: "reply deterministically"      # substring or /regex/
//!     reply: "kohral-ok"                     # Kohral conformance basic phase
//!   - match: "[[KOHRAL_HOLD]]"
//!     hold: true                             # emits Started then waits until cancelled
//!   - match: /implement .* auth/
//!     events: [ {tool_call: {name: edit, input_summary: "src/auth.rs"}}, {text: "Added auth"} ]
//!     structured: { status: "ok" }
//!     delay_ms: 50
//!   - match: "fail transient"
//!     fail: { class: transient, message: "simulated 429" }
//! ```
//!
//! First matching rule wins; `default` otherwise. The fake honours
//! cancellation and timeouts exactly like real workers, so orchestrator tests
//! and the Kohral conformance suite need no model. Without a script the
//! [`Scenario::builtin`] scenario applies, which carries the two Kohral
//! conformance hooks (`plan/08-kohral-runtime.md` §1.9).

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use kevin_domain::{FailureClass, ModelAlias, WorkerKind};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::structured;
use crate::supervisor::{sha256_hex, transcript_path};
use crate::types::{ArtifactKind, ArtifactRef, ConfigError, ModelEntry, TaskAttemptRequest, Usage};
use crate::worker::{
    AuthStatus, Doctor, EventSink, Worker, WorkerError, WorkerEvent, WorkerHandle, WorkerOutcome,
    WorkerSessionId,
};

/// Kohral conformance hook: this input must complete with output exactly `kohral-ok`.
pub const KOHRAL_REPLY_INPUT: &str = "reply deterministically";
/// Kohral conformance hook: the deterministic reply.
pub const KOHRAL_REPLY_OUTPUT: &str = "kohral-ok";
/// Kohral conformance hook: this input makes the worker hang until cancelled/killed.
pub const KOHRAL_HOLD_INPUT: &str = "[[KOHRAL_HOLD]]";

/// How a rule matches the prompt: substring, or `/regex/`.
#[derive(Clone, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Matcher {
    raw: String,
    #[serde(skip)]
    regex: Option<regex::Regex>,
}

impl Matcher {
    /// Parses `"text"` (substring) or `"/pattern/"` (regex).
    pub fn parse(raw: impl Into<String>) -> Result<Self, ScenarioError> {
        let raw = raw.into();
        let regex =
            match raw.strip_prefix('/').and_then(|r| r.strip_suffix('/')) {
                Some(pattern) if raw.len() >= 2 => Some(regex::Regex::new(pattern).map_err(
                    |e| ScenarioError::InvalidRegex {
                        pattern: pattern.to_owned(),
                        message: e.to_string(),
                    },
                )?),
                _ => None,
            };
        Ok(Self { raw, regex })
    }

    /// Substring matcher.
    pub fn substring(text: impl Into<String>) -> Self {
        Self {
            raw: text.into(),
            regex: None,
        }
    }

    /// The raw form (`text` or `/pattern/`).
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// Whether `text` matches.
    #[must_use]
    pub fn matches(&self, text: &str) -> bool {
        match &self.regex {
            Some(re) => re.is_match(text),
            None => text.contains(&self.raw),
        }
    }
}

impl fmt::Debug for Matcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Matcher").field(&self.raw).finish()
    }
}

impl PartialEq for Matcher {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}

impl TryFrom<String> for Matcher {
    type Error = ScenarioError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<Matcher> for String {
    fn from(m: Matcher) -> String {
        m.raw
    }
}

/// A scripted failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailSpec {
    /// Failure class.
    pub class: FailureClass,
    /// Message.
    #[serde(default)]
    pub message: String,
}

/// One scripted stream event emitted before the terminal event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptedEvent {
    /// `AssistantText`.
    Text(String),
    /// `Thinking`.
    Thinking(String),
    /// `ToolCall`.
    ToolCall {
        /// Tool name.
        name: String,
        /// Input summary.
        #[serde(default)]
        input_summary: String,
    },
    /// `ToolResult`.
    ToolResult {
        /// Tool name.
        name: String,
        /// Ok flag.
        #[serde(default = "default_true")]
        ok: bool,
        /// Output summary.
        #[serde(default)]
        output_summary: String,
    },
    /// `Usage` delta.
    Usage(Usage),
    /// `InputRequested`.
    InputRequested {
        /// Question.
        question: String,
        /// Options.
        #[serde(default)]
        options: Vec<String>,
    },
}

fn default_true() -> bool {
    true
}

impl ScriptedEvent {
    fn to_worker_event(&self) -> WorkerEvent {
        match self.clone() {
            ScriptedEvent::Text(delta) => WorkerEvent::AssistantText { delta },
            ScriptedEvent::Thinking(delta) => WorkerEvent::Thinking { delta },
            ScriptedEvent::ToolCall {
                name,
                input_summary,
            } => WorkerEvent::ToolCall {
                name,
                input_summary,
            },
            ScriptedEvent::ToolResult {
                name,
                ok,
                output_summary,
            } => WorkerEvent::ToolResult {
                name,
                ok,
                output_summary,
            },
            ScriptedEvent::Usage(delta) => WorkerEvent::Usage { delta },
            ScriptedEvent::InputRequested { question, options } => {
                WorkerEvent::InputRequested { question, options }
            }
        }
    }
}

/// One scenario rule (also the shape of `default`, whose `match` is ignored).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rule {
    /// Substring or `/regex/` matched against the prompt (title + instructions).
    #[serde(rename = "match", skip_serializing_if = "Option::is_none")]
    pub r#match: Option<Matcher>,
    /// Final answer text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply: Option<String>,
    /// Emit `Started` then wait until cancelled (or the attempt times out).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub hold: bool,
    /// Fail instead of replying.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fail: Option<FailSpec>,
    /// Stream events emitted before the terminal event.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<ScriptedEvent>,
    /// Structured output attached to `Final`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured: Option<Value>,
    /// Usage reported on the terminal event (default: sum of scripted `usage` events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Delay before the events start (cancellable).
    #[serde(skip_serializing_if = "is_zero")]
    pub delay_ms: u64,
    /// Session id to report (default `fake-<attempt_id>`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(n: &u64) -> bool {
    *n == 0
}

impl Rule {
    /// A rule matching `matcher` (substring or `/regex/`).
    pub fn matching(matcher: impl Into<String>) -> Self {
        let raw = matcher.into();
        Self {
            r#match: Some(Matcher::parse(raw.clone()).unwrap_or_else(|_| Matcher::substring(raw))),
            ..Self::default()
        }
    }

    /// A rule that replies `text` (no matcher; use as `default`).
    pub fn replying(text: impl Into<String>) -> Self {
        Self::default().reply(text)
    }

    /// Sets the reply.
    #[must_use]
    pub fn reply(mut self, text: impl Into<String>) -> Self {
        self.reply = Some(text.into());
        self
    }

    /// Makes the rule hold.
    #[must_use]
    pub fn hold(mut self) -> Self {
        self.hold = true;
        self
    }

    /// Makes the rule fail.
    #[must_use]
    pub fn fail(mut self, class: FailureClass, message: impl Into<String>) -> Self {
        self.fail = Some(FailSpec {
            class,
            message: message.into(),
        });
        self
    }

    /// Appends a scripted text event.
    #[must_use]
    pub fn text(mut self, delta: impl Into<String>) -> Self {
        self.events.push(ScriptedEvent::Text(delta.into()));
        self
    }

    /// Appends a scripted tool call.
    #[must_use]
    pub fn tool_call(mut self, name: impl Into<String>, input_summary: impl Into<String>) -> Self {
        self.events.push(ScriptedEvent::ToolCall {
            name: name.into(),
            input_summary: input_summary.into(),
        });
        self
    }

    /// Appends a scripted event.
    #[must_use]
    pub fn event(mut self, event: ScriptedEvent) -> Self {
        self.events.push(event);
        self
    }

    /// Sets the structured output.
    #[must_use]
    pub fn structured(mut self, value: Value) -> Self {
        self.structured = Some(value);
        self
    }

    /// Sets the reported usage.
    #[must_use]
    pub fn usage(mut self, usage: Usage) -> Self {
        self.usage = Some(usage);
        self
    }

    /// Sets the delay.
    #[must_use]
    pub fn delay_ms(mut self, ms: u64) -> Self {
        self.delay_ms = ms;
        self
    }

    /// Sets the session id.
    #[must_use]
    pub fn session_id(mut self, id: impl Into<String>) -> Self {
        self.session_id = Some(id.into());
        self
    }
}

/// A fake-worker scenario.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Scenario {
    /// Applied when no rule matches.
    pub default: Rule,
    /// Tried in order; first match wins.
    pub rules: Vec<Rule>,
}

impl Default for Scenario {
    fn default() -> Self {
        Self::builtin()
    }
}

impl Scenario {
    /// The scenario used when `workers.fake.script` is empty: replies `done`,
    /// plus the Kohral conformance hooks.
    #[must_use]
    pub fn builtin() -> Self {
        Self {
            default: Rule::replying("done").usage(Usage::tokens(10, 5)),
            rules: vec![
                Rule::matching(KOHRAL_REPLY_INPUT).reply(KOHRAL_REPLY_OUTPUT),
                Rule::matching(KOHRAL_HOLD_INPUT).hold(),
            ],
        }
    }

    /// An empty scenario replying `text` to everything.
    pub fn replying(text: impl Into<String>) -> Self {
        Self {
            default: Rule::replying(text),
            rules: Vec::new(),
        }
    }

    /// Appends a rule.
    #[must_use]
    pub fn rule(mut self, rule: Rule) -> Self {
        self.rules.push(rule);
        self
    }

    /// Replaces the default rule.
    #[must_use]
    pub fn with_default(mut self, rule: Rule) -> Self {
        self.default = rule;
        self
    }

    /// Parses YAML (JSON is accepted too).
    pub fn from_yaml(text: &str) -> Result<Self, ScenarioError> {
        serde_saphyr::from_str(text).map_err(|e| ScenarioError::Parse {
            message: e.to_string(),
        })
    }

    /// Parses JSON.
    pub fn from_json(text: &str) -> Result<Self, ScenarioError> {
        serde_json::from_str(text).map_err(|e| ScenarioError::Parse {
            message: e.to_string(),
        })
    }

    /// Loads a `.yaml`/`.yml`/`.json` file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ScenarioError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|e| ScenarioError::Io {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })?;
        match path.extension().and_then(|e| e.to_str()) {
            Some("json") => Self::from_json(&text),
            _ => Self::from_yaml(&text),
        }
    }

    /// Serialises to YAML.
    pub fn to_yaml(&self) -> Result<String, ScenarioError> {
        serde_saphyr::to_string(self).map_err(|e| ScenarioError::Parse {
            message: e.to_string(),
        })
    }

    /// The rule for `prompt`: first match, else `default`.
    #[must_use]
    pub fn select(&self, prompt: &str) -> &Rule {
        self.rules
            .iter()
            .find(|r| r.r#match.as_ref().is_some_and(|m| m.matches(prompt)))
            .unwrap_or(&self.default)
    }
}

/// Scenario loading errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScenarioError {
    /// Could not read the file.
    #[error("cannot read scenario {path:?}: {reason}")]
    Io {
        /// Path.
        path: PathBuf,
        /// Error text.
        reason: String,
    },
    /// Could not parse the document.
    #[error("invalid scenario: {message}")]
    Parse {
        /// Parser message.
        message: String,
    },
    /// A `/regex/` matcher does not compile.
    #[error("invalid regex matcher /{pattern}/: {message}")]
    InvalidRegex {
        /// Pattern.
        pattern: String,
        /// Compiler message.
        message: String,
    },
}

/// The in-process fake worker.
#[derive(Debug, Clone)]
pub struct FakeWorker {
    scenario: Arc<Scenario>,
    data_dir: PathBuf,
}

impl FakeWorker {
    /// A fake worker replaying `scenario`, writing transcripts under `data_dir`.
    pub fn new(scenario: Scenario, data_dir: impl Into<PathBuf>) -> Self {
        Self {
            scenario: Arc::new(scenario),
            data_dir: data_dir.into(),
        }
    }

    /// The built-in scenario.
    pub fn builtin(data_dir: impl Into<PathBuf>) -> Self {
        Self::new(Scenario::builtin(), data_dir)
    }

    /// The scenario in use.
    #[must_use]
    pub fn scenario(&self) -> &Scenario {
        &self.scenario
    }

    /// Transcript root.
    #[must_use]
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }
}

#[async_trait]
impl Worker for FakeWorker {
    fn kind(&self) -> WorkerKind {
        WorkerKind::Fake
    }

    async fn doctor(&self) -> Doctor {
        Doctor {
            kind: WorkerKind::Fake,
            binary: std::env::current_exe().ok(),
            version: Some(format!("in-process {}", env!("CARGO_PKG_VERSION"))),
            auth_ready: AuthStatus::Ready,
            notes: vec![format!(
                "scenario: {} rule(s) + default",
                self.scenario.rules.len()
            )],
        }
    }

    fn validate_alias(&self, alias: &ModelAlias, entry: &ModelEntry) -> Result<(), ConfigError> {
        if entry.worker == WorkerKind::Fake {
            Ok(())
        } else {
            Err(ConfigError::new(
                format!("models.{alias}.worker"),
                format!("expected `fake`, found `{}`", entry.worker),
            ))
        }
    }

    async fn start(&self, req: TaskAttemptRequest) -> Result<WorkerHandle, WorkerError> {
        let rule = self.scenario.select(&req.prompt_text()).clone();
        let path = transcript_path(&self.data_dir, &req.run_id, &req.task_id, &req.attempt_id);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| WorkerError::io(format!("creating {}", parent.display()), e))?;
        }
        let cancel = req.cancel.clone();
        Ok(WorkerHandle::spawn(WorkerKind::Fake, cancel, move |sink| {
            run_script(rule, req, sink, path)
        }))
    }
}

enum Pause {
    Done,
    Cancelled,
    Timeout,
}

async fn pause(delay: Duration, cancel: &CancellationToken, deadline: Instant) -> Pause {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Pause::Cancelled,
        () = tokio::time::sleep_until(deadline.into()) => Pause::Timeout,
        () = tokio::time::sleep(delay) => Pause::Done,
    }
}

struct Transcript {
    path: PathBuf,
    lines: Vec<String>,
}

impl Transcript {
    fn record(&mut self, event: &WorkerEvent) {
        let record = serde_json::json!({
            "ts": Utc::now().to_rfc3339(),
            "stream": "event",
            "line": serde_json::to_string(event).unwrap_or_default(),
        });
        self.lines.push(record.to_string());
    }

    async fn finish(self) -> Option<ArtifactRef> {
        let mut body = self.lines.join("\n");
        if !body.is_empty() {
            body.push('\n');
        }
        match tokio::fs::write(&self.path, body.as_bytes()).await {
            Ok(()) => Some(ArtifactRef {
                id: uuid::Uuid::now_v7(),
                kind: ArtifactKind::Transcript,
                uri: format!("file://{}", self.path.display()),
                sha256: sha256_hex(body.as_bytes()),
                bytes: body.len() as u64,
            }),
            Err(err) => {
                tracing::warn!(path = %self.path.display(), error = %err, "fake transcript write failed");
                None
            }
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn run_script(
    rule: Rule,
    req: TaskAttemptRequest,
    mut sink: EventSink,
    path: PathBuf,
) -> WorkerOutcome {
    let start = Instant::now();
    let deadline = start + req.budget.timeout;
    let cancel = req.cancel.clone();
    let mut transcript = Transcript {
        path,
        lines: Vec::new(),
    };
    let session_id = WorkerSessionId::new(
        rule.session_id
            .clone()
            .unwrap_or_else(|| format!("fake-{}", req.attempt_id)),
    );
    let started = WorkerEvent::Started {
        session_id: Some(session_id.clone()),
        pid: None,
    };
    transcript.record(&started);
    sink.emit(started).await;

    let mut usage = Usage::default();
    let fail_with = |class: FailureClass, message: &str, usage: &Usage, wall: Duration| {
        let mut usage = usage.clone();
        usage.wall_ms = u64::try_from(wall.as_millis()).unwrap_or(u64::MAX);
        WorkerEvent::Failed {
            class,
            message: message.to_owned(),
            usage,
        }
    };

    let interrupted = if rule.hold {
        match pause(Duration::MAX, &cancel, deadline).await {
            Pause::Cancelled => Some((FailureClass::Cancelled, "cancelled")),
            Pause::Timeout => Some((FailureClass::Transient, "timeout")),
            Pause::Done => None,
        }
    } else if rule.delay_ms > 0 {
        match pause(Duration::from_millis(rule.delay_ms), &cancel, deadline).await {
            Pause::Cancelled => Some((FailureClass::Cancelled, "cancelled")),
            Pause::Timeout => Some((FailureClass::Transient, "timeout")),
            Pause::Done => None,
        }
    } else if cancel.is_cancelled() {
        Some((FailureClass::Cancelled, "cancelled"))
    } else {
        None
    };
    if let Some((class, message)) = interrupted {
        let ev = fail_with(class, message, &usage, start.elapsed());
        transcript.record(&ev);
        sink.emit(ev).await;
        return WorkerOutcome::Failed {
            class,
            message: message.to_owned(),
            usage,
            transcript: transcript.finish().await,
        };
    }

    for scripted in &rule.events {
        if cancel.is_cancelled() {
            let ev = fail_with(
                FailureClass::Cancelled,
                "cancelled",
                &usage,
                start.elapsed(),
            );
            transcript.record(&ev);
            sink.emit(ev).await;
            return WorkerOutcome::Failed {
                class: FailureClass::Cancelled,
                message: "cancelled".to_owned(),
                usage,
                transcript: transcript.finish().await,
            };
        }
        let ev = scripted.to_worker_event();
        if let WorkerEvent::Usage { delta } = &ev {
            usage += delta.clone();
        }
        transcript.record(&ev);
        sink.emit(ev).await;
    }

    if let Some(explicit) = &rule.usage {
        usage = explicit.clone();
    }
    usage.wall_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);

    if let Some(fail) = &rule.fail {
        let ev = WorkerEvent::Failed {
            class: fail.class,
            message: fail.message.clone(),
            usage: usage.clone(),
        };
        transcript.record(&ev);
        sink.emit(ev).await;
        return WorkerOutcome::Failed {
            class: fail.class,
            message: fail.message.clone(),
            usage,
            transcript: transcript.finish().await,
        };
    }

    let text = rule.reply.clone().unwrap_or_default();
    let structured = match (&rule.structured, &req.spec.output_schema) {
        (Some(value), Some(schema)) => {
            structured::validate(value, schema).map(|()| Some(value.clone()))
        }
        (Some(value), None) => Ok(Some(value.clone())),
        (None, Some(schema)) => structured::extract_and_validate(&text, schema).map(Some),
        (None, None) => Ok(None),
    };
    let structured = match structured {
        Ok(value) => value,
        Err(err) => {
            let message = format!("schema_violation: {err}");
            let ev = WorkerEvent::Failed {
                class: FailureClass::Permanent,
                message: message.clone(),
                usage: usage.clone(),
            };
            transcript.record(&ev);
            sink.emit(ev).await;
            return WorkerOutcome::Failed {
                class: FailureClass::Permanent,
                message,
                usage,
                transcript: transcript.finish().await,
            };
        }
    };

    let ev = WorkerEvent::Final {
        text: text.clone(),
        structured: structured.clone(),
        usage: usage.clone(),
    };
    transcript.record(&ev);
    sink.emit(ev).await;
    let transcript = transcript.finish().await.unwrap_or_else(|| ArtifactRef {
        id: uuid::Uuid::now_v7(),
        kind: ArtifactKind::Transcript,
        uri: String::new(),
        sha256: sha256_hex(b""),
        bytes: 0,
    });
    WorkerOutcome::Succeeded {
        text,
        structured,
        usage,
        session_id: Some(session_id),
        transcript,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAN_YAML: &str = r#"
default: { reply: "done", usage: { input_tokens: 10, output_tokens: 5 } }
rules:
  - match: "reply deterministically"
    reply: "kohral-ok"
  - match: "[[KOHRAL_HOLD]]"
    hold: true
  - match: /implement .* auth/
    events: [ {tool_call: {name: edit, input_summary: "src/auth.rs"}}, {text: "Added auth"} ]
    structured: { status: "ok" }
    delay_ms: 50
  - match: "fail transient"
    fail: { class: transient, message: "simulated 429" }
"#;

    #[test]
    fn parses_the_plan_yaml_and_selects_first_match() {
        let s = Scenario::from_yaml(PLAN_YAML).unwrap();
        assert_eq!(s.rules.len(), 4);
        assert_eq!(s.default.reply.as_deref(), Some("done"));
        assert_eq!(s.default.usage, Some(Usage::tokens(10, 5)));
        assert_eq!(
            s.select("please reply deterministically").reply.as_deref(),
            Some("kohral-ok")
        );
        assert!(s.select("x [[KOHRAL_HOLD]] y").hold);
        let auth = s.select("implement the auth module");
        assert_eq!(auth.events.len(), 2);
        assert_eq!(auth.delay_ms, 50);
        assert_eq!(auth.structured, Some(serde_json::json!({"status": "ok"})));
        assert_eq!(
            s.select("fail transient now").fail,
            Some(FailSpec {
                class: FailureClass::Transient,
                message: "simulated 429".into()
            })
        );
        assert_eq!(s.select("anything else").reply.as_deref(), Some("done"));
        let yaml = s.to_yaml().unwrap();
        assert_eq!(Scenario::from_yaml(&yaml).unwrap(), s);
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(Scenario::from_json(&json).unwrap(), s);
    }

    #[test]
    fn invalid_regex_is_reported() {
        let err = Scenario::from_yaml("rules:\n  - match: /(/\n    reply: x\n").unwrap_err();
        assert!(matches!(err, ScenarioError::Parse { .. }), "{err}");
        assert!(err.to_string().contains("regex"), "{err}");
        assert!(Matcher::parse("/[/").is_err());
        assert!(Matcher::parse("/").unwrap().matches("a/b"));
    }

    #[test]
    fn builtin_scenario_has_kohral_hooks() {
        let s = Scenario::builtin();
        assert_eq!(
            s.select(KOHRAL_REPLY_INPUT).reply.as_deref(),
            Some(KOHRAL_REPLY_OUTPUT)
        );
        assert!(s.select(KOHRAL_HOLD_INPUT).hold);
        assert_eq!(s.select("other").reply.as_deref(), Some("done"));
        assert_eq!(Scenario::default(), s);
    }
}
