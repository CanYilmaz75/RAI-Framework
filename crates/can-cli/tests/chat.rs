//! `can chat` gegen einen lokalen Nachbau der OpenAI-API: kein echter Modellaufruf.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use can_cli::{CliError, Options, Terminal, build_app, chat_turn};
use can_core::{Decision, FinishReason, SessionId};
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
    let dir = std::env::temp_dir().join(format!("can-cli-test-{}", SessionId::new()));
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
    let app = build_app(&options).await.unwrap();
    let mut out = Vec::new();
    let mut approve = |_: &can_core::ToolCall| Decision::Deny;
    let outcome = chat_turn(
        &app,
        SessionId::new(),
        "Wie lautet die Geheimzahl?",
        Terminal {
            out: &mut out,
            approve: &mut approve,
        },
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
        std::process::Command::new(env!("CARGO_BIN_EXE_can"))
            .args(["chat", "Wie lautet die Geheimzahl?", "--workdir"])
            .arg(&workdir)
            .arg("--db")
            .arg(workdir.join("sitzungen.db"))
            .env("CAN_BASE_URL", base_url)
            .env("CAN_MODEL", "test-modell")
            .env_remove("CAN_API_KEY")
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
        let app = build_app(&options).await.unwrap();
        let mut asked = Vec::new();
        let mut approve = |call: &can_core::ToolCall| {
            asked.push(call.name.clone());
            answer
        };
        let mut out = Vec::new();
        chat_turn(
            &app,
            SessionId::new(),
            "Schreib neu.txt",
            Terminal {
                out: &mut out,
                approve: &mut approve,
            },
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

#[tokio::test]
async fn missing_model_is_a_config_error() {
    let error = build_app(&Options::default()).await.err().unwrap();
    assert!(matches!(error, CliError::Config(_)));
    assert!(error.to_string().contains("--model"));
}

/// Abnahme Phase 3 über das Binary: Der erste Prozess bleibt an der Rückfrage stehen und
/// „stürzt ab“; `can resume <id>` setzt den Lauf aus der SQLite-Datei fort.
#[tokio::test(flavor = "multi_thread")]
async fn resume_continues_an_interrupted_run_from_the_database() {
    let (base_url, requests) = mock_api(write_script()).await;
    let dir = workdir();
    let db = dir.join("sitzungen.db");
    let options = Options {
        model: Some("test-modell".into()),
        workdir: Some(dir.clone()),
        base_url: Some(base_url.clone()),
        db: Some(db.clone()),
        ..Options::default()
    };
    let session = SessionId::new();
    let app = build_app(&options).await.unwrap();
    let mut run = app.agent.run(session, "Schreib neu.txt").await.unwrap();
    loop {
        if matches!(
            run.next().await.unwrap().unwrap().event,
            can_core::Event::ApprovalNeeded { .. }
        ) {
            break;
        }
    }
    // `forget` statt `drop`: ein verworfenes Handle würde den Lauf sauber abbrechen.
    std::mem::forget(run);
    assert!(!dir.join("neu.txt").exists());

    let workdir = dir.clone();
    let output = tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_can"))
            .args([
                "resume",
                &session.to_string(),
                "--model",
                "test-modell",
                "--base-url",
                &base_url,
            ])
            .arg("--workdir")
            .arg(&workdir)
            .arg("--db")
            .arg(&db)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"j\n").unwrap();
        child.wait_with_output().unwrap()
    })
    .await
    .unwrap();

    let (stdout, stderr) = (
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("Tool write_file mit"), "{stderr}");
    assert!(stdout.ends_with("Erledigt.\n"), "{stdout}");
    assert_eq!(
        std::fs::read_to_string(dir.join("neu.txt")).unwrap(),
        "hallo"
    );
    assert_eq!(
        requests.lock().unwrap().len(),
        2,
        "die erste Modellantwort wurde nicht erneut angefragt"
    );
    std::fs::remove_dir_all(dir).unwrap();
}
