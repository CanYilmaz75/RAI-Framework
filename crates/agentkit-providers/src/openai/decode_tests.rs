use super::*;
use agentkit_core::{Assembler, Message};
use serde_json::json;

/// Aufgezeichnete Antwort im OpenAI-Format: Text, ein Tool-Aufruf in Fragmenten, Usage, DONE.
const TOOL_STREAM: &str = concat!(
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Ich lese \"}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"die Datei.\"}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_abc\",",
    "\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"\"}}]}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,",
    "\"function\":{\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,",
    "\"function\":{\"arguments\":\"\\\"README.md\\\"}\"}}]}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":42,\"completion_tokens\":7,",
    "\"prompt_tokens_details\":{\"cached_tokens\":30}}}\n\n",
    "data: [DONE]\n\n",
);

async fn collect(pieces: Vec<Vec<u8>>) -> Result<(Message, Usage, StopReason), ProviderError> {
    let body = futures::stream::iter(pieces.into_iter().map(Ok));
    let mut stream = decode_body(body);
    let mut assembler = Assembler::default();
    while let Some(chunk) = stream.next().await {
        assembler.push(chunk?)?;
    }
    assembler.finish()
}

fn split(input: &str, size: usize) -> Vec<Vec<u8>> {
    input.as_bytes().chunks(size).map(<[u8]>::to_vec).collect()
}

#[tokio::test]
async fn decodes_text_tool_call_and_usage() {
    let (message, usage, stop) = collect(split(TOOL_STREAM, usize::MAX)).await.unwrap();
    assert_eq!(message.text(), "Ich lese die Datei.");
    let calls = message.tool_uses();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "read_file");
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

#[tokio::test]
async fn result_does_not_depend_on_chunk_size() {
    for size in [1, 2, 3, 7, 16, 64, 1000] {
        let (message, usage, stop) = collect(split(TOOL_STREAM, size)).await.unwrap();
        assert_eq!(message.text(), "Ich lese die Datei.", "Blockgröße {size}");
        assert_eq!(message.tool_uses()[0].input, json!({"path": "README.md"}));
        assert_eq!((usage.input_tokens, stop), (42, StopReason::ToolUse));
    }
}

#[tokio::test]
async fn stream_without_done_marker_still_finishes() {
    let input = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    );
    let (message, usage, stop) = collect(split(input, 5)).await.unwrap();
    assert_eq!((message.text().as_str(), stop), ("hi", StopReason::EndTurn));
    assert_eq!(usage, Usage::default());
}

#[tokio::test]
async fn truncated_stream_is_a_protocol_error() {
    let input = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n";
    assert!(matches!(
        collect(split(input, 9)).await,
        Err(ProviderError::Protocol(_))
    ));
}

#[tokio::test]
async fn garbage_and_error_payloads_fail_cleanly() {
    let garbage = collect(split("data: {nicht json\n\n", 100)).await;
    assert!(matches!(garbage, Err(ProviderError::Protocol(_))));
    let error = collect(split("data: {\"error\":{\"message\":\"boom\"}}\n\n", 100)).await;
    assert!(matches!(error, Err(ProviderError::Protocol(m)) if m.contains("boom")));
}

#[tokio::test]
async fn transport_errors_pass_through_and_end_the_stream() {
    let body = futures::stream::iter(vec![
        Ok(b"data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\n".to_vec()),
        Err(ProviderError::Overloaded),
        Ok(b"data: [DONE]\n\n".to_vec()),
    ]);
    let items: Vec<Item> = decode_body(body).collect().await;
    assert_eq!(items.len(), 2);
    assert!(matches!(items[1], Err(ProviderError::Overloaded)));
}

#[test]
fn parallel_calls_without_index_use_their_position() {
    let mut decoder = Decoder::default();
    let mut out = VecDeque::new();
    let data = json!({"choices": [{"delta": {"tool_calls": [
        {"function": {"name": "a", "arguments": "{}"}},
        {"function": {"name": "b", "arguments": "{}"}},
    ]}, "finish_reason": "length"}]});
    decoder.on_data(&data.to_string(), &mut out);
    decoder.on_end(&mut out);
    assert!(decoder.is_finished());
    let starts: Vec<(u32, String)> = out
        .iter()
        .filter_map(|item| match item {
            Ok(Chunk::ToolCallStart { index, name, .. }) => Some((*index, name.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(starts, vec![(0, "a".to_owned()), (1, "b".to_owned())]);
    assert!(matches!(
        out.back(),
        Some(Ok(Chunk::Done {
            stop: StopReason::MaxTokens,
            ..
        }))
    ));
    decoder.on_data("[DONE]", &mut out);
    assert_eq!(out.len(), 5, "nach dem Ende kommt nichts mehr");
}
