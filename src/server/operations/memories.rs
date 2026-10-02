//! Keeping Memories: storing one, searching them, recalling one whole,
//! changing one, and forgetting one — the operations a Sidekick's Memory
//! Tools perform, through the same interface as its acts on Sessions.
//!
//! Memories are this Server's own, so none of these reaches a Remote, and no
//! route of the Session API serves them: a Peer or a Client never reads or
//! writes one, and a Sidekick reaches only its own Server's (ADR 0044). Each
//! takes what was written as it was written and answers what the Memories
//! made of it: the store holds it to a Memory's bounds before anything is
//! kept (see [`crate::memories`]). A failure of the Server's own storage is
//! said to the Log here, with what it was doing, so whatever answers the
//! caller need only say that it failed.

use super::SessionOperations;
use crate::memories::{
    FoundMemories, Memory, MemoryError, MemoryId, WrittenChange, WrittenMemory, WrittenSearch,
};

/// Says a failure of the Server's own storage to the Log, as `doing` names
/// what it failed at; anything else `error` says is the caller's to answer.
fn logged(doing: &str, memory: Option<MemoryId>, error: &MemoryError) {
    if let MemoryError::Storage(error) = error {
        match memory {
            Some(memory_id) => tracing::error!(%memory_id, "could not {doing}: {error}"),
            None => tracing::error!("could not {doing}: {error}"),
        }
    }
}

impl SessionOperations {
    /// Stores the Memory `written` gives, and answers it as kept.
    pub(crate) async fn store_memory(
        &self,
        written: WrittenMemory<'_>,
    ) -> Result<Memory, MemoryError> {
        self.memories
            .store(written)
            .await
            .inspect_err(|error| logged("store a Memory", None, error))
    }

    /// The Memories the search `written` asks for finds.
    pub(crate) async fn search_memories(
        &self,
        written: WrittenSearch<'_>,
    ) -> Result<FoundMemories, MemoryError> {
        self.memories
            .search(written)
            .await
            .inspect_err(|error| logged("search Memories", None, error))
    }

    /// The Memory `id` names, whole.
    pub(crate) async fn recall_memory(&self, id: MemoryId) -> Result<Memory, MemoryError> {
        self.memories
            .recall(id)
            .await
            .inspect_err(|error| logged("read a Memory", Some(id), error))
    }

    /// Changes the Memory `id` names as `written` says, and answers it as it
    /// stands.
    pub(crate) async fn change_memory(
        &self,
        id: MemoryId,
        written: WrittenChange<'_>,
    ) -> Result<Memory, MemoryError> {
        self.memories
            .change(id, written)
            .await
            .inspect_err(|error| logged("change a Memory", Some(id), error))
    }

    /// Forgets the Memory `id` names.
    pub(crate) async fn forget_memory(&self, id: MemoryId) -> Result<(), MemoryError> {
        self.memories
            .forget(id)
            .await
            .inspect_err(|error| logged("forget a Memory", Some(id), error))
    }
}
