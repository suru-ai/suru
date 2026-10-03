use std::{sync::Arc, time::SystemTime};

/// Where a Relay reads the time it stamps its records with; injectable so a
/// test can see that nothing it keeps lapses as years pass.
#[derive(Clone)]
pub struct Clock(Arc<dyn Fn() -> SystemTime + Send + Sync>);

impl Clock {
    /// The operating system's clock.
    pub fn system() -> Self {
        Self(Arc::new(SystemTime::now))
    }

    /// A clock that reads `now` for the time.
    pub fn from_fn(now: impl Fn() -> SystemTime + Send + Sync + 'static) -> Self {
        Self(Arc::new(now))
    }

    pub fn now(&self) -> SystemTime {
        (self.0)()
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self::system()
    }
}
