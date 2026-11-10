//! Bibliotheksteil der CLI: Agent bauen und einen Gesprächszug im Terminal ausgeben.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use agentkit_core::{
    CallId, Decision, Event, MemoryStore, Provider, ProviderError, RuleSet, Secret, SessionId,
    Store, StoreError, ToolCall,
};
use agentkit_providers::{OpenAiCompat, Recording};
use agentkit_runtime::{Agent, BuildError, Outcome, Run, RunError};
use agentkit_store::SqliteStore;
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
    #[error(transparent)]
    Store(#[from] StoreError),
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
    /// SQLite-Datei für Sitzungen; ohne Angabe bleibt alles im Arbeitsspeicher.
    pub db: Option<PathBuf>,
    /// Datei, in die die Modellantworten für ein späteres Replay geschrieben werden.
    pub record: Option<PathBuf>,
}

/// Der Agent samt dem Store, in dem seine Sitzungen liegen.
pub struct App {
    pub agent: Agent,
    pub store: Arc<dyn Store>,
}

/// Baut den Agenten mit OpenAI-kompatiblem Provider und den Tools `read_file`, `write_file`
/// und `shell`. Lesen ist erlaubt; Schreiben und `shell` fragen nach.
pub fn build_app(options: &Options) -> Result<App, CliError> {
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
    let provider: Arc<dyn Provider> = match &options.record {
        Some(path) => Arc::new(Recording::new(provider, path.clone())),
        None => Arc::new(provider),
    };
    let store: Arc<dyn Store> = match &options.db {
        Some(path) => Arc::new(SqliteStore::open(path)?),
        None => Arc::new(MemoryStore::new()),
    };
    let mut builder = Agent::builder()
        .provider_arc(provider)
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
        .policy(RuleSet::safe_defaults())
        .store_arc(store.clone());
    if let Some(workdir) = &options.workdir {
        builder = builder.workdir(workdir.clone());
    }
    Ok(App {
        agent: builder.build()?,
        store,
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
    prompt: &str,
    terminal: Terminal<'_>,
) -> Result<Outcome, CliError> {
    let run = app.agent.run(session, prompt).await?;
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
