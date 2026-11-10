//! OpenAI-kompatible Chat-API mit SSE-Streaming. Deckt auch Ollama und viele gehostete Modelle ab.

mod decode;
mod wire;

use async_trait::async_trait;
use can_core::{Capabilities, Provider, ProviderError, ProviderStream, Request, Secret};
use futures::StreamExt;
use std::time::Duration;

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
                images: false,
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

fn network(error: reqwest::Error) -> ProviderError {
    ProviderError::Network(Box::new(error))
}

/// Ordnet eine HTTP-Fehlerantwort einem `ProviderError` zu.
pub(crate) fn error_for_status(
    status: u16,
    retry_after: Option<&str>,
    body: &str,
) -> ProviderError {
    let message = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value["error"]["message"].as_str().map(str::to_owned))
        .unwrap_or_else(|| body.chars().take(500).collect());
    let lower = message.to_lowercase();
    match status {
        401 | 403 => ProviderError::Auth,
        429 => ProviderError::RateLimited {
            retry_after: retry_after
                .and_then(|v| v.trim().parse().ok())
                .map(Duration::from_secs),
        },
        400 | 413 if lower.contains("context") || lower.contains("too long") => {
            ProviderError::ContextTooLong { limit: 0 }
        }
        500..=599 => ProviderError::Overloaded,
        _ => ProviderError::InvalidRequest(format!("HTTP {status}: {message}")),
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
        let response = builder.send().await.map_err(network)?;
        let status = response.status();
        if !status.is_success() {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let text = response.text().await.unwrap_or_default();
            return Err(error_for_status(
                status.as_u16(),
                retry_after.as_deref(),
                &text,
            ));
        }
        let bytes = response
            .bytes_stream()
            .map(|item| item.map(|b| b.to_vec()).map_err(network));
        Ok(decode::decode_body(bytes))
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

    #[test]
    fn http_errors_map_to_provider_errors() {
        assert!(matches!(
            error_for_status(401, None, ""),
            ProviderError::Auth
        ));
        assert!(matches!(
            error_for_status(429, Some("7"), ""),
            ProviderError::RateLimited { retry_after: Some(d) } if d.as_secs() == 7
        ));
        assert!(matches!(
            error_for_status(503, None, ""),
            ProviderError::Overloaded
        ));
        let body = r#"{"error":{"message":"This model's maximum context length is 8192 tokens"}}"#;
        assert!(matches!(
            error_for_status(400, None, body),
            ProviderError::ContextTooLong { .. }
        ));
        let other = error_for_status(404, None, r#"{"error":{"message":"model not found"}}"#);
        assert_eq!(
            other.to_string(),
            "ungültige Anfrage: HTTP 404: model not found"
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
