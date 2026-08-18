//! Client-side projection of one provider-neutral Session stream.

use anyhow::Result;

use crate::{
    protocol::{SessionSnapshot, SessionUpdate},
    session_projection::apply_update,
};

#[derive(Clone, Debug)]
pub(crate) struct SessionProjection {
    snapshot: SessionSnapshot,
}

impl SessionProjection {
    pub(crate) fn new(snapshot: SessionSnapshot) -> Self {
        Self { snapshot }
    }

    pub(crate) fn snapshot(&self) -> &SessionSnapshot {
        &self.snapshot
    }

    pub(crate) fn apply(&mut self, update: SessionUpdate) -> Result<()> {
        apply_update(&mut self.snapshot, &update)
    }
}
