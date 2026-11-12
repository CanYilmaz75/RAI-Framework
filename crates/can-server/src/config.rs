//! Konfigurationsdatei des Dienstes. Schlüssel stehen nie in der Datei, nur die Namen der
//! Umgebungsvariablen, aus denen sie kommen.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use can_core::{Decision, InMemoryBank, MemoryBank, MemoryStore, Pricing, Rule, Store};
use can_runtime::Agent;
use can_store::{Cipher, PostgresStore, SqliteStore};
use can_tools::Sandbox;
use serde::Deserialize;

use crate::auth::{Directory, Quota, Role};
use crate::meta::{MemoryDirectory, SessionDirectory};
use crate::pgmeta::PostgresDirectory;
use crate::schedule::ScheduleConfig;
use crate::secrets::SecretSource;

/// Fehler beim Lesen oder Anwenden der Konfiguration.
#[derive(Debug, thiserror::Error)]
#[error("Konfiguration: {0}")]
pub struct ConfigError(pub String);

pub(crate) fn invalid(message: impl std::fmt::Display) -> ConfigError {
    ConfigError(message.to_string())
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Adresse, auf der der Dienst lauscht.
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Kennung dieses Workers für die Sitzungssperren; Vorgabe ist der Rechnername.
    pub worker: Option<String>,
    #[serde(default)]
    pub store: StoreConfig,
    /// Variable mit dem Schlüssel (64 Hex-Zeichen) für die Verschlüsselung im Ruhezustand.
    pub encryption_key_env: Option<String>,
    /// Woher die genannten Secrets kommen.
    #[serde(default)]
    pub secrets: SecretSource,
    #[serde(default)]
    pub agents: Vec<AgentConfig>,
    #[serde(default)]
    pub tenants: Vec<TenantConfig>,
    /// Geplante Läufe.
    #[serde(default)]
    pub schedules: Vec<ScheduleConfig>,
}

fn default_listen() -> String {
    "127.0.0.1:8080".to_owned()
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub enum StoreConfig {
    /// Nur zum Ausprobieren: alles ist nach dem Neustart weg.
    #[default]
    Memory,
    /// Eine Datei, ein Prozess. Das Sitzungsverzeichnis bleibt dabei im Arbeitsspeicher.
    Sqlite { path: PathBuf },
    /// Für den Betrieb mit mehreren Workern. `url_env` nennt die Variable mit der Adresse.
    Postgres { url_env: String },
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    pub name: String,
    pub model: String,
    pub system: Option<String>,
    /// `openai` (Vorgabe) oder `anthropic`.
    #[serde(default)]
    pub provider: String,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    /// AWS-Region für `bedrock`.
    pub region: Option<String>,
    /// Google-Cloud-Projekt und -Region für `vertex` und `vertex_anthropic`.
    pub project: Option<String>,
    pub location: Option<String>,
    /// Deployment und API-Version für `azure`.
    pub deployment: Option<String>,
    pub api_version: Option<String>,
    /// Zugangstoken für Vertex: Variable oder Programm, das es ausgibt.
    pub token_env: Option<String>,
    #[serde(default)]
    pub token_command: Vec<String>,
    /// Aus `read_file`, `write_file`, `edit_file`, `ls`, `glob`, `grep`, `write_todos`, `shell`,
    /// `http_fetch`.
    #[serde(default)]
    pub tools: Vec<String>,
    pub workdir: Option<PathBuf>,
    #[serde(default)]
    pub http_allowed_hosts: Vec<String>,
    /// Regeln zusätzlich zu den sicheren Vorgaben.
    #[serde(default)]
    pub rules: Vec<Rule>,
    pub max_steps: Option<u32>,
    pub max_tokens: Option<u64>,
    pub timeout_secs: Option<u64>,
    /// Datei, deren Inhalt als Gedächtnis immer im System-Prompt steht.
    pub memory_file: Option<PathBuf>,
    /// Tools `remember`, `recall` und `forget` für das durchsuchbare Gedächtnis des Mandanten.
    #[serde(default)]
    pub memory: bool,
    /// Ordner mit Skills (`<name>/SKILL.md`); das Modell lädt sie bei Bedarf.
    pub skills_dir: Option<PathBuf>,
    /// Preise des Modells je Million Token, für Kosten und Kostenbudget.
    pub pricing: Option<Pricing>,
    /// Sandbox für `shell`. Ohne Angabe läuft `shell` ungeschützt.
    pub sandbox: Option<Sandbox>,
    /// Eigenes Arbeitsverzeichnis pro Sitzung; im Dienst die Vorgabe.
    #[serde(default = "yes")]
    pub isolate_workdir: bool,
    /// Tools, deren Ausgaben als vertraulich gelten.
    #[serde(default)]
    pub confidential_tools: Vec<String>,
    /// `ask` oder `deny` für externe Tools, sobald vertrauliche Daten im Verlauf stehen.
    pub exfiltration_guard: Option<Decision>,
    /// Begriffe, die der Inhaltsfilter in Eingaben und Antworten sperrt.
    #[serde(default)]
    pub blocked_terms: Vec<String>,
    /// MCP-Server dieses Agenten.
    #[serde(default)]
    pub mcp: BTreeMap<String, McpServerConfig>,
    /// Tools als WebAssembly-Module, jedes mit eigenen Freigaben.
    #[serde(default)]
    pub wasm_tools: Vec<can_tools::WasmToolConfig>,
    /// Weiter oben definierte Agenten, die dieser als Tool aufruft und auf deren Antwort er wartet.
    #[serde(default)]
    pub subagents: Vec<String>,
    /// Weiter oben definierte Agenten, die dieser im Hintergrund arbeiten lässt; je fünf Tools
    /// `<name>__start`, `__check`, `__message`, `__cancel`, `__list`.
    #[serde(default)]
    pub background_agents: Vec<String>,
}

fn yes() -> bool {
    true
}

/// Ein MCP-Server: entweder `command` (stdio) oder `url` (HTTP, optional mit Token).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    pub url: Option<String>,
    /// Variable mit dem Zugangstoken für den HTTP-Server.
    pub token_env: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TenantConfig {
    pub name: String,
    #[serde(flatten)]
    pub quota: Option<Quota>,
    /// Adresse, die nach jedem Lauf des Mandanten per POST benachrichtigt wird.
    pub webhook_url: Option<String>,
    #[serde(default)]
    pub keys: Vec<KeyConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyConfig {
    /// Name der Umgebungsvariablen mit dem API-Schlüssel.
    pub key_env: String,
    pub user: String,
    pub role: Role,
}

impl ServerConfig {
    /// Liest die Konfiguration aus TOML-Text.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(text).map_err(invalid)?;
        let mut names = std::collections::HashSet::new();
        for agent in &config.agents {
            if !names.insert(&agent.name) {
                return Err(invalid(format!("Agent {} doppelt", agent.name)));
            }
            agent
                .provider_settings()
                .validate()
                .map_err(|error| invalid(format!("Agent {}: {error}", agent.name)))?;
            for tool in &agent.tools {
                if !matches!(
                    tool.as_str(),
                    "read_file"
                        | "write_file"
                        | "edit_file"
                        | "ls"
                        | "glob"
                        | "grep"
                        | "write_todos"
                        | "shell"
                        | "http_fetch"
                ) {
                    return Err(invalid(format!(
                        "Agent {}: unbekanntes Tool {tool}",
                        agent.name
                    )));
                }
            }
        }
        crate::schedule::validate(&config)?;
        Ok(config)
    }

    /// Schlüssel und Mandanten. `lookup` liefert den Wert einer Umgebungsvariablen.
    pub fn directory(
        &self,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Directory, ConfigError> {
        let mut directory = Directory::new();
        for tenant in &self.tenants {
            directory = directory.tenant(&tenant.name, tenant.quota.unwrap_or_default());
            for key in &tenant.keys {
                let value = lookup(&key.key_env)
                    .filter(|value| value.len() >= 16)
                    .ok_or_else(|| {
                        invalid(format!(
                            "Variable {} fehlt oder ist kürzer als 16 Zeichen",
                            key.key_env
                        ))
                    })?;
                directory = directory.key(&value, &tenant.name, &key.user, key.role);
            }
        }
        Ok(directory)
    }

    /// Preise je Agent.
    pub fn prices(&self) -> HashMap<String, Pricing> {
        self.agents
            .iter()
            .filter_map(|agent| Some((agent.name.clone(), agent.pricing?)))
            .collect()
    }

    /// Webhook je Mandant.
    pub fn webhooks(&self) -> HashMap<String, String> {
        self.tenants
            .iter()
            .filter_map(|tenant| Some((tenant.name.clone(), tenant.webhook_url.clone()?)))
            .collect()
    }

    /// Namen aller Secrets, die diese Konfiguration nennt.
    pub fn secret_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.encryption_key_env.iter().cloned().collect();
        if let StoreConfig::Postgres { url_env } = &self.store {
            names.push(url_env.clone());
        }
        for agent in &self.agents {
            names.extend(agent.provider_settings().secret_names());
            names.extend(
                agent
                    .wasm_tools
                    .iter()
                    .flat_map(|tool| tool.secrets.iter().cloned()),
            );
            names.extend(
                agent
                    .mcp
                    .values()
                    .filter_map(|server| server.token_env.clone()),
            );
        }
        names.extend(
            self.tenants
                .iter()
                .flat_map(|tenant| tenant.keys.iter().map(|key| key.key_env.clone())),
        );
        names.extend(
            [
                "CAN_API_KEY",
                "OPENAI_API_KEY",
                "CAN_ANTHROPIC_API_KEY",
                "ANTHROPIC_API_KEY",
                "HOSTNAME",
            ]
            .map(String::from),
        );
        names.sort();
        names.dedup();
        names
    }

    /// Öffnet Event-Store, Sitzungsverzeichnis und Gedächtnis; das Gedächtnis liegt im selben
    /// Backend wie die Events und wird genauso verschlüsselt.
    pub async fn open_store(
        &self,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Storage, ConfigError> {
        let cipher = match &self.encryption_key_env {
            Some(variable) => {
                let key = lookup(variable)
                    .ok_or_else(|| invalid(format!("Variable {variable} fehlt")))?;
                Some(Cipher::from_hex(&key).map_err(invalid)?)
            }
            None => None,
        };
        Ok(match &self.store {
            StoreConfig::Memory => Storage {
                store: Arc::new(MemoryStore::new()),
                sessions: Arc::new(MemoryDirectory::new()),
                memory: Arc::new(InMemoryBank::new()),
            },
            StoreConfig::Sqlite { path } => {
                let store = SqliteStore::open(path).map_err(invalid)?;
                let store = match cipher {
                    Some(cipher) => store.with_cipher(cipher),
                    None => store,
                };
                let store = Arc::new(store);
                Storage {
                    store: store.clone(),
                    sessions: Arc::new(MemoryDirectory::new()),
                    memory: store,
                }
            }
            StoreConfig::Postgres { url_env } => {
                let url =
                    lookup(url_env).ok_or_else(|| invalid(format!("Variable {url_env} fehlt")))?;
                let store = PostgresStore::connect(&url).await.map_err(invalid)?;
                let store = match cipher {
                    Some(cipher) => store.with_cipher(cipher),
                    None => store,
                };
                let sessions = PostgresDirectory::connect(&url).await.map_err(invalid)?;
                let store = Arc::new(store);
                Storage {
                    store: store.clone(),
                    sessions: Arc::new(sessions),
                    memory: store,
                }
            }
        })
    }

    /// Baut alle Agenten auf dem gegebenen Store.
    pub async fn agents(
        &self,
        store: &Arc<dyn Store>,
        memory: &Arc<dyn MemoryBank>,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<HashMap<String, Agent>, ConfigError> {
        let worker = self
            .worker
            .clone()
            .or_else(|| lookup("HOSTNAME"))
            .unwrap_or_else(|| "can-server".to_owned());
        let mut agents = HashMap::new();
        for config in &self.agents {
            let agent = config
                .build(store.clone(), memory, &worker, &lookup, &agents)
                .await?;
            agents.insert(config.name.clone(), agent);
        }
        Ok(agents)
    }
}

/// Was `open_store` öffnet.
pub struct Storage {
    pub store: Arc<dyn Store>,
    pub sessions: Arc<dyn SessionDirectory>,
    pub memory: Arc<dyn MemoryBank>,
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
