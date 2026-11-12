//! Übersetzt eine neutrale `Request` in den Body von `generateContent` (Gemini API und Vertex).

use std::collections::HashMap;

use can_core::{ContentBlock, Message, Request, Role, ToolOutput};
use serde_json::{Map, Value, json};

/// Gemini versteht eine OpenAPI-Teilmenge von JSON-Schema: lokale Verweise werden eingesetzt,
/// Typ-Listen mit `null` werden zu `nullable`, nicht unterstützte Schlüssel entfallen.
pub(crate) fn schema(value: &Value) -> Value {
    fn walk(value: &Value, root: &Value, depth: u32) -> Value {
        let Value::Object(object) = value else {
            return value.clone();
        };
        if depth > 32 {
            return json!({});
        }
        if let Some(target) = object
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|r| r.strip_prefix('#'))
            && let Some(resolved) = root.pointer(target)
        {
            return walk(resolved, root, depth + 1);
        }
        let mut out = Map::new();
        for (key, item) in object {
            match key.as_str() {
                "type" => match item {
                    Value::Array(types) => {
                        let real: Vec<&Value> = types
                            .iter()
                            .filter(|t| t.as_str() != Some("null"))
                            .collect();
                        if real.len() < types.len() {
                            out.insert("nullable".into(), json!(true));
                        }
                        if let Some(first) = real.first() {
                            out.insert("type".into(), (*first).clone());
                        }
                    }
                    other => {
                        out.insert("type".into(), other.clone());
                    }
                },
                "properties" => {
                    let properties = item.as_object().map(|map| {
                        map.iter()
                            .map(|(name, schema)| (name.clone(), walk(schema, root, depth + 1)))
                            .collect::<Map<_, _>>()
                    });
                    out.insert(key.clone(), Value::Object(properties.unwrap_or_default()));
                }
                "items" => {
                    out.insert(key.clone(), walk(item, root, depth + 1));
                }
                "anyOf" => {
                    let options = item
                        .as_array()
                        .map(|list| list.iter().map(|s| walk(s, root, depth + 1)).collect());
                    out.insert(key.clone(), Value::Array(options.unwrap_or_default()));
                }
                "description" | "required" | "enum" | "format" | "nullable" | "minimum"
                | "maximum" | "minItems" | "maxItems" | "minLength" | "maxLength" | "pattern"
                | "title" => {
                    out.insert(key.clone(), item.clone());
                }
                _ => {}
            }
        }
        if out.get("enum").is_some() && out.get("type").is_none() {
            out.insert("type".into(), json!("string"));
        }
        Value::Object(out)
    }
    walk(value, value, 0)
}

fn result_text(output: &ToolOutput, is_error: bool) -> Value {
    if is_error {
        json!({"error": output.as_text()})
    } else {
        json!({"content": output.as_text()})
    }
}

/// Teile einer Nachricht. `names` ordnet Aufrufkennungen dem Tool-Namen zu, den Gemini in der
/// Antwort auf einen Aufruf verlangt.
fn parts(message: &Message, names: &HashMap<String, String>) -> Vec<Value> {
    let mut parts = Vec::new();
    let mut signature: Option<String> = None;
    for block in &message.content {
        match block {
            ContentBlock::Text(text) if !text.is_empty() => parts.push(json!({"text": text})),
            ContentBlock::Image { media_type, data } => {
                parts.push(
                    json!({"inlineData": {"mimeType": media_type, "data": data.to_base64()}}),
                );
            }
            ContentBlock::Reasoning {
                signature: Some(next),
                ..
            } => signature = Some(next.clone()),
            ContentBlock::ToolUse { name, input, .. } => {
                let args = if input.is_object() {
                    input.clone()
                } else {
                    json!({})
                };
                let mut part = json!({"functionCall": {"name": name, "args": args}});
                if let Some(signature) = signature.take() {
                    part["thoughtSignature"] = json!(signature);
                }
                parts.push(part);
            }
            ContentBlock::ToolResult {
                call_id,
                output,
                is_error,
            } => {
                let name = names.get(&call_id.to_string()).cloned().unwrap_or_default();
                parts.push(json!({"functionResponse": {"name": name, "response": result_text(output, *is_error)}}));
            }
            _ => {}
        }
    }
    parts
}

/// Baut den Anfrage-Body. Felder aus `extra` überschreiben die erzeugten.
pub(crate) fn body(req: &Request) -> Value {
    let mut names = HashMap::new();
    for message in &req.messages {
        for call in message.tool_uses() {
            names.insert(call.id.to_string(), call.name);
        }
    }
    let mut contents: Vec<Value> = Vec::new();
    for message in &req.messages {
        let role = if message.role == Role::Assistant {
            "model"
        } else {
            "user"
        };
        let parts = parts(message, &names);
        if parts.is_empty() {
            continue;
        }
        // Gemini erwartet abwechselnde Rollen; Nachrichten derselben Rolle werden zusammengelegt.
        match contents.last_mut() {
            Some(last) if last["role"] == role => {
                if let Some(existing) = last["parts"].as_array_mut() {
                    existing.extend(parts);
                }
            }
            _ => contents.push(json!({"role": role, "parts": parts})),
        }
    }
    let mut body = Map::new();
    body.insert("contents".into(), Value::Array(contents));
    if let Some(system) = &req.system {
        body.insert(
            "systemInstruction".into(),
            json!({"parts": [{"text": system}]}),
        );
    }
    if !req.tools.is_empty() {
        let declarations: Vec<Value> = req
            .tools
            .iter()
            .map(|tool| json!({"name": tool.name, "description": tool.description, "parameters": schema(&tool.input_schema.0)}))
            .collect();
        body.insert(
            "tools".into(),
            json!([{"functionDeclarations": declarations}]),
        );
    }
    if let Some(max_output) = req.max_output {
        body.insert(
            "generationConfig".into(),
            json!({"maxOutputTokens": max_output}),
        );
    }
    for (key, value) in &req.extra {
        body.insert(key.clone(), value.clone());
    }
    Value::Object(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use can_core::{Bytes, CallId, Schema, ToolDecl};

    #[test]
    fn schema_is_reduced_to_what_gemini_accepts() {
        let input = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Pfad"},
                "lines": {"type": ["integer", "null"], "minimum": 1},
                "range": {"$ref": "#/$defs/Range"},
                "mode": {"enum": ["a", "b"]}
            },
            "required": ["path"],
            "additionalProperties": false,
            "$defs": {"Range": {"type": "object", "properties": {"from": {"type": "integer"}}, "required": ["from"]}}
        });
        assert_eq!(
            schema(&input),
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Pfad"},
                    "lines": {"type": "integer", "nullable": true, "minimum": 1},
                    "range": {"type": "object", "properties": {"from": {"type": "integer"}}, "required": ["from"]},
                    "mode": {"enum": ["a", "b"], "type": "string"}
                },
                "required": ["path"]
            })
        );
        assert_eq!(
            schema(&json!({"$ref": "#"})),
            json!({}),
            "endlose Verweise brechen ab"
        );
    }

    #[test]
    fn translates_roles_tool_round_signature_and_image() {
        let id = CallId::from_parts(1, 1);
        let request = Request {
            model: "gemini".into(),
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
                        ContentBlock::Reasoning {
                            text: String::new(),
                            signature: Some("sig-1".into()),
                        },
                        ContentBlock::ToolUse {
                            id,
                            name: "read_file".into(),
                            input: json!({"path": "a"}),
                        },
                    ],
                },
                Message {
                    role: Role::Tool,
                    content: vec![ContentBlock::ToolResult {
                        call_id: id,
                        output: ToolOutput::text("Inhalt"),
                        is_error: false,
                    }],
                },
                Message::user("Und weiter?"),
            ],
            tools: vec![ToolDecl {
                name: "read_file".into(),
                description: "liest".into(),
                input_schema: Schema(json!({"type": "object", "additionalProperties": false})),
            }],
            max_output: Some(256),
            extra: Map::new(),
        };
        let body = body(&request);
        assert_eq!(
            body["systemInstruction"],
            json!({"parts": [{"text": "Du hilfst."}]})
        );
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(
            contents.len(),
            3,
            "Tool-Ergebnis und Folgefrage liegen in einer Nutzer-Nachricht"
        );
        assert_eq!(
            contents[0]["parts"][1],
            json!({"inlineData": {"mimeType": "image/png", "data": "Zm9v"}})
        );
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(
            contents[1]["parts"][0],
            json!({"functionCall": {"name": "read_file", "args": {"path": "a"}}, "thoughtSignature": "sig-1"})
        );
        assert_eq!(
            contents[2]["parts"][0],
            json!({"functionResponse": {"name": "read_file", "response": {"content": "Inhalt"}}})
        );
        assert_eq!(contents[2]["parts"][1], json!({"text": "Und weiter?"}));
        assert_eq!(
            body["tools"][0]["functionDeclarations"][0]["parameters"],
            json!({"type": "object"})
        );
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 256);
    }
}
