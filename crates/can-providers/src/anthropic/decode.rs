//! Übersetzt den SSE-Strom der Messages API in normalisierte Chunks.

use std::collections::VecDeque;

use can_core::{CallId, Chunk, ProviderError, StopReason, Usage};
use serde_json::Value;

use crate::sse::{Item, SseDecoder, SseEvent};

/// Zustand der Übersetzung eines einzelnen Antwort-Stroms.
#[derive(Default)]
pub(crate) struct Decoder {
    usage: Usage,
    stop: Option<StopReason>,
    finished: bool,
}

impl Decoder {
    fn fail(&mut self, out: &mut VecDeque<Item>, error: ProviderError) {
        self.finished = true;
        out.push_back(Err(error));
    }
}

fn index(value: &Value) -> u32 {
    value
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|index| u32::try_from(index).ok())
        .unwrap_or(0)
}

impl SseDecoder for Decoder {
    fn on_event(&mut self, event: &SseEvent, out: &mut VecDeque<Item>) {
        if self.finished {
            return;
        }
        let value: Value = match serde_json::from_str(event.data.trim()) {
            Ok(value) => value,
            Err(error) => {
                return self.fail(
                    out,
                    ProviderError::Protocol(format!("kein gültiges JSON im Stream: {error}")),
                );
            }
        };
        let count = |pointer: &str| value.pointer(pointer).and_then(Value::as_u64).unwrap_or(0);
        match value.get("type").and_then(Value::as_str).unwrap_or("") {
            "message_start" => {
                let cached = count("/message/usage/cache_read_input_tokens");
                let written = count("/message/usage/cache_creation_input_tokens");
                self.usage.input_tokens = count("/message/usage/input_tokens") + cached + written;
                self.usage.cached_tokens = cached;
                self.usage.output_tokens = count("/message/usage/output_tokens");
            }
            "content_block_start" => {
                match value.pointer("/content_block/type").and_then(Value::as_str) {
                    Some("tool_use") => {
                        let name = value
                            .pointer("/content_block/name")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        // Die Kennung des Anbieters wird verworfen; der Verlauf trägt eine eigene.
                        let id = CallId::new();
                        out.push_back(Ok(Chunk::ToolCallStart {
                            index: index(&value),
                            id,
                            name: name.to_owned(),
                        }));
                    }
                    Some("text") => {
                        if let Some(text) =
                            value.pointer("/content_block/text").and_then(Value::as_str)
                            && !text.is_empty()
                        {
                            out.push_back(Ok(Chunk::TextDelta {
                                text: text.to_owned(),
                            }));
                        }
                    }
                    _ => {}
                }
            }
            "content_block_delta" => match value.pointer("/delta/type").and_then(Value::as_str) {
                Some("text_delta") => {
                    let text = value
                        .pointer("/delta/text")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if !text.is_empty() {
                        out.push_back(Ok(Chunk::TextDelta {
                            text: text.to_owned(),
                        }));
                    }
                }
                Some("input_json_delta") => {
                    let delta = value
                        .pointer("/delta/partial_json")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if !delta.is_empty() {
                        out.push_back(Ok(Chunk::ToolCallArgs {
                            index: index(&value),
                            delta: delta.to_owned(),
                        }));
                    }
                }
                _ => {}
            },
            "message_delta" => {
                if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop = Some(match reason {
                        "end_turn" | "stop_sequence" => StopReason::EndTurn,
                        "tool_use" => StopReason::ToolUse,
                        "max_tokens" => StopReason::MaxTokens,
                        _ => StopReason::Other,
                    });
                }
                if value.pointer("/usage/output_tokens").is_some() {
                    self.usage.output_tokens = count("/usage/output_tokens");
                }
            }
            "message_stop" => {
                self.finished = true;
                let stop = self.stop.unwrap_or(StopReason::EndTurn);
                out.push_back(Ok(Chunk::Done {
                    usage: self.usage,
                    stop,
                }));
            }
            "error" => {
                let message = value
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("unbekannt");
                let error = match value.pointer("/error/type").and_then(Value::as_str) {
                    Some("overloaded_error") => ProviderError::Overloaded,
                    Some("rate_limit_error") => ProviderError::RateLimited { retry_after: None },
                    _ => ProviderError::Protocol(format!("Fehler im Stream: {message}")),
                };
                self.fail(out, error);
            }
            // `ping`, `content_block_stop` und künftige Event-Typen.
            _ => {}
        }
    }

    fn on_end(&mut self, out: &mut VecDeque<Item>) {
        if !self.finished {
            self.fail(
                out,
                ProviderError::Protocol("Stream endete ohne message_stop".into()),
            );
        }
    }

    fn is_finished(&self) -> bool {
        self.finished
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sse::sse_stream;
    use can_core::{Assembler, Message};
    use futures::StreamExt;
    use serde_json::json;

    /// Aufgezeichnete Antwort im Anthropic-Format: Text, ein Tool-Aufruf in Fragmenten, Usage.
    pub(crate) const TOOL_STREAM: &str = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":",
        "{\"input_tokens\":12,\"cache_read_input_tokens\":30,\"output_tokens\":1}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,",
        "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: ping\ndata: {\"type\":\"ping\"}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,",
        "\"delta\":{\"type\":\"text_delta\",\"text\":\"Ich lese \"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,",
        "\"delta\":{\"type\":\"text_delta\",\"text\":\"die Datei.\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,",
        "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read_file\",\"input\":{}}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,",
        "\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,",
        "\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"README.md\\\"}\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},",
        "\"usage\":{\"output_tokens\":7}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );

    async fn collect(
        input: &str,
        size: usize,
    ) -> Result<(Message, Usage, StopReason), ProviderError> {
        let pieces: Vec<Result<Vec<u8>, ProviderError>> = input
            .as_bytes()
            .chunks(size)
            .map(|piece| Ok(piece.to_vec()))
            .collect();
        let mut stream = sse_stream(futures::stream::iter(pieces), Decoder::default());
        let mut assembler = Assembler::default();
        while let Some(chunk) = stream.next().await {
            assembler.push(chunk?)?;
        }
        assembler.finish()
    }

    #[tokio::test]
    async fn decodes_text_tool_call_and_usage_for_any_chunk_size() {
        for size in [1, 3, 17, 100, usize::MAX] {
            let (message, usage, stop) = collect(TOOL_STREAM, size).await.unwrap();
            assert_eq!(message.text(), "Ich lese die Datei.", "Blockgröße {size}");
            let calls = message.tool_uses();
            assert_eq!((calls.len(), calls[0].name.as_str()), (1, "read_file"));
            assert_eq!(calls[0].input, json!({"path": "README.md"}));
            assert_eq!(
                usage,
                Usage {
                    input_tokens: 42,
                    output_tokens: 7,
                    cached_tokens: 30
                }
            );
            assert_eq!(stop, StopReason::ToolUse);
        }
    }

    #[tokio::test]
    async fn errors_in_the_stream_map_to_provider_errors() {
        let event = |kind: &str| {
            format!(
                "data: {{\"type\":\"error\",\"error\":{{\"type\":\"{kind}\",\"message\":\"boom\"}}}}\n\n"
            )
        };
        assert!(matches!(
            collect(&event("overloaded_error"), 50).await,
            Err(ProviderError::Overloaded)
        ));
        assert!(matches!(
            collect(&event("rate_limit_error"), 50).await,
            Err(ProviderError::RateLimited { .. })
        ));
        assert!(
            matches!(collect(&event("api_error"), 50).await, Err(ProviderError::Protocol(m)) if m.contains("boom"))
        );
        assert!(matches!(
            collect("data: {kaputt\n\n", 50).await,
            Err(ProviderError::Protocol(_))
        ));
    }

    #[tokio::test]
    async fn truncated_stream_is_a_protocol_error_and_nothing_follows_the_end() {
        let cut = TOOL_STREAM.find("event: message_delta").unwrap();
        assert!(matches!(
            collect(&TOOL_STREAM[..cut], 64).await,
            Err(ProviderError::Protocol(_))
        ));
        let doubled = format!("{TOOL_STREAM}{TOOL_STREAM}");
        assert_eq!(collect(&doubled, 64).await.unwrap().0.tool_uses().len(), 1);
    }
}
