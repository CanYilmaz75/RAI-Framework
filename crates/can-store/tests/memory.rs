//! Das Gedächtnis in SQLite und PostgreSQL verhält sich wie das im Arbeitsspeicher.
#![cfg(any(feature = "sqlite", feature = "postgres"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use can_core::{InMemoryBank, MemoryBank, SessionId, Timestamp};

const KEY: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

/// Gemeinsamer Ablauf für jede Ablage; eigene Namensräume, damit Läufe sich nicht stören.
async fn conforms(bank: &dyn MemoryBank) {
    let (a, b) = (
        format!("a-{}", SessionId::new()),
        format!("b-{}", SessionId::new()),
    );
    let tea = bank
        .add(&a, "Frau Schulz trinkt grünen Tee", Timestamp(1))
        .await
        .unwrap();
    bank.add(&a, "Der Server steht in Frankfurt", Timestamp(2))
        .await
        .unwrap();
    bank.add(&b, "Herr Kaya trinkt Tee", Timestamp(3))
        .await
        .unwrap();
    let all = bank.all(&a).await.unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].id, tea);
    assert_eq!(all[0].created_at, Timestamp(1));
    let found = bank.search(&a, "Was trinkt Frau Schulz?", 5).await.unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].text, "Frau Schulz trinkt grünen Tee");
    assert!(bank.search(&a, "Pinguin", 5).await.unwrap().is_empty());
    assert!(!bank.delete(&b, &tea).await.unwrap());
    assert!(bank.delete(&a, &tea).await.unwrap());
    assert_eq!(bank.all(&a).await.unwrap().len(), 1);
    assert_eq!(bank.all(&b).await.unwrap().len(), 1);
}

#[tokio::test]
async fn in_memory_bank_conforms() {
    conforms(&InMemoryBank::new()).await;
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_bank_conforms_and_seals_text() {
    use can_store::{Cipher, SqliteStore};
    conforms(&SqliteStore::in_memory().unwrap()).await;
    let path = std::env::temp_dir().join(format!("can-memory-{}.db", SessionId::new()));
    let store = SqliteStore::open(&path)
        .unwrap()
        .with_cipher(Cipher::from_hex(KEY).unwrap());
    conforms(&store).await;
    store
        .add("geheim", "Kennwort-Hinweis: Blume", Timestamp(5))
        .await
        .unwrap();
    let raw: String = rusqlite::Connection::open(&path)
        .unwrap()
        .query_row(
            "SELECT text FROM memories WHERE namespace = 'geheim'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!raw.contains("Blume"));
    assert_eq!(store.search("geheim", "blume", 1).await.unwrap().len(), 1);
    let _ = std::fs::remove_file(&path);
}

/// Läuft nur, wenn `CAN_TEST_POSTGRES_URL` auf eine erreichbare Datenbank zeigt.
#[cfg(feature = "postgres")]
#[tokio::test]
async fn postgres_bank_conforms() {
    let Some(url) = std::env::var("CAN_TEST_POSTGRES_URL")
        .ok()
        .filter(|url| !url.is_empty())
    else {
        eprintln!("CAN_TEST_POSTGRES_URL ist nicht gesetzt: PostgreSQL-Test übersprungen");
        return;
    };
    let store = can_store::PostgresStore::connect(&url).await.unwrap();
    conforms(&store).await;
}
