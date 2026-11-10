//! Store-Trait: ein reines Event-Log pro Sitzung, es wird nur angehängt.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::event::Envelope;
use crate::ids::SessionId;
use crate::provider::BoxError;

/// Fehler eines Stores.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Die Position ist schon belegt oder nicht die nächste: ein anderer Schreiber besitzt die Sitzung.
    #[error("Schreibkonflikt in Sitzung {session} an Position {seq}")]
    Conflict { session: SessionId, seq: u64 },
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
}

/// Store im Arbeitsspeicher, für Tests und eingebettete Nutzung.
#[derive(Debug, Default)]
pub struct MemoryStore {
    sessions: Mutex<HashMap<SessionId, Vec<Envelope>>>,
}

impl MemoryStore {
    /// Leerer Store.
    pub fn new() -> Self {
        Self::default()
    }
}

fn poisoned() -> StoreError {
    StoreError::Backend("Sperre des MemoryStore ist vergiftet".into())
}

#[async_trait]
impl Store for MemoryStore {
    async fn load(&self, id: SessionId) -> Result<Vec<Envelope>, StoreError> {
        let sessions = self.sessions.lock().map_err(|_| poisoned())?;
        Ok(sessions.get(&id).cloned().unwrap_or_default())
    }

    async fn append(&self, id: SessionId, events: &[Envelope]) -> Result<(), StoreError> {
        let mut sessions = self.sessions.lock().map_err(|_| poisoned())?;
        let log = sessions.entry(id).or_default();
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
