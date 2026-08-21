//! `PlanValidator` acceptance (`plan/05-orchestration.md` §3.4): cycles,
//! unknown kinds, too many tasks, dangling deps — plus the schema shape.

// Test helpers panic on broken fixtures; that is the intended behaviour.
#![allow(clippy::unwrap_used)]

use kevin_domain::plan::{
    DEFAULT_MAX_TASKS, PLAN_SCHEMA_ID, Plan, PlanEdge, PlanError, PlanTask, PlanValidator,
};
use kevin_domain::understanding::{UNDERSTANDING_SCHEMA_ID, Understanding};

fn t(id: &str, kind: &str) -> PlanTask {
    PlanTask::new(id, kind, format!("task {id}"))
}

#[test]
fn ac_ws01_4_plan_validator_rejects_cycles_unknown_kinds_too_many_tasks_and_dangling_deps() {
    let validator = PlanValidator::default();
    assert_eq!(validator.max_tasks(), DEFAULT_MAX_TASKS);

    // A valid DAG passes.
    let ok = Plan::new(
        vec![
            t("t1", "research"),
            t("t2", "implement").depends_on(["t1"]),
            t("t3", "test").depends_on(["t2"]),
            t("t4", "review").depends_on(["t2", "t3"]),
        ],
        "ok",
    );
    validator.validate(&ok).unwrap();
    let order: Vec<_> = ok
        .topological_order()
        .unwrap()
        .iter()
        .map(|p| p.id.as_str())
        .collect();
    assert_eq!(order, ["t1", "t2", "t3", "t4"]);

    // Cycle (through depends_on and through edges).
    let cycle = Plan::new(
        vec![
            t("t1", "implement").depends_on(["t3"]),
            t("t2", "test").depends_on(["t1"]),
            t("t3", "review").depends_on(["t2"]),
        ],
        "cycle",
    );
    let errs = validator.validate(&cycle).unwrap_err();
    assert!(
        matches!(&errs[..], [PlanError::Cycle { tasks }] if tasks.len() == 3),
        "{errs:?}"
    );
    let mut edge_cycle = Plan::new(vec![t("t1", "implement"), t("t2", "test")], "edge cycle");
    edge_cycle.edges = vec![PlanEdge::new("t1", "t2"), PlanEdge::new("t2", "t1")];
    assert!(matches!(
        validator.validate(&edge_cycle).unwrap_err()[..],
        [PlanError::Cycle { .. }]
    ));

    // Unknown kinds: not in the schema enum, or `custom` without `custom_kind`,
    // or a planner-only kind (`understand`, `plan`, …).
    let unknown = Plan::new(
        vec![t("t1", "deploy"), t("t2", "custom"), t("t3", "plan")],
        "kinds",
    );
    let errs = validator.validate(&unknown).unwrap_err();
    assert_eq!(
        errs,
        vec![
            PlanError::UnknownKind {
                task: "t1".into(),
                kind: "deploy".into()
            },
            PlanError::MissingCustomKind { task: "t2".into() },
            PlanError::UnknownKind {
                task: "t3".into(),
                kind: "plan".into()
            },
        ]
    );

    // Too many tasks (> max_tasks_per_run).
    let many = Plan::new(
        (1..=DEFAULT_MAX_TASKS + 1)
            .map(|i| t(&format!("t{i}"), "implement"))
            .collect(),
        "many",
    );
    assert_eq!(
        validator.validate(&many).unwrap_err(),
        vec![PlanError::TooManyTasks {
            count: DEFAULT_MAX_TASKS + 1,
            max: DEFAULT_MAX_TASKS
        }]
    );
    assert!(
        PlanValidator::new(DEFAULT_MAX_TASKS + 1)
            .validate(&many)
            .is_ok()
    );

    // Dangling dependencies and edges.
    let mut dangling = Plan::new(
        vec![t("t1", "implement").depends_on(["t9"]), t("t2", "test")],
        "dangling",
    );
    dangling.edges.push(PlanEdge::new("t2", "t8"));
    let errs = validator.validate(&dangling).unwrap_err();
    assert_eq!(
        errs,
        vec![
            PlanError::DanglingDependency {
                task: "t1".into(),
                depends_on: "t9".into()
            },
            PlanError::DanglingEdge {
                from: "t2".into(),
                to: "t8".into()
            },
        ]
    );

    // Every problem is reported at once (not just the first).
    let mut everything = Plan::new(
        vec![
            t("t1", "deploy").depends_on(["t1"]),
            t("t1", "implement"),
            t("bad", "test").depends_on(["t7"]),
        ],
        "everything",
    );
    everything.tasks[1].acceptance_criteria.clear();
    everything.tasks[2].title = "x".repeat(121);
    let errs = validator.validate(&everything).unwrap_err();
    for expected in [
        PlanError::UnknownKind {
            task: "t1".into(),
            kind: "deploy".into(),
        },
        PlanError::SelfDependency { task: "t1".into() },
        PlanError::DuplicateTaskId { id: "t1".into() },
        PlanError::NoAcceptanceCriteria { task: "t1".into() },
        PlanError::InvalidTaskId { id: "bad".into() },
        PlanError::DanglingDependency {
            task: "bad".into(),
            depends_on: "t7".into(),
        },
    ] {
        assert!(errs.contains(&expected), "missing {expected:?} in {errs:?}");
    }
    assert!(
        errs.iter()
            .any(|e| matches!(e, PlanError::InvalidTitle { .. }))
    );
    assert!(errs.iter().all(|e| !e.to_string().is_empty()));
    assert_eq!(
        validator.validate(&Plan::new(vec![], "empty")).unwrap_err(),
        vec![PlanError::Empty]
    );
}

#[test]
fn plan_and_understanding_match_the_schema_ids_and_shapes() {
    assert_eq!(PLAN_SCHEMA_ID, "kevin.plan.v1");
    assert_eq!(UNDERSTANDING_SCHEMA_ID, "kevin.understanding.v1");
    // Minimal planner output per schema `required` lists.
    let plan: Plan = serde_json::from_str(
        r#"{"tasks":[{"id":"t1","title":"x","kind":"implement","instructions":"do",
            "acceptance_criteria":["ok"],"depends_on":[]}],"rationale":"r"}"#,
    )
    .unwrap();
    PlanValidator::default().validate(&plan).unwrap();
    let understanding: Understanding = serde_json::from_str(
        r#"{"objective":"o","assumptions":[],"risks":[],"success_criteria":["s"],
            "proposed_questions":[],"complexity":"medium","suggested_task_kinds":["implement"]}"#,
    )
    .unwrap();
    understanding.validate().unwrap();
    // additionalProperties: false
    assert!(serde_json::from_str::<Plan>(r#"{"tasks":[],"rationale":"r","extra":1}"#).is_err());
    assert!(serde_json::from_str::<Understanding>(r#"{"objective":"o","nope":true}"#).is_err());
}
