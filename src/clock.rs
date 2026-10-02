//! The wall clock a Server reads where a test must be able to move time
//! rather than wait it out.

use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use crate::protocol::SessionTimestamp;

/// Where a Server reads the time now: the real clock by default, or one a
/// test moves by hand.
#[derive(Clone)]
pub struct ServerClock(Arc<dyn Fn() -> SessionTimestamp + Send + Sync>);

impl ServerClock {
    /// A clock that starts at the real time now and moves only when its
    /// handle advances it.
    pub fn manual() -> (Self, ManualClock) {
        let millis = Arc::new(AtomicU64::new(SessionTimestamp::now().0));
        let reading = millis.clone();
        (
            Self(Arc::new(move || {
                SessionTimestamp(reading.load(Ordering::SeqCst))
            })),
            ManualClock(millis),
        )
    }

    pub fn now(&self) -> SessionTimestamp {
        (self.0)()
    }
}

#[cfg(test)]
impl ServerClock {
    /// A clock that reads the time from `reading`, whatever it does, for a
    /// test that must see when, or how often, the time is read.
    pub(crate) fn reading(reading: impl Fn() -> SessionTimestamp + Send + Sync + 'static) -> Self {
        Self(Arc::new(reading))
    }
}

impl Default for ServerClock {
    fn default() -> Self {
        Self(Arc::new(SessionTimestamp::now))
    }
}

impl fmt::Debug for ServerClock {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ServerClock")
            .field(&self.now())
            .finish()
    }
}

/// The hand that moves a [`ServerClock::manual`] clock.
#[derive(Clone, Debug)]
pub struct ManualClock(Arc<AtomicU64>);

impl ManualClock {
    pub fn advance(&self, by: Duration) {
        let by = u64::try_from(by.as_millis()).unwrap_or(u64::MAX);
        self.0.fetch_add(by, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manual_clock_moves_only_when_advanced() {
        let (clock, hand) = ServerClock::manual();
        let start = clock.now();
        assert_eq!(clock.now(), start);
        hand.advance(Duration::from_secs(61 * 60));
        assert_eq!(clock.now(), SessionTimestamp(start.0 + 61 * 60 * 1000));
    }
}
