//! MCP-Client gegen den Beispielserver, über stdio und über HTTP.
#![cfg(feature = "mcp")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use can_core::{
    CallId, Effect, ManualClock, OutputBlock, ProgressSink, SecretsHandle, SessionId, ToolCtx,
    ToolError, schema::validate,
};
use can_tools::mcp::{McpClient, McpError, echo};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

fn ctx() -> ToolCtx {
    ToolCtx {
        session: SessionId::new(),
        call_id: CallId::new(),
        workdir: std::env::temp_dir(),
        cancel: CancellationToken::new(),
        clock: Arc::new(ManualClock::default()),
        progress: ProgressSink::noop(),
        secrets: SecretsHandle::empty(),
    }
}

/// Was über jeden Transport gleich funktionieren muss.
async fn exercise(client: Arc<McpClient>) {
    let tools = client.tools().await.unwrap();
    let names: Vec<&str> = tools.iter().map(|tool| tool.spec().name.as_str()).collect();
    assert_eq!(
        names,
        ["demo__echo", "demo__fail", "demo__picture"],
        "Präfix und beide Seiten der Liste"
    );
    assert!(
        tools
            .iter()
            .all(|tool| tool.spec().effect == Effect::External && !tool.spec().idempotent)
    );
    assert_eq!(tools[0].spec().description, "Gibt den Text zurück.");
    assert!(
        validate(&tools[0].spec().input_schema.0, &json!({})).is_err(),
        "das Schema des Servers gilt"
    );

    let echoed = tools[0]
        .call(json!({"text": "grüße"}), &ctx())
        .await
        .unwrap();
    assert_eq!(echoed.as_text(), "echo: grüße");
    let failed = tools[1].call(json!({}), &ctx()).await;
    assert!(matches!(failed, Err(ToolError::ForModel(m)) if m == "absichtlich gescheitert"));
    let picture = tools[2].call(json!({}), &ctx()).await.unwrap();
    assert!(
        matches!(&picture.0[0], OutputBlock::Image { media_type, data } if media_type == "image/png" && data.0 == b"foo")
    );

    let unknown = client
        .request("tools/call", json!({"name": "gibt_es_nicht"}))
        .await;
    assert!(matches!(
        unknown,
        Err(McpError::Server { code: -32602, .. })
    ));
    assert_eq!(client.server(), "demo");
}

#[tokio::test]
async fn stdio_server_lists_and_calls_tools() {
    let client = McpClient::connect_stdio("demo", env!("CARGO_BIN_EXE_can-mcp-echo"), &[])
        .await
        .unwrap();
    exercise(client).await;
}

#[tokio::test]
async fn missing_program_and_dead_server_are_errors() {
    let missing = McpClient::connect_stdio("x", "gibt-es-nicht-4711", &[]).await;
    assert!(matches!(missing, Err(McpError::Io(m)) if m.contains("nicht startbar")));
    // `true` beendet sich sofort, ohne zu antworten.
    let dead = McpClient::connect_stdio("x", "true", &[]).await;
    assert!(matches!(dead, Err(McpError::Io(_))));
}

/// HTTP-Server um denselben Beispielserver; antwortet abwechselnd als JSON und als SSE.
async fn http_server() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let heads = Arc::new(Mutex::new(Vec::new()));
    let log = heads.clone();
    tokio::spawn(async move {
        let mut count = 0;
        loop {
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
            log.lock().unwrap().push(head);
            let message: Value = serde_json::from_str(&body).unwrap();
            let response = match echo::handle(&message) {
                None => "HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    .to_owned(),
                Some(reply) => {
                    count += 1;
                    let (kind, payload) = if count % 2 == 0 {
                        (
                            "text/event-stream",
                            format!("event: message\ndata: {reply}\n\n"),
                        )
                    } else {
                        ("application/json", reply.to_string())
                    };
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: {kind}\r\nmcp-session-id: sitzung-1\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
                        payload.len()
                    )
                }
            };
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        }
    });
    (url, heads)
}

#[tokio::test]
async fn http_server_lists_and_calls_tools() {
    let (url, heads) = http_server().await;
    let client = McpClient::connect_http("demo", url).await.unwrap();
    exercise(client).await;
    let heads = heads.lock().unwrap();
    assert!(
        !heads[0].contains("mcp-session-id"),
        "die erste Anfrage hat noch keine Sitzung"
    );
    assert!(
        heads[1..]
            .iter()
            .all(|head| head.contains("mcp-session-id: sitzung-1"))
    );
    assert!(
        heads
            .iter()
            .all(|head| head.contains("mcp-protocol-version:"))
    );
}

#[tokio::test]
async fn unreachable_http_server_is_an_error() {
    let result = McpClient::connect_http("x", "http://127.0.0.1:1/mcp").await;
    assert!(matches!(result, Err(McpError::Io(_))));
}
