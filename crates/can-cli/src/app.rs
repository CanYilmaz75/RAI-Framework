//! Baut aus Optionen und Konfigurationsdatei den Agenten samt Provider, Tools, Policy und Store.

use std::path::PathBuf;
use std::sync::Arc;

use can_core::{MemoryStore, Provider, RuleSet, Store};
use can_providers::{FakeProvider, Recording, Retry};
use can_runtime::{Agent, SystemClock};
use can_store::SqliteStore;
use can_tools::{
    HttpFetch, LoadSkill, McpClient, ReadFile, SkillSet, WriteTodos, edit_file, glob, grep,
    load_memory, ls, shell, system_prompt, write_file,
};

use crate::CliError;
use crate::config::Config;

/// Vorgabe für den System-Prompt.
pub const DEFAULT_SYSTEM: &str = "Du bist ein hilfreicher Assistent. Nutze die Tools, um Dateien zu finden, zu lesen und zu ändern oder Programme zu starten. Halte bei mehrstufigen Aufgaben einen Plan mit write_todos.";

/// Einstellungen der Kommandozeile. Sie haben Vorrang vor der Konfigurationsdatei.
#[derive(Clone, Debug, Default)]
pub struct Options {
    pub model: Option<String>,
    pub system: Option<String>,
    /// Arbeitsverzeichnis der Tools; Vorgabe ist das aktuelle Verzeichnis.
    pub workdir: Option<PathBuf>,
    /// Basis-URL der API; ohne Angabe gelten Konfiguration und Umgebungsvariablen.
    pub base_url: Option<String>,
    /// Anbieter, etwa `anthropic`, `gemini`, `bedrock` oder `ollama`.
    pub provider: Option<String>,
    /// SQLite-Datei für Sitzungen; ohne Angabe bleibt alles im Arbeitsspeicher.
    pub db: Option<PathBuf>,
    /// Datei, in die die Modellantworten für ein späteres Replay geschrieben werden.
    pub record: Option<PathBuf>,
    /// Aufzeichnung, aus der die Modellantworten kommen, statt von einem echten Anbieter.
    pub replay: Option<PathBuf>,
    pub config: Config,
}

/// Der Agent samt dem Store, in dem seine Sitzungen liegen.
pub struct App {
    pub agent: Agent,
    pub store: Arc<dyn Store>,
}

/// Wiederholungen innen, Aufzeichnung außen: aufgezeichnet wird nur, was der Lauf auch sieht.
/// Wiederholungen innen, Aufzeichnung außen: aufgezeichnet wird nur, was der Lauf auch sieht.
fn decorate(provider: Arc<dyn Provider>, record: &Option<PathBuf>) -> Arc<dyn Provider> {
    let retrying = Retry::new(provider, Arc::new(SystemClock));
    match record {
        Some(path) => Arc::new(Recording::new(retrying, path.clone())),
        None => Arc::new(retrying),
    }
}

fn provider(options: &Options) -> Result<Arc<dyn Provider>, CliError> {
    if let Some(path) = &options.replay {
        return Ok(Arc::new(FakeProvider::from_recording(path)?));
    }
    let mut settings = options.config.provider.clone();
    if let Some(kind) = &options.provider {
        settings.kind = kind.clone();
    }
    if options.base_url.is_some() {
        settings.base_url = options.base_url.clone();
    }
    let lookup = |name: &str| std::env::var(name).ok();
    Ok(decorate(settings.build(&lookup)?, &options.record))
}

/// Öffnet den Store der Sitzungen: die SQLite-Datei aus Optionen oder Konfiguration, sonst den
/// Arbeitsspeicher.
pub fn open_store(options: &Options) -> Result<Arc<dyn Store>, CliError> {
    Ok(
        match options.db.clone().or_else(|| options.config.db.clone()) {
            Some(path) => Arc::new(SqliteStore::open(path)?),
            None => Arc::new(MemoryStore::new()),
        },
    )
}

/// Baut den Agenten. Lesen ist erlaubt; Schreiben, `shell`, HTTP und MCP-Tools fragen nach,
/// solange keine Regel der Konfiguration etwas anderes sagt.
pub async fn build_app(options: &Options) -> Result<App, CliError> {
    let config = &options.config;
    let model = options
        .model
        .clone()
        .or_else(|| config.model.clone())
        .filter(|model| !model.trim().is_empty());
    let model = model.ok_or_else(|| {
        CliError::Config(
            "Kein Modell angegeben: --model, CAN_MODEL oder `model` in can.toml setzen.".to_owned(),
        )
    })?;
    let store = open_store(options)?;
    let mut policy = RuleSet::safe_defaults();
    policy.rules.extend(config.policy.rules.iter().cloned());
    let system = options.system.clone().or_else(|| config.system.clone());
    // Gedächtnis (AGENTS.md) und Skills (.can/skills) liegen im Arbeitsverzeichnis.
    let project = options
        .workdir
        .clone()
        .or_else(|| config.workdir.clone())
        .unwrap_or_else(|| PathBuf::from("."));
    let memory = load_memory(&project.join("AGENTS.md"))?;
    let skills = SkillSet::load(&project.join(".can").join("skills"))?;
    let system = system_prompt(
        system.as_deref().unwrap_or(DEFAULT_SYSTEM),
        memory.as_deref(),
        &skills,
    );
    let mut builder = Agent::builder()
        .provider_arc(provider(options)?)
        .model(model)
        .system(system)
        .tool(ReadFile::new())
        .tool(write_file)
        .tool(edit_file)
        .tool(ls)
        .tool(glob)
        .tool(grep)
        .tool(WriteTodos::new())
        .tool(shell)
        .policy(policy)
        .limits(config.limits.limits())
        // Ein Nutzer, ein Rechner: nach einem Absturz soll `resume` sofort weitermachen können.
        .owner("can-cli")
        .store_arc(store.clone());
    if !skills.is_empty() {
        builder = builder.tool(LoadSkill::new(skills));
    }
    if !config.http.allowed_hosts.is_empty() {
        builder = builder.tool(HttpFetch::new(config.http.allowed_hosts.clone()));
    }
    for (name, server) in &config.mcp.servers {
        let client = match (&server.command, &server.url) {
            (Some(command), _) => {
                McpClient::connect_stdio(name.clone(), command, &server.args).await?
            }
            (None, Some(url)) => McpClient::connect_http(name.clone(), url.clone()).await?,
            (None, None) => continue,
        };
        for tool in client.tools().await? {
            builder = builder.tool_arc(tool);
        }
    }
    if let Some(workdir) = options.workdir.clone().or_else(|| config.workdir.clone()) {
        builder = builder.workdir(workdir);
    }
    Ok(App {
        agent: builder.build()?,
        store,
    })
}
