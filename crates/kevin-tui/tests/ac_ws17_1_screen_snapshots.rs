//! WS-17 acceptance 1 — `TestBackend` snapshots for each screen.
//!
//! `plan/11-testing.md` (`kevin-tui` row) asks for snapshots of every screen at
//! 80×24 and 120×40; `plan/07-api-and-tui.md` §Tests adds the modals. The
//! buffer `TestBackend` prints holds characters only, so these assert layout
//! and content, and `Theme` is unit-tested separately for the colour rules.

mod support;

use kevin_tui::model::{InboxFocus, LessonsTab, Overlay, Pane, RouteSort, TextInput};
use kevin_tui::{Model, Screen};

use support::{detail_model, render, seeded_model};

/// The two terminal sizes `plan/11` pins.
const SIZES: [(u16, u16); 2] = [(80, 24), (120, 40)];

fn assert_screens(name: &str, model: &Model) {
    for (width, height) in SIZES {
        insta::assert_snapshot!(
            format!("{name}_{width}x{height}"),
            render(model, width, height)
        );
    }
}

#[test]
fn ac_ws17_1_runs_screen_snapshot() {
    assert_screens("runs", &seeded_model());
}

#[test]
fn ac_ws17_1_run_detail_screen_snapshot() {
    assert_screens("run_detail", &detail_model());
}

#[test]
fn ac_ws17_1_run_detail_collapses_below_the_minimum_terminal() {
    // `plan/07` §Rendering rules: below 80×24 the panes collapse to one column.
    let mut model = detail_model();
    model.detail.pane = Pane::Transcript;
    insta::assert_snapshot!("run_detail_narrow_60x18", render(&model, 60, 18));
}

#[test]
fn ac_ws17_1_question_inbox_snapshot() {
    let mut model = seeded_model();
    model.screen = Screen::Questions;
    model.inbox.focus = InboxFocus::Options;
    model.inbox.option_selected = 1;
    assert_screens("question_inbox", &model);
}

#[test]
fn ac_ws17_1_question_inbox_multi_select_snapshot() {
    let mut model = seeded_model();
    model.screen = Screen::Questions;
    model.inbox.selected = 1;
    model.inbox.focus = InboxFocus::Options;
    model.inbox.chosen.insert("kevin-store".to_owned());
    model.inbox.chosen.insert("kevin-memory".to_owned());
    model.inbox.free_text = Some("only the sqlx call sites".to_owned());
    insta::assert_snapshot!("question_inbox_multi_select_80x24", render(&model, 80, 24));
}

#[test]
fn ac_ws17_1_plan_approval_modal_snapshot() {
    let mut model = seeded_model();
    model.screen = Screen::RunDetail;
    model.detail.run = Some(support::run_awaiting_plan());
    model.overlay = Some(Overlay::PlanApproval);
    assert_screens("plan_approval", &model);
}

#[test]
fn ac_ws17_1_routes_screen_snapshot() {
    let mut model = seeded_model();
    model.screen = Screen::Routes;
    model.routes.sort = RouteSort::Score;
    assert_screens("routes", &model);
}

#[test]
fn ac_ws17_1_lessons_and_proposals_screens_snapshot() {
    let mut model = seeded_model();
    model.screen = Screen::Lessons;
    assert_screens("lessons", &model);
    model.lessons.tab = LessonsTab::Proposals;
    insta::assert_snapshot!("proposals_80x24", render(&model, 80, 24));
}

#[test]
fn ac_ws17_1_workers_screen_snapshot() {
    let mut model = seeded_model();
    model.screen = Screen::Workers;
    assert_screens("workers", &model);
}

#[test]
fn ac_ws17_1_help_overlay_lists_the_plan_keybindings() {
    let mut model = seeded_model();
    model.overlay = Some(Overlay::Help);
    let rendered = render(&model, 120, 40);
    for keys in ["1..6", "Ctrl-c / Q", "Tab", "Space", "A / X"] {
        assert!(rendered.contains(keys), "help is missing `{keys}`");
    }
    insta::assert_snapshot!("help_120x40", rendered);
}

#[test]
fn ac_ws17_1_prompt_and_client_log_snapshot() {
    let mut model = seeded_model();
    model.show_client_log = true;
    model.client_log.push(kevin_tui::model::LogLine {
        at: support::now(),
        level: kevin_tui::model::Level::Info,
        text: "resync #1: the server asked for a resync".to_owned(),
    });
    model.overlay = Some(Overlay::NewRun(TextInput::with_value(
        "goal",
        "add a /readyz endpoint",
    )));
    insta::assert_snapshot!("new_run_prompt_80x24", render(&model, 80, 24));
}
