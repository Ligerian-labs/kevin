//! Deterministic `Clock` and `IdGen` fakes (`plan/11-testing.md` §Determinism).

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, TimeDelta, TimeZone, Utc};
use kevin_domain::{Clock, IdGen};
use uuid::Uuid;

/// A clock that only moves when the test tells it to.
///
/// Starts at `2026-01-01T00:00:00Z` by default ([`FixedClock::default`]).
#[derive(Debug)]
pub struct FixedClock {
    now: Mutex<DateTime<Utc>>,
}

/// Alias used by the plan docs (`FakeClock`).
pub type FakeClock = FixedClock;

impl FixedClock {
    /// A clock frozen at `at`.
    #[must_use]
    pub fn new(at: DateTime<Utc>) -> Self {
        Self {
            now: Mutex::new(at),
        }
    }

    /// A clock frozen at the given UTC unix timestamp (seconds).
    #[must_use]
    pub fn at_unix(secs: i64) -> Self {
        Self::new(Utc.timestamp_opt(secs, 0).single().unwrap_or_else(|| {
            panic!("FixedClock::at_unix: {secs} is out of range for a UTC timestamp")
        }))
    }

    /// Moves the clock forward by `delta`.
    pub fn advance(&self, delta: TimeDelta) {
        let mut now = self.lock();
        *now += delta;
    }

    /// Moves the clock forward by `secs` seconds.
    pub fn advance_secs(&self, secs: i64) {
        self.advance(TimeDelta::seconds(secs));
    }

    /// Sets the clock to an absolute instant.
    pub fn set(&self, at: DateTime<Utc>) {
        *self.lock() = at;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, DateTime<Utc>> {
        self.now
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Default for FixedClock {
    fn default() -> Self {
        Self::new(
            Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
                .single()
                .unwrap_or_else(|| unreachable!("2026-01-01T00:00:00Z is a valid instant")),
        )
    }
}

impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        *self.lock()
    }
}

/// An [`IdGen`] that yields predictable, increasing, v7-shaped uuids:
/// `00000000-0001-7000-8000-000000000001`, `…-0002-7000-8000-…0002`, …
///
/// The counter is encoded both in the timestamp field (milliseconds) and the
/// random tail, so ids sort in generation order and read naturally in snapshots.
#[derive(Debug)]
pub struct SeqIdGen {
    next: AtomicU64,
}

impl SeqIdGen {
    /// A generator starting at 1.
    #[must_use]
    pub fn new() -> Self {
        Self::starting_at(1)
    }

    /// A generator whose first id encodes `first`.
    #[must_use]
    pub fn starting_at(first: u64) -> Self {
        Self {
            next: AtomicU64::new(first),
        }
    }

    /// The uuid that encodes counter value `n` (what the generator yields at step `n`).
    #[must_use]
    pub fn id_for(n: u64) -> Uuid {
        let mut tail = [0u8; 10];
        tail[2..].copy_from_slice(&n.to_be_bytes());
        uuid::Builder::from_unix_timestamp_millis(n, &tail).into_uuid()
    }

    /// The value the next call to [`IdGen::next_id`] will encode.
    #[must_use]
    pub fn peek(&self) -> u64 {
        self.next.load(Ordering::SeqCst)
    }
}

impl Default for SeqIdGen {
    fn default() -> Self {
        Self::new()
    }
}

impl IdGen for SeqIdGen {
    fn next_id(&self) -> Uuid {
        let n = self.next.fetch_add(1, Ordering::SeqCst);
        Self::id_for(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_clock_only_moves_on_demand() {
        let clock = FixedClock::default();
        let t0 = clock.now();
        assert_eq!(clock.now(), t0);
        clock.advance_secs(90);
        assert_eq!(clock.now(), t0 + TimeDelta::seconds(90));
        let later = Utc.with_ymd_and_hms(2030, 6, 1, 12, 0, 0).unwrap();
        clock.set(later);
        assert_eq!(clock.now(), later);
        assert_eq!(FixedClock::at_unix(0).now(), DateTime::UNIX_EPOCH);
    }

    #[test]
    fn seq_idgen_is_predictable_and_ordered() {
        let ids = SeqIdGen::new();
        let a = ids.next_id();
        let b = ids.next_id();
        assert_eq!(a.to_string(), "00000000-0001-7000-8000-000000000001");
        assert_eq!(b.to_string(), "00000000-0002-7000-8000-000000000002");
        assert!(a < b);
        assert_eq!(a.get_version(), Some(uuid::Version::SortRand));
        assert_eq!(ids.peek(), 3);
        assert_eq!(ids.run_id().as_uuid(), SeqIdGen::id_for(3));
        assert_eq!(SeqIdGen::starting_at(42).next_id(), SeqIdGen::id_for(42));
    }

    #[test]
    fn fakes_implement_the_domain_traits_as_objects() {
        let clock: std::sync::Arc<dyn Clock> = std::sync::Arc::new(FixedClock::default());
        let ids: std::sync::Arc<dyn IdGen> = std::sync::Arc::new(SeqIdGen::new());
        assert_eq!(clock.now().timestamp(), 1_767_225_600);
        assert!(!ids.task_id().is_nil());
    }
}
