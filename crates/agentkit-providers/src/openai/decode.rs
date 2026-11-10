//! Übersetzt den SSE-Strom der Chat-Completions-API in normalisierte Chunks.

use std::collections::{HashSet, VecDeque};

use agentkit_core::{CallId, Chunk, ProviderError, ProviderStream, StopReason, Usage};
use futures::{Stream, StreamExt};
use serde_json::Value;

use crate::sse::SseParser;

type Item = Result<Chunk, ProviderError>;

/// Zustand der Übersetzung eines einzelnen Antwort-Stroms.
#[derive(Default)]
pub(crate) struct Decoder {
    started: HashSet<u32>,
    usage: Option<Usage>,
    stop: Option<StopReason>,
    finished: bool,
}

impl Decoder {
    /// Verarbeitet die Nutzdaten eines SSE-Events.
    pub(crate) fn on_data(&mut self, data: &str, out: &mut VecDeque<Item>) {
        if self.finished {
            return;
        }
        let data = data.trim();
        if data == "[DONE]" {
            return self.finish(out);
        }
        let value: Value = match serde_json::from_str(data) {
            Ok(value) => value,
            Err(error) => return self.fail(out, format!("kein gültiges JSON im Stream: {error}")),
        };
        if let Some(error) = value.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unbekannt");
            return self.fail(out, format!("Fehler im Stream: {message}"));
        }
        if let Some(usage) = value.get("usage").filter(|usage| usage.is_object()) {
            let count = |pointer: &str| usage.pointer(pointer).and_then(Value::as_u64).unwrap_or(0);
            self.usage = Some(Usage {
                input_tokens: count("/prompt_tokens"),
                output_tokens: count("/completion_tokens"),
                cached_tokens: count("/prompt_tokens_details/cached_tokens"),
            });
        }
        let Some(choice) = value.pointer("/choices/0") else {
            return;
        };
        if let Some(text) = choice.pointer("/delta/content").and_then(Value::as_str)
            && !text.is_empty()
        {
            out.push_back(Ok(Chunk::TextDelta {
                text: text.to_owned(),
            }));
        }
        if let Some(calls) = choice
            .pointer("/delta/tool_calls")
            .and_then(Value::as_array)
        {
            for (position, call) in (0u32..).zip(calls) {
                self.on_tool_call(position, call, out);
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.stop = Some(match reason {
                "stop" => StopReason::EndTurn,
                "tool_calls" | "function_call" => StopReason::ToolUse,
                "length" => StopReason::MaxTokens,
                _ => StopReason::Other,
            });
        }
    }

    /// Der Byte-Strom ist zu Ende. Ohne `finish_reason` ist das ein Protokollfehler.
    pub(crate) fn on_end(&mut self, out: &mut VecDeque<Item>) {
        if self.finished {
            return;
        }
        if self.stop.is_some() {
            self.finish(out);
        } else {
            self.fail(out, "Stream endete ohne finish_reason".to_owned());
        }
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.finished
    }

    fn on_tool_call(&mut self, position: u32, call: &Value, out: &mut VecDeque<Item>) {
        let index = call
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|index| u32::try_from(index).ok())
            .unwrap_or(position);
        let name = call
            .pointer("/function/name")
            .and_then(Value::as_str)
            .unwrap_or("");
        if !name.is_empty() && self.started.insert(index) {
            // Die Kennung des Anbieters wird verworfen: der Verlauf wird bei jeder Anfrage neu
            // gesendet, also genügt eine eigene, in sich stimmige Kennung.
            out.push_back(Ok(Chunk::ToolCallStart {
                index,
                id: CallId::new(),
                name: name.to_owned(),
            }));
        }
        if let Some(delta) = call.pointer("/function/arguments").and_then(Value::as_str)
            && !delta.is_empty()
        {
            out.push_back(Ok(Chunk::ToolCallArgs {
                index,
                delta: delta.to_owned(),
            }));
        }
    }

    fn finish(&mut self, out: &mut VecDeque<Item>) {
        self.finished = true;
        let stop = match self.stop {
            Some(stop) => stop,
            None if self.started.is_empty() => StopReason::EndTurn,
            None => StopReason::ToolUse,
        };
        out.push_back(Ok(Chunk::Done {
            usage: self.usage.unwrap_or_default(),
            stop,
        }));
    }

    fn fail(&mut self, out: &mut VecDeque<Item>, message: String) {
        self.finished = true;
        out.push_back(Err(ProviderError::Protocol(message)));
    }
}

struct State<S> {
    body: S,
    sse: SseParser,
    decoder: Decoder,
    queue: VecDeque<Item>,
}

/// Macht aus einem Strom von Byte-Blöcken den normalisierten Chunk-Strom.
pub(crate) fn decode_body<S>(body: S) -> ProviderStream
where
    S: Stream<Item = Result<Vec<u8>, ProviderError>> + Send + Unpin + 'static,
{
    let state = State {
        body,
        sse: SseParser::new(),
        decoder: Decoder::default(),
        queue: VecDeque::new(),
    };
    Box::pin(futures::stream::unfold(state, |mut state| async move {
        loop {
            if let Some(item) = state.queue.pop_front() {
                return Some((item, state));
            }
            if state.decoder.is_finished() {
                return None;
            }
            match state.body.next().await {
                Some(Ok(bytes)) => {
                    for event in state.sse.feed(&bytes) {
                        state.decoder.on_data(&event.data, &mut state.queue);
                    }
                }
                Some(Err(error)) => {
                    state.decoder.finished = true;
                    state.queue.push_back(Err(error));
                }
                None => {
                    if let Some(event) = state.sse.finish() {
                        state.decoder.on_data(&event.data, &mut state.queue);
                    }
                    state.decoder.on_end(&mut state.queue);
                }
            }
        }
    }))
}

#[cfg(test)]
#[path = "decode_tests.rs"]
mod tests;
