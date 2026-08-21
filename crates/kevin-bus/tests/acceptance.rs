//! WS-04 acceptance tests for `kevin-bus` (plan/12 criteria 1–3).

use std::sync::Arc;
use std::time::Duration;

use kevin_bus::{
    BusMessage, EventBus, InProcBus, InProcBusConfig, PgNotifyBus, PgNotifyBusConfig,
    SubscriptionFilter,
};
use kevin_testkit::bus::{VecEventSource, run_event};
use uuid::Uuid;

const TIMEOUT: Duration = Duration::from_secs(5);

async fn next(stream: &mut kevin_bus::BusStream) -> BusMessage {
    tokio::time::timeout(TIMEOUT, stream.next())
        .await
        .expect("timed out waiting for a bus message")
        .expect("bus closed")
}

fn positions(messages: &[BusMessage]) -> Vec<u64> {
    messages
        .iter()
        .filter_map(|m| m.live().map(|e| e.position))
        .collect()
}

/// (1) A subscriber that disconnects resumes from its last position and sees
/// every later event exactly once, then keeps receiving live ones.
#[tokio::test]
async fn ac_ws04_1_subscriber_catches_up_from_position_after_reconnect() {
    let bus = InProcBus::with_defaults();
    let run = Uuid::now_v7();
    let mut first = bus.subscribe(SubscriptionFilter::for_run(run).named("sse"));

    bus.publish(&(1..=10).map(|n| run_event(run, n)).collect::<Vec<_>>())
        .await
        .unwrap();
    let mut last_seen = 0;
    for _ in 0..4 {
        let ev = next(&mut first).await.live().cloned().expect("live");
        last_seen = ev.position;
    }
    assert_eq!(last_seen, 4);
    drop(first); // "disconnect"

    // More events while nobody listens.
    bus.publish(&(11..=12).map(|n| run_event(run, n)).collect::<Vec<_>>())
        .await
        .unwrap();
    assert_eq!(bus.position(), 12);

    // Reconnect: resume after position 4 → 5..=12 replayed, then live 13.
    let mut resumed = bus.subscribe_from(last_seen, SubscriptionFilter::for_run(run).named("sse"));
    let mut got = Vec::new();
    for _ in 5..=12 {
        got.push(next(&mut resumed).await);
    }
    bus.publish(&[run_event(run, 13)]).await.unwrap();
    got.push(next(&mut resumed).await);
    assert_eq!(
        positions(&got),
        (5..=13).collect::<Vec<_>>(),
        "no gaps, no duplicates"
    );
    assert!(got.iter().all(|m| !m.is_lagged()), "{got:?}");
    for m in &got {
        assert_eq!(
            m.live().unwrap().aggregate_version,
            m.live().unwrap().position
        );
    }
}

/// (2) A slow subscriber on a bounded channel is told exactly what it missed
/// (no history) or healed from history — never silently skipped.
#[tokio::test]
async fn ac_ws04_2_lag_is_reported_never_silently_dropped() {
    let run = Uuid::now_v7();

    // Without history: the missed range surfaces as `Lagged{from,to}`.
    let bus = InProcBus::new(InProcBusConfig {
        capacity: 4,
        history: 0,
    });
    let mut slow = bus.subscribe(SubscriptionFilter::all().named("slow"));
    bus.publish(&(1..=20).map(|n| run_event(run, n)).collect::<Vec<_>>())
        .await
        .unwrap();
    let first = next(&mut slow).await;
    let BusMessage::Lagged { from, to } = first else {
        panic!("expected Lagged first, got {first:?}");
    };
    assert_eq!((from, to), (1, 16), "exactly the evicted range is reported");
    let mut rest = Vec::new();
    for _ in 17..=20 {
        rest.push(next(&mut slow).await);
    }
    assert_eq!(positions(&rest), vec![17, 18, 19, 20]);

    // With history: the lag is healed, every event is delivered in order.
    let bus = InProcBus::new(InProcBusConfig {
        capacity: 4,
        history: 64,
    });
    let mut slow = bus.subscribe(SubscriptionFilter::all().named("slow"));
    bus.publish(&(1..=20).map(|n| run_event(run, n)).collect::<Vec<_>>())
        .await
        .unwrap();
    let mut got = Vec::new();
    for _ in 1..=20 {
        got.push(next(&mut slow).await);
    }
    assert_eq!(positions(&got), (1..=20).collect::<Vec<_>>());
    assert!(got.iter().all(|m| !m.is_lagged()));

    // History smaller than the lag: the evicted prefix is reported, the rest healed.
    let bus = InProcBus::new(InProcBusConfig {
        capacity: 4,
        history: 10,
    });
    let mut slow = bus.subscribe(SubscriptionFilter::all().named("slow"));
    bus.publish(&(1..=20).map(|n| run_event(run, n)).collect::<Vec<_>>())
        .await
        .unwrap();
    let first = next(&mut slow).await;
    assert!(
        matches!(first, BusMessage::Lagged { from: 1, to: 10 }),
        "{first:?}"
    );
    let mut got = Vec::new();
    for _ in 11..=20 {
        got.push(next(&mut slow).await);
    }
    assert_eq!(positions(&got), (11..=20).collect::<Vec<_>>());
}

fn database_url() -> Option<String> {
    match std::env::var("DATABASE_URL") {
        Ok(url) if !url.trim().is_empty() => Some(url),
        _ => {
            eprintln!("SKIPPED: DATABASE_URL not set");
            None
        }
    }
}

/// (3) Two buses (two "processes") share one store: a publish on the first
/// wakes the second through pg NOTIFY and it reads the same events from the
/// store by position. Polling is disabled (long interval) to prove NOTIFY did it.
#[tokio::test]
async fn ac_ws04_3_pg_notify_wakes_second_listener_which_reads_same_events() {
    let Some(url) = database_url() else { return };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .unwrap();
    let source = Arc::new(VecEventSource::new());
    let run = Uuid::now_v7();
    // Pre-existing events are not replayed live.
    source.push(run_event(run, 1));

    let channel = format!("kevin_events_test_{}", Uuid::now_v7().simple());
    let cfg = PgNotifyBusConfig {
        channel: channel.clone(),
        poll_interval: Duration::from_secs(3600),
        ..PgNotifyBusConfig::default()
    };
    let process_a = PgNotifyBus::with_config(pool.clone(), source.clone(), cfg.clone())
        .await
        .unwrap();
    let process_b = PgNotifyBus::with_config(pool.clone(), source.clone(), cfg)
        .await
        .unwrap();
    assert_eq!(process_a.position(), 1);
    assert_eq!(process_b.position(), 1);

    let mut sub_b = process_b.subscribe(SubscriptionFilter::for_run(run).named("tui"));
    let mut sub_a = process_a.subscribe(SubscriptionFilter::all().named("projection:task_board"));

    // "Commit" to the shared store, then publish (wake-up) on process A.
    let events: Vec<_> = (2..=4).map(|n| run_event(run, n)).collect();
    source.push_all(events.iter().cloned());
    process_a.publish(&events).await.unwrap();

    let mut got_b = Vec::new();
    for _ in 2..=4 {
        got_b.push(next(&mut sub_b).await);
    }
    assert_eq!(positions(&got_b), vec![2, 3, 4]);
    let mut got_a = Vec::new();
    for _ in 2..=4 {
        got_a.push(next(&mut sub_a).await);
    }
    assert_eq!(positions(&got_a), vec![2, 3, 4]);
    assert_eq!(
        got_a
            .iter()
            .map(|m| m.live().unwrap().event_id)
            .collect::<Vec<_>>(),
        got_b
            .iter()
            .map(|m| m.live().unwrap().event_id)
            .collect::<Vec<_>>(),
        "both processes read the same stored events"
    );
    assert_eq!(process_b.position(), 4);

    // A raw NOTIFY from anywhere (e.g. the store's outbox relay) is enough.
    source.push(run_event(run, 5));
    sqlx::query("SELECT pg_notify($1, $2)")
        .bind(&channel)
        .bind("5")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(positions(&[next(&mut sub_b).await]), vec![5]);

    // Resume-from-position on the pg bus replays from the store, then goes live.
    let mut resumed = process_b.subscribe_from(2, SubscriptionFilter::all());
    let mut got = Vec::new();
    for _ in 3..=5 {
        got.push(next(&mut resumed).await);
    }
    source.push(run_event(run, 6));
    process_a.publish(&[]).await.unwrap();
    got.push(next(&mut resumed).await);
    assert_eq!(positions(&got), vec![3, 4, 5, 6]);
    assert!(got.iter().all(|m| !m.is_lagged()));
}

#[tokio::test]
async fn filter_selects_by_run_event_type_and_aggregate_type() {
    let bus = InProcBus::with_defaults();
    let run_a = Uuid::now_v7();
    let run_b = Uuid::now_v7();
    let mut only_a_started = bus.subscribe(
        SubscriptionFilter::for_run(run_a)
            .with_event_types(["run.started"])
            .with_aggregate_types(["run"]),
    );
    bus.publish(&[
        run_event(run_b, 1),
        run_event(run_a, 1),
        run_event(run_a, 2),
    ])
    .await
    .unwrap();
    bus.publish(&[run_event(run_a, 3), run_event(run_b, 2)])
        .await
        .unwrap();
    let ev = next(&mut only_a_started).await.live().cloned().unwrap();
    assert_eq!(ev.position, 2);
    assert_eq!(ev.correlation_id, run_a);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), only_a_started.next())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn subscribe_from_beyond_history_reports_evicted_range_then_replays() {
    let bus = InProcBus::new(InProcBusConfig {
        capacity: 8,
        history: 4,
    });
    let run = Uuid::now_v7();
    bus.publish(&(1..=10).map(|n| run_event(run, n)).collect::<Vec<_>>())
        .await
        .unwrap();
    let mut s = bus.subscribe_from(0, SubscriptionFilter::all());
    let first = next(&mut s).await;
    assert!(
        matches!(first, BusMessage::Lagged { from: 1, to: 6 }),
        "{first:?}"
    );
    let mut got = Vec::new();
    for _ in 7..=10 {
        got.push(next(&mut s).await);
    }
    assert_eq!(positions(&got), vec![7, 8, 9, 10]);
}

#[tokio::test]
async fn stream_ends_when_bus_is_dropped() {
    let bus = InProcBus::with_defaults();
    let mut s = bus.subscribe(SubscriptionFilter::all());
    drop(bus);
    assert!(
        tokio::time::timeout(TIMEOUT, s.next())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn pg_bus_reports_lagged_when_source_fails_during_heal() {
    let Some(url) = database_url() else { return };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .unwrap();
    let source = Arc::new(VecEventSource::new());
    let run = Uuid::now_v7();
    let cfg = PgNotifyBusConfig {
        channel: format!("kevin_events_test_{}", Uuid::now_v7().simple()),
        capacity: 4,
        poll_interval: Duration::from_secs(3600),
        notify_on_publish: false,
        ..PgNotifyBusConfig::default()
    };
    let bus = PgNotifyBus::with_config(pool, source.clone(), cfg)
        .await
        .unwrap();
    let mut slow = bus.subscribe(SubscriptionFilter::all().named("slow"));
    source.push_all((1..=12).map(|n| run_event(run, n)));
    bus.publish(&[]).await.unwrap();
    // Heal from the store works: all 12 delivered.
    let mut got = Vec::new();
    for _ in 1..=12 {
        got.push(next(&mut slow).await);
    }
    assert_eq!(positions(&got), (1..=12).collect::<Vec<_>>());

    // Now the store is unreachable while the subscriber lags: the gap is reported.
    source.push_all((13..=24).map(|n| run_event(run, n)));
    bus.publish(&[]).await.unwrap();
    source.set_failing(true);
    let first = next(&mut slow).await;
    assert!(
        matches!(first, BusMessage::Lagged { from: 13, to: 20 }),
        "{first:?}"
    );
    let mut got = Vec::new();
    for _ in 21..=24 {
        got.push(next(&mut slow).await);
    }
    assert_eq!(positions(&got), vec![21, 22, 23, 24]);

    // Dropping the last bus clone stops the pump and ends the stream.
    drop(bus);
    assert!(
        tokio::time::timeout(TIMEOUT, slow.next())
            .await
            .unwrap()
            .is_none()
    );
}
