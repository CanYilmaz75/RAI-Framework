//! Auswahl und Aufbau eines Anbieters aus Einstellungen, wie sie in `can.toml` und
//! `can-server.toml` stehen. Schlüssel stehen nie in den Einstellungen, nur Variablennamen.

use std::sync::Arc;

use can_core::{Provider, ProviderError, Secret};
use serde::Deserialize;

use crate::auth::TokenSource;
use crate::presets::{PRESETS, preset};
use crate::{Anthropic, Bedrock, Gemini, OpenAiCompat, VertexAnthropic};

/// Einstellungen eines Anbieters.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProviderSettings {
    /// `openai` (Vorgabe), `anthropic`, `azure`, `gemini`, `vertex`, `vertex_anthropic`,
    /// `bedrock` oder eine OpenAI-kompatible Voreinstellung wie `ollama` oder `groq`.
    #[serde(default)]
    pub kind: String,
    pub base_url: Option<String>,
    /// Variable mit dem Schlüssel; ohne Angabe gilt die übliche Variable des Anbieters.
    pub api_key_env: Option<String>,
    /// AWS-Region für `bedrock`.
    pub region: Option<String>,
    /// Google-Cloud-Projekt für `vertex` und `vertex_anthropic`.
    pub project: Option<String>,
    /// Google-Cloud-Region; Vorgabe `us-central1`.
    pub location: Option<String>,
    /// Deployment für `azure`.
    pub deployment: Option<String>,
    /// API-Version für `azure`; Vorgabe `2024-10-21`.
    pub api_version: Option<String>,
    /// Variable mit einem Zugangstoken für Vertex.
    pub token_env: Option<String>,
    /// Programm, das ein Zugangstoken für Vertex ausgibt; Vorgabe `gcloud auth print-access-token`.
    #[serde(default)]
    pub token_command: Vec<String>,
}

/// Namen aller unterstützten Arten.
pub fn kinds() -> Vec<&'static str> {
    let mut kinds = vec![
        "anthropic",
        "azure",
        "gemini",
        "vertex",
        "vertex_anthropic",
        "bedrock",
    ];
    kinds.extend(PRESETS.iter().map(|preset| preset.name));
    kinds.sort_unstable();
    kinds
}

fn missing(what: &str, kind: &str) -> ProviderError {
    ProviderError::InvalidRequest(format!("Für den Anbieter {kind} fehlt `{what}`."))
}

impl ProviderSettings {
    /// Die Art, mit `openai` als Vorgabe.
    pub fn kind(&self) -> &str {
        if self.kind.is_empty() {
            "openai"
        } else {
            &self.kind
        }
    }

    /// Prüft, ob die Art bekannt ist.
    pub fn validate(&self) -> Result<(), ProviderError> {
        if kinds().contains(&self.kind()) {
            return Ok(());
        }
        Err(ProviderError::InvalidRequest(format!(
            "Unbekannter Anbieter {}; erlaubt: {}",
            self.kind(),
            kinds().join(", ")
        )))
    }

    /// Variablen, deren Werte dieser Anbieter braucht.
    pub fn secret_names(&self) -> Vec<String> {
        let mut names: Vec<String> = [&self.api_key_env, &self.token_env]
            .into_iter()
            .flatten()
            .cloned()
            .collect();
        let defaults: Vec<&str> = match self.kind() {
            "openai" => vec![
                "CAN_API_KEY",
                "OPENAI_API_KEY",
                "CAN_BASE_URL",
                "OPENAI_BASE_URL",
            ],
            "anthropic" => vec![
                "CAN_ANTHROPIC_API_KEY",
                "ANTHROPIC_API_KEY",
                "CAN_ANTHROPIC_BASE_URL",
                "ANTHROPIC_BASE_URL",
            ],
            "azure" => vec!["AZURE_OPENAI_API_KEY"],
            "gemini" => vec!["GEMINI_API_KEY", "GOOGLE_API_KEY"],
            "bedrock" => vec!["AWS_BEARER_TOKEN_BEDROCK"],
            "vertex" | "vertex_anthropic" => vec!["GOOGLE_CLOUD_ACCESS_TOKEN"],
            other => preset(other)
                .and_then(|preset| preset.key_env)
                .into_iter()
                .collect(),
        };
        names.extend(defaults.iter().map(|name| (*name).to_owned()));
        names
    }

    fn key(
        &self,
        lookup: &dyn Fn(&str) -> Option<String>,
        defaults: &[&str],
    ) -> Option<Secret<String>> {
        let named = self.api_key_env.as_deref().and_then(lookup);
        named
            .or_else(|| defaults.iter().find_map(|name| lookup(name)))
            .filter(|key| !key.trim().is_empty())
            .map(Secret::new)
    }

    fn token(&self, lookup: &dyn Fn(&str) -> Option<String>) -> TokenSource {
        let variable = self
            .token_env
            .as_deref()
            .unwrap_or("GOOGLE_CLOUD_ACCESS_TOKEN");
        if let Some(token) = lookup(variable).filter(|token| !token.trim().is_empty()) {
            return TokenSource::Static(Secret::new(token));
        }
        match self.token_command.split_first() {
            Some((program, args)) => TokenSource::command(program.clone(), args.to_vec()),
            None => {
                TokenSource::command("gcloud", vec!["auth".into(), "print-access-token".into()])
            }
        }
    }

    /// Baut den Anbieter. `lookup` liefert den Wert einer Variablen.
    pub fn build(
        &self,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Arc<dyn Provider>, ProviderError> {
        self.validate()?;
        let kind = self.kind();
        let location = self
            .location
            .clone()
            .unwrap_or_else(|| "us-central1".to_owned());
        Ok(match kind {
            "openai" => {
                let source = |name: &str| {
                    if name.ends_with("BASE_URL") && self.base_url.is_some() {
                        return self.base_url.clone();
                    }
                    match &self.api_key_env {
                        Some(variable) if name.ends_with("API_KEY") => lookup(variable),
                        _ => lookup(name),
                    }
                };
                Arc::new(OpenAiCompat::from_lookup(source)?)
            }
            "anthropic" => {
                let source = |name: &str| {
                    if name.ends_with("BASE_URL") && self.base_url.is_some() {
                        return self.base_url.clone();
                    }
                    match &self.api_key_env {
                        Some(variable) if name.ends_with("API_KEY") => lookup(variable),
                        _ => lookup(name),
                    }
                };
                Arc::new(Anthropic::from_lookup(source)?)
            }
            "azure" => {
                let endpoint = self
                    .base_url
                    .as_deref()
                    .ok_or_else(|| missing("base_url", kind))?;
                let deployment = self
                    .deployment
                    .as_deref()
                    .ok_or_else(|| missing("deployment", kind))?;
                let key = self
                    .key(lookup, &["AZURE_OPENAI_API_KEY"])
                    .ok_or_else(|| missing("api_key_env", kind))?;
                Arc::new(OpenAiCompat::azure(
                    endpoint,
                    deployment,
                    self.api_version.as_deref().unwrap_or("2024-10-21"),
                    key,
                ))
            }
            "gemini" => Arc::new(Gemini::new(
                self.base_url.clone(),
                self.key(lookup, &["GEMINI_API_KEY", "GOOGLE_API_KEY"]),
            )),
            "vertex" => {
                let project = self
                    .project
                    .as_deref()
                    .ok_or_else(|| missing("project", kind))?;
                let provider = Gemini::vertex(project, &location, self.token(lookup));
                Arc::new(match &self.base_url {
                    Some(url) => provider.with_base_url(url),
                    None => provider,
                })
            }
            "vertex_anthropic" => {
                let project = self
                    .project
                    .as_deref()
                    .ok_or_else(|| missing("project", kind))?;
                let provider = VertexAnthropic::new(project, &location, self.token(lookup));
                Arc::new(match &self.base_url {
                    Some(url) => provider.with_base_url(url),
                    None => provider,
                })
            }
            "bedrock" => {
                let region = self
                    .region
                    .as_deref()
                    .ok_or_else(|| missing("region", kind))?;
                let key = self
                    .key(lookup, &["AWS_BEARER_TOKEN_BEDROCK"])
                    .ok_or_else(|| missing("api_key_env", kind))?;
                let provider = Bedrock::new(region, key);
                Arc::new(match &self.base_url {
                    Some(url) => provider.with_base_url(url),
                    None => provider,
                })
            }
            other => {
                let preset = preset(other).ok_or_else(|| missing("kind", other))?;
                let base_url = self
                    .base_url
                    .clone()
                    .unwrap_or_else(|| preset.base_url.to_owned());
                let key = self.key(lookup, preset.key_env.as_slice());
                if preset.key_env.is_some() && key.is_none() {
                    return Err(missing("api_key_env", other));
                }
                Arc::new(OpenAiCompat::new(base_url, key))
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(kind: &str) -> ProviderSettings {
        ProviderSettings {
            kind: kind.into(),
            ..ProviderSettings::default()
        }
    }

    fn env(name: &str) -> Option<String> {
        match name {
            "GROQ_API_KEY" | "AWS_BEARER_TOKEN_BEDROCK" | "GEMINI_API_KEY" | "MEIN_KEY" => {
                Some("geheim".into())
            }
            "GOOGLE_CLOUD_ACCESS_TOKEN" => Some("token".into()),
            _ => None,
        }
    }

    #[test]
    fn every_kind_builds_with_what_it_needs() {
        for kind in ["openai", "anthropic", "gemini", "groq", "ollama"] {
            assert!(settings(kind).build(&env).is_ok(), "{kind}");
        }
        let bedrock = ProviderSettings {
            region: Some("eu-central-1".into()),
            ..settings("bedrock")
        };
        assert!(bedrock.build(&env).is_ok());
        let vertex = ProviderSettings {
            project: Some("projekt".into()),
            ..settings("vertex")
        };
        assert!(vertex.build(&env).is_ok());
        let claude = ProviderSettings {
            project: Some("projekt".into()),
            ..settings("vertex_anthropic")
        };
        assert!(claude.build(&env).is_ok());
        let azure = ProviderSettings {
            base_url: Some("https://x.openai.azure.com".into()),
            deployment: Some("d".into()),
            api_key_env: Some("MEIN_KEY".into()),
            ..settings("azure")
        };
        assert!(azure.build(&env).is_ok());
        assert_eq!(ProviderSettings::default().kind(), "openai");
    }

    #[test]
    fn missing_pieces_and_unknown_kinds_are_named() {
        let error = |settings: ProviderSettings| settings.build(&env).err().unwrap().to_string();
        assert!(error(settings("bedrock")).contains("region"));
        assert!(error(settings("vertex")).contains("project"));
        assert!(error(settings("azure")).contains("base_url"));
        assert!(
            error(settings("mistral")).contains("api_key_env"),
            "ohne Schlüssel kein Start"
        );
        assert!(error(settings("gpt")).contains("erlaubt:"));
        let parsed: Result<ProviderSettings, _> =
            serde_json::from_value(serde_json::json!({"api_key": "x"}));
        assert!(
            parsed.is_err(),
            "Schlüssel gehören nicht in die Einstellungen"
        );
    }

    #[test]
    fn secret_names_cover_the_defaults() {
        assert!(
            settings("groq")
                .secret_names()
                .contains(&"GROQ_API_KEY".to_owned())
        );
        let named = ProviderSettings {
            api_key_env: Some("MEIN_KEY".into()),
            ..settings("bedrock")
        };
        assert_eq!(named.secret_names()[0], "MEIN_KEY");
        assert!(kinds().contains(&"vertex_anthropic") && kinds().contains(&"ollama"));
    }
}
