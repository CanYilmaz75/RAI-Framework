//! Anthropic Messages API mit SSE-Streaming.

pub(crate) mod decode;
pub(crate) mod wire;

use async_trait::async_trait;
use can_core::{Capabilities, Provider, ProviderError, ProviderStream, Request, Secret};

use crate::sse::sse_stream;

/// Vorgabe für die Basis-URL, wenn keine Umgebungsvariable gesetzt ist.
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const API_VERSION: &str = "2023-06-01";

/// Provider für `POST {base}/v1/messages`.
pub struct Anthropic {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<Secret<String>>,
    capabilities: Capabilities,
}

impl Anthropic {
    /// Provider für die angegebene Basis-URL.
    pub fn new(base_url: impl Into<String>, api_key: Option<Secret<String>>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            api_key,
            capabilities: Capabilities {
                tools: true,
                parallel_tool_calls: true,
                images: true,
                prompt_cache: true,
                context_window: 200_000,
                max_output: 8_192,
            },
        }
    }

    /// Liest `CAN_ANTHROPIC_BASE_URL` und `CAN_ANTHROPIC_API_KEY`, ersatzweise
    /// `ANTHROPIC_BASE_URL` und `ANTHROPIC_API_KEY`, aus der Umgebung.
    pub fn from_env() -> Result<Self, ProviderError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Wie [`Anthropic::from_env`], aber mit austauschbarer Quelle für die Variablen.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ProviderError> {
        let first = |names: [&str; 2]| {
            names
                .iter()
                .find_map(|name| lookup(name))
                .filter(|value| !value.trim().is_empty())
        };
        let base_url = first(["CAN_ANTHROPIC_BASE_URL", "ANTHROPIC_BASE_URL"])
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());
        if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
            return Err(ProviderError::InvalidRequest(format!(
                "Basis-URL muss mit http:// oder https:// beginnen: {base_url}"
            )));
        }
        let api_key = first(["CAN_ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY"]).map(Secret::new);
        Ok(Self::new(base_url, api_key))
    }

    /// Setzt die Fähigkeiten, die für alle Modelle dieses Endpunkts gemeldet werden.
    pub fn with_capabilities(mut self, capabilities: Capabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Die verwendete Basis-URL ohne abschließenden Schrägstrich.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

#[async_trait]
impl Provider for Anthropic {
    fn capabilities(&self, _model: &str) -> Capabilities {
        self.capabilities
    }

    async fn stream(&self, req: Request) -> Result<ProviderStream, ProviderError> {
        let body = serde_json::to_vec(&wire::body(&req, self.capabilities.max_output))
            .map_err(|error| ProviderError::InvalidRequest(error.to_string()))?;
        let mut builder = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .header("anthropic-version", API_VERSION)
            .body(body);
        if let Some(key) = &self.api_key {
            builder = builder.header("x-api-key", key.expose());
        }
        Ok(sse_stream(
            crate::http::send(builder).await?,
            decode::Decoder::default(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    #[test]
    fn from_lookup_uses_defaults_and_fallbacks() {
        let provider = Anthropic::from_lookup(lookup(&[])).unwrap();
        assert_eq!(provider.base_url(), DEFAULT_BASE_URL);
        assert!(provider.api_key.is_none());
        let pairs = [
            ("ANTHROPIC_BASE_URL", "http://localhost:9/"),
            ("CAN_ANTHROPIC_API_KEY", "k"),
        ];
        let provider = Anthropic::from_lookup(lookup(&pairs)).unwrap();
        assert_eq!(provider.base_url(), "http://localhost:9");
        assert_eq!(
            provider.api_key.as_ref().map(|key| key.expose().as_str()),
            Some("k")
        );
        assert!(Anthropic::from_lookup(lookup(&[("CAN_ANTHROPIC_BASE_URL", "x")])).is_err());
        assert!(Anthropic::from_env().is_ok());
    }

    #[test]
    fn capabilities_are_configurable() {
        let provider = Anthropic::new("http://x", None);
        let mut capabilities = provider.capabilities("m");
        assert!(capabilities.images && capabilities.prompt_cache);
        capabilities.context_window = 1_000;
        assert_eq!(
            provider
                .with_capabilities(capabilities)
                .capabilities("m")
                .context_window,
            1_000
        );
    }

    #[tokio::test]
    async fn unreachable_host_is_a_network_error() {
        let provider = Anthropic::new("http://127.0.0.1:1", None);
        assert!(matches!(
            provider.stream(Request::default()).await,
            Err(ProviderError::Network(_))
        ));
    }
}
