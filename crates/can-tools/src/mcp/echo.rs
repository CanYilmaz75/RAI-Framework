//! Kleiner MCP-Server für Tests und zum Ausprobieren: kennt `echo`, `fail` und `picture`.

use serde_json::{Value, json};

/// Beantwortet eine JSON-RPC-Nachricht. Benachrichtigungen ergeben `None`.
pub fn handle(message: &Value) -> Option<Value> {
    let id = message.get("id")?.clone();
    let reply = |result: Value| Some(json!({"jsonrpc": "2.0", "id": id, "result": result}));
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    match message.get("method").and_then(Value::as_str).unwrap_or("") {
        "initialize" => reply(json!({
            "protocolVersion": params.get("protocolVersion").cloned().unwrap_or(Value::Null),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "can-mcp-echo", "version": env!("CARGO_PKG_VERSION")},
        })),
        // Zwei Seiten, damit der Client das Blättern zeigen muss.
        "tools/list" if params.get("cursor").is_none() => reply(json!({
            "tools": [{
                "name": "echo",
                "description": "Gibt den Text zurück.",
                "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]},
            }],
            "nextCursor": "seite-2",
        })),
        "tools/list" => reply(json!({"tools": [
            {"name": "fail", "description": "Scheitert immer.", "inputSchema": {"type": "object"}},
            {"name": "picture", "inputSchema": {"type": "object"}},
        ]})),
        "tools/call" => match params.get("name").and_then(Value::as_str) {
            Some("echo") => {
                let text = params
                    .pointer("/arguments/text")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                reply(
                    json!({"content": [{"type": "text", "text": format!("echo: {text}")}], "isError": false}),
                )
            }
            Some("fail") => reply(
                json!({"content": [{"type": "text", "text": "absichtlich gescheitert"}], "isError": true}),
            ),
            Some("picture") => reply(
                json!({"content": [{"type": "image", "data": "Zm9v", "mimeType": "image/png"}]}),
            ),
            _ => Some(
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32602, "message": "Unbekanntes Tool"}}),
            ),
        },
        _ => Some(
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "Unbekannte Methode"}}),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_requests_and_ignores_notifications() {
        assert_eq!(
            handle(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})),
            None
        );
        let listed = handle(&json!({"id": 1, "method": "tools/list"})).unwrap();
        assert_eq!(listed["result"]["tools"][0]["name"], "echo");
        let second =
            handle(&json!({"id": 2, "method": "tools/list", "params": {"cursor": "seite-2"}}))
                .unwrap();
        assert_eq!(second["result"]["tools"].as_array().unwrap().len(), 2);
        let unknown = handle(&json!({"id": 3, "method": "gibt/es/nicht"})).unwrap();
        assert_eq!(unknown["error"]["code"], -32601);
    }
}
