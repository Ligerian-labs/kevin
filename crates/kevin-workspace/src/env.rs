//! Worker environment allow-list (`plan/09-security.md` §Environment and
//! secrets, `plan/04-workers.md` §Subprocess supervisor).
//!
//! Workers receive **only** `workers.<kind>.env_passthrough` ∪
//! `sandbox.env_allowlist_extra` from Kevin's own environment, plus the
//! Kevin-set variables `KEVIN_RUN_ID`, `KEVIN_TASK_ID`, `KEVIN_ATTEMPT_ID`,
//! `KEVIN_WORKSPACE`. Nothing else is inherited; the supervisor does
//! `env_clear()` then applies [`EnvAllowlist::vars`].

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use kevin_domain::{AttemptId, RunId, TaskId};
use serde::{Deserialize, Serialize};

/// Name of the variable carrying the run id.
pub const KEVIN_RUN_ID: &str = "KEVIN_RUN_ID";
/// Name of the variable carrying the task id.
pub const KEVIN_TASK_ID: &str = "KEVIN_TASK_ID";
/// Name of the variable carrying the attempt id.
pub const KEVIN_ATTEMPT_ID: &str = "KEVIN_ATTEMPT_ID";
/// Name of the variable carrying the workspace root (also the worker's cwd).
pub const KEVIN_WORKSPACE: &str = "KEVIN_WORKSPACE";

/// Which variable *names* a worker may inherit: `workers.<kind>.env_passthrough`
/// plus `sandbox.env_allowlist_extra`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EnvAllowlistSpec {
    /// `workers.<kind>.env_passthrough`.
    pub passthrough: Vec<String>,
    /// `sandbox.env_allowlist_extra`.
    pub extra: Vec<String>,
}

impl EnvAllowlistSpec {
    /// Builds the spec from the two config lists.
    pub fn new<I, J, S, T>(passthrough: I, extra: J) -> Self
    where
        I: IntoIterator<Item = S>,
        J: IntoIterator<Item = T>,
        S: Into<String>,
        T: Into<String>,
    {
        Self {
            passthrough: passthrough.into_iter().map(Into::into).collect(),
            extra: extra.into_iter().map(Into::into).collect(),
        }
    }

    /// The de-duplicated, sorted set of allowed names.
    #[must_use]
    pub fn names(&self) -> BTreeSet<&str> {
        self.passthrough
            .iter()
            .chain(self.extra.iter())
            .map(String::as_str)
            .filter(|n| !n.is_empty())
            .collect()
    }
}

/// The Kevin-set variables of one attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KevinEnv {
    /// `KEVIN_RUN_ID`.
    pub run_id: RunId,
    /// `KEVIN_TASK_ID`.
    pub task_id: TaskId,
    /// `KEVIN_ATTEMPT_ID`.
    pub attempt_id: AttemptId,
    /// `KEVIN_WORKSPACE` (the workspace root).
    pub workspace: PathBuf,
}

impl KevinEnv {
    /// The four variables as name/value pairs.
    #[must_use]
    pub fn vars(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            (KEVIN_RUN_ID.to_owned(), self.run_id.to_string()),
            (KEVIN_TASK_ID.to_owned(), self.task_id.to_string()),
            (KEVIN_ATTEMPT_ID.to_owned(), self.attempt_id.to_string()),
            (
                KEVIN_WORKSPACE.to_owned(),
                self.workspace.to_string_lossy().into_owned(),
            ),
        ])
    }
}

/// The computed environment of one worker process: allow-listed variables
/// that exist in Kevin's environment + the `KEVIN_*` variables.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EnvAllowlist {
    vars: BTreeMap<String, String>,
}

impl EnvAllowlist {
    /// Builds the worker environment from the **current process environment**:
    /// only names listed in `spec` are copied (missing ones are skipped, never
    /// invented), then the `KEVIN_*` variables from `extra` are set (they win
    /// over a same-named inherited variable).
    pub fn build(spec: &EnvAllowlistSpec, extra: &KevinEnv) -> Self {
        Self::build_from(spec, extra, std::env::vars_os())
    }

    /// Same as [`EnvAllowlist::build`] but reading from `source` instead of
    /// the process environment (tests, or a captured environment).
    pub fn build_from<I, K, V>(spec: &EnvAllowlistSpec, extra: &KevinEnv, source: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<std::ffi::OsString>,
        V: Into<std::ffi::OsString>,
    {
        let allowed = spec.names();
        let mut vars: BTreeMap<String, String> = source
            .into_iter()
            .filter_map(|(k, v)| {
                let k = k.into().into_string().ok()?;
                let v = v.into().into_string().ok()?;
                allowed.contains(k.as_str()).then_some((k, v))
            })
            .collect();
        vars.extend(extra.vars());
        Self { vars }
    }

    /// An allow-list holding exactly `vars` (tests, fake worker).
    #[must_use]
    pub fn from_vars(vars: BTreeMap<String, String>) -> Self {
        Self { vars }
    }

    /// The resulting variables (what `Command::envs` receives after `env_clear`).
    #[must_use]
    pub fn vars(&self) -> &BTreeMap<String, String> {
        &self.vars
    }

    /// Consumes into the variable map.
    #[must_use]
    pub fn into_vars(self) -> BTreeMap<String, String> {
        self.vars
    }

    /// Variable **names** only — safe to log (`plan/09`: "logged at startup as
    /// variable names only").
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.vars.keys().map(String::as_str)
    }

    /// Value of one variable.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.vars.get(name).map(String::as_str)
    }

    /// `true` when empty (never the case after `build`, which always sets `KEVIN_*`).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.vars.is_empty()
    }

    /// Number of variables.
    #[must_use]
    pub fn len(&self) -> usize {
        self.vars.len()
    }
}

impl IntoIterator for EnvAllowlist {
    type Item = (String, String);
    type IntoIter = std::collections::btree_map::IntoIter<String, String>;

    fn into_iter(self) -> Self::IntoIter {
        self.vars.into_iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kevin() -> KevinEnv {
        KevinEnv {
            run_id: RunId::nil(),
            task_id: TaskId::nil(),
            attempt_id: AttemptId::nil(),
            workspace: PathBuf::from("/repo/.kevin/workspaces/x/y"),
        }
    }

    #[test]
    fn only_allowlisted_names_are_copied() {
        let spec = EnvAllowlistSpec::new(["HOME", "PATH", "ANTHROPIC_API_KEY"], ["EXTRA_ONE"]);
        let source = [
            ("HOME", "/home/k"),
            ("PATH", "/bin"),
            ("AWS_SECRET_ACCESS_KEY", "nope"),
            ("EXTRA_ONE", "yes"),
            ("KEVIN_RUN_ID", "spoofed"),
        ];
        let env = EnvAllowlist::build_from(&spec, &kevin(), source);
        let names: Vec<&str> = env.names().collect();
        assert_eq!(
            names,
            [
                "EXTRA_ONE",
                "HOME",
                "KEVIN_ATTEMPT_ID",
                "KEVIN_RUN_ID",
                "KEVIN_TASK_ID",
                "KEVIN_WORKSPACE",
                "PATH",
            ]
        );
        assert_eq!(
            env.get("KEVIN_RUN_ID"),
            Some(RunId::nil().to_string().as_str())
        );
        assert_eq!(
            env.get("KEVIN_WORKSPACE"),
            Some("/repo/.kevin/workspaces/x/y")
        );
        assert_eq!(env.get("AWS_SECRET_ACCESS_KEY"), None);
        assert_eq!(
            env.get("ANTHROPIC_API_KEY"),
            None,
            "missing vars are not invented"
        );
    }

    #[test]
    fn build_reads_the_process_env() {
        // PATH exists in every test environment; a random name does not.
        let spec = EnvAllowlistSpec::new(
            ["PATH", "KEVIN_TEST_SURELY_UNSET_9f3a"],
            Vec::<String>::new(),
        );
        let env = EnvAllowlist::build(&spec, &kevin());
        assert!(env.get("PATH").is_some());
        assert!(env.get("KEVIN_TEST_SURELY_UNSET_9f3a").is_none());
        assert_eq!(env.len(), 5);
    }
}
