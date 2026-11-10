//! Übersetzt eine neutrale `Request` in den JSON-Body der Messages API.

use can_core::{ContentBlock, Message, OutputBlock, Request, Role, ToolOutput};
use serde_json::{Map, Value, json};

/// Baut den Anfrage-Body. `max_tokens` ist Pflicht; ohne Angabe gilt `default_max_output`.
/// Felder aus `extra` überschreiben die erzeugten.
pub(crate) fn body(req: &Request, default_max_output: u32) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    body.insert(
        "max_tokens".into(),
        json!(req.max_output.unwrap_or(default_max_output)),
    );
    body.insert("stream".into(), json!(true));
    if let Some(system) = &req.system {
        body.insert("system".into(), json!(system));
    }
    let messages: Vec<Value> = req.messages.iter().filter_map(message).collect();
    body.insert("messages".into(), Value::Array(messages));
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|tool| json!({"name": tool.name, "description": tool.description, "input_schema": tool.input_schema}))
            .collect();
        body.insert("tools".into(), Value::Array(tools));
    }
    for (key, value) in &req.extra {
        body.insert(key.clone(), value.clone());
    }
    Value::Object(body)
}

fn image(media_type: &str, data: &can_core::Bytes) -> Value {
    json!({"type": "image", "source": {"type": "base64", "media_type": media_type, "data": data.to_base64()}})
}

fn tool_result_content(output: &ToolOutput) -> Value {
    let parts = output.0.iter().filter_map(|block| match block {
        OutputBlock::Text(text) => Some(json!({"type": "text", "text": text})),
        OutputBlock::Image { media_type, data } => Some(image(media_type, data)),
        _ => None,
    });
    Value::Array(parts.collect())
}

/// Tool-Ergebnisse stehen bei Anthropic in einer Nutzernachricht. Leere Nachrichten entfallen.
fn message(message: &Message) -> Option<Value> {
    let role = if message.role == Role::Assistant {
        "assistant"
    } else {
        "user"
    };
    let content: Vec<Value> = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) if !text.is_empty() => {
                Some(json!({"type": "text", "text": text}))
            }
            ContentBlock::Image { media_type, data } => Some(image(media_type, data)),
            ContentBlock::ToolUse { id, name, input } => {
                // Die API verlangt ein Objekt; alles andere stammt aus kaputtem Modell-JSON.
                let input = if input.is_object() {
                    input.clone()
                } else {
                    json!({})
                };
                Some(json!({"type": "tool_use", "id": id, "name": name, "input": input}))
            }
            ContentBlock::ToolResult {
                call_id,
                output,
                is_error,
            } => Some(json!({
                "type": "tool_result",
                "tool_use_id": call_id,
                "content": tool_result_content(output),
                "is_error": is_error,
            })),
            _ => None,
        })
        .collect();
    (!content.is_empty()).then(|| json!({"role": role, "content": content}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use can_core::{Bytes, CallId, Schema, ToolDecl};

    fn conversation() -> Request {
        let id = CallId::from_parts(1, 1);
        Request {
            model: "m".into(),
            system: Some("Du hilfst.".into()),
            messages: vec![
                Message {
                    role: Role::User,
                    content: vec![
                        ContentBlock::Text("Was zeigt das?".into()),
                        ContentBlock::Image {
                            media_type: "image/png".into(),
                            data: Bytes(b"foo".to_vec()),
                        },
                    ],
                },
                Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::Text(String::new()),
                        ContentBlock::ToolUse {
                            id,
                            name: "read_file".into(),
                            input: json!({"path": "a.txt"}),
                        },
                    ],
                },
                Message {
                    role: Role::Tool,
                    content: vec![ContentBlock::ToolResult {
                        call_id: id,
                        output: ToolOutput::text("Inhalt"),
                        is_error: true,
                    }],
                },
                Message {
                    role: Role::Assistant,
                    content: vec![],
                },
            ],
            tools: vec![ToolDecl {
                name: "read_file".into(),
                description: "Liest eine Datei.".into(),
                input_schema: Schema(json!({"type": "object"})),
            }],
            max_output: None,
            extra: Map::new(),
        }
    }

    #[test]
    fn translates_a_tool_round_with_image() {
        let id = CallId::from_parts(1, 1).to_string();
        let body = body(&conversation(), 8_192);
        assert_eq!(
            (body["model"].as_str(), body["max_tokens"].as_u64()),
            (Some("m"), Some(8_192))
        );
        assert_eq!(body["system"], "Du hilfst.");
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3, "die leere Nachricht entfällt");
        assert_eq!(
            messages[0]["content"][1]["source"],
            json!({"type": "base64", "media_type": "image/png", "data": "Zm9v"})
        );
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(
            messages[1]["content"],
            json!([{"type": "tool_use", "id": id, "name": "read_file", "input": {"path": "a.txt"}}])
        );
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(
            messages[2]["content"][0],
            json!({"type": "tool_result", "tool_use_id": id, "content": [{"type": "text", "text": "Inhalt"}], "is_error": true})
        );
        assert_eq!(
            body["tools"][0],
            json!({"name": "read_file", "description": "Liest eine Datei.", "input_schema": {"type": "object"}})
        );
    }

    #[test]
    fn max_output_extra_and_broken_input() {
        let mut request = conversation();
        request.tools.clear();
        request.max_output = Some(100);
        request.extra.insert("temperature".into(), json!(0.2));
        if let ContentBlock::ToolUse { input, .. } = &mut request.messages[1].content[1] {
            *input = json!("{kaputt");
        }
        let body = body(&request, 8_192);
        assert_eq!(body["max_tokens"], 100);
        assert_eq!(body["temperature"], 0.2);
        assert!(body.get("tools").is_none());
        assert_eq!(body["messages"][1]["content"][0]["input"], json!({}));
    }
}
