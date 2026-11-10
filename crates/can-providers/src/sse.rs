//! Zustandsbehafteter SSE-Parser, der beliebig zerschnittene Byte-Blöcke annimmt.

use std::collections::VecDeque;

use can_core::{Chunk, ProviderError, ProviderStream};
use futures::{Stream, StreamExt};

/// Ein vollständiges Server-Sent Event.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Längste Zeile, die der Parser annimmt.
pub const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

/// Parser für einen SSE-Byte-Strom.
#[derive(Debug, Default)]
pub struct SseParser {
    line: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
    overflow: bool,
}

impl SseParser {
    /// Leerer Parser.
    pub fn new() -> Self {
        Self::default()
    }

    /// Nimmt den nächsten Byte-Block auf und liefert alle damit abgeschlossenen Events.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        let mut out = Vec::new();
        for &byte in bytes {
            if byte == b'\n' {
                let line = std::mem::take(&mut self.line);
                if !std::mem::take(&mut self.overflow) {
                    self.on_line(&line, &mut out);
                }
            } else if self.line.len() < MAX_LINE_BYTES {
                self.line.push(byte);
            } else {
                // Überlange Zeile: der Rest wird verworfen, damit ein Server den Speicher nicht füllt.
                self.overflow = true;
            }
        }
        out
    }

    /// Schließt den Strom ab. Ein Event ohne abschließende Leerzeile wird noch geliefert.
    pub fn finish(&mut self) -> Option<SseEvent> {
        let mut out = Vec::new();
        let line = std::mem::take(&mut self.line);
        if !line.is_empty() {
            self.on_line(&line, &mut out);
        }
        self.on_line(b"", &mut out);
        out.pop()
    }

    fn on_line(&mut self, raw: &[u8], out: &mut Vec<SseEvent>) {
        let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
        if raw.is_empty() {
            let event = self.event.take();
            if !self.data.is_empty() {
                out.push(SseEvent {
                    event,
                    data: std::mem::take(&mut self.data).join("\n"),
                });
            }
            return;
        }
        let line = String::from_utf8_lossy(raw);
        if line.starts_with(':') {
            return;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line.as_ref(), ""),
        };
        match field {
            "data" => self.data.push(value.to_owned()),
            "event" => self.event = Some(value.to_owned()),
            _ => {}
        }
    }
}

/// Ein Element des normalisierten Chunk-Stroms.
pub(crate) type Item = Result<Chunk, ProviderError>;

/// Übersetzt die SSE-Events eines Anbieters in normalisierte Chunks.
pub(crate) trait SseDecoder: Send + 'static {
    /// Verarbeitet ein Event und hängt die daraus entstehenden Chunks an.
    fn on_event(&mut self, event: &SseEvent, out: &mut VecDeque<Item>);
    /// Der Byte-Strom ist zu Ende.
    fn on_end(&mut self, out: &mut VecDeque<Item>);
    /// Kommt nach diesem Stand nichts mehr?
    fn is_finished(&self) -> bool;
}

struct State<S, D> {
    body: S,
    parser: SseParser,
    decoder: D,
    queue: VecDeque<Item>,
    failed: bool,
}

/// Macht aus einem Strom von Byte-Blöcken den normalisierten Chunk-Strom.
pub(crate) fn sse_stream<S, D>(body: S, decoder: D) -> ProviderStream
where
    S: Stream<Item = Result<Vec<u8>, ProviderError>> + Send + Unpin + 'static,
    D: SseDecoder,
{
    let state = State {
        body,
        parser: SseParser::new(),
        decoder,
        queue: VecDeque::new(),
        failed: false,
    };
    Box::pin(futures::stream::unfold(state, |mut state| async move {
        loop {
            if let Some(item) = state.queue.pop_front() {
                return Some((item, state));
            }
            if state.failed || state.decoder.is_finished() {
                return None;
            }
            match state.body.next().await {
                Some(Ok(bytes)) => {
                    for event in state.parser.feed(&bytes) {
                        state.decoder.on_event(&event, &mut state.queue);
                    }
                }
                Some(Err(error)) => {
                    state.failed = true;
                    state.queue.push_back(Err(error));
                }
                None => {
                    if let Some(event) = state.parser.finish() {
                        state.decoder.on_event(&event, &mut state.queue);
                    }
                    state.decoder.on_end(&mut state.queue);
                    state.failed = true;
                }
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn parse_whole(input: &[u8]) -> Vec<SseEvent> {
        let mut parser = SseParser::new();
        let mut events = parser.feed(input);
        events.extend(parser.finish());
        events
    }

    #[test]
    fn parses_fields_comments_and_crlf() {
        let input = b": ping\r\nevent: delta\r\ndata: {\"a\":1}\r\n\r\ndata:ohne leerzeichen\n\n";
        let events = parse_whole(input);
        assert_eq!(
            events,
            vec![
                SseEvent {
                    event: Some("delta".into()),
                    data: "{\"a\":1}".into()
                },
                SseEvent {
                    event: None,
                    data: "ohne leerzeichen".into()
                },
            ]
        );
    }

    #[test]
    fn joins_multiple_data_lines() {
        let events = parse_whole(b"data: a\ndata: b\n\n");
        assert_eq!(events[0].data, "a\nb");
    }

    #[test]
    fn blank_lines_without_data_emit_nothing() {
        assert!(parse_whole(b"\n\nevent: x\n\nid: 7\nretry: 3\n\n").is_empty());
    }

    #[test]
    fn overlong_lines_are_dropped_without_growing() {
        let mut parser = SseParser::new();
        let big = vec![b'x'; MAX_LINE_BYTES + 10];
        assert!(parser.feed(b"data: ").is_empty());
        assert!(parser.feed(&big).is_empty());
        assert!(parser.line.len() <= MAX_LINE_BYTES);
        let events = parser.feed(b"\n\ndata: danach\n\n");
        assert_eq!(
            events,
            vec![SseEvent {
                event: None,
                data: "danach".into()
            }]
        );
    }

    #[test]
    fn finish_delivers_unterminated_event() {
        let mut parser = SseParser::new();
        assert!(parser.feed(b"data: [DONE]").is_empty());
        assert_eq!(
            parser.finish(),
            Some(SseEvent {
                event: None,
                data: "[DONE]".into()
            })
        );
        assert_eq!(parser.finish(), None);
    }

    #[test]
    fn multibyte_characters_survive_any_split() {
        let input = "data: grüße 🎉\n\n".as_bytes();
        for cut in 0..=input.len() {
            let mut parser = SseParser::new();
            let mut events = parser.feed(&input[..cut]);
            events.extend(parser.feed(&input[cut..]));
            assert_eq!(
                events,
                vec![SseEvent {
                    event: None,
                    data: "grüße 🎉".into()
                }],
                "Schnitt {cut}"
            );
        }
    }

    proptest! {
        #[test]
        fn chunking_never_changes_the_result(
            input in proptest::collection::vec(any::<u8>(), 0..300),
            cuts in proptest::collection::vec(0usize..300, 0..8),
        ) {
            let expected = parse_whole(&input);
            let mut cuts: Vec<usize> = cuts.into_iter().map(|cut| cut.min(input.len())).collect();
            cuts.sort_unstable();
            let mut parser = SseParser::new();
            let mut events = Vec::new();
            let mut start = 0;
            for cut in cuts {
                events.extend(parser.feed(&input[start..cut]));
                start = cut;
            }
            events.extend(parser.feed(&input[start..]));
            events.extend(parser.finish());
            prop_assert_eq!(events, expected);
        }

        #[test]
        fn structured_streams_roundtrip(
            payloads in proptest::collection::vec("[a-zA-Z0-9 {}:\",äö]{1,40}", 0..6),
        ) {
            let mut input = String::new();
            for payload in &payloads {
                input.push_str(&format!("data: {payload}\n\n"));
            }
            let events = parse_whole(input.as_bytes());
            let data: Vec<String> = events.into_iter().map(|event| event.data).collect();
            prop_assert_eq!(data, payloads);
        }
    }
}
