//! Übersetzt eine neutrale `Request` in den JSON-Body der Chat-Completions-API.

use agentkit_core::{ContentBlock, Message, Request, Role};
use serde_json::{Map, Value, json};

/// Baut den Anfrage-Body. Felder aus `extra` überschreiben die erzeugten.
pub(crate) fn body(req: &Request) -> Value {
    let mut messages = Vec::new();
    if let Some(system) = &req.system {
        messages.push(json!({"role": "system", "content": system}));
    }
    for message in &req.messages {
        push_message(message, &mut messages);
    }
    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    body.insert("messages".into(), Value::Array(messages));
    body.insert("stream".into(), json!(true));
    body.insert("stream_options".into(), json!({"include_usage": true}));
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|tool| {
                json!({"type": "function", "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.input_schema,
                }})
            })
            .collect();
        body.insert("tools".into(), Value::Array(tools));
    }
    if let Some(max_output) = req.max_output {
        body.insert("max_tokens".into(), json!(max_output));
    }
    for (key, value) in &req.extra {
        body.insert(key.clone(), value.clone());
    }
    Value::Object(body)
}

fn push_message(message: &Message, out: &mut Vec<Value>) {
    match message.role {
        Role::System => out.push(json!({"role": "system", "content": message.text()})),
        Role::User => out.push(json!({"role": "user", "content": message.text()})),
        Role::Assistant => {
            let tool_calls: Vec<Value> = message
                .tool_uses()
                .into_iter()
                .map(|call| {
                    json!({"id": call.id, "type": "function", "function": {
                        "name": call.name,
                        "arguments": call.input.to_string(),
                    }})
                })
                .collect();
            let text = message.text();
            let mut entry = Map::new();
            entry.insert("role".into(), json!("assistant"));
            entry.insert(
                "content".into(),
                if text.is_empty() {
                    Value::Null
                } else {
                    json!(text)
                },
            );
            if !tool_calls.is_empty() {
                entry.insert("tool_calls".into(), Value::Array(tool_calls));
            }
            out.push(Value::Object(entry));
        }
        Role::Tool => {
            for block in &message.content {
                if let ContentBlock::ToolResult {
                    call_id,
                    output,
                    is_error,
                } = block
                {
                    let text = output.as_text();
                    let content = if *is_error {
                        format!("Fehler: {text}")
                    } else {
                        text
                    };
                    out.push(json!({"role": "tool", "tool_call_id": call_id, "content": content}));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_core::{CallId, Schema, ToolDecl, ToolOutput};

    fn conversation() -> Request {
        let id = CallId::from_parts(1, 1);
        Request {
            model: "m".into(),
            system: Some("Du hilfst.".into()),
            messages: vec![
                Message::user("Lies a.txt"),
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id,
                        name: "read_file".into(),
                        input: json!({"path": "a.txt"}),
                    }],
                },
                Message {
                    role: Role::Tool,
                    content: vec![ContentBlock::ToolResult {
                        call_id: id,
                        output: ToolOutput::text("Inhalt"),
                        is_error: false,
                    }],
                },
            ],
            tools: vec![ToolDecl {
                name: "read_file".into(),
                description: "Liest eine Datei.".into(),
                input_schema: Schema(json!({"type": "object"})),
            }],
            max_output: Some(256),
            extra: Map::new(),
        }
    }

    #[test]
    fn translates_a_tool_round() {
        let id = CallId::from_parts(1, 1).to_string();
        let body = body(&conversation());
        assert_eq!(body["model"], "m");
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], 256);
        assert_eq!(
            body["messages"][0],
            json!({"role": "system", "content": "Du hilfst."})
        );
        assert_eq!(
            body["messages"][1],
            json!({"role": "user", "content": "Lies a.txt"})
        );
        let assistant = &body["messages"][2];
        assert_eq!(assistant["content"], Value::Null);
        assert_eq!(assistant["tool_calls"][0]["id"], id.as_str());
        assert_eq!(
            assistant["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"a.txt\"}"
        );
        assert_eq!(
            body["messages"][3],
            json!({"role": "tool", "tool_call_id": id, "content": "Inhalt"})
        );
        assert_eq!(
            body["tools"][0]["function"]["parameters"],
            json!({"type": "object"})
        );
    }

    #[test]
    fn omits_tools_when_empty_and_marks_errors() {
        let mut request = conversation();
        request.tools.clear();
        request.max_output = None;
        if let ContentBlock::ToolResult { is_error, .. } = &mut request.messages[2].content[0] {
            *is_error = true;
        }
        let body = body(&request);
        assert!(body.get("tools").is_none());
        assert!(body.get("max_tokens").is_none());
        assert_eq!(body["messages"][3]["content"], "Fehler: Inhalt");
    }

    #[test]
    fn extra_overrides_generated_fields() {
        let mut request = conversation();
        request.extra.insert("temperature".into(), json!(0.1));
        request.extra.insert("max_tokens".into(), json!(9));
        let body = body(&request);
        assert_eq!(body["temperature"], 0.1);
        assert_eq!(body["max_tokens"], 9);
    }
}
