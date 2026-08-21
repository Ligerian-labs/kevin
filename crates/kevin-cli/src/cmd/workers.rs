//! `kevin workers …` — worker adapters (`plan/04-workers.md` §Registry and doctor).
//! Owned by WS-05.
//!
//! - `kevin workers doctor` — one row per configured worker: kind, enabled,
//!   binary path, version, auth status, models/notes. Exits 1 if any *enabled*
//!   worker is unhealthy (missing binary or missing auth). Never panics when
//!   binaries are missing.
//! - `kevin workers ls` — configured workers and the aliases they serve.

use std::process::ExitCode;

use clap::Args as _;
use kevin_domain::WorkerKind;
use kevin_worker::registry::{RegistryConfig, WorkerRegistry};
use kevin_worker::{AuthStatus, Doctor, SandboxPolicy};

use crate::{Ctx, ExitError, exit};

/// Subcommand name.
pub const NAME: &str = "workers";

/// Arguments of `kevin workers`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// What to do.
    #[command(subcommand)]
    pub cmd: Cmd,
}

/// `kevin workers` subcommands.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum Cmd {
    /// Check every configured worker CLI (binary, version, auth); exit 1 if an enabled one is unhealthy.
    Doctor,
    /// List configured workers and the model aliases they serve.
    Ls,
}

/// The `kevin workers` command definition.
#[must_use]
pub fn command() -> clap::Command {
    Args::augment_args(clap::Command::new(NAME))
        .about("Worker adapters (claude, codex, pi, opencode, fake)")
}

/// Runs `kevin workers`.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let resolved = crate::cmd::config::load_from_ctx(ctx)
        .map_err(|errs| ExitError::new(exit::INVALID_ARGS, errs.to_string()))?;
    let cfg = RegistryConfig::from(&resolved.config);
    let registry = WorkerRegistry::from_config(&cfg, SandboxPolicy::from(&resolved.config.sandbox))
        .map_err(|errs| ExitError::new(exit::INVALID_ARGS, errs.to_string()))?;
    match args.cmd {
        Cmd::Doctor => doctor(&registry, ctx.global.json).await,
        Cmd::Ls => {
            ls(&registry, ctx.global.json);
            Ok(ExitCode::SUCCESS)
        }
    }
}

async fn doctor(registry: &WorkerRegistry, json: bool) -> anyhow::Result<ExitCode> {
    let cfg = registry.config();
    let doctors = registry.doctor_all().await;
    let mut rows: Vec<Row> = doctors
        .iter()
        .map(|d| Row::from_doctor(d, true, cfg))
        .collect();
    for kind in WorkerKind::ALL {
        if !cfg.worker(kind).enabled {
            rows.push(Row::disabled(kind, cfg));
        }
    }
    let unhealthy: Vec<&Doctor> = doctors.iter().filter(|d| !d.is_healthy()).collect();
    if json {
        let payload = serde_json::json!({
            "workers": rows.iter().map(Row::to_json).collect::<Vec<_>>(),
            "healthy": unhealthy.is_empty(),
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        print_table(
            &["KIND", "ENABLED", "BINARY", "VERSION", "AUTH", "MODELS / NOTES"],
            &rows.iter().map(Row::cells).collect::<Vec<_>>(),
        );
        if !unhealthy.is_empty() {
            eprintln!(
                "error: {} enabled worker(s) unhealthy: {}",
                unhealthy.len(),
                unhealthy
                    .iter()
                    .map(|d| d.kind.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
    Ok(if unhealthy.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(exit::FAILED)
    })
}

fn ls(registry: &WorkerRegistry, json: bool) {
    let cfg = registry.config();
    let rows: Vec<Row> = WorkerKind::ALL
        .into_iter()
        .map(|kind| {
            let w = cfg.worker(kind);
            Row {
                kind,
                enabled: w.enabled,
                binary: if kind == WorkerKind::Fake {
                    "(in-process)".to_owned()
                } else {
                    w.bin.clone()
                },
                version: String::new(),
                auth: String::new(),
                notes: models_for(kind, cfg),
            }
        })
        .collect();
    if json {
        let payload = serde_json::json!({
            "workers": rows.iter().map(Row::to_json).collect::<Vec<_>>(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );
    } else {
        print_table(
            &["KIND", "ENABLED", "BINARY", "MODELS"],
            &rows
                .iter()
                .map(|r| {
                    vec![
                        r.kind.to_string(),
                        yes_no(r.enabled),
                        r.binary.clone(),
                        r.notes.clone(),
                    ]
                })
                .collect::<Vec<_>>(),
        );
    }
}

struct Row {
    kind: WorkerKind,
    enabled: bool,
    binary: String,
    version: String,
    auth: String,
    notes: String,
}

impl Row {
    fn from_doctor(d: &Doctor, enabled: bool, cfg: &RegistryConfig) -> Self {
        let mut notes = Vec::new();
        if d.binary.is_some() {
            let models = models_for(d.kind, cfg);
            if !models.is_empty() {
                notes.push(format!("models: {models}"));
            }
        }
        notes.extend(d.notes.iter().cloned());
        Self {
            kind: d.kind,
            enabled,
            binary: d
                .binary
                .as_ref()
                .map_or_else(|| "missing".to_owned(), |p| p.display().to_string()),
            version: d.version.clone().unwrap_or_else(|| "-".to_owned()),
            auth: match &d.auth_ready {
                AuthStatus::Ready => "ready".to_owned(),
                AuthStatus::Missing(hint) => format!("missing ({hint})"),
                AuthStatus::Unknown => "unknown".to_owned(),
            },
            notes: notes.join("; "),
        }
    }

    fn disabled(kind: WorkerKind, cfg: &RegistryConfig) -> Self {
        Self {
            kind,
            enabled: false,
            binary: cfg.worker(kind).bin,
            version: "-".to_owned(),
            auth: "-".to_owned(),
            notes: "disabled".to_owned(),
        }
    }

    fn cells(&self) -> Vec<String> {
        vec![
            self.kind.to_string(),
            yes_no(self.enabled),
            self.binary.clone(),
            self.version.clone(),
            self.auth.clone(),
            self.notes.clone(),
        ]
    }

    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "kind": self.kind,
            "enabled": self.enabled,
            "binary": self.binary,
            "version": self.version,
            "auth": self.auth,
            "notes": self.notes,
        })
    }
}

fn models_for(kind: WorkerKind, cfg: &RegistryConfig) -> String {
    cfg.aliases_for(kind)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn yes_no(b: bool) -> String {
    if b { "yes" } else { "no" }.to_owned()
}

fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(cell.chars().count());
            }
        }
    }
    let render = |cells: Vec<String>| {
        let last = cells.len().saturating_sub(1);
        cells
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if i == last {
                    c.clone()
                } else {
                    format!("{c:<width$}", width = widths[i])
                }
            })
            .collect::<Vec<_>>()
            .join("  ")
            .trim_end()
            .to_owned()
    };
    println!(
        "{}",
        render(headers.iter().map(|h| (*h).to_owned()).collect())
    );
    for row in rows {
        println!("{}", render(row.clone()));
    }
}
