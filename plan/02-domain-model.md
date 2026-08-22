# 02 — Domain model

All types live in `crates/kevin-domain` (no IO). Names below are the canonical
ubiquitous language; implementers must not rename them.

## Identifiers and value objects

| Type | Representation | Notes |
|---|---|---|
| `RunId`, `TaskId`, `QuestionId`, `AttemptId`, `EvaluationId`, `MemoryItemId`, `EventId`, `CommandId` | `uuid` v7 newtypes | v7 gives time ordering; Display = plain uuid string. |
| `TaskKind` | enum `Understand, Clarify, Plan, Research, Implement, Test, Review, Refactor, Debug, Write, Ops, Evaluate, Integrate, Custom(String)` | serde as snake_case; `custom:<name>` string form. |
| `WorkerKind` | enum `Claude, Codex, Pi, Opencode, Fake` | |
| `ModelAlias` | `String` newtype, validated `[a-z0-9][a-z0-9._-]*` | Key into config `[models.<alias>]`. |
| `Route` | `{ worker: WorkerKind, model: ModelAlias, effort: Option<Effort> }` | |
| `Effort` | enum `Low, Medium, High, XHigh, Max` | mapped per worker (claude: effort flag n/a in CLI → model choice; pi: `--thinking`; opencode: `--variant`; codex: `-c model_reasoning_effort`). |
| `Budget` | `{ max_usd: Option<Decimal>, max_tokens: Option<u64>, max_wall: Option<Duration>, max_attempts: u8, max_parallel: u16 }` | all optional except attempts (default 2) and parallel (default 4). |
| `Usage` | `{ input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, cost_usd: Option<Decimal>, wall_ms }` | additive; `cost_usd` computed by router's price table when the worker doesn't report it. |
| `Workspace` | `{ root: PathBuf, kind: InPlace | GitWorktree{branch} | JjWorkspace{name}, base_rev: Option<String> }` | |
| `ArtifactRef` | `{ id, kind: Diff|File|PrUrl|Report|Json|Transcript, uri, sha256, bytes }` | Artifacts stored in `orch.artifacts`; large blobs on disk under data_dir. |
| `RubricScore` | `{ criterion: String, score: u8 /*0..=10*/, rationale: String }` | |
| `Verdict` | enum `Accept, AcceptWithFixes, Reject` | |

## Aggregates

### `Run` (root)

Fields: `id`, `goal: Goal{ text, attachments, cwd, repo_kind }`, `requested_by`,
`mode: Interactive | Headless | Kohral{turn_id, session_key, session_id}`, `budget`,
`status`, `understanding: Option<Understanding>`, `plan: Option<Plan>`,
`task_ids: Vec<TaskId>`, `open_question_ids`, `usage: Usage` (rolled up from
task events), `version`.

State machine:

```text
received ──UnderstandingStarted──▶ understanding ──UnderstandingCompleted──▶ awaiting_answers
                                                                    │ (no questions)
awaiting_answers ──AllQuestionsAnswered──▶ planning ◀───────────────┘
planning ──PlanProposed──▶ (mode Interactive: awaiting_plan_approval) ──PlanApproved──▶ executing
planning ──PlanProposed──▶ (mode Headless/Kohral, auto_approve=true) ──▶ executing
executing ──AllTasksTerminal──▶ integrating ──RunIntegrated──▶ evaluating ──RunEvaluated──▶ completed
any non-terminal ──RunCancelled──▶ cancelled
any non-terminal ──RunFailed{reason}──▶ failed
```

Invariants:
- A run cannot enter `executing` without an approved plan (or auto-approval).
- Budget: `usage.cost_usd <= budget.max_usd`; exceeding it emits
  `BudgetExhausted` and the run fails with `reason = budget_exhausted` after
  cancelling running attempts.
- A run in `awaiting_answers` has ≥1 open question; answering the last one
  transitions to `planning` (command `AnswerQuestion` handled by `Question`
  aggregate; the run reacts to `QuestionAnswered` via the process manager).
- Terminal runs reject every command except `Evaluate` (re-evaluation) and
  `Remember`.

Commands (handled by `Run`): `StartRun`, `RecordUnderstanding`, `ProposePlan`,
`ApprovePlan`, `RejectPlan{feedback}`, `NoteTaskTerminal` (from process manager),
`MarkIntegrated`, `MarkEvaluated`, `CancelRun`, `FailRun`, `Evaluate` (re-run the
judge on a terminal run; creates a new `Evaluation`).

### `Task`

Fields: `id`, `run_id`, `kind`, `spec: TaskSpec{ title, instructions,
inputs: Vec<ArtifactRef>, acceptance_criteria: Vec<String>, depends_on:
Vec<TaskId>, workspace_policy, output_schema: Option<JsonSchema> }`,
`route: Option<Route>`, `attempts: Vec<Attempt>`, `status`, `budget`, `usage`,
`artifacts`, `version`.

State machine:

```text
pending ──TaskRouted──▶ routed ──TaskAttemptStarted──▶ running
running ──TaskAttemptSucceeded──▶ succeeded
running ──TaskAttemptFailed──▶ (attempts < max) ──TaskRetried──▶ routed
                            └─▶ failed
running ──TaskInputRequested──▶ awaiting_input ──TaskInputProvided──▶ running
pending|routed|running|awaiting_input ──TaskCancelled──▶ cancelled
pending ──TaskSkipped{reason}──▶ skipped   (dependency failed)
```

Invariants: one running attempt at a time; `TaskAttemptStarted` requires a
route; retry re-routes (may pick another model — router decides); total
attempts ≤ `budget.max_attempts`.

Commands: `CreateTask`, `RouteTask`, `StartAttempt`, `RecordProgress`,
`RequestInput`, `ProvideInput`, `SucceedAttempt{artifacts, usage, summary}`,
`FailAttempt{class, message, usage}`, `RetryTask`, `CancelTask`, `SkipTask`.

Failure classes (value enum `FailureClass`): `Transient` (timeout, rate limit,
worker crashed), `Permanent` (invalid spec, tool refused, output schema
violation after N tries), `Budget`, `Cancelled`, `RuntimeRestarted`.

### `Question`

Fields: `id`, `run_id`, `task_id: Option<TaskId>`, `text`, `options:
Vec<QuestionOption{label, description, recommended}>`, `multi_select`,
`default: Option<Answer>`, `policy: Block | DefaultAfter{timeout}`, `status:
Open | Answered | Expired`, `answer: Option<Answer{selected: Vec<String>,
free_text: Option<String>, answered_by}>`.

Commands: `AskQuestion`, `AnswerQuestion`, `ExpireQuestion`. Invariant: answer
only once; `Expired` applies `default` if present (emits
`QuestionAnswered{answered_by: "default"}`), otherwise the run fails with
`reason = unanswered_question` (Kohral-mode runs never ask: defaults are
applied at once and questions without a default become planner assumptions,
see [08 §3](./08-kohral-runtime.md)).

### `Evaluation`

Fields: `id`, `subject: Run(RunId) | Task(TaskId)`, `rubric_id`, `judge_route`,
`scores: Vec<RubricScore>`, `overall: f32 (0..1)`, `verdict`, `lessons:
Vec<String>`, `proposals: Vec<Proposal{kind: Prompt|Config|Routing, body,
status: Proposed|Accepted|Rejected}>`, `usage`.

Commands: `RecordEvaluation`, `AcceptProposal`, `RejectProposal`.

### Routing: `RouteScore` (one aggregate per `(task_kind, model_alias)`)

Fields: `attempts`, `successes`, `sum_quality`, `sum_cost_usd`, `sum_wall_ms`,
`alpha`, `beta` (Beta distribution params for Thompson sampling), `last_used`.
Commands: `RecordRouteOutcome{quality: Option<f32>, success: bool, cost, wall}`.
Event: `RouteScoreUpdated`.

### Memory: `MemoryItem`

Fields: `id`, `kind: Lesson | Preference | Fact | RunSummary | ArtifactSummary`,
`content`, `tags`, `source: { run_id?, task_id?, evaluation_id?, actor }`,
`embedding_model`, `importance: f32`, `created_at`, `superseded_by`.
Commands: `StoreMemoryItem`, `SupersedeMemoryItem`, `ForgetMemoryItem`.
(Embedding vector is stored by kevin-memory; the aggregate only knows the
model name.)

## Event envelope

```rust
pub struct EventEnvelope<E> {
    pub event_id: EventId,            // uuid v7
    pub event_type: &'static str,     // "run.started" (context.past_tense)
    pub schema_version: u16,          // per event type, additive evolution
    pub occurred_at: DateTime<Utc>,
    pub aggregate_type: &'static str, // "run" | "task" | "question" | ...
    pub aggregate_id: Uuid,
    pub aggregate_version: u64,       // 1-based within the stream
    pub correlation_id: Uuid,         // always the RunId when one exists
    pub causation_id: Option<Uuid>,   // command_id or event_id that caused it
    pub actor: Actor,                 // User{name} | System{component} | Worker{kind} | Kohral{agent_id}
    pub payload: E,                   // serde_json
}
```

Global ordering: `core.events.position BIGSERIAL`. Per-stream ordering:
`(aggregate_type, aggregate_id, aggregate_version)` unique.

## Event catalog (v1)

| Event type | Aggregate | Payload (key fields) |
|---|---|---|
| `run.started` | run | goal, mode, budget, requested_by, cwd |
| `run.understanding_started` | run | planner_route |
| `run.understanding_completed` | run | understanding {objective, assumptions[], risks[], success_criteria[], proposed_questions[], context_refs[]} , usage |
| `run.plan_proposed` | run | plan {tasks[] (TaskSpec + suggested_route), edges[], rationale}, usage |
| `run.plan_approved` / `run.plan_rejected` | run | by, feedback? |
| `run.execution_started` | run | task_ids |
| `run.integrated` | run | artifacts[], summary |
| `run.evaluated` | run | evaluation_id, overall, verdict |
| `run.completed` | run | summary, usage |
| `run.failed` | run | reason, class, usage |
| `run.cancelled` | run | by, reason |
| `run.budget_exhausted` | run | dimension (usd/tokens/wall), limit, actual |
| `task.created` | task | run_id, kind, spec |
| `task.routed` | task | route, selection {policy, candidates[], scores[]} |
| `task.attempt_started` | task | attempt_id, route, workspace, worker_session_id? |
| `task.progressed` | task | attempt_id, summary, usage_delta, log_seq |
| `task.input_requested` | task | attempt_id, question_id |
| `task.input_provided` | task | attempt_id, question_id |
| `task.attempt_succeeded` | task | attempt_id, artifacts[], summary, usage |
| `task.attempt_failed` | task | attempt_id, class, message, usage |
| `task.retried` | task | next_attempt_no, reason |
| `task.cancelled` / `task.skipped` | task | reason |
| `question.asked` | question | run_id, task_id?, text, options[], policy |
| `question.answered` | question | answer, answered_by |
| `question.expired` | question | applied_default: bool |
| `evaluation.recorded` | evaluation | subject, rubric_id, scores[], overall, verdict, lessons[], proposals[] |
| `evaluation.proposal_accepted` / `_rejected` | evaluation | proposal_id, by, note? *(v2)* |
| `routing.score_updated` | route_score | task_kind, alias, stats after |
| `memory.item_stored` | memory_item | kind, content, tags, source |
| `memory.item_superseded` / `memory.item_forgotten` | memory_item | — |

Events are past tense, context-qualified, additive. A breaking payload change
bumps `schema_version` and the store keeps an upcaster registry
(`kevin-store::upcast`). `Upcasters::domain()` is the registry
`PgEventStore::new` installs; every version listed above as *(vN)* has an entry
in it.

Schema versions in use:

| Event | Version | Change |
|---|---|---|
| `evaluation.proposal_accepted` / `_rejected` | 2 | `note?` added: the operator's reason for the decision, so `kevin proposals accept\|reject --note` and `POST /api/v1/proposals/{id}/accept\|reject {note?}` ([07](./07-api-and-tui.md)) persist it instead of only printing it. v1 payloads upcast to `note: null`. |
| everything else | 1 | — |

## Process manager: `RunSaga`

`RunActor` hosts a saga that reacts to events and issues commands. It is the
only place with cross-aggregate flow logic:

| On event | Do |
|---|---|
| `run.started` | retrieve memory context (top-k lessons/preferences/summaries for goal + repo) → `RecordUnderstanding` via planner worker |
| `run.understanding_completed` | for each proposed question with confidence below threshold → `AskQuestion`; if none → `ProposePlan` |
| `question.answered` (last open) | `ProposePlan` via planner worker (with answers) |
| `run.plan_approved` | `CreateTask` ×N, then for every task whose deps are satisfied: `RouteTask` (router query) → `StartAttempt` (respecting semaphores) |
| `task.attempt_succeeded` | unblock dependents; if all terminal → `MarkIntegrated` via integrator step |
| `task.attempt_failed` | classify → `RetryTask` or `SkipTask` dependents / `FailRun` per policy |
| `run.integrated` | evaluator: `RecordEvaluation` for run (and for tasks lacking one) |
| `evaluation.recorded` | router: `RecordRouteOutcome` per task; memory: store lessons, run summary |
| `run.budget_exhausted` | cancel running attempts, `FailRun` |

The saga's own progress is implicit in aggregate state (no separate saga
table): on restart, `RunActor`s are rebuilt for every non-terminal run by
replaying its stream and resuming at the first unsatisfied step.

## Read models

| Projection | Source events | Consumers |
|---|---|---|
| `orch.run_overview` | run.* | API list/get, TUI runs pane |
| `orch.task_board` | task.* | TUI board, API |
| `orch.question_inbox` | question.* | TUI inbox, API, Kohral "input_required" |
| `orch.cost_ledger` | task.attempt_*, run.* | `kevin cost`, TUI footer |
| `routing.route_leaderboard` | routing.score_updated | `kevin routes`, TUI |
| `eval.proposals_inbox` | evaluation.* | `kevin proposals` |
| `memory.lessons_view` | memory.* | `kevin lessons`, planner context |
