//! The pure reducer (`plan/07-api-and-tui.md` §4).
//!
//! `update(&mut Model, Msg) -> Vec<Cmd>` is the only thing that changes state.
//! It never awaits, never touches the network and never looks at a terminal, so
//! every keybinding and every resync path is a plain unit test.

use kevin_api::dto::{AnswerRequest, EventDto, QuestionDto};
use kevin_domain::ids::RunId;

use crate::keys::{Key, KeyPress};
use crate::model::{
    InboxFocus, LessonsTab, Level, Model, Overlay, Pane, PhaseEntry, Screen, TextInput,
};
use crate::msg::{Cmd, Msg};

/// What a fresh session asks the server for.
#[must_use]
pub fn init(run: Option<RunId>) -> Vec<Cmd> {
    let mut cmds = vec![
        Cmd::FetchRuns,
        Cmd::FetchQuestions,
        Cmd::FetchDrain,
        Cmd::Subscribe(None),
    ];
    if let Some(run) = run {
        cmds.push(Cmd::FetchRun(run));
        cmds.push(Cmd::FetchTasks(run));
        cmds.push(Cmd::FetchCost(Some(run)));
    }
    cmds
}

/// Folds one message into the model and returns the effects to run.
#[must_use]
pub fn update(model: &mut Model, msg: Msg) -> Vec<Cmd> {
    match msg {
        Msg::Key(key) => on_key(model, key),
        Msg::Tick(now) => {
            model.now = now;
            poll(model)
        }
        Msg::Resized(cols, rows) => {
            model.size = (cols, rows);
            Vec::new()
        }

        Msg::RunsLoaded(items) => {
            model.runs.items = items;
            model.runs.selected = model
                .runs
                .selected
                .min(model.runs.items.len().saturating_sub(1));
            Vec::new()
        }
        Msg::RunLoaded(run) => on_run_loaded(model, *run),
        Msg::TasksLoaded(run_id, tasks) => {
            if model.detail.run.as_ref().is_none_or(|run| run.id == run_id) {
                model.detail.tasks = tasks;
                let len = model.detail.board_order().len();
                model.detail.board_selected =
                    model.detail.board_selected.min(len.saturating_sub(1));
            }
            Vec::new()
        }
        Msg::LogLines(task_id, lines) => {
            if model.detail.focused_task == Some(task_id) {
                for line in lines {
                    model.detail.log_seq = Some(
                        model
                            .detail
                            .log_seq
                            .map_or(line.seq, |seq| seq.max(line.seq)),
                    );
                    model.detail.log.push(line);
                }
                if model.detail.follow {
                    model.detail.log_scroll = 0;
                }
            }
            Vec::new()
        }
        Msg::QuestionsLoaded(items) => {
            let same = model
                .inbox
                .selected()
                .map(|q| q.id)
                .and_then(|id| items.iter().position(|q| q.id == id));
            model.inbox.items = items;
            model.inbox.selected = same.unwrap_or(0);
            if same.is_none() {
                model.inbox.clear_answer();
            }
            Vec::new()
        }
        Msg::QuestionAnswered(question) => on_question_answered(model, &question),
        Msg::RoutesLoaded(items) => {
            model.routes.items = items;
            model.routes.selected = model
                .routes
                .selected
                .min(model.routes.items.len().saturating_sub(1));
            Vec::new()
        }
        Msg::LessonsLoaded(items) => {
            model.lessons.lessons = items;
            model.lessons.lesson_selected = model
                .lessons
                .lesson_selected
                .min(model.lessons.lessons.len().saturating_sub(1));
            Vec::new()
        }
        Msg::ProposalsLoaded(items) => {
            model.lessons.proposals = items;
            model.lessons.proposal_selected = model
                .lessons
                .proposal_selected
                .min(model.lessons.proposals.len().saturating_sub(1));
            Vec::new()
        }
        Msg::WorkersLoaded(items) => {
            model.workers.items = items;
            model.workers.selected = model
                .workers
                .selected
                .min(model.workers.items.len().saturating_sub(1));
            Vec::new()
        }
        Msg::CostLoaded(report) => {
            model.detail.cost = Some(*report);
            Vec::new()
        }
        Msg::DrainLoaded(status) => {
            model.drain = Some(status);
            Vec::new()
        }

        Msg::ApiEvent(event) => on_event(model, &event),
        Msg::Resync => resync(model, "the server asked for a resync"),
        Msg::StreamError(text) => {
            model.log(Level::Error, format!("stream: {text}"));
            Vec::new()
        }
        Msg::ClientError(text) => {
            model.log(Level::Error, text);
            Vec::new()
        }
        Msg::Notice(text) => {
            model.log(Level::Info, text);
            Vec::new()
        }
        Msg::Quit => {
            model.quit = true;
            vec![Cmd::Quit]
        }
    }
}

// ---------------------------------------------------------------------------
// Server data
// ---------------------------------------------------------------------------

fn on_run_loaded(model: &mut Model, run: kevin_api::dto::RunDto) -> Vec<Cmd> {
    let switched = model
        .detail
        .run
        .as_ref()
        .is_none_or(|open| open.id != run.id);
    if switched {
        model.detail.reset();
    }
    let id = run.id;
    let awaiting = run.status == kevin_api::dto::RunStatusDto::AwaitingPlanApproval;
    let has_plan = run.plan.is_some();
    model.detail.run = Some(run);

    // `plan/07` §Screens: the approval view is a modal shown when the run is in
    // `awaiting_plan_approval`. It never steals the keyboard from another modal.
    if awaiting && has_plan && model.overlay.is_none() && model.screen == Screen::RunDetail {
        model.overlay = Some(Overlay::PlanApproval);
    }
    if switched {
        return vec![Cmd::FetchTasks(id), Cmd::FetchCost(Some(id))];
    }
    Vec::new()
}

fn on_question_answered(model: &mut Model, question: &QuestionDto) -> Vec<Cmd> {
    model.inbox.items.retain(|open| open.id != question.id);
    model.inbox.selected = model
        .inbox
        .selected
        .min(model.inbox.items.len().saturating_sub(1));
    model.inbox.clear_answer();
    model.log(Level::Info, format!("answered question {}", question.id));
    let mut cmds = vec![Cmd::FetchQuestions];
    if let Some(run) = model.detail.run.as_ref().map(|run| run.id) {
        cmds.push(Cmd::FetchRun(run));
    }
    cmds
}

fn on_event(model: &mut Model, event: &EventDto) -> Vec<Cmd> {
    // A hole in an aggregate's version sequence means the bus dropped events
    // (`plan/07` §4): refetch snapshots instead of rendering a torn state.
    let previous = model
        .aggregate_versions
        .insert(event.aggregate_id, event.aggregate_version);
    let gap = previous.is_some_and(|last| event.aggregate_version > last + 1);
    model.stream_position = Some(
        model
            .stream_position
            .map_or(event.position, |pos| pos.max(event.position)),
    );

    if gap {
        return resync(
            model,
            format!(
                "gap on {} {}: version {} after {}",
                event.aggregate_type,
                event.aggregate_id,
                event.aggregate_version,
                previous.unwrap_or_default()
            ),
        );
    }

    let mut cmds = Vec::new();
    let open_run = model.detail.run.as_ref().map(|run| run.id.as_uuid());
    let concerns_open_run = open_run == Some(event.correlation_id);

    if concerns_open_run {
        model.detail.timeline.push(PhaseEntry {
            at: event.occurred_at,
            event_type: event.event_type.clone(),
        });
        if let Some(run) = open_run.map(RunId::from_uuid) {
            if event.event_type.starts_with("run.") {
                cmds.push(Cmd::FetchRun(run));
            }
            if event.event_type.starts_with("task.") {
                cmds.push(Cmd::FetchTasks(run));
            }
            if event.event_type.starts_with("task.attempt_") {
                cmds.push(Cmd::FetchCost(Some(run)));
            }
        }
    }
    if event.event_type.starts_with("run.") {
        cmds.push(Cmd::FetchRuns);
    }
    if event.event_type.starts_with("question.") {
        cmds.push(Cmd::FetchQuestions);
    }
    cmds.dedup();
    cmds
}

/// Refetches every snapshot and reconnects the stream from the last position
/// the session saw (`plan/07` §Event streams).
fn resync(model: &mut Model, reason: impl Into<String>) -> Vec<Cmd> {
    model.resync_count += 1;
    model.aggregate_versions.clear();
    let reason = reason.into();
    model.log(
        Level::Info,
        format!("resync #{}: {reason}", model.resync_count),
    );
    let mut cmds = vec![Cmd::FetchRuns, Cmd::FetchQuestions];
    if let Some(run) = model.detail.run.as_ref().map(|run| run.id) {
        cmds.push(Cmd::FetchRun(run));
        cmds.push(Cmd::FetchTasks(run));
        cmds.push(Cmd::FetchCost(Some(run)));
    }
    if let Some(task) = model.detail.focused_task {
        cmds.push(Cmd::FetchTaskLog {
            task_id: task,
            after_seq: model.detail.log_seq,
        });
    }
    cmds.push(Cmd::Subscribe(model.stream_position));
    cmds
}

/// The periodic snapshot poll: only what the visible screen needs.
fn poll(model: &Model) -> Vec<Cmd> {
    let mut cmds = match model.screen {
        Screen::Runs => vec![Cmd::FetchRuns, Cmd::FetchDrain],
        Screen::RunDetail => {
            let mut cmds = Vec::new();
            if let Some(run) = model.detail.run.as_ref().map(|run| run.id) {
                cmds.push(Cmd::FetchRun(run));
                cmds.push(Cmd::FetchTasks(run));
                cmds.push(Cmd::FetchCost(Some(run)));
            }
            cmds
        }
        Screen::Questions => vec![Cmd::FetchQuestions],
        Screen::Routes => vec![Cmd::FetchRoutes(model.routes.kind_filter.clone())],
        Screen::Lessons => vec![Cmd::FetchLessons, Cmd::FetchProposals],
        Screen::Workers => vec![Cmd::FetchWorkers],
    };
    // Follow mode pulls the tail of the transcript even when the SSE log stream
    // is between reconnects.
    if model.detail.follow
        && let Some(task) = model.detail.focused_task
    {
        cmds.push(Cmd::FetchTaskLog {
            task_id: task,
            after_seq: model.detail.log_seq,
        });
    }
    cmds
}

/// What a screen loads when it is opened.
fn enter(model: &Model, screen: Screen) -> Vec<Cmd> {
    match screen {
        Screen::Runs => vec![Cmd::FetchRuns, Cmd::FetchDrain],
        Screen::RunDetail => model
            .detail
            .run
            .as_ref()
            .map(|run| {
                vec![
                    Cmd::FetchRun(run.id),
                    Cmd::FetchTasks(run.id),
                    Cmd::FetchCost(Some(run.id)),
                ]
            })
            .unwrap_or_default(),
        Screen::Questions => vec![Cmd::FetchQuestions],
        Screen::Routes => vec![Cmd::FetchRoutes(model.routes.kind_filter.clone())],
        Screen::Lessons => vec![Cmd::FetchLessons, Cmd::FetchProposals],
        Screen::Workers => vec![Cmd::FetchWorkers],
    }
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

fn on_key(model: &mut Model, key: KeyPress) -> Vec<Cmd> {
    model.status = None;
    if model.overlay.is_some() {
        return on_overlay_key(model, key);
    }
    if let Some(cmds) = on_global_key(model, key) {
        return cmds;
    }
    match model.screen {
        Screen::Runs => on_runs_key(model, key),
        Screen::RunDetail => on_detail_key(model, key),
        Screen::Questions => on_inbox_key(model, key),
        Screen::Routes => on_routes_key(model, key),
        Screen::Lessons => on_lessons_key(model, key),
        Screen::Workers => on_workers_key(model, key),
    }
}

/// Returns `Some` when the key was a global one.
fn on_global_key(model: &mut Model, key: KeyPress) -> Option<Vec<Cmd>> {
    if key.ctrl && key.key == Key::Char('c') {
        model.quit = true;
        return Some(vec![Cmd::Quit]);
    }
    let c = key.plain_char()?;
    match c {
        'Q' => {
            model.quit = true;
            Some(vec![Cmd::Quit])
        }
        '1'..='6' => {
            let screen = Screen::from_digit(c)?;
            model.screen = screen;
            Some(enter(model, screen))
        }
        '?' => {
            model.screen = Screen::Questions;
            Some(vec![Cmd::FetchQuestions])
        }
        'h' => {
            model.overlay = Some(Overlay::Help);
            Some(Vec::new())
        }
        'L' => {
            model.show_client_log = !model.show_client_log;
            Some(Vec::new())
        }
        'g' => {
            set_cursor(model, 0);
            Some(Vec::new())
        }
        'G' => {
            set_cursor(model, usize::MAX);
            Some(Vec::new())
        }
        _ => None,
    }
}

fn set_cursor(model: &mut Model, index: usize) {
    match model.screen {
        Screen::Runs => model.runs.selected = clamp(index, model.runs.items.len()),
        Screen::RunDetail => {
            model.detail.board_selected = clamp(index, model.detail.board_order().len());
        }
        Screen::Questions => {
            model.inbox.selected = clamp(index, model.inbox.items.len());
            model.inbox.clear_answer();
        }
        Screen::Routes => model.routes.selected = clamp(index, model.routes.items.len()),
        Screen::Lessons => match model.lessons.tab {
            LessonsTab::Lessons => {
                model.lessons.lesson_selected = clamp(index, model.lessons.lessons.len());
            }
            LessonsTab::Proposals => {
                model.lessons.proposal_selected = clamp(index, model.lessons.proposals.len());
            }
        },
        Screen::Workers => model.workers.selected = clamp(index, model.workers.items.len()),
    }
}

const fn clamp(index: usize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    if index >= len { len - 1 } else { index }
}

fn step(cursor: &mut usize, len: usize, delta: isize) {
    if len == 0 {
        *cursor = 0;
        return;
    }
    let next = if delta.is_negative() {
        cursor.saturating_sub(delta.unsigned_abs())
    } else {
        cursor.saturating_add(delta.unsigned_abs())
    };
    *cursor = clamp(next, len);
}

/// `j`/`k` and the arrow keys, as a delta.
const fn vertical(key: KeyPress) -> Option<isize> {
    match key.key {
        Key::Down => Some(1),
        Key::Up => Some(-1),
        Key::PageDown => Some(10),
        Key::PageUp => Some(-10),
        Key::Char('j') if !key.ctrl && !key.alt => Some(1),
        Key::Char('k') if !key.ctrl && !key.alt => Some(-1),
        _ => None,
    }
}

// -- runs -------------------------------------------------------------------

fn on_runs_key(model: &mut Model, key: KeyPress) -> Vec<Cmd> {
    if let Some(delta) = vertical(key) {
        step(&mut model.runs.selected, model.runs.items.len(), delta);
        return Vec::new();
    }
    match key.key {
        Key::Enter => {
            let Some(run) = model.runs.selected().map(|run| run.id) else {
                return Vec::new();
            };
            model.screen = Screen::RunDetail;
            if model.detail.run.as_ref().is_none_or(|open| open.id != run) {
                model.detail.reset();
            }
            vec![
                Cmd::FetchRun(run),
                Cmd::FetchTasks(run),
                Cmd::FetchCost(Some(run)),
            ]
        }
        Key::Char('n') if !key.ctrl => {
            model.overlay = Some(Overlay::NewRun(TextInput::new("goal")));
            Vec::new()
        }
        Key::Char('c') if !key.ctrl => model
            .runs
            .selected()
            .map(|run| vec![Cmd::CancelRun(run.id, None)])
            .unwrap_or_default(),
        Key::Char('/') => {
            let current = model.runs.status_filter.clone().unwrap_or_default();
            model.overlay = Some(Overlay::Filter(TextInput::with_value("status", current)));
            Vec::new()
        }
        Key::Char('r') => vec![Cmd::FetchRuns, Cmd::FetchDrain],
        Key::Char('y') => model
            .runs
            .selected()
            .map(|run| vec![Cmd::Yank(run.id.to_string())])
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

// -- run detail -------------------------------------------------------------

fn on_detail_key(model: &mut Model, key: KeyPress) -> Vec<Cmd> {
    if let Some(delta) = vertical(key) {
        return move_in_detail(model, delta);
    }
    match key.key {
        Key::Tab => {
            model.detail.pane = model.detail.pane.next();
            Vec::new()
        }
        Key::Esc => {
            model.screen = Screen::Runs;
            vec![Cmd::FetchRuns]
        }
        Key::Enter => focus_selected_task(model),
        Key::Char('f') => {
            model.detail.follow = !model.detail.follow;
            if model.detail.follow {
                model.detail.log_scroll = 0;
                if let Some(task) = model.detail.focused_task {
                    return vec![Cmd::FollowTaskLog(task)];
                }
            }
            vec![Cmd::UnfollowTaskLog]
        }
        Key::Char('a') => model
            .current_run()
            .map(|run| vec![Cmd::ApprovePlan(run.id)])
            .unwrap_or_default(),
        Key::Char('x') => {
            if model.current_run().is_some() {
                model.overlay = Some(Overlay::RejectFeedback(TextInput::new("feedback")));
            }
            Vec::new()
        }
        Key::Char('R') => model
            .detail
            .selected_task()
            .map(|task| vec![Cmd::RetryTask(task.id, true)])
            .unwrap_or_default(),
        Key::Char('C') => match model.detail.pane {
            Pane::Board | Pane::Transcript => model
                .detail
                .selected_task()
                .map(|task| vec![Cmd::CancelTask(task.id)])
                .unwrap_or_default(),
            Pane::Timeline => model
                .current_run()
                .map(|run| vec![Cmd::CancelRun(run.id, None)])
                .unwrap_or_default(),
        },
        Key::Char('q') => {
            model.screen = Screen::Questions;
            vec![Cmd::FetchQuestions]
        }
        Key::Char('o') => {
            let path = model
                .detail
                .focused()
                .and_then(|task| task.artifacts.first().map(|a| a.uri.clone()))
                .or_else(|| {
                    model
                        .detail
                        .focused()
                        .and_then(|task| task.attempts.last())
                        .and_then(|attempt| attempt.workspace.as_ref())
                        .map(|ws| ws.root.display().to_string())
                });
            if let Some(path) = path {
                model.log(Level::Info, format!("artifact: {path}"));
                vec![Cmd::Yank(path)]
            } else {
                model.log(Level::Info, "no artifact on the focused task");
                Vec::new()
            }
        }
        Key::Char('y') => {
            let id = model
                .detail
                .selected_task()
                .map(|task| task.id.to_string())
                .or_else(|| model.current_run().map(|run| run.id.to_string()));
            id.map(|id| vec![Cmd::Yank(id)]).unwrap_or_default()
        }
        Key::Char('p') => {
            if model
                .current_run()
                .and_then(|run| run.plan.as_ref())
                .is_some()
            {
                model.overlay = Some(Overlay::PlanApproval);
            }
            Vec::new()
        }
        _ => Vec::new(),
    }
}

fn move_in_detail(model: &mut Model, delta: isize) -> Vec<Cmd> {
    match model.detail.pane {
        Pane::Timeline => {
            step(
                &mut model.detail.timeline_selected,
                model.detail.timeline.len(),
                delta,
            );
            Vec::new()
        }
        Pane::Board => {
            let len = model.detail.board_order().len();
            step(&mut model.detail.board_selected, len, delta);
            Vec::new()
        }
        Pane::Transcript => {
            // Scrolling up leaves follow mode and pages older lines in.
            if delta.is_negative() {
                model.detail.follow = false;
                model.detail.log_scroll = model
                    .detail
                    .log_scroll
                    .saturating_add(delta.unsigned_abs())
                    .min(model.detail.log.len().saturating_sub(1));
            } else {
                model.detail.log_scroll =
                    model.detail.log_scroll.saturating_sub(delta.unsigned_abs());
            }
            Vec::new()
        }
    }
}

fn focus_selected_task(model: &mut Model) -> Vec<Cmd> {
    let Some(task) = model.detail.selected_task().map(|task| task.id) else {
        return Vec::new();
    };
    if model.detail.focused_task == Some(task) {
        model.detail.pane = Pane::Transcript;
        return Vec::new();
    }
    model.detail.focused_task = Some(task);
    model.detail.log = crate::ring::Ring::new(crate::model::LOG_CAPACITY);
    model.detail.log_seq = None;
    model.detail.log_scroll = 0;
    model.detail.pane = Pane::Transcript;
    let mut cmds = vec![Cmd::FetchTaskLog {
        task_id: task,
        after_seq: None,
    }];
    if model.detail.follow {
        cmds.push(Cmd::FollowTaskLog(task));
    }
    cmds
}

// -- question inbox ---------------------------------------------------------

fn on_inbox_key(model: &mut Model, key: KeyPress) -> Vec<Cmd> {
    let options = model
        .inbox
        .selected()
        .map(|q| q.options.len())
        .unwrap_or_default();
    if let Some(delta) = vertical(key) {
        match model.inbox.focus {
            InboxFocus::Questions => {
                let before = model.inbox.selected;
                step(&mut model.inbox.selected, model.inbox.items.len(), delta);
                if model.inbox.selected != before {
                    model.inbox.clear_answer();
                }
            }
            InboxFocus::Options => step(&mut model.inbox.option_selected, options, delta),
        }
        return Vec::new();
    }
    match key.key {
        Key::Tab | Key::BackTab => {
            model.inbox.focus = model.inbox.focus.other();
            Vec::new()
        }
        Key::Esc => {
            model.screen = Screen::Runs;
            vec![Cmd::FetchRuns]
        }
        Key::Char(' ') => {
            model.inbox.focus = InboxFocus::Options;
            toggle_option(model);
            Vec::new()
        }
        Key::Char('t') => {
            let current = model.inbox.free_text.clone().unwrap_or_default();
            model.overlay = Some(Overlay::FreeText(TextInput::with_value("answer", current)));
            Vec::new()
        }
        Key::Char('r') => vec![Cmd::FetchQuestions],
        Key::Char('y') => model
            .inbox
            .selected()
            .map(|q| vec![Cmd::Yank(q.id.to_string())])
            .unwrap_or_default(),
        Key::Enter => {
            // Single-select: `Enter` on an option *is* the answer.
            if model.inbox.focus == InboxFocus::Options
                && model.inbox.selected().is_some_and(|q| !q.multi_select)
            {
                model.inbox.chosen.clear();
                toggle_option(model);
            }
            submit_answer(model)
        }
        _ => Vec::new(),
    }
}

fn toggle_option(model: &mut Model) {
    let Some(question) = model.inbox.selected() else {
        return;
    };
    let multi = question.multi_select;
    let Some(label) = question
        .options
        .get(model.inbox.option_selected)
        .map(|option| option.label.clone())
    else {
        return;
    };
    if !multi {
        model.inbox.chosen.clear();
        model.inbox.chosen.insert(label);
        return;
    }
    if !model.inbox.chosen.remove(&label) {
        model.inbox.chosen.insert(label);
    }
}

/// Sends the composed answer. An empty answer falls back to the question's
/// default when it has one; otherwise it is refused rather than sent blank.
fn submit_answer(model: &mut Model) -> Vec<Cmd> {
    let Some(question) = model.inbox.selected().cloned() else {
        return Vec::new();
    };
    let mut selected: Vec<String> = model.inbox.chosen.iter().cloned().collect();
    let mut free_text = model
        .inbox
        .free_text
        .clone()
        .filter(|text| !text.trim().is_empty());
    if selected.is_empty() && free_text.is_none() {
        if let Some(default) = question.default.as_ref() {
            selected.clone_from(&default.selected);
            free_text.clone_from(&default.free_text);
        } else {
            model.log(
                Level::Error,
                "pick an option (Space/Enter) or type an answer (t) first",
            );
            return Vec::new();
        }
    }
    vec![Cmd::AnswerQuestion(
        question.id,
        AnswerRequest {
            selected,
            free_text,
        },
    )]
}

// -- routes / lessons / workers ---------------------------------------------

fn on_routes_key(model: &mut Model, key: KeyPress) -> Vec<Cmd> {
    // `plan/07` binds `k` to "change kind" on this screen, so the leaderboard is
    // walked with `j`/`Down`/`Up` and `k` never means "up" here.
    if !key.is_char('k')
        && let Some(delta) = vertical(key)
    {
        step(&mut model.routes.selected, model.routes.items.len(), delta);
        return Vec::new();
    }
    match key.key {
        Key::Char('k') if !key.ctrl => {
            let kinds = model.routes.kinds();
            let next = match model.routes.kind_filter.as_ref() {
                None => kinds.first().cloned(),
                Some(current) => kinds
                    .iter()
                    .position(|kind| kind == current)
                    .and_then(|index| kinds.get(index + 1))
                    .cloned(),
            };
            model.routes.kind_filter.clone_from(&next);
            model.routes.selected = 0;
            vec![Cmd::FetchRoutes(next)]
        }
        Key::Char('s') => {
            model.routes.sort = model.routes.sort.next();
            Vec::new()
        }
        Key::Char('r') => vec![Cmd::FetchRoutes(model.routes.kind_filter.clone())],
        _ => Vec::new(),
    }
}

fn on_lessons_key(model: &mut Model, key: KeyPress) -> Vec<Cmd> {
    if let Some(delta) = vertical(key) {
        match model.lessons.tab {
            LessonsTab::Lessons => step(
                &mut model.lessons.lesson_selected,
                model.lessons.lessons.len(),
                delta,
            ),
            LessonsTab::Proposals => step(
                &mut model.lessons.proposal_selected,
                model.lessons.proposals.len(),
                delta,
            ),
        }
        return Vec::new();
    }
    match key.key {
        Key::Tab | Key::BackTab => {
            model.lessons.tab = model.lessons.tab.other();
            Vec::new()
        }
        Key::Char('A') => selected_proposal(model)
            .map(|id| vec![Cmd::AcceptProposal(id)])
            .unwrap_or_default(),
        Key::Char('X') => selected_proposal(model)
            .map(|id| vec![Cmd::RejectProposal(id)])
            .unwrap_or_default(),
        Key::Char('d') => model
            .lessons
            .lessons
            .get(model.lessons.lesson_selected)
            .map(|item| vec![Cmd::ForgetLesson(item.id), Cmd::FetchLessons])
            .unwrap_or_default(),
        Key::Char('/') => {
            let current = model.lessons.search.clone().unwrap_or_default();
            model.overlay = Some(Overlay::MemorySearch(TextInput::with_value(
                "search", current,
            )));
            Vec::new()
        }
        Key::Char('r') => vec![Cmd::FetchLessons, Cmd::FetchProposals],
        _ => Vec::new(),
    }
}

fn selected_proposal(model: &Model) -> Option<kevin_domain::ids::ProposalId> {
    model
        .lessons
        .proposals
        .get(model.lessons.proposal_selected)
        .map(|proposal| proposal.id)
}

fn on_workers_key(model: &mut Model, key: KeyPress) -> Vec<Cmd> {
    if let Some(delta) = vertical(key) {
        step(
            &mut model.workers.selected,
            model.workers.items.len(),
            delta,
        );
        return Vec::new();
    }
    if key.is_char('r') {
        return vec![Cmd::FetchWorkers];
    }
    Vec::new()
}

// -- overlays ---------------------------------------------------------------

fn on_overlay_key(model: &mut Model, key: KeyPress) -> Vec<Cmd> {
    let Some(overlay) = model.overlay.clone() else {
        return Vec::new();
    };
    match overlay {
        Overlay::Help => {
            if matches!(key.key, Key::Esc | Key::Enter) || key.is_char('h') || key.is_char('q') {
                model.overlay = None;
            }
            Vec::new()
        }
        Overlay::PlanApproval => on_plan_key(model, key),
        Overlay::NewRun(input) => match text_key(key, input) {
            TextOutcome::Cancelled => {
                model.overlay = None;
                Vec::new()
            }
            TextOutcome::Editing(input) => {
                model.overlay = Some(Overlay::NewRun(input));
                Vec::new()
            }
            TextOutcome::Submitted(input) => {
                model.overlay = None;
                if input.is_blank() {
                    return Vec::new();
                }
                vec![Cmd::CreateRun(input.trimmed().to_owned())]
            }
        },
        Overlay::Filter(input) => match text_key(key, input) {
            TextOutcome::Cancelled => {
                model.overlay = None;
                Vec::new()
            }
            TextOutcome::Editing(input) => {
                model.overlay = Some(Overlay::Filter(input));
                Vec::new()
            }
            TextOutcome::Submitted(input) => {
                model.overlay = None;
                model.runs.status_filter = (!input.is_blank()).then(|| input.trimmed().to_owned());
                model.runs.selected = 0;
                vec![Cmd::FetchRuns]
            }
        },
        Overlay::RejectFeedback(input) => match text_key(key, input) {
            TextOutcome::Cancelled => {
                model.overlay = None;
                Vec::new()
            }
            TextOutcome::Editing(input) => {
                model.overlay = Some(Overlay::RejectFeedback(input));
                Vec::new()
            }
            TextOutcome::Submitted(input) => {
                model.overlay = None;
                if input.is_blank() {
                    model.log(Level::Error, "a rejection needs feedback");
                    return Vec::new();
                }
                model
                    .current_run()
                    .map(|run| vec![Cmd::RejectPlan(run.id, input.trimmed().to_owned())])
                    .unwrap_or_default()
            }
        },
        Overlay::FreeText(input) => match text_key(key, input) {
            TextOutcome::Cancelled => {
                model.overlay = None;
                Vec::new()
            }
            TextOutcome::Editing(input) => {
                model.overlay = Some(Overlay::FreeText(input));
                Vec::new()
            }
            TextOutcome::Submitted(input) => {
                model.overlay = None;
                model.inbox.free_text = (!input.is_blank()).then(|| input.trimmed().to_owned());
                Vec::new()
            }
        },
        Overlay::MemorySearch(input) => match text_key(key, input) {
            TextOutcome::Cancelled => {
                model.overlay = None;
                Vec::new()
            }
            TextOutcome::Editing(input) => {
                model.overlay = Some(Overlay::MemorySearch(input));
                Vec::new()
            }
            TextOutcome::Submitted(input) => {
                model.overlay = None;
                if input.is_blank() {
                    model.lessons.search = None;
                    return vec![Cmd::FetchLessons];
                }
                model.lessons.search = Some(input.trimmed().to_owned());
                vec![Cmd::SearchMemory(input.trimmed().to_owned())]
            }
        },
    }
}

fn on_plan_key(model: &mut Model, key: KeyPress) -> Vec<Cmd> {
    match key.key {
        Key::Esc => {
            model.overlay = None;
            Vec::new()
        }
        Key::Char('a') => {
            model.overlay = None;
            model
                .current_run()
                .map(|run| vec![Cmd::ApprovePlan(run.id)])
                .unwrap_or_default()
        }
        Key::Char('x') => {
            model.overlay = Some(Overlay::RejectFeedback(TextInput::new("feedback")));
            Vec::new()
        }
        Key::Enter | Key::Down | Key::Up | Key::Char('j' | 'k') => {
            let len = model
                .current_run()
                .and_then(|run| run.plan.as_ref())
                .map(|plan| crate::plan::PlanView::parse(plan).tasks.len())
                .unwrap_or_default();
            let delta = vertical(key).unwrap_or(0);
            step(&mut model.detail.board_selected, len, delta);
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// What a key did to a text field.
enum TextOutcome {
    /// `Esc`.
    Cancelled,
    /// Still typing.
    Editing(TextInput),
    /// `Enter`.
    Submitted(TextInput),
}

fn text_key(key: KeyPress, mut input: TextInput) -> TextOutcome {
    match key.key {
        Key::Esc => TextOutcome::Cancelled,
        Key::Enter => TextOutcome::Submitted(input),
        Key::Backspace => {
            input.backspace();
            TextOutcome::Editing(input)
        }
        Key::Char(c) if !key.ctrl && !key.alt => {
            input.push(c);
            TextOutcome::Editing(input)
        }
        Key::Char('u') if key.ctrl => {
            input.value.clear();
            TextOutcome::Editing(input)
        }
        _ => TextOutcome::Editing(input),
    }
}

#[cfg(test)]
mod tests {
    use kevin_api::dto::{RouteScoreDto, RunStatusDto, RunSummaryDto, TaskCountsDto, UsageDto};
    use kevin_domain::ids::RunId;

    use super::{Cmd, Key, KeyPress, Model, Msg, Overlay, Screen, update};
    use crate::model::{LessonsTab, RouteSort};

    fn press(key: Key) -> Msg {
        Msg::Key(KeyPress::new(key))
    }

    fn key(c: char) -> Msg {
        Msg::Key(KeyPress::char(c))
    }

    fn summary(id: RunId, status: RunStatusDto) -> RunSummaryDto {
        RunSummaryDto {
            id,
            status,
            goal_excerpt: "goal".to_owned(),
            usage: UsageDto::default(),
            task_counts: TaskCountsDto::default(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn score(kind: &str, alias: &str, attempts: u32, successes: u32) -> RouteScoreDto {
        RouteScoreDto {
            kind: kind.to_owned(),
            alias: alias.to_owned(),
            attempts,
            successes,
            mean_quality: None,
            mean_cost_usd: None,
            mean_wall_ms: None,
            sampled_score: None,
        }
    }

    fn with_runs() -> Model {
        let mut model = Model::new("http://localhost:7777/");
        model.runs.items = vec![
            summary(
                RunId::from_uuid(uuid::Uuid::from_u128(1)),
                RunStatusDto::Executing,
            ),
            summary(
                RunId::from_uuid(uuid::Uuid::from_u128(2)),
                RunStatusDto::Completed,
            ),
        ];
        model
    }

    #[test]
    fn digits_switch_screens_and_load_them() {
        let mut model = with_runs();
        assert_eq!(update(&mut model, key('4')), vec![Cmd::FetchRoutes(None)]);
        assert_eq!(model.screen, Screen::Routes);
        assert_eq!(
            update(&mut model, key('5')),
            vec![Cmd::FetchLessons, Cmd::FetchProposals]
        );
        assert_eq!(update(&mut model, key('6')), vec![Cmd::FetchWorkers]);
        assert_eq!(update(&mut model, key('3')), vec![Cmd::FetchQuestions]);
        assert_eq!(
            update(&mut model, key('1')),
            vec![Cmd::FetchRuns, Cmd::FetchDrain]
        );
    }

    #[test]
    fn question_mark_opens_the_inbox_and_h_the_help() {
        let mut model = with_runs();
        assert_eq!(update(&mut model, key('?')), vec![Cmd::FetchQuestions]);
        assert_eq!(model.screen, Screen::Questions);

        assert!(update(&mut model, key('h')).is_empty());
        assert_eq!(model.overlay, Some(Overlay::Help));
        assert!(update(&mut model, press(Key::Esc)).is_empty());
        assert_eq!(model.overlay, None, "Esc closes the help");
    }

    #[test]
    fn quit_keys_stop_the_session() {
        let mut model = with_runs();
        assert_eq!(update(&mut model, key('Q')), vec![Cmd::Quit]);
        assert!(model.quit);

        let mut model = with_runs();
        let cmds = update(&mut model, Msg::Key(KeyPress::ctrl(Key::Char('c'))));
        assert_eq!(cmds, vec![Cmd::Quit]);
        assert!(model.quit);
    }

    #[test]
    fn l_toggles_the_client_log_pane() {
        let mut model = with_runs();
        assert!(update(&mut model, key('L')).is_empty());
        assert!(model.show_client_log);
        let _ = update(&mut model, key('L'));
        assert!(!model.show_client_log);
    }

    #[test]
    fn g_and_shift_g_jump_to_the_ends_of_the_list() {
        let mut model = with_runs();
        let _ = update(&mut model, key('G'));
        assert_eq!(model.runs.selected, 1);
        let _ = update(&mut model, key('g'));
        assert_eq!(model.runs.selected, 0);
    }

    #[test]
    fn j_and_k_are_clamped_to_the_list() {
        let mut model = with_runs();
        for _ in 0..5 {
            let _ = update(&mut model, key('j'));
        }
        assert_eq!(model.runs.selected, 1);
        for _ in 0..5 {
            let _ = update(&mut model, key('k'));
        }
        assert_eq!(model.runs.selected, 0);
    }

    #[test]
    fn the_runs_filter_modal_sets_the_status_query() {
        let mut model = with_runs();
        let _ = update(&mut model, key('/'));
        for c in "failed".chars() {
            let _ = update(&mut model, key(c));
        }
        assert_eq!(update(&mut model, press(Key::Enter)), vec![Cmd::FetchRuns]);
        assert_eq!(model.runs.status_filter.as_deref(), Some("failed"));

        // An empty filter clears it again.
        let _ = update(&mut model, key('/'));
        for _ in 0..6 {
            let _ = update(&mut model, press(Key::Backspace));
        }
        let _ = update(&mut model, press(Key::Enter));
        assert_eq!(model.runs.status_filter, None);
    }

    #[test]
    fn escaping_a_prompt_discards_it() {
        let mut model = with_runs();
        let _ = update(&mut model, key('n'));
        let _ = update(&mut model, key('x'));
        assert!(update(&mut model, press(Key::Esc)).is_empty());
        assert_eq!(model.overlay, None);
    }

    #[test]
    fn a_new_run_prompt_creates_a_run() {
        let mut model = with_runs();
        let _ = update(&mut model, key('n'));
        for c in "add /readyz".chars() {
            let _ = update(&mut model, key(c));
        }
        assert_eq!(
            update(&mut model, press(Key::Enter)),
            vec![Cmd::CreateRun("add /readyz".to_owned())]
        );
    }

    #[test]
    fn a_blank_new_run_prompt_does_nothing() {
        let mut model = with_runs();
        let _ = update(&mut model, key('n'));
        assert!(update(&mut model, press(Key::Enter)).is_empty());
        assert_eq!(model.overlay, None);
    }

    #[test]
    fn c_cancels_the_highlighted_run_and_y_yanks_its_id() {
        let mut model = with_runs();
        let id = model.runs.items[0].id;
        assert_eq!(update(&mut model, key('c')), vec![Cmd::CancelRun(id, None)]);
        assert_eq!(
            update(&mut model, key('y')),
            vec![Cmd::Yank(id.to_string())]
        );
    }

    #[test]
    fn routes_cycles_kinds_and_sort_orders() {
        let mut model = with_runs();
        model.screen = Screen::Routes;
        model.routes.items = vec![
            score("implement", "a", 10, 9),
            score("test", "b", 10, 5),
            score("implement", "c", 10, 1),
        ];

        assert_eq!(
            update(&mut model, key('k')),
            vec![Cmd::FetchRoutes(Some("implement".to_owned()))]
        );
        assert_eq!(
            update(&mut model, key('k')),
            vec![Cmd::FetchRoutes(Some("test".to_owned()))]
        );
        assert_eq!(
            update(&mut model, key('k')),
            vec![Cmd::FetchRoutes(None)],
            "the cycle wraps back to every kind"
        );

        assert_eq!(model.routes.sort, RouteSort::Score);
        let _ = update(&mut model, key('s'));
        assert_eq!(model.routes.sort, RouteSort::Success);
        assert_eq!(
            model.routes.sorted().first().map(|row| row.alias.as_str()),
            Some("a")
        );
    }

    #[test]
    fn lessons_tab_switches_between_the_two_lists() {
        let mut model = with_runs();
        model.screen = Screen::Lessons;
        assert!(update(&mut model, press(Key::Tab)).is_empty());
        assert_eq!(model.lessons.tab, LessonsTab::Proposals);
        let _ = update(&mut model, press(Key::Tab));
        assert_eq!(model.lessons.tab, LessonsTab::Lessons);
    }

    #[test]
    fn a_key_clears_the_previous_status_line() {
        let mut model = with_runs();
        let _ = update(&mut model, Msg::ClientError("boom".to_owned()));
        assert!(model.status.is_some());
        let _ = update(&mut model, key('L'));
        assert!(model.status.is_none(), "the next key clears the banner");
    }

    #[test]
    fn init_asks_for_the_focused_run_when_one_was_given() {
        let id = RunId::from_uuid(uuid::Uuid::from_u128(7));
        let cmds = super::init(Some(id));
        assert!(cmds.contains(&Cmd::FetchRun(id)), "{cmds:?}");
        assert!(cmds.contains(&Cmd::Subscribe(None)), "{cmds:?}");
    }
}
