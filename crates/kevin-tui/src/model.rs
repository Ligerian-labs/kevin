//! The pure state of the TUI (`plan/07-api-and-tui.md` §4).
//!
//! [`Model`] holds everything the screens render and nothing else: no client,
//! no terminal, no clock. `now` is fed in by `Msg::Tick`, so a test can pin it
//! and the `insta` snapshots stay stable.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, TimeZone as _, Utc};
use kevin_api::dto::{
    CostReportDto, DrainStatusDto, MemoryItemDto, ProposalDto, QuestionDto, RouteScoreDto, RunDto,
    RunSummaryDto, TaskDto, TaskLogLineDto, WorkerDoctorDto,
};
use kevin_domain::ids::TaskId;
use uuid::Uuid;

use crate::ring::Ring;

/// Transcript lines kept for the focused task (`plan/07` §4 "bounded buffers").
pub const LOG_CAPACITY: usize = 5_000;
/// Events kept in a run's phase timeline.
pub const TIMELINE_CAPACITY: usize = 500;
/// Client-side log lines (errors, reconnects) kept for the `L` pane.
pub const CLIENT_LOG_CAPACITY: usize = 200;
/// Task statuses of the run-detail board, in the order `plan/07` §4 lists them.
pub const BOARD_STATUSES: [&str; 7] = [
    "pending",
    "routed",
    "running",
    "awaiting_input",
    "succeeded",
    "failed",
    "skipped",
];

/// The seven top-level screens; `1..6` switch between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Screen {
    /// Home: the runs table.
    #[default]
    Runs,
    /// Timeline + task board + transcript of one run.
    RunDetail,
    /// Open questions across every run.
    Questions,
    /// Routing leaderboard.
    Routes,
    /// Lessons and evaluator proposals.
    Lessons,
    /// `workers doctor`.
    Workers,
}

impl Screen {
    /// The screen `1..6` selects.
    #[must_use]
    pub const fn from_digit(d: char) -> Option<Self> {
        match d {
            '1' => Some(Self::Runs),
            '2' => Some(Self::RunDetail),
            '3' => Some(Self::Questions),
            '4' => Some(Self::Routes),
            '5' => Some(Self::Lessons),
            '6' => Some(Self::Workers),
            _ => None,
        }
    }

    /// Tab label.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Runs => "1 Runs",
            Self::RunDetail => "2 Run",
            Self::Questions => "3 Inbox",
            Self::Routes => "4 Routes",
            Self::Lessons => "5 Lessons",
            Self::Workers => "6 Workers",
        }
    }

    /// Every screen, in tab order.
    #[must_use]
    pub const fn all() -> [Self; 6] {
        [
            Self::Runs,
            Self::RunDetail,
            Self::Questions,
            Self::Routes,
            Self::Lessons,
            Self::Workers,
        ]
    }
}

/// A single-line text field (goal prompt, feedback, free-text answer, filter).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextInput {
    /// What has been typed.
    pub value: String,
    /// Prompt shown in front of the field.
    pub label: &'static str,
}

impl TextInput {
    /// An empty field labelled `label`.
    #[must_use]
    pub fn new(label: &'static str) -> Self {
        Self {
            value: String::new(),
            label,
        }
    }

    /// A field pre-filled with `value`.
    #[must_use]
    pub fn with_value(label: &'static str, value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label,
        }
    }

    /// Appends a character.
    pub fn push(&mut self, c: char) {
        self.value.push(c);
    }

    /// Removes the last character.
    pub fn backspace(&mut self) {
        self.value.pop();
    }

    /// Whether anything was typed (ignoring surrounding blanks).
    #[must_use]
    pub fn is_blank(&self) -> bool {
        self.value.trim().is_empty()
    }

    /// The trimmed content.
    #[must_use]
    pub fn trimmed(&self) -> &str {
        self.value.trim()
    }
}

/// A modal on top of the current screen. Every modal shows its keybindings in
/// its footer (`plan/07` §Rendering rules).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    /// The global keybinding table.
    Help,
    /// `n` on the runs screen: type a goal.
    NewRun(TextInput),
    /// `/` on the runs screen: filter by status.
    Filter(TextInput),
    /// `x` on a plan: why it is rejected.
    RejectFeedback(TextInput),
    /// `t` in the inbox: an answer the options do not cover.
    FreeText(TextInput),
    /// `/` on the lessons screen: memory search.
    MemorySearch(TextInput),
    /// The plan-approval modal (task DAG as a tree).
    PlanApproval,
}

/// Which pane of the run-detail screen has the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Pane {
    /// Left: the phase timeline.
    Timeline,
    /// Centre: the task board.
    #[default]
    Board,
    /// Right: the transcript of the focused task.
    Transcript,
}

impl Pane {
    /// `Tab` order.
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Timeline => Self::Board,
            Self::Board => Self::Transcript,
            Self::Transcript => Self::Timeline,
        }
    }
}

/// Severity of a client-side log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Something happened (a command was accepted, a resync was requested).
    Info,
    /// A call or a stream failed.
    Error,
}

/// One line of the `L` pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    /// When it was recorded.
    pub at: DateTime<Utc>,
    /// How bad it is.
    pub level: Level,
    /// The message.
    pub text: String,
}

/// One row of a run's phase timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseEntry {
    /// When the event happened.
    pub at: DateTime<Utc>,
    /// Its stable event type (`run.started`, `run.plan_proposed`, …).
    pub event_type: String,
}

/// The runs screen.
#[derive(Debug, Clone, Default)]
pub struct RunsState {
    /// The page currently shown.
    pub items: Vec<RunSummaryDto>,
    /// Cursor into [`RunsState::items`].
    pub selected: usize,
    /// `?status=` filter, when one is set.
    pub status_filter: Option<String>,
}

impl RunsState {
    /// The highlighted run.
    #[must_use]
    pub fn selected(&self) -> Option<&RunSummaryDto> {
        self.items.get(self.selected)
    }
}

/// The run-detail screen.
#[derive(Debug, Clone)]
pub struct DetailState {
    /// The run being shown.
    pub run: Option<RunDto>,
    /// Its task board.
    pub tasks: Vec<TaskDto>,
    /// Focused pane.
    pub pane: Pane,
    /// Cursor into the flattened board (see [`DetailState::board_order`]).
    pub board_selected: usize,
    /// Task whose transcript the right pane shows.
    pub focused_task: Option<TaskId>,
    /// Bounded transcript of the focused task.
    pub log: Ring<TaskLogLineDto>,
    /// Highest `seq` seen for the focused task (the log `Last-Event-ID`).
    pub log_seq: Option<u64>,
    /// Whether the transcript sticks to the bottom.
    pub follow: bool,
    /// How many lines the transcript is scrolled up by (0 = bottom).
    pub log_scroll: usize,
    /// Bounded phase timeline.
    pub timeline: Ring<PhaseEntry>,
    /// Cursor into the timeline.
    pub timeline_selected: usize,
    /// The run's cost report, for the footer.
    pub cost: Option<CostReportDto>,
}

impl Default for DetailState {
    fn default() -> Self {
        Self {
            run: None,
            tasks: Vec::new(),
            pane: Pane::default(),
            board_selected: 0,
            focused_task: None,
            log: Ring::new(LOG_CAPACITY),
            log_seq: None,
            follow: true,
            log_scroll: 0,
            timeline: Ring::new(TIMELINE_CAPACITY),
            timeline_selected: 0,
            cost: None,
        }
    }
}

impl DetailState {
    /// Forgets everything about the previous run.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Tasks grouped by status, in [`BOARD_STATUSES`] order; unknown statuses
    /// come last so a new status never disappears from the board.
    #[must_use]
    pub fn groups(&self) -> Vec<(String, Vec<&TaskDto>)> {
        let mut by_status: BTreeMap<&str, Vec<&TaskDto>> = BTreeMap::new();
        for task in &self.tasks {
            by_status
                .entry(task.status.as_str())
                .or_default()
                .push(task);
        }
        let mut groups = Vec::new();
        for status in BOARD_STATUSES {
            if let Some(tasks) = by_status.remove(status) {
                groups.push((status.to_owned(), tasks));
            }
        }
        for (status, tasks) in by_status {
            groups.push((status.to_owned(), tasks));
        }
        groups
    }

    /// The board flattened into the order the cursor walks.
    #[must_use]
    pub fn board_order(&self) -> Vec<TaskId> {
        self.groups()
            .into_iter()
            .flat_map(|(_, tasks)| tasks.into_iter().map(|task| task.id))
            .collect()
    }

    /// The task under the cursor.
    #[must_use]
    pub fn selected_task(&self) -> Option<&TaskDto> {
        let id = *self.board_order().get(self.board_selected)?;
        self.tasks.iter().find(|task| task.id == id)
    }

    /// The task whose transcript is shown.
    #[must_use]
    pub fn focused(&self) -> Option<&TaskDto> {
        let id = self.focused_task?;
        self.tasks.iter().find(|task| task.id == id)
    }
}

/// Which list of the inbox modal `j`/`k` walks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InboxFocus {
    /// The list of open questions.
    #[default]
    Questions,
    /// The options of the selected question.
    Options,
}

impl InboxFocus {
    /// `Tab` toggles the two.
    #[must_use]
    pub const fn other(self) -> Self {
        match self {
            Self::Questions => Self::Options,
            Self::Options => Self::Questions,
        }
    }
}

/// The question inbox.
#[derive(Debug, Clone, Default)]
pub struct InboxState {
    /// Open questions across every run.
    pub items: Vec<QuestionDto>,
    /// Cursor into [`InboxState::items`].
    pub selected: usize,
    /// Which list has the keyboard.
    pub focus: InboxFocus,
    /// Cursor into the selected question's options.
    pub option_selected: usize,
    /// Options ticked so far (labels).
    pub chosen: BTreeSet<String>,
    /// Free text typed with `t`.
    pub free_text: Option<String>,
}

impl InboxState {
    /// The highlighted question.
    #[must_use]
    pub fn selected(&self) -> Option<&QuestionDto> {
        self.items.get(self.selected)
    }

    /// Clears the answer being composed (after a submit or a move).
    pub fn clear_answer(&mut self) {
        self.chosen.clear();
        self.free_text = None;
        self.option_selected = 0;
        self.focus = InboxFocus::default();
    }
}

/// How the routes leaderboard is sorted (`s`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RouteSort {
    /// By sampled score, then success rate.
    #[default]
    Score,
    /// By success rate.
    Success,
    /// By mean spend, cheapest first.
    Cost,
    /// By mean latency, fastest first.
    Latency,
}

impl RouteSort {
    /// `s` cycles through the orders.
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Score => Self::Success,
            Self::Success => Self::Cost,
            Self::Cost => Self::Latency,
            Self::Latency => Self::Score,
        }
    }

    /// Column name shown in the footer.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Score => "score",
            Self::Success => "success",
            Self::Cost => "cost",
            Self::Latency => "latency",
        }
    }
}

/// The routes screen.
#[derive(Debug, Clone, Default)]
pub struct RoutesState {
    /// Leaderboard rows as the server returned them.
    pub items: Vec<RouteScoreDto>,
    /// Cursor.
    pub selected: usize,
    /// `?kind=` filter cycled with `k`.
    pub kind_filter: Option<String>,
    /// Sort order cycled with `s`.
    pub sort: RouteSort,
}

impl RoutesState {
    /// Rows in display order.
    #[must_use]
    pub fn sorted(&self) -> Vec<&RouteScoreDto> {
        let mut rows: Vec<&RouteScoreDto> = self.items.iter().collect();
        match self.sort {
            RouteSort::Score => rows.sort_by(|a, b| {
                score_key(b)
                    .total_cmp(&score_key(a))
                    .then_with(|| a.alias.cmp(&b.alias))
            }),
            RouteSort::Success => rows.sort_by(|a, b| {
                success_rate(b)
                    .total_cmp(&success_rate(a))
                    .then_with(|| a.alias.cmp(&b.alias))
            }),
            RouteSort::Cost => rows.sort_by(|a, b| {
                a.mean_cost_usd
                    .unwrap_or(rust_decimal::Decimal::MAX)
                    .cmp(&b.mean_cost_usd.unwrap_or(rust_decimal::Decimal::MAX))
                    .then_with(|| a.alias.cmp(&b.alias))
            }),
            RouteSort::Latency => rows.sort_by(|a, b| {
                a.mean_wall_ms
                    .unwrap_or(u64::MAX)
                    .cmp(&b.mean_wall_ms.unwrap_or(u64::MAX))
                    .then_with(|| a.alias.cmp(&b.alias))
            }),
        }
        rows
    }

    /// Every task kind present in the leaderboard, sorted.
    #[must_use]
    pub fn kinds(&self) -> Vec<String> {
        let mut kinds: Vec<String> = self.items.iter().map(|row| row.kind.clone()).collect();
        kinds.sort();
        kinds.dedup();
        kinds
    }
}

fn score_key(row: &RouteScoreDto) -> f32 {
    row.sampled_score
        .or(row.mean_quality)
        .unwrap_or_else(|| success_rate(row))
}

fn success_rate(row: &RouteScoreDto) -> f32 {
    if row.attempts == 0 {
        return 0.0;
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "attempt counts are small; f32 is only used for ordering and display"
    )]
    {
        row.successes as f32 / row.attempts as f32
    }
}

/// Which tab of the lessons screen is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LessonsTab {
    /// Memory items of kind `lesson`.
    #[default]
    Lessons,
    /// Evaluator proposals.
    Proposals,
}

impl LessonsTab {
    /// `Tab` toggles the two.
    #[must_use]
    pub const fn other(self) -> Self {
        match self {
            Self::Lessons => Self::Proposals,
            Self::Proposals => Self::Lessons,
        }
    }
}

/// The lessons & proposals screen.
#[derive(Debug, Clone, Default)]
pub struct LessonsState {
    /// Which tab.
    pub tab: LessonsTab,
    /// Lessons (or the last memory-search hits).
    pub lessons: Vec<MemoryItemDto>,
    /// Proposals.
    pub proposals: Vec<ProposalDto>,
    /// Cursor into the lessons list.
    pub lesson_selected: usize,
    /// Cursor into the proposals list.
    pub proposal_selected: usize,
    /// The query `/` last ran, when the list shows search hits.
    pub search: Option<String>,
}

/// The workers screen.
#[derive(Debug, Clone, Default)]
pub struct WorkersState {
    /// Doctor rows.
    pub items: Vec<WorkerDoctorDto>,
    /// Cursor.
    pub selected: usize,
}

/// The whole TUI state.
#[derive(Debug, Clone)]
pub struct Model {
    /// The daemon this session talks to, shown in the footer.
    pub server: String,
    /// The clock the view renders ages against.
    pub now: DateTime<Utc>,
    /// Terminal size; below 80×24 the panes collapse to one column.
    pub size: (u16, u16),
    /// Colours, or the `NO_COLOR` fallback.
    pub theme: crate::theme::Theme,
    /// Current screen.
    pub screen: Screen,
    /// Modal on top, when any.
    pub overlay: Option<Overlay>,
    /// Runs screen.
    pub runs: RunsState,
    /// Run-detail screen.
    pub detail: DetailState,
    /// Question inbox.
    pub inbox: InboxState,
    /// Routes screen.
    pub routes: RoutesState,
    /// Lessons & proposals screen.
    pub lessons: LessonsState,
    /// Workers screen.
    pub workers: WorkersState,
    /// Drain state of the daemon, shown in the footer.
    pub drain: Option<DrainStatusDto>,
    /// Client errors and reconnects (`L`).
    pub client_log: Ring<LogLine>,
    /// Whether the `L` pane is open.
    pub show_client_log: bool,
    /// Transient one-line status.
    pub status: Option<String>,
    /// Highest event position the session has seen.
    pub stream_position: Option<u64>,
    /// How many resyncs this session performed.
    pub resync_count: u64,
    /// Last `aggregate_version` per aggregate, for gap detection.
    pub aggregate_versions: BTreeMap<Uuid, u64>,
    /// Set once the reducer accepted a quit.
    pub quit: bool,
}

impl Default for Model {
    fn default() -> Self {
        Self::new("")
    }
}

impl Model {
    /// A fresh model for a session against `server`.
    #[must_use]
    pub fn new(server: impl Into<String>) -> Self {
        Self {
            server: server.into(),
            now: Utc
                .timestamp_opt(0, 0)
                .single()
                .unwrap_or_else(|| DateTime::<Utc>::from_timestamp_nanos(0)),
            size: (80, 24),
            theme: crate::theme::Theme::COLOR,
            screen: Screen::Runs,
            overlay: None,
            runs: RunsState::default(),
            detail: DetailState::default(),
            inbox: InboxState::default(),
            routes: RoutesState::default(),
            lessons: LessonsState::default(),
            workers: WorkersState::default(),
            drain: None,
            client_log: Ring::new(CLIENT_LOG_CAPACITY),
            show_client_log: false,
            status: None,
            stream_position: None,
            resync_count: 0,
            aggregate_versions: BTreeMap::new(),
            quit: false,
        }
    }

    /// Whether the terminal is smaller than the 80×24 minimum, in which case
    /// the run-detail panes collapse into a single column.
    #[must_use]
    pub const fn is_narrow(&self) -> bool {
        self.size.0 < 80 || self.size.1 < 24
    }

    /// The run currently open in the detail screen.
    #[must_use]
    pub fn current_run(&self) -> Option<&RunDto> {
        self.detail.run.as_ref()
    }

    /// Records a line in the `L` pane and, for errors, in the status bar.
    pub fn log(&mut self, level: Level, text: impl Into<String>) {
        let text = text.into();
        if level == Level::Error {
            self.status = Some(text.clone());
        }
        self.client_log.push(LogLine {
            at: self.now,
            level,
            text,
        });
    }
}
