//! WS-17 acceptance 2 — the reducer handles `Lagged`/`resync`.
//!
//! `plan/07-api-and-tui.md` §4: a `Resync` from the client, or a gap in an
//! aggregate's version sequence, must refetch the snapshots and reconnect the
//! stream from the last position the session saw.

mod support;

use kevin_tui::msg::{Cmd, Msg};
use kevin_tui::{Screen, update};

use support::{RUN_A, detail_model, event, seeded_model, tasks};

#[test]
fn ac_ws17_2_server_resync_refetches_every_snapshot_and_resubscribes() {
    let mut model = detail_model();
    // The session has already seen up to position 9.
    let _ = update(
        &mut model,
        Msg::ApiEvent(Box::new(event(9, RUN_A, "run.started", 1))),
    );

    let cmds = update(&mut model, Msg::Resync);

    assert_eq!(model.resync_count, 1);
    assert!(cmds.contains(&Cmd::FetchRuns), "{cmds:?}");
    assert!(cmds.contains(&Cmd::FetchQuestions), "{cmds:?}");
    assert!(cmds.contains(&Cmd::FetchRun(support::run_a())), "{cmds:?}");
    assert!(
        cmds.contains(&Cmd::FetchTasks(support::run_a())),
        "{cmds:?}"
    );
    assert!(
        cmds.contains(&Cmd::FetchTaskLog {
            task_id: support::task_1(),
            after_seq: Some(6)
        }),
        "the transcript resumes after the last seq: {cmds:?}"
    );
    assert_eq!(
        cmds.last(),
        Some(&Cmd::Subscribe(Some(9))),
        "the stream reconnects from the last position seen: {cmds:?}"
    );
}

#[test]
fn ac_ws17_2_a_version_gap_triggers_a_resync_on_its_own() {
    let mut model = detail_model();
    let cmds = update(
        &mut model,
        Msg::ApiEvent(Box::new(event(1, RUN_A, "run.started", 1))),
    );
    assert_eq!(model.resync_count, 0, "{cmds:?}");

    // Version 4 after version 1: the bus dropped 2 and 3.
    let cmds = update(
        &mut model,
        Msg::ApiEvent(Box::new(event(4, RUN_A, "run.plan_approved", 4))),
    );

    assert_eq!(model.resync_count, 1);
    assert!(cmds.contains(&Cmd::FetchRuns), "{cmds:?}");
    assert_eq!(cmds.last(), Some(&Cmd::Subscribe(Some(4))), "{cmds:?}");
    assert!(
        model
            .client_log
            .iter()
            .any(|line| line.text.contains("gap on run")),
        "the resync reason is logged for the `L` pane"
    );
}

#[test]
fn ac_ws17_2_contiguous_versions_never_resync() {
    let mut model = seeded_model();
    for (position, version) in (1..=5u64).zip(1..=5u64) {
        let _ = update(
            &mut model,
            Msg::ApiEvent(Box::new(event(position, RUN_A, "run.started", version))),
        );
    }
    assert_eq!(model.resync_count, 0);
    assert_eq!(model.stream_position, Some(5));
}

#[test]
fn ac_ws17_2_events_of_the_open_run_feed_the_timeline_and_refetches() {
    let mut model = detail_model();
    model.detail.timeline.clear();

    let cmds = update(
        &mut model,
        Msg::ApiEvent(Box::new(event(1, RUN_A, "task.attempt_started", 1))),
    );

    assert_eq!(model.detail.timeline.len(), 1);
    assert!(
        cmds.contains(&Cmd::FetchTasks(support::run_a())),
        "{cmds:?}"
    );
    assert!(
        cmds.contains(&Cmd::FetchCost(Some(support::run_a()))),
        "an attempt changes the cost footer: {cmds:?}"
    );
}

#[test]
fn ac_ws17_2_a_stream_error_is_logged_without_dropping_the_session() {
    let mut model = seeded_model();
    let cmds = update(&mut model, Msg::StreamError("connection reset".to_owned()));
    assert!(cmds.is_empty());
    assert!(!model.quit);
    assert_eq!(
        model.status.as_deref(),
        Some("stream: connection reset"),
        "the operator sees why the stream blinked"
    );
}

#[test]
fn ac_ws17_2_the_periodic_poll_only_asks_for_the_visible_screen() {
    let mut model = seeded_model();
    model.screen = Screen::Workers;
    let cmds = update(&mut model, Msg::Tick(support::now()));
    assert_eq!(cmds, vec![Cmd::FetchWorkers]);

    model.screen = Screen::RunDetail;
    model.detail.run = Some(support::run_executing());
    model.detail.tasks = tasks();
    model.detail.follow = false;
    let cmds = update(&mut model, Msg::Tick(support::now()));
    assert_eq!(
        cmds,
        vec![
            Cmd::FetchRun(support::run_a()),
            Cmd::FetchTasks(support::run_a()),
            Cmd::FetchCost(Some(support::run_a())),
        ]
    );
}
