//! Bibliotheksteil der CLI: Konfiguration lesen, Agent bauen, einen Gesprächszug ausgeben.

mod app;
pub mod config;
pub mod eval;
pub mod inspect;

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use can_core::{
    Bytes, CallId, ContentBlock, Decision, Event, Message, ProviderError, Role, SessionId,
    StoreError, ToolCall,
};
use can_runtime::{BuildError, Outcome, Run, RunError};
use can_tools::McpError;

pub use app::{App, DEFAULT_SYSTEM, Options, build_app, open_store};
pub use config::Config;

/// Fehler der CLI.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("{0}")]
    Config(String),
    #[error(transparent)]
    Build(#[from] BuildError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Run(#[from] RunError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Mcp(#[from] McpError),
    #[error("Ein-/Ausgabefehler: {0}")]
    Io(#[from] std::io::Error),
}

/// Baut die Nutzernachricht aus Text und Bilddateien. Der Bildtyp folgt aus der Dateiendung.
pub fn user_message(text: &str, images: &[impl AsRef<Path>]) -> Result<Message, CliError> {
    let mut content = vec![ContentBlock::Text(text.to_owned())];
    for path in images {
        let path = path.as_ref();
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("")
            .to_lowercase();
        let media_type = match extension.as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            _ => {
                return Err(CliError::Config(format!(
                    "Unbekanntes Bildformat: {} (erlaubt: png, jpg, gif, webp)",
                    path.display()
                )));
            }
        };
        let data = Bytes(std::fs::read(path)?);
        content.push(ContentBlock::Image {
            media_type: media_type.to_owned(),
            data,
        });
    }
    Ok(Message {
        role: Role::User,
        content,
    })
}

/// Ausgabe und Rückfrage eines Gesprächszugs.
pub struct Terminal<'a> {
    pub out: &'a mut (dyn Write + Send),
    /// Wird für jeden Aufruf gefragt, der eine Freigabe braucht.
    pub approve: &'a mut (dyn FnMut(&ToolCall) -> Decision + Send),
}

/// Führt einen Gesprächszug aus und streamt die Antwort ins Terminal.
pub async fn chat_turn(
    app: &App,
    session: SessionId,
    input: impl Into<Message>,
    terminal: Terminal<'_>,
) -> Result<Outcome, CliError> {
    let run = app.agent.run(session, input).await?;
    drive(run, HashMap::new(), terminal).await
}

/// Setzt einen unterbrochenen Lauf am letzten gespeicherten Zustand fort.
pub async fn resume_turn(
    app: &App,
    session: SessionId,
    terminal: Terminal<'_>,
) -> Result<Outcome, CliError> {
    // Aufrufe aus dem Log, damit eine erneute Rückfrage zeigen kann, worum es geht.
    let mut calls = HashMap::new();
    for envelope in app.store.load(session).await? {
        if let Event::ToolRequested { call } = envelope.event {
            calls.insert(call.id, call);
        }
    }
    let run = app.agent.resume(session).await?;
    drive(run, calls, terminal).await
}

async fn drive(
    mut run: Run,
    mut calls: HashMap<CallId, ToolCall>,
    terminal: Terminal<'_>,
) -> Result<Outcome, CliError> {
    let Terminal { out, approve } = terminal;
    while let Some(envelope) = run.next().await {
        match envelope?.event {
            Event::TextDelta { text, .. } => {
                write!(out, "{text}")?;
                out.flush()?;
            }
            Event::ToolRequested { call } => {
                calls.insert(call.id, call);
            }
            Event::ToolStarted { call_id } => {
                if let Some(call) = calls.get(&call_id) {
                    writeln!(out, "\n[{} {}]", call.name, call.input)?;
                }
            }
            Event::ApprovalNeeded { call_id } => {
                let decision = calls
                    .get(&call_id)
                    .map(&mut *approve)
                    .unwrap_or(Decision::Deny);
                run.resolve(call_id, decision).await?;
            }
            Event::RunFinished { .. } => writeln!(out)?,
            _ => {}
        }
    }
    Ok(run.outcome().await?)
}
