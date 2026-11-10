//! Bibliotheksteil der CLI: Agent bauen und einen Gesprächszug im Terminal ausgeben.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;

use agentkit_core::{CallId, Decision, Event, ProviderError, RuleSet, Secret, SessionId, ToolCall};
use agentkit_providers::OpenAiCompat;
use agentkit_runtime::{Agent, BuildError, Outcome, RunError};
use agentkit_tools::{ReadFile, shell, write_file};

/// Vorgabe für den System-Prompt.
pub const DEFAULT_SYSTEM: &str = "Du bist ein hilfreicher Assistent. Nutze die Tools, um Dateien zu lesen, zu schreiben oder Programme zu starten.";

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
    #[error("Ein-/Ausgabefehler: {0}")]
    Io(#[from] std::io::Error),
}

/// Einstellungen, aus denen der Agent gebaut wird.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// Modellname; Pflicht.
    pub model: Option<String>,
    pub system: Option<String>,
    /// Arbeitsverzeichnis der Tools; Vorgabe ist das aktuelle Verzeichnis.
    pub workdir: Option<PathBuf>,
    /// Basis-URL der API; ohne Angabe gelten die Umgebungsvariablen.
    pub base_url: Option<String>,
}

/// Baut den Agenten mit OpenAI-kompatiblem Provider und den Tools `read_file`, `write_file`
/// und `shell`. Lesen ist erlaubt; Schreiben und `shell` fragen nach.
pub fn build_agent(options: &Options) -> Result<Agent, CliError> {
    let model = options
        .model
        .clone()
        .filter(|model| !model.trim().is_empty())
        .ok_or_else(|| {
            CliError::Config(
                "Kein Modell angegeben: --model oder AGENTKIT_MODEL setzen.".to_owned(),
            )
        })?;
    let provider = match &options.base_url {
        Some(base_url) => {
            let key = ["AGENTKIT_API_KEY", "OPENAI_API_KEY"]
                .iter()
                .find_map(|name| std::env::var(name).ok());
            OpenAiCompat::new(base_url.clone(), key.map(Secret::new))
        }
        None => OpenAiCompat::from_env()?,
    };
    let mut builder = Agent::builder()
        .provider(provider)
        .model(model)
        .system(
            options
                .system
                .clone()
                .unwrap_or_else(|| DEFAULT_SYSTEM.to_owned()),
        )
        .tool(ReadFile::new())
        .tool(write_file)
        .tool(shell)
        .policy(RuleSet::safe_defaults());
    if let Some(workdir) = &options.workdir {
        builder = builder.workdir(workdir.clone());
    }
    Ok(builder.build()?)
}

/// Führt einen Gesprächszug aus und streamt die Antwort nach `out`.
///
/// `approve` wird für jeden Aufruf gefragt, der eine Freigabe braucht.
pub async fn chat_turn(
    agent: &Agent,
    session: SessionId,
    prompt: &str,
    out: &mut (dyn Write + Send),
    approve: &mut (dyn FnMut(&ToolCall) -> Decision + Send),
) -> Result<Outcome, CliError> {
    let mut run = agent.run(session, prompt).await?;
    let mut calls: HashMap<CallId, ToolCall> = HashMap::new();
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
