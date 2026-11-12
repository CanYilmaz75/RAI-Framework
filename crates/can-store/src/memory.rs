//! Das Gedächtnis ([`MemoryBank`]) in SQLite und PostgreSQL.
//!
//! Gespeichert wird nur der Text, bei Verschlüsselung versiegelt. Gesucht wird über alle
//! Einträge des Namensraums mit der gemeinsamen Rangfolge aus `can_core::knowledge`; das hält
//! beide Backends gleich und funktioniert auch mit versiegeltem Text.

use async_trait::async_trait;
use can_core::{CallId, Memory, MemoryBank, StoreError, Timestamp};

fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn new_id() -> String {
    format!("mem_{CallId}", CallId = CallId::new())
}

#[cfg(feature = "sqlite")]
mod sqlite {
    use super::*;
    use crate::sqlite::{SqliteStore, backend};
    use rusqlite::params;

    #[async_trait]
    impl MemoryBank for SqliteStore {
        async fn add(
            &self,
            namespace: &str,
            text: &str,
            now: Timestamp,
        ) -> Result<String, StoreError> {
            let (id, namespace, sealed) = (new_id(), namespace.to_owned(), self.sealer.seal(text)?);
            let key = id.clone();
            self.with(move |connection| {
                connection
                    .execute(
                        "INSERT INTO memories (id, namespace, created_at, text) VALUES (?1, ?2, ?3, ?4)",
                        params![key, namespace, to_i64(now.0), sealed],
                    )
                    .map_err(backend)?;
                Ok(())
            })
            .await?;
            Ok(id)
        }

        async fn all(&self, namespace: &str) -> Result<Vec<Memory>, StoreError> {
            let namespace = namespace.to_owned();
            let rows = self
                .with(move |connection| {
                    let mut statement = connection
                        .prepare(
                            "SELECT id, created_at, text FROM memories WHERE namespace = ?1
                             ORDER BY created_at, id",
                        )
                        .map_err(backend)?;
                    let rows = statement
                        .query_map([namespace], |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, i64>(1)?,
                                row.get::<_, String>(2)?,
                            ))
                        })
                        .map_err(backend)?;
                    rows.collect::<Result<Vec<_>, _>>().map_err(backend)
                })
                .await?;
            rows.into_iter()
                .map(|(id, at, text)| {
                    Ok(Memory {
                        id,
                        text: self.sealer.open(&text)?,
                        created_at: Timestamp(u64::try_from(at).unwrap_or(0)),
                        score: 0.0,
                    })
                })
                .collect()
        }

        async fn delete(&self, namespace: &str, id: &str) -> Result<bool, StoreError> {
            let (namespace, id) = (namespace.to_owned(), id.to_owned());
            self.with(move |connection| {
                connection
                    .execute(
                        "DELETE FROM memories WHERE namespace = ?1 AND id = ?2",
                        params![namespace, id],
                    )
                    .map(|deleted| deleted > 0)
                    .map_err(backend)
            })
            .await
        }
    }
}

#[cfg(feature = "postgres")]
mod postgres {
    use super::*;
    use crate::postgres::{PostgresStore, backend};

    #[async_trait]
    impl MemoryBank for PostgresStore {
        async fn add(
            &self,
            namespace: &str,
            text: &str,
            now: Timestamp,
        ) -> Result<String, StoreError> {
            let (id, sealed) = (new_id(), self.sealer.seal(text)?);
            let client = self.client.lock().await;
            client
                .execute(
                    "INSERT INTO can_memories (id, namespace, created_at, text) VALUES ($1, $2, $3, $4)",
                    &[&id, &namespace, &to_i64(now.0), &sealed],
                )
                .await
                .map_err(backend)?;
            Ok(id)
        }

        async fn all(&self, namespace: &str) -> Result<Vec<Memory>, StoreError> {
            let rows = {
                let client = self.client.lock().await;
                client
                    .query(
                        "SELECT id, created_at, text FROM can_memories WHERE namespace = $1
                         ORDER BY created_at, id",
                        &[&namespace],
                    )
                    .await
                    .map_err(backend)?
            };
            rows.iter()
                .map(|row| {
                    Ok(Memory {
                        id: row.get(0),
                        text: self.sealer.open(row.get::<_, &str>(2))?,
                        created_at: Timestamp(u64::try_from(row.get::<_, i64>(1)).unwrap_or(0)),
                        score: 0.0,
                    })
                })
                .collect()
        }

        async fn delete(&self, namespace: &str, id: &str) -> Result<bool, StoreError> {
            let client = self.client.lock().await;
            let deleted = client
                .execute(
                    "DELETE FROM can_memories WHERE namespace = $1 AND id = $2",
                    &[&namespace, &id],
                )
                .await
                .map_err(backend)?;
            Ok(deleted > 0)
        }
    }
}
