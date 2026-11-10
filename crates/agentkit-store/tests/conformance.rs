//! Gemeinsame Testsuite: `MemoryStore` und `SqliteStore` verhalten sich gleich.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use agentkit_core::{
    Digest, Envelope, Event, FinishReason, MemoryStore, Message, RunId, SCHEMA_VERSION, SessionId,
    Store, StoreError, Timestamp,
};

fn envelope(seq: u64, event: Event) -> Envelope {
    Envelope {
        seq,
        run_id: RunId::from_parts(1, 1),
        ts: Timestamp(1_000 + seq),
        schema: SCHEMA_VERSION,
        event,
    }
}

fn requested(seq: u64) -> Envelope {
    envelope(seq, Event::ModelRequested { step: seq as u32 })
}

/// Was jeder Store können muss.
async fn conformance(store: &dyn Store) {
    let (a, b) = (SessionId::new(), SessionId::new());
    assert!(
        store.load(a).await.unwrap().is_empty(),
        "unbekannte Sitzung ist leer"
    );
    store.append(a, &[]).await.unwrap();

    let start = envelope(
        0,
        Event::RunStarted {
            input: Message::user("grüße 🎉"),
            config_digest: Digest::of(b"c"),
        },
    );
    store
        .append(a, &[start.clone(), requested(1)])
        .await
        .unwrap();
    store.append(a, &[requested(2)]).await.unwrap();
    store.append(b, &[requested(0)]).await.unwrap();
    assert_eq!(
        store.load(a).await.unwrap(),
        vec![start, requested(1), requested(2)]
    );
    assert_eq!(
        store.load(b).await.unwrap().len(),
        1,
        "Sitzungen sind getrennt"
    );

    // Belegte Position, Lücke und falscher Anfang: Konflikt, und nichts wird geschrieben.
    assert!(matches!(
        store.append(a, &[requested(2)]).await,
        Err(StoreError::Conflict { seq: 2, .. })
    ));
    assert!(matches!(
        store.append(a, &[requested(4)]).await,
        Err(StoreError::Conflict { seq: 4, .. })
    ));
    assert!(matches!(
        store.append(a, &[requested(3), requested(5)]).await,
        Err(StoreError::Conflict { seq: 5, .. })
    ));
    assert!(matches!(
        store.append(SessionId::new(), &[requested(1)]).await,
        Err(StoreError::Conflict { .. })
    ));
    assert_eq!(store.load(a).await.unwrap().len(), 3);

    store
        .append(
            a,
            &[envelope(
                3,
                Event::RunFinished {
                    reason: FinishReason::Completed,
                },
            )],
        )
        .await
        .unwrap();
    assert_eq!(store.load(a).await.unwrap().len(), 4);
}

#[tokio::test]
async fn memory_store_conforms() {
    conformance(&MemoryStore::new()).await;
}

#[cfg(feature = "sqlite")]
mod sqlite {
    use super::*;
    use agentkit_store::SqliteStore;
    use std::path::PathBuf;

    fn temp_db() -> PathBuf {
        std::env::temp_dir().join(format!("agentkit-store-test-{}.db", SessionId::new()))
    }

    fn cleanup(path: &std::path::Path) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[tokio::test]
    async fn sqlite_store_conforms_in_memory_and_on_disk() {
        conformance(&SqliteStore::in_memory().unwrap()).await;
        let path = temp_db();
        conformance(&SqliteStore::open(&path).unwrap()).await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn data_survives_reopening_and_two_writers_conflict() {
        let path = temp_db();
        let session = SessionId::new();
        let first = SqliteStore::open(&path).unwrap();
        first
            .append(session, &[requested(0), requested(1)])
            .await
            .unwrap();

        // Zweiter Schreiber auf derselben Datei: sieht den Stand und verliert das Rennen um Position 2.
        let second = SqliteStore::open(&path).unwrap();
        assert_eq!(second.load(session).await.unwrap().len(), 2);
        first.append(session, &[requested(2)]).await.unwrap();
        assert!(matches!(
            second.append(session, &[requested(2)]).await,
            Err(StoreError::Conflict { seq: 2, .. })
        ));
        drop((first, second));

        let reopened = SqliteStore::open(&path).unwrap();
        assert_eq!(
            reopened.load(session).await.unwrap(),
            vec![requested(0), requested(1), requested(2)]
        );
        cleanup(&path);
    }

    #[tokio::test]
    async fn sessions_report_status_and_event_count() {
        let store = SqliteStore::in_memory().unwrap();
        let (done, failed, open) = (
            SessionId::from_parts(1, 1),
            SessionId::from_parts(2, 1),
            SessionId::from_parts(3, 1),
        );
        store
            .append(
                done,
                &[
                    requested(0),
                    envelope(
                        1,
                        Event::RunFinished {
                            reason: FinishReason::Completed,
                        },
                    ),
                ],
            )
            .await
            .unwrap();
        let error = FinishReason::Error {
            message: "x".into(),
        };
        store
            .append(failed, &[envelope(0, Event::RunFinished { reason: error })])
            .await
            .unwrap();
        store.append(open, &[requested(0)]).await.unwrap();
        let sessions = store.sessions().await.unwrap();
        let summary: Vec<(SessionId, &str, u64)> = sessions
            .iter()
            .map(|s| (s.id, s.status.as_str(), s.events))
            .collect();
        assert_eq!(
            summary,
            vec![
                (open, "running", 1),
                (failed, "failed", 1),
                (done, "finished", 2)
            ]
        );
    }

    #[tokio::test]
    async fn blobs_are_content_addressed() {
        let store = SqliteStore::in_memory().unwrap();
        let digest = store.put_blob(b"grosse ausgabe".to_vec()).await.unwrap();
        assert_eq!(digest, Digest::of(b"grosse ausgabe"));
        assert_eq!(
            store.put_blob(b"grosse ausgabe".to_vec()).await.unwrap(),
            digest
        );
        assert_eq!(
            store.get_blob(digest).await.unwrap(),
            Some(b"grosse ausgabe".to_vec())
        );
        assert_eq!(store.get_blob(Digest::of(b"fehlt")).await.unwrap(), None);
    }

    #[tokio::test]
    async fn newer_schema_versions_are_refused_not_misread() {
        let store = SqliteStore::in_memory().unwrap();
        let session = SessionId::new();
        let mut future = requested(0);
        future.schema = SCHEMA_VERSION + 1;
        store.append(session, &[future]).await.unwrap();
        assert!(
            matches!(store.load(session).await, Err(StoreError::Corrupt(m)) if m.contains("neuer"))
        );
    }
}
