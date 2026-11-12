//! Freigaben, Kontingente, Idempotenz, Abbruch und Übernahme nach einem Absturz.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use can_core::MemoryStore;
use can_providers::FakeProvider;
use can_server::{MemoryDirectory, SessionDirectory};
use common::{serve, serve_with};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};

#[tokio::test]
async fn approvals_quota_and_metrics() {
    let provider = FakeProvider::new()
        .tool_call("save_note", json!({"title": "Einkauf"}))
        .text("Gespeichert.");
    let server = serve(provider).await;
    let session = server.create("ada-key", "Speichere eine Notiz").await;
    let approval = server.wait_approval("ada-key").await;
    assert_eq!(
        (approval["tool"].as_str(), &approval["input"]),
        (Some("save_note"), &json!({"title": "Einkauf"}))
    );
    assert_eq!(
        server.get("bob-key", "/v1/approvals").await.1,
        json!({"approvals": []}),
        "fremder Mandant sieht nichts"
    );

    // Kontingent: der Mandant darf nur einen Lauf gleichzeitig haben.
    let second = server
        .post(
            "ada-key",
            "/v1/sessions",
            json!({"agent": "helfer", "input": "noch einer"}),
        )
        .await;
    assert_eq!(second.0, StatusCode::TOO_MANY_REQUESTS);
    let busy = server
        .post(
            "ada-key",
            &format!("/v1/sessions/{session}/messages"),
            json!({"input": "x"}),
        )
        .await;
    assert_eq!(busy.0, StatusCode::TOO_MANY_REQUESTS);

    let path = format!(
        "/v1/sessions/{session}/approvals/{}",
        approval["call_id"].as_str().unwrap()
    );
    assert_eq!(
        server
            .post("leser-key", &path, json!({"decision": "allow"}))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        server
            .post("bob-key", &path, json!({"decision": "allow"}))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        server
            .post("ada-key", &path, json!({"decision": "ask"}))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        server
            .post("ada-key", &path, json!({"decision": "allow"}))
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    server.wait_status("ada-key", &session, "idle").await;
    assert_eq!(
        server
            .post("ada-key", &path, json!({"decision": "allow"}))
            .await
            .0,
        StatusCode::NOT_FOUND
    );

    let (_, log) = server
        .get("ada-key", &format!("/v1/sessions/{session}/log"))
        .await;
    assert!(log.to_string().contains("Notiz Einkauf gespeichert."));
    let metrics = Client::new()
        .get(format!("{}/metrics", server.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for line in [
        "can_runs_started_total 1",
        "can_runs_finished_total{reason=\"completed\"} 1",
        "can_approvals_resolved_total 1",
        "can_tokens_total{tenant=\"acme\"} 30",
        "can_runs_running 0",
    ] {
        assert!(metrics.contains(line), "{line} fehlt:\n{metrics}");
    }
}

#[tokio::test]
async fn idempotency_key_starts_only_one_run() {
    let server = serve(FakeProvider::new().text("einmal")).await;
    let send = || async {
        Client::new()
            .post(format!("{}/v1/sessions", server.base))
            .bearer_auth("ada-key")
            .header("idempotency-key", "bestellung-17")
            .json(&json!({"agent": "helfer", "input": "los"}))
            .send()
            .await
            .unwrap()
    };
    let first = send().await;
    assert_eq!(first.status(), StatusCode::CREATED);
    let first: Value = first.json().await.unwrap();
    server
        .wait_status("ada-key", first["session_id"].as_str().unwrap(), "idle")
        .await;
    let again = send().await;
    assert_eq!(again.status(), StatusCode::OK);
    let again: Value = again.json().await.unwrap();
    assert_eq!(
        (again["session_id"].clone(), again["created"].clone()),
        (first["session_id"].clone(), json!(false))
    );
    assert_eq!(server.sessions.list("acme").await.unwrap().len(), 1);
}

#[tokio::test]
async fn cancel_stops_a_waiting_run() {
    let server = serve(FakeProvider::new().tool_call("save_note", json!({"title": "x"}))).await;
    let session = server.create("ada-key", "los").await;
    server.wait_approval("ada-key").await;
    assert_eq!(
        server
            .post(
                "ada-key",
                &format!("/v1/sessions/{session}/cancel"),
                json!({})
            )
            .await
            .0,
        StatusCode::ACCEPTED
    );
    server.wait_status("ada-key", &session, "stopped").await;
    assert_eq!(
        server
            .post(
                "ada-key",
                &format!("/v1/sessions/{session}/cancel"),
                json!({})
            )
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        server.get("ada-key", "/v1/approvals").await.1,
        json!({"approvals": []})
    );
}

/// Ein Worker fällt aus, während ein Lauf auf eine Freigabe wartet; ein zweiter übernimmt aus
/// Store und Verzeichnis und führt den Lauf zu Ende.
#[tokio::test]
async fn another_worker_takes_over_after_a_crash() {
    let (store, sessions) = (
        Arc::new(MemoryStore::new()),
        Arc::new(MemoryDirectory::new()),
    );
    let first = serve_with(
        FakeProvider::new().tool_call("save_note", json!({"title": "Einkauf"})),
        store.clone(),
        sessions.clone(),
    )
    .await;
    let session = first.create("ada-key", "Speichere eine Notiz").await;
    first.wait_approval("ada-key").await;

    // Der zweite Worker kennt den Lauf nur aus Store und Verzeichnis.
    let second = serve_with(FakeProvider::new().text("Gespeichert."), store, sessions).await;
    assert_eq!(
        second.get("ada-key", "/v1/approvals").await.1,
        json!({"approvals": []})
    );
    assert_eq!(second.app.runs.recover().await, 1);
    let approval = second.wait_approval("ada-key").await;
    assert_eq!(approval["session_id"], session.as_str());
    let path = format!(
        "/v1/sessions/{session}/approvals/{}",
        approval["call_id"].as_str().unwrap()
    );
    assert_eq!(
        second
            .post("ada-key", &path, json!({"decision": "allow"}))
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    second.wait_status("ada-key", &session, "idle").await;
    let (_, log) = second
        .get("ada-key", &format!("/v1/sessions/{session}/log"))
        .await;
    let text = log.to_string();
    assert!(text.contains("Notiz Einkauf gespeichert.") && text.contains("Gespeichert."));
    assert_eq!(
        text.matches("\"type\":\"tool_started\"").count(),
        1,
        "das Tool lief genau einmal"
    );
    assert_eq!(second.app.runs.recover().await, 0);
}

#[tokio::test]
async fn admins_can_verify_that_a_log_is_intact() {
    let server = serve(FakeProvider::new().text("hallo")).await;
    let (status, body) = server
        .post(
            "bob-key",
            "/v1/sessions",
            json!({"agent": "helfer", "input": "hi"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let session = body["session_id"].as_str().unwrap().to_owned();
    server.wait_status("bob-key", &session, "idle").await;
    let (status, body) = server
        .get("bob-key", &format!("/v1/sessions/{session}/verify"))
        .await;
    assert_eq!(
        (status, body),
        (
            StatusCode::OK,
            json!({"intact": true, "first_broken_seq": null})
        )
    );

    let own = server.create("ada-key", "hi").await;
    assert_eq!(
        server
            .get("ada-key", &format!("/v1/sessions/{own}/verify"))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        server
            .get("bob-key", &format!("/v1/sessions/{own}/verify"))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn admins_can_delete_a_session_and_its_log() {
    let server = serve(FakeProvider::new().text("hallo")).await;
    let (_, body) = server
        .post(
            "bob-key",
            "/v1/sessions",
            json!({"agent": "helfer", "input": "hi"}),
        )
        .await;
    let session = body["session_id"].as_str().unwrap().to_owned();
    server.wait_status("bob-key", &session, "idle").await;
    let path = format!("{}/v1/sessions/{session}", server.base);
    let delete = |key: &'static str| Client::new().delete(&path).bearer_auth(key).send();
    assert_eq!(
        delete("ada-key").await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        delete("bob-key").await.unwrap().status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        server
            .get("bob-key", &format!("/v1/sessions/{session}"))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert!(
        can_core::Store::load(server.store.as_ref(), session.parse().unwrap())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        delete("bob-key").await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
}

/// Preise machen aus Token Kosten; das Kostenbudget sperrt, und der Webhook erfährt vom Ende.
#[tokio::test]
async fn costs_budget_and_webhook() {
    use can_core::Pricing;
    use can_server::{App, Directory, Quota, Role, RunManager, router};
    use std::collections::HashMap;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let hook = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hook_url = format!("http://{}/ende", hook.local_addr().unwrap());
    let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = hook.accept().await.unwrap();
        let mut buffer = vec![0u8; 8192];
        let read = socket.read(&mut buffer).await.unwrap();
        socket
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
            .await
            .unwrap();
        let _ = seen_tx.send(String::from_utf8_lossy(&buffer[..read]).into_owned());
    });

    let store = Arc::new(MemoryStore::new());
    let agent = can_runtime::Agent::builder()
        .provider(FakeProvider::new().text("eins").text("zwei"))
        .model("test")
        .store_arc(store.clone())
        .build()
        .unwrap();
    // Der Fake-Provider meldet 10 Eingabe- und 5 Ausgabe-Token: 10 × 0,04 + 5 × 0,04 = 0,60.
    let pricing = Pricing {
        input_per_million: 40_000.0,
        output_per_million: 40_000.0,
        cached_per_million: None,
    };
    let runs = RunManager::with(
        HashMap::from([("helfer".to_owned(), agent)]),
        store,
        Arc::new(MemoryDirectory::new()),
        HashMap::from([("helfer".to_owned(), pricing)]),
        HashMap::from([("acme".to_owned(), hook_url)]),
    );
    let quota = Quota {
        max_concurrent_runs: 4,
        max_tokens: 0,
        max_cost_cents: 50,
    };
    let directory =
        Directory::new()
            .tenant("acme", quota)
            .key("ada-key", "acme", "ada", Role::Operator);
    let app = Arc::new(App {
        runs,
        directory,
        clock: Arc::new(can_runtime::SystemClock),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let routes = router(app.clone());
    tokio::spawn(async move { axum::serve(listener, routes).await.unwrap() });

    let start = || {
        Client::new()
            .post(format!("{base}/v1/sessions"))
            .bearer_auth("ada-key")
            .json(&json!({"agent": "helfer", "input": "hi"}))
            .send()
    };
    assert_eq!(start().await.unwrap().status(), StatusCode::CREATED);
    let notified = seen_rx.await.unwrap();
    assert!(notified.starts_with("POST /ende"), "{notified}");
    let body: Value = serde_json::from_str(notified.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(
        (
            body["tenant"].as_str(),
            body["reason"].as_str(),
            body["tokens"].as_u64()
        ),
        (Some("acme"), Some("completed"), Some(15))
    );
    assert!((body["cost"].as_f64().unwrap() - 0.6).abs() < 1e-9);

    let metrics = Client::new()
        .get(format!("{base}/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains("can_cost_total{tenant=\"acme\"} 0.600000"),
        "{metrics}"
    );
    // 60 Cent verbraucht, 50 erlaubt: der nächste Lauf wird abgewiesen.
    assert_eq!(
        start().await.unwrap().status(),
        StatusCode::TOO_MANY_REQUESTS
    );
}
