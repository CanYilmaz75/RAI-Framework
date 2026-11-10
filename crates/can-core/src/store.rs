//! Store-Trait: ein reines Event-Log pro Sitzung, es wird nur angehängt. Dazu Blobs für große
//! Nutzdaten und eine Sperre, damit nur ein Schreiber eine Sitzung führt.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;

use crate::event::{Digest, Envelope, Timestamp};
use crate::ids::SessionId;
use crate::provider::BoxError;

/// Fehler eines Stores.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Die Position ist schon belegt oder nicht die nächste: ein anderer Schreiber besitzt die Sitzung.
    #[error("Schreibkonflikt in Sitzung {session} an Position {seq}")]
    Conflict { session: SessionId, seq: u64 },
    /// Ein anderer Schreiber hält die Sitzung, und seine Sperre ist noch nicht abgelaufen.
    #[error("Sitzung {session} wird gerade von {owner} geführt")]
    Locked { session: SessionId, owner: String },
    #[error("beschädigte Daten: {0}")]
    Corrupt(String),
    #[error("Store-Fehler: {0}")]
    Backend(#[source] BoxError),
}

/// Speichert und lädt die Events einer Sitzung.
#[async_trait]
pub trait Store: Send + Sync {
    /// Alle Events der Sitzung in Reihenfolge; eine unbekannte Sitzung ist leer.
    async fn load(&self, id: SessionId) -> Result<Vec<Envelope>, StoreError>;

    /// Hängt Events an. Entweder alle oder keines; `seq` muss lückenlos anschließen.
    async fn append(&self, id: SessionId, events: &[Envelope]) -> Result<(), StoreError>;

    /// Legt Bytes unter ihrer Inhaltsadresse ab.
    async fn put_blob(&self, bytes: Vec<u8>) -> Result<Digest, StoreError>;

    /// Liest einen Blob; `None`, wenn es ihn nicht gibt.
    async fn get_blob(&self, digest: &Digest) -> Result<Option<Vec<u8>>, StoreError>;

    /// Nimmt oder verlängert die Sperre der Sitzung für `owner` bis `now + ttl`. Hält ein
    /// anderer die Sperre und ist sie nicht abgelaufen, ist das `StoreError::Locked`.
    async fn acquire(
        &self,
        id: SessionId,
        owner: &str,
        now: Timestamp,
        ttl: Duration,
    ) -> Result<(), StoreError>;

    /// Gibt die Sperre frei, falls `owner` sie hält.
    async fn release(&self, id: SessionId, owner: &str) -> Result<(), StoreError>;
}

/// Ablaufzeitpunkt einer Sperre.
pub fn lease_until(now: Timestamp, ttl: Duration) -> Timestamp {
    now.plus_millis(u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX))
}

#[derive(Debug, Default)]
struct Inner {
    sessions: HashMap<SessionId, Vec<Envelope>>,
    blobs: HashMap<Digest, Vec<u8>>,
    leases: HashMap<SessionId, (String, Timestamp)>,
}

/// Store im Arbeitsspeicher, für Tests und eingebettete Nutzung.
#[derive(Debug, Default)]
pub struct MemoryStore {
    inner: Mutex<Inner>,
}

impl MemoryStore {
    /// Leerer Store.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Inner>, StoreError> {
        self.inner
            .lock()
            .map_err(|_| StoreError::Backend("Sperre des MemoryStore ist vergiftet".into()))
    }
}

#[async_trait]
impl Store for MemoryStore {
    async fn load(&self, id: SessionId) -> Result<Vec<Envelope>, StoreError> {
        Ok(self.lock()?.sessions.get(&id).cloned().unwrap_or_default())
    }

    async fn append(&self, id: SessionId, events: &[Envelope]) -> Result<(), StoreError> {
        let mut inner = self.lock()?;
        let log = inner.sessions.entry(id).or_default();
        for (expected, envelope) in (log.len() as u64..).zip(events) {
            if envelope.seq != expected {
                return Err(StoreError::Conflict {
                    session: id,
                    seq: envelope.seq,
                });
            }
        }
        log.extend_from_slice(events);
        Ok(())
    }

    async fn put_blob(&self, bytes: Vec<u8>) -> Result<Digest, StoreError> {
        let digest = Digest::of(&bytes);
        self.lock()?.blobs.insert(digest.clone(), bytes);
        Ok(digest)
    }

    async fn get_blob(&self, digest: &Digest) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(self.lock()?.blobs.get(digest).cloned())
    }

    async fn acquire(
        &self,
        id: SessionId,
        owner: &str,
        now: Timestamp,
        ttl: Duration,
    ) -> Result<(), StoreError> {
        let mut inner = self.lock()?;
        if let Some((holder, until)) = inner.leases.get(&id)
            && holder != owner
            && *until > now
        {
            return Err(StoreError::Locked {
                session: id,
                owner: holder.clone(),
            });
        }
        inner
            .leases
            .insert(id, (owner.to_owned(), lease_until(now, ttl)));
        Ok(())
    }

    async fn release(&self, id: SessionId, owner: &str) -> Result<(), StoreError> {
        let mut inner = self.lock()?;
        if inner
            .leases
            .get(&id)
            .is_some_and(|(holder, _)| holder == owner)
        {
            inner.leases.remove(&id);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Event, SCHEMA_VERSION, Timestamp};
    use crate::ids::RunId;

    fn envelope(seq: u64) -> Envelope {
        Envelope {
            seq,
            run_id: RunId::from_parts(1, 1),
            ts: Timestamp(seq),
            schema: SCHEMA_VERSION,
            event: Event::ModelRequested { step: 1 },
        }
    }

    #[tokio::test]
    async fn unknown_session_is_empty() {
        assert!(
            MemoryStore::new()
                .load(SessionId::new())
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn append_then_load_keeps_order() {
        let (store, id) = (MemoryStore::new(), SessionId::new());
        store.append(id, &[envelope(0), envelope(1)]).await.unwrap();
        store.append(id, &[envelope(2)]).await.unwrap();
        let seqs: Vec<u64> = store
            .load(id)
            .await
            .unwrap()
            .iter()
            .map(|e| e.seq)
            .collect();
        assert_eq!(seqs, vec![0, 1, 2]);
    }

    #[tokio::test]
    async fn gaps_and_duplicates_conflict_and_write_nothing() {
        let (store, id) = (MemoryStore::new(), SessionId::new());
        store.append(id, &[envelope(0)]).await.unwrap();
        let duplicate = store.append(id, &[envelope(0)]).await;
        assert!(matches!(
            duplicate,
            Err(StoreError::Conflict { seq: 0, .. })
        ));
        let gap = store.append(id, &[envelope(1), envelope(3)]).await;
        assert!(matches!(gap, Err(StoreError::Conflict { seq: 3, .. })));
        assert_eq!(store.load(id).await.unwrap().len(), 1);
    }
}
