//! The configuration this crate consumes: the `[workspace]`, `[sandbox]` and
//! `[checks]` sections of `plan/03-config-schema.md`, re-exported from
//! `kevin-config` under the names used throughout this crate.

pub use kevin_config::{
    Checks as ChecksConfig, Integration as IntegrationMode, KevinConfig, Sandbox as SandboxConfig,
    SandboxNetwork as NetworkPolicy, SandboxTier, WorkspaceCfg as WorkspaceConfig,
    WorkspaceCleanup as CleanupPolicy, WorkspaceStrategy as Strategy,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_plan_03() {
        let ws = WorkspaceConfig::default();
        assert_eq!(ws.strategy, Strategy::Auto);
        assert_eq!(ws.root, std::path::PathBuf::from(".kevin/workspaces"));
        assert_eq!(ws.branch_prefix, "kevin/");
        assert_eq!(ws.cleanup, CleanupPolicy::OnSuccess);
        assert_eq!(ws.integration, IntegrationMode::Pr);
        assert!(!ws.pr_per_task);
        let sb = SandboxConfig::default();
        assert_eq!(sb.tier, SandboxTier::CliNative);
        assert!(!sb.allow_dangerous_flags);
        assert_eq!(sb.network, NetworkPolicy::Inherit);
        assert!(sb.env_allowlist_extra.is_empty());
        assert!(ChecksConfig::default().commands.is_empty());
    }

    #[test]
    fn tier_serde_is_kebab_case() {
        assert_eq!(
            serde_json::to_string(&SandboxTier::CliNative).unwrap(),
            "\"cli-native\""
        );
        assert_eq!(
            serde_json::from_str::<SandboxTier>("\"container\"").unwrap(),
            SandboxTier::Container
        );
    }
}
