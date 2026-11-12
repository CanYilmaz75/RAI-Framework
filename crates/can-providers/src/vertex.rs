//! Claude-Modelle über Vertex AI: das Format der Anthropic API, aber Adresse und Anmeldung von
//! Google Cloud.

use async_trait::async_trait;
use can_core::{Capabilities, Provider, ProviderError, ProviderStream, Request};

use crate::anthropic::{decode::Decoder, wire};
use crate::auth::TokenSource;
use crate::gemini::vertex_base;
use crate::sse::sse_stream;

/// Version des Anthropic-Formats, die Vertex AI erwartet.
const VERTEX_ANTHROPIC_VERSION: &str = "vertex-2023-10-16";

/// Provider für Anthropic-Modelle auf Vertex AI.
pub struct VertexAnthropic {
    client: reqwest::Client,
    base_url: String,
    project: String,
    location: String,
    token: TokenSource,
    capabilities: Capabilities,
}

impl VertexAnthropic {
    /// Provider für ein Google-Cloud-Projekt und eine Region.
    pub fn new(
        project: impl Into<String>,
        location: impl Into<String>,
        token: TokenSource,
    ) -> Self {
        let location = location.into();
        Self {
            client: reqwest::Client::new(),
            base_url: vertex_base(&location),
            project: project.into(),
            location,
            token,
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

    /// Ersetzt die Basis-URL, etwa für Tests.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into().trim_end_matches('/').to_owned();
        self
    }
}

#[async_trait]
impl Provider for VertexAnthropic {
    fn capabilities(&self, _model: &str) -> Capabilities {
        self.capabilities
    }

    async fn stream(&self, req: Request) -> Result<ProviderStream, ProviderError> {
        let mut body = wire::body(&req, self.capabilities.max_output);
        // Das Modell steht in der Adresse; dafür verlangt Vertex die eigene Formatversion.
        if let Some(object) = body.as_object_mut() {
            object.remove("model");
            object.insert("anthropic_version".into(), VERTEX_ANTHROPIC_VERSION.into());
        }
        let url = format!(
            "{}/projects/{}/locations/{}/publishers/anthropic/models/{}:streamRawPredict",
            self.base_url, self.project, self.location, req.model
        );
        let body = serde_json::to_vec(&body)
            .map_err(|error| ProviderError::InvalidRequest(error.to_string()))?;
        let builder = self
            .client
            .post(url)
            .bearer_auth(self.token.token().await?.expose())
            .header("content-type", "application/json")
            .body(body);
        Ok(sse_stream(
            crate::http::send(builder).await?,
            Decoder::default(),
        ))
    }
}
