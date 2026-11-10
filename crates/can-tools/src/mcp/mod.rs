//! MCP-Client: bindet die Tools eines MCP-Servers als `Tool` ein, über stdio oder HTTP.
//!
//! Tool-Namen bekommen das Präfix `<server>__`. Beschreibungen des Servers gelten als Daten,
//! und jedes MCP-Tool hat die Wirkung `External`, fragt also ohne eigene Regel nach.

pub mod echo;
mod http;
mod stdio;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use can_core::{Bytes, Effect, OutputBlock, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec};
use serde_json::{Value, json};

pub use http::HttpTransport;
pub use stdio::StdioTransport;

/// Version des Protokolls, die dieser Client spricht.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Fehler im Umgang mit einem MCP-Server.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("Verbindung zum MCP-Server: {0}")]
    Io(String),
    #[error("MCP-Protokollfehler: {0}")]
    Protocol(String),
    #[error("MCP-Server meldet Fehler {code}: {message}")]
    Server { code: i64, message: String },
}

/// Überträgt JSON-RPC-Nachrichten. Für eine Anfrage (mit `id`) kommt die Antwort zurück,
/// für eine Benachrichtigung `None`.
#[async_trait]
pub trait Transport: Send + Sync {
    async fn send(&self, message: Value) -> Result<Option<Value>, McpError>;
}

/// Verbindung zu einem MCP-Server nach abgeschlossenem Handshake.
pub struct McpClient {
    server: String,
    transport: Box<dyn Transport>,
    next_id: AtomicU64,
}

impl McpClient {
    /// Führt den Handshake über den gegebenen Transport aus. `server` wird zum Namenspräfix.
    pub async fn connect(
        server: impl Into<String>,
        transport: impl Transport + 'static,
    ) -> Result<Arc<Self>, McpError> {
        let client = Self {
            server: server.into(),
            transport: Box::new(transport),
            next_id: AtomicU64::new(1),
        };
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "can", "version": env!("CARGO_PKG_VERSION")},
        });
        client.request("initialize", params).await?;
        client
            .transport
            .send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await?;
        Ok(Arc::new(client))
    }

    /// Startet `program` als MCP-Server und spricht über dessen Standardein- und -ausgabe.
    pub async fn connect_stdio(
        server: impl Into<String>,
        program: &str,
        args: &[String],
    ) -> Result<Arc<Self>, McpError> {
        Self::connect(server, StdioTransport::spawn(program, args)?).await
    }

    /// Spricht mit einem MCP-Server über HTTP (Streamable HTTP).
    pub async fn connect_http(
        server: impl Into<String>,
        url: impl Into<String>,
    ) -> Result<Arc<Self>, McpError> {
        Self::connect(server, HttpTransport::new(url)).await
    }

    /// Name des Servers, zugleich Präfix seiner Tools.
    pub fn server(&self) -> &str {
        &self.server
    }

    /// Sendet eine Anfrage und liefert ihr `result`.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let response = self
            .transport
            .send(message)
            .await?
            .ok_or_else(|| McpError::Protocol(format!("keine Antwort auf {method}")))?;
        if let Some(error) = response.get("error") {
            return Err(McpError::Server {
                code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unbekannt")
                    .to_owned(),
            });
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| McpError::Protocol(format!("Antwort auf {method} ohne result")))
    }

    /// Listet die Tools des Servers auf, über alle Seiten, und hängt sie als `Tool` ein.
    pub async fn tools(self: &Arc<Self>) -> Result<Vec<Arc<dyn Tool>>, McpError> {
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = cursor
                .as_ref()
                .map(|cursor| json!({"cursor": cursor}))
                .unwrap_or_else(|| json!({}));
            let page = self.request("tools/list", params).await?;
            for entry in page
                .get("tools")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let name = entry
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty());
                let name = name.ok_or_else(|| McpError::Protocol("Tool ohne Namen".into()))?;
                let description = entry
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let schema = entry
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object"}));
                let spec = ToolSpec::new(format!("{}__{name}", self.server), description, schema)
                    .with_effect(Effect::External);
                tools.push(Arc::new(McpTool {
                    spec,
                    client: self.clone(),
                    remote_name: name.to_owned(),
                }));
            }
            cursor = page
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if cursor.is_none() {
                return Ok(tools);
            }
        }
    }
}

/// Ein Tool eines MCP-Servers.
struct McpTool {
    spec: ToolSpec,
    client: Arc<McpClient>,
    remote_name: String,
}

/// Übersetzt das Ergebnis von `tools/call` in Text- und Bildblöcke.
fn output_of(result: &Value) -> ToolOutput {
    let mut blocks = Vec::new();
    for item in result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match item.get("type").and_then(Value::as_str) {
            Some("text") => {
                blocks.push(OutputBlock::Text(
                    item.get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                ));
            }
            Some("image") => {
                let data = item
                    .get("data")
                    .and_then(Value::as_str)
                    .and_then(Bytes::from_base64);
                let media_type = item
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("image/png");
                match data {
                    Some(data) => blocks.push(OutputBlock::Image {
                        media_type: media_type.to_owned(),
                        data,
                    }),
                    None => {
                        blocks.push(OutputBlock::Text("[Bild mit ungültigen Daten]".to_owned()))
                    }
                }
            }
            Some(other) => blocks.push(OutputBlock::Text(format!(
                "[Inhalt vom Typ {other} ausgelassen]"
            ))),
            None => {}
        }
    }
    ToolOutput(blocks)
}

#[async_trait]
impl Tool for McpTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, args: Value, cx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let params = json!({"name": self.remote_name, "arguments": args});
        let result = tokio::select! {
            result = self.client.request("tools/call", params) => result,
            _ = cx.cancel.cancelled() => return Err(ToolError::ForModel("Abgebrochen.".into())),
        };
        let result = result.map_err(|error| ToolError::ForModel(error.to_string()))?;
        let output = output_of(&result);
        if result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Err(ToolError::ForModel(output.as_text()));
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_translates_text_images_and_unknown_content() {
        let output = output_of(&json!({"content": [
            {"type": "text", "text": "hallo"},
            {"type": "image", "data": "Zm9v", "mimeType": "image/jpeg"},
            {"type": "image", "data": "***"},
            {"type": "resource", "resource": {}},
        ]}));
        assert_eq!(output.0[0], OutputBlock::Text("hallo".into()));
        assert_eq!(
            output.0[1],
            OutputBlock::Image {
                media_type: "image/jpeg".into(),
                data: Bytes(b"foo".to_vec())
            }
        );
        assert_eq!(
            output.0[2],
            OutputBlock::Text("[Bild mit ungültigen Daten]".into())
        );
        assert_eq!(
            output.0[3],
            OutputBlock::Text("[Inhalt vom Typ resource ausgelassen]".into())
        );
        assert!(output_of(&json!({})).0.is_empty());
    }
}
