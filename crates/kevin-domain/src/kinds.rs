//! Kind and classification value objects (`plan/02-domain-model.md` §Identifiers and value objects).
//!
//! - [`TaskKind`] — the fixed task taxonomy plus `custom:<name>`; drives routing.
//! - [`WorkerKind`] — which CLI adapter (or the in-process `fake`) runs a task.
//! - [`ModelAlias`] — validated config-level model name (`[models.<alias>]`).
//! - [`Effort`] — reasoning effort requested from a worker.
//! - [`FailureClass`] — why an attempt failed; decides retry policy.
//! - [`Complexity`] — low/medium/high estimate from the understanding phase.
//! - [`Tier`] — fast/balanced/frontier model tier.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Grammar shared by [`ModelAlias`] and custom task-kind names:
/// `[a-z0-9][a-z0-9._-]*`.
fn is_valid_name(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

// ---------------------------------------------------------------------------
// TaskKind
// ---------------------------------------------------------------------------

/// Prefix of the string form of [`TaskKind::Custom`].
pub const CUSTOM_TASK_KIND_PREFIX: &str = "custom:";

/// Kind of a task; a value from the fixed taxonomy or `custom:<name>`.
///
/// String / serde form is `snake_case` for built-ins (`implement`) and
/// `custom:<name>` for custom kinds, where `<name>` matches
/// `[a-z0-9][a-z0-9._-]*`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TaskKind {
    /// Understand the goal (planner role).
    Understand,
    /// Ask the user clarifying questions (clarifier role).
    Clarify,
    /// Produce the task graph (planner role).
    Plan,
    /// Investigate code/docs without changing anything.
    Research,
    /// Write or change code.
    Implement,
    /// Write or run tests.
    Test,
    /// Review a change.
    Review,
    /// Refactor without behaviour change.
    Refactor,
    /// Find and fix a defect.
    Debug,
    /// Write prose/documentation.
    Write,
    /// Operational work (infra, CI, deploy).
    Ops,
    /// Judge an outcome against a rubric (judge role).
    Evaluate,
    /// Integrate task results (integrator role).
    Integrate,
    /// Operator-defined kind; the name is validated (`[a-z0-9][a-z0-9._-]*`).
    Custom(String),
}

impl TaskKind {
    /// Every built-in kind, in taxonomy order.
    pub const BUILTIN: [TaskKind; 13] = [
        TaskKind::Understand,
        TaskKind::Clarify,
        TaskKind::Plan,
        TaskKind::Research,
        TaskKind::Implement,
        TaskKind::Test,
        TaskKind::Review,
        TaskKind::Refactor,
        TaskKind::Debug,
        TaskKind::Write,
        TaskKind::Ops,
        TaskKind::Evaluate,
        TaskKind::Integrate,
    ];

    /// Builds a custom kind after validating `name`.
    pub fn custom(name: impl Into<String>) -> Result<Self, InvalidTaskKind> {
        let name = name.into();
        if is_valid_name(&name) {
            Ok(TaskKind::Custom(name))
        } else {
            Err(InvalidTaskKind(format!("{CUSTOM_TASK_KIND_PREFIX}{name}")))
        }
    }

    /// `true` for [`TaskKind::Custom`].
    #[must_use]
    pub const fn is_custom(&self) -> bool {
        matches!(self, TaskKind::Custom(_))
    }

    /// The `snake_case` name of a built-in kind, or the bare name of a custom
    /// kind (without the `custom:` prefix). Use `Display` for the full form.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            TaskKind::Understand => "understand",
            TaskKind::Clarify => "clarify",
            TaskKind::Plan => "plan",
            TaskKind::Research => "research",
            TaskKind::Implement => "implement",
            TaskKind::Test => "test",
            TaskKind::Review => "review",
            TaskKind::Refactor => "refactor",
            TaskKind::Debug => "debug",
            TaskKind::Write => "write",
            TaskKind::Ops => "ops",
            TaskKind::Evaluate => "evaluate",
            TaskKind::Integrate => "integrate",
            TaskKind::Custom(name) => name,
        }
    }
}

impl fmt::Display for TaskKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TaskKind::Custom(name) => write!(f, "{CUSTOM_TASK_KIND_PREFIX}{name}"),
            builtin => f.write_str(builtin.name()),
        }
    }
}

/// Error returned when a string is not a valid task kind.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "invalid task kind {0:?}: expected one of the built-in kinds or `custom:<name>` with name matching [a-z0-9][a-z0-9._-]*"
)]
pub struct InvalidTaskKind(pub String);

impl FromStr for TaskKind {
    type Err = InvalidTaskKind;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(name) = s.strip_prefix(CUSTOM_TASK_KIND_PREFIX) {
            return TaskKind::custom(name).map_err(|_| InvalidTaskKind(s.to_owned()));
        }
        TaskKind::BUILTIN
            .iter()
            .find(|k| k.name() == s)
            .cloned()
            .ok_or_else(|| InvalidTaskKind(s.to_owned()))
    }
}

impl Serialize for TaskKind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for TaskKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// WorkerKind
// ---------------------------------------------------------------------------

/// Which adapter executes a task attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkerKind {
    /// Claude Code (`claude`).
    Claude,
    /// OpenAI Codex CLI (`codex`).
    Codex,
    /// `pi`.
    Pi,
    /// `opencode`.
    Opencode,
    /// In-process deterministic worker for tests and Kohral conformance.
    Fake,
}

impl WorkerKind {
    /// Every worker kind.
    pub const ALL: [WorkerKind; 5] = [
        WorkerKind::Claude,
        WorkerKind::Codex,
        WorkerKind::Pi,
        WorkerKind::Opencode,
        WorkerKind::Fake,
    ];

    /// Lowercase name, identical to the serde and config form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            WorkerKind::Claude => "claude",
            WorkerKind::Codex => "codex",
            WorkerKind::Pi => "pi",
            WorkerKind::Opencode => "opencode",
            WorkerKind::Fake => "fake",
        }
    }
}

impl fmt::Display for WorkerKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned when a string names no known worker.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown worker kind {0:?}: expected one of claude, codex, pi, opencode, fake")]
pub struct UnknownWorkerKind(pub String);

impl FromStr for WorkerKind {
    type Err = UnknownWorkerKind;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        WorkerKind::ALL
            .into_iter()
            .find(|k| k.as_str() == s)
            .ok_or_else(|| UnknownWorkerKind(s.to_owned()))
    }
}

// ---------------------------------------------------------------------------
// ModelAlias
// ---------------------------------------------------------------------------

/// Config-level model name (`[models.<alias>]`), validated as
/// `[a-z0-9][a-z0-9._-]*`. Routing works on aliases, never on raw model ids.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ModelAlias(String);

/// Error returned when a string is not a valid [`ModelAlias`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid model alias {0:?}: must match [a-z0-9][a-z0-9._-]*")]
pub struct InvalidModelAlias(pub String);

impl ModelAlias {
    /// Validates and wraps `alias`.
    pub fn new(alias: impl Into<String>) -> Result<Self, InvalidModelAlias> {
        let alias = alias.into();
        if is_valid_name(&alias) {
            Ok(Self(alias))
        } else {
            Err(InvalidModelAlias(alias))
        }
    }

    /// The alias as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ModelAlias {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for ModelAlias {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl FromStr for ModelAlias {
    type Err = InvalidModelAlias;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl TryFrom<String> for ModelAlias {
    type Error = InvalidModelAlias;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<&str> for ModelAlias {
    type Error = InvalidModelAlias;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ModelAlias> for String {
    fn from(alias: ModelAlias) -> String {
        alias.0
    }
}

// ---------------------------------------------------------------------------
// Effort
// ---------------------------------------------------------------------------

/// Reasoning effort requested for an attempt; each adapter maps it to its own
/// flag (`plan/04-workers.md` §Usage, cost, effort). Config form is lowercase
/// (`xhigh`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    /// Minimal reasoning.
    Low,
    /// Default reasoning.
    Medium,
    /// Extended reasoning.
    High,
    /// Extra-high reasoning (`xhigh`).
    XHigh,
    /// Maximum the worker supports.
    Max,
}

impl Effort {
    /// Every effort level, lowest first.
    pub const ALL: [Effort; 5] = [
        Effort::Low,
        Effort::Medium,
        Effort::High,
        Effort::XHigh,
        Effort::Max,
    ];

    /// Lowercase name, identical to the serde and config form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::XHigh => "xhigh",
            Effort::Max => "max",
        }
    }
}

impl fmt::Display for Effort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned when a string names no effort level.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown effort {0:?}: expected one of low, medium, high, xhigh, max")]
pub struct UnknownEffort(pub String);

impl FromStr for Effort {
    type Err = UnknownEffort;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Effort::ALL
            .into_iter()
            .find(|e| e.as_str() == s)
            .ok_or_else(|| UnknownEffort(s.to_owned()))
    }
}

// ---------------------------------------------------------------------------
// FailureClass
// ---------------------------------------------------------------------------

/// Classification of a failed attempt; drives the retry policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// Timeout, rate limit, worker crash — retry may succeed.
    Transient,
    /// Invalid spec, tool refused, schema violation after N tries — do not retry.
    Permanent,
    /// A budget dimension was exhausted.
    Budget,
    /// Cancelled by a user or by the run.
    Cancelled,
    /// The runtime restarted while the attempt was running.
    RuntimeRestarted,
}

impl FailureClass {
    /// Every failure class.
    pub const ALL: [FailureClass; 5] = [
        FailureClass::Transient,
        FailureClass::Permanent,
        FailureClass::Budget,
        FailureClass::Cancelled,
        FailureClass::RuntimeRestarted,
    ];

    /// `snake_case` name, identical to the serde form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            FailureClass::Transient => "transient",
            FailureClass::Permanent => "permanent",
            FailureClass::Budget => "budget",
            FailureClass::Cancelled => "cancelled",
            FailureClass::RuntimeRestarted => "runtime_restarted",
        }
    }
}

impl fmt::Display for FailureClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned when a string names no failure class.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown failure class {0:?}")]
pub struct UnknownFailureClass(pub String);

impl FromStr for FailureClass {
    type Err = UnknownFailureClass;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        FailureClass::ALL
            .into_iter()
            .find(|c| c.as_str() == s)
            .ok_or_else(|| UnknownFailureClass(s.to_owned()))
    }
}

impl FailureClass {
    /// Whether the retry policy may start another attempt after this failure
    /// (`plan/05-orchestration.md` §3.5): `Transient` and `RuntimeRestarted`
    /// are not the model's fault; `Permanent`, `Budget` and `Cancelled` are final.
    #[must_use]
    pub const fn is_retryable(self) -> bool {
        matches!(
            self,
            FailureClass::Transient | FailureClass::RuntimeRestarted
        )
    }

    /// Whether this failure counts against the model in routing scores
    /// (`plan/06-memory-and-learning.md` §2.4): only `Permanent` and `Budget`.
    #[must_use]
    pub const fn blames_model(self) -> bool {
        matches!(self, FailureClass::Permanent | FailureClass::Budget)
    }
}

// ---------------------------------------------------------------------------
// Complexity
// ---------------------------------------------------------------------------

/// Estimated complexity of a goal or task (`kevin.understanding.v1` `complexity`);
/// the router maps it to an effort and a preferred tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Complexity {
    /// Small, local change.
    Low,
    /// Typical feature-sized work.
    Medium,
    /// Cross-cutting or risky work.
    High,
}

impl Complexity {
    /// Every complexity, lowest first.
    pub const ALL: [Complexity; 3] = [Complexity::Low, Complexity::Medium, Complexity::High];

    /// Lowercase name, identical to the serde form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Complexity::Low => "low",
            Complexity::Medium => "medium",
            Complexity::High => "high",
        }
    }

    /// Default effort for an attempt of this complexity
    /// (`plan/06-memory-and-learning.md` §2.2 step 6).
    #[must_use]
    pub const fn default_effort(self) -> Effort {
        match self {
            Complexity::Low => Effort::Medium,
            Complexity::Medium => Effort::High,
            Complexity::High => Effort::XHigh,
        }
    }
}

impl fmt::Display for Complexity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned when a string names no complexity.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown complexity {0:?}: expected one of low, medium, high")]
pub struct UnknownComplexity(pub String);

impl FromStr for Complexity {
    type Err = UnknownComplexity;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Complexity::ALL
            .into_iter()
            .find(|c| c.as_str() == s)
            .ok_or_else(|| UnknownComplexity(s.to_owned()))
    }
}

// ---------------------------------------------------------------------------
// Tier
// ---------------------------------------------------------------------------

/// Price/capability tier of a model alias (`[models.<alias>].tier`) and the
/// planner's `suggested_tier` hint in `kevin.plan.v1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// Cheap and quick.
    Fast,
    /// Default trade-off.
    Balanced,
    /// Most capable, most expensive.
    Frontier,
}

impl Tier {
    /// Every tier, cheapest first.
    pub const ALL: [Tier; 3] = [Tier::Fast, Tier::Balanced, Tier::Frontier];

    /// Lowercase name, identical to the serde form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Tier::Fast => "fast",
            Tier::Balanced => "balanced",
            Tier::Frontier => "frontier",
        }
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned when a string names no tier.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown tier {0:?}: expected one of fast, balanced, frontier")]
pub struct UnknownTier(pub String);

impl FromStr for Tier {
    type Err = UnknownTier;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Tier::ALL
            .into_iter()
            .find(|t| t.as_str() == s)
            .ok_or_else(|| UnknownTier(s.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn task_kind_builtins_serde_snake_case_round_trip() {
        for kind in TaskKind::BUILTIN {
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(json, format!("\"{}\"", kind.name()));
            assert_eq!(serde_json::from_str::<TaskKind>(&json).unwrap(), kind);
            assert_eq!(kind.name().parse::<TaskKind>().unwrap(), kind);
            assert!(!kind.is_custom());
        }
        assert_eq!(
            serde_json::to_string(&TaskKind::Implement).unwrap(),
            "\"implement\""
        );
    }

    #[test]
    fn task_kind_custom_uses_prefixed_string_form() {
        let kind: TaskKind = "custom:data_migration".parse().unwrap();
        assert_eq!(kind, TaskKind::Custom("data_migration".into()));
        assert!(kind.is_custom());
        assert_eq!(kind.name(), "data_migration");
        assert_eq!(kind.to_string(), "custom:data_migration");
        let json = serde_json::to_string(&kind).unwrap();
        assert_eq!(json, "\"custom:data_migration\"");
        assert_eq!(serde_json::from_str::<TaskKind>(&json).unwrap(), kind);
        assert_eq!(
            TaskKind::custom("x.y-z").unwrap().to_string(),
            "custom:x.y-z"
        );
    }

    #[test]
    fn task_kind_rejects_unknown_and_invalid_custom() {
        assert!("deploy".parse::<TaskKind>().is_err());
        assert!("Implement".parse::<TaskKind>().is_err());
        assert!("custom:".parse::<TaskKind>().is_err());
        assert!("custom:Bad".parse::<TaskKind>().is_err());
        assert!("custom:-x".parse::<TaskKind>().is_err());
        assert!("custom:a b".parse::<TaskKind>().is_err());
        assert!(TaskKind::custom("").is_err());
        assert!(serde_json::from_str::<TaskKind>("\"custom:Bad\"").is_err());
        assert!(serde_json::from_str::<TaskKind>("{\"custom\":\"x\"}").is_err());
    }

    #[test]
    fn worker_kind_serde_and_parse() {
        for kind in WorkerKind::ALL {
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(json, format!("\"{kind}\""));
            assert_eq!(serde_json::from_str::<WorkerKind>(&json).unwrap(), kind);
            assert_eq!(kind.as_str().parse::<WorkerKind>().unwrap(), kind);
        }
        assert_eq!(WorkerKind::Opencode.to_string(), "opencode");
        assert!("gemini".parse::<WorkerKind>().is_err());
    }

    #[test]
    fn model_alias_validation() {
        for ok in ["opus5-claude", "gpt56-codex", "a", "0x", "sonnet5.pi_v2-x"] {
            assert!(ModelAlias::new(ok).is_ok(), "{ok} should be valid");
        }
        for bad in ["", "Opus", "-x", ".x", "a b", "a/b", "ä", "a:b"] {
            assert!(ModelAlias::new(bad).is_err(), "{bad:?} should be invalid");
        }
        let alias: ModelAlias = "opus5-claude".parse().unwrap();
        assert_eq!(alias.as_str(), "opus5-claude");
        assert_eq!(alias.to_string(), "opus5-claude");
        assert_eq!(String::from(alias.clone()), "opus5-claude");
        assert_eq!(ModelAlias::try_from("opus5-claude").unwrap(), alias);
    }

    #[test]
    fn model_alias_serde_is_validated_string_and_works_as_map_key() {
        let alias = ModelAlias::new("sonnet5-claude").unwrap();
        assert_eq!(serde_json::to_string(&alias).unwrap(), "\"sonnet5-claude\"");
        assert_eq!(
            serde_json::from_str::<ModelAlias>("\"sonnet5-claude\"").unwrap(),
            alias
        );
        assert!(serde_json::from_str::<ModelAlias>("\"Bad Alias\"").is_err());

        let mut map = BTreeMap::new();
        map.insert(alias.clone(), 1u8);
        let json = serde_json::to_string(&map).unwrap();
        assert_eq!(json, "{\"sonnet5-claude\":1}");
        let back: BTreeMap<ModelAlias, u8> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, map);
        assert!(serde_json::from_str::<BTreeMap<ModelAlias, u8>>("{\"NOPE\":1}").is_err());
    }

    #[test]
    fn effort_serde_is_lowercase_and_ordered() {
        assert_eq!(serde_json::to_string(&Effort::XHigh).unwrap(), "\"xhigh\"");
        for effort in Effort::ALL {
            let json = serde_json::to_string(&effort).unwrap();
            assert_eq!(json, format!("\"{effort}\""));
            assert_eq!(serde_json::from_str::<Effort>(&json).unwrap(), effort);
            assert_eq!(effort.as_str().parse::<Effort>().unwrap(), effort);
        }
        assert!(Effort::Low < Effort::Medium && Effort::XHigh < Effort::Max);
        assert!("ultra".parse::<Effort>().is_err());
    }

    #[test]
    fn failure_class_serde_is_snake_case() {
        assert_eq!(
            serde_json::to_string(&FailureClass::RuntimeRestarted).unwrap(),
            "\"runtime_restarted\""
        );
        for class in FailureClass::ALL {
            let json = serde_json::to_string(&class).unwrap();
            assert_eq!(json, format!("\"{class}\""));
            assert_eq!(serde_json::from_str::<FailureClass>(&json).unwrap(), class);
            assert_eq!(class.as_str().parse::<FailureClass>().unwrap(), class);
        }
        assert!("flaky".parse::<FailureClass>().is_err());
    }

    #[test]
    fn complexity_and_tier_serde_lowercase() {
        for c in Complexity::ALL {
            let json = serde_json::to_string(&c).unwrap();
            assert_eq!(json, format!("\"{c}\""));
            assert_eq!(serde_json::from_str::<Complexity>(&json).unwrap(), c);
            assert_eq!(c.as_str().parse::<Complexity>().unwrap(), c);
        }
        for t in Tier::ALL {
            let json = serde_json::to_string(&t).unwrap();
            assert_eq!(json, format!("\"{t}\""));
            assert_eq!(serde_json::from_str::<Tier>(&json).unwrap(), t);
            assert_eq!(t.as_str().parse::<Tier>().unwrap(), t);
        }
        assert!("extreme".parse::<Complexity>().is_err());
        assert!("premium".parse::<Tier>().is_err());
        assert_eq!(Complexity::High.default_effort(), Effort::XHigh);
    }

    #[test]
    fn failure_class_retry_policy() {
        assert!(FailureClass::Transient.is_retryable());
        assert!(FailureClass::RuntimeRestarted.is_retryable());
        assert!(!FailureClass::Permanent.is_retryable());
        assert!(!FailureClass::Budget.is_retryable());
        assert!(!FailureClass::Cancelled.is_retryable());
        assert!(FailureClass::Permanent.blames_model());
        assert!(!FailureClass::Transient.blames_model());
    }
}
