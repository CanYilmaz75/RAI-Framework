# Changelog

## Unveröffentlicht

### Phase 3: Speicher, Fortsetzen, Record/Replay

- `can-store`: `SqliteStore` mit Schema für Sitzungen, Events und Blobs, Migrationen beim
  Öffnen, WAL und `synchronous = FULL`; gemeinsame Testsuite mit `MemoryStore`
- `Agent::resume` und `can resume <id>` setzen einen unterbrochenen Lauf fort; ein nicht
  idempotentes Tool läuft dabei nie doppelt
- `Recording` schreibt Modellantworten in eine Datei, `FakeProvider::from_recording` spielt sie ab
- `can chat` speichert Sitzungen in SQLite (`--db`, `--session`, `--record`)
- Test, der den Lauf nach jedem einzelnen Event abbricht und fortsetzt

### Phase 2: Tool-Makro und Policy

- `can-macros`: `#[tool]` leitet Name, Beschreibung und JSON-Schema aus einer `async fn` ab
- Argumente werden vor der Policy gegen das Schema geprüft; ungültige gehen als Tool-Fehler
  mit Feldpfad ans Modell zurück
- `RuleSet`: Regeln pro Tool-Name, Wirkung und Argumentmuster, die strengste gewinnt
- `can-tools`: `write_file` und `shell` (direkter Programmstart, bereinigte Umgebung)
- `can-cli`: fragt bei `Ask` im Terminal nach

### Phase 1: Kern und erster Lauf

- Workspace mit sieben Crates, `rust-toolchain.toml` und `scripts/check.sh`
- `can-core`: Datenmodell, `RunState::apply`, Traits `Provider`, `Tool`, `Store`, `Policy`,
  `Clock`, dazu `MemoryStore`, `ManualClock` und der Chunk-Assembler
- `can-providers`: `FakeProvider` und OpenAI-kompatibler Provider mit eigenem SSE-Parser
- `can-runtime`: reine Schrittfunktion, Treiber, `Agent`-Builder, Limits für Schritte,
  Token, Zeit und Tool-Aufrufe, Abbruch, Freigaben über `Run::resolve`
- `can-tools`: `read_file`, begrenzt auf das Arbeitsverzeichnis
- `can-cli`: `can chat` streamt die Antwort ins Terminal
