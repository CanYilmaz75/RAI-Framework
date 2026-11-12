//! Amazon Bedrock über die Converse-API, angemeldet mit einem Bedrock-API-Schlüssel.
//!
//! Die Antwort wird vollständig abgewartet und dann als Chunks weitergegeben; Text erscheint
//! deshalb erst am Ende eines Schritts. Der Strom-Endpunkt nutzt ein binäres Event-Format, das
//! hier nicht umgesetzt ist.

use async_trait::async_trait;
use can_core::{
    CallId, Capabilities, Chunk, ContentBlock, Message, Provider, ProviderError, ProviderStream,
    Request, Role, Secret, StopReason, Usage,
};
use serde_json::{Map, Value, json};

/// Provider für ein Modell oder Inferenzprofil bei Amazon Bedrock.
pub struct Bedrock {
    client: reqwest::Client,
    base_url: String,
    api_key: Secret<String>,
    capabilities: Capabilities,
}

/// Kodiert eine Modellkennung für den Pfad; Doppelpunkte und Schrägstriche in ARNs bleiben erhalten
/// beziehungsweise werden maskiert.
fn encode(model: &str) -> String {
    model
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b':' => {
                (byte as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

fn image_format(media_type: &str) -> &str {
    media_type
        .strip_prefix("image/")
        .map(|format| if format == "jpg" { "jpeg" } else { format })
        .unwrap_or("png")
}

fn blocks(message: &Message) -> Vec<Value> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) if !text.is_empty() => Some(json!({"text": text})),
            ContentBlock::Image { media_type, data } => {
                Some(json!({"image": {"format": image_format(media_type), "source": {"bytes": data.to_base64()}}}))
            }
            ContentBlock::ToolUse { id, name, input } => {
                let input = if input.is_object() { input.clone() } else { json!({}) };
                Some(json!({"toolUse": {"toolUseId": id, "name": name, "input": input}}))
            }
            ContentBlock::ToolResult { call_id, output, is_error } => Some(json!({"toolResult": {
                "toolUseId": call_id,
                "content": [{"text": output.as_text()}],
                "status": if *is_error { "error" } else { "success" },
            }})),
            _ => None,
        })
        .collect()
}

/// Baut den Body der Converse-API. Nachrichten derselben Rolle werden zusammengelegt, weil die
/// API abwechselnde Rollen verlangt.
pub(crate) fn body(req: &Request) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    for message in &req.messages {
        let role = if message.role == Role::Assistant {
            "assistant"
        } else {
            "user"
        };
        let content = blocks(message);
        if content.is_empty() {
            continue;
        }
        match messages.last_mut() {
            Some(last) if last["role"] == role => {
                if let Some(existing) = last["content"].as_array_mut() {
                    existing.extend(content);
                }
            }
            _ => messages.push(json!({"role": role, "content": content})),
        }
    }
    let mut body = Map::new();
    body.insert("messages".into(), Value::Array(messages));
    if let Some(system) = &req.system {
        body.insert("system".into(), json!([{"text": system}]));
    }
    if let Some(max_output) = req.max_output {
        body.insert("inferenceConfig".into(), json!({"maxTokens": max_output}));
    }
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|tool| json!({"toolSpec": {"name": tool.name, "description": tool.description, "inputSchema": {"json": tool.input_schema}}}))
            .collect();
        body.insert("toolConfig".into(), json!({"tools": tools}));
    }
    for (key, value) in &req.extra {
        body.insert(key.clone(), value.clone());
    }
    Value::Object(body)
}

/// Übersetzt die Antwort der Converse-API in Chunks.
pub(crate) fn chunks(response: &Value) -> Result<Vec<Chunk>, ProviderError> {
    let content = response
        .pointer("/output/message/content")
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::Protocol("Antwort ohne output.message.content".into()))?;
    let mut chunks = Vec::new();
    let mut index = 0;
    for block in content {
        if let Some(text) = block.get("text").and_then(Value::as_str) {
            chunks.push(Chunk::TextDelta {
                text: text.to_owned(),
            });
        } else if let Some(call) = block.get("toolUse") {
            let name = call
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            chunks.push(Chunk::ToolCallStart {
                index,
                id: CallId::new(),
                name,
            });
            let input = call.get("input").cloned().unwrap_or_else(|| json!({}));
            chunks.push(Chunk::ToolCallArgs {
                index,
                delta: input.to_string(),
            });
            index += 1;
        }
    }
    let count = |key: &str| {
        response
            .pointer(&format!("/usage/{key}"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let cached = count("cacheReadInputTokens");
    let usage = Usage {
        input_tokens: count("inputTokens") + cached + count("cacheWriteInputTokens"),
        output_tokens: count("outputTokens"),
        cached_tokens: cached,
    };
    let stop = match response.get("stopReason").and_then(Value::as_str) {
        Some("end_turn" | "stop_sequence") => StopReason::EndTurn,
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::MaxTokens,
        _ => StopReason::Other,
    };
    chunks.push(Chunk::Done { usage, stop });
    Ok(chunks)
}

impl Bedrock {
    /// Provider für eine AWS-Region.
    pub fn new(region: &str, api_key: Secret<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: format!("https://bedrock-runtime.{region}.amazonaws.com"),
            api_key,
            capabilities: Capabilities {
                tools: true,
                parallel_tool_calls: true,
                images: true,
                prompt_cache: false,
                context_window: 200_000,
                max_output: 4_096,
            },
        }
    }

    /// Ersetzt die Basis-URL, etwa für einen VPC-Endpunkt oder Tests.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into().trim_end_matches('/').to_owned();
        self
    }

    /// Setzt die Fähigkeiten, die für alle Modelle gemeldet werden.
    pub fn with_capabilities(mut self, capabilities: Capabilities) -> Self {
        self.capabilities = capabilities;
        self
    }
}

#[async_trait]
impl Provider for Bedrock {
    fn capabilities(&self, _model: &str) -> Capabilities {
        self.capabilities
    }

    async fn stream(&self, req: Request) -> Result<ProviderStream, ProviderError> {
        let url = format!("{}/model/{}/converse", self.base_url, encode(&req.model));
        let body = serde_json::to_vec(&body(&req))
            .map_err(|error| ProviderError::InvalidRequest(error.to_string()))?;
        let builder = self
            .client
            .post(url)
            .bearer_auth(self.api_key.expose())
            .header("content-type", "application/json")
            .body(body);
        let mut bytes = Vec::new();
        let mut stream = crate::http::send(builder).await?;
        while let Some(piece) = futures::StreamExt::next(&mut stream).await {
            bytes.extend(piece?);
        }
        let response: Value = serde_json::from_slice(&bytes)
            .map_err(|error| ProviderError::Protocol(format!("kein gültiges JSON: {error}")))?;
        let chunks = chunks(&response)?;
        Ok(Box::pin(futures::stream::iter(chunks.into_iter().map(Ok))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use can_core::{Assembler, Bytes, Schema, ToolDecl, ToolOutput};

    #[test]
    fn model_ids_and_arns_are_path_safe() {
        assert_eq!(
            encode("anthropic.claude-3-5-sonnet-20241022-v2:0"),
            "anthropic.claude-3-5-sonnet-20241022-v2:0"
        );
        assert_eq!(
            encode("arn:aws:bedrock:eu-central-1:123:inference-profile/eu.anthropic.x"),
            "arn:aws:bedrock:eu-central-1:123:inference-profile%2Feu.anthropic.x"
        );
    }

    #[test]
    fn request_alternates_roles_and_carries_tools_images_and_results() {
        let id = CallId::from_parts(1, 1);
        let request = Request {
            model: "m".into(),
            system: Some("Du hilfst.".into()),
            messages: vec![
                Message {
                    role: Role::User,
                    content: vec![
                        ContentBlock::Text("Bild?".into()),
                        ContentBlock::Image {
                            media_type: "image/jpg".into(),
                            data: Bytes(b"foo".to_vec()),
                        },
                    ],
                },
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id,
                        name: "t".into(),
                        input: json!({"a": 1}),
                    }],
                },
                Message {
                    role: Role::Tool,
                    content: vec![ContentBlock::ToolResult {
                        call_id: id,
                        output: ToolOutput::text("kaputt"),
                        is_error: true,
                    }],
                },
                Message::user("weiter"),
            ],
            tools: vec![ToolDecl {
                name: "t".into(),
                description: "d".into(),
                input_schema: Schema(json!({"type": "object"})),
            }],
            max_output: Some(100),
            extra: Map::new(),
        };
        let body = body(&request);
        assert_eq!(body["system"], json!([{"text": "Du hilfst."}]));
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(
            messages[0]["content"][1],
            json!({"image": {"format": "jpeg", "source": {"bytes": "Zm9v"}}})
        );
        assert_eq!(
            messages[1]["content"][0]["toolUse"]["input"],
            json!({"a": 1})
        );
        assert_eq!(messages[2]["content"][0]["toolResult"]["status"], "error");
        assert_eq!(messages[2]["content"][1], json!({"text": "weiter"}));
        assert_eq!(
            body["toolConfig"]["tools"][0]["toolSpec"]["inputSchema"]["json"],
            json!({"type": "object"})
        );
        assert_eq!(body["inferenceConfig"]["maxTokens"], 100);
    }

    #[test]
    fn response_becomes_chunks() {
        let response = json!({
            "output": {"message": {"role": "assistant", "content": [
                {"text": "Ich lese."},
                {"toolUse": {"toolUseId": "tooluse_1", "name": "read_file", "input": {"path": "a"}}}
            ]}},
            "stopReason": "tool_use",
            "usage": {"inputTokens": 12, "outputTokens": 7, "cacheReadInputTokens": 30}
        });
        let mut assembler = Assembler::default();
        for chunk in chunks(&response).unwrap() {
            assembler.push(chunk).unwrap();
        }
        let (message, usage, stop) = assembler.finish().unwrap();
        assert_eq!(message.text(), "Ich lese.");
        assert_eq!(message.tool_uses()[0].input, json!({"path": "a"}));
        assert_eq!(
            usage,
            Usage {
                input_tokens: 42,
                output_tokens: 7,
                cached_tokens: 30
            }
        );
        assert_eq!(stop, StopReason::ToolUse);
        assert!(chunks(&json!({"fehler": 1})).is_err());
    }
}
