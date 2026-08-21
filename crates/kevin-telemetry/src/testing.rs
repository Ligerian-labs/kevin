//! Helpers for asserting on log output in tests of any crate.

use std::io;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tracing_subscriber::fmt::MakeWriter;

/// An in-memory `MakeWriter` collecting every record; `Clone` shares the buffer.
#[derive(Debug, Clone, Default)]
pub struct MemoryWriter {
    buf: Arc<Mutex<Vec<u8>>>,
}

impl MemoryWriter {
    /// An empty buffer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything written so far, as text.
    #[must_use]
    pub fn contents(&self) -> String {
        String::from_utf8_lossy(&self.lock()).into_owned()
    }

    /// Non-empty lines written so far.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        self.contents()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(str::to_owned)
            .collect()
    }

    /// Lines parsed as JSON objects (JSON format only); non-JSON lines are skipped.
    #[must_use]
    pub fn json_records(&self) -> Vec<Value> {
        self.lines()
            .iter()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .collect()
    }

    /// Clears the buffer.
    pub fn clear(&self) {
        self.lock().clear();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<u8>> {
        self.buf
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl io::Write for MemoryWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.lock().extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for MemoryWriter {
    type Writer = MemoryWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
