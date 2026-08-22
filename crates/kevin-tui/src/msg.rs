//! What goes into the reducer ([`Msg`]) and what comes out of it ([`Cmd`]).
//!
//! `plan/07-api-and-tui.md` §4: `update(&mut Model, Msg) -> Vec<Cmd>` is pure.
//! Every side effect — an HTTP call, a subscription, a clipboard write — is a
//! [`Cmd`] the runtime performs and feeds back as another [`Msg`]. Nothing in
//! this module touches the network, so the whole state machine is unit-testable.

use kevin_api::dto::{
    AnswerRequest, CostReportDto, DrainStatusDto, EventDto, MemoryItemDto, ProposalDto,
    QuestionDto, RouteScoreDto, RunDto, RunSummaryDto, TaskDto, TaskLogLineDto, WorkerDoctorDto,
};
use kevin_domain::ids::{MemoryItemId, ProposalId, QuestionId, RunId, TaskId};

use crate::keys::KeyPress;

/// Everything that can move the model forward.
#[derive(Debug, Clone)]
pub enum Msg {
    /// A key was pressed.
    Key(KeyPress),
    /// The periodic poll fired; `now` is the wall clock the view renders ages
    /// against (injected so snapshots are deterministic).
    Tick(chrono::DateTime<chrono::Utc>),
    /// The terminal was resized to `cols`×`rows`.
    Resized(u16, u16),

    /// `GET /api/v1/runs` answered.
    RunsLoaded(Vec<RunSummaryDto>),
    /// `GET /api/v1/runs/{id}` answered.
    RunLoaded(Box<RunDto>),
    /// `GET /api/v1/runs/{id}/tasks` answered.
    TasksLoaded(RunId, Vec<TaskDto>),
    /// Transcript lines for the focused task (poll or `log/stream`).
    LogLines(TaskId, Vec<TaskLogLineDto>),
    /// `GET /api/v1/questions?status=open` answered.
    QuestionsLoaded(Vec<QuestionDto>),
    /// `POST /api/v1/questions/{id}/answer` answered.
    QuestionAnswered(Box<QuestionDto>),
    /// `GET /api/v1/routes` answered.
    RoutesLoaded(Vec<RouteScoreDto>),
    /// `GET /api/v1/lessons` answered.
    LessonsLoaded(Vec<MemoryItemDto>),
    /// `GET /api/v1/proposals` answered.
    ProposalsLoaded(Vec<ProposalDto>),
    /// `GET /api/v1/workers` answered.
    WorkersLoaded(Vec<WorkerDoctorDto>),
    /// `GET /api/v1/cost` answered.
    CostLoaded(Box<CostReportDto>),
    /// `GET /api/v1/maintenance/drain` answered.
    DrainLoaded(DrainStatusDto),

    /// One event off the SSE firehose.
    ApiEvent(Box<EventDto>),
    /// The server (or the client's gap detector) asked for a full refetch.
    Resync,
    /// A stream broke; the client is reconnecting on its own.
    StreamError(String),
    /// A one-shot call failed.
    ClientError(String),
    /// Something worth telling the operator (a command was accepted…).
    Notice(String),

    /// Leave.
    Quit,
}

/// A side effect for the runtime to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    /// Refetch the runs list.
    FetchRuns,
    /// Refetch one run.
    FetchRun(RunId),
    /// Refetch a run's task board.
    FetchTasks(RunId),
    /// Fetch transcript lines after `after_seq`.
    FetchTaskLog {
        /// Which task.
        task_id: TaskId,
        /// Only lines with a greater `seq`.
        after_seq: Option<u64>,
    },
    /// Follow the focused task's transcript over SSE (replaces any previous one).
    FollowTaskLog(TaskId),
    /// Stop following whatever transcript is being followed.
    UnfollowTaskLog,
    /// Refetch the open-question inbox.
    FetchQuestions,
    /// Refetch the routing leaderboard, optionally for one kind.
    FetchRoutes(Option<String>),
    /// Refetch the lessons page.
    FetchLessons,
    /// Refetch the proposals inbox.
    FetchProposals,
    /// Refetch the worker doctor table.
    FetchWorkers,
    /// Refetch the cost report of a run (or of everything).
    FetchCost(Option<RunId>),
    /// Refetch the drain status shown in the runs footer.
    FetchDrain,
    /// Search memory (the `/` key on the lessons screen).
    SearchMemory(String),

    /// `POST /api/v1/runs`.
    CreateRun(String),
    /// `POST …/cancel` on a run.
    CancelRun(RunId, Option<String>),
    /// `POST …/plan/approve`.
    ApprovePlan(RunId),
    /// `POST …/plan/reject`.
    RejectPlan(RunId, String),
    /// `POST /api/v1/tasks/{id}/retry`.
    RetryTask(TaskId, bool),
    /// `POST /api/v1/tasks/{id}/cancel`.
    CancelTask(TaskId),
    /// `POST /api/v1/questions/{id}/answer`.
    AnswerQuestion(QuestionId, AnswerRequest),
    /// `POST /api/v1/proposals/{id}/accept`.
    AcceptProposal(ProposalId),
    /// `POST /api/v1/proposals/{id}/reject`.
    RejectProposal(ProposalId),
    /// `DELETE /api/v1/memory/{id}`.
    ForgetLesson(MemoryItemId),

    /// (Re)subscribe to the event firehose from this position.
    Subscribe(Option<u64>),
    /// Copy text to the system clipboard (OSC 52).
    Yank(String),
    /// Tear the terminal down and return.
    Quit,
}
