# RAI-Framework (`agentkit`)

Ein eigenständiges AI-Agent-Framework in Rust: ein Cargo-Workspace ohne fremde Agent-Frameworks.
`agentkit` ist ein Arbeitsname.

Der Kern ist eine reine Schrittfunktion ohne Ein- und Ausgabe. Alles, was in einem Lauf passiert,
ist ein Event; der Zustand entsteht nur durch Falten der Event-Folge. Jeder Tool-Aufruf läuft
durch eine Policy.

## Stand

Phase 1 von 6 ist fertig: Kern und erster Lauf.

| Crate | Aufgabe |
| --- | --- |
| `agentkit-core` | Typen (`Message`, `Event`, `RunState`), die Traits `Provider`, `Tool`, `Store`, `Policy`, `Clock`. Kein Netzwerk. |
| `agentkit-providers` | `FakeProvider` für Tests, OpenAI-kompatibler Provider mit SSE-Streaming (Feature `openai`) |
| `agentkit-runtime` | Schrittfunktion `step`, Treiber, `Agent`-Builder, Limits, Abbruch |
| `agentkit-tools` | `read_file` mit Begrenzung auf das Arbeitsverzeichnis |
| `agentkit-store` | `MemoryStore` (SQLite folgt in Phase 3) |
| `agentkit-macros` | noch leer (`#[tool]` folgt in Phase 2) |
| `agentkit-cli` | Binary `agentkit` mit dem Befehl `chat` |

## Ausprobieren

Ohne Netzwerk und ohne Schlüssel, mit dem Fake-Provider:

```bash
cargo run -p agentkit-runtime --example minimal
```

Mit einem echten Modell über eine OpenAI-kompatible API (auch Ollama):

```bash
export AGENTKIT_BASE_URL=http://localhost:11434/v1   # Vorgabe: https://api.openai.com/v1
export AGENTKIT_API_KEY=...                          # für Ollama nicht nötig
export AGENTKIT_MODEL=mein-modell
cargo run -p agentkit-cli -- chat "Fasse README.md zusammen"
```

Ohne Frage startet `agentkit chat` ein Gespräch, das Zeile für Zeile von der Standardeingabe liest.

## Als Bibliothek

```rust
let agent = Agent::builder()
    .provider(OpenAiCompat::from_env()?)
    .model("mein-modell")
    .system("Du bist ein hilfreicher Assistent.")
    .tool(ReadFile::new())
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

## Prüfen

```bash
./scripts/check.sh
```

Das Skript führt `cargo fmt --check`, `cargo clippy -D warnings`, alle Tests und `cargo doc` aus.
Kein Test ruft eine echte Modell-API auf.
