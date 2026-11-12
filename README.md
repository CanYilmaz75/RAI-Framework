# Can

Can ist ein eigenständiges AI-Agent-Framework in Rust: ein Cargo-Workspace ohne fremde
Agent-Frameworks. Es ist für alle drei Einsatzarten gedacht: als Terminal-Werkzeug (`can`), als
Dienst für viele Nutzer (`can-server`) und eingebettet in andere Programme (Crate `can`).

Der Kern ist eine reine Schrittfunktion ohne Ein- und Ausgabe. Alles, was in einem Lauf passiert,
ist ein Event; der Zustand entsteht nur durch Falten der Event-Folge. Jeder Tool-Aufruf läuft
durch eine Policy.

## Stand

Phase 1 bis 6 des Bauplans sind umgesetzt, dazu das Fundament für den Betrieb als Dienst und ein
erster HTTP-Server. Phase 6 (WASM-Sandbox) und der weitere Enterprise-Ausbau sind offen; der Stand
steht unter „Dienst“ weiter unten.

| Crate | Aufgabe |
| --- | --- |
| `can-core` | Typen (`Message`, `Event`, `RunState`), die Traits `Provider`, `Tool`, `Store`, `Policy`, `Clock`, dazu `RuleSet` und die Schema-Prüfung. Kein Netzwerk. |
| `can-providers` | OpenAI-kompatibel (auch Azure, Ollama und weitere), Anthropic, Gemini, Vertex AI, Bedrock; `Retry`, `Fallback`, `Recording`, `FakeProvider` |
| `can-runtime` | Schrittfunktion `step`, Treiber, `Agent`-Builder, Limits, Abbruch, Kompaktierung, parallele Tools, `SubAgent` |
| `can-tools` | `read_file`, `write_file`, `shell`, `http_fetch` (Feature `http`), MCP-Client (Feature `mcp`) |
| `can-store` | `MemoryStore`, `SqliteStore` (Feature `sqlite`) und `PostgresStore` (Feature `postgres`), mit Blobs und Sitzungssperre |
| `can-macros` | `#[tool]` macht aus einer `async fn` ein Tool samt JSON-Schema |
| `can-cli` | Binary `can` mit den Befehlen `chat` und `resume` |
| `can` | Fassade: die öffentliche API in einem Crate |
| `can-server` | Binary `can-server`: HTTP-API, Event-Strom, Freigaben, Mandanten und Rollen, Web-Oberfläche |

## Ausprobieren

Ohne Netzwerk und ohne Schlüssel, mit dem Fake-Provider:

```bash
cargo run -p can-runtime --example minimal
```

```bash
cargo run -p can-runtime --example policy
```

```bash
cargo run -p can-runtime --example resume
```

Mit einem echten Modell über eine OpenAI-kompatible API (auch Ollama):

```bash
export CAN_BASE_URL=http://localhost:11434/v1   # Vorgabe: https://api.openai.com/v1
export CAN_API_KEY=...                          # für Ollama nicht nötig
export CAN_MODEL=mein-modell
cargo run -p can-cli -- chat "Fasse README.md zusammen"
```

Mit Anthropic:

```bash
export ANTHROPIC_API_KEY=...
cargo run -p can-cli -- chat --provider anthropic --model mein-modell "Fasse README.md zusammen"
```

Ohne Frage startet `can chat` ein Gespräch, das Zeile für Zeile von der Standardeingabe liest.
Lesen ist erlaubt; vor jedem Schreiben, vor `shell`, vor HTTP-Abrufen und vor MCP-Tools fragt die
CLI im Terminal nach, solange keine Regel etwas anderes sagt. Mit `--image bild.png` liegt der
ersten Frage ein Bild bei.

## Dienst

`can-server` stellt die Laufzeit als HTTP-Dienst für mehrere Mandanten bereit. Eine Vorlage für die
Konfiguration steht in [`can-server.example.toml`](can-server.example.toml).

```bash
export CAN_DATABASE_URL=postgres://can@localhost:5432/can
export ACME_ADA_KEY=ein-langer-zufaelliger-schluessel
cargo run -p can-server -- can-server.toml
```

Die Oberfläche liegt unter `/`, Kennzahlen im Prometheus-Format unter `/metrics`, der
Health-Check unter `/healthz`. Alles unter `/v1` verlangt `Authorization: Bearer <schlüssel>`.

| Aufruf | Wirkung |
| --- | --- |
| `POST /v1/sessions` | Sitzung anlegen und Lauf starten; mit `Idempotency-Key` startet ein doppelter Aufruf keinen zweiten Lauf |
| `POST /v1/sessions/{id}/messages` | nächste Nachricht in derselben Sitzung |
| `GET /v1/sessions/{id}/events` | Event-Strom (SSE); mit `?after=` oder `Last-Event-ID` ab einer Position |
| `GET /v1/sessions/{id}/log` | gespeicherte Events als JSON |
| `GET /v1/approvals` | offene Rückfragen des Mandanten |
| `GET /v1/memories?query=` | Gedächtnis des Mandanten durchsuchen; `POST` ergänzt, `DELETE /v1/memories/{id}` löscht |
| `GET /v1/schedules` | Zeitpläne des Mandanten mit nächster und letzter Ausführung |
| `DELETE /v1/sessions/{id}` | Sitzung samt Log löschen (Rolle `admin`) |
| `POST /v1/sessions/{id}/fork` | Sitzung ab einer Position verzweigen |
| `GET /openapi.json` | Beschreibung der API |
| `POST /v1/sessions/{id}/approvals/{call_id}` | Rückfrage mit `allow` oder `deny` beantworten; mit `input` laufen geänderte Argumente |
| `POST /v1/sessions/{id}/cancel`, `.../resume` | Lauf abbrechen oder fortsetzen |

- **Mandanten:** Jede Sitzung gehört einem Mandanten; fremde Sitzungen sehen aus wie nicht vorhanden.
- **Rollen:** `viewer` liest, `operator` startet Läufe und erteilt Freigaben, `admin` darf alles im Mandanten.
- **Kontingente:** gleichzeitige Läufe, Token- und Kostenbudget pro Mandant; darüber antwortet der Dienst mit 429.
- **Webhook:** Nach jedem Lauf geht eine Meldung mit Grund, Token und Kosten an die Adresse des Mandanten.
- **Mehrere Worker:** Mit PostgreSQL teilen sich Worker Store und Sitzungsverzeichnis. Fällt einer aus,
  übernimmt ein anderer dessen Läufe, sobald die Sitzungssperre abgelaufen ist (30 Sekunden).
- **Container:** Das `Dockerfile` baut ein Image mit `can-server` und `can`.
- **Nachrichten während eines Laufs:** `POST /v1/sessions/{id}/messages` nimmt `mode` an:
  `reject` (Vorgabe, 409), `queue` (startet nach dem laufenden Lauf), `inject` (geht vor der
  nächsten Modellanfrage in den laufenden Lauf) oder `interrupt` (bricht ab und beginnt neu).
- **Gedächtnis:** Mit `memory = true` bekommt ein Agent die Tools `remember`, `recall` und
  `forget`. Einträge liegen im Store (verschlüsselt wie die Events) und gehören dem Mandanten;
  die Suche gewichtet Stichworte nach TF-IDF.
- **Zeitpläne:** `[[schedules]]` startet Läufe nach einem Cron-Ausdruck (fünf Felder, UTC, oder
  `@daily` usw.), jede Auslösung in einer neuen Sitzung. Bei mehreren Workern startet nur einer;
  verpasste Zeitpunkte werden nicht nachgeholt.
- **Hintergrund-Agenten:** `background_agents = ["recherche"]` gibt einem Agenten Tools, mit denen
  er einen weiter oben definierten Agenten in eigener Sitzung starten, nach dem Stand fragen, ihm
  Nachrichten schicken und ihn abbrechen kann; `subagents` wartet dagegen auf die Antwort.

### Isolation und Sicherheit

- **Arbeitsverzeichnis pro Sitzung:** Im Dienst arbeitet jede Sitzung in einem eigenen Unterverzeichnis.
- **Sandbox für `shell`:** `namespaces` (über `unshare`: kein Netzwerk, eigene Prozesstabelle,
  Grenzen für Speicher, CPU-Zeit und Prozesse) oder `docker` (Wegwerf-Container, nur das
  Arbeitsverzeichnis eingehängt, Wurzel schreibgeschützt, keine Capabilities). `namespaces` trennt
  das Dateisystem nicht; dafür ist `docker` gedacht.
- **Schutz vor Datenabfluss:** Tools lassen sich als vertraulich markieren. Hat eine Sitzung
  solche Daten gesehen, gilt für jedes externe Tool mindestens `ask` oder `deny`.
- **Inhaltsfilter:** gesperrte Begriffe in Eingaben und Modellantworten; hinter dem Trait
  `Moderator` lässt sich ein externer Dienst anschließen.
- **Verschlüsselung im Ruhezustand:** Events und Blobs in SQLite und PostgreSQL, ChaCha20-Poly1305.
- **Manipulationssicheres Protokoll:** Jede Zeile trägt ein Glied einer Hash-Kette;
  `GET /v1/sessions/{id}/verify` meldet die erste veränderte Position.
- **Secrets:** aus Umgebungsvariablen, eingehängten Dateien oder HashiCorp Vault (KV v2).
- **MCP:** Server pro Agent in der Konfiguration, HTTP-Server mit Bearer-Token.

Was dem Dienst noch fehlt: Anmeldung über SSO, ein Netzwerk-Proxy für ausgehende Verbindungen,
ein Schlüssel pro Mandant, TLS zur Datenbank und ein Verbindungspool. Token- und Kostenbudget zählen pro
Prozess und beginnt nach einem Neustart bei null. Die Docker-Sandbox ist geschrieben, aber
mangels Docker in der Entwicklungsumgebung nicht ausgeführt worden.

## Anbieter

| `kind` | Anbieter | Was zusätzlich nötig ist |
| --- | --- | --- |
| `openai` (Vorgabe) | OpenAI oder jede kompatible Adresse über `base_url` | `OPENAI_API_KEY` |
| `anthropic` | Anthropic API | `ANTHROPIC_API_KEY` |
| `azure` | Azure OpenAI | `base_url`, `deployment`, `AZURE_OPENAI_API_KEY` |
| `gemini` | Gemini API | `GEMINI_API_KEY` |
| `vertex` | Gemini auf Vertex AI | `project`, `location`; Token aus `GOOGLE_CLOUD_ACCESS_TOKEN` oder `gcloud auth print-access-token` |
| `vertex_anthropic` | Claude auf Vertex AI | wie `vertex` |
| `bedrock` | Amazon Bedrock (Converse-API) | `region`, Bedrock-API-Schlüssel in `AWS_BEARER_TOKEN_BEDROCK` |
| `ollama`, `openrouter`, `groq`, `mistral`, `deepseek`, `xai`, `together`, `fireworks`, `perplexity` | OpenAI-kompatible Anbieter mit fester Adresse | der Schlüssel des Anbieters, etwa `GROQ_API_KEY`; Ollama braucht keinen |

```bash
cargo run -p can-cli -- chat --provider groq --model llama-3.3-70b-versatile "Hallo"
```

Bedrock liefert die Antwort am Stück statt gestreamt. Gemini-Modelle senden zu Tool-Aufrufen eine
Signatur, die Can bei der nächsten Anfrage unverändert zurückgibt.

## Konfigurationsdatei

`can` liest `can.toml` im aktuellen Verzeichnis oder die Datei hinter `--config`. Eine Vorlage
steht in [`can.example.toml`](can.example.toml). Optionen der Kommandozeile haben Vorrang.

```toml
model = "mein-modell"

[provider]
kind = "anthropic"
api_key_env = "MEIN_SCHLUESSEL"

[[policy.rules]]
tool = "write_file"
args = { path = "notizen/*" }
decision = "allow"

[http]
allowed_hosts = ["example.com"]

[mcp.servers.suche]
url = "http://localhost:8080/mcp"
```

Schlüssel stehen nie in der Datei, nur der Name der Umgebungsvariablen; ein `api_key` in der
Datei ist ein Fehler.

## MCP-Server

Jeder Eintrag unter `[mcp.servers.<name>]` bindet einen Server ein: mit `command` und `args` über
stdio, mit `url` über HTTP. Seine Tools heißen `<name>__<tool>`, gelten als extern und fragen
deshalb nach, bis eine Regel sie erlaubt. Zum Ausprobieren gibt es einen kleinen Server:

```bash
cargo build -p can-tools --features mcp --bin can-mcp-echo
```

## Lange Läufe

Ist das Kontextfenster zu 80 % voll, ersetzt Can zuerst alte, große Tool-Ausgaben durch eine
Vorschau. Reicht das nicht, fasst das Modell die ältesten Runden zusammen; die ursprüngliche
Aufgabe und die letzten vier Runden bleiben wörtlich erhalten, und ein Tool-Aufruf wird nie von
seinem Ergebnis getrennt. Meldet der Anbieter einen Kontextüberlauf, wird verdichtet und der
Schritt wiederholt. Das Event-Log selbst wird nie gekürzt.

Bei Rate-Limit, Überlast und Netzwerkfehlern wiederholt die CLI die Anfrage bis zu fünfmal mit
wachsender Wartezeit, solange noch nichts von der Antwort angekommen ist.

## Parallele Tools und Sub-Agenten

Aufeinanderfolgende lesende Tools einer Modellantwort laufen parallel, höchstens vier
gleichzeitig; schreibende und externe laufen einzeln in der Reihenfolge des Modells. Die
Ergebnisse gehen immer in Aufrufreihenfolge ans Modell.

Ein Sub-Agent ist ein Tool mit eigenem Budget und eigener Policy:

```rust
let expert = Agent::builder().provider(provider).model("klein").tool(ReadFile::new()).build()?;
let agent = Agent::builder()
    .provider(other_provider)
    .model("gross")
    .tool(SubAgent::new("experte", "Recherchiert im Projekt.", expert))
    .build()?;
```

`BackgroundAgents::new("recherche", "…", expert).tools()` liefert stattdessen fünf Tools
(`recherche__start`, `__check`, `__message`, `__cancel`, `__list`): Der Agent arbeitet in eigener
Sitzung weiter, während der aufrufende Lauf fortfährt. Aufgaben leben im Prozess; ihre Sitzungen
bleiben im Store. Mit `Run::inject` kommt eine Nachricht in einen laufenden Lauf, ohne ihn
abzubrechen.

## Speichern, Fortsetzen, Replay

Die CLI schreibt jede Sitzung in eine SQLite-Datei (Vorgabe `can.db`) und gibt beim Start die
Kennung der Sitzung aus. Jedes Event ist gespeichert, bevor die nächste Wirkung nach außen passiert.

```bash
cargo run -p can-cli -- chat --session <kennung> "Und weiter?"
```

```bash
cargo run -p can-cli -- resume <kennung>
```

`resume` setzt einen unterbrochenen Lauf fort. Lief gerade ein Tool, wird es nur wiederholt, wenn
es als idempotent deklariert ist; sonst bekommt das Modell das Ergebnis „Unterbrochen, Ausgang
unbekannt“ und entscheidet selbst. So läuft keine Schreibaktion unbemerkt doppelt.

Mit `--record antworten.jsonl` schreibt `chat` die Modellantworten mit.
`FakeProvider::from_recording("antworten.jsonl")` spielt sie in Tests wieder ab und ergibt dieselbe
Event-Folge.

## Als Bibliothek

```rust
let agent = Agent::builder()
    .provider(Retry::new(OpenAiCompat::from_env()?, Arc::new(SystemClock)))
    .model("mein-modell")
    .system("Du bist ein hilfreicher Assistent.")
    .tool(ReadFile::new())
    .tool(write_file)
    .policy(RuleSet::safe_defaults())
    .limits(Limits { max_steps: 50, ..Limits::default() })
    .build()?;

let mut run = agent.run(SessionId::new(), "Fasse README.md zusammen").await?;
while let Some(envelope) = run.next().await {
    match envelope?.event {
        Event::TextDelta { text, .. } => print!("{text}"),
        Event::ApprovalNeeded { call_id } => run.resolve(call_id, Decision::Allow).await?,
        _ => {}
    }
}
let outcome = run.outcome().await?;
```

`agent.complete(...)` ist die Kurzform: Sie liefert nur das Ergebnis und behandelt jede Rückfrage
als Ablehnung.

## Ein eigenes Tool

```rust
/// Zählt die Wörter eines Textes.
#[tool(effect = "read_only", idempotent)]
async fn count_words(text: String) -> Result<String, ToolError> {
    Ok(text.split_whitespace().count().to_string())
}
```

Name und Beschreibung kommen aus Funktionsname und Doc-Kommentar, das Schema aus den Parametern.
Ohne Attribute gelten die sicheren Vorgaben: `write`, nicht idempotent, 60 Sekunden Zeitlimit.
Argumente des Modells werden vor dem Aufruf gegen das Schema geprüft; Fehler gehen mit Feldpfad
ans Modell zurück.

## Policy

```rust
let policy = RuleSet::safe_defaults()
    .rule(Rule::tool("write_file").arg("path", "notizen/*").decide(Decision::Allow))
    .rule(Rule::tool("write_file").arg("path", "*.env").decide(Decision::Deny));
```

Greifen mehrere Regeln, gewinnt die strengste. Greift keine, fragt der Agent nach; ohne Mensch
(`agent.complete`) wird aus der Rückfrage eine Ablehnung. Eine Ablehnung beendet den Lauf nicht:
Das Modell bekommt sie als Fehlerergebnis.

`http_fetch` ruft nur Hosts der Freigabeliste ab. Private und lokale Adressen sind gesperrt, auch
nach der DNS-Auflösung und nach jeder Weiterleitung.

## Arbeit an Code

Die CLI bringt dafür diese Tools mit; im Dienst wählt man sie pro Agent in der Konfiguration.

| Tool | Wirkung |
| --- | --- |
| `ls`, `glob`, `grep` | Ordner auflisten, Dateien nach Muster finden (`src/**/*.rs`), Text suchen. `grep` sucht wörtlich, nicht mit regulären Ausdrücken. |
| `read_file`, `write_file`, `edit_file` | lesen, neu schreiben, eine Textstelle ersetzen (sie muss genau einmal vorkommen) |
| `write_todos` | Plan mit Aufgaben und Stand; jede Änderung steht als Event `PlanUpdated` im Log |
| `load_skill` | lädt die Anleitung eines Skills |

- **Projektwissen:** Liegt im Arbeitsverzeichnis eine `AGENTS.md`, steht ihr Inhalt immer im System-Prompt.
  Mit `memory = true` in `can.toml` kommen die Tools `remember`, `recall` und `forget` dazu (dauerhaft mit `db`).
- **Skills:** Ordner unter `.can/skills/<name>/` mit einer `SKILL.md`, deren Kopf `name` und
  `description` nennt. In den Prompt kommen nur diese beiden Angaben; den vollen Text holt das
  Modell bei Bedarf.
- **Freigabe mit Änderung:** Auf eine Rückfrage antwortet man mit `j`, `n` oder `e`. Mit `e`
  gibt man neue Argumente als JSON ein; sie werden gegen das Schema geprüft, im Log
  festgehalten, und das Modell erfährt, dass sie geändert wurden.
- **Verzweigen:** `can fork <kennung> --at <position>` legt eine neue Sitzung an, die den Verlauf
  bis zu dieser Position übernimmt. Die Positionen zeigt `can inspect`.

## WASM-Tools

Ein Tool kann ein WebAssembly-Modul sein (WASI Preview 1). Es bekommt die Argumente als JSON auf der
Standardeingabe und schreibt sein Ergebnis auf die Standardausgabe. Es sieht nur, was freigegeben
ist:

| Freigabe | Wirkung |
| --- | --- |
| `files` | `none` (Vorgabe), `read` oder `read_write`; das Arbeitsverzeichnis erscheint als `/work` |
| `network_hosts` | Hosts für die Host-Funktion `can.http_get`; ohne Eintrag kein Netzwerk. Private Adressen bleiben gesperrt. |
| `secrets` | Schlüssel, die das Modul als Umgebungsvariablen sieht; sonst sieht es keine Umgebung |
| `memory_mb`, `fuel_millions` | Speichergrenze und Rechenbudget |

```toml
[[wasm_tools]]
name = "demo"
description = "Beispiel-Tool"
module = "wasm/demo-tool/target/wasm32-wasip1/release/can-demo-tool.wasm"
effect = "read_only"
files = "read"
```

Ein Beispiel samt Anleitung liegt unter [`wasm/demo-tool`](wasm/demo-tool):

```bash
rustup target add wasm32-wasip1
```

```bash
cargo build --release --target wasm32-wasip1 --manifest-path wasm/demo-tool/Cargo.toml
```

WASI Preview 1 kennt keine Netzwerkverbindungen; Abrufe gehen nur über `can.http_get`, und die
prüft jede Adresse samt Weiterleitungen. Ein abgebrochener Lauf wartet nicht auf das Modul; das
Modul selbst endet spätestens, wenn sein Rechenbudget verbraucht ist.

## Werkzeuge für den Betrieb

```bash
cargo run -p can-cli -- inspect <kennung>
```

`inspect` zeigt den Verlauf einer Sitzung als Zeitleiste mit Token, Tool-Dauern und dem Zustand
der Hash-Kette.

```bash
cargo run -p can-cli -- eval faelle.toml --replay antworten.jsonl
```

`eval` lässt Testfälle laufen (`prompt`, `expect_contains`, `expect_not_contains`) und endet mit
Fehler, wenn einer scheitert. Mit `--replay` kommen die Antworten aus einer Aufzeichnung; so wird
daraus ein wiederholbarer Regressionstest.

Weitere Bausteine der Bibliothek:

- **`Fallback`:** fällt ein Anbieter aus, übernimmt ein zweiter.
- **`OpenAiCompat::azure`:** Deployments bei Azure OpenAI.
- **`Agent::complete_structured`:** Endantwort als JSON, geprüft gegen ein Schema, mit bis zu zwei Korrekturen.
- **`Pricing`:** Preise je Million Token; der Dienst rechnet damit Kosten und Kostenbudget.
- **Crate `can`:** Fassade mit `use can::prelude::*;` und Feature-Flags für Anbieter, Stores und Tools.

## Prüfen

```bash
./scripts/check.sh
```

Das Skript führt `cargo fmt --check`, `cargo clippy -D warnings`, alle Tests und `cargo doc` aus.
Kein Test ruft eine echte Modell-API auf; Provider, MCP und HTTP laufen gegen lokale Server.
Die PostgreSQL-Tests laufen nur, wenn `CAN_TEST_POSTGRES_URL` auf eine Datenbank zeigt; in der CI
ist das der Fall.

## Lizenz

MIT, siehe [`LICENSE`](LICENSE).
