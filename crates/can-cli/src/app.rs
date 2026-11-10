//! Baut aus Optionen und Konfigurationsdatei den Agenten samt Provider, Tools, Policy und Store.

use std::path::PathBuf;
use std::sync::Arc;

use can_core::{MemoryStore, Provider, RuleSet, Store};
use can_providers::{Anthropic, OpenAiCompat, Recording, Retry};
use can_runtime::{Agent, SystemClock};
use can_store::SqliteStore;
use can_tools::{HttpFetch, McpClient, ReadFile, shell, write_file};

use crate::CliError;
use crate::config::{Config, ProviderKind};

/// Vorgabe für den System-Prompt.
pub const DEFAULT_SYSTEM: &str = "Du bist ein hilfreicher Assistent. Nutze die Tools, um Dateien zu lesen, zu schreiben oder Programme zu starten.";

/// Einstellungen der Kommandozeile. Sie haben Vorrang vor der Konfigurationsdatei.
#[derive(Clone, Debug, Default)]
pub struct Options {
    pub model: Option<String>,
    pub system: Option<String>,
    /// Arbeitsverzeichnis der Tools; Vorgabe ist das aktuelle Verzeichnis.
    pub workdir: Option<PathBuf>,
    /// Basis-URL der API; ohne Angabe gelten Konfiguration und Umgebungsvariablen.
    pub base_url: Option<String>,
    pub provider: Option<ProviderKind>,
    /// SQLite-Datei für Sitzungen; ohne Angabe bleibt alles im Arbeitsspeicher.
    pub db: Option<PathBuf>,
    /// Datei, in die die Modellantworten für ein späteres Replay geschrieben werden.
    pub record: Option<PathBuf>,
    pub config: Config,
}

/// Der Agent samt dem Store, in dem seine Sitzungen liegen.
pub struct App {
    pub agent: Agent,
    pub store: Arc<dyn Store>,
}

/// Wiederholungen innen, Aufzeichnung außen: aufgezeichnet wird nur, was der Lauf auch sieht.
fn decorate<P: Provider + 'static>(provider: P, record: &Option<PathBuf>) -> Arc<dyn Provider> {
    let retrying = Retry::new(provider, Arc::new(SystemClock));
    match record {
        Some(path) => Arc::new(Recording::new(retrying, path.clone())),
        None => Arc::new(retrying),
    }
}

fn provider(options: &Options) -> Result<Arc<dyn Provider>, CliError> {
    let config = &options.config.provider;
    let base_url = options.base_url.clone().or_else(|| config.base_url.clone());
    // Basis-URL und Schlüsselquelle aus Optionen und Datei überlagern die Umgebungsvariablen.
    let lookup = |name: &str| {
        if name.ends_with("BASE_URL") && base_url.is_some() {
            return base_url.clone();
        }
        match &config.api_key_env {
            Some(variable) if name.ends_with("API_KEY") => std::env::var(variable).ok(),
            _ => std::env::var(name).ok(),
        }
    };
    Ok(match options.provider.unwrap_or(config.kind) {
        ProviderKind::Openai => decorate(OpenAiCompat::from_lookup(lookup)?, &options.record),
        ProviderKind::Anthropic => decorate(Anthropic::from_lookup(lookup)?, &options.record),
    })
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
    let store: Arc<dyn Store> = match options.db.clone().or_else(|| config.db.clone()) {
        Some(path) => Arc::new(SqliteStore::open(path)?),
        None => Arc::new(MemoryStore::new()),
    };
    let mut policy = RuleSet::safe_defaults();
    policy.rules.extend(config.policy.rules.iter().cloned());
    let system = options.system.clone().or_else(|| config.system.clone());
    let mut builder = Agent::builder()
        .provider_arc(provider(options)?)
        .model(model)
        .system(system.unwrap_or_else(|| DEFAULT_SYSTEM.to_owned()))
        .tool(ReadFile::new())
        .tool(write_file)
        .tool(shell)
        .policy(policy)
        .limits(config.limits.limits())
        // Ein Nutzer, ein Rechner: nach einem Absturz soll `resume` sofort weitermachen können.
        .owner("can-cli")
        .store_arc(store.clone());
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
