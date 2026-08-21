//! Bounded non-blocking log writer (`plan/10` §Logging).
//!
//! Records are handed to a dedicated thread through a bounded queue. In
//! `lossy` mode a full queue drops the record and counts it
//! (`kevin_telemetry_dropped_records_total`); in lossless mode the caller
//! waits. [`WorkerGuard`] flushes the queue on drop within a bounded time.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::JoinHandle;
use std::time::Duration;

/// Default queue capacity in records.
pub const DEFAULT_QUEUE_LINES: usize = 65_536;
/// How long [`WorkerGuard`] waits for the queue to flush on drop.
pub const FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

enum Msg {
    Line(Vec<u8>),
    Shutdown,
}

/// Counts records dropped by a lossy [`NonBlocking`] writer.
#[derive(Debug, Clone, Default)]
pub struct ErrorCounter {
    dropped: Arc<AtomicU64>,
}

impl ErrorCounter {
    /// Records dropped so far.
    #[must_use]
    pub fn dropped_lines(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// A `Write` handle that enqueues whole records for the worker thread.
#[derive(Debug, Clone)]
pub struct NonBlocking {
    tx: SyncSender<Msg>,
    lossy: bool,
    counter: ErrorCounter,
}

impl std::fmt::Debug for Msg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Msg::Line(l) => write!(f, "Line({} bytes)", l.len()),
            Msg::Shutdown => write!(f, "Shutdown"),
        }
    }
}

impl NonBlocking {
    /// Builds a writer over `sink` with `queue_lines` capacity.
    pub fn new<T: io::Write + Send + 'static>(
        sink: T,
        queue_lines: usize,
        lossy: bool,
        thread_name: &str,
    ) -> (Self, WorkerGuard) {
        let (tx, rx) = mpsc::sync_channel::<Msg>(queue_lines.max(1));
        let (done_tx, done_rx) = mpsc::sync_channel::<()>(1);
        let worker = std::thread::Builder::new()
            .name(thread_name.to_owned())
            .spawn(move || {
                run_worker(&rx, sink);
                let _ = done_tx.send(());
            })
            .ok();
        let writer = Self {
            tx: tx.clone(),
            lossy,
            counter: ErrorCounter::default(),
        };
        let guard = WorkerGuard {
            tx,
            done: done_rx,
            worker,
        };
        (writer, guard)
    }

    /// Dropped-record counter (only ever non-zero in lossy mode).
    #[must_use]
    pub fn error_counter(&self) -> ErrorCounter {
        self.counter.clone()
    }
}

fn run_worker<T: io::Write>(rx: &Receiver<Msg>, mut sink: T) {
    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Line(line) => {
                let _ = sink.write_all(&line);
            }
            Msg::Shutdown => break,
        }
    }
    let _ = sink.flush();
}

impl io::Write for NonBlocking {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let msg = Msg::Line(buf.to_vec());
        if self.lossy {
            match self.tx.try_send(msg) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    self.counter.dropped.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Disconnected(_)) => {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "log worker stopped",
                    ));
                }
            }
        } else if self.tx.send(msg).is_err() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "log worker stopped",
            ));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Keeps the worker thread alive; dropping it flushes pending records
/// (bounded by [`FLUSH_TIMEOUT`]).
#[derive(Debug)]
pub struct WorkerGuard {
    tx: SyncSender<Msg>,
    done: Receiver<()>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        // Queue the shutdown marker behind everything already enqueued, then
        // wait for the worker to reach it — both bounded by FLUSH_TIMEOUT.
        let deadline = std::time::Instant::now() + FLUSH_TIMEOUT;
        loop {
            match self.tx.try_send(Msg::Shutdown) {
                Ok(()) | Err(TrySendError::Disconnected(_)) => break,
                Err(TrySendError::Full(_)) => {
                    if std::time::Instant::now() >= deadline {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let _ = self.done.recv_timeout(remaining);
        if let Some(worker) = self.worker.take() {
            if worker.is_finished() {
                let _ = worker.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Shared {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn lossless_writer_flushes_on_guard_drop() {
        let sink = Shared::default();
        let (mut w, guard) = NonBlocking::new(sink.clone(), 8, false, "t");
        for i in 0..100 {
            writeln!(w, "line {i}").unwrap();
        }
        drop(guard);
        let text = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
        assert_eq!(text.lines().count(), 100);
    }

    #[test]
    fn lossy_writer_counts_drops_when_full() {
        // A sink that blocks until released, so the queue fills up.
        struct Blocking(Arc<(Mutex<bool>, std::sync::Condvar)>);
        impl io::Write for Blocking {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                let (lock, cv) = &*self.0;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = cv.wait(released).unwrap();
                }
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let (mut w, guard) = NonBlocking::new(Blocking(gate.clone()), 4, true, "t");
        for _ in 0..50 {
            let _ = w.write_all(b"x\n");
        }
        assert!(w.error_counter().dropped_lines() >= 40);
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        drop(guard);
    }
}
