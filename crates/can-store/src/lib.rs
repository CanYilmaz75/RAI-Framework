//! Sitzungen und Event-Verlauf: `MemoryStore`, hinter dem Feature `sqlite` der `SqliteStore` und hinter
//! `postgres` der `PostgresStore` für viele gleichzeitige Schreiber.

#[cfg(feature = "sqlite")]
mod migrate;
#[cfg(feature = "postgres")]
pub mod postgres;
#[cfg(feature = "sqlite")]
pub mod sqlite;

pub use can_core::{MemoryStore, Store, StoreError};
#[cfg(feature = "postgres")]
pub use postgres::PostgresStore;
#[cfg(feature = "sqlite")]
pub use sqlite::{SessionInfo, SqliteStore};
