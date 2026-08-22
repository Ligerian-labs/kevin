//! Harness for the WS-12 acceptance scenarios.
//!
//! Every scenario runs the real `kevin` binary against a per-test database
//! (`kevin_testkit::pg::TestDb`), a hermetic `HOME`/`XDG_CONFIG_HOME`, a plain
//! working directory (no repository: the runs pass `--allow-plain-dir`) and the
//! in-process `fake` worker driven by a YAML scenario. Nothing else on the
//! machine is touched and no coding-agent CLI is ever spawned.

#![allow(dead_code)]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use kevin_testkit::pg::TestDb;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt as _, BufReader};

/// How long a scenario waits for the CLI to finish.
pub const WAIT: Duration = Duration::from_secs(120);

/// The default scenario: the planner answers with a one-task plan and the task
/// succeeds. Rules match on the role name, which the role runner puts in the
/// worker prompt's title (`plan/04-workers.md` §Adapter: `fake`).
pub const SCENARIO: &str = r#"
default:
  reply: "done"
  usage: { input_tokens: 10, output_tokens: 5 }
rules:
  - match: "/^planner\\.understanding/"
    structured:
      objective: "Add a /healthz endpoint"
      assumptions: []
      risks: []
      success_criteria: ["GET /healthz returns 200"]
      proposed_questions: []
      complexity: "low"
      suggested_task_kinds: ["implement"]
  - match: "/^planner\\.plan/"
    structured:
      rationale: "one task is enough"
      tasks:
        - id: "t1"
          title: "Add the healthz route"
          kind: "implement"
          instructions: "add the route and a test"
          acceptance_criteria: ["GET /healthz returns 200"]
          depends_on: []
  - match: "/^integrator/"
    structured:
      status: "skipped"
      summary: "nothing to integrate (workspace.integration = none)"
      merged: []
      conflicts: []
      checks: []
      artifacts: []
  - match: "/^judge/"
    structured:
      scores:
        - { criterion: "correctness", score: 9, rationale: "the route returns 200" }
        - { criterion: "completeness", score: 8, rationale: "every criterion is met" }
        - { criterion: "quality", score: 8, rationale: "small and readable" }
        - { criterion: "safety", score: 10, rationale: "no destructive change" }
        - { criterion: "efficiency", score: 7, rationale: "one attempt" }
      overall: 0.85
      verdict: "accept"
      lessons: ["Health endpoints belong in their own module"]
      proposals: []
"#;

/// Same, but the understanding proposes a low-confidence question, so the
/// interactive run blocks on `question.asked`.
pub const SCENARIO_WITH_QUESTION: &str = r#"
default:
  reply: "done"
  usage: { input_tokens: 10, output_tokens: 5 }
rules:
  - match: "/^planner\\.understanding/"
    structured:
      objective: "Add a /healthz endpoint"
      assumptions: []
      risks: []
      success_criteria: ["GET /healthz returns 200"]
      complexity: "low"
      suggested_task_kinds: ["implement"]
      proposed_questions:
        - text: "Which database should /healthz probe?"
          why_it_matters: "the probe query differs"
          confidence_if_unasked: 0.1
          multi_select: false
          options:
            - label: "postgres"
              description: "the production store"
              recommended: true
            - label: "sqlite"
              description: "the test store"
  - match: "/^planner\\.plan/"
    structured:
      rationale: "one task is enough"
      tasks:
        - id: "t1"
          title: "Add the healthz route"
          kind: "implement"
          instructions: "add the route and a test"
          acceptance_criteria: ["GET /healthz returns 200"]
          depends_on: []
  - match: "/^integrator/"
    structured:
      status: "skipped"
      summary: "nothing to integrate"
      merged: []
      conflicts: []
      checks: []
      artifacts: []
  - match: "/^judge/"
    structured:
      scores:
        - { criterion: "correctness", score: 9, rationale: "the route returns 200" }
        - { criterion: "completeness", score: 8, rationale: "every criterion is met" }
        - { criterion: "quality", score: 8, rationale: "small and readable" }
        - { criterion: "safety", score: 10, rationale: "no destructive change" }
        - { criterion: "efficiency", score: 7, rationale: "one attempt" }
      overall: 0.85
      verdict: "accept"
      lessons: ["Health endpoints belong in their own module"]
      proposals: []
"#;

/// Same as [`SCENARIO`], but the implement attempt never finishes: the fake
/// worker holds until it is cancelled, which is what Ctrl-C must do.
pub const SCENARIO_HOLDING: &str = r#"
default:
  reply: "done"
  usage: { input_tokens: 10, output_tokens: 5 }
rules:
  - match: "/^planner\\.understanding/"
    structured:
      objective: "Add a /healthz endpoint"
      assumptions: []
      risks: []
      success_criteria: ["GET /healthz returns 200"]
      proposed_questions: []
      complexity: "low"
      suggested_task_kinds: ["implement"]
  - match: "/^planner\\.plan/"
    structured:
      rationale: "one task is enough"
      tasks:
        - id: "t1"
          title: "Hold the healthz route"
          kind: "implement"
          instructions: "wait to be cancelled"
          acceptance_criteria: ["GET /healthz returns 200"]
          depends_on: []
  - match: "/^Hold the healthz route/"
    hold: true
  - match: "/^integrator/"
    structured:
      status: "skipped"
      summary: "nothing to integrate"
      merged: []
      conflicts: []
      checks: []
      artifacts: []
  - match: "/^judge/"
    structured:
      scores:
        - { criterion: "correctness", score: 9, rationale: "the route returns 200" }
        - { criterion: "completeness", score: 8, rationale: "every criterion is met" }
        - { criterion: "quality", score: 8, rationale: "small and readable" }
        - { criterion: "safety", score: 10, rationale: "no destructive change" }
        - { criterion: "efficiency", score: 7, rationale: "one attempt" }
      overall: 0.85
      verdict: "accept"
      lessons: ["Health endpoints belong in their own module"]
      proposals: []
"#;

/// A booted scenario: database, hermetic home, config file and scenario file.
pub struct Harness {
    db: Option<TestDb>,
    tmp: TempDir,
    config: PathBuf,
    repo: PathBuf,
}

impl Harness {
    /// A harness with the default scenario.
    pub async fn new() -> Self {
        Self::with_scenario(SCENARIO).await
    }

    /// A harness whose fake worker runs `scenario` (YAML).
    pub async fn with_scenario(scenario: &str) -> Self {
        let db = TestDb::new().await;
        let url = db.url().to_owned();
        Self::build(Some(db), &url, scenario)
    }

    /// A harness with no database: for the argument-validation scenarios that
    /// must fail before anything connects.
    pub fn offline() -> Self {
        Self::build(None, "postgres://kevin:kevin@127.0.0.1:1/kevin", SCENARIO)
    }

    fn build(db: Option<TestDb>, url: &str, scenario: &str) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        for dir in ["home/.config", "data", "repo"] {
            std::fs::create_dir_all(root.join(dir)).expect("create dir");
        }
        let script = root.join("scenario.yaml");
        std::fs::write(&script, scenario).expect("write scenario");
        let config = root.join("kevin.toml");
        std::fs::write(&config, config_toml(root, url, &script)).expect("write config");
        Self {
            db,
            config,
            repo: root.join("repo"),
            tmp,
        }
    }

    /// The working directory runs happen in.
    pub fn repo(&self) -> &Path {
        &self.repo
    }

    /// A `kevin` command; `run` invocations get `--allow-plain-dir` appended
    /// because the working directory is deliberately not a repository.
    pub fn kevin(&self, args: &[&str]) -> assert_cmd::Command {
        let mut command = self.kevin_raw(args);
        if args.contains(&"run") {
            command.arg("--allow-plain-dir");
        }
        command
    }

    /// A `kevin` command with exactly `args`.
    pub fn kevin_raw(&self, args: &[&str]) -> assert_cmd::Command {
        let mut command = assert_cmd::Command::cargo_bin("kevin").expect("kevin binary is built");
        self.apply_env(&mut command);
        command.args(args);
        command.timeout(WAIT);
        command
    }

    fn apply_env(&self, command: &mut assert_cmd::Command) {
        let home = self.tmp.path().join("home");
        command
            .current_dir(&self.repo)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", self.tmp.path().join("data"))
            .env("USER", "tester")
            .env("KEVIN_CONFIG", &self.config)
            .env("NO_COLOR", "1");
    }

    /// Runs `args` and parses stdout as one JSON value.
    pub fn json(&self, args: &[&str]) -> serde_json::Value {
        let output = self
            .kevin(args)
            .assert()
            .code(0)
            .get_output()
            .stdout
            .clone();
        serde_json::from_slice(&output)
            .unwrap_or_else(|e| panic!("{args:?} did not print one JSON object: {e}"))
    }

    /// Spawns `kevin`, waits until the `marker` event is recorded, sends
    /// SIGINT and returns the exit code.
    pub async fn interrupt_after(&self, args: &[&str], marker: &str) -> Option<i32> {
        let bin = assert_cmd::cargo::cargo_bin("kevin");
        let mut command = tokio::process::Command::new(bin);
        let home = self.tmp.path().join("home");
        command
            .current_dir(&self.repo)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", self.tmp.path().join("data"))
            .env("USER", "tester")
            .env("KEVIN_CONFIG", &self.config)
            .env("NO_COLOR", "1")
            .args(args)
            .arg("--allow-plain-dir")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let mut child = command.spawn().expect("spawn kevin");
        let stdout = child.stdout.take().expect("piped stdout");

        // Drain stdout so the child never blocks on a full pipe.
        let drain = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            let mut collected = Vec::new();
            while let Ok(Some(line)) = lines.next_line().await {
                collected.push(line);
            }
            collected
        });

        // `core.events` is the observable both processes share.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while tokio::time::Instant::now() < deadline && !self.has_event(marker).await {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            self.has_event(marker).await,
            "{marker} never happened; cannot interrupt the run"
        );
        signal_interrupt(child.id().expect("child pid"));

        let status = tokio::time::timeout(Duration::from_secs(60), child.wait())
            .await
            .expect("kevin exits after SIGINT")
            .expect("wait");
        let _ = drain.await;
        status.code()
    }

    async fn has_event(&self, event_type: &str) -> bool {
        let Some(db) = &self.db else { return false };
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM core.events WHERE event_type = $1")
            .bind(event_type)
            .fetch_one(db.pool())
            .await
            .unwrap_or(0)
            > 0
    }

    /// Drops the per-test database.
    pub async fn close(self) {
        if let Some(db) = self.db {
            db.close().await;
        }
    }
}

impl std::fmt::Debug for Harness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Harness")
            .field("repo", &self.repo)
            .field("config", &self.config)
            .field("database", &self.db.is_some())
            .finish_non_exhaustive()
    }
}

/// `(event_type, payload)` of every event, in global order.
pub async fn run_events(harness: &Harness) -> Vec<(String, serde_json::Value)> {
    let db = harness.db.as_ref().expect("a database");
    sqlx::query_as::<_, (String, serde_json::Value)>(
        "SELECT event_type, payload FROM core.events ORDER BY position",
    )
    .fetch_all(db.pool())
    .await
    .expect("read core.events")
}

/// The `kind` of every task on the board.
pub async fn task_kinds(harness: &Harness) -> Vec<String> {
    let db = harness.db.as_ref().expect("a database");
    sqlx::query_scalar::<_, String>("SELECT kind FROM orch.task_board ORDER BY seq")
        .fetch_all(db.pool())
        .await
        .expect("read orch.task_board")
}

#[cfg(unix)]
fn signal_interrupt(pid: u32) {
    let status = std::process::Command::new("kill")
        .args(["-INT", &pid.to_string()])
        .status();
    assert!(
        status.is_ok_and(|s| s.success()),
        "could not SIGINT pid {pid}"
    );
}

#[cfg(not(unix))]
fn signal_interrupt(_pid: u32) {
    unimplemented!("the Ctrl-C scenario needs a unix signal");
}

fn config_toml(root: &Path, url: &str, script: &Path) -> String {
    let data_dir = root.join("data");
    let mut toml = format!(
        r#"
[kevin]
data_dir = "{data}"
instance_name = "kevin-ws12"
auto_approve_plans = false
shutdown_grace_period = "3s"

[database]
url = "{url}"
pool_size = 8
auto_migrate = true

[client]
server_url = ""

[budget]
default_run_usd = 100.0
default_task_usd = 50.0
default_run_wall = "120s"
default_task_wall = "60s"
max_attempts = 1
max_parallel_tasks = 4

[orchestrator]
question_default_timeout = "60s"
role_call_timeout = "30s"
evaluation_timeout = "60s"
progress_interval = "20ms"

[memory]
enabled = false

[evaluation]
enabled = true
evaluate_tasks = false
rubric = "default"
auto_apply = ["routing"]

[workspace]
strategy = "in_place"
cleanup = "never"
integration = "none"

[telemetry]
log_level = "error"

[workers.claude]
enabled = false
[workers.codex]
enabled = false
[workers.pi]
enabled = false
[workers.opencode]
enabled = false
[workers.fake]
enabled = true
script = "{script}"

[models.fake]
worker = "fake"
model = "fake"
tier = "balanced"
input_usd_per_m = 1.0
output_usd_per_m = 2.0

[roles]
planner = "fake"
clarifier = "fake"
judge = "fake"
integrator = "fake"
default = "fake"

[routing]
policy = "fixed"
"#,
        data = data_dir.display(),
        url = url,
        script = script.display(),
    );
    for kind in [
        "implement",
        "test",
        "review",
        "research",
        "write",
        "debug",
        "refactor",
        "ops",
    ] {
        let _ = write!(toml, "\n[routing.kinds.{kind}]\ncandidates = [\"fake\"]\n");
    }
    toml
}
