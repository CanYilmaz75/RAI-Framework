//! Fähigkeiten für die Arbeit an Code: geänderte Freigaben, Gedächtnis, Skills und Verzweigen.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use can_cli::{Answer, Options, Terminal, build_app, chat_turn};
use can_core::SessionId;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn sse(chunks: &[Value]) -> String {
    let mut body: String = chunks
        .iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .collect();
    body.push_str("data: [DONE]\n\n");
    body
}

fn tool_call(name: &str, arguments: Value) -> String {
    let call = json!({"index": 0, "id": "call_1", "type": "function",
        "function": {"name": name, "arguments": arguments.to_string()}});
    sse(&[
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [call]}}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
    ])
}

fn text(answer: &str) -> String {
    sse(&[
        json!({"choices": [{"index": 0, "delta": {"content": answer}}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
    ])
}

/// Lokaler Nachbau der Modell-API: liefert die Antworten der Reihe nach, merkt sich die Anfragen.
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
                        break body.to_owned();
                    }
                }
                assert!(read > 0);
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
    let dir = std::env::temp_dir().join(format!("can-coding-test-{}", SessionId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn options(dir: &Path, base_url: String) -> Options {
    Options {
        model: Some("test-modell".into()),
        workdir: Some(dir.to_path_buf()),
        base_url: Some(base_url),
        ..Options::default()
    }
}

fn tool_message(request: &Value) -> String {
    let messages = request["messages"].as_array().unwrap();
    messages.iter().rev().find(|m| m["role"] == "tool").unwrap()["content"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn approval_can_edit_the_arguments_before_the_tool_runs() {
    let script = || {
        vec![
            tool_call(
                "write_file",
                json!({"path": "neu.txt", "content": "vom modell"}),
            ),
            text("Erledigt."),
        ]
    };
    // Gültige Änderung: die Datei entsteht unter dem neuen Namen mit dem neuen Inhalt.
    let (base_url, requests) = mock_api(script()).await;
    let dir = workdir();
    let app = build_app(&options(&dir, base_url)).await.unwrap();
    let mut approve = |_: &can_core::ToolCall| {
        Answer::Edit(json!({"path": "anders.txt", "content": "vom menschen"}))
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
    assert!(!dir.join("neu.txt").exists());
    assert_eq!(
        std::fs::read_to_string(dir.join("anders.txt")).unwrap(),
        "vom menschen"
    );
    let printed = String::from_utf8(out).unwrap();
    assert!(
        printed.contains("[write_file {\"content\":\"vom menschen\",\"path\":\"anders.txt\"}]"),
        "{printed}"
    );
    let result = tool_message(&requests.lock().unwrap()[1]);
    assert!(
        result.starts_with("[Ein Mensch hat die Argumente vor der Ausführung geändert in:"),
        "{result}"
    );

    // Ungültige Änderung: nichts wird geschrieben, das Modell bekommt eine Ablehnung.
    let (base_url, requests) = mock_api(script()).await;
    let app = build_app(&options(&dir, base_url)).await.unwrap();
    let mut approve = |_: &can_core::ToolCall| Answer::Edit(json!({"pfad": "x"}));
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
    assert!(!dir.join("neu.txt").exists() && !dir.join("x").exists());
    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("passen nicht zum Tool")
    );
    assert!(tool_message(&requests.lock().unwrap()[1]).contains("abgelehnt"));
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn memory_is_always_loaded_and_skills_only_on_demand() {
    let dir = workdir();
    std::fs::write(dir.join("AGENTS.md"), "Antworte immer auf Deutsch.\n").unwrap();
    std::fs::create_dir_all(dir.join(".can/skills/release")).unwrap();
    let skill = "---\nname: release\ndescription: Wie eine Version veröffentlicht wird\n---\nErst Tests, dann Tag setzen.\n";
    std::fs::write(dir.join(".can/skills/release/SKILL.md"), skill).unwrap();
    std::fs::write(dir.join("main.rs"), "fn main() {}\n").unwrap();

    let plan = json!({"todos": [{"content": "Skill laden", "status": "in_progress"}]});
    let script = vec![
        tool_call("write_todos", plan),
        tool_call("load_skill", json!({"name": "release"})),
        tool_call("glob", json!({"pattern": "*.rs"})),
        text("Fertig."),
    ];
    let (base_url, requests) = mock_api(script).await;
    let app = build_app(&options(&dir, base_url)).await.unwrap();
    let mut approve =
        |call: &can_core::ToolCall| panic!("lesende Tools fragen nicht nach: {}", call.name);
    let mut out = Vec::new();
    chat_turn(
        &app,
        SessionId::new(),
        "Veröffentliche",
        Terminal {
            out: &mut out,
            approve: &mut approve,
        },
    )
    .await
    .unwrap();

    let requests = requests.lock().unwrap();
    let system = requests[0]["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains("# Gedächtnis") && system.contains("Antworte immer auf Deutsch."));
    assert!(system.contains("- release: Wie eine Version veröffentlicht wird"));
    assert!(
        !system.contains("Tag setzen"),
        "der volle Skill steht nicht im System-Prompt"
    );
    let tools: Vec<&str> = requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect();
    assert!(
        tools.contains(&"load_skill") && tools.contains(&"grep") && tools.contains(&"edit_file")
    );
    assert_eq!(tool_message(&requests[2]), "Erst Tests, dann Tag setzen.");
    assert_eq!(tool_message(&requests[3]), "main.rs");
    let printed = String::from_utf8(out).unwrap();
    assert!(printed.contains("[Plan]\n  [~] Skill laden"), "{printed}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_forked_session_continues_independently() {
    let (base_url, requests) = mock_api(vec![text("eins"), text("zwei"), text("zwei-b")]).await;
    let dir = workdir();
    let app = build_app(&options(&dir, base_url)).await.unwrap();
    let source = SessionId::new();
    for prompt in ["erste Frage", "zweite Frage"] {
        let mut approve = |_: &can_core::ToolCall| Answer::Deny;
        chat_turn(
            &app,
            source,
            prompt,
            Terminal {
                out: &mut Vec::new(),
                approve: &mut approve,
            },
        )
        .await
        .unwrap();
    }
    let fork = app.agent.fork(source, 3).await.unwrap();
    let mut approve = |_: &can_core::ToolCall| Answer::Deny;
    let outcome = chat_turn(
        &app,
        fork,
        "andere Frage",
        Terminal {
            out: &mut Vec::new(),
            approve: &mut approve,
        },
    )
    .await
    .unwrap();
    assert_eq!(outcome.text, "zwei-b");
    let last = requests.lock().unwrap().pop().unwrap();
    let texts: Vec<&str> = last["messages"]
        .as_array()
        .unwrap()
        .iter()
        .skip(1)
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert_eq!(texts, ["erste Frage", "eins", "andere Frage"]);
    assert_eq!(app.store.load(source).await.unwrap().len(), 8);
    std::fs::remove_dir_all(dir).unwrap();
}
