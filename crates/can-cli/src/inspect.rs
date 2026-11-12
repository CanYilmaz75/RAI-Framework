//! `can inspect`: der Verlauf einer Sitzung als Zeitleiste aus dem Store.

use std::collections::HashMap;
use std::io::Write;

use can_core::{Event, SessionId, Store};

use crate::CliError;

fn shorten(text: &str) -> String {
    let line = text.replace('\n', " ");
    if line.chars().count() <= 80 {
        line
    } else {
        format!("{}…", line.chars().take(80).collect::<String>())
    }
}

/// Schreibt Zeitleiste und Kennzahlen der Sitzung nach `out`.
pub async fn inspect(
    store: &dyn Store,
    session: SessionId,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let log = store.load(session).await?;
    let Some(first) = log.first() else {
        return Err(CliError::Config(format!(
            "Sitzung {session} ist leer oder unbekannt."
        )));
    };
    let start = first.ts;
    let (mut runs, mut steps, mut compactions, mut input, mut output) =
        (0u32, 0u32, 0u32, 0u64, 0u64);
    let mut names = HashMap::new();
    let mut started = HashMap::new();
    let mut tools: Vec<(String, u64, bool)> = Vec::new();
    for envelope in &log {
        let detail = match &envelope.event {
            Event::RunStarted { input, .. } => {
                runs += 1;
                shorten(&input.text())
            }
            Event::ModelRequested { step } => {
                steps += 1;
                format!("Schritt {step}")
            }
            Event::ModelResponded { message, usage, .. } => {
                input += usage.input_tokens;
                output += usage.output_tokens;
                format!("{} Token, {}", usage.total(), shorten(&message.text()))
            }
            Event::ToolRequested { call } => {
                names.insert(call.id, call.name.clone());
                format!("{} {}", call.name, shorten(&call.input.to_string()))
            }
            Event::ApprovalResolved { decision, by, .. } => format!("{decision:?} durch {by:?}"),
            Event::ToolStarted { call_id } => {
                started.insert(*call_id, envelope.ts);
                names.get(call_id).cloned().unwrap_or_default()
            }
            Event::ToolFinished {
                call_id,
                output,
                is_error,
            } => {
                let name = names.get(call_id).cloned().unwrap_or_default();
                if let Some(begin) = started.remove(call_id) {
                    tools.push((name.clone(), envelope.ts.millis_since(begin), *is_error));
                }
                format!(
                    "{name}{} {}",
                    if *is_error { " (Fehler)" } else { "" },
                    shorten(&output.as_text())
                )
            }
            Event::Compacted { up_to_seq, .. } => {
                compactions += 1;
                format!("bis Position {up_to_seq}")
            }
            Event::RunFinished { reason } => format!("{reason:?}"),
            _ => String::new(),
        };
        writeln!(
            out,
            "{:>5}  +{:>7} ms  {:<18} {detail}",
            envelope.seq,
            envelope.ts.millis_since(start),
            envelope.event.kind()
        )?;
    }
    writeln!(
        out,
        "\n{runs} Läufe, {steps} Modellanfragen, {input} Eingabe- und {output} Ausgabe-Token, {compactions} Verdichtungen"
    )?;
    for (name, millis, is_error) in &tools {
        writeln!(
            out,
            "  Tool {name}: {millis} ms{}",
            if *is_error { ", Fehler" } else { "" }
        )?;
    }
    match store.verify(session).await? {
        None => writeln!(out, "Hash-Kette: unversehrt")?,
        Some(seq) => writeln!(out, "Hash-Kette: verändert ab Position {seq}")?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use can_core::MemoryStore;
    use can_providers::FakeProvider;
    use can_runtime::Agent;
    use can_tools::ReadFile;
    use serde_json::json;
    use std::sync::Arc;

    #[tokio::test]
    async fn timeline_lists_every_event_and_sums_up() {
        let store = Arc::new(MemoryStore::new());
        let agent = Agent::builder()
            .provider(
                FakeProvider::new()
                    .tool_call("read_file", json!({"path": "fehlt.txt"}))
                    .text("Die Datei fehlt."),
            )
            .model("test")
            .tool(ReadFile::new())
            .store_arc(store.clone())
            .build()
            .unwrap();
        let session = SessionId::new();
        agent.complete(session, "Lies fehlt.txt").await.unwrap();

        let mut out = Vec::new();
        inspect(store.as_ref(), session, &mut out).await.unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            text.lines().filter(|line| line.contains(" ms  ")).count(),
            10,
            "{text}"
        );
        assert!(text.contains("run_started        Lies fehlt.txt"));
        assert!(text.contains("tool_finished      read_file (Fehler)"));
        assert!(text.contains(
            "1 Läufe, 2 Modellanfragen, 20 Eingabe- und 10 Ausgabe-Token, 0 Verdichtungen"
        ));
        assert!(text.contains("  Tool read_file: ") && text.contains("Hash-Kette: unversehrt"));

        let unknown = inspect(store.as_ref(), SessionId::new(), &mut Vec::new()).await;
        assert!(matches!(unknown, Err(CliError::Config(_))));
    }
}
