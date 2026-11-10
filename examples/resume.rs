//! Fortsetzen nach einem Absturz: Der erste „Prozess“ endet an einer offenen Rückfrage, der
//! zweite lädt das Log aus SQLite und macht dort weiter.
//!
//! ```text
//! cargo run -p can-runtime --example resume
//! ```

use can_core::{Decision, Event, RuleSet, SessionId, ToolError};
use can_macros::tool;
use can_providers::FakeProvider;
use can_runtime::Agent;
use can_store::SqliteStore;
use serde_json::json;

/// Legt eine Notiz ab (hier nur zum Schein).
#[tool(effect = "write")]
async fn save_note(title: String) -> Result<String, ToolError> {
    Ok(format!("Notiz „{title}“ gespeichert."))
}

fn agent(
    provider: FakeProvider,
    db: &std::path::Path,
) -> Result<Agent, Box<dyn std::error::Error>> {
    Ok(Agent::builder()
        .provider(provider)
        .model("fake")
        .tool(save_note)
        .policy(RuleSet::safe_defaults())
        .store(SqliteStore::open(db)?)
        .build()?)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db = std::env::temp_dir().join(format!("can-beispiel-{}.db", SessionId::new()));
    let session = SessionId::new();

    // Erster Prozess: läuft bis zur Rückfrage und verschwindet dann, ohne aufzuräumen.
    let first = agent(
        FakeProvider::new().tool_call("save_note", json!({"title": "Einkauf"})),
        &db,
    )?;
    let mut run = first.run(session, "Speichere eine Notiz.").await?;
    while let Some(envelope) = run.next().await {
        if matches!(envelope?.event, Event::ApprovalNeeded { .. }) {
            println!("Prozess 1: Rückfrage offen, Absturz.");
            break;
        }
    }
    std::mem::forget(run);

    // Zweiter Prozess: gleiche Datei, gleiche Sitzung. Die Modellantwort wird nicht neu angefragt.
    let second = agent(FakeProvider::new().text("Die Notiz ist gespeichert."), &db)?;
    let mut run = second.resume(session).await?;
    while let Some(envelope) = run.next().await {
        match envelope?.event {
            Event::ApprovalNeeded { call_id } => {
                println!("Prozess 2: dieselbe Rückfrage, jetzt freigegeben.");
                run.resolve(call_id, Decision::Allow).await?;
            }
            Event::ToolFinished { output, .. } => println!("Prozess 2: {}", output.as_text()),
            Event::TextDelta { text, .. } => println!("Antwort: {text}"),
            _ => {}
        }
    }
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", db.display()));
    }
    Ok(())
}
