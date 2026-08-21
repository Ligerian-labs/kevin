//! [`Plan`] — the planner's task graph, serialised exactly as the
//! `kevin.plan.v1` JSON schema (`plan/05-orchestration.md` §3.4), and the pure
//! [`PlanValidator`].

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::ids::TaskId;
use crate::kinds::{TaskKind, Tier};
use crate::values::{JsonSchema, Route, TaskSpec, WorkspacePolicy};

/// `$id` of the JSON schema this type mirrors.
pub const PLAN_SCHEMA_ID: &str = "kevin.plan.v1";

/// Default `orchestrator.max_tasks_per_run` and the schema's `maxItems`.
pub const DEFAULT_MAX_TASKS: usize = 24;

/// Maximum length of a plan task title (`maxLength`).
pub const MAX_TITLE_CHARS: usize = 120;

/// Name of the `kind` value that requires `custom_kind`.
pub const CUSTOM_KIND: &str = "custom";

/// Task kinds a plan may use (the schema's `kind` enum).
pub const PLAN_TASK_KINDS: [&str; 9] = [
    "research",
    "implement",
    "test",
    "review",
    "refactor",
    "debug",
    "write",
    "ops",
    CUSTOM_KIND,
];

/// A task graph proposed by the planner (`kevin.plan.v1`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    /// Tasks in plan order (1..=24).
    pub tasks: Vec<PlanTask>,
    /// Extra dependency edges `[from, to]` (`from` must finish before `to`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edges: Vec<PlanEdge>,
    /// Why this decomposition.
    pub rationale: String,
}

/// One task of a [`Plan`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanTask {
    /// Plan-local id matching `^t[0-9]{1,3}$`.
    pub id: String,
    /// Title (≤ 120 chars).
    pub title: String,
    /// One of [`PLAN_TASK_KINDS`].
    pub kind: String,
    /// Required when `kind == "custom"`; validated as `[a-z0-9][a-z0-9._-]*`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_kind: Option<String>,
    /// Instructions for the worker.
    pub instructions: String,
    /// Acceptance criteria (≥ 1).
    pub acceptance_criteria: Vec<String>,
    /// Plan-local ids of tasks that must succeed first.
    pub depends_on: Vec<String>,
    /// Input references (artifact ids, paths).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<String>,
    /// Router hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_tier: Option<Tier>,
    /// May run concurrently (default true).
    #[serde(default = "default_true")]
    pub parallel_safe: bool,
    /// Workspace preparation policy (default isolated).
    #[serde(default)]
    pub workspace_policy: WorkspacePolicy,
    /// Failure does not fail the run (default false).
    #[serde(default)]
    pub optional: bool,
    /// Worker may push (default false).
    #[serde(default)]
    pub allow_push: bool,
    /// JSON schema for the worker's structured output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<JsonSchema>,
    /// Route suggested by the orchestrator when the plan is recorded
    /// (`run.plan_proposed` carries "`TaskSpec` + `suggested_route`"); never
    /// produced by the planner itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_route: Option<Route>,
}

const fn default_true() -> bool {
    true
}

/// A dependency edge `[from, to]`, serialised as a two-element array.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PlanEdge(pub String, pub String);

impl PlanEdge {
    /// Builds an edge.
    pub fn new(from: impl Into<String>, to: impl Into<String>) -> Self {
        Self(from.into(), to.into())
    }

    /// The prerequisite task id.
    #[must_use]
    pub fn from(&self) -> &str {
        &self.0
    }

    /// The dependent task id.
    #[must_use]
    pub fn to(&self) -> &str {
        &self.1
    }
}

impl PlanTask {
    /// A minimal task of the given kind with one acceptance criterion.
    #[must_use]
    pub fn new(id: impl Into<String>, kind: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            kind: kind.into(),
            custom_kind: None,
            instructions: String::new(),
            acceptance_criteria: vec!["done".to_owned()],
            depends_on: Vec::new(),
            inputs: Vec::new(),
            suggested_tier: None,
            parallel_safe: true,
            workspace_policy: WorkspacePolicy::Isolated,
            optional: false,
            allow_push: false,
            output_schema: None,
            suggested_route: None,
        }
    }

    /// Adds dependencies.
    #[must_use]
    pub fn depends_on<I, S>(mut self, ids: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.depends_on.extend(ids.into_iter().map(Into::into));
        self
    }

    /// Resolves the plan `kind`/`custom_kind` pair into a [`TaskKind`].
    pub fn task_kind(&self) -> Result<TaskKind, PlanError> {
        if self.kind == CUSTOM_KIND {
            let name = self
                .custom_kind
                .as_deref()
                .ok_or_else(|| PlanError::MissingCustomKind {
                    task: self.id.clone(),
                })?;
            return TaskKind::custom(name).map_err(|_| PlanError::InvalidCustomKind {
                task: self.id.clone(),
                name: name.to_owned(),
            });
        }
        if !PLAN_TASK_KINDS.contains(&self.kind.as_str()) {
            return Err(PlanError::UnknownKind {
                task: self.id.clone(),
                kind: self.kind.clone(),
            });
        }
        self.kind
            .parse::<TaskKind>()
            .map_err(|_| PlanError::UnknownKind {
                task: self.id.clone(),
                kind: self.kind.clone(),
            })
    }

    /// Builds the [`TaskSpec`] for this task, mapping plan-local dependency
    /// ids through `ids`. `inputs` are left empty: artifact resolution is the
    /// orchestrator's job.
    pub fn to_task_spec(&self, ids: &BTreeMap<String, TaskId>) -> Result<TaskSpec, PlanError> {
        let depends_on = self
            .depends_on
            .iter()
            .map(|dep| {
                ids.get(dep)
                    .copied()
                    .ok_or_else(|| PlanError::DanglingDependency {
                        task: self.id.clone(),
                        depends_on: dep.clone(),
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(TaskSpec {
            title: self.title.clone(),
            instructions: self.instructions.clone(),
            inputs: Vec::new(),
            acceptance_criteria: self.acceptance_criteria.clone(),
            depends_on,
            workspace_policy: self.workspace_policy,
            output_schema: self.output_schema.clone(),
            optional: self.optional,
            parallel_safe: self.parallel_safe,
            allow_push: self.allow_push,
        })
    }
}

impl Plan {
    /// A plan with the given tasks and no extra edges.
    #[must_use]
    pub fn new(tasks: Vec<PlanTask>, rationale: impl Into<String>) -> Self {
        Self {
            tasks,
            edges: Vec::new(),
            rationale: rationale.into(),
        }
    }

    /// Looks a task up by plan-local id.
    #[must_use]
    pub fn task(&self, id: &str) -> Option<&PlanTask> {
        self.tasks.iter().find(|t| t.id == id)
    }

    /// All prerequisites of `id`: its `depends_on` plus every edge whose `to`
    /// is `id`, deduplicated and in first-seen order.
    #[must_use]
    pub fn dependencies_of(&self, id: &str) -> Vec<&str> {
        let mut seen = BTreeSet::new();
        let mut deps = Vec::new();
        let own = self
            .task(id)
            .map(|t| t.depends_on.iter())
            .into_iter()
            .flatten();
        let edges = self
            .edges
            .iter()
            .filter(|e| e.to() == id)
            .map(PlanEdge::from);
        for dep in own.map(String::as_str).chain(edges) {
            if seen.insert(dep) {
                deps.push(dep);
            }
        }
        deps
    }

    /// Tasks in a dependency-respecting order (stable: plan order among
    /// ready tasks). Errors when the graph has a cycle or dangling references.
    pub fn topological_order(&self) -> Result<Vec<&PlanTask>, PlanError> {
        let ids: BTreeSet<&str> = self.tasks.iter().map(|t| t.id.as_str()).collect();
        let mut indegree: BTreeMap<&str, usize> = BTreeMap::new();
        let mut dependents: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for task in &self.tasks {
            let deps = self.dependencies_of(&task.id);
            for dep in &deps {
                if !ids.contains(dep) {
                    return Err(PlanError::DanglingDependency {
                        task: task.id.clone(),
                        depends_on: (*dep).to_owned(),
                    });
                }
                dependents.entry(dep).or_default().push(task.id.as_str());
            }
            indegree.insert(task.id.as_str(), deps.len());
        }
        let mut ready: VecDeque<&str> = self
            .tasks
            .iter()
            .map(|t| t.id.as_str())
            .filter(|id| indegree.get(id).copied().unwrap_or(0) == 0)
            .collect();
        let mut order = Vec::with_capacity(self.tasks.len());
        while let Some(id) = ready.pop_front() {
            order.push(id);
            if let Some(next) = dependents.get(id) {
                for dependent in next {
                    if let Some(count) = indegree.get_mut(dependent) {
                        *count -= 1;
                        if *count == 0 {
                            ready.push_back(dependent);
                        }
                    }
                }
            }
        }
        if order.len() != self.tasks.len() {
            let stuck: Vec<String> = self
                .tasks
                .iter()
                .map(|t| t.id.clone())
                .filter(|id| !order.contains(&id.as_str()))
                .collect();
            return Err(PlanError::Cycle { tasks: stuck });
        }
        // Preserve plan order among tasks that became ready in the same wave:
        // Kahn above already processes them FIFO in plan order, so `order` is
        // stable. Map back to tasks.
        Ok(order.into_iter().filter_map(|id| self.task(id)).collect())
    }

    /// Assigns a [`TaskId`] to every plan task (in plan order) and builds the
    /// specs. `ids` must have one id per task.
    pub fn task_specs(
        &self,
        ids: &[TaskId],
    ) -> Result<Vec<(TaskId, TaskKind, TaskSpec)>, PlanError> {
        if ids.len() != self.tasks.len() {
            return Err(PlanError::TaskIdCount {
                expected: self.tasks.len(),
                got: ids.len(),
            });
        }
        let map: BTreeMap<String, TaskId> = self
            .tasks
            .iter()
            .zip(ids)
            .map(|(t, id)| (t.id.clone(), *id))
            .collect();
        self.tasks
            .iter()
            .zip(ids)
            .map(|(task, id)| {
                let mut spec = task.to_task_spec(&map)?;
                // Edges are dependencies too.
                for dep in self.dependencies_of(&task.id) {
                    if let Some(dep_id) = map.get(dep)
                        && !spec.depends_on.contains(dep_id)
                    {
                        spec.depends_on.push(*dep_id);
                    }
                }
                Ok((*id, task.task_kind()?, spec))
            })
            .collect()
    }
}

/// A validation failure of a [`Plan`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    /// No tasks at all.
    #[error("plan has no tasks")]
    Empty,
    /// More tasks than allowed.
    #[error("plan has {count} tasks, maximum is {max}")]
    TooManyTasks {
        /// Tasks in the plan.
        count: usize,
        /// Configured maximum.
        max: usize,
    },
    /// The same id twice.
    #[error("duplicate task id `{id}`")]
    DuplicateTaskId {
        /// The id.
        id: String,
    },
    /// Id does not match `^t[0-9]{1,3}$`.
    #[error("task id `{id}` does not match ^t[0-9]{{1,3}}$")]
    InvalidTaskId {
        /// The id.
        id: String,
    },
    /// Title too long or empty.
    #[error("task `{task}` has an invalid title: {reason}")]
    InvalidTitle {
        /// The task.
        task: String,
        /// Why.
        reason: String,
    },
    /// No acceptance criteria.
    #[error("task `{task}` has no acceptance criteria")]
    NoAcceptanceCriteria {
        /// The task.
        task: String,
    },
    /// `kind` is not in [`PLAN_TASK_KINDS`].
    #[error("task `{task}` has unknown kind `{kind}`")]
    UnknownKind {
        /// The task.
        task: String,
        /// The kind.
        kind: String,
    },
    /// `kind == "custom"` without `custom_kind`.
    #[error("task `{task}` has kind `custom` but no custom_kind")]
    MissingCustomKind {
        /// The task.
        task: String,
    },
    /// `custom_kind` is not a valid name or not in the allow-list.
    #[error("task `{task}` has invalid or unknown custom kind `{name}`")]
    InvalidCustomKind {
        /// The task.
        task: String,
        /// The custom kind name.
        name: String,
    },
    /// `depends_on` names an unknown task.
    #[error("task `{task}` depends on unknown task `{depends_on}`")]
    DanglingDependency {
        /// The task.
        task: String,
        /// The missing dependency.
        depends_on: String,
    },
    /// An edge names an unknown task.
    #[error("edge [{from}, {to}] references an unknown task")]
    DanglingEdge {
        /// Edge source.
        from: String,
        /// Edge target.
        to: String,
    },
    /// A task depends on itself.
    #[error("task `{task}` depends on itself")]
    SelfDependency {
        /// The task.
        task: String,
    },
    /// The dependency graph has a cycle among these tasks.
    #[error("dependency cycle among tasks {tasks:?}")]
    Cycle {
        /// Tasks that never became ready.
        tasks: Vec<String>,
    },
    /// `task_specs` was given the wrong number of ids.
    #[error("expected {expected} task ids, got {got}")]
    TaskIdCount {
        /// Tasks in the plan.
        expected: usize,
        /// Ids given.
        got: usize,
    },
}

/// Pure validator for [`Plan`]s (`plan/05-orchestration.md` §3.4): size,
/// ids, kinds, references, acyclicity. Collects every error it finds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanValidator {
    max_tasks: usize,
    allowed_custom_kinds: Option<BTreeSet<String>>,
}

impl Default for PlanValidator {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_TASKS)
    }
}

impl PlanValidator {
    /// A validator allowing up to `max_tasks` tasks and any well-formed custom kind.
    #[must_use]
    pub const fn new(max_tasks: usize) -> Self {
        Self {
            max_tasks,
            allowed_custom_kinds: None,
        }
    }

    /// Restricts `custom_kind` values to this allow-list.
    #[must_use]
    pub fn with_allowed_custom_kinds<I, S>(mut self, kinds: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.allowed_custom_kinds = Some(kinds.into_iter().map(Into::into).collect());
        self
    }

    /// The configured maximum number of tasks.
    #[must_use]
    pub const fn max_tasks(&self) -> usize {
        self.max_tasks
    }

    /// Validates `plan`, returning every problem found (empty `Err` never happens).
    pub fn validate(&self, plan: &Plan) -> Result<(), Vec<PlanError>> {
        let mut errors = Vec::new();

        if plan.tasks.is_empty() {
            errors.push(PlanError::Empty);
        }
        if plan.tasks.len() > self.max_tasks {
            errors.push(PlanError::TooManyTasks {
                count: plan.tasks.len(),
                max: self.max_tasks,
            });
        }

        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for task in &plan.tasks {
            if !is_valid_task_id(&task.id) {
                errors.push(PlanError::InvalidTaskId {
                    id: task.id.clone(),
                });
            }
            if !seen.insert(task.id.as_str()) {
                errors.push(PlanError::DuplicateTaskId {
                    id: task.id.clone(),
                });
            }
            if task.title.trim().is_empty() {
                errors.push(PlanError::InvalidTitle {
                    task: task.id.clone(),
                    reason: "empty".to_owned(),
                });
            } else if task.title.chars().count() > MAX_TITLE_CHARS {
                errors.push(PlanError::InvalidTitle {
                    task: task.id.clone(),
                    reason: format!("longer than {MAX_TITLE_CHARS} characters"),
                });
            }
            if task.acceptance_criteria.is_empty() {
                errors.push(PlanError::NoAcceptanceCriteria {
                    task: task.id.clone(),
                });
            }
            match task.task_kind() {
                Ok(TaskKind::Custom(name)) => {
                    if let Some(allowed) = &self.allowed_custom_kinds
                        && !allowed.contains(&name)
                    {
                        errors.push(PlanError::InvalidCustomKind {
                            task: task.id.clone(),
                            name,
                        });
                    }
                }
                Ok(_) => {}
                Err(e) => errors.push(e),
            }
        }

        let ids: BTreeSet<&str> = plan.tasks.iter().map(|t| t.id.as_str()).collect();
        for task in &plan.tasks {
            for dep in &task.depends_on {
                if dep == &task.id {
                    errors.push(PlanError::SelfDependency {
                        task: task.id.clone(),
                    });
                } else if !ids.contains(dep.as_str()) {
                    errors.push(PlanError::DanglingDependency {
                        task: task.id.clone(),
                        depends_on: dep.clone(),
                    });
                }
            }
        }
        for edge in &plan.edges {
            if edge.from() == edge.to() {
                errors.push(PlanError::SelfDependency {
                    task: edge.to().to_owned(),
                });
            } else if !ids.contains(edge.from()) || !ids.contains(edge.to()) {
                errors.push(PlanError::DanglingEdge {
                    from: edge.from().to_owned(),
                    to: edge.to().to_owned(),
                });
            }
        }

        // Only look for cycles when references are sound (otherwise Kahn
        // would report the dangling reference again).
        let references_ok = !errors.iter().any(|e| {
            matches!(
                e,
                PlanError::DanglingDependency { .. }
                    | PlanError::DanglingEdge { .. }
                    | PlanError::SelfDependency { .. }
                    | PlanError::DuplicateTaskId { .. }
            )
        });
        if references_ok && let Err(e) = plan.topological_order() {
            errors.push(e);
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// `^t[0-9]{1,3}$`
fn is_valid_task_id(id: &str) -> bool {
    let Some(digits) = id.strip_prefix('t') else {
        return false;
    };
    (1..=3).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit())
}

impl fmt::Display for Plan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for task in &self.tasks {
            write!(f, "{} [{}] {}", task.id, task.kind, task.title)?;
            if !task.depends_on.is_empty() {
                write!(f, " (after {})", task.depends_on.join(", "))?;
            }
            writeln!(f)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn plan(tasks: Vec<PlanTask>) -> Plan {
        Plan::new(tasks, "because")
    }

    #[test]
    fn plan_round_trips_schema_shape() {
        let v = json!({
            "tasks": [
                {"id":"t1","title":"Implement","kind":"implement","instructions":"do it",
                 "acceptance_criteria":["works"],"depends_on":[]},
                {"id":"t2","title":"Test","kind":"custom","custom_kind":"bench","instructions":"test it",
                 "acceptance_criteria":["green"],"depends_on":["t1"],"inputs":["diff:t1"],
                 "suggested_tier":"fast","parallel_safe":false,"workspace_policy":"shared",
                 "optional":true,"allow_push":false,"output_schema":{"type":"object"}}
            ],
            "edges": [["t1","t2"]],
            "rationale": "because"
        });
        let p: Plan = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(
            p.tasks[1].task_kind().unwrap(),
            TaskKind::Custom("bench".into())
        );
        assert_eq!(p.tasks[0].task_kind().unwrap(), TaskKind::Implement);
        assert!(p.tasks[0].parallel_safe);
        let mut expected = v.clone();
        // Schema defaults are materialised on output.
        expected["tasks"][0]["parallel_safe"] = json!(true);
        expected["tasks"][0]["workspace_policy"] = json!("isolated");
        expected["tasks"][0]["optional"] = json!(false);
        expected["tasks"][0]["allow_push"] = json!(false);
        assert_eq!(serde_json::to_value(&p).unwrap(), expected);
        let back: Plan = serde_json::from_value(serde_json::to_value(&p).unwrap()).unwrap();
        assert_eq!(back, p);
        assert!(
            serde_json::from_value::<Plan>(json!({"tasks":[],"rationale":"x","nope":1})).is_err()
        );
        PlanValidator::default().validate(&p).unwrap();
        assert_eq!(p.dependencies_of("t2"), vec!["t1"]);
    }

    #[test]
    fn validator_accepts_a_dag_and_orders_it() {
        let p = plan(vec![
            PlanTask::new("t3", "test", "c").depends_on(["t1", "t2"]),
            PlanTask::new("t1", "research", "a"),
            PlanTask::new("t2", "implement", "b").depends_on(["t1"]),
        ]);
        PlanValidator::default().validate(&p).unwrap();
        let order: Vec<_> = p
            .topological_order()
            .unwrap()
            .iter()
            .map(|t| t.id.as_str())
            .collect();
        assert_eq!(order, vec!["t1", "t2", "t3"]);
    }

    #[test]
    fn validator_rejects_cycles() {
        let p = plan(vec![
            PlanTask::new("t1", "implement", "a").depends_on(["t2"]),
            PlanTask::new("t2", "test", "b").depends_on(["t1"]),
        ]);
        let errs = PlanValidator::default().validate(&p).unwrap_err();
        assert!(matches!(&errs[..], [PlanError::Cycle { tasks }] if tasks == &["t1", "t2"]));
        // cycle through an edge
        let mut p = plan(vec![
            PlanTask::new("t1", "implement", "a"),
            PlanTask::new("t2", "test", "b").depends_on(["t1"]),
        ]);
        p.edges.push(PlanEdge::new("t2", "t1"));
        assert!(matches!(
            PlanValidator::default().validate(&p).unwrap_err()[..],
            [PlanError::Cycle { .. }]
        ));
        let mut p = plan(vec![PlanTask::new("t1", "implement", "a")]);
        p.tasks[0].depends_on.push("t1".into());
        assert!(matches!(
            PlanValidator::default().validate(&p).unwrap_err()[..],
            [PlanError::SelfDependency { .. }]
        ));
    }

    #[test]
    fn validator_rejects_unknown_kinds_and_missing_custom_kind() {
        let p = plan(vec![
            PlanTask::new("t1", "deploy", "a"),
            PlanTask::new("t2", "custom", "b"),
            PlanTask::new("t3", "understand", "c"),
        ]);
        let errs = PlanValidator::default().validate(&p).unwrap_err();
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
                    kind: "understand".into()
                },
            ]
        );
        let mut t = PlanTask::new("t1", "custom", "a");
        t.custom_kind = Some("Bad Name".into());
        assert!(matches!(
            PlanValidator::default()
                .validate(&plan(vec![t.clone()]))
                .unwrap_err()[..],
            [PlanError::InvalidCustomKind { .. }]
        ));
        t.custom_kind = Some("bench".into());
        PlanValidator::default()
            .validate(&plan(vec![t.clone()]))
            .unwrap();
        let restricted = PlanValidator::default().with_allowed_custom_kinds(["migrate"]);
        assert!(restricted.validate(&plan(vec![t])).is_err());
    }

    #[test]
    fn validator_rejects_too_many_tasks() {
        let tasks: Vec<_> = (1..=25)
            .map(|i| PlanTask::new(format!("t{i}"), "implement", format!("task {i}")))
            .collect();
        let errs = PlanValidator::default()
            .validate(&plan(tasks.clone()))
            .unwrap_err();
        assert_eq!(errs, vec![PlanError::TooManyTasks { count: 25, max: 24 }]);
        assert!(PlanValidator::new(25).validate(&plan(tasks)).is_ok());
        assert_eq!(
            PlanValidator::default()
                .validate(&plan(vec![]))
                .unwrap_err(),
            vec![PlanError::Empty]
        );
    }

    #[test]
    fn validator_rejects_dangling_deps_edges_and_duplicates() {
        let mut p = plan(vec![
            PlanTask::new("t1", "implement", "a").depends_on(["t9"]),
            PlanTask::new("t1", "test", "b"),
            PlanTask::new("x", "test", "c"),
        ]);
        p.edges.push(PlanEdge::new("t1", "t7"));
        let errs = PlanValidator::default().validate(&p).unwrap_err();
        assert!(errs.contains(&PlanError::DanglingDependency {
            task: "t1".into(),
            depends_on: "t9".into()
        }));
        assert!(errs.contains(&PlanError::DuplicateTaskId { id: "t1".into() }));
        assert!(errs.contains(&PlanError::InvalidTaskId { id: "x".into() }));
        assert!(errs.contains(&PlanError::DanglingEdge {
            from: "t1".into(),
            to: "t7".into()
        }));
        assert!(!errs.iter().any(|e| matches!(e, PlanError::Cycle { .. })));
    }

    #[test]
    fn validator_checks_title_and_criteria() {
        let mut t = PlanTask::new("t1", "implement", "x".repeat(121));
        t.acceptance_criteria.clear();
        let errs = PlanValidator::default()
            .validate(&plan(vec![t]))
            .unwrap_err();
        assert!(
            errs.iter()
                .any(|e| matches!(e, PlanError::InvalidTitle { .. }))
        );
        assert!(
            errs.iter()
                .any(|e| matches!(e, PlanError::NoAcceptanceCriteria { .. }))
        );
    }

    #[test]
    fn task_specs_map_dependencies_and_edges() {
        let mut p = plan(vec![
            PlanTask::new("t1", "implement", "a"),
            PlanTask::new("t2", "test", "b").depends_on(["t1"]),
            PlanTask::new("t3", "review", "c"),
        ]);
        p.edges.push(PlanEdge::new("t2", "t3"));
        let ids = [TaskId::new(), TaskId::new(), TaskId::new()];
        let specs = p.task_specs(&ids).unwrap();
        assert_eq!(specs.len(), 3);
        assert_eq!(specs[1].2.depends_on, vec![ids[0]]);
        assert_eq!(specs[2].2.depends_on, vec![ids[1]]);
        assert_eq!(specs[2].1, TaskKind::Review);
        assert!(matches!(
            p.task_specs(&ids[..2]),
            Err(PlanError::TaskIdCount { .. })
        ));
        assert!(p.to_string().contains("t2 [test] b (after t1)"));
    }

    #[test]
    fn task_id_pattern() {
        assert!(is_valid_task_id("t1"));
        assert!(is_valid_task_id("t999"));
        assert!(!is_valid_task_id("t"));
        assert!(!is_valid_task_id("t1000"));
        assert!(!is_valid_task_id("T1"));
        assert!(!is_valid_task_id("1"));
    }
}
