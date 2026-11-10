//! Sitzungen und Event-Verlauf: `MemoryStore` und, hinter dem Feature `sqlite`, `SqliteStore`.

#[cfg(feature = "sqlite")]
mod migrate;
#[cfg(feature = "sqlite")]
pub mod sqlite;

pub use agentkit_core::{MemoryStore, Store, StoreError};
#[cfg(feature = "sqlite")]
pub use sqlite::{SessionInfo, SqliteStore};
