# Can

Can ist ein eigenständiges AI-Agent-Framework in Rust: ein Cargo-Workspace ohne fremde
Agent-Frameworks.

Der Kern ist eine reine Schrittfunktion ohne Ein- und Ausgabe. Alles, was in einem Lauf passiert,
ist ein Event; der Zustand entsteht nur durch Falten der Event-Folge. Jeder Tool-Aufruf läuft
durch eine Policy.

## Stand

Phase 1 bis 5 des Bauplans sind umgesetzt. Phase 6 (WASM-Sandbox) ist offen.

| Crate | Aufgabe |
| --- | --- |
| `can-core` | Typen (`Message`, `Event`, `RunState`), die Traits `Provider`, `Tool`, `Store`, `Policy`, `Clock`, dazu `RuleSet` und die Schema-Prüfung. Kein Netzwerk. |
| `can-providers` | OpenAI-kompatibler Provider (Feature `openai`), Anthropic (Feature `anthropic`), `Retry`, `Recording`, `FakeProvider` |
| `can-runtime` | Schrittfunktion `step`, Treiber, `Agent`-Builder, Limits, Abbruch, Kompaktierung, parallele Tools, `SubAgent` |
| `can-tools` | `read_file`, `write_file`, `shell`, `http_fetch` (Feature `http`), MCP-Client (Feature `mcp`) |
| `can-store` | `MemoryStore` und `SqliteStore` (Feature `sqlite`) mit Migrationen |
| `can-macros` | `#[tool]` macht aus einer `async fn` ein Tool samt JSON-Schema |
| `can-cli` | Binary `can` mit den Befehlen `chat` und `resume` |

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

## Prüfen

```bash
./scripts/check.sh
```

Das Skript führt `cargo fmt --check`, `cargo clippy -D warnings`, alle Tests und `cargo doc` aus.
Kein Test ruft eine echte Modell-API auf; Provider, MCP und HTTP laufen gegen lokale Server.
