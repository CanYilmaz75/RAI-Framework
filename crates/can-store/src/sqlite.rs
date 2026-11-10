//! Store auf SQLite: eine Datei, Transaktionen, keine Infrastruktur.

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use can_core::{
    Digest, Envelope, Event, FinishReason, SCHEMA_VERSION, SessionId, Store, StoreError,
};
use rusqlite::{Connection, ErrorCode, OptionalExtension, params};

use crate::migrate;

/// Kurzangaben zu einer gespeicherten Sitzung.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: SessionId,
    /// `running`, `finished` oder `failed`.
    pub status: String,
    pub events: u64,
}

/// Event-Log in einer SQLite-Datei. Zugriffe laufen über `spawn_blocking`.
#[derive(Clone)]
pub struct SqliteStore {
    connection: Arc<Mutex<Connection>>,
    /// Kennung dieses Schreibers; steht zur Diagnose in `sessions.owner`.
    owner: String,
}

fn backend(error: impl std::error::Error + Send + Sync + 'static) -> StoreError {
    StoreError::Backend(Box::new(error))
}

fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

impl SqliteStore {
    /// Öffnet oder erzeugt die Datei und führt ausstehende Migrationen aus.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::prepare(Connection::open(path).map_err(backend)?)
    }

    /// Store, der nur im Arbeitsspeicher lebt.
    pub fn in_memory() -> Result<Self, StoreError> {
        Self::prepare(Connection::open_in_memory().map_err(backend)?)
    }

    fn prepare(mut connection: Connection) -> Result<Self, StoreError> {
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(backend)?;
        migrate::prepare(&mut connection).map_err(backend)?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            owner: SessionId::new().to_string(),
        })
    }

    /// Führt `work` mit der Verbindung auf einem blockierenden Thread aus.
    async fn with<T, F>(&self, work: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, StoreError> + Send + 'static,
    {
        let connection = self.connection.clone();
        let task = tokio::task::spawn_blocking(move || {
            let mut guard = connection.lock().map_err(|_| {
                StoreError::Backend("Sperre der SQLite-Verbindung ist vergiftet".into())
            })?;
            work(&mut guard)
        });
        task.await.map_err(backend)?
    }

    /// Alle Sitzungen, neueste zuerst.
    pub async fn sessions(&self) -> Result<Vec<SessionInfo>, StoreError> {
        self.with(|connection| {
            let mut statement = connection
                .prepare(
                    "SELECT s.id, s.status, (SELECT count(*) FROM events e WHERE e.session_id = s.id)
                     FROM sessions s ORDER BY s.id DESC",
                )
                .map_err(backend)?;
            let rows = statement
                .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?)))
                .map_err(backend)?;
            let mut sessions = Vec::new();
            for row in rows {
                let (id, status, events) = row.map_err(backend)?;
                let id = id.parse().map_err(|_| StoreError::Corrupt(format!("Sitzungskennung {id}")))?;
                sessions.push(SessionInfo { id, status, events: events.max(0) as u64 });
            }
            Ok(sessions)
        })
        .await
    }

    /// Legt Bytes unter ihrer Inhaltsadresse ab.
    pub async fn put_blob(&self, bytes: Vec<u8>) -> Result<Digest, StoreError> {
        self.with(move |connection| {
            let digest = Digest::of(&bytes);
            connection
                .execute(
                    "INSERT OR IGNORE INTO blobs (digest, bytes) VALUES (?1, ?2)",
                    params![digest.0, bytes],
                )
                .map_err(backend)?;
            Ok(digest)
        })
        .await
    }

    /// Liest einen Blob; `None`, wenn es ihn nicht gibt.
    pub async fn get_blob(&self, digest: Digest) -> Result<Option<Vec<u8>>, StoreError> {
        self.with(move |connection| {
            connection
                .query_row(
                    "SELECT bytes FROM blobs WHERE digest = ?1",
                    params![digest.0],
                    |row| row.get(0),
                )
                .optional()
                .map_err(backend)
        })
        .await
    }
}

/// Hebt ein gespeichertes Event älterer Formatversion auf die aktuelle an.
fn upcast(schema: u16, payload: &str) -> Result<Envelope, StoreError> {
    if schema > SCHEMA_VERSION {
        return Err(StoreError::Corrupt(format!(
            "Event-Format {schema} ist neuer als dieses Programm"
        )));
    }
    // Version 1 ist die erste; künftige Upcaster wandeln hier das JSON um, bevor es gelesen wird.
    serde_json::from_str(payload).map_err(|error| StoreError::Corrupt(error.to_string()))
}

#[async_trait]
impl Store for SqliteStore {
    async fn load(&self, id: SessionId) -> Result<Vec<Envelope>, StoreError> {
        self.with(move |connection| {
            let mut statement = connection
                .prepare("SELECT schema, payload FROM events WHERE session_id = ?1 ORDER BY seq")
                .map_err(backend)?;
            let rows = statement
                .query_map(params![id.to_string()], |row| {
                    Ok((row.get::<_, u16>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(backend)?;
            let mut log = Vec::new();
            for row in rows {
                let (schema, payload) = row.map_err(backend)?;
                log.push(upcast(schema, &payload)?);
            }
            Ok(log)
        })
        .await
    }

    async fn append(&self, id: SessionId, events: &[Envelope]) -> Result<(), StoreError> {
        let (Some(first), Some(last)) = (events.first(), events.last()) else {
            return Ok(());
        };
        let (first_seq, first_ts, last_ts) = (first.seq, first.ts.0, last.ts.0);
        let status = match &last.event {
            Event::RunFinished {
                reason: FinishReason::Error { .. },
            } => "failed",
            Event::RunFinished { .. } => "finished",
            _ => "running",
        };
        let mut rows = Vec::with_capacity(events.len());
        for envelope in events {
            let payload = serde_json::to_string(envelope).map_err(backend)?;
            rows.push((
                envelope.seq,
                envelope.ts.0,
                envelope.schema,
                envelope.event.kind(),
                payload,
            ));
        }
        let (session, owner) = (id.to_string(), self.owner.clone());
        self.with(move |connection| {
            let transaction = connection.transaction().map_err(backend)?;
            transaction
                .execute(
                    "INSERT OR IGNORE INTO sessions (id, created_at, config_json, status) VALUES (?1, ?2, '{}', 'running')",
                    params![session, to_i64(first_ts)],
                )
                .map_err(backend)?;
            let next: i64 = transaction
                .query_row("SELECT coalesce(max(seq) + 1, 0) FROM events WHERE session_id = ?1", params![session], |row| {
                    row.get(0)
                })
                .map_err(backend)?;
            for (expected, (seq, ts, schema, kind, payload)) in (next..).zip(rows) {
                if to_i64(seq) != expected {
                    return Err(StoreError::Conflict { session: id, seq });
                }
                let inserted = transaction.execute(
                    "INSERT INTO events (session_id, seq, ts, schema, kind, payload) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![session, to_i64(seq), to_i64(ts), schema, kind, payload],
                );
                match inserted {
                    Ok(_) => {}
                    // Ein anderer Schreiber hat die Position belegt.
                    Err(rusqlite::Error::SqliteFailure(error, _)) if error.code == ErrorCode::ConstraintViolation => {
                        return Err(StoreError::Conflict { session: id, seq });
                    }
                    Err(error) => return Err(backend(error)),
                }
            }
            transaction
                .execute(
                    "UPDATE sessions SET status = ?2, owner = ?3, heartbeat = ?4 WHERE id = ?1",
                    params![session, status, owner, to_i64(last_ts)],
                )
                .map_err(backend)?;
            transaction.commit().map_err(|error| match error {
                rusqlite::Error::SqliteFailure(failure, _) if failure.code == ErrorCode::ConstraintViolation => {
                    StoreError::Conflict { session: id, seq: first_seq }
                }
                other => backend(other),
            })
        })
        .await
    }
}
