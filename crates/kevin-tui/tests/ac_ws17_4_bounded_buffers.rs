//! WS-17 acceptance 4 — the log buffer is bounded.
//!
//! `plan/07-api-and-tui.md` §4: 5 000 transcript lines per focused task and 500
//! timeline events per run, oldest first out. A run that produces a million
//! lines must cost the same memory as one that produces ten thousand.

mod support;

use kevin_tui::model::{CLIENT_LOG_CAPACITY, LOG_CAPACITY, TIMELINE_CAPACITY};
use kevin_tui::msg::Msg;
use kevin_tui::update;

use support::{RUN_A, detail_model, event, log_lines};

#[test]
fn ac_ws17_4_the_transcript_ring_never_exceeds_its_capacity() {
    let mut model = detail_model();
    model.detail.log.clear();
    model.detail.log_seq = None;

    let overflow = 1_234u64;
    let total = LOG_CAPACITY as u64 + overflow;
    // Feed the lines in pages, exactly like the poll and the SSE log stream do.
    for page in 0..total / 500 {
        let _ = update(
            &mut model,
            Msg::LogLines(support::task_1(), log_lines(page * 500 + 1, 500)),
        );
    }
    let sent = (total / 500) * 500;
    let _ = update(
        &mut model,
        Msg::LogLines(support::task_1(), log_lines(sent + 1, total - sent)),
    );

    assert_eq!(model.detail.log.len(), LOG_CAPACITY, "the ring is capped");
    assert_eq!(
        model.detail.log.dropped(),
        overflow,
        "every evicted line is counted so the pane can say so"
    );
    assert_eq!(
        model.detail.log_seq,
        Some(total),
        "the highest seq is kept so a resync resumes after it"
    );
    assert_eq!(
        model.detail.log.iter().next().map(|line| line.seq),
        Some(overflow + 1),
        "the oldest surviving line is the first one that was not evicted"
    );
    assert_eq!(
        model.detail.log.last().map(|line| line.seq),
        Some(total),
        "the newest line is the last one pushed"
    );
}

#[test]
fn ac_ws17_4_lines_for_another_task_are_dropped_on_the_floor() {
    let mut model = detail_model();
    let before = model.detail.log.len();
    let _ = update(
        &mut model,
        Msg::LogLines(support::task_2(), log_lines(1, 50)),
    );
    assert_eq!(
        model.detail.log.len(),
        before,
        "a late page from the previous task never pollutes the focused one"
    );
}

#[test]
fn ac_ws17_4_the_run_timeline_is_bounded_too() {
    let mut model = detail_model();
    model.detail.timeline.clear();

    let overflow = 37u64;
    let total = TIMELINE_CAPACITY as u64 + overflow;
    for position in 1..=total {
        let _ = update(
            &mut model,
            Msg::ApiEvent(Box::new(event(
                position,
                RUN_A,
                "task.attempt_started",
                position,
            ))),
        );
    }

    assert_eq!(model.detail.timeline.len(), TIMELINE_CAPACITY);
    assert_eq!(model.detail.timeline.dropped(), overflow);
    assert_eq!(model.resync_count, 0, "contiguous versions never resync");
}

#[test]
fn ac_ws17_4_the_client_log_pane_is_bounded() {
    let mut model = detail_model();
    for index in 0..CLIENT_LOG_CAPACITY + 25 {
        let _ = update(&mut model, Msg::ClientError(format!("failure {index}")));
    }
    assert_eq!(model.client_log.len(), CLIENT_LOG_CAPACITY);
    assert_eq!(model.client_log.dropped(), 25);
}

#[test]
fn ac_ws17_4_focusing_another_task_starts_a_fresh_ring() {
    let mut model = detail_model();
    let _ = update(
        &mut model,
        Msg::LogLines(support::task_1(), log_lines(7, 20)),
    );
    assert!(model.detail.log.len() > 6);

    // `Enter` on another task in the board.
    model.detail.board_selected = 0; // "pending" group first: task_2
    let cmds = update(
        &mut model,
        Msg::Key(kevin_tui::keys::KeyPress::new(kevin_tui::keys::Key::Enter)),
    );

    assert_eq!(model.detail.focused_task, Some(support::task_2()));
    assert_eq!(
        model.detail.log.len(),
        0,
        "the previous transcript is dropped"
    );
    assert_eq!(model.detail.log_seq, None);
    assert!(
        cmds.contains(&kevin_tui::msg::Cmd::FetchTaskLog {
            task_id: support::task_2(),
            after_seq: None
        }),
        "{cmds:?}"
    );
}
