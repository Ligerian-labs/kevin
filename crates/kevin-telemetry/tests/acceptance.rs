//! WS-04 acceptance tests for `kevin-telemetry` (plan/12 criteria 4 and 5).

use std::time::Duration;

use kevin_telemetry::testing::MemoryWriter;
use kevin_telemetry::{LogFormat, TelemetryConfig, events, fields};
use tracing::Instrument;

fn json_cfg() -> TelemetryConfig {
    TelemetryConfig {
        log_format: LogFormat::Json,
        log_level: "debug".to_owned(),
        ..TelemetryConfig::default()
    }
}

/// (4) logs are JSON with `run_id` propagated through spawned tasks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac_ws04_4_json_logs_carry_run_id_through_spawned_tasks() {
    let writer = MemoryWriter::new();
    let subscriber = kevin_telemetry::build_subscriber(&json_cfg(), writer.clone()).unwrap();
    // Spawned tasks run on other runtime threads: like production, they rely on
    // the *global* subscriber (this is the only test in this binary installing one).
    tracing::subscriber::set_global_default(subscriber).unwrap();
    let run_id = "01910000-0000-7000-8000-0000000000aa";
    let task_id = "01910000-0000-7000-8000-0000000000bb";

    let span = tracing::info_span!(fields::span::RUN, { fields::RUN_ID } = run_id);
    let handle = tokio::spawn(
        async move {
            tracing::info!({ fields::EVENT } = events::run::STARTED, "run started");
            let inner = tracing::info_span!(fields::span::TASK, { fields::TASK_ID } = task_id);
            tokio::spawn(
                async {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    tracing::info!(
                        { fields::EVENT } = events::task::CREATED,
                        attempts = 1_u64,
                        "task created"
                    );
                }
                .instrument(inner),
            )
            .await
            .unwrap();
        }
        .instrument(span),
    );
    handle.await.unwrap();

    let records = writer.json_records();
    assert_eq!(records.len(), 2, "{}", writer.contents());
    let started = &records[0];
    assert_eq!(started["event"], events::run::STARTED);
    assert_eq!(started["run_id"], run_id);
    assert_eq!(started["level"], "info");
    assert_eq!(started["service"], "kevin");
    assert_eq!(started["message"], "run started");
    assert!(started["ts"].as_str().unwrap().ends_with('Z'));
    assert!(
        started.get("version").is_some()
            && started.get("instance").is_some()
            && started.get("profile").is_some()
    );

    let created = &records[1];
    assert_eq!(created["event"], events::task::CREATED);
    assert_eq!(
        created["run_id"], run_id,
        "outer span field must flow into the nested spawned task"
    );
    assert_eq!(created["task_id"], task_id);
    assert_eq!(created["attempts"], 1);
    assert_eq!(created["span"], fields::span::TASK);
}

/// (5) redaction layer masks a token in a log field (and in the message).
#[test]
fn ac_ws04_5_redaction_masks_token_in_log_field() {
    let writer = MemoryWriter::new();
    let subscriber = kevin_telemetry::build_subscriber(&json_cfg(), writer.clone()).unwrap();
    let token = "sk-ant-api03-SECRETSECRETSECRET0123";
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!("http", authorization = "Bearer abcdefghijklmnop.qrstuv");
        let _e = span.enter();
        tracing::warn!(
            { fields::EVENT } = events::api::AUTH_FAILED,
            api_key = token,
            db = "postgres://kevin:pa55word@localhost/kevin",
            "auth failed with key {token}"
        );
    });
    let output = writer.contents();
    assert!(!output.contains("SECRETSECRET"), "token leaked: {output}");
    assert!(
        !output.contains("abcdefghijklmnop"),
        "bearer leaked: {output}"
    );
    assert!(!output.contains("pa55word"), "db password leaked: {output}");
    let record = &writer.json_records()[0];
    assert_eq!(record["api_key"], "[REDACTED:anthropic_key]");
    assert_eq!(record["authorization"], "Bearer [REDACTED:bearer]");
    assert_eq!(
        record["db"],
        "postgres://kevin:[REDACTED:postgres_password]@localhost/kevin"
    );
    assert_eq!(
        record["message"],
        "auth failed with key [REDACTED:anthropic_key]"
    );
}

#[test]
fn pretty_format_is_single_line_with_event_and_fields() {
    let writer = MemoryWriter::new();
    let cfg = TelemetryConfig {
        log_format: LogFormat::Pretty,
        ..json_cfg()
    };
    let subscriber = kevin_telemetry::build_subscriber(&cfg, writer.clone()).unwrap();
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!("run", run_id = "r1");
        let _e = span.enter();
        tracing::info!(
            { fields::EVENT } = events::run::COMPLETED,
            cost = 1.5,
            "done"
        );
    });
    let lines = writer.lines();
    assert_eq!(lines.len(), 1);
    let line = &lines[0];
    assert!(
        line.contains(" info kevin.run.completed [run]: done run_id=\"r1\" cost=1.5"),
        "{line}"
    );
}

#[test]
fn oversized_fields_and_records_are_capped() {
    let writer = MemoryWriter::new();
    let subscriber = kevin_telemetry::build_subscriber(&json_cfg(), writer.clone()).unwrap();
    let big = "x".repeat(20_000);
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(blob = %big, "big");
    });
    let record = &writer.json_records()[0];
    let blob = record["blob"].as_str().unwrap();
    assert!(blob.len() < 9_000, "field not capped: {}", blob.len());
    assert!(
        blob.ends_with("…[truncated 11808 bytes]"),
        "{}",
        &blob[blob.len() - 40..]
    );
    assert!(writer.contents().len() <= 32 * 1024);
}

#[tokio::test]
async fn metrics_handle_renders_and_listener_serves() {
    let handle = kevin_telemetry::metrics::install().unwrap();
    metrics::counter!(kevin_telemetry::metrics::BUS_LAGGED_TOTAL, "subscriber" => "test")
        .increment(3);
    let body = handle.render();
    assert!(body.contains("kevin_bus_lagged_total"), "{body}");

    let (addr, task) = kevin_telemetry::serve_metrics("127.0.0.1:0".parse().unwrap(), handle)
        .await
        .unwrap();
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    tokio::io::AsyncWriteExt::write_all(&mut stream, b"GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    let mut response = String::new();
    tokio::io::AsyncReadExt::read_to_string(&mut stream, &mut response)
        .await
        .unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(response.contains("kevin_bus_lagged_total"), "{response}");
    task.abort();
}
