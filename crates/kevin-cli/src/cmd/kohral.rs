//! `kevin kohral …` — Kohral runtime contract tooling
//! (`plan/07-api-and-tui.md` §3, `plan/08-kohral-runtime.md` §8). Owned by WS-22.
//!
//! `kevin kohral conformance` runs **Kohral's own** `contract.py --runtime
//! hermes` — the script is never vendored, so the assertions Kevin is judged by
//! are always the ones Kohral currently ships. Two modes:
//!
//! - `--base-url <url> --token <t>`: run the phases against a gateway that is
//!   already up (the container built by WS-23, a staging deployment, …). The
//!   crash phases then need the operator to restart that gateway between
//!   `accept-crash` and `verify-crash`.
//! - no `--base-url`: boot a complete Kevin **in this process** in the
//!   conformance profile (fake worker, one `fake` alias, no repository) against
//!   `--database-url`, run every phase, and simulate the crash by killing and
//!   re-booting the engine over the same database — which is exactly what the
//!   `runtime_restarted` contract is about.
//!
//! The embedded mode writes to the `kohral` schema of the database it is given,
//! so point it at a scratch database, never at production.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::Args as _;
use kevin_kohral::conformance::{ContractScript, Gateway, PHASES, Phase, run_suite};
use kevin_store::{DatabaseCfg, Db};

use crate::cmd::config::load_from_ctx;
use crate::{Ctx, ExitError, exit};

/// Subcommand name.
pub const NAME: &str = "kohral";

/// Default token used by the embedded gateway.
const EMBEDDED_TOKEN: &str = "kevin-conformance-token";

/// How long `contract.py` waits for a turn to terminalise.
const STATE_TIMEOUT: Duration = Duration::from_secs(120);

/// Conformance phases of `contract.py`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum PhaseArg {
    /// Basic contract.
    Basic,
    /// Accept a run, then the harness crashes the gateway.
    AcceptCrash,
    /// Verify ledger state after the crash.
    VerifyCrash,
}

impl From<PhaseArg> for Phase {
    fn from(phase: PhaseArg) -> Self {
        match phase {
            PhaseArg::Basic => Phase::Basic,
            PhaseArg::AcceptCrash => Phase::AcceptCrash,
            PhaseArg::VerifyCrash => Phase::VerifyCrash,
        }
    }
}

/// Arguments of `kevin kohral`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// What to do.
    #[command(subcommand)]
    pub cmd: Cmd,
}

/// `kevin kohral` subcommands.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum Cmd {
    /// Run the Kohral conformance suite against a Kevin gateway.
    Conformance {
        /// Gateway base URL. Omit to boot an embedded gateway in this process.
        #[arg(long, value_name = "URL")]
        base_url: Option<String>,
        /// Kohral token (required with `--base-url`).
        #[arg(long, value_name = "TOKEN")]
        token: Option<String>,
        /// Path to Kohral's `contract.py` (default: `$KEVIN_KOHRAL_CONTRACT`,
        /// then `~/workspace/kohral/runtime/conformance/contract.py`).
        #[arg(long, value_name = "PATH")]
        script: Option<PathBuf>,
        /// Scratch database for the embedded gateway (default: `database.url`).
        #[arg(long, value_name = "URL")]
        database_url: Option<String>,
        /// Only this phase (default: all three, in order).
        #[arg(long, value_enum)]
        phase: Option<PhaseArg>,
    },
}

/// The `kevin kohral` command definition.
#[must_use]
pub fn command() -> clap::Command {
    Args::augment_args(clap::Command::new(NAME)).about("Kohral runtime contract tooling")
}

/// Runs `kevin kohral`.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    match args.cmd {
        Cmd::Conformance {
            base_url,
            token,
            script,
            database_url,
            phase,
        } => {
            conformance(
                ctx,
                base_url.as_deref(),
                token.as_deref(),
                script,
                database_url.as_deref(),
                phase,
            )
            .await
        }
    }
}

async fn conformance(
    ctx: &Ctx,
    base_url: Option<&str>,
    token: Option<&str>,
    script: Option<PathBuf>,
    database_url: Option<&str>,
    phase: Option<PhaseArg>,
) -> anyhow::Result<ExitCode> {
    let script = locate(script)?;
    let phases: Vec<Phase> = phase.map_or_else(|| PHASES.to_vec(), |phase| vec![phase.into()]);

    if let Some(base_url) = base_url {
        return against_remote(ctx, &script, base_url, token, &phases).await;
    }
    against_embedded(ctx, &script, database_url, &phases).await
}

fn locate(script: Option<PathBuf>) -> anyhow::Result<ContractScript> {
    if let Some(path) = script {
        if !path.is_file() {
            return Err(ExitError::new(
                exit::INVALID_ARGS,
                format!("no conformance script at {}", path.display()),
            )
            .into());
        }
        return Ok(ContractScript::at(path));
    }
    ContractScript::locate().ok_or_else(|| {
        ExitError::new(
            exit::INVALID_ARGS,
            format!(
                "Kohral's contract.py was not found; pass --script or set {} \
                 (it lives in the Kohral checkout at runtime/conformance/contract.py)",
                kevin_kohral::conformance::SCRIPT_ENV
            ),
        )
        .into()
    })
}

/// Phases against a gateway somebody else runs.
async fn against_remote(
    ctx: &Ctx,
    script: &ContractScript,
    base_url: &str,
    token: Option<&str>,
    phases: &[Phase],
) -> anyhow::Result<ExitCode> {
    let token = token.ok_or_else(|| {
        ExitError::new(
            exit::INVALID_ARGS,
            "--token is required with --base-url (the Kohral runtime token)",
        )
    })?;
    let run_id_file = std::env::temp_dir().join("kevin-kohral-conformance.run-id");
    let mut failed = false;
    for phase in phases {
        if *phase == Phase::VerifyCrash {
            eprintln!(
                "note: restart the gateway now — `verify-crash` asserts the turn \
                 accepted by `accept-crash` came back as failed/runtime_restarted"
            );
        }
        let report = script
            .run(
                *phase,
                base_url,
                token,
                Some(&run_id_file),
                STATE_TIMEOUT,
            )
            .await?;
        failed |= !report.success;
        report_phase(ctx, &report);
    }
    Ok(finish(failed))
}

/// Phases against a Kevin booted in this process.
async fn against_embedded(
    ctx: &Ctx,
    script: &ContractScript,
    database_url: Option<&str>,
    phases: &[Phase],
) -> anyhow::Result<ExitCode> {
    let url = if let Some(url) = database_url {
        url.to_owned()
    } else {
        let resolved = load_from_ctx(ctx).map_err(|errors| {
            ExitError::new(exit::INVALID_ARGS, format!("configuration: {errors}"))
        })?;
        DatabaseCfg::from_config(&resolved.config.database)
            .map_err(|error| ExitError::new(exit::INVALID_ARGS, error.to_string()))?
            .url
    };
    let cfg = DatabaseCfg {
        url,
        ..DatabaseCfg::default()
    };
    let pool = Db::connect_with(&cfg)
        .await
        .map_err(|error| ExitError::new(exit::UNREACHABLE, error.to_string()))?;

    let mut gateway = Gateway::start(pool, EMBEDDED_TOKEN).await?;
    if !ctx.global.quiet {
        eprintln!(
            "conformance gateway listening on {} (script: {})",
            gateway.base_url(),
            script.path().display()
        );
    }
    let result = run_suite(script, &mut gateway, phases).await;
    gateway.shutdown().await;

    match result {
        Ok(reports) => {
            for report in reports.values() {
                report_phase(ctx, report);
            }
            Ok(finish(false))
        }
        Err(error) => Err(ExitError::new(exit::FAILED, format!("{error:#}")).into()),
    }
}

fn report_phase(ctx: &Ctx, report: &kevin_kohral::conformance::PhaseReport) {
    if ctx.global.json {
        let body = serde_json::json!({
            "phase": report.phase.as_str(),
            "ok": report.success,
            "stdout": report.stdout,
            "stderr": report.stderr,
        });
        println!("{body}");
        return;
    }
    let verdict = if report.success { "ok" } else { "FAILED" };
    println!("{:<14} {verdict}", report.phase.as_str());
    if !report.success && !report.stderr.trim().is_empty() {
        eprintln!("{}", report.stderr.trim());
    }
}

fn finish(failed: bool) -> ExitCode {
    if failed {
        ExitCode::from(exit::FAILED)
    } else {
        ExitCode::from(exit::OK)
    }
}

#[cfg(test)]
mod tests {
    use super::{Args, PhaseArg, command, locate};
    use kevin_kohral::conformance::Phase;

    #[test]
    fn the_command_tree_is_well_formed() {
        command().debug_assert();
    }

    #[test]
    fn the_documented_flags_are_accepted() {
        use clap::FromArgMatches as _;
        let matches = command()
            .try_get_matches_from([
                "kohral",
                "conformance",
                "--base-url",
                "http://127.0.0.1:8080",
                "--token",
                "t",
                "--phase",
                "verify-crash",
            ])
            .expect("parses");
        let args = Args::from_arg_matches(&matches).expect("args");
        let super::Cmd::Conformance {
            base_url,
            token,
            phase,
            ..
        } = args.cmd;
        assert_eq!(base_url.as_deref(), Some("http://127.0.0.1:8080"));
        assert_eq!(token.as_deref(), Some("t"));
        assert_eq!(phase, Some(PhaseArg::VerifyCrash));
    }

    #[test]
    fn the_phase_names_match_contract_py() {
        assert_eq!(Phase::from(PhaseArg::Basic).as_str(), "basic");
        assert_eq!(Phase::from(PhaseArg::AcceptCrash).as_str(), "accept-crash");
        assert_eq!(Phase::from(PhaseArg::VerifyCrash).as_str(), "verify-crash");
    }

    #[test]
    fn a_missing_script_is_an_argument_error() {
        let error = locate(Some("/nonexistent/contract.py".into())).expect_err("must fail");
        assert!(error.to_string().contains("no conformance script"));
    }
}
