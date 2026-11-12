//! Kosten, Kostenbudget und Webhook am Ende eines Laufs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use can_core::MemoryStore;
use can_providers::FakeProvider;
use can_server::MemoryDirectory;
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};

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
        memory: Arc::new(can_core::InMemoryBank::new()),
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
