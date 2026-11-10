//! Konfigurationsdatei des Dienstes. Schlüssel stehen nie in der Datei, nur die Namen der
//! Umgebungsvariablen, aus denen sie kommen.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use can_core::{Limits, MemoryStore, Provider, Rule, RuleSet, Store};
use can_providers::{Anthropic, OpenAiCompat, Retry};
use can_runtime::{Agent, SystemClock};
use can_store::{PostgresStore, SqliteStore};
use can_tools::{HttpFetch, ReadFile, shell, write_file};
use serde::Deserialize;

use crate::auth::{Directory, Quota, Role};
use crate::meta::{MemoryDirectory, SessionDirectory};
use crate::pgmeta::PostgresDirectory;

/// Fehler beim Lesen oder Anwenden der Konfiguration.
#[derive(Debug, thiserror::Error)]
#[error("Konfiguration: {0}")]
pub struct ConfigError(pub String);

fn invalid(message: impl std::fmt::Display) -> ConfigError {
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
    #[serde(default)]
    pub agents: Vec<AgentConfig>,
    #[serde(default)]
    pub tenants: Vec<TenantConfig>,
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
    /// Aus `read_file`, `write_file`, `shell`, `http_fetch`.
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
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TenantConfig {
    pub name: String,
    #[serde(flatten)]
    pub quota: Option<Quota>,
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
            if !matches!(agent.provider.as_str(), "" | "openai" | "anthropic") {
                return Err(invalid(format!(
                    "Agent {}: unbekannter Provider {}",
                    agent.name, agent.provider
                )));
            }
            for tool in &agent.tools {
                if !matches!(
                    tool.as_str(),
                    "read_file" | "write_file" | "shell" | "http_fetch"
                ) {
                    return Err(invalid(format!(
                        "Agent {}: unbekanntes Tool {tool}",
                        agent.name
                    )));
                }
            }
        }
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

    /// Öffnet Event-Store und Sitzungsverzeichnis.
    pub async fn open_store(
        &self,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<(Arc<dyn Store>, Arc<dyn SessionDirectory>), ConfigError> {
        Ok(match &self.store {
            StoreConfig::Memory => (
                Arc::new(MemoryStore::new()),
                Arc::new(MemoryDirectory::new()),
            ),
            StoreConfig::Sqlite { path } => (
                Arc::new(SqliteStore::open(path).map_err(invalid)?),
                Arc::new(MemoryDirectory::new()),
            ),
            StoreConfig::Postgres { url_env } => {
                let url =
                    lookup(url_env).ok_or_else(|| invalid(format!("Variable {url_env} fehlt")))?;
                let store = PostgresStore::connect(&url).await.map_err(invalid)?;
                let sessions = PostgresDirectory::connect(&url).await.map_err(invalid)?;
                (Arc::new(store), Arc::new(sessions))
            }
        })
    }

    /// Baut alle Agenten auf dem gegebenen Store.
    pub fn agents(
        &self,
        store: &Arc<dyn Store>,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<HashMap<String, Agent>, ConfigError> {
        let worker = self
            .worker
            .clone()
            .or_else(|| lookup("HOSTNAME"))
            .unwrap_or_else(|| "can-server".to_owned());
        let mut agents = HashMap::new();
        for config in &self.agents {
            let agent = config.build(store.clone(), &worker, &lookup)?;
            agents.insert(config.name.clone(), agent);
        }
        Ok(agents)
    }
}

impl AgentConfig {
    fn provider(
        &self,
        lookup: &impl Fn(&str) -> Option<String>,
    ) -> Result<Arc<dyn Provider>, ConfigError> {
        let source = |name: &str| {
            if name.ends_with("BASE_URL") && self.base_url.is_some() {
                return self.base_url.clone();
            }
            match &self.api_key_env {
                Some(variable) if name.ends_with("API_KEY") => lookup(variable),
                _ => lookup(name),
            }
        };
        let clock = Arc::new(SystemClock);
        Ok(match self.provider.as_str() {
            "anthropic" => Arc::new(Retry::new(
                Anthropic::from_lookup(source).map_err(invalid)?,
                clock,
            )),
            _ => Arc::new(Retry::new(
                OpenAiCompat::from_lookup(source).map_err(invalid)?,
                clock,
            )),
        })
    }

    fn build(
        &self,
        store: Arc<dyn Store>,
        worker: &str,
        lookup: &impl Fn(&str) -> Option<String>,
    ) -> Result<Agent, ConfigError> {
        let defaults = Limits::default();
        let limits = Limits {
            max_steps: self.max_steps.unwrap_or(defaults.max_steps),
            max_tokens: self.max_tokens.unwrap_or(defaults.max_tokens),
            timeout: self
                .timeout_secs
                .map(Duration::from_secs)
                .unwrap_or(defaults.timeout),
            ..defaults
        };
        let mut policy = RuleSet::safe_defaults();
        policy.rules.extend(self.rules.iter().cloned());
        let mut builder = Agent::builder()
            .provider_arc(self.provider(lookup)?)
            .model(&self.model)
            .policy(policy)
            .limits(limits)
            .store_arc(store)
            .owner(worker);
        if let Some(system) = &self.system {
            builder = builder.system(system);
        }
        if let Some(workdir) = &self.workdir {
            builder = builder.workdir(workdir);
        }
        for tool in &self.tools {
            builder = match tool.as_str() {
                "read_file" => builder.tool(ReadFile::new()),
                "write_file" => builder.tool(write_file),
                "shell" => builder.tool(shell),
                _ => builder.tool(HttpFetch::new(self.http_allowed_hosts.clone())),
            };
        }
        builder
            .build()
            .map_err(|error| invalid(format!("Agent {}: {error}", self.name)))
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
