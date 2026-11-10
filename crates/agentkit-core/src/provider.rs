//! Provider-Trait: übersetzt zwischen dem neutralen Datenmodell und einer Anbieter-API.

use std::pin::Pin;
use std::time::Duration;

use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::ids::CallId;
use crate::message::{Message, StopReason, Usage};

/// Beliebiger Fehler aus einer tieferen Schicht.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Normalisierter Antwort-Stream eines Providers.
pub type ProviderStream = Pin<Box<dyn Stream<Item = Result<Chunk, ProviderError>> + Send>>;

/// JSON-Schema als Datenwert.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Schema(pub Value);

/// Was das Modell über ein Tool erfährt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolDecl {
    pub name: String,
    pub description: String,
    pub input_schema: Schema,
}

/// Eine Modellanfrage im neutralen Format.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDecl>,
    pub max_output: Option<u32>,
    /// Anbieterspezifische Parameter, die unverändert durchgereicht werden.
    pub extra: Map<String, Value>,
}

/// Ein Stück des normalisierten Streams; jeder Provider liefert dieselben Typen.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Chunk {
    TextDelta {
        text: String,
    },
    ToolCallStart {
        index: u32,
        id: CallId,
        name: String,
    },
    ToolCallArgs {
        index: u32,
        delta: String,
    },
    Done {
        usage: Usage,
        stop: StopReason,
    },
}

/// Was ein Modell kann.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub tools: bool,
    pub parallel_tool_calls: bool,
    pub images: bool,
    pub prompt_cache: bool,
    pub context_window: u32,
    pub max_output: u32,
}

/// Fehler eines Providers.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProviderError {
    #[error("Rate-Limit erreicht")]
    RateLimited { retry_after: Option<Duration> },
    #[error("Anbieter überlastet")]
    Overloaded,
    #[error("Netzwerkfehler: {0}")]
    Network(#[source] BoxError),
    #[error("Kontext zu lang (Limit {limit} Token)")]
    ContextTooLong { limit: u32 },
    #[error("Anmeldung fehlgeschlagen")]
    Auth,
    #[error("ungültige Anfrage: {0}")]
    InvalidRequest(String),
    /// Unerwartete Antwortform.
    #[error("Protokollfehler: {0}")]
    Protocol(String),
}

/// Ein Modellanbieter.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Fähigkeiten des genannten Modells.
    fn capabilities(&self, model: &str) -> Capabilities;

    /// Startet eine Anfrage und liefert den normalisierten Stream.
    async fn stream(&self, req: Request) -> Result<ProviderStream, ProviderError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_and_chunks_roundtrip() {
        let mut extra = Map::new();
        extra.insert("temperature".into(), json!(0.2));
        let request = Request {
            model: "m".into(),
            system: Some("sys".into()),
            messages: vec![Message::user("hi")],
            tools: vec![ToolDecl {
                name: "t".into(),
                description: "d".into(),
                input_schema: Schema(json!({"type": "object"})),
            }],
            max_output: Some(10),
            extra,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), request);

        let chunks = vec![
            Chunk::TextDelta { text: "a".into() },
            Chunk::ToolCallStart {
                index: 0,
                id: CallId::from_parts(1, 1),
                name: "t".into(),
            },
            Chunk::ToolCallArgs {
                index: 0,
                delta: "{}".into(),
            },
            Chunk::Done {
                usage: Usage::default(),
                stop: StopReason::ToolUse,
            },
        ];
        let json = serde_json::to_string(&chunks).unwrap();
        assert_eq!(serde_json::from_str::<Vec<Chunk>>(&json).unwrap(), chunks);
    }

    #[test]
    fn errors_have_readable_messages() {
        assert_eq!(
            ProviderError::ContextTooLong { limit: 8 }.to_string(),
            "Kontext zu lang (Limit 8 Token)"
        );
        let network = ProviderError::Network("weg".into());
        assert_eq!(network.to_string(), "Netzwerkfehler: weg");
    }
}
