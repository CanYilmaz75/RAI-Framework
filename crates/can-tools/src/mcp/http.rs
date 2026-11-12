//! MCP über HTTP (Streamable HTTP): jede Nachricht ist ein POST; die Antwort kommt als JSON
//! oder als SSE-Strom.

use std::sync::Mutex;

use async_trait::async_trait;
use can_core::Secret;
use serde_json::Value;

use super::{McpError, PROTOCOL_VERSION, Transport};

/// Transport zu einem MCP-Server hinter einer URL.
pub struct HttpTransport {
    client: reqwest::Client,
    url: String,
    /// Vom Server beim Handshake vergebene Sitzung.
    session: Mutex<Option<String>>,
    /// Zugangstoken für den Header `Authorization`.
    bearer: Option<Secret<String>>,
}

fn io(error: impl std::fmt::Display) -> McpError {
    McpError::Io(error.to_string())
}

impl HttpTransport {
    /// Transport zur angegebenen Adresse.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            url: url.into(),
            session: Mutex::new(None),
            bearer: None,
        }
    }
}

impl HttpTransport {
    /// Sendet bei jeder Anfrage dieses Token als `Authorization: Bearer`.
    pub fn with_bearer(mut self, token: Secret<String>) -> Self {
        self.bearer = Some(token);
        self
    }
}

/// Sucht im SSE-Text die Antwort mit der passenden Kennung.
pub(crate) fn reply_from_sse(body: &str, id: &Value) -> Option<Value> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
        .find(|reply| reply.get("id") == Some(id) && reply.get("method").is_none())
}

#[async_trait]
impl Transport for HttpTransport {
    async fn send(&self, message: Value) -> Result<Option<Value>, McpError> {
        let session = self.session.lock().map_err(io)?.clone();
        let mut request = self
            .client
            .post(&self.url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", PROTOCOL_VERSION)
            .body(message.to_string());
        if let Some(token) = &self.bearer {
            request = request.bearer_auth(token.expose());
        }
        if let Some(session) = session {
            request = request.header("mcp-session-id", session);
        }
        let response = request.send().await.map_err(io)?;
        let status = response.status();
        if let Some(session) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
        {
            *self.session.lock().map_err(io)? = Some(session.to_owned());
        }
        let is_sse = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|kind| kind.starts_with("text/event-stream"));
        let body = response.text().await.map_err(io)?;
        if !status.is_success() {
            return Err(McpError::Io(format!(
                "HTTP {}: {}",
                status.as_u16(),
                body.chars().take(200).collect::<String>()
            )));
        }
        let Some(id) = message.get("id") else {
            return Ok(None);
        };
        let reply = if is_sse {
            reply_from_sse(&body, id)
        } else {
            serde_json::from_str(&body).ok()
        };
        reply
            .map(Some)
            .ok_or_else(|| McpError::Protocol("keine passende Antwort im HTTP-Body".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn finds_the_matching_reply_among_sse_events() {
        let body = concat!(
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"sampling/createMessage\"}\n\n",
            "data: kein json\n\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"ok\":true}}\n\n",
        );
        assert_eq!(
            reply_from_sse(body, &json!(7)).unwrap()["result"]["ok"],
            true
        );
        assert_eq!(reply_from_sse(body, &json!(8)), None);
    }
}
