//! WS-25 security checklist — the `kevin-worker` rows of `plan/09-security.md`.
//!
//! Two findings from the checklist walk, each with the test that now proves
//! the fix:
//!
//! - **transcripts were not a redaction sink.** `plan/09` §Redaction lists
//!   "task logs/transcripts before persistence"; the supervisor wrote every
//!   line to `data_dir` verbatim, so a worker that ran `cat .env` left the
//!   credential on disk for as long as the artifacts were kept. Lines are now
//!   redacted and capped at `TRANSCRIPT_LINE_CAP_BYTES`, which was a declared
//!   constant nothing used.
//! - **the forbidden-flag matcher compared raw tokens.** `codex` accepts
//!   `-c sandbox_mode="danger-full-access"`, quotes included, which is exactly
//!   the flag the `cli-native` tier exists to forbid — and it did not match.
//!   Tokens are unquoted before comparison, and `sandbox.allow_dangerous_flags`
//!   no longer relaxes the tier (the tier is the single validated switch).

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use kevin_domain::WorkerKind;
use kevin_worker::supervisor::{ChildHandle, SpawnOpts, Supervisor};
use kevin_worker::{SandboxPolicy, WorkerError};

fn shim() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fake-cli"))
}

fn spawn(args: &[&str], opts: SpawnOpts) -> ChildHandle {
    let mut cmd = Supervisor::command(shim());
    cmd.args(args);
    Supervisor::spawn(cmd, opts).expect("spawn shim")
}

fn base(dir: &Path) -> SpawnOpts {
    SpawnOpts::new(WorkerKind::Fake, dir).env(BTreeMap::from([(
        "PATH".to_owned(),
        std::env::var("PATH").unwrap_or_default(),
    )]))
}

// ---------------------------------------------------------------------------
// Transcripts are a redaction sink
// ---------------------------------------------------------------------------

/// A worker that echoes a credential must not leave it in the transcript.
#[tokio::test]
async fn ac_ws25_6_2_transcript_lines_are_redacted_before_they_hit_the_disk() {
    let dir = tempfile::tempdir().unwrap();
    let transcript = dir.path().join("runs/r/t/a.jsonl");
    let secret = "sk-ant-api03-BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
    let leaked = format!("ANTHROPIC_API_KEY={secret}");

    let mut child = spawn(
        &["--stderr", &leaked],
        base(dir.path()).transcript(&transcript),
    );
    while child.next_line().await.is_some() {}
    let exit = child.wait().await;
    assert!(exit.success(), "{exit:?}");

    let body = std::fs::read_to_string(&transcript).unwrap();
    assert!(
        !body.contains(secret),
        "the credential survived into the transcript: {body}"
    );
    assert!(
        kevin_telemetry::redact::contains_marker(&body),
        "nothing was redacted at all: {body}"
    );
    // The artifact's digest and size describe what was actually written, or a
    // later integrity check would fail against the file on disk.
    let artifact = exit.transcript.expect("transcript artifact");
    assert_eq!(artifact.bytes, body.len() as u64);
    assert_eq!(
        artifact.sha256,
        kevin_worker::supervisor::sha256_hex(body.as_bytes())
    );
}

/// `TRANSCRIPT_LINE_CAP_BYTES` bounds a single transcript line, so one
/// pathological worker cannot fill `data_dir`.
#[tokio::test]
async fn ac_ws25_6_3_a_huge_transcript_line_is_truncated_with_a_marker() {
    let dir = tempfile::tempdir().unwrap();
    let transcript = dir.path().join("runs/r/t/big.jsonl");
    let cap = kevin_telemetry::redact::TRANSCRIPT_LINE_CAP_BYTES;

    // One line far past the cap. The supervisor's own reader cap applies
    // first; whatever survives it must still be bounded by the transcript cap.
    let mut child = spawn(
        &["--lines", "1", "--bytes", "200000"],
        base(dir.path())
            .transcript(&transcript)
            .max_line_bytes(4 * cap),
    );
    while child.next_line().await.is_some() {}
    let exit = child.wait().await;
    assert!(exit.success(), "{exit:?}");

    let body = std::fs::read_to_string(&transcript).unwrap();
    // `truncate` appends `…[truncated N bytes]`, so the bound is the cap plus
    // that marker — the point is that the line is bounded by a constant and
    // not by whatever the worker printed.
    let bound = cap + 64;
    let mut truncated = false;
    for line in body.lines() {
        assert!(
            line.len() <= bound,
            "a transcript line of {} bytes exceeds the {cap}-byte cap",
            line.len()
        );
        truncated |= line.contains("[truncated");
    }
    assert!(
        truncated,
        "a 200 KiB line must have been truncated somewhere: {} bytes written",
        body.len()
    );
}

// ---------------------------------------------------------------------------
// The sandbox tier cannot be talked around
// ---------------------------------------------------------------------------

/// Quoting a forbidden value used to slip past the matcher. `codex` reads
/// `-c sandbox_mode="danger-full-access"` exactly like the bare form.
#[test]
fn ac_ws25_5_2_a_quoted_forbidden_value_is_still_rejected() {
    let native = SandboxPolicy::cli_native();
    let quoted = [
        vec!["codex", "-c", "sandbox_mode=\"danger-full-access\""],
        vec!["codex", "-c", "sandbox_mode='danger-full-access'"],
        vec!["codex", "--sandbox", "\"danger-full-access\""],
        vec!["claude", "--permission-mode=\"bypassPermissions\""],
        vec!["opencode", "\"--auto\""],
    ];
    for argv in quoted {
        let err = native
            .check_argv(&argv)
            .unwrap_err_or_else_panic(&format!("{argv:?} must be rejected"));
        assert!(
            matches!(err, WorkerError::PolicyViolation { .. }),
            "{argv:?} produced {err:?}"
        );
        // The `container` tier is the documented escape hatch and still works.
        assert!(SandboxPolicy::container().check_argv(&argv).is_ok());
    }
}

/// Ordinary quoted arguments are untouched: unquoting must not turn a benign
/// argument into a violation.
#[test]
fn ac_ws25_5_2b_quoting_does_not_create_false_positives() {
    let native = SandboxPolicy::cli_native();
    for argv in [
        vec!["claude", "--permission-mode", "acceptEdits"],
        vec!["codex", "-s", "workspace-write"],
        vec!["codex", "-c", "sandbox_mode=\"read-only\""],
        vec!["opencode", "run", "--model", "\"anthropic/claude\""],
        vec!["claude", "-p", "please do not use --auto anywhere"],
    ] {
        assert!(native.check_argv(&argv).is_ok(), "{argv:?} was rejected");
    }
}

/// `sandbox.allow_dangerous_flags` is not a second switch: only the *tier*
/// decides. A project or user config that sets the flag under `cli-native`
/// must not gain the bypass flags.
#[test]
fn ac_ws25_5_4_the_config_flag_cannot_relax_the_cli_native_tier() {
    let sandbox = kevin_config::Sandbox {
        tier: kevin_config::SandboxTier::CliNative,
        allow_dangerous_flags: true,
        ..kevin_config::Sandbox::default()
    };
    let policy = SandboxPolicy::from(&sandbox);
    assert!(
        !policy.allows_dangerous_flags(),
        "the tier is the only switch (`plan/09` §Sandbox tiers)"
    );
    assert!(
        policy
            .check_argv(&["claude", "--dangerously-skip-permissions"])
            .is_err()
    );

    // The container tier still allows them, flag or no flag.
    let container = kevin_config::Sandbox {
        tier: kevin_config::SandboxTier::Container,
        allow_dangerous_flags: false,
        ..kevin_config::Sandbox::default()
    };
    assert!(SandboxPolicy::from(&container).allows_dangerous_flags());
}

/// `WorkerError` classifies itself, so a missing binary is permanent and a
/// full disk is transient (`ac_ws25_4_1` covers the orchestrator half).
#[test]
fn ac_ws25_4_2_worker_errors_carry_the_right_failure_class() {
    use kevin_domain::FailureClass;
    let missing = WorkerError::BinaryMissing {
        kind: WorkerKind::Claude,
        bin: "claude".to_owned(),
    };
    assert_eq!(missing.failure_class(), FailureClass::Permanent);
    let policy = WorkerError::PolicyViolation {
        flag: "--auto".to_owned(),
        tier: "cli-native".to_owned(),
    };
    assert_eq!(policy.failure_class(), FailureClass::Permanent);
    let io = WorkerError::io("creating /nope", std::io::Error::other("full"));
    assert_eq!(io.failure_class(), FailureClass::Transient);
}

/// `Result::unwrap_err` with a message, so a passing-by-accident case reads
/// clearly in the failure output.
trait UnwrapErrOrPanic<T, E> {
    fn unwrap_err_or_else_panic(self, message: &str) -> E;
}

impl<T: std::fmt::Debug, E> UnwrapErrOrPanic<T, E> for Result<T, E> {
    fn unwrap_err_or_else_panic(self, message: &str) -> E {
        match self {
            Ok(value) => panic!("{message}, got Ok({value:?})"),
            Err(err) => err,
        }
    }
}
