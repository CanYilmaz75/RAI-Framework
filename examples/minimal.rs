//! Kleinster vollständiger Lauf: Modell → Tool-Aufruf → Tool-Ergebnis → Endantwort.
//!
//! Läuft ohne Netzwerk und ohne Schlüssel, weil der Fake-Provider die Modellantworten liefert:
//!
//! ```text
//! cargo run -p agentkit-runtime --example minimal
//! ```

use agentkit_core::{Effect, Event, SessionId, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec};
use agentkit_providers::FakeProvider;
use agentkit_runtime::Agent;
use serde_json::{Value, json};

/// Ein von Hand geschriebenes Tool: liefert die Länge eines Textes.
struct TextLength(ToolSpec);

#[async_trait::async_trait]
impl Tool for TextLength {
    fn spec(&self) -> &ToolSpec {
        &self.0
    }

    async fn call(&self, args: Value, _cx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let text = args["text"]
            .as_str()
            .ok_or_else(|| ToolError::ForModel("text fehlt".into()))?;
        Ok(ToolOutput::text(text.chars().count().to_string()))
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let schema =
        json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]});
    let spec = ToolSpec::new("text_length", "Zählt die Zeichen eines Textes.", schema)
        .with_effect(Effect::ReadOnly)
        .with_idempotent(true);

    // Das Skript ersetzt das Modell: erst ein Tool-Aufruf, dann die Antwort.
    let provider = FakeProvider::new()
        .tool_call("text_length", json!({"text": "Hallo Welt"}))
        .text("Der Text hat 10 Zeichen.");

    let agent = Agent::builder()
        .provider(provider)
        .model("fake")
        .system("Du bist ein hilfreicher Assistent.")
        .tool(TextLength(spec))
        .build()?;

    let mut run = agent
        .run(SessionId::new(), "Wie lang ist „Hallo Welt“?")
        .await?;
    while let Some(envelope) = run.next().await {
        let envelope = envelope?;
        match &envelope.event {
            Event::TextDelta { text, .. } => println!("Antwort: {text}"),
            event => println!("{:>3}  {}", envelope.seq, event.kind()),
        }
    }
    let outcome = run.outcome().await?;
    println!(
        "Ende: {:?} nach {} Schritten, {} Token",
        outcome.reason,
        outcome.steps,
        outcome.usage.total()
    );
    Ok(())
}
