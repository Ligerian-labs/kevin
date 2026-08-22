//! Reading the planner's `kevin.plan.v1` document for the approval screen.
//!
//! `PlanDto` is deliberately opaque on the wire (`plan/07-api-and-tui.md`
//! §DTOs) so a planner-schema change is not a breaking API change. The
//! approval modal still has to draw the task DAG as an indented tree
//! (`plan/07` §Screens), so this module reads the fields it needs and tolerates
//! everything else: a document it cannot parse yields an empty [`PlanView`] and
//! the modal falls back to the raw JSON.

use kevin_api::dto::PlanDto;
use serde_json::Value;

/// One node of the plan tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanTask {
    /// Plan-local id (`t1`, `t2`…).
    pub id: String,
    /// One-line title.
    pub title: String,
    /// `implement`, `test`, … (`custom_kind` when the kind is `custom`).
    pub kind: String,
    /// `fast` | `balanced` | `frontier`, when the planner suggested one.
    pub suggested_tier: Option<String>,
    /// Whether the task may run beside its siblings (defaults to `true`).
    pub parallel_safe: bool,
    /// Whether the task is allowed to push to a remote. `plan/09-security.md`
    /// §Workspace isolation requires this to be **visible in the approval
    /// view**: it is the one plan field that widens the blast radius beyond
    /// the workspace, so an operator must not have to read the raw JSON to
    /// see it. Defaults to `false`.
    pub allow_push: bool,
    /// Ids this task waits for.
    pub depends_on: Vec<String>,
    /// How many acceptance criteria it carries.
    pub acceptance_criteria: Vec<String>,
    /// Depth in the rendered tree (0 = a root).
    pub depth: usize,
}

/// A parsed `kevin.plan.v1` document.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanView {
    /// Tasks in topological order, annotated with their tree depth.
    pub tasks: Vec<PlanTask>,
    /// Why the planner proposed this shape.
    pub rationale: String,
}

impl PlanView {
    /// Reads `plan`; an unparseable document yields an empty view.
    #[must_use]
    pub fn parse(plan: &PlanDto) -> Self {
        let Value::Object(root) = &plan.0 else {
            return Self::default();
        };
        let rationale = root
            .get("rationale")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let Some(items) = root.get("tasks").and_then(Value::as_array) else {
            return Self {
                tasks: Vec::new(),
                rationale,
            };
        };

        let mut tasks: Vec<PlanTask> = items.iter().filter_map(read_task).collect();
        // `edges` is an optional second way to express dependencies.
        if let Some(edges) = root.get("edges").and_then(Value::as_array) {
            for edge in edges {
                let Some([from, to]) = edge.as_array().and_then(|pair| match pair.as_slice() {
                    [from, to] => Some([from.as_str()?, to.as_str()?]),
                    _ => None,
                }) else {
                    continue;
                };
                if let Some(task) = tasks.iter_mut().find(|task| task.id == to)
                    && !task.depends_on.iter().any(|dep| dep == from)
                {
                    task.depends_on.push(from.to_owned());
                }
            }
        }

        Self {
            tasks: topological(tasks),
            rationale,
        }
    }

    /// Whether the document held no task at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    /// Titles of the tasks `task` waits for, in plan order.
    #[must_use]
    pub fn dependency_titles(&self, task: &PlanTask) -> Vec<&str> {
        task.depends_on
            .iter()
            .filter_map(|id| {
                self.tasks
                    .iter()
                    .find(|other| &other.id == id)
                    .map(|other| other.title.as_str())
            })
            .collect()
    }
}

fn read_task(value: &Value) -> Option<PlanTask> {
    let object = value.as_object()?;
    let id = object.get("id").and_then(Value::as_str)?.to_owned();
    let kind = match object.get("kind").and_then(Value::as_str) {
        Some("custom") => object
            .get("custom_kind")
            .and_then(Value::as_str)
            .unwrap_or("custom"),
        Some(kind) => kind,
        None => "custom",
    }
    .to_owned();
    Some(PlanTask {
        title: object
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or(&id)
            .to_owned(),
        kind,
        suggested_tier: object
            .get("suggested_tier")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        parallel_safe: object
            .get("parallel_safe")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        allow_push: object
            .get("allow_push")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        depends_on: object
            .get("depends_on")
            .and_then(Value::as_array)
            .map(|deps| {
                deps.iter()
                    .filter_map(|dep| dep.as_str().map(ToOwned::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
        acceptance_criteria: object
            .get("acceptance_criteria")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|c| c.as_str().map(ToOwned::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
        depth: 0,
        id,
    })
}

/// Kahn order with a stable tie-break on plan order; a cycle (which
/// `PlanValidator` rejects server-side) degrades to plan order so the modal
/// still shows something.
fn topological(tasks: Vec<PlanTask>) -> Vec<PlanTask> {
    let known: Vec<String> = tasks.iter().map(|task| task.id.clone()).collect();
    let mut remaining = tasks;
    let mut emitted: Vec<PlanTask> = Vec::with_capacity(remaining.len());
    let mut depths: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();

    while !remaining.is_empty() {
        let ready: Vec<usize> = remaining
            .iter()
            .enumerate()
            .filter(|(_, task)| {
                task.depends_on
                    .iter()
                    .filter(|dep| known.contains(dep))
                    .all(|dep| depths.contains_key(dep))
            })
            .map(|(index, _)| index)
            .collect();
        if ready.is_empty() {
            // Cycle or dangling dependency: emit the rest in plan order.
            for mut task in remaining {
                task.depth = 0;
                emitted.push(task);
            }
            break;
        }
        for index in ready.into_iter().rev() {
            let mut task = remaining.remove(index);
            task.depth = task
                .depends_on
                .iter()
                .filter_map(|dep| depths.get(dep))
                .max()
                .map_or(0, |depth| depth + 1);
            depths.insert(task.id.clone(), task.depth);
            emitted.push(task);
        }
        emitted.sort_by_key(|task| task.depth);
    }
    emitted
}

#[cfg(test)]
mod tests {
    use kevin_api::dto::PlanDto;
    use serde_json::json;

    use super::PlanView;

    fn plan() -> PlanDto {
        PlanDto(json!({
            "rationale": "split the change in two",
            "tasks": [
                { "id": "t2", "title": "Test /healthz", "kind": "test",
                  "depends_on": ["t1"], "acceptance_criteria": ["a test fails without the route"],
                  "suggested_tier": "fast", "parallel_safe": false },
                { "id": "t1", "title": "Add /healthz", "kind": "implement",
                  "depends_on": [], "acceptance_criteria": ["GET /healthz returns 200"],
                  "suggested_tier": "balanced" }
            ]
        }))
    }

    #[test]
    fn parses_tasks_in_topological_order_with_depths() {
        let view = PlanView::parse(&plan());
        assert_eq!(
            view.tasks
                .iter()
                .map(|t| (t.id.as_str(), t.depth))
                .collect::<Vec<_>>(),
            vec![("t1", 0), ("t2", 1)]
        );
        assert_eq!(view.rationale, "split the change in two");
        assert!(view.tasks[0].parallel_safe);
        assert!(!view.tasks[1].parallel_safe);
        assert_eq!(view.dependency_titles(&view.tasks[1]), vec!["Add /healthz"]);
    }

    #[test]
    fn an_unparseable_document_is_empty() {
        assert!(PlanView::parse(&PlanDto(serde_json::Value::Null)).is_empty());
    }

    #[test]
    fn a_cycle_degrades_to_plan_order_instead_of_looping() {
        let view = PlanView::parse(&PlanDto(json!({
            "tasks": [
                { "id": "t1", "title": "a", "kind": "implement", "depends_on": ["t2"] },
                { "id": "t2", "title": "b", "kind": "implement", "depends_on": ["t1"] }
            ]
        })));
        assert_eq!(view.tasks.len(), 2);
    }

    #[test]
    fn edges_add_dependencies() {
        let view = PlanView::parse(&PlanDto(json!({
            "tasks": [
                { "id": "t1", "title": "a", "kind": "implement" },
                { "id": "t2", "title": "b", "kind": "implement" }
            ],
            "edges": [["t1", "t2"]]
        })));
        assert_eq!(view.tasks[1].depends_on, vec!["t1".to_owned()]);
        assert_eq!(view.tasks[1].depth, 1);
    }
}
