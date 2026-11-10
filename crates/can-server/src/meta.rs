//! Verzeichnis der Sitzungen: wem sie gehören, welcher Agent sie führt, in welchem Zustand sie sind.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use can_core::{SessionId, Timestamp};
use serde::{Deserialize, Serialize};

/// Zustand einer Sitzung aus Sicht des Dienstes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// Ein Lauf ist unterwegs oder wurde unterbrochen und wartet auf Übernahme.
    Running,
    /// Der letzte Lauf ist regulär beendet; die Sitzung nimmt neue Nachrichten an.
    Idle,
    /// Der letzte Lauf endete mit Fehler, Limit oder Abbruch.
    Stopped,
}

/// Eintrag zu einer Sitzung.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: SessionId,
    pub tenant: String,
    pub user: String,
    pub agent: String,
    pub created_at: Timestamp,
    pub status: SessionStatus,
    /// Vom Aufrufer gewählter Schlüssel, der doppelte Starts erkennt.
    pub idempotency_key: Option<String>,
}

/// Fehler des Verzeichnisses.
#[derive(Debug, thiserror::Error)]
#[error("Sitzungsverzeichnis: {0}")]
pub struct MetaError(pub String);

/// Speichert die Einträge. Für mehrere Worker muss die Implementierung geteilt sein.
#[async_trait]
pub trait SessionDirectory: Send + Sync {
    /// Legt einen Eintrag an. Gibt es im Mandanten schon einen mit demselben
    /// Idempotenz-Schlüssel, kommt dieser zurück und nichts wird angelegt.
    async fn create(&self, meta: SessionMeta) -> Result<SessionMeta, MetaError>;

    /// Eintrag der Sitzung.
    async fn get(&self, id: SessionId) -> Result<Option<SessionMeta>, MetaError>;

    /// Alle Sitzungen eines Mandanten, neueste zuerst.
    async fn list(&self, tenant: &str) -> Result<Vec<SessionMeta>, MetaError>;

    /// Alle Sitzungen im Zustand `Running`, über alle Mandanten: Kandidaten für die Übernahme.
    async fn running(&self) -> Result<Vec<SessionMeta>, MetaError>;

    /// Setzt den Zustand.
    async fn set_status(&self, id: SessionId, status: SessionStatus) -> Result<(), MetaError>;
}

/// Verzeichnis im Arbeitsspeicher: für Tests und einen einzelnen Prozess.
#[derive(Debug, Default)]
pub struct MemoryDirectory {
    sessions: Mutex<HashMap<SessionId, SessionMeta>>,
}

impl MemoryDirectory {
    /// Leeres Verzeichnis.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<SessionId, SessionMeta>>, MetaError> {
        self.sessions
            .lock()
            .map_err(|_| MetaError("Sperre vergiftet".into()))
    }
}

#[async_trait]
impl SessionDirectory for MemoryDirectory {
    async fn create(&self, meta: SessionMeta) -> Result<SessionMeta, MetaError> {
        let mut sessions = self.lock()?;
        if let Some(key) = &meta.idempotency_key {
            let same = |other: &&SessionMeta| {
                other.tenant == meta.tenant && other.idempotency_key.as_ref() == Some(key)
            };
            if let Some(existing) = sessions.values().find(same) {
                return Ok(existing.clone());
            }
        }
        sessions.insert(meta.id, meta.clone());
        Ok(meta)
    }

    async fn get(&self, id: SessionId) -> Result<Option<SessionMeta>, MetaError> {
        Ok(self.lock()?.get(&id).cloned())
    }

    async fn list(&self, tenant: &str) -> Result<Vec<SessionMeta>, MetaError> {
        let mut found: Vec<SessionMeta> = self
            .lock()?
            .values()
            .filter(|meta| meta.tenant == tenant)
            .cloned()
            .collect();
        found.sort_by_key(|meta| std::cmp::Reverse(meta.id));
        Ok(found)
    }

    async fn running(&self) -> Result<Vec<SessionMeta>, MetaError> {
        let running = |meta: &&SessionMeta| meta.status == SessionStatus::Running;
        Ok(self.lock()?.values().filter(running).cloned().collect())
    }

    async fn set_status(&self, id: SessionId, status: SessionStatus) -> Result<(), MetaError> {
        if let Some(meta) = self.lock()?.get_mut(&id) {
            meta.status = status;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(n: u64, tenant: &str, key: Option<&str>) -> SessionMeta {
        SessionMeta {
            id: SessionId::from_parts(n, 1),
            tenant: tenant.into(),
            user: "ada".into(),
            agent: "helfer".into(),
            created_at: Timestamp(n),
            status: SessionStatus::Running,
            idempotency_key: key.map(str::to_owned),
        }
    }

    #[tokio::test]
    async fn create_list_and_status() {
        let directory = MemoryDirectory::new();
        directory.create(meta(1, "acme", None)).await.unwrap();
        directory.create(meta(2, "acme", None)).await.unwrap();
        directory.create(meta(3, "globex", None)).await.unwrap();
        let ids: Vec<SessionId> = directory
            .list("acme")
            .await
            .unwrap()
            .iter()
            .map(|meta| meta.id)
            .collect();
        assert_eq!(
            ids,
            vec![SessionId::from_parts(2, 1), SessionId::from_parts(1, 1)],
            "neueste zuerst, nur der Mandant"
        );
        assert_eq!(directory.running().await.unwrap().len(), 3);
        directory
            .set_status(SessionId::from_parts(1, 1), SessionStatus::Idle)
            .await
            .unwrap();
        assert_eq!(
            directory
                .get(SessionId::from_parts(1, 1))
                .await
                .unwrap()
                .unwrap()
                .status,
            SessionStatus::Idle
        );
        assert_eq!(directory.running().await.unwrap().len(), 2);
        assert_eq!(
            directory.get(SessionId::from_parts(9, 9)).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn idempotency_key_returns_the_first_session_within_a_tenant() {
        let directory = MemoryDirectory::new();
        let first = directory
            .create(meta(1, "acme", Some("abc")))
            .await
            .unwrap();
        let again = directory
            .create(meta(2, "acme", Some("abc")))
            .await
            .unwrap();
        assert_eq!(again.id, first.id);
        let other_tenant = directory
            .create(meta(3, "globex", Some("abc")))
            .await
            .unwrap();
        assert_eq!(
            other_tenant.id,
            SessionId::from_parts(3, 1),
            "Schlüssel gelten pro Mandant"
        );
        assert_eq!(directory.list("acme").await.unwrap().len(), 1);
    }
}
