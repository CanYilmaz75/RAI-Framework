//! `agentkit chat` gegen einen lokalen Nachbau der OpenAI-API: kein echter Modellaufruf.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agentkit_cli::{CliError, Options, build_agent, chat_turn};
use agentkit_core::{Decision, FinishReason, SessionId};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn sse(chunks: &[Value]) -> String {
    let mut body = String::new();
    for chunk in chunks {
        body.push_str(&format!("data: {chunk}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    body
}

/// Erste Antwort: Tool-Aufruf `read_file`. Zweite Antwort: Text in zwei Stücken.
fn script() -> Vec<String> {
    let call = json!({"index": 0, "id": "call_1", "type": "function",
        "function": {"name": "read_file", "arguments": "{\"path\":\"notiz.txt\"}"}});
    vec![
        sse(&[
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [call]}}]}),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
        ]),
        sse(&[
            json!({"choices": [{"index": 0, "delta": {"content": "Die Geheimzahl "}}]}),
            json!({"choices": [{"index": 0, "delta": {"content": "ist 4711."}}]}),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
        ]),
    ]
}

/// Startet einen HTTP-Server, der die Antworten der Reihe nach ausliefert und Anfragen mitschreibt.
async fn mock_api(responses: Vec<String>) -> (String, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    tokio::spawn(async move {
        for response in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = Vec::new();
            let body = loop {
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
                        break body.to_owned();
                    }
                }
                assert!(read > 0, "Verbindung endete vor dem Ende der Anfrage");
            };
            seen.lock()
                .unwrap()
                .push(serde_json::from_str(&body).unwrap());
            let head =
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
            socket
                .write_all(format!("{head}{response}").as_bytes())
                .await
                .unwrap();
            socket.shutdown().await.unwrap();
        }
    });
    (base_url, requests)
}

fn workdir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("agentkit-cli-test-{}", SessionId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("notiz.txt"), "Geheimzahl: 4711").unwrap();
    dir
}

fn assert_tool_round(requests: &[Value]) {
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["model"], "test-modell");
    let tools: Vec<&str> = requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(tools, ["read_file", "shell", "write_file"]);
    let messages = requests[1]["messages"].as_array().unwrap();
    let tool = messages
        .iter()
        .find(|message| message["role"] == "tool")
        .unwrap();
    assert_eq!(tool["content"], "Geheimzahl: 4711");
    let assistant = messages
        .iter()
        .find(|message| message["role"] == "assistant")
        .unwrap();
    assert_eq!(assistant["tool_calls"][0]["id"], tool["tool_call_id"]);
}

/// Abnahme Phase 1: `chat` beantwortet eine Frage, für die es `read_file` braucht.
#[tokio::test]
async fn chat_turn_answers_a_question_that_needs_read_file() {
    let (base_url, requests) = mock_api(script()).await;
    let dir = workdir();
    let options = Options {
        model: Some("test-modell".into()),
        workdir: Some(dir.clone()),
        base_url: Some(base_url),
        ..Options::default()
    };
    let agent = build_agent(&options).unwrap();
    let mut out = Vec::new();
    let mut approve = |_: &agentkit_core::ToolCall| Decision::Deny;
    let outcome = chat_turn(
        &agent,
        SessionId::new(),
        "Wie lautet die Geheimzahl?",
        &mut out,
        &mut approve,
    )
    .await
    .unwrap();

    assert_eq!(outcome.reason, FinishReason::Completed);
    assert_eq!(outcome.text, "Die Geheimzahl ist 4711.");
    let printed = String::from_utf8(out).unwrap();
    assert!(
        printed.contains("[read_file {\"path\":\"notiz.txt\"}]"),
        "{printed}"
    );
    assert!(printed.ends_with("Die Geheimzahl ist 4711.\n"), "{printed}");
    assert_tool_round(&requests.lock().unwrap());
    std::fs::remove_dir_all(dir).unwrap();
}

/// Dasselbe über das echte Binary samt Argumenten und Umgebungsvariablen.
#[tokio::test(flavor = "multi_thread")]
async fn binary_streams_the_answer_to_stdout() {
    let (base_url, requests) = mock_api(script()).await;
    let dir = workdir();
    let workdir = dir.clone();
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_agentkit"))
            .args(["chat", "Wie lautet die Geheimzahl?", "--workdir"])
            .arg(&workdir)
            .env("AGENTKIT_BASE_URL", base_url)
            .env("AGENTKIT_MODEL", "test-modell")
            .env_remove("AGENTKIT_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.ends_with("Die Geheimzahl ist 4711.\n"), "{stdout}");
    assert_tool_round(&requests.lock().unwrap());
    std::fs::remove_dir_all(dir).unwrap();
}

/// Eine Antwort mit einem Aufruf von `write_file`, danach eine Textantwort.
fn write_script() -> Vec<String> {
    let call = json!({"index": 0, "id": "call_1", "type": "function", "function": {
        "name": "write_file", "arguments": "{\"path\":\"neu.txt\",\"content\":\"hallo\"}"}});
    vec![
        sse(&[
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [call]}}]}),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
        ]),
        sse(&[
            json!({"choices": [{"index": 0, "delta": {"content": "Erledigt."}}]}),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
        ]),
    ]
}

/// `Ask` löst eine Rückfrage aus; erst die Antwort entscheidet, ob geschrieben wird.
#[tokio::test]
async fn write_file_asks_first_and_obeys_the_answer() {
    for (answer, written) in [(Decision::Allow, true), (Decision::Deny, false)] {
        let (base_url, requests) = mock_api(write_script()).await;
        let dir = workdir();
        let options = Options {
            model: Some("test-modell".into()),
            workdir: Some(dir.clone()),
            base_url: Some(base_url),
            ..Options::default()
        };
        let agent = build_agent(&options).unwrap();
        let mut asked = Vec::new();
        let mut approve = |call: &agentkit_core::ToolCall| {
            asked.push(call.name.clone());
            answer
        };
        let mut out = Vec::new();
        chat_turn(
            &agent,
            SessionId::new(),
            "Schreib neu.txt",
            &mut out,
            &mut approve,
        )
        .await
        .unwrap();

        assert_eq!(asked, ["write_file"]);
        assert_eq!(dir.join("neu.txt").exists(), written);
        let requests = requests.lock().unwrap();
        let messages = requests[1]["messages"].as_array().unwrap();
        let result = messages.iter().find(|m| m["role"] == "tool").unwrap()["content"]
            .as_str()
            .unwrap();
        assert_eq!(result.contains("abgelehnt"), !written, "{result}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn missing_model_is_a_config_error() {
    let error = build_agent(&Options::default()).err().unwrap();
    assert!(matches!(error, CliError::Config(_)));
    assert!(error.to_string().contains("--model"));
}
