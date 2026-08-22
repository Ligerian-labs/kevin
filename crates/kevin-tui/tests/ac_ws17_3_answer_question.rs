//! WS-17 acceptance 3 — answering a question from the inbox calls the API.
//!
//! The reducer turns the keystrokes into a `Cmd::AnswerQuestion`, the runtime
//! turns that into `POST /api/v1/questions/{id}/answer`, and
//! `kevin_testkit::fake_api` records the command it received.

mod support;

use kevin_api::client::KevinClient;
use kevin_testkit::fake_api::{self, FakeRuntime};
use kevin_tui::keys::{Key, KeyPress};
use kevin_tui::model::InboxFocus;
use kevin_tui::msg::{Cmd, Msg};
use kevin_tui::runtime::execute;
use kevin_tui::{Model, Screen, update};
use secrecy::SecretString;

use support::{question_multi_select, question_with_options, seeded_model};

fn key(c: char) -> Msg {
    Msg::Key(KeyPress::char(c))
}

fn press(key: Key) -> Msg {
    Msg::Key(KeyPress::new(key))
}

fn inbox_model() -> Model {
    let mut model = seeded_model();
    model.screen = Screen::Questions;
    model
}

#[tokio::test]
async fn ac_ws17_3_answering_a_single_select_question_posts_the_chosen_option() {
    let runtime = FakeRuntime::new();
    runtime.insert_question(question_with_options());
    let server = fake_api::spawn(&runtime).await;
    let client = KevinClient::connect(&server.base_url(), SecretString::from(fake_api::TOKEN))
        .expect("the fake API base URL parses");

    // Tab into the options, pick the second one, submit.
    let mut model = inbox_model();
    let _ = update(&mut model, press(Key::Tab));
    assert_eq!(model.inbox.focus, InboxFocus::Options);
    let _ = update(&mut model, key('j'));
    let cmds = update(&mut model, press(Key::Enter));

    let Some(Cmd::AnswerQuestion(id, answer)) = cmds.first().cloned() else {
        panic!("expected an answer command, got {cmds:?}");
    };
    assert_eq!(id, support::question_1());
    assert_eq!(answer.selected, vec!["axum 0.7".to_owned()]);
    assert_eq!(answer.free_text, None);

    // The runtime performs it against the real socket.
    let msg = execute(&client, Cmd::AnswerQuestion(id, answer))
        .await
        .expect("answering is an HTTP command")
        .expect("the fake API accepts the answer");
    let Msg::QuestionAnswered(question) = msg.clone() else {
        panic!("expected QuestionAnswered, got {msg:?}");
    };
    assert_eq!(question.status, "answered");

    runtime.with_state(|state| {
        assert!(
            state.commands.contains(&"answer_question".to_owned()),
            "the API issued the command: {:?}",
            state.commands
        );
        let stored = &state.questions[&support::QUESTION_1];
        assert_eq!(
            stored.answer.as_ref().map(|a| a.selected.clone()),
            Some(vec!["axum 0.7".to_owned()])
        );
    });

    // Folding the answer back removes the question from the inbox.
    let cmds = update(&mut model, msg);
    assert!(model.inbox.items.iter().all(|q| q.id != id));
    assert!(cmds.contains(&Cmd::FetchQuestions), "{cmds:?}");

    server.shutdown().await;
}

#[tokio::test]
async fn ac_ws17_3_a_multi_select_answer_carries_every_tick_and_the_free_text() {
    let runtime = FakeRuntime::new();
    runtime.insert_question(question_multi_select());
    let server = fake_api::spawn(&runtime).await;
    let client = KevinClient::connect(&server.base_url(), SecretString::from(fake_api::TOKEN))
        .expect("the fake API base URL parses");

    let mut model = inbox_model();
    let _ = update(&mut model, key('j')); // second question
    assert_eq!(model.inbox.selected, 1);
    let _ = update(&mut model, key(' ')); // tick kevin-store
    let _ = update(&mut model, key('j')); // next option
    let _ = update(&mut model, key(' ')); // tick kevin-memory

    // `t` opens the free-text modal; typing then Enter keeps the text.
    let _ = update(&mut model, key('t'));
    for c in "sqlx call sites only".chars() {
        let _ = update(&mut model, key(c));
    }
    let _ = update(&mut model, press(Key::Enter));
    assert_eq!(
        model.inbox.free_text.as_deref(),
        Some("sqlx call sites only")
    );

    let cmds = update(&mut model, press(Key::Enter));
    let Some(Cmd::AnswerQuestion(id, answer)) = cmds.first().cloned() else {
        panic!("expected an answer command, got {cmds:?}");
    };
    assert_eq!(id, support::question_2());
    assert_eq!(
        answer.selected,
        vec!["kevin-memory".to_owned(), "kevin-store".to_owned()],
        "both ticks are sent (the set is ordered by label)"
    );
    assert_eq!(answer.free_text.as_deref(), Some("sqlx call sites only"));

    execute(&client, Cmd::AnswerQuestion(id, answer))
        .await
        .expect("answering is an HTTP command")
        .expect("the fake API accepts the answer");
    runtime.with_state(|state| {
        let stored = &state.questions[&support::QUESTION_2];
        assert_eq!(stored.status, "answered");
        assert_eq!(
            stored.answer.as_ref().and_then(|a| a.free_text.clone()),
            Some("sqlx call sites only".to_owned())
        );
    });

    server.shutdown().await;
}

#[test]
fn ac_ws17_3_an_empty_answer_falls_back_to_the_default_or_is_refused() {
    // The first fixture has a default: `Enter` with nothing picked sends it.
    let mut model = inbox_model();
    let cmds = update(&mut model, Msg::Key(KeyPress::new(Key::Enter)));
    let Some(Cmd::AnswerQuestion(_, answer)) = cmds.first().cloned() else {
        panic!("expected the default to be sent, got {cmds:?}");
    };
    assert_eq!(answer.selected, vec!["axum 0.8".to_owned()]);

    // The second has none: the reducer refuses and says why.
    let mut model = inbox_model();
    model.inbox.selected = 1;
    let cmds = update(&mut model, Msg::Key(KeyPress::new(Key::Enter)));
    assert!(cmds.is_empty(), "{cmds:?}");
    assert!(
        model
            .status
            .as_deref()
            .is_some_and(|status| status.contains("pick an option")),
        "the operator is told what to do: {:?}",
        model.status
    );
}

#[tokio::test]
async fn ac_ws17_3_answering_an_already_answered_question_surfaces_the_api_error() {
    let runtime = FakeRuntime::new();
    let mut question = question_with_options();
    question.status = "answered".to_owned();
    runtime.insert_question(question);
    let server = fake_api::spawn(&runtime).await;
    let client = KevinClient::connect(&server.base_url(), SecretString::from(fake_api::TOKEN))
        .expect("the fake API base URL parses");

    let cmd = Cmd::AnswerQuestion(
        support::question_1(),
        kevin_api::dto::AnswerRequest {
            selected: vec!["axum 0.8".to_owned()],
            free_text: None,
        },
    );
    let err = execute(&client, cmd)
        .await
        .expect("answering is an HTTP command")
        .expect_err("the question is already answered");
    assert_eq!(err.code(), Some("question_already_answered"), "{err:?}");

    let mut model = inbox_model();
    let cmds = update(&mut model, Msg::ClientError(err.to_string()));
    assert!(cmds.is_empty());
    assert!(model.status.is_some(), "the failure reaches the status bar");

    server.shutdown().await;
}
