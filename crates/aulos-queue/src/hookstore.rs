//! [`EngineHookStore`]: the `aulos_core::ports::HookStore` implementation (DESIGN §7.1, §13.3).
//!
//! The read goes straight to the store's read pool; both **writes** become
//! [`crate::EngineCmd::HookWrite`]s. That routing is the whole point: a hook that wrote SQLite
//! directly would leave `size` stale in the engine's item cache, in the aggregator's `last_sent`
//! and on every connected client until a restart — and `stress_consistency` could not see it,
//! because it compares the frames against that same stale snapshot.

use aulos_core::{EntryBlob, HookStore, ItemId, PortError};
use aulos_store::Store;

use crate::cmd::{EngineHandle, HookWrite};
use crate::engine::map_port_error;

/// The engine-mediated item-store port a hook is handed (DESIGN §13.3).
#[derive(Clone, Debug)]
pub struct EngineHookStore {
    engine: EngineHandle,
    store: Store,
}

impl EngineHookStore {
    /// Pairs the engine handle with the store the read delegates to.
    #[must_use]
    pub fn new(engine: EngineHandle, store: Store) -> Self {
        Self { engine, store }
    }
}

#[async_trait::async_trait]
impl HookStore for EngineHookStore {
    async fn entry_blob(&self, id: ItemId) -> Result<Option<EntryBlob>, PortError> {
        self.store.entry_blob(id).await.map_err(map_port_error)
    }

    async fn drop_entry_blob(&self, id: ItemId) -> Result<(), PortError> {
        self.engine.hook_write(id, HookWrite::DropEntryBlob).await
    }

    async fn set_size(&self, id: ItemId, size: u64) -> Result<(), PortError> {
        self.engine.hook_write(id, HookWrite::Size(size)).await
    }
}
