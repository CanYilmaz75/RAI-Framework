//! Schema und Migrationen. Die Version steht in `PRAGMA user_version`.

use rusqlite::Connection;

/// Jede Migration hebt die Version um eins; neue kommen ans Ende.
const MIGRATIONS: &[&str] = &[
    "
CREATE TABLE sessions (
  id          TEXT PRIMARY KEY,
  created_at  INTEGER NOT NULL,
  config_json TEXT NOT NULL,
  status      TEXT NOT NULL,      -- running, finished, failed
  owner       TEXT,               -- Schreiber, der die Sitzung gerade führt
  heartbeat   INTEGER             -- Ablauf seiner Sperre in Millisekunden
);
CREATE TABLE events (
  session_id  TEXT NOT NULL REFERENCES sessions(id),
  seq         INTEGER NOT NULL,
  ts          INTEGER NOT NULL,
  schema      INTEGER NOT NULL,
  kind        TEXT NOT NULL,
  payload     TEXT NOT NULL,      -- JSON
  PRIMARY KEY (session_id, seq)
);
CREATE TABLE blobs (
  digest TEXT PRIMARY KEY,        -- Inhaltsadresse
  bytes  BLOB NOT NULL
);
",
    "ALTER TABLE events ADD COLUMN chain TEXT;",
];

/// Stellt WAL-Modus und volle Synchronisierung ein und bringt das Schema auf den neuesten Stand.
pub(crate) fn prepare(connection: &mut Connection) -> rusqlite::Result<()> {
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    let version: usize = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    for (index, migration) in MIGRATIONS.iter().enumerate().skip(version) {
        let transaction = connection.transaction()?;
        transaction.execute_batch(migration)?;
        transaction.pragma_update(None, "user_version", index + 1)?;
        transaction.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_creates_tables_and_is_repeatable() {
        let mut connection = Connection::open_in_memory().unwrap();
        prepare(&mut connection).unwrap();
        prepare(&mut connection).unwrap();
        let version: usize = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len());
        let tables: usize = connection
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name IN ('sessions', 'events', 'blobs')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 3);
    }
}
