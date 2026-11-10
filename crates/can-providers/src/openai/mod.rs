//! OpenAI-kompatible Chat-API mit SSE-Streaming. Deckt auch Ollama und viele gehostete Modelle ab.

mod decode;
mod wire;

use async_trait::async_trait;
use can_core::{Capabilities, Provider, ProviderError, ProviderStream, Request, Secret};

/// Vorgabe für die Basis-URL, wenn keine Umgebungsvariable gesetzt ist.
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// Provider für jede API, die `POST {base}/chat/completions` im OpenAI-Format versteht.
pub struct OpenAiCompat {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<Secret<String>>,
    capabilities: Capabilities,
}

impl OpenAiCompat {
    /// Provider für die angegebene Basis-URL; der Schlüssel ist optional (etwa für Ollama).
    pub fn new(base_url: impl Into<String>, api_key: Option<Secret<String>>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            api_key,
            capabilities: Capabilities {
                tools: true,
                parallel_tool_calls: true,
                images: true,
                prompt_cache: false,
                context_window: 128_000,
                max_output: 4_096,
            },
        }
    }

    /// Liest `CAN_BASE_URL` und `CAN_API_KEY`, ersatzweise `OPENAI_BASE_URL` und
    /// `OPENAI_API_KEY`, aus der Umgebung.
    pub fn from_env() -> Result<Self, ProviderError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Wie [`OpenAiCompat::from_env`], aber mit austauschbarer Quelle für die Variablen.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ProviderError> {
        let first = |names: [&str; 2]| {
            names
                .iter()
                .find_map(|name| lookup(name))
                .filter(|value| !value.trim().is_empty())
        };
        let base_url = first(["CAN_BASE_URL", "OPENAI_BASE_URL"])
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());
        if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
            return Err(ProviderError::InvalidRequest(format!(
                "Basis-URL muss mit http:// oder https:// beginnen: {base_url}"
            )));
        }
        let api_key = first(["CAN_API_KEY", "OPENAI_API_KEY"]).map(Secret::new);
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
impl Provider for OpenAiCompat {
    fn capabilities(&self, _model: &str) -> Capabilities {
        self.capabilities
    }

    async fn stream(&self, req: Request) -> Result<ProviderStream, ProviderError> {
        let body = serde_json::to_vec(&wire::body(&req))
            .map_err(|error| ProviderError::InvalidRequest(error.to_string()))?;
        let mut builder = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .body(body);
        if let Some(key) = &self.api_key {
            builder = builder.bearer_auth(key.expose());
        }
        Ok(decode::decode_body(crate::http::send(builder).await?))
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
        let provider = OpenAiCompat::from_lookup(lookup(&[])).unwrap();
        assert_eq!(provider.base_url(), DEFAULT_BASE_URL);
        assert!(provider.api_key.is_none());

        let pairs = [
            ("OPENAI_BASE_URL", "http://localhost:11434/v1/"),
            ("CAN_API_KEY", "k"),
        ];
        let provider = OpenAiCompat::from_lookup(lookup(&pairs)).unwrap();
        assert_eq!(provider.base_url(), "http://localhost:11434/v1");
        assert_eq!(
            provider.api_key.as_ref().map(|key| key.expose().as_str()),
            Some("k")
        );
    }

    #[test]
    fn from_lookup_rejects_non_http_urls() {
        let result = OpenAiCompat::from_lookup(lookup(&[("CAN_BASE_URL", "ftp://x")]));
        assert!(matches!(result, Err(ProviderError::InvalidRequest(_))));
    }

    #[test]
    fn from_env_builds_a_provider() {
        assert!(OpenAiCompat::from_env().is_ok());
    }

    #[test]
    fn capabilities_are_configurable() {
        let provider = OpenAiCompat::new("http://x", None);
        let mut capabilities = provider.capabilities("m");
        assert!(capabilities.tools);
        capabilities.context_window = 8_000;
        assert_eq!(
            provider
                .with_capabilities(capabilities)
                .capabilities("m")
                .context_window,
            8_000
        );
    }

    #[tokio::test]
    async fn unreachable_host_is_a_network_error() {
        let provider = OpenAiCompat::new("http://127.0.0.1:1", None);
        assert!(matches!(
            provider.stream(Request::default()).await,
            Err(ProviderError::Network(_))
        ));
    }
}
