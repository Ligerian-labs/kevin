//! [`EventSource`]: where a bus reads events back by global position.
//!
//! Implemented by the event store (`kevin-store`, WS-03) and by
//! `kevin_testkit::bus::VecEventSource` for tests.

use std::fmt;

use async_trait::async_trait;

use crate::BusEvent;

/// Opaque error from an [`EventSource`].
#[derive(Debug)]
pub struct SourceError(Box<dyn std::error::Error + Send + Sync + 'static>);

impl SourceError {
    /// Wraps any error.
    pub fn new(err: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self(Box::new(err))
    }

    /// From a message.
    pub fn msg(msg: impl Into<String>) -> Self {
        Self(msg.into().into())
    }
}

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl std::error::Error for SourceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}

/// Ordered, position-addressed read access to the committed events.
#[async_trait]
pub trait EventSource: Send + Sync + 'static {
    /// Events with `position > from_position`, ascending, at most `limit`
    /// (`from_position` is exclusive; `0` reads from the beginning).
    async fn read_all(
        &self,
        from_position: u64,
        limit: usize,
    ) -> Result<Vec<BusEvent>, SourceError>;

    /// The highest committed position (`0` when empty).
    async fn latest_position(&self) -> Result<u64, SourceError>;
}
