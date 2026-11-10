//! Konfigurationsdatei `can.toml`. Secrets stehen nie in der Datei, nur der Name der
//! Umgebungsvariablen, aus der sie kommen.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use can_core::{Limits, Rule};
use serde::Deserialize;

use crate::CliError;

/// Inhalt von `can.toml`. Unbekannte Schlüssel sind ein Fehler, etwa ein `api_key`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub model: Option<String>,
    pub system: Option<String>,
    pub workdir: Option<PathBuf>,
    pub db: Option<PathBuf>,
    #[serde(default)]
    pub provider: ProviderConfig,
    #[serde(default)]
    pub limits: LimitsConfig,
    #[serde(default)]
    pub policy: PolicyConfig,
    #[serde(default)]
    pub http: HttpConfig,
    #[serde(default)]
    pub mcp: McpConfig,
}

/// Welche API angesprochen wird.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    #[default]
    Openai,
    Anthropic,
}

impl std::str::FromStr for ProviderKind {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "openai" => Ok(Self::Openai),
            "anthropic" => Ok(Self::Anthropic),
            other => Err(format!(
                "unbekannter Provider {other}; erlaubt: openai, anthropic"
            )),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    #[serde(default)]
    pub kind: ProviderKind,
    pub base_url: Option<String>,
    /// Name der Umgebungsvariablen mit dem Schlüssel.
    pub api_key_env: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LimitsConfig {
    pub max_steps: Option<u32>,
    pub max_tokens: Option<u64>,
    pub timeout_secs: Option<u64>,
    pub max_tool_calls_per_step: Option<u32>,
}

impl LimitsConfig {
    /// Die Vorgaben, überschrieben durch die gesetzten Werte.
    pub fn limits(&self) -> Limits {
        let defaults = Limits::default();
        Limits {
            max_steps: self.max_steps.unwrap_or(defaults.max_steps),
            max_tokens: self.max_tokens.unwrap_or(defaults.max_tokens),
            timeout: self
                .timeout_secs
                .map(Duration::from_secs)
                .unwrap_or(defaults.timeout),
            max_tool_calls_per_step: self
                .max_tool_calls_per_step
                .unwrap_or(defaults.max_tool_calls_per_step),
        }
    }
}

/// Regeln zusätzlich zu den sicheren Vorgaben.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PolicyConfig {
    #[serde(default)]
    pub rules: Vec<Rule>,
}

/// Ohne freigegebene Hosts gibt es das Tool `http_fetch` nicht.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HttpConfig {
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: BTreeMap<String, McpServer>,
}

/// Ein MCP-Server: entweder `command` (stdio) oder `url` (HTTP).
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct McpServer {
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    pub url: Option<String>,
}

impl Config {
    /// Liest die Konfiguration aus TOML-Text.
    pub fn parse(text: &str) -> Result<Self, CliError> {
        let config: Self = toml::from_str(text)
            .map_err(|error| CliError::Config(format!("Konfiguration ungültig: {error}")))?;
        for (name, server) in &config.mcp.servers {
            if server.command.is_some() == server.url.is_some() {
                return Err(CliError::Config(format!(
                    "MCP-Server {name}: genau eines von `command` und `url` angeben"
                )));
            }
            if name.contains("__") || name.is_empty() {
                return Err(CliError::Config(format!(
                    "MCP-Server {name}: der Name darf kein `__` enthalten"
                )));
            }
        }
        Ok(config)
    }

    /// Liest die angegebene Datei. Ohne Angabe gilt `can.toml` im aktuellen Verzeichnis, falls
    /// vorhanden; sonst die leere Konfiguration.
    pub fn load(path: Option<&Path>) -> Result<Self, CliError> {
        let path = match path {
            Some(path) => path.to_owned(),
            None if Path::new("can.toml").exists() => PathBuf::from("can.toml"),
            None => return Ok(Self::default()),
        };
        let text = std::fs::read_to_string(&path).map_err(|error| {
            CliError::Config(format!("{} nicht lesbar: {error}", path.display()))
        })?;
        Self::parse(&text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use can_core::{Decision, Effect};

    const FULL: &str = r#"
model = "mein-modell"
system = "Du hilfst."
workdir = "projekt"
db = "sitzungen.db"

[provider]
kind = "anthropic"
base_url = "http://localhost:9"
api_key_env = "MEIN_SCHLUESSEL"

[limits]
max_steps = 20
timeout_secs = 90

[[policy.rules]]
tool = "write_file"
args = { path = "notizen/*" }
decision = "allow"

[[policy.rules]]
effect = "external"
decision = "deny"

[http]
allowed_hosts = ["example.com", "*.example.org"]

[mcp.servers.dateien]
command = "mcp-dateien"
args = ["--root", "."]

[mcp.servers.suche]
url = "http://localhost:8080/mcp"
"#;

    #[test]
    fn full_file_parses() {
        let config = Config::parse(FULL).unwrap();
        assert_eq!(config.model.as_deref(), Some("mein-modell"));
        assert_eq!(config.provider.kind, ProviderKind::Anthropic);
        assert_eq!(
            config.provider.api_key_env.as_deref(),
            Some("MEIN_SCHLUESSEL")
        );
        let limits = config.limits.limits();
        assert_eq!(
            (
                limits.max_steps,
                limits.timeout.as_secs(),
                limits.max_tokens
            ),
            (20, 90, 200_000)
        );
        assert_eq!(
            config.policy.rules[0],
            Rule::tool("write_file")
                .arg("path", "notizen/*")
                .decide(Decision::Allow)
        );
        assert_eq!(
            config.policy.rules[1],
            Rule::effect(Effect::External).decide(Decision::Deny)
        );
        assert_eq!(config.http.allowed_hosts.len(), 2);
        assert_eq!(config.mcp.servers["dateien"].args, ["--root", "."]);
        assert_eq!(
            config.mcp.servers["suche"].url.as_deref(),
            Some("http://localhost:8080/mcp")
        );
    }

    #[test]
    fn shipped_example_file_is_valid() {
        let config = Config::parse(include_str!("../../../can.example.toml")).unwrap();
        assert_eq!(config.policy.rules.len(), 2);
        assert_eq!(config.limits.limits(), Limits::default());
    }

    #[test]
    fn empty_file_is_all_defaults() {
        let config = Config::parse("").unwrap();
        assert_eq!(config, Config::default());
        assert_eq!(config.provider.kind, ProviderKind::Openai);
        assert_eq!(config.limits.limits(), Limits::default());
    }

    #[test]
    fn secrets_and_typos_are_rejected() {
        let secret = Config::parse("[provider]\napi_key = \"sk-geheim\"").unwrap_err();
        assert!(secret.to_string().contains("api_key"), "{secret}");
        assert!(Config::parse("modell = \"x\"").is_err());
        assert!(Config::parse("[provider]\nkind = \"gemini\"").is_err());
        assert!(Config::parse("model = ").is_err());
    }

    #[test]
    fn mcp_servers_need_exactly_one_transport_and_a_clean_name() {
        let both = "[mcp.servers.a]\ncommand = \"x\"\nurl = \"http://x\"";
        assert!(
            Config::parse(both)
                .unwrap_err()
                .to_string()
                .contains("genau eines")
        );
        assert!(Config::parse("[mcp.servers.a]\nargs = []").is_err());
        assert!(
            Config::parse("[mcp.servers.a__b]\ncommand = \"x\"")
                .unwrap_err()
                .to_string()
                .contains("__")
        );
    }

    #[test]
    fn load_reads_files_and_reports_missing_ones() {
        let path =
            std::env::temp_dir().join(format!("can-config-{}.toml", can_core::SessionId::new()));
        std::fs::write(&path, "model = \"aus-datei\"").unwrap();
        assert_eq!(
            Config::load(Some(&path)).unwrap().model.as_deref(),
            Some("aus-datei")
        );
        std::fs::remove_file(&path).unwrap();
        assert!(
            Config::load(Some(&path))
                .unwrap_err()
                .to_string()
                .contains("nicht lesbar")
        );
        assert_eq!(
            "anthropic".parse::<ProviderKind>(),
            Ok(ProviderKind::Anthropic)
        );
        assert!("x".parse::<ProviderKind>().is_err());
    }
}
