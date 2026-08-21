//! The planner's two roles: understanding (`plan/05-orchestration.md` §3.2)
//! and planning (§3.4, including the revision loop).

use kevin_domain::plan::Plan;
use kevin_domain::{PlanValidator, TaskKind, Understanding};

use super::context::RoleContext;
use super::{Role, RoleError, RoleRequest, build_request, deserialize, extract, schemas, vars_of};

const UNDERSTANDING_SYSTEM: &str = include_str!("../../prompts/planner_understanding.system.md");
const UNDERSTANDING_USER: &str = include_str!("../../prompts/planner_understanding.user.md");
const PLAN_SYSTEM: &str = include_str!("../../prompts/planner_plan.system.md");
const PLAN_USER: &str = include_str!("../../prompts/planner_plan.user.md");

/// Understanding phase: goal + repo + memory → [`Understanding`]
/// (`kevin.understanding.v1`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlannerUnderstanding;

impl Role for PlannerUnderstanding {
    type Output = Understanding;

    fn name(&self) -> &'static str {
        "planner.understanding"
    }

    fn task_kind(&self) -> TaskKind {
        TaskKind::Understand
    }

    fn build(&self, ctx: &RoleContext) -> RoleRequest {
        build_request(
            UNDERSTANDING_SYSTEM,
            UNDERSTANDING_USER,
            vars_of(ctx),
            schemas::understanding().clone(),
            schemas::UNDERSTANDING_V1_ID,
        )
    }

    fn parse(&self, raw: &str) -> Result<Understanding, RoleError> {
        let role = self.name();
        let value = extract(role, raw, schemas::understanding())?;
        let understanding: Understanding = deserialize(role, value)?;
        understanding.validate().map_err(|err| RoleError::Invalid {
            role,
            subject: "understanding",
            message: err.to_string(),
        })?;
        Ok(understanding)
    }
}

/// Planning phase: understanding + answers → [`Plan`] (`kevin.plan.v1`),
/// validated with [`PlanValidator`] before it ever reaches the saga.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlannerPlan {
    validator: PlanValidator,
}

impl PlannerPlan {
    /// A planner bounded by `orchestrator.max_tasks_per_run`.
    #[must_use]
    pub fn new(max_tasks: usize) -> Self {
        Self {
            validator: PlanValidator::new(max_tasks),
        }
    }

    /// A planner using a pre-built validator (custom-kind allow-list).
    #[must_use]
    pub const fn with_validator(validator: PlanValidator) -> Self {
        Self { validator }
    }

    /// The validator plans are checked against.
    #[must_use]
    pub const fn validator(&self) -> &PlanValidator {
        &self.validator
    }
}

impl Role for PlannerPlan {
    type Output = Plan;

    fn name(&self) -> &'static str {
        "planner.plan"
    }

    fn task_kind(&self) -> TaskKind {
        TaskKind::Plan
    }

    fn build(&self, ctx: &RoleContext) -> RoleRequest {
        build_request(
            PLAN_SYSTEM,
            PLAN_USER,
            vars_of(ctx),
            schemas::plan_with_max_tasks(self.validator.max_tasks()),
            schemas::PLAN_V1_ID,
        )
    }

    fn parse(&self, raw: &str) -> Result<Plan, RoleError> {
        let role = self.name();
        let value = extract(role, raw, schemas::plan())?;
        let plan: Plan = deserialize(role, value)?;
        self.validator
            .validate(&plan)
            .map_err(|errors| RoleError::InvalidPlan { role, errors })?;
        Ok(plan)
    }
}

#[cfg(test)]
mod tests {
    use kevin_domain::plan::PlanTask;

    use super::*;

    fn plan_json() -> serde_json::Value {
        serde_json::json!({
            "rationale": "one task is enough",
            "tasks": [{
                "id": "t1", "title": "Add the route", "kind": "implement",
                "instructions": "add it", "acceptance_criteria": ["it returns 200"],
                "depends_on": []
            }]
        })
    }

    #[test]
    fn plan_role_validates_with_its_own_max_tasks() {
        let role = PlannerPlan::new(1);
        assert_eq!(
            role.build(&RoleContext::new("g")).schema.unwrap()["properties"]["tasks"]["maxItems"],
            1
        );
        role.parse(&plan_json().to_string()).unwrap();

        let mut two = plan_json();
        two["tasks"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "id": "t2", "title": "b", "kind": "test", "instructions": "i",
                "acceptance_criteria": ["c"], "depends_on": []
            }));
        let err = role.parse(&two.to_string()).unwrap_err();
        assert!(matches!(err, RoleError::InvalidPlan { .. }), "{err}");
    }

    #[test]
    fn understanding_role_rejects_a_document_the_domain_refuses() {
        // Schema-valid (one criterion, blank objective) but invalid for the domain.
        let raw = serde_json::json!({
            "objective": "   ",
            "assumptions": [], "risks": [], "success_criteria": ["x"],
            "proposed_questions": [], "complexity": "low", "suggested_task_kinds": []
        })
        .to_string();
        let err = PlannerUnderstanding.parse(&raw).unwrap_err();
        assert!(
            matches!(
                err,
                RoleError::Invalid {
                    subject: "understanding",
                    ..
                }
            ),
            "{err}"
        );
        assert!(!err.is_schema_violation());
    }

    #[test]
    fn a_domain_plan_round_trips_through_the_role() {
        let plan = Plan::new(
            vec![PlanTask::new("t1", "implement", "Add the route")],
            "because",
        );
        let raw = serde_json::to_string(&plan).unwrap();
        assert_eq!(PlannerPlan::default().parse(&raw).unwrap(), plan);
    }
}
