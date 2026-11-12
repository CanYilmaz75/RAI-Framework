//! Sitzungen und Event-Verlauf: `MemoryStore`, hinter dem Feature `sqlite` der `SqliteStore` und hinter
//! `postgres` der `PostgresStore` für viele gleichzeitige Schreiber.

#[cfg(any(feature = "sqlite", feature = "postgres"))]
mod memory;
#[cfg(feature = "sqlite")]
mod migrate;
#[cfg(feature = "postgres")]
pub mod postgres;
#[cfg(any(feature = "sqlite", feature = "postgres"))]
pub mod seal;
#[cfg(feature = "sqlite")]
pub mod sqlite;

pub use can_core::{InMemoryBank, Memory, MemoryBank, MemoryStore, Store, StoreError};
#[cfg(feature = "postgres")]
pub use postgres::PostgresStore;
#[cfg(any(feature = "sqlite", feature = "postgres"))]
pub use seal::{Cipher, Sealer};
#[cfg(feature = "sqlite")]
pub use sqlite::{SessionInfo, SqliteStore};
