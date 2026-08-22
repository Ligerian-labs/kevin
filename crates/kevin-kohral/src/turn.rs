//! A Kohral turn, and the [`StartRun`] it becomes
//! (`plan/08-kohral-runtime.md` §1.2).
//!
//! The whole point of this crate is that Kevin's core never learns what a
//! "turn" is: everything Kohral-shaped is translated here, once.
//!
//! | Kohral | Kevin |
//! |---|---|
//! | `Idempotency-Key` | `CommandId` of `StartRun` + the ledger primary key |
//! | `input` | the request part of [`Goal::text`] |
//! | `instructions` | an `Operator instructions` block in the goal |
//! | `conversation_history` | a `Conversation so far` block in the goal |
//! | `session_id` / `X-Hermes-Session-Key` | [`RunMode::Kohral`] |
//! | `model` | role override for planner/judge/default (see [`crate::catalog`]) |
//! | `attachments` | [`Goal::attachments`] |
//!
//! Deviation worth knowing: `plan/08` §1.2 speaks of prepending `instructions`
//! and the history to the *system context*, but
//! [`kevin_orchestrator::roles::SystemContextProvider`] is process-wide (it is
//! built once at boot for the platform briefing) while these two are
//! **per-turn**. They therefore travel in `Goal.text`, which is the only
//! per-run channel into every role and worker prompt. The rendering keeps them
//! in clearly marked sections so a planner can tell the operator's words from
//! the user's request.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kevin_domain::run::StartRun;
use kevin_domain::{
    ArtifactId, ArtifactKind, ArtifactRef, Budget, Goal, ModelAlias, RepoKind, RunId, RunMode,
};
use serde::Deserialize;

use crate::catalog::Resolution;
use crate::error::{KohralError, KohralErrorCode, KohralResult};

/// Kohral already caps the history it sends; Kevin caps again so a hostile or
/// buggy caller cannot blow up a prompt (`plan/08` §1.2).
pub const MAX_HISTORY_MESSAGES: usize = 100;
/// …and the same cap by size.
pub const MAX_HISTORY_BYTES: usize = 200 * 1024;
/// Largest `input` Kevin accepts, mirroring `kevin_api::state::MAX_GOAL_BYTES`.
pub const MAX_INPUT_BYTES: usize = 64 * 1024;

/// `Idempotency-Key` grammar (`plan/08` §1.2).
pub const MAX_IDEMPOTENCY_KEY: usize = 256;

/// One entry of `conversation_history`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct HistoryMessage {
    /// `user` or `assistant` (Kohral filters `system` into `instructions`).
    #[serde(default)]
    pub role: String,
    /// The text.
    #[serde(default)]
    pub content: String,
}

/// One temporary attachment, as returned by `PUT /v1/attachments/…`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TurnAttachment {
    /// Absolute path under `/tmp/kohral-uploads/`.
    #[serde(default)]
    pub path: String,
    /// Size in bytes.
    #[serde(default)]
    pub size: Option<u64>,
    /// Hex digest.
    #[serde(default)]
    pub sha256: Option<String>,
}

/// The body of `POST /v1/runs`.
///
/// Deliberately **not** `deny_unknown_fields`: Kohral is free to add fields to
/// the Hermes payload, and a runtime that 400s on an unknown key would break
/// on the next Kohral release.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct TurnRequest {
    /// The user's message.
    pub input: String,
    /// The system entries of the conversation, joined by blank lines.
    pub instructions: String,
    /// Everything said so far, oldest first.
    pub conversation_history: Vec<HistoryMessage>,
    /// The Kohral conversation id.
    pub session_id: String,
    /// `"hermes-agent"`, `""` or a `provider/model` override.
    pub model: String,
    /// Files uploaded for this turn.
    pub attachments: Vec<TurnAttachment>,
}

impl TurnRequest {
    /// Rejects a turn Kevin cannot execute.
    pub fn validate(&self) -> KohralResult<()> {
        if self.input.trim().is_empty() {
            return Err(KohralError::new(
                KohralErrorCode::InvalidRequest,
                "`input` must not be empty",
            ));
        }
        if self.input.len() > MAX_INPUT_BYTES {
            return Err(KohralError::new(
                KohralErrorCode::InvalidRequest,
                format!("`input` must be at most {MAX_INPUT_BYTES} bytes"),
            ));
        }
        Ok(())
    }

    /// The conversation id, falling back to the session key's suffix and then
    /// to `"default"` — Kohral always sends one, but the ledger column is
    /// `NOT NULL` and a missing id must not 500.
    #[must_use]
    pub fn session_id(&self, session_key: Option<&str>) -> String {
        let trimmed = self.session_id.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
        session_key
            .and_then(|key| key.rsplit(':').next())
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .unwrap_or("default")
            .to_owned()
    }

    /// The goal text: the request first, then the operator's instructions and
    /// the conversation, each in its own section.
    #[must_use]
    pub fn goal_text(&self) -> String {
        let mut text = self.input.trim().to_owned();
        let instructions = self.instructions.trim();
        if !instructions.is_empty() {
            text.push_str("\n\n## Operator instructions (from the Kohral control plane)\n\n");
            text.push_str(instructions);
        }
        let history = self.capped_history();
        if !history.is_empty() {
            text.push_str("\n\n## Conversation so far\n");
            for message in history {
                let role = if message.role.eq_ignore_ascii_case("assistant") {
                    "assistant"
                } else {
                    "user"
                };
                let _ = write!(text, "\n**{role}**: {}\n", message.content.trim());
            }
        }
        text
    }

    /// The tail of the history that fits both caps, oldest first.
    #[must_use]
    pub fn capped_history(&self) -> Vec<&HistoryMessage> {
        let mut budget = MAX_HISTORY_BYTES;
        let mut kept: Vec<&HistoryMessage> = Vec::new();
        for message in self
            .conversation_history
            .iter()
            .rev()
            .take(MAX_HISTORY_MESSAGES)
        {
            let size = message.content.len() + message.role.len();
            if size > budget {
                break;
            }
            budget -= size;
            kept.push(message);
        }
        kept.reverse();
        kept
    }

    /// Attachments as artifact references. A path outside the upload directory
    /// is dropped: a turn must not be able to hand a worker an arbitrary file.
    #[must_use]
    pub fn artifacts(&self, upload_root: &Path) -> Vec<ArtifactRef> {
        self.attachments
            .iter()
            .filter(|attachment| Path::new(&attachment.path).starts_with(upload_root))
            .map(|attachment| ArtifactRef {
                id: ArtifactId::new(),
                kind: ArtifactKind::File,
                uri: format!("file://{}", attachment.path),
                sha256: attachment.sha256.clone(),
                bytes: attachment.size,
            })
            .collect()
    }
}

/// Everything the ledger and the orchestrator need for one accepted turn.
#[derive(Debug, Clone)]
pub struct AcceptedTurn {
    /// Kevin's run id.
    pub run_id: RunId,
    /// The command that starts it.
    pub command: StartRun,
    /// The alias the `model` field resolved to, if any.
    pub model_override: Option<ModelAlias>,
    /// The conversation this turn belongs to.
    pub session_id: String,
}

/// Where a Kohral run works. There is usually no repository in a Kohral
/// workload, so the default is a per-run directory under the data volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnEnvironment {
    /// Root under which each run gets its own directory.
    pub work_root: PathBuf,
    /// `/tmp/kohral-uploads`.
    pub upload_root: PathBuf,
    /// `kohral.run_timeout`.
    pub run_timeout: Duration,
}

/// Builds the [`StartRun`] for an accepted turn.
///
/// - `mode` is always [`RunMode::Kohral`] and `auto_approve_plans` is forced
///   `true`: a Kohral turn never waits for a human (`plan/08` §3).
/// - the wall-clock budget is capped to `kohral.run_timeout` (`plan/05` §1).
/// - the `model` override is applied by the caller as a role override; it is
///   reported here so the caller does not have to resolve it twice.
pub fn accept(
    run_id: RunId,
    request: &TurnRequest,
    idempotency_key: &str,
    session_key: Option<&str>,
    resolution: &Resolution,
    defaults: &kevin_config::schema::Budget,
    env: &TurnEnvironment,
) -> AcceptedTurn {
    let session_id = request.session_id(session_key);
    let cwd = env.work_root.join(run_id.to_string());
    let goal = Goal {
        text: request.goal_text(),
        attachments: request.artifacts(&env.upload_root),
        repo_kind: RepoKind::None,
        cwd,
    };
    let mut budget = Budget {
        max_usd: Some(defaults.default_run_usd),
        max_tokens: None,
        max_wall: Some(defaults.default_run_wall.min(env.run_timeout)),
        max_attempts: defaults.max_attempts,
        max_parallel: defaults.max_parallel_tasks,
    };
    if budget.max_wall.is_none_or(|wall| wall > env.run_timeout) {
        budget.max_wall = Some(env.run_timeout);
    }
    AcceptedTurn {
        run_id,
        command: StartRun {
            run_id,
            goal,
            mode: RunMode::Kohral {
                turn_id: idempotency_key.to_owned(),
                session_key: session_key.unwrap_or_default().to_owned(),
                session_id: session_id.clone(),
            },
            budget,
            requested_by: "kohral".to_owned(),
            // A Kohral turn is headless by contract; the config flag is
            // irrelevant here (`plan/08` §3, `plan/05` §5).
            auto_approve_plans: true,
        },
        model_override: match resolution {
            Resolution::Alias(alias) => Some(alias.clone()),
            Resolution::NoOverride | Resolution::Unknown => None,
        },
        session_id,
    }
}

/// Validates `Idempotency-Key` against `^[A-Za-z0-9][A-Za-z0-9._:-]{0,255}$`.
pub fn validate_idempotency_key(key: &str) -> KohralResult<()> {
    let invalid = || {
        KohralError::new(
            KohralErrorCode::InvalidIdempotencyKey,
            "Idempotency-Key must match ^[A-Za-z0-9][A-Za-z0-9._:-]{0,255}$",
        )
    };
    if key.is_empty() || key.len() > MAX_IDEMPOTENCY_KEY {
        return Err(invalid());
    }
    let mut bytes = key.bytes();
    if !bytes.next().is_some_and(|b| b.is_ascii_alphanumeric()) {
        return Err(invalid());
    }
    if !bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-')) {
        return Err(invalid());
    }
    Ok(())
}

/// The user message recorded for `/api/sessions/{id}/messages`.
#[must_use]
pub fn user_message_id(run_id: RunId) -> String {
    format!("umsg_{run_id}")
}

/// The stable assistant message id of a turn (`plan/08` §1.3).
#[must_use]
pub fn assistant_message_id(run_id: RunId) -> String {
    format!("msg_{run_id}")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use kevin_domain::{RunId, RunMode};

    use super::{
        HistoryMessage, MAX_HISTORY_MESSAGES, TurnAttachment, TurnEnvironment, TurnRequest, accept,
        validate_idempotency_key,
    };
    use crate::catalog::Resolution;

    fn env() -> TurnEnvironment {
        TurnEnvironment {
            work_root: std::path::PathBuf::from("/opt/kevin/data/work"),
            upload_root: std::path::PathBuf::from("/tmp/kohral-uploads"),
            run_timeout: Duration::from_secs(60),
        }
    }

    fn request() -> TurnRequest {
        TurnRequest {
            input: "add a health endpoint".to_owned(),
            instructions: "Answer in French.".to_owned(),
            conversation_history: vec![
                HistoryMessage {
                    role: "user".to_owned(),
                    content: "hello".to_owned(),
                },
                HistoryMessage {
                    role: "assistant".to_owned(),
                    content: "hi".to_owned(),
                },
            ],
            session_id: "conv-1".to_owned(),
            model: "hermes-agent".to_owned(),
            attachments: Vec::new(),
        }
    }

    #[test]
    fn the_hermes_payload_deserializes_and_tolerates_unknown_keys() {
        let parsed: TurnRequest = serde_json::from_value(serde_json::json!({
            "input": "hi",
            "instructions": "",
            "conversation_history": [],
            "session_id": "c",
            "model": "hermes-agent",
            "attachments": [],
            "something_kohral_added_later": 42,
        }))
        .expect("the Hermes payload must parse");
        assert_eq!(parsed.input, "hi");
        assert_eq!(parsed.session_id, "c");
    }

    #[test]
    fn the_goal_keeps_the_request_first_and_labels_the_context() {
        let text = request().goal_text();
        assert!(text.starts_with("add a health endpoint"), "{text}");
        assert!(text.contains("## Operator instructions"));
        assert!(text.contains("Answer in French."));
        assert!(text.contains("## Conversation so far"));
        assert!(text.contains("**assistant**: hi"));
    }

    #[test]
    fn the_history_is_capped_to_the_most_recent_messages() {
        let mut request = request();
        request.conversation_history = (0..MAX_HISTORY_MESSAGES + 20)
            .map(|index| HistoryMessage {
                role: "user".to_owned(),
                content: format!("m{index}"),
            })
            .collect();
        let kept = request.capped_history();
        assert_eq!(kept.len(), MAX_HISTORY_MESSAGES);
        assert_eq!(kept[0].content, "m20", "the oldest messages are dropped");
        assert_eq!(kept[MAX_HISTORY_MESSAGES - 1].content, "m119");
    }

    #[test]
    fn a_huge_history_is_capped_by_size_too() {
        let mut request = request();
        request.conversation_history = (0..50)
            .map(|_| HistoryMessage {
                role: "user".to_owned(),
                content: "x".repeat(20 * 1024),
            })
            .collect();
        let kept = request.capped_history();
        assert!(kept.len() < 50, "kept {} messages", kept.len());
        let bytes: usize = kept.iter().map(|m| m.content.len() + m.role.len()).sum();
        assert!(bytes <= super::MAX_HISTORY_BYTES);
    }

    #[test]
    fn an_empty_input_is_rejected() {
        let mut request = request();
        request.input = "   ".to_owned();
        assert!(request.validate().is_err());
        let minimal = TurnRequest {
            input: "x".to_owned(),
            ..TurnRequest::default()
        };
        assert!(minimal.validate().is_ok());
    }

    #[test]
    fn a_kohral_run_is_headless_and_capped_to_the_run_timeout() {
        let run_id = RunId::new();
        let budget = kevin_config::schema::Budget {
            default_run_wall: Duration::from_secs(3600),
            ..kevin_config::schema::Budget::default()
        };
        let accepted = accept(
            run_id,
            &request(),
            "turn-1",
            Some("kohral:conv-1"),
            &Resolution::NoOverride,
            &budget,
            &env(),
        );
        assert!(accepted.command.auto_approve_plans);
        assert_eq!(
            accepted.command.budget.max_wall,
            Some(Duration::from_secs(60))
        );
        assert_eq!(accepted.session_id, "conv-1");
        assert_eq!(
            accepted.command.mode,
            RunMode::Kohral {
                turn_id: "turn-1".to_owned(),
                session_key: "kohral:conv-1".to_owned(),
                session_id: "conv-1".to_owned(),
            }
        );
        assert!(accepted.command.goal.cwd.ends_with(run_id.to_string()));
        assert!(accepted.model_override.is_none());
    }

    #[test]
    fn the_session_id_falls_back_to_the_session_key() {
        let mut request = request();
        request.session_id = String::new();
        assert_eq!(request.session_id(Some("kohral:abc")), "abc");
        assert_eq!(request.session_id(None), "default");
    }

    #[test]
    fn attachments_outside_the_upload_root_are_dropped() {
        let mut request = request();
        request.attachments = vec![
            TurnAttachment {
                path: "/tmp/kohral-uploads/c/m/a--x.png".to_owned(),
                size: Some(3),
                sha256: Some("ab".repeat(32)),
            },
            TurnAttachment {
                path: "/etc/passwd".to_owned(),
                size: None,
                sha256: None,
            },
        ];
        let artifacts = request.artifacts(std::path::Path::new("/tmp/kohral-uploads"));
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].uri, "file:///tmp/kohral-uploads/c/m/a--x.png");
    }

    #[test]
    fn idempotency_keys_follow_the_documented_grammar() {
        assert!(validate_idempotency_key("turn-0191f3a0").is_ok());
        assert!(validate_idempotency_key("a.b:c_d-1").is_ok());
        assert!(validate_idempotency_key("").is_err());
        assert!(validate_idempotency_key("-leading-dash").is_err());
        assert!(validate_idempotency_key("has space").is_err());
        assert!(validate_idempotency_key(&"x".repeat(257)).is_err());
        assert!(validate_idempotency_key(&"x".repeat(256)).is_ok());
    }
}
