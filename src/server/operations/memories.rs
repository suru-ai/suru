//! Keeping Memories: storing one, searching them, recalling one whole,
//! changing one, and forgetting one — the operations a Sidekick's Memory
//! Tools perform, through the same interface as its acts on Sessions.
//!
//! Memories are this Server's own, so none of these reaches a Remote, and no
//! route of the Session API serves them: a Peer or a Client never reads or
//! writes one, and a Sidekick reaches only its own Server's (ADR 0044). Each
//! takes what it is given already within a Memory's bounds and normalised,
//! where a Sidekick's Tool read it, and answers what the Server's own storage
//! did. A failure of that storage is said to the Log here, with what it was
//! doing, so whatever answers the caller need only say that it failed.

use super::SessionOperations;
use crate::{
    memories::{FoundMemories, Memory, MemoryChange, MemoryId, MemorySearch, NewMemory},
    storage::StorageError,
};

impl SessionOperations {
    /// Stores `memory`, and answers it as kept.
    pub(crate) async fn store_memory(&self, memory: NewMemory) -> Result<Memory, StorageError> {
        self.memories
            .store(memory)
            .await
            .inspect_err(|error| tracing::error!("could not store a Memory: {error}"))
    }

    /// The Memories `search` finds.
    pub(crate) async fn search_memories(
        &self,
        search: MemorySearch,
    ) -> Result<FoundMemories, StorageError> {
        self.memories
            .search(search)
            .await
            .inspect_err(|error| tracing::error!("could not search Memories: {error}"))
    }

    /// The Memory `id` names, whole, where one is kept.
    pub(crate) async fn recall_memory(&self, id: MemoryId) -> Result<Option<Memory>, StorageError> {
        self.memories.recall(id).await.inspect_err(
            |error| tracing::error!(memory_id = %id, "could not read a Memory: {error}"),
        )
    }

    /// Changes the Memory `id` names as `change` says, and answers it as it
    /// stands, where one is kept.
    pub(crate) async fn change_memory(
        &self,
        id: MemoryId,
        change: MemoryChange,
    ) -> Result<Option<Memory>, StorageError> {
        self.memories.change(id, change).await.inspect_err(|error| {
            tracing::error!(memory_id = %id, "could not change a Memory: {error}");
        })
    }

    /// Forgets the Memory `id` names; answers whether one was kept to forget.
    pub(crate) async fn forget_memory(&self, id: MemoryId) -> Result<bool, StorageError> {
        self.memories.forget(id).await.inspect_err(|error| {
            tracing::error!(memory_id = %id, "could not forget a Memory: {error}");
        })
    }
}
