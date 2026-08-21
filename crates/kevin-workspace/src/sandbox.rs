//! Sandbox tier policy and the forbidden-flag table (`plan/09-security.md`
//! §Sandbox tiers).
//!
//! `sandbox.tier` is the **only** place that decides whether a dangerous worker
//! flag may be emitted. Every worker adapter calls
//! [`SandboxPolicy::check_argv`] on the *final* argv (including `extra_args`)
//! before spawning and maps an `Err` to `WorkerError::PolicyViolation`.

use std::fmt;

use kevin_domain::WorkerKind;
use serde::{Deserialize, Serialize};

use crate::config::{KevinConfig, NetworkPolicy, SandboxConfig, SandboxTier};

/// How a forbidden flag appears on a command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ForbiddenFlagShape {
    /// A bare flag: `--dangerously-skip-permissions` (also matched as `--flag=…`).
    Flag(&'static str),
    /// An option with a specific dangerous value: `--permission-mode bypassPermissions`
    /// (matched as two tokens or as `--option=value`; quotes around the value are ignored).
    OptionValue {
        /// The option token (`-s`, `--sandbox`, `--permission-mode`, `-c`).
        option: &'static str,
        /// The dangerous value.
        value: &'static str,
    },
}

/// One entry of [`FORBIDDEN_FLAGS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ForbiddenFlag {
    /// Worker the flag belongs to.
    pub worker: WorkerKind,
    /// Shape to match.
    pub shape: ForbiddenFlagShape,
}

impl ForbiddenFlag {
    const fn flag(worker: WorkerKind, flag: &'static str) -> Self {
        Self {
            worker,
            shape: ForbiddenFlagShape::Flag(flag),
        }
    }

    const fn option(worker: WorkerKind, option: &'static str, value: &'static str) -> Self {
        Self {
            worker,
            shape: ForbiddenFlagShape::OptionValue { option, value },
        }
    }

    /// Human form (`--permission-mode bypassPermissions`).
    #[must_use]
    pub fn render(&self) -> String {
        match self.shape {
            ForbiddenFlagShape::Flag(f) => f.to_owned(),
            ForbiddenFlagShape::OptionValue { option, value } => format!("{option} {value}"),
        }
    }

    /// Returns the first position in `argv` where this flag occurs.
    #[must_use]
    pub fn find_in(&self, argv: &[String]) -> Option<usize> {
        match self.shape {
            ForbiddenFlagShape::Flag(flag) => argv.iter().position(|a| {
                a == flag || a.strip_prefix(flag).is_some_and(|r| r.starts_with('='))
            }),
            ForbiddenFlagShape::OptionValue { option, value } => {
                argv.iter().enumerate().position(|(i, a)| {
                    if a == option {
                        argv.get(i + 1).is_some_and(|v| unquote(v) == value)
                    } else {
                        a.strip_prefix(option)
                            .and_then(|r| r.strip_prefix('='))
                            .is_some_and(|v| unquote(v) == value)
                    }
                })
            }
        }
    }
}

/// Drops shell-style quotes so `sandbox_mode="danger-full-access"` and
/// `'danger-full-access'` compare equal to the table value.
fn unquote(v: &str) -> String {
    v.replace(['"', '\''], "")
}

/// The single source of truth for flags that disable a worker's own sandbox
/// or approval flow. Allowed only when the tier permits dangerous flags.
pub const FORBIDDEN_FLAGS: &[ForbiddenFlag] = &[
    // claude
    ForbiddenFlag::flag(WorkerKind::Claude, "--dangerously-skip-permissions"),
    ForbiddenFlag::option(WorkerKind::Claude, "--permission-mode", "bypassPermissions"),
    // codex
    ForbiddenFlag::flag(
        WorkerKind::Codex,
        "--dangerously-bypass-approvals-and-sandbox",
    ),
    ForbiddenFlag::flag(WorkerKind::Codex, "--dangerously-bypass-hook-trust"),
    ForbiddenFlag::option(WorkerKind::Codex, "-s", "danger-full-access"),
    ForbiddenFlag::option(WorkerKind::Codex, "--sandbox", "danger-full-access"),
    ForbiddenFlag::option(WorkerKind::Codex, "-c", "sandbox_mode=danger-full-access"),
    ForbiddenFlag::option(
        WorkerKind::Codex,
        "--config",
        "sandbox_mode=danger-full-access",
    ),
    // opencode
    ForbiddenFlag::flag(WorkerKind::Opencode, "--auto"),
];

/// Forbidden flags of one worker kind.
pub fn forbidden_flags_for(kind: WorkerKind) -> impl Iterator<Item = &'static ForbiddenFlag> {
    FORBIDDEN_FLAGS.iter().filter(move |f| f.worker == kind)
}

/// A dangerous flag was found on a worker command line while the tier forbids it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error(
    "worker `{worker}` argv contains forbidden flag `{flag}` at position {position} under sandbox tier `{tier}`; set sandbox.tier = \"container\" (user config only) if the platform is the isolation boundary"
)]
pub struct PolicyViolation {
    /// Worker whose argv was checked.
    pub worker: WorkerKind,
    /// Effective tier.
    pub tier: SandboxTier,
    /// Rendered forbidden flag.
    pub flag: String,
    /// Index in argv.
    pub position: usize,
}

/// Pure table lookup: does `argv` for `kind` contain any forbidden flag?
/// Ignores the tier — use [`SandboxPolicy::check_argv`] for the policy decision.
pub fn find_forbidden(
    kind: WorkerKind,
    argv: &[String],
) -> Option<(&'static ForbiddenFlag, usize)> {
    forbidden_flags_for(kind)
        .filter_map(|f| f.find_in(argv).map(|pos| (f, pos)))
        .min_by_key(|(_, pos)| *pos)
}

/// Checks `argv` under the **default (`cli-native`) tier**; convenience for
/// adapters that have no policy at hand. Prefer [`SandboxPolicy::check_argv`].
pub fn check_argv(kind: WorkerKind, argv: &[String]) -> Result<(), PolicyViolation> {
    SandboxPolicy::default().check_argv(kind, argv)
}

/// The effective sandbox policy derived from `[sandbox]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxPolicy {
    /// The tier.
    pub tier: SandboxTier,
    /// Derived from the tier: `true` for `container` and `none`, `false` for `cli-native`.
    pub allow_dangerous_flags: bool,
    /// Network policy (honoured by the container tier only).
    pub network: NetworkPolicy,
}

impl Default for SandboxPolicy {
    fn default() -> Self {
        Self::from(&SandboxConfig::default())
    }
}

impl From<&SandboxConfig> for SandboxPolicy {
    /// `allow_dangerous_flags` is **derived** from the tier; the config field is
    /// not trusted (a project layer cannot relax the tier, `plan/09` rules).
    fn from(cfg: &SandboxConfig) -> Self {
        Self::for_tier(cfg.tier, cfg.network)
    }
}

impl From<&KevinConfig> for SandboxPolicy {
    fn from(cfg: &KevinConfig) -> Self {
        Self::from(&cfg.sandbox)
    }
}

impl From<SandboxConfig> for SandboxPolicy {
    fn from(cfg: SandboxConfig) -> Self {
        Self::from(&cfg)
    }
}

impl SandboxPolicy {
    /// Policy for a tier.
    #[must_use]
    pub const fn for_tier(tier: SandboxTier, network: NetworkPolicy) -> Self {
        Self {
            tier,
            allow_dangerous_flags: !matches!(tier, SandboxTier::CliNative),
            network,
        }
    }

    /// Checks the final argv of a worker subprocess against [`FORBIDDEN_FLAGS`].
    /// `Ok` when the tier allows dangerous flags or none is present.
    pub fn check_argv(&self, kind: WorkerKind, argv: &[String]) -> Result<(), PolicyViolation> {
        if self.allow_dangerous_flags {
            return Ok(());
        }
        match find_forbidden(kind, argv) {
            Some((flag, position)) => Err(PolicyViolation {
                worker: kind,
                tier: self.tier,
                flag: flag.render(),
                position,
            }),
            None => Ok(()),
        }
    }

    /// Alias of [`SandboxPolicy::check_argv`].
    pub fn check_args(&self, kind: WorkerKind, args: &[String]) -> Result<(), PolicyViolation> {
        self.check_argv(kind, args)
    }

    /// Checks a single proposed flag/option pair without building argv.
    pub fn check_flag(&self, kind: WorkerKind, flag: &str) -> Result<(), PolicyViolation> {
        let argv: Vec<String> = flag.split_whitespace().map(str::to_owned).collect();
        self.check_argv(kind, &argv)
    }

    /// `true` when the tier is `none` (callers log `kevin.sandbox.disabled`).
    #[must_use]
    pub const fn is_disabled(&self) -> bool {
        matches!(self.tier, SandboxTier::None)
    }
}

impl fmt::Display for SandboxPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "tier={} dangerous_flags={} network={:?}",
            self.tier, self.allow_dangerous_flags, self.network
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn table_covers_each_cli_worker() {
        for kind in [WorkerKind::Claude, WorkerKind::Codex, WorkerKind::Opencode] {
            assert!(forbidden_flags_for(kind).next().is_some(), "{kind}");
        }
        assert_eq!(forbidden_flags_for(WorkerKind::Pi).count(), 0);
        assert_eq!(forbidden_flags_for(WorkerKind::Fake).count(), 0);
    }

    #[test]
    fn cli_native_rejects_every_forbidden_shape() {
        let p = SandboxPolicy::default();
        assert_eq!(p.tier, SandboxTier::CliNative);
        assert!(!p.allow_dangerous_flags);
        let cases = [
            (
                WorkerKind::Claude,
                "claude -p --dangerously-skip-permissions",
            ),
            (
                WorkerKind::Claude,
                "claude -p --permission-mode bypassPermissions",
            ),
            (
                WorkerKind::Claude,
                "claude -p --permission-mode=bypassPermissions",
            ),
            (WorkerKind::Codex, "codex exec -s danger-full-access -"),
            (
                WorkerKind::Codex,
                "codex exec --sandbox=danger-full-access -",
            ),
            (
                WorkerKind::Codex,
                "codex exec --dangerously-bypass-approvals-and-sandbox",
            ),
            (
                WorkerKind::Codex,
                "codex exec --dangerously-bypass-hook-trust",
            ),
            (
                WorkerKind::Codex,
                "codex exec -c sandbox_mode=\"danger-full-access\"",
            ),
            (WorkerKind::Opencode, "opencode run --auto hello"),
        ];
        for (kind, line) in cases {
            let err = p.check_argv(kind, &argv(line)).expect_err(line);
            assert_eq!(err.worker, kind);
            assert_eq!(err.tier, SandboxTier::CliNative);
            assert!(err.to_string().contains("forbidden flag"), "{err}");
        }
    }

    #[test]
    fn cli_native_accepts_safe_argv() {
        let p = SandboxPolicy::default();
        assert!(
            p.check_argv(
                WorkerKind::Claude,
                &argv("claude -p --permission-mode acceptEdits")
            )
            .is_ok()
        );
        assert!(
            p.check_argv(WorkerKind::Codex, &argv("codex exec -s workspace-write -"))
                .is_ok()
        );
        assert!(
            p.check_argv(WorkerKind::Opencode, &argv("opencode run --format json hi"))
                .is_ok()
        );
        // A claude flag on a codex command line is not codex's flag.
        assert!(
            p.check_argv(
                WorkerKind::Codex,
                &argv("codex --dangerously-skip-permissions")
            )
            .is_ok()
        );
    }

    #[test]
    fn container_and_none_allow_dangerous_flags() {
        for tier in [SandboxTier::Container, SandboxTier::None] {
            let p = SandboxPolicy::from(&SandboxConfig {
                tier,
                ..SandboxConfig::default()
            });
            assert!(p.allow_dangerous_flags, "{tier}");
            assert!(
                p.check_argv(WorkerKind::Claude, &argv("--dangerously-skip-permissions"))
                    .is_ok()
            );
        }
        assert!(SandboxPolicy::for_tier(SandboxTier::None, NetworkPolicy::Inherit).is_disabled());
    }

    #[test]
    fn config_flag_cannot_relax_cli_native() {
        let p = SandboxPolicy::from(&SandboxConfig {
            tier: SandboxTier::CliNative,
            allow_dangerous_flags: true,
            ..SandboxConfig::default()
        });
        assert!(!p.allow_dangerous_flags);
        assert!(check_argv(WorkerKind::Opencode, &argv("--auto")).is_err());
    }

    #[test]
    fn violation_reports_first_position() {
        let err = SandboxPolicy::default()
            .check_argv(
                WorkerKind::Claude,
                &argv(
                    "claude -p --permission-mode bypassPermissions --dangerously-skip-permissions",
                ),
            )
            .unwrap_err();
        assert_eq!(err.position, 2);
        assert_eq!(err.flag, "--permission-mode bypassPermissions");
    }
}
