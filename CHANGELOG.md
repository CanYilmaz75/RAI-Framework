# Changelog

## Unveröffentlicht

### Phase 1: Kern und erster Lauf

- Workspace mit sieben Crates, `rust-toolchain.toml` und `scripts/check.sh`
- `agentkit-core`: Datenmodell, `RunState::apply`, Traits `Provider`, `Tool`, `Store`, `Policy`,
  `Clock`, dazu `MemoryStore`, `ManualClock` und der Chunk-Assembler
- `agentkit-providers`: `FakeProvider` und OpenAI-kompatibler Provider mit eigenem SSE-Parser
- `agentkit-runtime`: reine Schrittfunktion, Treiber, `Agent`-Builder, Limits für Schritte,
  Token, Zeit und Tool-Aufrufe, Abbruch, Freigaben über `Run::resolve`
- `agentkit-tools`: `read_file`, begrenzt auf das Arbeitsverzeichnis
- `agentkit-cli`: `agentkit chat` streamt die Antwort ins Terminal
