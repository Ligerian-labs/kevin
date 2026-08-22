//! WS-25 hardening — a [`PgNotifyBus`] must not wedge a shutdown.
//!
//! A `PgListener` holds one pool connection for as long as it lives, and
//! `PgPool::close()` waits for every connection to come back. A daemon that
//! closes its pool while the bus pump is still alive therefore hangs forever
//! — which is exactly what `kevin serve` did the first time it was switched
//! from `InProcBus` to `PgNotifyBus`. [`PgNotifyBus::shutdown`] is the release
//! valve, and this is its regression test.
//!
//! Skips cleanly without `DATABASE_URL`.

use std::sync::Arc;
use std::time::Duration;

use kevin_bus::{EventBus, PgNotifyBus, PgNotifyBusConfig, SubscriptionFilter};
use kevin_testkit::bus::{VecEventSource, run_event};
use uuid::Uuid;

fn database_url() -> Option<String> {
    match std::env::var("DATABASE_URL") {
        Ok(url) if !url.trim().is_empty() => Some(url),
        _ => {
            eprintln!("SKIPPED: DATABASE_URL not set");
            None
        }
    }
}

#[tokio::test]
async fn ac_ws25_5_3_shutdown_releases_the_listen_connection_so_the_pool_can_close() {
    let Some(url) = database_url() else { return };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("pool");
    let source = Arc::new(VecEventSource::new());
    let bus = PgNotifyBus::with_config(
        pool.clone(),
        Arc::clone(&source) as Arc<dyn kevin_bus::EventSource>,
        PgNotifyBusConfig {
            poll_interval: Duration::from_millis(50),
            ..PgNotifyBusConfig::default()
        },
    )
    .await
    .expect("pg bus");

    // A live subscriber, so the pump is genuinely running.
    let run = Uuid::now_v7();
    let _stream = bus.subscribe(SubscriptionFilter::for_run(run));
    source.push(run_event(run, 1));
    bus.publish(&[run_event(run, 1)]).await.expect("publish");

    // The bus is still shared — a daemon holds it in `AppState`, in the
    // projections and in the saga — so `Drop` cannot help here.
    let shared = bus.clone();
    bus.shutdown();
    // Idempotent, and safe from any clone.
    shared.shutdown();

    tokio::time::timeout(Duration::from_secs(10), pool.close())
        .await
        .expect("the pool closes once the listener is gone");
    assert!(pool.is_closed());
}
