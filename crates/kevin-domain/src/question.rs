//! The [`Question`] aggregate (`plan/02-domain-model.md` §Aggregates › Question).
//!
//! ```text
//! (none) ──AskQuestion──▶ open ──AnswerQuestion──▶ answered
//! open ──ExpireQuestion (default present)──▶ answered   (question.expired + question.answered{by: default})
//! open ──ExpireQuestion (no default)──▶ expired
//! ```
//!
//! Invariant: answered only once; `Block` policy questions never expire.

use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::aggregate::{Aggregate, EventMeta};
use crate::error::DomainError;
use crate::ids::{QuestionId, RunId, TaskId};
use crate::values::{Answer, QuestionOption, QuestionPolicy, QuestionStatus};

/// Aggregate type name (`EventEnvelope::aggregate_type`).
pub const QUESTION_AGGREGATE_TYPE: &str = "question";

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Creates a question (`question.asked`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskQuestion {
    /// New question id.
    pub question_id: QuestionId,
    /// Owning run.
    pub run_id: RunId,
    /// Task that asked (`None` for clarification questions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    /// Text.
    pub text: String,
    /// Options (empty = free text).
    #[serde(default)]
    pub options: Vec<QuestionOption>,
    /// Several options may be selected.
    #[serde(default)]
    pub multi_select: bool,
    /// Applied on expiry when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Answer>,
    /// Expiry policy.
    pub policy: QuestionPolicy,
}

impl AskQuestion {
    /// A free-text, blocking question.
    #[must_use]
    pub fn new(question_id: QuestionId, run_id: RunId, text: impl Into<String>) -> Self {
        Self {
            question_id,
            run_id,
            task_id: None,
            text: text.into(),
            options: Vec::new(),
            multi_select: false,
            default: None,
            policy: QuestionPolicy::Block,
        }
    }

    /// The default answer implied by the recommended option, if any.
    #[must_use]
    pub fn recommended_default(&self) -> Option<Answer> {
        self.options
            .iter()
            .find(|o| o.recommended)
            .map(|o| Answer::selected([o.label.clone()], Answer::DEFAULT_ANSWERED_BY))
    }
}

/// Answers an open question (`question.answered`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnswerQuestion {
    /// The answer.
    pub answer: Answer,
}

/// Expires an open question (`question.expired`, plus `question.answered`
/// when a default exists).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ExpireQuestion;

/// Every command the [`Question`] aggregate handles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionCommand {
    /// [`AskQuestion`].
    Ask(AskQuestion),
    /// [`AnswerQuestion`].
    Answer(AnswerQuestion),
    /// [`ExpireQuestion`].
    Expire(ExpireQuestion),
}

impl QuestionCommand {
    /// `snake_case` command name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            QuestionCommand::Ask(_) => "ask_question",
            QuestionCommand::Answer(_) => "answer_question",
            QuestionCommand::Expire(_) => "expire_question",
        }
    }
}

impl From<AskQuestion> for QuestionCommand {
    fn from(cmd: AskQuestion) -> Self {
        QuestionCommand::Ask(cmd)
    }
}

impl From<AnswerQuestion> for QuestionCommand {
    fn from(cmd: AnswerQuestion) -> Self {
        QuestionCommand::Answer(cmd)
    }
}

impl From<ExpireQuestion> for QuestionCommand {
    fn from(cmd: ExpireQuestion) -> Self {
        QuestionCommand::Expire(cmd)
    }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// Events of the `question` stream (internally tagged on `type`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum QuestionEvent {
    /// `question.asked`
    #[serde(rename = "question.asked")]
    Asked {
        /// Question id.
        question_id: QuestionId,
        /// Owning run.
        run_id: RunId,
        /// Asking task.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<TaskId>,
        /// Text.
        text: String,
        /// Options.
        options: Vec<QuestionOption>,
        /// Multi-select.
        multi_select: bool,
        /// Default answer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<Answer>,
        /// Policy.
        policy: QuestionPolicy,
    },
    /// `question.answered`
    #[serde(rename = "question.answered")]
    Answered {
        /// The answer.
        answer: Answer,
        /// Who answered (`default` when applied on expiry).
        answered_by: String,
    },
    /// `question.expired`
    #[serde(rename = "question.expired")]
    Expired {
        /// A default answer was applied (followed by `question.answered`).
        applied_default: bool,
    },
}

impl QuestionEvent {
    /// Every event type of the `question` stream, in catalog order.
    pub const TYPES: [&'static str; 3] =
        ["question.asked", "question.answered", "question.expired"];
}

impl EventMeta for QuestionEvent {
    fn event_type(&self) -> &'static str {
        match self {
            QuestionEvent::Asked { .. } => "question.asked",
            QuestionEvent::Answered { .. } => "question.answered",
            QuestionEvent::Expired { .. } => "question.expired",
        }
    }

    fn schema_version(&self) -> u16 {
        1
    }

    fn aggregate_type(&self) -> &'static str {
        QUESTION_AGGREGATE_TYPE
    }
}

// ---------------------------------------------------------------------------
// Aggregate
// ---------------------------------------------------------------------------

/// The question aggregate.
#[derive(Debug, Clone)]
pub struct Question {
    version: u64,
    id: QuestionId,
    run_id: RunId,
    task_id: Option<TaskId>,
    text: String,
    options: Vec<QuestionOption>,
    multi_select: bool,
    default: Option<Answer>,
    policy: Option<QuestionPolicy>,
    status: QuestionStatus,
    answer: Option<Answer>,
}

impl Default for Question {
    fn default() -> Self {
        Self {
            version: 0,
            id: QuestionId::nil(),
            run_id: RunId::nil(),
            task_id: None,
            text: String::new(),
            options: Vec::new(),
            multi_select: false,
            default: None,
            policy: None,
            status: QuestionStatus::Open,
            answer: None,
        }
    }
}

impl Question {
    /// Typed id.
    #[must_use]
    pub const fn question_id(&self) -> QuestionId {
        self.id
    }

    /// Owning run.
    #[must_use]
    pub const fn run_id(&self) -> RunId {
        self.run_id
    }

    /// Asking task, if any.
    #[must_use]
    pub const fn task_id(&self) -> Option<TaskId> {
        self.task_id
    }

    /// Text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Options.
    #[must_use]
    pub fn options(&self) -> &[QuestionOption] {
        &self.options
    }

    /// Multi-select.
    #[must_use]
    pub const fn multi_select(&self) -> bool {
        self.multi_select
    }

    /// Default answer.
    #[must_use]
    pub const fn default_answer(&self) -> Option<&Answer> {
        self.default.as_ref()
    }

    /// Policy (after `question.asked`).
    #[must_use]
    pub const fn policy(&self) -> Option<QuestionPolicy> {
        self.policy
    }

    /// Status.
    #[must_use]
    pub const fn status(&self) -> QuestionStatus {
        self.status
    }

    /// The answer once answered.
    #[must_use]
    pub const fn answer(&self) -> Option<&Answer> {
        self.answer.as_ref()
    }

    /// `status == open`.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.status == QuestionStatus::Open
    }

    /// Checks an answer against the question: non-empty; selected labels
    /// exist; at most one selection unless `multi_select`; free text only when
    /// the question has no options (or alongside a selection).
    pub fn validate_answer(&self, answer: &Answer) -> Result<(), DomainError> {
        if answer.is_empty() {
            return Err(DomainError::InvalidAnswer {
                reason: "answer is empty".to_owned(),
            });
        }
        if answer.answered_by.trim().is_empty() {
            return Err(DomainError::InvalidAnswer {
                reason: "answered_by is empty".to_owned(),
            });
        }
        for label in &answer.selected {
            if !self.options.iter().any(|o| &o.label == label) {
                return Err(DomainError::InvalidAnswer {
                    reason: format!("`{label}` is not an option"),
                });
            }
        }
        if !self.multi_select && answer.selected.len() > 1 {
            return Err(DomainError::InvalidAnswer {
                reason: "question is single-select".to_owned(),
            });
        }
        Ok(())
    }

    fn handle_ask(&self, cmd: &AskQuestion) -> Result<Vec<QuestionEvent>, DomainError> {
        if self.version > 0 {
            return Err(DomainError::AlreadyExists {
                aggregate: QUESTION_AGGREGATE_TYPE,
                id: self.id.as_uuid(),
            });
        }
        if cmd.text.trim().is_empty() {
            return Err(DomainError::invalid_value("text", "must not be empty"));
        }
        let mut labels: Vec<&str> = cmd.options.iter().map(|o| o.label.as_str()).collect();
        labels.sort_unstable();
        labels.dedup();
        if labels.len() != cmd.options.len() {
            return Err(DomainError::invalid_value(
                "options",
                "duplicate option label",
            ));
        }
        if let Some(default) = &cmd.default {
            let probe = Question {
                options: cmd.options.clone(),
                multi_select: cmd.multi_select,
                ..Question::default()
            };
            probe.validate_answer(default).map_err(|e| match e {
                DomainError::InvalidAnswer { reason } => {
                    DomainError::invalid_value("default", reason)
                }
                other => other,
            })?;
        }
        Ok(vec![QuestionEvent::Asked {
            question_id: cmd.question_id,
            run_id: cmd.run_id,
            task_id: cmd.task_id,
            text: cmd.text.clone(),
            options: cmd.options.clone(),
            multi_select: cmd.multi_select,
            default: cmd.default.clone(),
            policy: cmd.policy,
        }])
    }
}

impl Aggregate for Question {
    type Command = QuestionCommand;
    type Event = QuestionEvent;

    const TYPE: &'static str = QUESTION_AGGREGATE_TYPE;

    fn id(&self) -> Uuid {
        self.id.as_uuid()
    }

    fn version(&self) -> u64 {
        self.version
    }

    fn handle(&self, cmd: &QuestionCommand) -> Result<Vec<QuestionEvent>, DomainError> {
        if let QuestionCommand::Ask(c) = cmd {
            return self.handle_ask(c);
        }
        if self.version == 0 {
            return Err(DomainError::NotFound {
                aggregate: QUESTION_AGGREGATE_TYPE,
                id: self.id.as_uuid(),
            });
        }
        match self.status {
            QuestionStatus::Answered => return Err(DomainError::AlreadyAnswered),
            QuestionStatus::Expired => {
                return Err(DomainError::invalid_transition(
                    QUESTION_AGGREGATE_TYPE,
                    "expired",
                    cmd.name(),
                ));
            }
            QuestionStatus::Open => {}
        }
        match cmd {
            QuestionCommand::Ask(_) => unreachable!("handled above"),
            QuestionCommand::Answer(c) => {
                self.validate_answer(&c.answer)?;
                Ok(vec![QuestionEvent::Answered {
                    answer: c.answer.clone(),
                    answered_by: c.answer.answered_by.clone(),
                }])
            }
            QuestionCommand::Expire(_) => {
                if self.policy.is_some_and(|p| p.is_blocking()) {
                    return Err(DomainError::QuestionDoesNotExpire);
                }
                match &self.default {
                    Some(default) => {
                        let mut answer = default.clone();
                        Answer::DEFAULT_ANSWERED_BY.clone_into(&mut answer.answered_by);
                        Ok(vec![
                            QuestionEvent::Expired {
                                applied_default: true,
                            },
                            QuestionEvent::Answered {
                                answer,
                                answered_by: Answer::DEFAULT_ANSWERED_BY.to_owned(),
                            },
                        ])
                    }
                    None => Ok(vec![QuestionEvent::Expired {
                        applied_default: false,
                    }]),
                }
            }
        }
    }

    fn apply(&mut self, event: &QuestionEvent) {
        self.version += 1;
        match event {
            QuestionEvent::Asked {
                question_id,
                run_id,
                task_id,
                text,
                options,
                multi_select,
                default,
                policy,
            } => {
                self.id = *question_id;
                self.run_id = *run_id;
                self.task_id = *task_id;
                self.text.clone_from(text);
                self.options.clone_from(options);
                self.multi_select = *multi_select;
                self.default.clone_from(default);
                self.policy = Some(*policy);
                self.status = QuestionStatus::Open;
            }
            QuestionEvent::Answered { answer, .. } => {
                self.answer = Some(answer.clone());
                self.status = QuestionStatus::Answered;
            }
            QuestionEvent::Expired { applied_default } => {
                if !applied_default {
                    self.status = QuestionStatus::Expired;
                }
            }
        }
    }
}

impl fmt::Display for Question {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.status_name(), self.text)
    }
}

impl Question {
    const fn status_name(&self) -> &'static str {
        match self.status {
            QuestionStatus::Open => "open",
            QuestionStatus::Answered => "answered",
            QuestionStatus::Expired => "expired",
        }
    }
}
