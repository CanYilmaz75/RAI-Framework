//! Verschlüsselung im Ruhezustand und Erkennen nachträglicher Änderungen am Log.
#![cfg(feature = "sqlite")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use can_core::{
    Digest, Envelope, Event, Message, RunId, SCHEMA_VERSION, SessionId, Store, StoreError,
    Timestamp,
};
use can_store::{Cipher, MemoryStore, SqliteStore};

const KEY: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

fn secret_event(seq: u64) -> Envelope {
    Envelope {
        seq,
        run_id: RunId::from_parts(1, 1),
        ts: Timestamp(seq),
        schema: SCHEMA_VERSION,
        event: Event::RunStarted {
            input: Message::user(format!("streng geheim {seq}")),
            config_digest: Digest::of(b""),
        },
    }
}

fn temp_db() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("can-seal-{}.db", SessionId::new()))
}

fn cleanup(path: &std::path::Path) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

#[tokio::test]
async fn encrypted_sqlite_file_contains_no_plaintext() {
    let path = temp_db();
    let session = SessionId::new();
    let cipher = || Cipher::from_hex(KEY).unwrap();
    let store = SqliteStore::open(&path).unwrap().with_cipher(cipher());
    store
        .append(session, &[secret_event(0), secret_event(1)])
        .await
        .unwrap();
    let digest = store
        .put_blob(b"geheime grosse ausgabe".to_vec())
        .await
        .unwrap();
    assert_eq!(
        digest,
        Digest::of(b"geheime grosse ausgabe"),
        "die Adresse gilt dem Klartext"
    );
    drop(store);

    let mut raw = Vec::new();
    for suffix in ["", "-wal"] {
        raw.extend(std::fs::read(format!("{}{suffix}", path.display())).unwrap_or_default());
    }
    let raw = String::from_utf8_lossy(&raw);
    assert!(
        !raw.contains("streng geheim") && !raw.contains("geheime grosse"),
        "Klartext in der Datei"
    );

    let reopened = SqliteStore::open(&path).unwrap().with_cipher(cipher());
    assert_eq!(
        reopened.load(session).await.unwrap(),
        vec![secret_event(0), secret_event(1)]
    );
    assert_eq!(
        reopened.get_blob(&digest).await.unwrap(),
        Some(b"geheime grosse ausgabe".to_vec())
    );
    assert_eq!(reopened.verify(session).await.unwrap(), None);

    // Ohne oder mit falschem Schlüssel ist nichts lesbar.
    let without = SqliteStore::open(&path).unwrap();
    assert!(
        matches!(without.load(session).await, Err(StoreError::Corrupt(m)) if m.contains("kein Schlüssel"))
    );
    let wrong = SqliteStore::open(&path)
        .unwrap()
        .with_cipher(Cipher::from_hex(&KEY.replace("00", "ff")).unwrap());
    assert!(matches!(
        wrong.load(session).await,
        Err(StoreError::Corrupt(_))
    ));
    cleanup(&path);
}

#[tokio::test]
async fn tampering_with_the_sqlite_log_is_detected() {
    let path = temp_db();
    let session = SessionId::new();
    let store = SqliteStore::open(&path).unwrap();
    store
        .append(session, &[secret_event(0), secret_event(1)])
        .await
        .unwrap();
    store.append(session, &[secret_event(2)]).await.unwrap();
    assert_eq!(store.verify(session).await.unwrap(), None);
    assert_eq!(
        store.verify(SessionId::new()).await.unwrap(),
        None,
        "leere Sitzung ist unversehrt"
    );

    let raw = rusqlite::Connection::open(&path).unwrap();
    let change =
        "UPDATE events SET payload = replace(payload, 'geheim 1', 'harmlos 1') WHERE seq = 1";
    assert_eq!(raw.execute(change, []).unwrap(), 1);
    assert_eq!(
        store.verify(session).await.unwrap(),
        Some(1),
        "geänderter Eintrag"
    );

    let other = SessionId::new();
    store
        .append(other, &[secret_event(0), secret_event(1), secret_event(2)])
        .await
        .unwrap();
    raw.execute(
        "DELETE FROM events WHERE session_id = ?1 AND seq = 1",
        [other.to_string()],
    )
    .unwrap();
    assert_eq!(
        store.verify(other).await.unwrap(),
        Some(2),
        "gelöschter Eintrag"
    );
    cleanup(&path);
}

#[tokio::test]
async fn stores_without_a_chain_report_intact() {
    let store = MemoryStore::new();
    let session = SessionId::new();
    store.append(session, &[secret_event(0)]).await.unwrap();
    assert_eq!(store.verify(session).await.unwrap(), None);
}

/// Läuft nur, wenn `CAN_TEST_POSTGRES_URL` auf eine erreichbare Datenbank zeigt.
#[cfg(feature = "postgres")]
#[tokio::test]
async fn postgres_encrypts_and_detects_tampering() {
    use can_store::PostgresStore;
    let Ok(url) = std::env::var("CAN_TEST_POSTGRES_URL") else {
        eprintln!("CAN_TEST_POSTGRES_URL ist nicht gesetzt: PostgreSQL-Test übersprungen");
        return;
    };
    let session = SessionId::new();
    let store = PostgresStore::connect(&url)
        .await
        .unwrap()
        .with_cipher(Cipher::from_hex(KEY).unwrap());
    store
        .append(session, &[secret_event(0), secret_event(1)])
        .await
        .unwrap();
    store.append(session, &[secret_event(2)]).await.unwrap();
    let digest = store
        .put_blob(b"geheime grosse ausgabe".to_vec())
        .await
        .unwrap();
    assert_eq!(store.load(session).await.unwrap().len(), 3);
    assert_eq!(
        store.get_blob(&digest).await.unwrap(),
        Some(b"geheime grosse ausgabe".to_vec())
    );
    assert_eq!(store.verify(session).await.unwrap(), None);

    let (raw, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(connection);
    let id = session.to_string();
    let row = raw
        .query_one(
            "SELECT payload FROM can_events WHERE session_id = $1 AND seq = 0",
            &[&id],
        )
        .await
        .unwrap();
    let payload: String = row.get(0);
    assert!(payload.starts_with("enc1:") && !payload.contains("geheim"));
    let plain = PostgresStore::connect(&url).await.unwrap();
    assert!(matches!(
        plain.load(session).await,
        Err(StoreError::Corrupt(_))
    ));

    // Zwei Einträge vertauschen: beide bleiben entschlüsselbar, aber die Kette bricht.
    raw.batch_execute(&format!(
        "UPDATE can_events SET payload = (SELECT payload FROM can_events WHERE session_id = '{id}' AND seq = 2) \
         WHERE session_id = '{id}' AND seq = 1"
    ))
    .await
    .unwrap();
    assert_eq!(store.verify(session).await.unwrap(), Some(1));
}
