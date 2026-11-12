//! Google Gemini: über die Gemini API (Schlüssel) oder über Vertex AI (Zugangstoken).

mod decode;
pub(crate) mod wire;

use async_trait::async_trait;
use can_core::{Capabilities, Provider, ProviderError, ProviderStream, Request, Secret};

use crate::auth::TokenSource;
use crate::sse::sse_stream;

/// Vorgabe für die Basis-URL der Gemini API.
pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

enum Endpoint {
    /// Gemini API mit Schlüssel im Header `x-goog-api-key`.
    Api {
        base_url: String,
        api_key: Option<Secret<String>>,
    },
    /// Vertex AI mit Projekt, Region und Zugangstoken.
    Vertex {
        base_url: String,
        project: String,
        location: String,
        token: TokenSource,
    },
}

/// Provider für Gemini-Modelle.
pub struct Gemini {
    client: reqwest::Client,
    endpoint: Endpoint,
    capabilities: Capabilities,
}

fn default_capabilities() -> Capabilities {
    Capabilities {
        tools: true,
        parallel_tool_calls: true,
        images: true,
        prompt_cache: true,
        context_window: 1_000_000,
        max_output: 8_192,
    }
}

/// Basis-URL von Vertex AI für eine Region.
pub(crate) fn vertex_base(location: &str) -> String {
    if location == "global" {
        "https://aiplatform.googleapis.com/v1".to_owned()
    } else {
        format!("https://{location}-aiplatform.googleapis.com/v1")
    }
}

impl Gemini {
    /// Gemini API; ohne Basis-URL gilt die öffentliche.
    pub fn new(base_url: Option<String>, api_key: Option<Secret<String>>) -> Self {
        let base_url = base_url
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned())
            .trim_end_matches('/')
            .to_owned();
        Self {
            client: reqwest::Client::new(),
            endpoint: Endpoint::Api { base_url, api_key },
            capabilities: default_capabilities(),
        }
    }

    /// Gemini über Vertex AI in einem Google-Cloud-Projekt.
    pub fn vertex(
        project: impl Into<String>,
        location: impl Into<String>,
        token: TokenSource,
    ) -> Self {
        let location = location.into();
        let base_url = vertex_base(&location);
        let endpoint = Endpoint::Vertex {
            base_url,
            project: project.into(),
            location,
            token,
        };
        Self {
            client: reqwest::Client::new(),
            endpoint,
            capabilities: default_capabilities(),
        }
    }

    /// Ersetzt die Basis-URL, etwa für einen Proxy oder Tests.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        let url = url.into().trim_end_matches('/').to_owned();
        match &mut self.endpoint {
            Endpoint::Api { base_url, .. } | Endpoint::Vertex { base_url, .. } => *base_url = url,
        }
        self
    }

    /// Setzt die Fähigkeiten, die für alle Modelle gemeldet werden.
    pub fn with_capabilities(mut self, capabilities: Capabilities) -> Self {
        self.capabilities = capabilities;
        self
    }
}

#[async_trait]
impl Provider for Gemini {
    fn capabilities(&self, _model: &str) -> Capabilities {
        self.capabilities
    }

    async fn stream(&self, req: Request) -> Result<ProviderStream, ProviderError> {
        let body = serde_json::to_vec(&wire::body(&req))
            .map_err(|error| ProviderError::InvalidRequest(error.to_string()))?;
        let builder = match &self.endpoint {
            Endpoint::Api { base_url, api_key } => {
                let url = format!(
                    "{base_url}/models/{}:streamGenerateContent?alt=sse",
                    req.model
                );
                let builder = self.client.post(url);
                match api_key {
                    Some(key) => builder.header("x-goog-api-key", key.expose()),
                    None => builder,
                }
            }
            Endpoint::Vertex {
                base_url,
                project,
                location,
                token,
            } => {
                let url = format!(
                    "{base_url}/projects/{project}/locations/{location}/publishers/google/models/{}:streamGenerateContent?alt=sse",
                    req.model
                );
                self.client
                    .post(url)
                    .bearer_auth(token.token().await?.expose())
            }
        };
        let builder = builder
            .header("content-type", "application/json")
            .body(body);
        Ok(sse_stream(
            crate::http::send(builder).await?,
            decode::Decoder::default(),
        ))
    }
}
