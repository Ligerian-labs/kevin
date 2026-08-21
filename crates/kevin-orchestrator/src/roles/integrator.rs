//! The integrator: merges the succeeded task branches, runs the repository's
//! checks and reports what a human must look at
//! (`plan/05-orchestration.md` §3.6).

use kevin_domain::{ArtifactKind, TaskKind};
use kevin_worker::WorkspacePolicy;
use serde::{Deserialize, Serialize};

use super::context::RoleContext;
use super::{Role, RoleError, RoleRequest, build_request, deserialize, extract, schemas, vars_of};

const SYSTEM: &str = include_str!("../../prompts/integrator.system.md");
const USER: &str = include_str!("../../prompts/integrator.user.md");

/// How the integration ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationStatus {
    /// Everything that could land, landed.
    Integrated,
    /// Merge conflicts need a decision (`plan/05-orchestration.md` §3.6 spawns
    /// an `Integrate` task).
    Conflicts,
    /// Integration failed for another reason.
    Failed,
    /// Nothing to integrate (`workspace.integration = "none"`).
    Skipped,
}

/// A conflict the integrator refused to resolve on its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntegrationConflict {
    /// Branch that could not be merged.
    pub branch: String,
    /// Conflicting files.
    pub files: Vec<String>,
    /// What the two sides did.
    pub description: String,
}

/// One declared check run after the merge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntegrationCheck {
    /// The command as declared in `.kevin/kevin.toml [checks]`.
    pub command: String,
    /// Whether it passed.
    pub passed: bool,
    /// Tail of the output when it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_excerpt: Option<String>,
}

/// An artifact the integration produced (PR URL, branch, diff).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntegrationArtifact {
    /// What it is.
    pub kind: ArtifactKind,
    /// Where it lives.
    pub uri: String,
    /// One line for a human.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// The integrator's output (`kevin.integration.v1`); the saga turns it into
/// `MarkIntegrated{artifacts, summary}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntegrationReport {
    /// How it ended.
    pub status: IntegrationStatus,
    /// ≤ 600 characters recorded on the run.
    pub summary: String,
    /// Branches that landed, in merge order.
    pub merged: Vec<String>,
    /// Conflicts left for a human (or for an `Integrate` task).
    pub conflicts: Vec<IntegrationConflict>,
    /// Checks that were run.
    pub checks: Vec<IntegrationCheck>,
    /// PR URLs, branches, diffs.
    pub artifacts: Vec<IntegrationArtifact>,
    /// What still needs doing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub follow_up: Vec<String>,
}

impl IntegrationReport {
    /// `true` when everything landed.
    #[must_use]
    pub fn is_integrated(&self) -> bool {
        self.status == IntegrationStatus::Integrated
    }

    /// `true` when a conflict needs a decision.
    #[must_use]
    pub fn has_conflicts(&self) -> bool {
        self.status == IntegrationStatus::Conflicts || !self.conflicts.is_empty()
    }

    /// Checks that failed.
    #[must_use]
    pub fn failed_checks(&self) -> Vec<&IntegrationCheck> {
        self.checks.iter().filter(|c| !c.passed).collect()
    }
}

/// Integration instructions and report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Integrator;

impl Role for Integrator {
    type Output = IntegrationReport;

    fn name(&self) -> &'static str {
        "integrator"
    }

    fn task_kind(&self) -> TaskKind {
        TaskKind::Integrate
    }

    /// The integrator is the one role that writes: it merges branches in a
    /// fresh integration workspace.
    fn workspace_policy(&self) -> WorkspacePolicy {
        WorkspacePolicy::Isolated
    }

    fn build(&self, ctx: &RoleContext) -> RoleRequest {
        build_request(
            SYSTEM,
            USER,
            vars_of(ctx),
            schemas::integration().clone(),
            schemas::INTEGRATION_V1_ID,
        )
    }

    fn parse(&self, raw: &str) -> Result<IntegrationReport, RoleError> {
        let role = self.name();
        let value = extract(role, raw, schemas::integration())?;
        deserialize(role, value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_conflict_report_out_of_prose() {
        let raw = "I could not merge everything:\n```json\n{\
            \"status\": \"conflicts\", \"summary\": \"one conflict\", \
            \"merged\": [\"kevin/a\"], \
            \"conflicts\": [{\"branch\": \"kevin/b\", \"files\": [\"src/lib.rs\"], \
                             \"description\": \"both sides changed the router\"}], \
            \"checks\": [{\"command\": \"just ci\", \"passed\": false, \
                          \"output_excerpt\": \"error\"}], \
            \"artifacts\": []}\n```";
        let report = Integrator.parse(raw).unwrap();
        assert!(report.has_conflicts());
        assert!(!report.is_integrated());
        assert_eq!(report.failed_checks().len(), 1);
        assert_eq!(report.conflicts[0].files, vec!["src/lib.rs"]);
    }

    #[test]
    fn rejects_an_unknown_status() {
        let raw = "{\"status\": \"merged\", \"summary\": \"\", \"merged\": [], \
                   \"conflicts\": [], \"checks\": [], \"artifacts\": []}";
        assert!(Integrator.parse(raw).unwrap_err().is_schema_violation());
    }
}
