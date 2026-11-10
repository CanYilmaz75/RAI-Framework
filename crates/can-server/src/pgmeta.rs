//! Sitzungsverzeichnis in PostgreSQL: geteilt zwischen allen Workern, überlebt Neustarts.

use async_trait::async_trait;
use can_core::{SessionId, Timestamp};
use tokio::sync::Mutex;
use tokio_postgres::{Client, NoTls, Row};

use crate::meta::{MetaError, SessionDirectory, SessionMeta, SessionStatus};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS can_session_meta (
  id              TEXT PRIMARY KEY,
  tenant          TEXT NOT NULL,
  user_name       TEXT NOT NULL,
  agent           TEXT NOT NULL,
  created_at      BIGINT NOT NULL,
  status          TEXT NOT NULL,
  idempotency_key TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS can_session_meta_idempotency
  ON can_session_meta (tenant, idempotency_key) WHERE idempotency_key IS NOT NULL;
CREATE INDEX IF NOT EXISTS can_session_meta_tenant ON can_session_meta (tenant, id DESC);
";

const COLUMNS: &str = "id, tenant, user_name, agent, created_at, status, idempotency_key";

/// Verzeichnis in PostgreSQL.
pub struct PostgresDirectory {
    client: Mutex<Client>,
}

fn failed(error: impl std::fmt::Display) -> MetaError {
    MetaError(error.to_string())
}

fn status_text(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Running => "running",
        SessionStatus::Idle => "idle",
        SessionStatus::Stopped => "stopped",
    }
}

fn from_row(row: &Row) -> Result<SessionMeta, MetaError> {
    let id: String = row.get(0);
    let status = match row.get::<_, String>(5).as_str() {
        "running" => SessionStatus::Running,
        "idle" => SessionStatus::Idle,
        _ => SessionStatus::Stopped,
    };
    Ok(SessionMeta {
        id: id.parse().map_err(failed)?,
        tenant: row.get(1),
        user: row.get(2),
        agent: row.get(3),
        created_at: Timestamp(u64::try_from(row.get::<_, i64>(4)).unwrap_or(0)),
        status,
        idempotency_key: row.get(6),
    })
}

impl PostgresDirectory {
    /// Verbindet sich (ohne TLS) und legt die Tabelle an, falls sie fehlt.
    pub async fn connect(url: &str) -> Result<Self, MetaError> {
        let (client, connection) = tokio_postgres::connect(url, NoTls).await.map_err(failed)?;
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                tracing::error!(%error, "Verbindung zu PostgreSQL beendet");
            }
        });
        client.batch_execute(SCHEMA).await.map_err(failed)?;
        Ok(Self {
            client: Mutex::new(client),
        })
    }
}

#[async_trait]
impl SessionDirectory for PostgresDirectory {
    async fn create(&self, meta: SessionMeta) -> Result<SessionMeta, MetaError> {
        let client = self.client.lock().await;
        let created_at = i64::try_from(meta.created_at.0).unwrap_or(i64::MAX);
        // Der eindeutige Index entscheidet das Rennen zweier gleicher Idempotenz-Schlüssel.
        let inserted = client
            .execute(
                &format!("INSERT INTO can_session_meta ({COLUMNS}) VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT DO NOTHING"),
                &[&meta.id.to_string(), &meta.tenant, &meta.user, &meta.agent, &created_at, &status_text(meta.status), &meta.idempotency_key],
            )
            .await
            .map_err(failed)?;
        if inserted == 1 {
            return Ok(meta);
        }
        let row = client
            .query_one(
                &format!("SELECT {COLUMNS} FROM can_session_meta WHERE tenant = $1 AND idempotency_key = $2"),
                &[&meta.tenant, &meta.idempotency_key],
            )
            .await
            .map_err(failed)?;
        from_row(&row)
    }

    async fn get(&self, id: SessionId) -> Result<Option<SessionMeta>, MetaError> {
        let client = self.client.lock().await;
        let row = client
            .query_opt(
                &format!("SELECT {COLUMNS} FROM can_session_meta WHERE id = $1"),
                &[&id.to_string()],
            )
            .await
            .map_err(failed)?;
        row.as_ref().map(from_row).transpose()
    }

    async fn list(&self, tenant: &str) -> Result<Vec<SessionMeta>, MetaError> {
        let client = self.client.lock().await;
        let rows = client
            .query(
                &format!(
                    "SELECT {COLUMNS} FROM can_session_meta WHERE tenant = $1 ORDER BY id DESC"
                ),
                &[&tenant],
            )
            .await
            .map_err(failed)?;
        rows.iter().map(from_row).collect()
    }

    async fn running(&self) -> Result<Vec<SessionMeta>, MetaError> {
        let client = self.client.lock().await;
        let rows = client
            .query(
                &format!("SELECT {COLUMNS} FROM can_session_meta WHERE status = 'running'"),
                &[],
            )
            .await
            .map_err(failed)?;
        rows.iter().map(from_row).collect()
    }

    async fn set_status(&self, id: SessionId, status: SessionStatus) -> Result<(), MetaError> {
        let client = self.client.lock().await;
        client
            .execute(
                "UPDATE can_session_meta SET status = $2 WHERE id = $1",
                &[&id.to_string(), &status_text(status)],
            )
            .await
            .map(|_| ())
            .map_err(failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(tenant: &str, key: Option<&str>) -> SessionMeta {
        SessionMeta {
            id: SessionId::new(),
            tenant: tenant.into(),
            user: "ada".into(),
            agent: "helfer".into(),
            created_at: Timestamp(42),
            status: SessionStatus::Running,
            idempotency_key: key.map(str::to_owned),
        }
    }

    /// Läuft nur, wenn `CAN_TEST_POSTGRES_URL` auf eine erreichbare Datenbank zeigt.
    #[tokio::test]
    async fn postgres_directory_behaves_like_the_memory_one() {
        let Ok(url) = std::env::var("CAN_TEST_POSTGRES_URL") else {
            eprintln!("CAN_TEST_POSTGRES_URL ist nicht gesetzt: PostgreSQL-Test übersprungen");
            return;
        };
        let directory = PostgresDirectory::connect(&url).await.unwrap();
        // Eigener Mandant pro Lauf, damit frühere Testläufe nicht stören.
        let tenant = format!("test-{}", SessionId::new());
        let key = Some("bestellung-17");
        let first = directory.create(meta(&tenant, key)).await.unwrap();
        let again = directory.create(meta(&tenant, key)).await.unwrap();
        assert_eq!(again, first, "derselbe Schlüssel ergibt dieselbe Sitzung");
        let other = directory.create(meta(&tenant, None)).await.unwrap();

        assert_eq!(directory.get(first.id).await.unwrap(), Some(first.clone()));
        assert_eq!(directory.get(SessionId::new()).await.unwrap(), None);
        let listed: Vec<SessionId> = directory
            .list(&tenant)
            .await
            .unwrap()
            .iter()
            .map(|meta| meta.id)
            .collect();
        assert_eq!(listed, vec![other.id, first.id], "neueste zuerst");
        assert!(directory.list("anderer-mandant").await.unwrap().is_empty());

        directory
            .set_status(first.id, SessionStatus::Idle)
            .await
            .unwrap();
        let running: Vec<SessionId> = directory
            .running()
            .await
            .unwrap()
            .iter()
            .filter(|m| m.tenant == tenant)
            .map(|m| m.id)
            .collect();
        assert_eq!(running, vec![other.id]);
        // Eine zweite Verbindung sieht dasselbe.
        let second = PostgresDirectory::connect(&url).await.unwrap();
        assert_eq!(
            second.get(first.id).await.unwrap().unwrap().status,
            SessionStatus::Idle
        );
    }

    #[tokio::test]
    async fn unreachable_database_is_an_error() {
        assert!(
            PostgresDirectory::connect("postgres://can@127.0.0.1:1/postgres")
                .await
                .is_err()
        );
    }
}
