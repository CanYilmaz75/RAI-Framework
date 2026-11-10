//! Gemeinsame Testsuite: jeder Provider liefert für dieselbe Antwort dieselben normalisierten
//! Chunks und ordnet HTTP-Fehler gleich zu. Ein lokaler Server ersetzt die echte API.
#![cfg(all(feature = "openai", feature = "anthropic"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use can_core::{
    Assembler, Message, Provider, ProviderError, Request, Schema, Secret, StopReason, ToolDecl,
    Usage,
};
use can_providers::{Anthropic, OpenAiCompat};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const OPENAI_STREAM: &str = concat!(
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Ich lese \"}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"die Datei.\"}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",",
    "\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,",
    "\"function\":{\"arguments\":\"\\\"README.md\\\"}\"}}]}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":42,\"completion_tokens\":7,",
    "\"prompt_tokens_details\":{\"cached_tokens\":30}}}\n\n",
    "data: [DONE]\n\n",
);

const ANTHROPIC_STREAM: &str = concat!(
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":",
    "{\"input_tokens\":12,\"cache_read_input_tokens\":30,\"output_tokens\":1}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Ich lese \"}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"die Datei.\"}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",",
    "\"name\":\"read_file\",\"input\":{}}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",",
    "\"partial_json\":\"{\\\"path\\\":\\\"README.md\\\"}\"}}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":7}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

struct Seen {
    path: String,
    headers: String,
    body: Value,
}

/// Server, der die Antworten der Reihe nach ausliefert und die Anfragen mitschreibt.
async fn mock(responses: Vec<(u16, String)>) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        for (status, response) in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = Vec::new();
            let (head, body) = loop {
                let mut chunk = [0u8; 4096];
                let read = socket.read(&mut chunk).await.unwrap();
                buffer.extend_from_slice(&chunk[..read]);
                let text = String::from_utf8_lossy(&buffer).into_owned();
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let length: usize = head
                        .lines()
                        .find_map(|line| {
                            line.to_lowercase()
                                .strip_prefix("content-length:")
                                .map(str::to_owned)
                        })
                        .map(|value| value.trim().parse().unwrap())
                        .unwrap_or(0);
                    if body.len() >= length {
                        break (head.to_lowercase(), body.to_owned());
                    }
                }
                assert!(read > 0);
            };
            let path = head.split_whitespace().nth(1).unwrap().to_owned();
            log.lock().unwrap().push(Seen {
                path,
                headers: head,
                body: serde_json::from_str(&body).unwrap(),
            });
            let head = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: text/event-stream\r\nretry-after: 3\r\nconnection: close\r\n\r\n"
            );
            socket
                .write_all(format!("{head}{response}").as_bytes())
                .await
                .unwrap();
            socket.shutdown().await.unwrap();
        }
    });
    (base_url, seen)
}

fn request() -> Request {
    Request {
        model: "test-modell".into(),
        system: Some("Du hilfst.".into()),
        messages: vec![Message::user("Lies README.md")],
        tools: vec![ToolDecl {
            name: "read_file".into(),
            description: "Liest eine Datei.".into(),
            input_schema: Schema(json!({"type": "object"})),
        }],
        max_output: Some(256),
        extra: Default::default(),
    }
}

async fn answer(provider: &dyn Provider) -> Result<(Message, Usage, StopReason), ProviderError> {
    let mut stream = provider.stream(request()).await?;
    let mut assembler = Assembler::default();
    while let Some(chunk) = stream.next().await {
        assembler.push(chunk?)?;
    }
    assembler.finish()
}

/// Was jeder Provider leisten muss, unabhängig vom Format seiner API.
async fn conformance(
    build: impl Fn(String) -> Box<dyn Provider>,
    fixture: &str,
    path: &str,
    auth: &str,
) {
    let error = |message: &str| format!("{{\"error\":{{\"message\":\"{message}\"}}}}");
    let (base_url, seen) = mock(vec![
        (200, fixture.to_owned()),
        (401, error("falscher Schlüssel")),
        (429, error("langsam")),
        (500, error("kaputt")),
        (400, error("prompt is too long for the context window")),
        (404, error("gibt es nicht")),
    ])
    .await;
    let provider = build(base_url);

    let (message, usage, stop) = answer(provider.as_ref()).await.unwrap();
    assert_eq!(message.text(), "Ich lese die Datei.");
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

    assert!(matches!(
        answer(provider.as_ref()).await,
        Err(ProviderError::Auth)
    ));
    assert!(matches!(
        answer(provider.as_ref()).await,
        Err(ProviderError::RateLimited { retry_after: Some(wait) }) if wait.as_secs() == 3
    ));
    assert!(matches!(
        answer(provider.as_ref()).await,
        Err(ProviderError::Overloaded)
    ));
    assert!(matches!(
        answer(provider.as_ref()).await,
        Err(ProviderError::ContextTooLong { .. })
    ));
    assert!(matches!(
        answer(provider.as_ref()).await,
        Err(ProviderError::InvalidRequest(_))
    ));

    let seen = seen.lock().unwrap();
    assert_eq!(seen[0].path, path);
    assert!(seen[0].headers.contains(auth), "{}", seen[0].headers);
    assert_eq!(seen[0].body["model"], "test-modell");
    assert_eq!(seen[0].body["stream"], true);
    assert_eq!(seen[0].body["max_tokens"], 256);
    assert!(seen[0].body.to_string().contains("read_file"));
    assert!(provider.capabilities("test-modell").tools);
}

#[tokio::test]
async fn openai_compatible_provider_conforms() {
    let build = |base: String| -> Box<dyn Provider> {
        Box::new(OpenAiCompat::new(
            format!("{base}/v1"),
            Some(Secret::new("geheim".to_owned())),
        ))
    };
    conformance(
        build,
        OPENAI_STREAM,
        "/v1/chat/completions",
        "authorization: bearer geheim",
    )
    .await;
}

#[tokio::test]
async fn anthropic_provider_conforms() {
    let build = |base: String| -> Box<dyn Provider> {
        Box::new(Anthropic::new(base, Some(Secret::new("geheim".to_owned()))))
    };
    conformance(build, ANTHROPIC_STREAM, "/v1/messages", "x-api-key: geheim").await;
}
