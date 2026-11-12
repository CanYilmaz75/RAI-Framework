//! Übersetzt den SSE-Strom von `streamGenerateContent?alt=sse` in normalisierte Chunks.

use std::collections::VecDeque;

use can_core::{CallId, Chunk, ProviderError, StopReason, Usage};
use serde_json::Value;

use crate::sse::{Item, SseDecoder, SseEvent};

/// Zustand der Übersetzung. Gemini liefert Funktionsaufrufe jeweils vollständig in einem Teil.
#[derive(Default)]
pub(crate) struct Decoder {
    calls: u32,
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
        if let Some(error) = value.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unbekannt");
            let error = match error.get("code").and_then(Value::as_u64) {
                Some(429) => ProviderError::RateLimited { retry_after: None },
                Some(500..=599) => ProviderError::Overloaded,
                _ => ProviderError::Protocol(format!("Fehler im Stream: {message}")),
            };
            return self.fail(out, error);
        }
        if let Some(usage) = value.get("usageMetadata") {
            let count = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
            self.usage = Usage {
                input_tokens: count("promptTokenCount"),
                output_tokens: count("candidatesTokenCount") + count("thoughtsTokenCount"),
                cached_tokens: count("cachedContentTokenCount"),
            };
        }
        let Some(candidate) = value.pointer("/candidates/0") else {
            return;
        };
        for part in candidate
            .pointer("/content/parts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            // Gedankenteile (`thought: true`) gehen nicht an den Nutzer.
            if part.get("thought").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            if let Some(call) = part.get("functionCall") {
                let index = self.calls;
                self.calls += 1;
                let name = call
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                out.push_back(Ok(Chunk::ToolCallStart {
                    index,
                    id: CallId::new(),
                    name,
                }));
                if let Some(signature) = part.get("thoughtSignature").and_then(Value::as_str) {
                    out.push_back(Ok(Chunk::ToolCallSignature {
                        index,
                        signature: signature.to_owned(),
                    }));
                }
                let args = call
                    .get("args")
                    .cloned()
                    .unwrap_or_else(|| Value::Object(Default::default()));
                out.push_back(Ok(Chunk::ToolCallArgs {
                    index,
                    delta: args.to_string(),
                }));
            } else if let Some(text) = part.get("text").and_then(Value::as_str)
                && !text.is_empty()
            {
                out.push_back(Ok(Chunk::TextDelta {
                    text: text.to_owned(),
                }));
            }
        }
        if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
            self.stop = Some(match reason {
                "STOP" if self.calls > 0 => StopReason::ToolUse,
                "STOP" => StopReason::EndTurn,
                "MAX_TOKENS" => StopReason::MaxTokens,
                _ => StopReason::Other,
            });
        }
    }

    fn on_end(&mut self, out: &mut VecDeque<Item>) {
        if self.finished {
            return;
        }
        self.finished = true;
        match self.stop {
            Some(stop) => out.push_back(Ok(Chunk::Done {
                usage: self.usage,
                stop,
            })),
            None => out.push_back(Err(ProviderError::Protocol(
                "Stream endete ohne finishReason".into(),
            ))),
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
    use can_core::{Assembler, ContentBlock, Message};
    use futures::StreamExt;
    use serde_json::json;

    pub(crate) const TOOL_STREAM: &str = concat!(
        "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"Ich lese \"}]}}]}\r\n\r\n",
        "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"die Datei.\"},",
        "{\"functionCall\":{\"name\":\"read_file\",\"args\":{\"path\":\"README.md\"}},\"thoughtSignature\":\"sig-abc\"}]},",
        "\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":42,\"candidatesTokenCount\":5,",
        "\"thoughtsTokenCount\":2,\"cachedContentTokenCount\":30}}\r\n\r\n",
    );

    async fn collect(
        input: &str,
        size: usize,
    ) -> Result<(Message, Usage, StopReason), ProviderError> {
        let pieces: Vec<Result<Vec<u8>, ProviderError>> = input
            .as_bytes()
            .chunks(size)
            .map(|p| Ok(p.to_vec()))
            .collect();
        let mut stream = sse_stream(futures::stream::iter(pieces), Decoder::default());
        let mut assembler = Assembler::default();
        while let Some(chunk) = stream.next().await {
            assembler.push(chunk?)?;
        }
        assembler.finish()
    }

    #[tokio::test]
    async fn decodes_text_call_signature_and_usage_for_any_chunk_size() {
        for size in [1, 7, 64, usize::MAX] {
            let (message, usage, stop) = collect(TOOL_STREAM, size).await.unwrap();
            assert_eq!(message.text(), "Ich lese die Datei.", "Blockgröße {size}");
            assert_eq!(message.tool_uses()[0].input, json!({"path": "README.md"}));
            assert!(message.content.contains(&ContentBlock::Reasoning {
                text: String::new(),
                signature: Some("sig-abc".into())
            }));
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
    async fn thoughts_are_hidden_and_errors_mapped() {
        let input = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"geheim\",\"thought\":true},{\"text\":\"Antwort\"}]},",
            "\"finishReason\":\"MAX_TOKENS\"}]}\n\n",
        );
        let (message, _, stop) = collect(input, 9).await.unwrap();
        assert_eq!(
            (message.text().as_str(), stop),
            ("Antwort", StopReason::MaxTokens)
        );
        let busy = "data: {\"error\":{\"code\":503,\"message\":\"overloaded\"}}\n\n";
        assert!(matches!(
            collect(busy, 100).await,
            Err(ProviderError::Overloaded)
        ));
        let cut = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"halb\"}]}}]}\n\n";
        assert!(matches!(
            collect(cut, 100).await,
            Err(ProviderError::Protocol(_))
        ));
    }
}
