//! Minimal sandbox policy consumed by worker adapters (`plan/09-security.md`
//! §Sandbox tiers).
//!
//! The authoritative policy lives in `kevin-workspace` (WS-07:
//! `sandbox::{SandboxPolicy, FORBIDDEN_FLAGS, check_argv}`). Until that lands
//! this module carries a tiny equivalent with the same names so adapters can
//! code against it; the registry accepts it as a parameter and never derives
//! it from anything else.
//
// TODO(ws-07): replace by `kevin_workspace::SandboxPolicy` (type-alias swap).

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::worker::WorkerError;

/// `sandbox.tier`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxTier {
    /// Kevin runs on the operator's machine; workers' own sandboxes isolate.
    #[default]
    CliNative,
    /// Kevin itself runs inside a container; bypass flags allowed.
    Container,
    /// Explicit opt-out for local experimentation.
    None,
}

impl SandboxTier {
    /// Config form (`cli-native` | `container` | `none`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            SandboxTier::CliNative => "cli-native",
            SandboxTier::Container => "container",
            SandboxTier::None => "none",
        }
    }
}

impl fmt::Display for SandboxTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Flags/values that may only appear in a worker argv under the `container`
/// tier (`plan/09-security.md`). Checked against the *final* argv, including
/// `extra_args`, so no adapter can bypass it accidentally.
pub const FORBIDDEN_FLAGS: &[&str] = &[
    "--dangerously-skip-permissions",
    "bypassPermissions",
    "--dangerously-bypass-approvals-and-sandbox",
    "--dangerously-bypass-hook-trust",
    "danger-full-access",
    "--auto",
];

/// What the sandbox tier allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SandboxPolicy {
    /// Effective tier.
    pub tier: SandboxTier,
    /// Derived: `true` only when `tier = container` (or `none`).
    pub allow_dangerous_flags: bool,
}

impl Default for SandboxPolicy {
    fn default() -> Self {
        Self::cli_native()
    }
}

impl SandboxPolicy {
    /// Policy for a tier; `allow_dangerous_flags` is derived.
    #[must_use]
    pub const fn for_tier(tier: SandboxTier) -> Self {
        Self {
            tier,
            allow_dangerous_flags: matches!(tier, SandboxTier::Container | SandboxTier::None),
        }
    }

    /// The default tier.
    #[must_use]
    pub const fn cli_native() -> Self {
        Self::for_tier(SandboxTier::CliNative)
    }

    /// The container tier (Kohral image, CI container).
    #[must_use]
    pub const fn container() -> Self {
        Self::for_tier(SandboxTier::Container)
    }

    /// Whether bypass/dangerous flags may be emitted.
    #[must_use]
    pub const fn allows_dangerous_flags(&self) -> bool {
        self.allow_dangerous_flags
    }

    /// Rejects any forbidden flag in `argv` unless the tier allows them.
    pub fn check_argv<S: AsRef<str>>(&self, argv: &[S]) -> Result<(), WorkerError> {
        if self.allows_dangerous_flags() {
            return Ok(());
        }
        for arg in argv {
            let arg = arg.as_ref();
            if let Some(flag) = FORBIDDEN_FLAGS
                .iter()
                .find(|f| arg == **f || arg.split('=').any(|part| part == **f))
            {
                return Err(WorkerError::PolicyViolation {
                    flag: (*flag).to_owned(),
                    tier: self.tier.to_string(),
                });
            }
        }
        Ok(())
    }
}

impl From<&kevin_config::Sandbox> for SandboxPolicy {
    fn from(sandbox: &kevin_config::Sandbox) -> Self {
        let tier = match sandbox.tier {
            kevin_config::SandboxTier::CliNative => SandboxTier::CliNative,
            kevin_config::SandboxTier::Container => SandboxTier::Container,
            kevin_config::SandboxTier::None => SandboxTier::None,
        };
        Self {
            tier,
            allow_dangerous_flags: sandbox.allow_dangerous_flags
                || matches!(tier, SandboxTier::Container | SandboxTier::None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_native_rejects_each_forbidden_flag_container_allows() {
        let native = SandboxPolicy::cli_native();
        assert!(!native.allows_dangerous_flags());
        for flag in FORBIDDEN_FLAGS {
            let argv = ["claude", "-p", flag];
            let err = native.check_argv(&argv).unwrap_err();
            assert!(matches!(err, WorkerError::PolicyViolation { .. }), "{flag}");
            assert!(SandboxPolicy::container().check_argv(&argv).is_ok());
        }
        assert!(
            native
                .check_argv(&["--permission-mode", "acceptEdits"])
                .is_ok()
        );
        let err = native
            .check_argv(&["--permission-mode=bypassPermissions"])
            .unwrap_err();
        assert!(err.to_string().contains("bypassPermissions"));
        assert_eq!(SandboxTier::default().to_string(), "cli-native");
        assert_eq!(
            serde_json::to_string(&SandboxTier::CliNative).unwrap(),
            "\"cli-native\""
        );
    }
}
