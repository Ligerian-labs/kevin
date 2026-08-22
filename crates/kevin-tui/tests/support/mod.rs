//! Deterministic fixtures shared by the WS-17 acceptance tests.
//!
//! Every id, timestamp and cost is fixed so the `insta` snapshots only change
//! when the rendering changes.
#![allow(dead_code, reason = "each test binary uses a different subset")]

use chrono::{DateTime, Duration, TimeZone as _, Utc};
use kevin_api::dto::{
    AnswerDto, ArtifactDto, AttemptDto, BudgetDto, CostReportDto, CostRowDto, DrainStatusDto,
    EventDto, GoalDto, MemoryItemDto, ProposalDto, QuestionDto, QuestionOptionDto,
    QuestionPolicyDto, QuestionPolicyKind, RouteDto, RouteScoreDto, RunDto, RunModeDto,
    RunStatusDto, RunSummaryDto, TaskCountsDto, TaskDto, TaskLogLineDto, TaskSummaryDto, UsageDto,
    WorkerDoctorDto, WorkspaceDto,
};
use kevin_domain::ids::{
    ArtifactId, AttemptId, EvaluationId, EventId, MemoryItemId, ProposalId, QuestionId, RunId,
    TaskId,
};
use kevin_tui::Model;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use rust_decimal::Decimal;
use uuid::{Uuid, uuid};

// -- ids ---------------------------------------------------------------------

pub const RUN_A: Uuid = uuid!("0191f3a1-0000-7000-8000-00000000000a");
pub const RUN_B: Uuid = uuid!("0191f3b7-0000-7000-8000-00000000000b");
pub const TASK_1: Uuid = uuid!("0191f3a2-0000-7000-8000-000000000001");
pub const TASK_2: Uuid = uuid!("0191f3a2-0000-7000-8000-000000000002");
pub const TASK_3: Uuid = uuid!("0191f3a2-0000-7000-8000-000000000003");
pub const QUESTION_1: Uuid = uuid!("0191f3a3-0000-7000-8000-000000000001");
pub const QUESTION_2: Uuid = uuid!("0191f3a3-0000-7000-8000-000000000002");
pub const PROPOSAL_1: Uuid = uuid!("0191f3a4-0000-7000-8000-000000000001");
pub const EVALUATION_1: Uuid = uuid!("0191f3a5-0000-7000-8000-000000000001");
pub const LESSON_1: Uuid = uuid!("0191f3a6-0000-7000-8000-000000000001");
pub const ARTIFACT_1: Uuid = uuid!("0191f3a7-0000-7000-8000-000000000001");

pub fn run_a() -> RunId {
    RunId::from_uuid(RUN_A)
}
pub fn run_b() -> RunId {
    RunId::from_uuid(RUN_B)
}
pub fn task_1() -> TaskId {
    TaskId::from_uuid(TASK_1)
}
pub fn task_2() -> TaskId {
    TaskId::from_uuid(TASK_2)
}
pub fn task_3() -> TaskId {
    TaskId::from_uuid(TASK_3)
}
pub fn question_1() -> QuestionId {
    QuestionId::from_uuid(QUESTION_1)
}
pub fn question_2() -> QuestionId {
    QuestionId::from_uuid(QUESTION_2)
}

// -- clock -------------------------------------------------------------------

/// The instant the fixtures were "created at".
pub fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
        .single()
        .expect("a valid fixture instant")
}

/// The instant the screens render at (five minutes after [`t0`]).
pub fn now() -> DateTime<Utc> {
    t0() + Duration::minutes(5)
}

fn usd(units: i64, scale: u32) -> Decimal {
    Decimal::new(units, scale)
}

// -- value objects -----------------------------------------------------------

pub fn usage(cost_cents: i64, input: u64, output: u64) -> UsageDto {
    UsageDto {
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        cost_usd: Some(usd(cost_cents, 2)),
        wall_ms: 42_000,
    }
}

pub fn budget() -> BudgetDto {
    BudgetDto {
        max_usd: Some(usd(500, 2)),
        max_tokens: None,
        max_wall_ms: Some(7_200_000),
        max_attempts: 2,
        max_parallel: 4,
    }
}

pub fn route(worker: &str, model: &str) -> RouteDto {
    RouteDto {
        worker: worker.to_owned(),
        model: model.to_owned(),
        effort: Some("medium".to_owned()),
    }
}

// -- runs --------------------------------------------------------------------

pub fn run_summaries() -> Vec<RunSummaryDto> {
    vec![
        RunSummaryDto {
            id: run_a(),
            status: RunStatusDto::Executing,
            goal_excerpt: "Add a /healthz endpoint to the axum app and tests".to_owned(),
            usage: usage(42, 12_345, 3_400),
            task_counts: TaskCountsDto {
                total: 3,
                succeeded: 1,
                failed: 0,
                cancelled: 0,
                skipped: 0,
            },
            created_at: t0(),
            updated_at: t0() + Duration::minutes(4),
        },
        RunSummaryDto {
            id: run_b(),
            status: RunStatusDto::AwaitingPlanApproval,
            goal_excerpt: "Migrate the store to sqlx 0.9".to_owned(),
            usage: usage(7, 900, 120),
            task_counts: TaskCountsDto {
                total: 2,
                ..TaskCountsDto::default()
            },
            created_at: t0() - Duration::hours(2),
            updated_at: t0(),
        },
    ]
}

pub fn run_executing() -> RunDto {
    RunDto {
        id: run_a(),
        status: RunStatusDto::Executing,
        goal: GoalDto {
            text: "Add a /healthz endpoint to the axum app and tests".to_owned(),
            attachments: Vec::new(),
            cwd: std::path::PathBuf::from("/repo"),
            repo_kind: "git".to_owned(),
        },
        mode: RunModeDto::Interactive,
        budget: budget(),
        usage: usage(42, 12_345, 3_400),
        understanding: None,
        plan: None,
        open_questions: vec![question_1()],
        tasks: vec![
            TaskSummaryDto {
                id: task_1(),
                kind: "implement".to_owned(),
                title: "Add /healthz".to_owned(),
                status: "running".to_owned(),
                route: Some(route("claude", "sonnet")),
                attempt_count: 1,
                usage: usage(30, 9_000, 2_400),
            },
            TaskSummaryDto {
                id: task_2(),
                kind: "test".to_owned(),
                title: "Test /healthz".to_owned(),
                status: "pending".to_owned(),
                route: None,
                attempt_count: 0,
                usage: UsageDto::default(),
            },
        ],
        evaluation: None,
        created_at: t0(),
        updated_at: t0() + Duration::minutes(4),
        version: 12,
    }
}

pub fn plan_json() -> serde_json::Value {
    serde_json::json!({
        "rationale": "Implement the route first, then cover it with a test.",
        "tasks": [
            {
                "id": "t2", "title": "Test /healthz", "kind": "test",
                "instructions": "add an axum test", "depends_on": ["t1"],
                "acceptance_criteria": ["the test fails without the route"],
                "suggested_tier": "fast", "parallel_safe": false
            },
            {
                "id": "t1", "title": "Add /healthz", "kind": "implement",
                "instructions": "add the handler", "depends_on": [],
                "acceptance_criteria": ["GET /healthz returns 200", "it is registered in the router"],
                "suggested_tier": "balanced", "parallel_safe": true
            }
        ]
    })
}

pub fn run_awaiting_plan() -> RunDto {
    RunDto {
        id: run_b(),
        status: RunStatusDto::AwaitingPlanApproval,
        goal: GoalDto {
            text: "Migrate the store to sqlx 0.9".to_owned(),
            attachments: Vec::new(),
            cwd: std::path::PathBuf::from("/repo"),
            repo_kind: "jj".to_owned(),
        },
        mode: RunModeDto::Interactive,
        budget: budget(),
        usage: usage(7, 900, 120),
        understanding: None,
        plan: Some(kevin_api::dto::PlanDto(plan_json())),
        open_questions: Vec::new(),
        tasks: Vec::new(),
        evaluation: None,
        created_at: t0() - Duration::hours(2),
        updated_at: t0(),
        version: 4,
    }
}

// -- tasks -------------------------------------------------------------------

pub fn attempt(no: u8, status: &str, ended: bool) -> AttemptDto {
    AttemptDto {
        id: AttemptId::from_uuid(uuid!("0191f3a8-0000-7000-8000-000000000001")),
        no,
        route: route("claude", "sonnet"),
        status: status.to_owned(),
        workspace: Some(WorkspaceDto {
            root: std::path::PathBuf::from("/repo/.kevin/workspaces/t1"),
            kind: "git_worktree".to_owned(),
            base_rev: Some("deadbeef".to_owned()),
        }),
        worker_session_id: Some("sess-1".to_owned()),
        started_at: t0() + Duration::minutes(1),
        ended_at: ended.then(|| t0() + Duration::minutes(3)),
        usage: usage(30, 9_000, 2_400),
        failure: None,
    }
}

pub fn tasks() -> Vec<TaskDto> {
    vec![
        TaskDto {
            id: task_1(),
            run_id: run_a(),
            kind: "implement".to_owned(),
            title: "Add /healthz".to_owned(),
            status: "running".to_owned(),
            route: Some(route("claude", "sonnet")),
            attempts: vec![attempt(1, "running", false)],
            depends_on: Vec::new(),
            usage: usage(30, 9_000, 2_400),
            artifacts: vec![ArtifactDto {
                id: ArtifactId::from_uuid(ARTIFACT_1),
                run_id: run_a(),
                task_id: Some(task_1()),
                kind: "diff".to_owned(),
                uri: "file:///repo/.kevin/artifacts/t1.diff".to_owned(),
                sha256: None,
                bytes: Some(1_024),
                produced_by: "task".to_owned(),
                created_at: t0() + Duration::minutes(2),
            }],
            acceptance_criteria: vec!["GET /healthz returns 200".to_owned()],
        },
        TaskDto {
            id: task_2(),
            run_id: run_a(),
            kind: "test".to_owned(),
            title: "Test /healthz".to_owned(),
            status: "pending".to_owned(),
            route: None,
            attempts: Vec::new(),
            depends_on: vec![task_1()],
            usage: UsageDto::default(),
            artifacts: Vec::new(),
            acceptance_criteria: vec!["the test fails without the route".to_owned()],
        },
        TaskDto {
            id: task_3(),
            run_id: run_a(),
            kind: "review".to_owned(),
            title: "Review the diff".to_owned(),
            status: "succeeded".to_owned(),
            route: Some(route("codex", "gpt-mini")),
            attempts: vec![attempt(1, "succeeded", true)],
            depends_on: vec![task_1()],
            usage: usage(12, 3_345, 1_000),
            artifacts: Vec::new(),
            acceptance_criteria: Vec::new(),
        },
    ]
}

pub fn log_lines(from_seq: u64, count: u64) -> Vec<TaskLogLineDto> {
    (from_seq..from_seq + count)
        .map(|seq| TaskLogLineDto {
            seq,
            attempt: 1,
            at: t0() + Duration::seconds(i64::try_from(seq).unwrap_or(0)),
            kind: if seq % 3 == 0 {
                "tool_call"
            } else {
                "assistant"
            }
            .to_owned(),
            payload: serde_json::json!({ "text": format!("line {seq}") }),
        })
        .collect()
}

// -- questions ---------------------------------------------------------------

pub fn question_with_options() -> QuestionDto {
    QuestionDto {
        id: question_1(),
        run_id: run_a(),
        task_id: Some(task_1()),
        text: "Which axum version should the handler target?".to_owned(),
        options: vec![
            QuestionOptionDto {
                label: "axum 0.8".to_owned(),
                description: Some("the current major".to_owned()),
                recommended: true,
            },
            QuestionOptionDto {
                label: "axum 0.7".to_owned(),
                description: Some("what the repo pins today".to_owned()),
                recommended: false,
            },
        ],
        multi_select: false,
        default: Some(AnswerDto {
            selected: vec!["axum 0.8".to_owned()],
            free_text: None,
            answered_by: "default".to_owned(),
        }),
        policy: QuestionPolicyDto {
            kind: QuestionPolicyKind::DefaultAfter,
            timeout_ms: Some(600_000),
        },
        status: "open".to_owned(),
        answer: None,
        asked_at: t0() + Duration::minutes(1),
    }
}

pub fn question_multi_select() -> QuestionDto {
    QuestionDto {
        id: question_2(),
        run_id: run_b(),
        task_id: None,
        text: "Which crates may the migration touch?".to_owned(),
        options: vec![
            QuestionOptionDto {
                label: "kevin-store".to_owned(),
                description: None,
                recommended: true,
            },
            QuestionOptionDto {
                label: "kevin-memory".to_owned(),
                description: None,
                recommended: false,
            },
            QuestionOptionDto {
                label: "kevin-router".to_owned(),
                description: None,
                recommended: false,
            },
        ],
        multi_select: true,
        default: None,
        policy: QuestionPolicyDto {
            kind: QuestionPolicyKind::Block,
            timeout_ms: None,
        },
        status: "open".to_owned(),
        answer: None,
        asked_at: t0(),
    }
}

// -- side screens ------------------------------------------------------------

pub fn routes() -> Vec<RouteScoreDto> {
    vec![
        RouteScoreDto {
            kind: "implement".to_owned(),
            alias: "claude-sonnet".to_owned(),
            attempts: 40,
            successes: 36,
            mean_quality: Some(0.82),
            mean_cost_usd: Some(usd(1_200, 4)),
            mean_wall_ms: Some(64_000),
            sampled_score: Some(0.791),
        },
        RouteScoreDto {
            kind: "implement".to_owned(),
            alias: "codex-gpt-mini".to_owned(),
            attempts: 25,
            successes: 15,
            mean_quality: Some(0.61),
            mean_cost_usd: Some(usd(300, 4)),
            mean_wall_ms: Some(31_000),
            sampled_score: Some(0.604),
        },
        RouteScoreDto {
            kind: "test".to_owned(),
            alias: "pi-fast".to_owned(),
            attempts: 8,
            successes: 8,
            mean_quality: None,
            mean_cost_usd: None,
            mean_wall_ms: Some(9_500),
            sampled_score: None,
        },
    ]
}

pub fn lessons() -> Vec<MemoryItemDto> {
    vec![MemoryItemDto {
        id: MemoryItemId::from_uuid(LESSON_1),
        kind: "lesson".to_owned(),
        content: "axum handlers must be registered before the fallback route".to_owned(),
        tags: vec!["axum".to_owned(), "routing".to_owned()],
        importance: 0.80,
        similarity: None,
        source: serde_json::json!({ "run_id": RUN_A }),
        created_at: t0() - Duration::days(1),
    }]
}

pub fn proposals() -> Vec<ProposalDto> {
    vec![ProposalDto {
        id: ProposalId::from_uuid(PROPOSAL_1),
        evaluation_id: EvaluationId::from_uuid(EVALUATION_1),
        kind: "prompt".to_owned(),
        body: "Tell the planner to always add a test task after an implement task".to_owned(),
        status: "proposed".to_owned(),
        created_at: t0(),
    }]
}

pub fn workers() -> Vec<WorkerDoctorDto> {
    vec![
        WorkerDoctorDto {
            kind: "claude".to_owned(),
            enabled: true,
            binary: Some(std::path::PathBuf::from("/usr/local/bin/claude")),
            version: Some("2.4.1".to_owned()),
            auth_ready: Some(true),
            problems: Vec::new(),
        },
        WorkerDoctorDto {
            kind: "opencode".to_owned(),
            enabled: true,
            binary: None,
            version: None,
            auth_ready: Some(false),
            problems: vec!["binary not found on PATH".to_owned()],
        },
    ]
}

pub fn cost_report() -> CostReportDto {
    CostReportDto {
        total_usd: Some(usd(42, 2)),
        total_tokens: 15_745,
        rows: vec![CostRowDto {
            key: "claude-sonnet".to_owned(),
            usd: Some(usd(30, 2)),
            input_tokens: 9_000,
            output_tokens: 2_400,
            attempts: 1,
        }],
    }
}

pub fn drain() -> DrainStatusDto {
    DrainStatusDto {
        draining: false,
        running_runs: 1,
        running_attempts: 1,
    }
}

// -- events ------------------------------------------------------------------

pub fn event(position: u64, aggregate: Uuid, event_type: &str, version: u64) -> EventDto {
    EventDto {
        position,
        event_id: EventId::from_uuid(Uuid::from_u128(u128::from(position))),
        event_type: event_type.to_owned(),
        occurred_at: t0() + Duration::seconds(i64::try_from(position).unwrap_or(0)),
        aggregate_type: if event_type.starts_with("task.") {
            "task"
        } else {
            "run"
        }
        .to_owned(),
        aggregate_id: aggregate,
        aggregate_version: version,
        correlation_id: RUN_A,
        payload: serde_json::json!({}),
    }
}

// -- models ------------------------------------------------------------------

/// A model as it looks after the first snapshot poll, on the runs screen.
pub fn seeded_model() -> Model {
    let mut model = Model::new("http://127.0.0.1:7777/");
    model.now = now();
    model.runs.items = run_summaries();
    model.inbox.items = vec![question_with_options(), question_multi_select()];
    model.drain = Some(drain());
    model.routes.items = routes();
    model.lessons.lessons = lessons();
    model.lessons.proposals = proposals();
    model.workers.items = workers();
    model
}

/// [`seeded_model`] with run A open, three tasks and a followed transcript.
pub fn detail_model() -> Model {
    let mut model = seeded_model();
    model.screen = kevin_tui::Screen::RunDetail;
    model.detail.run = Some(run_executing());
    model.detail.tasks = tasks();
    model.detail.cost = Some(cost_report());
    model.detail.focused_task = Some(task_1());
    model.detail.log.extend(log_lines(1, 6));
    model.detail.log_seq = Some(6);
    for position in 1..=4u64 {
        model.detail.timeline.push(kevin_tui::model::PhaseEntry {
            at: t0() + Duration::seconds(i64::try_from(position).unwrap_or(0) * 30),
            event_type: [
                "run.started",
                "run.plan_approved",
                "task.started",
                "task.attempt_started",
            ][usize::try_from(position - 1).unwrap_or(0)]
            .to_owned(),
        });
    }
    model
}

// -- rendering ---------------------------------------------------------------

/// Draws `model` on a `width`×`height` `TestBackend` and returns the buffer.
pub fn render(model: &Model, width: u16, height: u16) -> String {
    let mut model = model.clone();
    model.size = (width, height);
    let mut terminal =
        Terminal::new(TestBackend::new(width, height)).expect("a TestBackend terminal");
    terminal
        .draw(|frame| kevin_tui::view(&model, frame))
        .expect("drawing never fails on a TestBackend");
    terminal.backend().to_string()
}
