//! WS-17 acceptance 5 — the TUI works against the fake API end to end.
//!
//! A real `KevinClient` over a real loopback socket in front of
//! `kevin_testkit::fake_api`: the reducer's start-up commands are performed,
//! their answers folded back, the SSE firehose is consumed, and a plan is
//! approved with `a`. Nothing here touches the store.

mod support;

use std::time::Duration;

use futures::StreamExt as _;
use kevin_api::client::KevinClient;
use kevin_testkit::fake_api::{self, FakeRuntime, ServerHandle};
use kevin_tui::keys::{Key, KeyPress};
use kevin_tui::msg::{Cmd, Msg};
use kevin_tui::runtime::{EVENT_TYPES, event_msg, execute};
use kevin_tui::{Model, Screen, init, update};
use secrecy::SecretString;

use support::{
    cost_report, log_lines, proposals, question_with_options, render, routes, run_awaiting_plan,
    run_executing, tasks, workers,
};

/// A fake daemon serving the WS-17 fixtures.
async fn daemon() -> (FakeRuntime, ServerHandle, KevinClient) {
    let runtime = FakeRuntime::new();
    runtime.insert_run(run_executing());
    runtime.insert_run(run_awaiting_plan());
    for task in tasks() {
        runtime.insert_task(task);
    }
    runtime.insert_question(question_with_options());
    runtime.insert_log(support::task_1(), log_lines(1, 6));
    runtime.with_state(|state| {
        state.routes = routes();
        state.proposals = proposals();
        state.memory = support::lessons();
        state.workers = workers();
        state.cost = cost_report();
    });
    let server = fake_api::spawn(&runtime).await;
    let client = KevinClient::connect(&server.base_url(), SecretString::from(fake_api::TOKEN))
        .expect("the fake API base URL parses");
    (runtime, server, client)
}

/// Performs `cmds` against `client` and folds every answer back into `model`,
/// exactly like `kevin_tui::runtime`'s event loop does.
async fn drive(model: &mut Model, client: &KevinClient, cmds: Vec<Cmd>) {
    let mut queue = cmds;
    // Bound the loop: a refetch may schedule another refetch.
    for _ in 0..4 {
        let mut next = Vec::new();
        for cmd in std::mem::take(&mut queue) {
            let Some(result) = execute(client, cmd).await else {
                continue;
            };
            let msg = result.unwrap_or_else(|err| Msg::ClientError(err.to_string()));
            next.extend(update(model, msg));
        }
        if next.is_empty() {
            return;
        }
        queue = next;
    }
}

#[tokio::test]
async fn ac_ws17_5_the_startup_snapshot_fills_every_screen() {
    let (_runtime, server, client) = daemon().await;

    let mut model = Model::new(server.base_url());
    model.now = support::now();
    drive(&mut model, &client, init(None)).await;

    assert_eq!(model.runs.items.len(), 2, "the runs list is populated");
    assert_eq!(
        model.inbox.items.len(),
        1,
        "the open question is in the inbox"
    );
    assert_eq!(model.drain.map(|d| d.draining), Some(false));

    // Each screen loads on entry.
    for (key, screen) in [
        ('4', Screen::Routes),
        ('5', Screen::Lessons),
        ('6', Screen::Workers),
    ] {
        let cmds = update(&mut model, Msg::Key(KeyPress::char(key)));
        assert_eq!(model.screen, screen);
        drive(&mut model, &client, cmds).await;
    }
    assert_eq!(model.routes.items.len(), 3);
    assert_eq!(model.lessons.lessons.len(), 1);
    assert_eq!(model.lessons.proposals.len(), 1);
    assert_eq!(model.workers.items.len(), 2);

    model.screen = Screen::Runs;
    model.status = None;
    // The fake daemon binds an ephemeral port; pin the footer for the snapshot.
    model.server = "http://127.0.0.1:7777/".to_owned();
    insta::assert_snapshot!("end_to_end_runs_80x24", render(&model, 80, 24));

    server.shutdown().await;
}

#[tokio::test]
async fn ac_ws17_5_opening_a_run_loads_its_board_transcript_and_cost() {
    let (_runtime, server, client) = daemon().await;

    let mut model = Model::new(server.base_url());
    model.now = support::now();
    drive(&mut model, &client, init(None)).await;

    // `Enter` on the first run, then `Enter` on the first board row.
    let cmds = update(&mut model, Msg::Key(KeyPress::new(Key::Enter)));
    drive(&mut model, &client, cmds).await;
    assert_eq!(model.screen, Screen::RunDetail);
    assert_eq!(model.detail.tasks.len(), 3, "the task board arrived");
    assert!(model.detail.cost.is_some(), "the cost footer arrived");

    // The board groups pending first, so `j` lands on the running task, the one
    // the fake daemon has a transcript for.
    let _ = update(&mut model, Msg::Key(KeyPress::char('j')));
    assert_eq!(
        model.detail.selected_task().map(|task| task.id),
        Some(support::task_1())
    );
    let cmds = update(&mut model, Msg::Key(KeyPress::new(Key::Enter)));
    assert!(
        cmds.contains(&Cmd::FollowTaskLog(
            model.detail.focused_task.expect("focused")
        )),
        "follow mode subscribes to the transcript: {cmds:?}"
    );
    drive(&mut model, &client, cmds).await;
    assert_eq!(model.detail.log.len(), 6, "the transcript page arrived");
    assert_eq!(model.detail.log_seq, Some(6));

    server.shutdown().await;
}

#[tokio::test]
async fn ac_ws17_5_approving_a_plan_from_the_modal_reaches_the_api() {
    let (runtime, server, client) = daemon().await;

    let mut model = Model::new(server.base_url());
    model.now = support::now();
    model.screen = Screen::RunDetail;
    drive(&mut model, &client, vec![Cmd::FetchRun(support::run_b())]).await;

    assert_eq!(
        model.overlay,
        Some(kevin_tui::Overlay::PlanApproval),
        "a run in awaiting_plan_approval opens the approval modal"
    );

    let cmds = update(&mut model, Msg::Key(KeyPress::char('a')));
    assert_eq!(cmds, vec![Cmd::ApprovePlan(support::run_b())]);
    assert_eq!(model.overlay, None, "the modal closes on approval");
    drive(&mut model, &client, cmds).await;

    runtime.with_state(|state| {
        assert!(
            state.commands.contains(&"approve_plan".to_owned()),
            "{:?}",
            state.commands
        );
    });
    assert_eq!(
        model.detail.run.as_ref().map(|run| run.status),
        Some(kevin_api::dto::RunStatusDto::Executing)
    );

    server.shutdown().await;
}

#[tokio::test]
async fn ac_ws17_5_rejecting_a_plan_sends_the_typed_feedback() {
    let (runtime, server, client) = daemon().await;

    let mut model = Model::new(server.base_url());
    model.now = support::now();
    model.screen = Screen::RunDetail;
    drive(&mut model, &client, vec![Cmd::FetchRun(support::run_b())]).await;

    let _ = update(&mut model, Msg::Key(KeyPress::char('x')));
    for c in "split the migration".chars() {
        let _ = update(&mut model, Msg::Key(KeyPress::char(c)));
    }
    let cmds = update(&mut model, Msg::Key(KeyPress::new(Key::Enter)));
    assert_eq!(
        cmds,
        vec![Cmd::RejectPlan(
            support::run_b(),
            "split the migration".to_owned()
        )]
    );
    drive(&mut model, &client, cmds).await;

    runtime.with_state(|state| {
        assert!(
            state.commands.contains(&"reject_plan".to_owned()),
            "{:?}",
            state.commands
        );
    });

    server.shutdown().await;
}

#[tokio::test]
async fn ac_ws17_5_the_event_firehose_feeds_the_reducer_over_a_real_socket() {
    let (runtime, server, client) = daemon().await;

    let mut model = Model::new(server.base_url());
    model.now = support::now();
    drive(&mut model, &client, init(None)).await;
    let cmds = update(&mut model, Msg::Key(KeyPress::new(Key::Enter)));
    drive(&mut model, &client, cmds).await;
    model.detail.timeline.clear();

    let mut stream = Box::pin(client.events(Some(EVENT_TYPES), Some(0)));
    runtime.publish("run.started", support::run_a()).await;
    runtime
        .publish("task.attempt_started", support::run_a())
        .await;

    for _ in 0..2 {
        let item = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("the SSE stream delivers within five seconds")
            .expect("the stream is still open");
        let cmds = update(&mut model, event_msg(item));
        drive(&mut model, &client, cmds).await;
    }

    assert_eq!(
        model.detail.timeline.len(),
        2,
        "both events landed in the open run's phase timeline"
    );
    assert_eq!(model.stream_position, Some(2));
    assert_eq!(model.resync_count, 0);

    drop(stream);
    server.shutdown().await;
}

#[tokio::test]
async fn ac_ws17_5_an_unreachable_daemon_is_reported_not_panicked() {
    // Bind and drop, so the port is almost certainly free.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("a loopback port");
    let addr = listener.local_addr().expect("a local address");
    drop(listener);

    let client = KevinClient::connect(
        &format!("http://{addr}/"),
        SecretString::from(fake_api::TOKEN),
    )
    .expect("the URL parses");
    let err = execute(&client, Cmd::FetchRuns)
        .await
        .expect("fetching runs is an HTTP command")
        .expect_err("nothing is listening");

    let mut model = Model::new(format!("http://{addr}/"));
    let cmds = update(&mut model, Msg::ClientError(err.to_string()));
    assert!(cmds.is_empty());
    assert!(!model.quit);
    assert!(model.status.is_some());
}
