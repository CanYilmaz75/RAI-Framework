//! Konfigurationsdatei von Ende zu Ende: Anthropic-Provider, MCP-Server, Regeln und ein Bild,
//! alles gegen einen lokalen Server.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use can_core::SessionId;
use can_tools::mcp::echo;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect()
}

/// Modellantworten im Anthropic-Format: erst ein Aufruf des MCP-Tools, dann Text.
fn model_script() -> VecDeque<String> {
    VecDeque::from([
        sse(&[
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 5}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "demo__echo", "input": {}}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"text\":\"hallo\"}"}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 3}}),
            json!({"type": "message_stop"}),
        ]),
        sse(&[
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 9}}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Der Server sagt: echo: hallo"}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 6}}),
            json!({"type": "message_stop"}),
        ]),
    ])
}

/// Ein Server für beides: `/v1/messages` spielt das Modell, `/mcp` ist der MCP-Server.
async fn server() -> (String, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let model_requests = Arc::new(Mutex::new(Vec::new()));
    let log = model_requests.clone();
    let mut script = model_script();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = Vec::new();
            let (head, body) = loop {
                let mut chunk = [0u8; 8192];
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
                        break (head.to_owned(), body.to_owned());
                    }
                }
                assert!(read > 0);
            };
            let message: Value = serde_json::from_str(&body).unwrap();
            let (kind, payload) = if head.starts_with("POST /mcp") {
                (
                    "application/json",
                    echo::handle(&message)
                        .map(|reply| reply.to_string())
                        .unwrap_or_default(),
                )
            } else {
                assert!(head.starts_with("POST /v1/messages"), "{head}");
                log.lock().unwrap().push(message);
                (
                    "text/event-stream",
                    script
                        .pop_front()
                        .expect("mehr Modellanfragen als erwartet"),
                )
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
                payload.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        }
    });
    (base, model_requests)
}

fn workdir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("can-config-test-{}", SessionId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Abnahme Phase 4: Ein MCP-Server lässt sich per Konfigurationsdatei einbinden.
#[tokio::test(flavor = "multi_thread")]
async fn config_file_wires_anthropic_mcp_rules_and_images() {
    let (base, model_requests) = server().await;
    let dir = workdir();
    let config = format!(
        r#"
model = "test-modell"
system = "Du testest."

[provider]
kind = "anthropic"
base_url = "{base}"
api_key_env = "CAN_TEST_KEY"

[limits]
max_steps = 5

[[policy.rules]]
tool = "demo__echo"
decision = "allow"

[http]
allowed_hosts = ["example.com"]

[mcp.servers.demo]
url = "{base}/mcp"
"#
    );
    std::fs::write(dir.join("can.toml"), config).unwrap();
    std::fs::write(dir.join("bild.png"), b"foo").unwrap();

    let workdir = dir.clone();
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_can"))
            .args([
                "chat",
                "Was zeigt das Bild? Frag den Server.",
                "--image",
                "bild.png",
            ])
            .args([
                "--config",
                "can.toml",
                "--db",
                "sitzungen.db",
                "--workdir",
                ".",
            ])
            .current_dir(&workdir)
            .env("CAN_TEST_KEY", "geheim")
            .env_remove("CAN_MODEL")
            .env_remove("CAN_BASE_URL")
            .env_remove("CAN_PROVIDER")
            .output()
            .unwrap()
    })
    .await
    .unwrap();

    let (stdout, stderr) = (
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(output.status.success(), "{stderr}");
    assert!(
        stdout.contains("[demo__echo {\"text\":\"hallo\"}]"),
        "{stdout}"
    );
    assert!(
        stdout.ends_with("Der Server sagt: echo: hallo\n"),
        "{stdout}"
    );
    assert!(
        !stderr.contains("ausführen?"),
        "die Regel aus der Datei erlaubt den Aufruf ohne Rückfrage"
    );

    let requests = model_requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        (
            requests[0]["model"].as_str(),
            requests[0]["system"].as_str()
        ),
        (Some("test-modell"), Some("Du testest."))
    );
    let tools: Vec<&str> = requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        tools,
        [
            "demo__echo",
            "demo__fail",
            "demo__picture",
            "http_fetch",
            "read_file",
            "shell",
            "write_file"
        ]
    );
    let first = &requests[0]["messages"][0]["content"];
    assert_eq!(first[0]["text"], "Was zeigt das Bild? Frag den Server.");
    assert_eq!(
        first[1]["source"],
        json!({"type": "base64", "media_type": "image/png", "data": "Zm9v"})
    );
    let result = &requests[1]["messages"][2]["content"][0];
    assert_eq!(result["type"], "tool_result");
    assert_eq!(result["content"][0]["text"], "echo: hallo");
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn broken_config_and_unknown_image_type_fail_with_a_clear_message() {
    let dir = workdir();
    std::fs::write(
        dir.join("can.toml"),
        "[provider]\napi_key = \"sk-geheim\"\n",
    )
    .unwrap();
    std::fs::write(dir.join("notiz.txt"), "x").unwrap();
    let run = |args: Vec<&'static str>, dir: PathBuf| {
        tokio::task::spawn_blocking(move || {
            std::process::Command::new(env!("CARGO_BIN_EXE_can"))
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap()
        })
    };
    let secret = run(vec!["chat", "hi", "--model", "m"], dir.clone())
        .await
        .unwrap();
    assert!(!secret.status.success());
    assert!(String::from_utf8_lossy(&secret.stderr).contains("Konfiguration ungültig"));

    std::fs::remove_file(dir.join("can.toml")).unwrap();
    let image = run(
        vec![
            "chat",
            "hi",
            "--model",
            "m",
            "--image",
            "notiz.txt",
            "--base-url",
            "http://127.0.0.1:1",
        ],
        dir.clone(),
    )
    .await
    .unwrap();
    assert!(String::from_utf8_lossy(&image.stderr).contains("Unbekanntes Bildformat"));
    std::fs::remove_dir_all(dir).unwrap();
}
