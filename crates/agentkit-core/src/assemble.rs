//! Setzt die Chunks eines Provider-Streams zu einer vollständigen Nachricht zusammen.
//!
//! Das Zusammenfügen von Argument-Fragmenten zu JSON passiert nur hier, nicht in jedem Provider.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::ids::CallId;
use crate::message::{ContentBlock, Message, Role, StopReason, Usage};
use crate::provider::{Chunk, ProviderError};

struct PartialCall {
    id: CallId,
    name: String,
    args: String,
}

/// Sammelt Chunks und liefert am Ende Nachricht, Verbrauch und Stop-Grund.
#[derive(Default)]
pub struct Assembler {
    text: String,
    calls: BTreeMap<u32, PartialCall>,
    done: Option<(Usage, StopReason)>,
}

impl Assembler {
    /// Nimmt den nächsten Chunk auf.
    pub fn push(&mut self, chunk: Chunk) -> Result<(), ProviderError> {
        if self.done.is_some() {
            return Err(ProviderError::Protocol("Chunk nach Nachrichtenende".into()));
        }
        match chunk {
            Chunk::TextDelta { text } => self.text.push_str(&text),
            Chunk::ToolCallStart { index, id, name } => {
                if self
                    .calls
                    .insert(
                        index,
                        PartialCall {
                            id,
                            name,
                            args: String::new(),
                        },
                    )
                    .is_some()
                {
                    return Err(ProviderError::Protocol(format!(
                        "Tool-Aufruf {index} doppelt begonnen"
                    )));
                }
            }
            Chunk::ToolCallArgs { index, delta } => match self.calls.get_mut(&index) {
                Some(call) => call.args.push_str(&delta),
                None => {
                    return Err(ProviderError::Protocol(format!(
                        "Argumente für unbekannten Tool-Aufruf {index}"
                    )));
                }
            },
            Chunk::Done { usage, stop } => self.done = Some((usage, stop)),
        }
        Ok(())
    }

    /// Schließt die Nachricht ab. Ein Stream ohne `Done` ist ein Protokollfehler.
    ///
    /// Argumente, die kein gültiges JSON sind, bleiben als Zeichenkette erhalten; die
    /// Schema-Prüfung meldet sie dem Modell dann als Tool-Fehler statt den Lauf abzubrechen.
    pub fn finish(self) -> Result<(Message, Usage, StopReason), ProviderError> {
        let Some((usage, stop)) = self.done else {
            return Err(ProviderError::Protocol(
                "Stream endete ohne Nachrichtenende".into(),
            ));
        };
        let mut content = Vec::new();
        if !self.text.is_empty() {
            content.push(ContentBlock::Text(self.text));
        }
        for call in self.calls.into_values() {
            let input = if call.args.trim().is_empty() {
                Value::Object(Map::new())
            } else {
                serde_json::from_str(&call.args).unwrap_or(Value::String(call.args))
            };
            content.push(ContentBlock::ToolUse {
                id: call.id,
                name: call.name,
                input,
            });
        }
        Ok((
            Message {
                role: Role::Assistant,
                content,
            },
            usage,
            stop,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn done() -> Chunk {
        Chunk::Done {
            usage: Usage {
                input_tokens: 1,
                output_tokens: 2,
                cached_tokens: 0,
            },
            stop: StopReason::ToolUse,
        }
    }

    fn run(chunks: Vec<Chunk>) -> Result<(Message, Usage, StopReason), ProviderError> {
        let mut assembler = Assembler::default();
        for chunk in chunks {
            assembler.push(chunk)?;
        }
        assembler.finish()
    }

    #[test]
    fn joins_text_and_argument_fragments() {
        let id = CallId::from_parts(1, 1);
        let (message, usage, stop) = run(vec![
            Chunk::TextDelta {
                text: "Ich ".into(),
            },
            Chunk::TextDelta {
                text: "lese.".into(),
            },
            Chunk::ToolCallStart {
                index: 0,
                id,
                name: "read_file".into(),
            },
            Chunk::ToolCallArgs {
                index: 0,
                delta: "{\"pa".into(),
            },
            Chunk::ToolCallArgs {
                index: 0,
                delta: "th\":\"a.txt\"}".into(),
            },
            done(),
        ])
        .unwrap();
        assert_eq!(message.text(), "Ich lese.");
        assert_eq!(message.tool_uses()[0].input, json!({"path": "a.txt"}));
        assert_eq!(usage.output_tokens, 2);
        assert_eq!(stop, StopReason::ToolUse);
    }

    #[test]
    fn interleaved_calls_keep_index_order() {
        let (a, b) = (CallId::from_parts(1, 1), CallId::from_parts(1, 2));
        let (message, ..) = run(vec![
            Chunk::ToolCallStart {
                index: 1,
                id: b,
                name: "b".into(),
            },
            Chunk::ToolCallStart {
                index: 0,
                id: a,
                name: "a".into(),
            },
            Chunk::ToolCallArgs {
                index: 1,
                delta: "{\"n\":2}".into(),
            },
            done(),
        ])
        .unwrap();
        let calls = message.tool_uses();
        assert_eq!((calls[0].id, calls[1].id), (a, b));
        assert_eq!(calls[0].input, json!({}));
        assert_eq!(calls[1].input, json!({"n": 2}));
    }

    #[test]
    fn invalid_json_is_kept_as_string() {
        let id = CallId::from_parts(1, 1);
        let (message, ..) = run(vec![
            Chunk::ToolCallStart {
                index: 0,
                id,
                name: "t".into(),
            },
            Chunk::ToolCallArgs {
                index: 0,
                delta: "{kaputt".into(),
            },
            done(),
        ])
        .unwrap();
        assert_eq!(message.tool_uses()[0].input, json!("{kaputt"));
    }

    #[test]
    fn protocol_violations_are_errors() {
        assert!(matches!(
            run(vec![Chunk::TextDelta { text: "a".into() }]),
            Err(ProviderError::Protocol(_))
        ));
        assert!(matches!(
            run(vec![
                Chunk::ToolCallArgs {
                    index: 3,
                    delta: "x".into()
                },
                done()
            ]),
            Err(ProviderError::Protocol(_))
        ));
        assert!(matches!(
            run(vec![done(), done()]),
            Err(ProviderError::Protocol(_))
        ));
        let id = CallId::from_parts(1, 1);
        let twice = vec![
            Chunk::ToolCallStart {
                index: 0,
                id,
                name: "t".into(),
            },
            Chunk::ToolCallStart {
                index: 0,
                id,
                name: "t".into(),
            },
        ];
        assert!(matches!(run(twice), Err(ProviderError::Protocol(_))));
    }
}
