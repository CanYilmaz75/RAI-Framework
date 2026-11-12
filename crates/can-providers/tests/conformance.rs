//! Gemeinsame Testsuite: jeder Provider liefert für dieselbe Antwort dieselben normalisierten
//! Chunks und ordnet HTTP-Fehler gleich zu. Ein lokaler Server ersetzt die echte API.
#![cfg(feature = "all")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use can_core::{
    Assembler, Message, Provider, ProviderError, Request, Schema, Secret, StopReason, ToolDecl,
    Usage,
};
use can_providers::auth::TokenSource;
use can_providers::{Anthropic, Bedrock, Gemini, OpenAiCompat, VertexAnthropic};
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

/// Anfrage im OpenAI-Format.
fn openai_body(body: &Value) {
    assert_eq!(
        (
            body["model"].as_str(),
            body["stream"].as_bool(),
            body["max_tokens"].as_u64()
        ),
        (Some("test-modell"), Some(true), Some(256))
    );
}

/// Anfrage im Anthropic-Format.
fn anthropic_body(body: &Value) {
    openai_body(body);
}

/// Anfrage im Gemini-Format: das Modell steht in der Adresse.
fn gemini_body(body: &Value) {
    assert_eq!(body["generationConfig"]["maxOutputTokens"], 256);
    assert_eq!(body["systemInstruction"]["parts"][0]["text"], "Du hilfst.");
    assert_eq!(body["contents"][0]["role"], "user");
}

/// Was jeder Provider leisten muss, unabhängig vom Format seiner API.
async fn conformance(
    build: impl Fn(String) -> Box<dyn Provider>,
    fixture: &str,
    path: &str,
    auth: &str,
    check_body: impl Fn(&Value),
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
    assert_eq!(seen[0].path, path.to_lowercase());
    assert!(seen[0].headers.contains(auth), "{}", seen[0].headers);
    check_body(&seen[0].body);
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
        openai_body,
    )
    .await;
}

#[tokio::test]
async fn anthropic_provider_conforms() {
    let build = |base: String| -> Box<dyn Provider> {
        Box::new(Anthropic::new(base, Some(Secret::new("geheim".to_owned()))))
    };
    conformance(
        build,
        ANTHROPIC_STREAM,
        "/v1/messages",
        "x-api-key: geheim",
        anthropic_body,
    )
    .await;
}

#[tokio::test]
async fn azure_openai_conforms_with_its_own_address_and_header() {
    let build = |base: String| -> Box<dyn Provider> {
        Box::new(OpenAiCompat::azure(
            &base,
            "mein-deployment",
            "2024-10-21",
            Secret::new("geheim".to_owned()),
        ))
    };
    let path = "/openai/deployments/mein-deployment/chat/completions?api-version=2024-10-21";
    conformance(build, OPENAI_STREAM, path, "api-key: geheim", openai_body).await;
}

const GEMINI_STREAM: &str = concat!(
    "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"Ich lese \"}]}}]}\r\n\r\n",
    "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"die Datei.\"},",
    "{\"functionCall\":{\"name\":\"read_file\",\"args\":{\"path\":\"README.md\"}}}]},\"finishReason\":\"STOP\"}],",
    "\"usageMetadata\":{\"promptTokenCount\":42,\"candidatesTokenCount\":7,\"cachedContentTokenCount\":30}}\r\n\r\n",
);

const BEDROCK_RESPONSE: &str = concat!(
    "{\"output\":{\"message\":{\"role\":\"assistant\",\"content\":[{\"text\":\"Ich lese die Datei.\"},",
    "{\"toolUse\":{\"toolUseId\":\"tooluse_1\",\"name\":\"read_file\",\"input\":{\"path\":\"README.md\"}}}]}},",
    "\"stopReason\":\"tool_use\",\"usage\":{\"inputTokens\":12,\"outputTokens\":7,\"cacheReadInputTokens\":30}}",
);

#[tokio::test]
async fn gemini_api_conforms() {
    let build = |base: String| -> Box<dyn Provider> {
        Box::new(Gemini::new(
            Some(format!("{base}/v1beta")),
            Some(Secret::new("geheim".to_owned())),
        ))
    };
    let path = "/v1beta/models/test-modell:streamGenerateContent?alt=sse";
    conformance(
        build,
        GEMINI_STREAM,
        path,
        "x-goog-api-key: geheim",
        gemini_body,
    )
    .await;
}

#[tokio::test]
async fn gemini_on_vertex_conforms() {
    let build = |base: String| -> Box<dyn Provider> {
        let token = TokenSource::Static(Secret::new("geheim".to_owned()));
        Box::new(
            Gemini::vertex("projekt", "europe-west4", token).with_base_url(format!("{base}/v1")),
        )
    };
    let path = "/v1/projects/projekt/locations/europe-west4/publishers/google/models/test-modell:streamGenerateContent?alt=sse";
    conformance(
        build,
        GEMINI_STREAM,
        path,
        "authorization: bearer geheim",
        gemini_body,
    )
    .await;
}

#[tokio::test]
async fn claude_on_vertex_conforms() {
    let build = |base: String| -> Box<dyn Provider> {
        let token = TokenSource::Static(Secret::new("geheim".to_owned()));
        Box::new(
            VertexAnthropic::new("projekt", "us-east5", token).with_base_url(format!("{base}/v1")),
        )
    };
    let path = "/v1/projects/projekt/locations/us-east5/publishers/anthropic/models/test-modell:streamRawPredict";
    let body = |body: &Value| {
        assert!(
            body.get("model").is_none(),
            "das Modell steht in der Adresse"
        );
        assert_eq!(body["anthropic_version"], "vertex-2023-10-16");
        assert_eq!(body["max_tokens"], 256);
    };
    conformance(
        build,
        ANTHROPIC_STREAM,
        path,
        "authorization: bearer geheim",
        body,
    )
    .await;
}

#[tokio::test]
async fn bedrock_conforms() {
    let build = |base: String| -> Box<dyn Provider> {
        Box::new(Bedrock::new("eu-central-1", Secret::new("geheim".to_owned())).with_base_url(base))
    };
    let body = |body: &Value| {
        assert_eq!(body["inferenceConfig"]["maxTokens"], 256);
        assert_eq!(body["system"][0]["text"], "Du hilfst.");
    };
    conformance(
        build,
        BEDROCK_RESPONSE,
        "/model/test-modell/converse",
        "authorization: bearer geheim",
        body,
    )
    .await;
}
