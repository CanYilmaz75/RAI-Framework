//! Store auf PostgreSQL: für einen Dienst mit vielen Sitzungen und mehreren Workern.

use std::time::Duration;

use async_trait::async_trait;
use can_core::{
    Digest, Envelope, Event, FinishReason, SCHEMA_VERSION, SessionId, Store, StoreError, Timestamp,
    store::lease_until,
};
use tokio::sync::Mutex;
use tokio_postgres::{Client, NoTls, error::SqlState};

/// Jede Migration hebt die Version um eins; neue kommen ans Ende.
const MIGRATIONS: &[&str] = &["
CREATE TABLE can_sessions (
  id          TEXT PRIMARY KEY,
  created_at  BIGINT NOT NULL,
  status      TEXT NOT NULL,      -- running, finished, failed
  owner       TEXT,               -- Schreiber, der die Sitzung gerade führt
  heartbeat   BIGINT              -- Ablauf seiner Sperre in Millisekunden
);
CREATE TABLE can_events (
  session_id  TEXT NOT NULL REFERENCES can_sessions(id),
  seq         BIGINT NOT NULL,
  ts          BIGINT NOT NULL,
  schema      INTEGER NOT NULL,
  kind        TEXT NOT NULL,
  payload     TEXT NOT NULL,      -- JSON
  PRIMARY KEY (session_id, seq)
);
CREATE TABLE can_blobs (
  digest TEXT PRIMARY KEY,        -- Inhaltsadresse
  bytes  BYTEA NOT NULL
);
"];

/// Event-Log in PostgreSQL. Mehrere Prozesse dürfen dieselbe Datenbank nutzen; pro Sitzung
/// schreibt immer nur einer.
pub struct PostgresStore {
    client: Mutex<Client>,
}

fn backend(error: tokio_postgres::Error) -> StoreError {
    StoreError::Backend(Box::new(error))
}

fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

impl PostgresStore {
    /// Verbindet sich (ohne TLS) und führt ausstehende Migrationen aus. `url` hat die Form
    /// `postgres://nutzer:passwort@host:port/datenbank`.
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        let (mut client, connection) =
            tokio_postgres::connect(url, NoTls).await.map_err(backend)?;
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                eprintln!("Verbindung zu PostgreSQL beendet: {error}");
            }
        });
        Self::migrate(&mut client).await?;
        Ok(Self {
            client: Mutex::new(client),
        })
    }

    /// Bringt das Schema auf den neuesten Stand. Eine Sperre verhindert, dass zwei startende
    /// Prozesse gleichzeitig migrieren.
    async fn migrate(client: &mut Client) -> Result<(), StoreError> {
        let transaction = client.transaction().await.map_err(backend)?;
        transaction
            .batch_execute("SELECT pg_advisory_xact_lock(7226001)")
            .await
            .map_err(backend)?;
        transaction
            .batch_execute("CREATE TABLE IF NOT EXISTS can_schema (version INTEGER NOT NULL)")
            .await
            .map_err(backend)?;
        let row = transaction
            .query_one("SELECT coalesce(max(version), 0) FROM can_schema", &[])
            .await
            .map_err(backend)?;
        let version = usize::try_from(row.get::<_, i32>(0)).unwrap_or(0);
        for (index, migration) in MIGRATIONS.iter().enumerate().skip(version) {
            transaction
                .batch_execute(migration)
                .await
                .map_err(backend)?;
            let next = i32::try_from(index + 1).unwrap_or(i32::MAX);
            transaction
                .execute("INSERT INTO can_schema (version) VALUES ($1)", &[&next])
                .await
                .map_err(backend)?;
        }
        transaction.commit().await.map_err(backend)
    }
}

#[async_trait]
impl Store for PostgresStore {
    async fn load(&self, id: SessionId) -> Result<Vec<Envelope>, StoreError> {
        let client = self.client.lock().await;
        let rows = client
            .query(
                "SELECT schema, payload FROM can_events WHERE session_id = $1 ORDER BY seq",
                &[&id.to_string()],
            )
            .await
            .map_err(backend)?;
        let mut log = Vec::with_capacity(rows.len());
        for row in rows {
            let (schema, payload): (i32, String) = (row.get(0), row.get(1));
            if schema > i32::from(SCHEMA_VERSION) {
                return Err(StoreError::Corrupt(format!(
                    "Event-Format {schema} ist neuer als dieses Programm"
                )));
            }
            log.push(
                serde_json::from_str(&payload)
                    .map_err(|error| StoreError::Corrupt(error.to_string()))?,
            );
        }
        Ok(log)
    }

    async fn append(&self, id: SessionId, events: &[Envelope]) -> Result<(), StoreError> {
        let (Some(first), Some(last)) = (events.first(), events.last()) else {
            return Ok(());
        };
        let status = match &last.event {
            Event::RunFinished {
                reason: FinishReason::Error { .. },
            } => "failed",
            Event::RunFinished { .. } => "finished",
            _ => "running",
        };
        let session = id.to_string();
        let mut client = self.client.lock().await;
        let transaction = client.transaction().await.map_err(backend)?;
        transaction
            .execute(
                "INSERT INTO can_sessions (id, created_at, status) VALUES ($1, $2, 'running') ON CONFLICT DO NOTHING",
                &[&session, &to_i64(first.ts.0)],
            )
            .await
            .map_err(backend)?;
        // Die Zeilensperre reiht gleichzeitige Schreiber derselben Sitzung hintereinander.
        transaction
            .execute(
                "SELECT 1 FROM can_sessions WHERE id = $1 FOR UPDATE",
                &[&session],
            )
            .await
            .map_err(backend)?;
        let row = transaction
            .query_one(
                "SELECT coalesce(max(seq) + 1, 0) FROM can_events WHERE session_id = $1",
                &[&session],
            )
            .await
            .map_err(backend)?;
        let next: i64 = row.get(0);
        for (expected, envelope) in (next..).zip(events) {
            if to_i64(envelope.seq) != expected {
                return Err(StoreError::Conflict {
                    session: id,
                    seq: envelope.seq,
                });
            }
            let payload = serde_json::to_string(envelope)
                .map_err(|error| StoreError::Backend(Box::new(error)))?;
            let inserted = transaction
                .execute(
                    "INSERT INTO can_events (session_id, seq, ts, schema, kind, payload) VALUES ($1, $2, $3, $4, $5, $6)",
                    &[&session, &expected, &to_i64(envelope.ts.0), &i32::from(envelope.schema), &envelope.event.kind(), &payload],
                )
                .await;
            match inserted {
                Ok(_) => {}
                Err(error) if error.code() == Some(&SqlState::UNIQUE_VIOLATION) => {
                    return Err(StoreError::Conflict {
                        session: id,
                        seq: envelope.seq,
                    });
                }
                Err(error) => return Err(backend(error)),
            }
        }
        transaction
            .execute(
                "UPDATE can_sessions SET status = $2 WHERE id = $1",
                &[&session, &status],
            )
            .await
            .map_err(backend)?;
        transaction.commit().await.map_err(backend)
    }

    async fn put_blob(&self, bytes: Vec<u8>) -> Result<Digest, StoreError> {
        let digest = Digest::of(&bytes);
        let client = self.client.lock().await;
        client
            .execute(
                "INSERT INTO can_blobs (digest, bytes) VALUES ($1, $2) ON CONFLICT DO NOTHING",
                &[&digest.0, &bytes],
            )
            .await
            .map_err(backend)?;
        Ok(digest)
    }

    async fn get_blob(&self, digest: &Digest) -> Result<Option<Vec<u8>>, StoreError> {
        let client = self.client.lock().await;
        let row = client
            .query_opt(
                "SELECT bytes FROM can_blobs WHERE digest = $1",
                &[&digest.0],
            )
            .await
            .map_err(backend)?;
        Ok(row.map(|row| row.get(0)))
    }

    async fn acquire(
        &self,
        id: SessionId,
        owner: &str,
        now: Timestamp,
        ttl: Duration,
    ) -> Result<(), StoreError> {
        let session = id.to_string();
        let (now, until) = (to_i64(now.0), to_i64(lease_until(now, ttl).0));
        let mut client = self.client.lock().await;
        let transaction = client.transaction().await.map_err(backend)?;
        transaction
            .execute(
                "INSERT INTO can_sessions (id, created_at, status) VALUES ($1, $2, 'running') ON CONFLICT DO NOTHING",
                &[&session, &now],
            )
            .await
            .map_err(backend)?;
        // Nur wer die Sperre schon hält oder eine abgelaufene vorfindet, bekommt sie.
        let changed = transaction
            .execute(
                "UPDATE can_sessions SET owner = $2, heartbeat = $3
                 WHERE id = $1 AND (owner IS NULL OR owner = $2 OR heartbeat IS NULL OR heartbeat <= $4)",
                &[&session, &owner, &until, &now],
            )
            .await
            .map_err(backend)?;
        if changed == 0 {
            let row = transaction
                .query_one("SELECT owner FROM can_sessions WHERE id = $1", &[&session])
                .await
                .map_err(backend)?;
            let holder: Option<String> = row.get(0);
            return Err(StoreError::Locked {
                session: id,
                owner: holder.unwrap_or_default(),
            });
        }
        transaction.commit().await.map_err(backend)
    }

    async fn release(&self, id: SessionId, owner: &str) -> Result<(), StoreError> {
        let client = self.client.lock().await;
        client
            .execute(
                "UPDATE can_sessions SET owner = NULL, heartbeat = NULL WHERE id = $1 AND owner = $2",
                &[&id.to_string(), &owner],
            )
            .await
            .map(|_| ())
            .map_err(backend)
    }
}
