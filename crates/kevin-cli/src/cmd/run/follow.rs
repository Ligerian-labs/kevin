//! Following a run: bus subscription (embedded) or store polling (`runs watch`),
//! event rendering and the terminal outcome.

use std::collections::HashSet;
use std::time::Duration;

use kevin_bus::{BusMessage, BusStream, Event};
use kevin_domain::run::{ApprovePlan, CancelRun, RejectPlan};
use kevin_domain::{Answer, QuestionId, QuestionOption, RunId};
use kevin_orchestrator::projections::RunOverviewRow;
use kevin_orchestrator::services::CommandContext;
use kevin_store::EventStore as _;
use serde::Deserialize;

use crate::cmd::answer::actor;
use crate::embedded::{Backend, EmbeddedRuntime};
use crate::{Ctx, render};

use super::prompt::{Decision, Prompter};

/// Poll interval of `kevin runs watch`.
const WATCH_POLL: Duration = Duration::from_millis(250);
/// Poll interval while waiting for the projections to catch up.
const PROJECTION_POLL: Duration = Duration::from_millis(50);
/// Events read per `kevin runs watch` round trip.
const PAGE: usize = 256;

/// How the followed run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// `run.completed`.
    Completed,
    /// `run.failed`.
    Failed {
        /// The failure reason was `budget_exhausted`.
        budget_exhausted: bool,
    },
    /// `run.cancelled`, by someone other than this CLI.
    Cancelled,
    /// Ctrl-C: this CLI cancelled the run.
    Interrupted,
    /// The stream ended before the run did.
    Detached,
}

impl Outcome {
    /// The process exit code (`plan/07-api-and-tui.md` §3).
    #[must_use]
    pub const fn exit_code(self) -> u8 {
        match self {
            Outcome::Completed => crate::exit::OK,
            Outcome::Failed {
                budget_exhausted: true,
            } => crate::exit::BUDGET_EXHAUSTED,
            Outcome::Failed { .. } | Outcome::Detached => crate::exit::FAILED,
            Outcome::Cancelled => crate::exit::CANCELLED,
            Outcome::Interrupted => crate::exit::INTERRUPTED,
        }
    }

    /// The `RunStatus` name this outcome corresponds to.
    #[must_use]
    pub const fn status(self) -> &'static str {
        match self {
            Outcome::Completed => "completed",
            Outcome::Failed { .. } | Outcome::Detached => "failed",
            Outcome::Cancelled | Outcome::Interrupted => "cancelled",
        }
    }

    /// One line for a human.
    #[must_use]
    pub const fn headline(self) -> &'static str {
        match self {
            Outcome::Completed => "completed",
            Outcome::Failed {
                budget_exhausted: true,
            } => "failed: budget exhausted",
            Outcome::Failed { .. } => "failed",
            Outcome::Cancelled => "cancelled",
            Outcome::Interrupted => "cancelled (interrupted)",
            Outcome::Detached => "detached before the run finished",
        }
    }
}

/// Follows one run on the bus, prompting for questions and plan approval.
pub struct Follow<'a> {
    runtime: &'a EmbeddedRuntime,
    run_id: RunId,
    json: bool,
    quiet: bool,
    auto_approve: bool,
    prompter: Option<Prompter>,
    answered: HashSet<QuestionId>,
    plan_decided: bool,
}

impl std::fmt::Debug for Follow<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Follow")
            .field("run_id", &self.run_id)
            .field("json", &self.json)
            .finish_non_exhaustive()
    }
}

impl<'a> Follow<'a> {
    /// A follower for `run_id`. Headless runs never prompt: the saga applies
    /// question defaults and auto-approves the plan (`plan/05` §3.3).
    #[must_use]
    pub fn new(
        runtime: &'a EmbeddedRuntime,
        run_id: RunId,
        ctx: &Ctx,
        auto_approve: bool,
        headless: bool,
    ) -> Self {
        Self {
            runtime,
            run_id,
            json: ctx.global.json,
            quiet: ctx.global.quiet,
            auto_approve,
            prompter: (!headless).then(Prompter::new),
            answered: HashSet::new(),
            plan_decided: auto_approve,
        }
    }

    /// Consumes the stream until the run is terminal or Ctrl-C is pressed.
    pub async fn follow(&mut self, mut stream: BusStream) -> anyhow::Result<Outcome> {
        let mut cancelling = false;
        loop {
            tokio::select! {
                biased;
                signal = tokio::signal::ctrl_c(), if !cancelling => {
                    signal?;
                    cancelling = true;
                    self.note("interrupted: cancelling the run…");
                    self.cancel().await;
                }
                message = stream.next() => match message {
                    Some(BusMessage::Live(event)) => {
                        self.emit(&event.envelope, event.position);
                        if let Some(outcome) = self.handle(&event.envelope).await? {
                            return Ok(match (cancelling, outcome) {
                                (true, Outcome::Cancelled) => Outcome::Interrupted,
                                (_, outcome) => outcome,
                            });
                        }
                    }
                    Some(BusMessage::Lagged { from, to }) => {
                        self.note(&format!("event stream lagged ({from}..={to} dropped)"));
                    }
                    None => return Ok(Outcome::Detached),
                },
            }
        }
    }

    fn emit(&self, envelope: &Event, position: u64) {
        if self.json {
            render::json_line(&event_json(position, envelope));
        } else if !self.quiet {
            render::line(&event_line(position, envelope));
        }
    }

    fn note(&self, text: &str) {
        if !self.json && !self.quiet {
            eprintln!("{text}");
        }
    }

    async fn handle(&mut self, envelope: &Event) -> anyhow::Result<Option<Outcome>> {
        match envelope.event_type {
            "run.completed" => return Ok(Some(Outcome::Completed)),
            "run.failed" => {
                let budget_exhausted =
                    envelope.payload.get("reason").and_then(serde_json::Value::as_str)
                        == Some("budget_exhausted");
                return Ok(Some(Outcome::Failed { budget_exhausted }));
            }
            "run.cancelled" => return Ok(Some(Outcome::Cancelled)),
            "question.asked" => self.on_question(envelope).await,
            "run.plan_proposed" => self.on_plan(envelope).await,
            _ => {}
        }
        Ok(None)
    }

    async fn on_question(&mut self, envelope: &Event) {
        let question_id = QuestionId::from_uuid(envelope.aggregate_id);
        if !self.answered.insert(question_id) {
            return;
        }
        let Ok(asked) = serde_json::from_value::<Asked>(envelope.payload.clone()) else {
            return;
        };
        let Some(prompter) = self.prompter.as_mut() else {
            self.hint_question(question_id, &asked);
            return;
        };
        let Some(answer) = prompter
            .ask(
                &asked.text,
                &asked.options,
                asked.multi_select,
                asked.default.as_ref(),
            )
            .await
        else {
            self.hint_question(question_id, &asked);
            return;
        };
        let backend = self.runtime.backend();
        let ctx = CommandContext::user(backend.ids().as_ref(), self.run_id, actor());
        if let Err(err) = backend
            .question_service()
            .answer(
                question_id,
                kevin_domain::question::AnswerQuestion { answer },
                &ctx,
            )
            .await
        {
            self.note(&format!("answering {question_id} failed: {err}"));
        }
    }

    fn hint_question(&self, question_id: QuestionId, asked: &Asked) {
        let hint = format!("kevin answer {question_id} <option>");
        if self.json {
            render::json_line(&serde_json::json!({
                "type": "question",
                "question_id": question_id,
                "text": asked.text,
                "options": asked.options,
                "multi_select": asked.multi_select,
                "hint": hint,
            }));
        } else if !self.quiet {
            render::line(&format!("question: {}\n  answer with: {hint}", asked.text));
        }
    }

    async fn on_plan(&mut self, envelope: &Event) {
        if self.auto_approve || self.plan_decided {
            return;
        }
        let plan = envelope.payload.get("plan").cloned().unwrap_or_default();
        let titles = plan_titles(&plan);
        let Some(prompter) = self.prompter.as_mut() else {
            self.hint_plan(&titles);
            return;
        };
        let decision = prompter.approve_plan(&titles).await;
        let backend = self.runtime.backend();
        let ctx = CommandContext::user(backend.ids().as_ref(), self.run_id, actor());
        let by = actor();
        let result = match decision {
            Some(Decision::Approve) => {
                self.plan_decided = true;
                backend
                    .run_service()
                    .approve_plan(self.run_id, ApprovePlan { by }, &ctx)
                    .await
                    .map(|_| ())
            }
            Some(Decision::Reject(feedback)) => backend
                .run_service()
                .reject_plan(self.run_id, RejectPlan { by, feedback }, &ctx)
                .await
                .map(|_| ()),
            None => {
                self.hint_plan(&titles);
                return;
            }
        };
        if let Err(err) = result {
            self.note(&format!("plan decision failed: {err}"));
        }
    }

    fn hint_plan(&self, titles: &[String]) {
        let hint = format!("kevin approve {} | kevin reject {} --feedback …", self.run_id, self.run_id);
        if self.json {
            render::json_line(&serde_json::json!({
                "type": "plan",
                "run_id": self.run_id,
                "tasks": titles,
                "hint": hint,
            }));
        } else if !self.quiet {
            render::line(&format!("plan proposed ({} tasks); {hint}", titles.len()));
        }
    }

    async fn cancel(&self) {
        let backend = self.runtime.backend();
        let by = actor();
        let ctx = CommandContext::user(backend.ids().as_ref(), self.run_id, by.clone());
        if let Err(err) = backend
            .run_service()
            .cancel(
                self.run_id,
                CancelRun {
                    by,
                    reason: "interrupted (Ctrl-C)".to_owned(),
                },
                &ctx,
            )
            .await
        {
            self.note(&format!("cancelling the run failed: {err}"));
        }
    }
}

/// `question.asked` payload fields the prompt needs.
#[derive(Debug, Deserialize)]
pub struct Asked {
    /// The question.
    pub text: String,
    /// Offered options.
    #[serde(default)]
    pub options: Vec<QuestionOption>,
    /// Whether several options may be selected.
    #[serde(default)]
    pub multi_select: bool,
    /// The default answer, when the policy has one.
    #[serde(default)]
    pub default: Option<Answer>,
}

fn plan_titles(plan: &serde_json::Value) -> Vec<String> {
    plan.get("tasks")
        .and_then(serde_json::Value::as_array)
        .map(|tasks| {
            tasks
                .iter()
                .map(|task| {
                    let kind = task.get("kind").and_then(serde_json::Value::as_str).unwrap_or("task");
                    let title = task
                        .get("title")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("(untitled)");
                    format!("{kind}: {title}")
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// One `event` line of the `--json` protocol.
#[must_use]
pub fn event_json(position: u64, envelope: &Event) -> serde_json::Value {
    serde_json::json!({
        "type": "event",
        "position": position,
        "event_id": envelope.event_id,
        "event_type": envelope.event_type,
        "occurred_at": envelope.occurred_at,
        "aggregate_type": envelope.aggregate_type,
        "aggregate_id": envelope.aggregate_id,
        "aggregate_version": envelope.aggregate_version,
        "run_id": envelope.correlation_id,
        "actor": envelope.actor,
        "payload": envelope.payload,
    })
}

/// One human line of the event stream.
#[must_use]
pub fn event_line(position: u64, envelope: &Event) -> String {
    let _ = position;
    let detail = detail(envelope.event_type, &envelope.payload);
    format!(
        "[{}] {:<28} {detail}",
        render::clock(envelope.occurred_at),
        envelope.event_type
    )
}

#[allow(clippy::too_many_lines)]
fn detail(event_type: &str, payload: &serde_json::Value) -> String {
    let text = |key: &str| {
        payload
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    match event_type {
        "run.started" => payload
            .get("goal")
            .and_then(|g| g.get("text"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned(),
        "run.understanding_started" => format!("planner={}", route(payload.get("planner_route"))),
        "run.understanding_completed" => payload
            .get("understanding")
            .and_then(|u| u.get("objective"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        "run.plan_proposed" => format!(
            "{} tasks (revision {})",
            payload
                .get("plan")
                .and_then(|p| p.get("tasks"))
                .and_then(serde_json::Value::as_array)
                .map_or(0, Vec::len),
            payload.get("revision").and_then(serde_json::Value::as_u64).unwrap_or(0),
        ),
        "run.plan_approved" => format!("by {}", text("by")),
        "run.plan_rejected" => format!("by {}: {}", text("by"), text("feedback")),
        "run.execution_started" => format!(
            "{} tasks",
            payload
                .get("task_ids")
                .and_then(serde_json::Value::as_array)
                .map_or(0, Vec::len)
        ),
        "run.task_terminal_noted" => format!(
            "{} {}",
            short(payload.get("task_id")),
            text("outcome")
        ),
        "run.budget_exhausted" => format!(
            "{} limit {} reached",
            text("dimension"),
            payload.get("limit").map_or_else(String::new, ToString::to_string)
        ),
        "run.integrated" | "run.completed" | "task.attempt_succeeded" => text("summary"),
        "run.failed" => format!(
            "{}{}",
            text("reason"),
            payload
                .get("message")
                .and_then(serde_json::Value::as_str)
                .map_or_else(String::new, |m| format!(": {m}"))
        ),
        "run.cancelled" => format!("by {}: {}", text("by"), text("reason")),
        "task.created" => format!("{} {}", text("kind"), spec_title(payload)),
        "task.routed" => format!("{} → {}", short(payload.get("task_id")), route(payload.get("route"))),
        "task.attempt_started" => format!("attempt #{}", payload.get("attempt_no").and_then(serde_json::Value::as_u64).unwrap_or(1)),
        "task.attempt_failed" => format!("{}: {}", text("class"), text("message")),
        "task.progressed" => text("note"),
        "task.input_requested" => text("question"),
        "task.cancelled" | "task.skipped" => text("reason"),
        "question.asked" => text("text"),
        "question.answered" => format!("by {}", text("answered_by")),
        _ => String::new(),
    }
}

fn spec_title(payload: &serde_json::Value) -> String {
    payload
        .get("spec")
        .and_then(|s| s.get("title"))
        .or_else(|| payload.get("title"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn route(value: Option<&serde_json::Value>) -> String {
    value
        .and_then(|r| r.get("model"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("-")
        .to_owned()
}

fn short(value: Option<&serde_json::Value>) -> String {
    value
        .and_then(serde_json::Value::as_str)
        .map_or_else(|| "-".to_owned(), |s| s.chars().take(8).collect())
}

// ---------------------------------------------------------------------------
// Store-backed following (`kevin runs watch`)
// ---------------------------------------------------------------------------

/// Follows a run by polling `core.events`; no engine is booted.
pub async fn watch_store(
    backend: &Backend,
    run_id: RunId,
    ctx: &Ctx,
) -> anyhow::Result<Outcome> {
    let mut position = 0_u64;
    loop {
        let page = backend.store().read_all(position, PAGE).await?;
        if page.is_empty() {
            tokio::time::sleep(WATCH_POLL).await;
            continue;
        }
        position = page.last().map_or(position, |e| e.position);
        for stored in page {
            if stored.envelope.correlation_id != run_id.as_uuid() {
                continue;
            }
            if ctx.global.json {
                render::json_line(&event_json(stored.position, &stored.envelope));
            } else if !ctx.global.quiet {
                render::line(&event_line(stored.position, &stored.envelope));
            }
            match stored.envelope.event_type {
                "run.completed" => return Ok(Outcome::Completed),
                "run.failed" => {
                    let budget_exhausted = stored
                        .envelope
                        .payload
                        .get("reason")
                        .and_then(serde_json::Value::as_str)
                        == Some("budget_exhausted");
                    return Ok(Outcome::Failed { budget_exhausted });
                }
                "run.cancelled" => return Ok(Outcome::Cancelled),
                _ => {}
            }
        }
    }
}

/// Waits until `orch.run_overview` reflects the terminal state, so the summary
/// is not printed from a stale row. Returns the last row it saw.
pub async fn await_projection(
    backend: &Backend,
    run_id: RunId,
    grace: Duration,
) -> anyhow::Result<Option<RunOverviewRow>> {
    let deadline = tokio::time::Instant::now() + grace;
    let mut last = None;
    loop {
        let row = backend.read_models().run(run_id.as_uuid()).await?;
        let terminal = row
            .as_ref()
            .is_some_and(|r| matches!(r.status.as_str(), "completed" | "failed" | "cancelled"));
        last = row.or(last);
        if terminal || tokio::time::Instant::now() >= deadline {
            return Ok(last);
        }
        tokio::time::sleep(PROJECTION_POLL).await;
    }
}
