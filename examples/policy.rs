//! Ein Tool per `#[tool]` und eine Policy, die nachfragt.
//!
//! Das Skript des Fake-Providers ruft das Tool zweimal auf. Der erste Aufruf wird freigegeben,
//! der zweite abgelehnt; die Ablehnung geht als Fehlerergebnis ans Modell zurück.
//!
//! ```text
//! cargo run -p agentkit-runtime --example policy
//! ```

use agentkit_core::{Decision, Event, RuleSet, SessionId, ToolError};
use agentkit_macros::tool;
use agentkit_providers::FakeProvider;
use agentkit_runtime::Agent;
use serde_json::json;

/// Legt eine Notiz ab (hier nur zum Schein).
#[tool(effect = "write")]
async fn save_note(title: String) -> Result<String, ToolError> {
    Ok(format!("Notiz „{title}“ gespeichert."))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let provider = FakeProvider::new()
        .tool_call("save_note", json!({"title": "Einkauf"}))
        .tool_call("save_note", json!({"title": "Geheim"}))
        .text("Eine Notiz ist gespeichert, die zweite wurde abgelehnt.");
    let agent = Agent::builder()
        .provider(provider)
        .model("fake")
        .tool(save_note)
        .policy(RuleSet::safe_defaults())
        .build()?;

    let mut run = agent
        .run(SessionId::new(), "Speichere zwei Notizen.")
        .await?;
    let mut answers = [Decision::Allow, Decision::Deny].into_iter();
    while let Some(envelope) = run.next().await {
        match envelope?.event {
            Event::ToolRequested { call } => {
                println!("Modell will {} mit {}", call.name, call.input)
            }
            Event::ApprovalNeeded { call_id } => {
                let decision = answers.next().unwrap_or(Decision::Deny);
                println!("  Rückfrage → {decision:?}");
                run.resolve(call_id, decision).await?;
            }
            Event::ToolFinished {
                output, is_error, ..
            } => {
                println!("  Ergebnis (Fehler: {is_error}): {}", output.as_text());
            }
            Event::TextDelta { text, .. } => println!("Antwort: {text}"),
            _ => {}
        }
    }
    Ok(())
}
