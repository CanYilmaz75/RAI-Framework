# Changelog

## Unveröffentlicht

### Isolation und Sicherheit

- Sandbox für `shell` über Namespaces oder Container; Arbeitsverzeichnis pro Sitzung
- Schutz vor Datenabfluss nach vertraulichen Tool-Ausgaben; Inhaltsfilter für Ein- und Ausgaben
- Verschlüsselung im Ruhezustand und Hash-Kette in SQLite und PostgreSQL, Prüf-Endpunkt
- Secrets aus Umgebung, Dateien oder Vault; Bearer-Token für MCP über HTTP
- Server-Konfiguration: Sandbox, MCP-Server, vertrauliche Tools, gesperrte Begriffe
- Neue Abhängigkeit: `chacha20poly1305`

### Dienst

- `can-server`: HTTP-API für Sitzungen, Nachrichten, Event-Strom mit Wiederaufsetzen, Freigaben,
  Abbruch und Fortsetzen; API-Schlüssel, Rollen, Mandantentrennung, Kontingente,
  Idempotenz-Schlüssel, Prometheus-Kennzahlen, Web-Oberfläche
- Übernahme unterbrochener Läufe durch einen anderen Worker
- Sitzungsverzeichnis in PostgreSQL, `Dockerfile`, CI-Workflow mit PostgreSQL

### Fundament für den Betrieb

- `Digest` ist SHA-256 statt FNV
- Secrets werden aus Events, Log, Kanal und Modellanfragen geschwärzt
- Bricht eine Modellantwort mittendrin ab, wird der Schritt bis zu zweimal neu angefragt
- `shell` beendet beim Abbruch die ganze Prozessgruppe und behält höchstens 1 MiB je Ausgabestrom;
  der SSE-Parser verwirft überlange Zeilen
- Große Tool-Ausgaben liegen vollständig als Blob im Store
- Sitzungssperre mit Heartbeat wird erzwungen
- `PostgresStore` für viele gleichzeitige Schreiber
- Neue Abhängigkeiten: `sha2`, `tokio-postgres`, `axum`

### Umbenennung

- Das Projekt heißt jetzt Can: Crates `can-*`, Binary `can`, Umgebungsvariablen `CAN_*`,
  Datenbank `can.db`

### Phase 5: Kontext und Parallelität

- Kompaktierung ab 80 % des Kontextfensters und nach `ContextTooLong`, mit Vorstufe, die alte
  Tool-Ausgaben durch eine Vorschau ersetzt; Event `Compacted`, das Log bleibt vollständig
- Lesende Tools einer Antwort laufen parallel (höchstens vier), Ergebnisse in Aufrufreihenfolge
- `SubAgent`: ein Agent als Tool mit eigenem Budget und eigener Policy
- Abbruch gibt laufenden Tools fünf Sekunden zum Aufräumen
- `Retry`: Wiederholung bei Rate-Limit, Überlast und Netzwerkfehlern mit Backoff und Jitter

### Phase 4: MCP und zweiter Provider

- MCP-Client über stdio und HTTP; Tools heißen `<server>__<tool>`; Beispielserver `can-mcp-echo`
- Anthropic-Provider hinter dem Feature `anthropic`
- Gemeinsame Provider-Testsuite gegen einen lokalen Server
- Bilder in Nutzernachrichten für beide Provider, `can chat --image`
- Tool `http_fetch` mit Host-Freigabeliste und Sperre privater Adressen
- Konfigurationsdatei `can.toml`: Provider, Modell, Limits, Regeln, HTTP-Hosts, MCP-Server

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
