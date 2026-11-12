//! Der Dienst von außen: Anmeldung, Event-Strom, Mandantentrennung.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use can_core::Store;
use can_providers::FakeProvider;
use common::serve;
use reqwest::{Client, StatusCode};
use serde_json::json;

#[tokio::test]
async fn keys_roles_and_unknown_things() {
    let server = serve(FakeProvider::new()).await;
    let start = json!({"agent": "helfer", "input": "hi"});
    assert_eq!(
        server.post("falsch", "/v1/sessions", start.clone()).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        server.post("leser-key", "/v1/sessions", start).await.0,
        StatusCode::FORBIDDEN
    );
    let unknown = server
        .post(
            "ada-key",
            "/v1/sessions",
            json!({"agent": "fehlt", "input": "hi"}),
        )
        .await;
    assert_eq!(unknown.0, StatusCode::NOT_FOUND);
    assert_eq!(
        server.get("ada-key", "/v1/agents").await.1,
        json!({"agents": ["helfer"]})
    );
    let health = Client::new()
        .get(format!("{}/healthz", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(health.text().await.unwrap(), "ok");
    let ui = Client::new().get(&server.base).send().await.unwrap();
    assert!(ui.status().is_success());
}

#[tokio::test]
async fn run_events_stream_and_can_be_resumed_from_a_position() {
    let server = serve(
        FakeProvider::new()
            .text("Hallo Ada.")
            .text("Gern geschehen."),
    )
    .await;
    let session = server.create("ada-key", "Sag hallo").await;
    server.wait_status("ada-key", &session, "idle").await;

    let stream = |query: &'static str| {
        let url = format!("{}/v1/sessions/{session}/events{query}", server.base);
        async move {
            Client::new()
                .get(url)
                .bearer_auth("leser-key")
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        }
    };
    let all = stream("").await;
    let kinds: Vec<&str> = all
        .lines()
        .filter_map(|line| line.strip_prefix("event: "))
        .collect();
    assert_eq!(
        kinds,
        [
            "run_started",
            "model_requested",
            "model_responded",
            "run_finished"
        ]
    );
    assert!(all.contains("id: 0\n") && all.contains("id: 3\n") && all.contains("Hallo Ada."));
    let rest = stream("?after=1").await;
    assert!(!rest.contains("event: run_started") && rest.contains("event: run_finished"));
    let resumed = Client::new()
        .get(format!("{}/v1/sessions/{session}/events", server.base))
        .bearer_auth("ada-key")
        .header("last-event-id", "2")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(
        resumed
            .lines()
            .filter(|line| line.starts_with("event: "))
            .count(),
        1
    );

    // Folgenachricht in derselben Sitzung.
    let (status, _) = server
        .post(
            "ada-key",
            &format!("/v1/sessions/{session}/messages"),
            json!({"input": "Danke"}),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    server.wait_status("ada-key", &session, "idle").await;
    let (_, log) = server
        .get("ada-key", &format!("/v1/sessions/{session}/log?after=3"))
        .await;
    assert_eq!(log["events"].as_array().unwrap().len(), 4);
    assert_eq!(
        server
            .store
            .load(session.parse().unwrap())
            .await
            .unwrap()
            .len(),
        8
    );
}

#[tokio::test]
async fn tenants_cannot_see_each_other() {
    let server = serve(FakeProvider::new().text("geheim für acme")).await;
    let session = server.create("ada-key", "hi").await;
    server.wait_status("ada-key", &session, "idle").await;
    for path in ["", "/log", "/events"] {
        let (status, _) = server
            .get("bob-key", &format!("/v1/sessions/{session}{path}"))
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
    let (status, _) = server
        .post(
            "bob-key",
            &format!("/v1/sessions/{session}/messages"),
            json!({"input": "x"}),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        server.get("bob-key", "/v1/sessions").await.1,
        json!({"sessions": []})
    );
    assert_eq!(
        server.get("ada-key", "/v1/sessions").await.1["sessions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

/// Die OpenAPI-Beschreibung nennt jede Route, die der Dienst unter /v1 anbietet.
#[tokio::test]
async fn openapi_document_is_served_and_covers_the_routes() {
    let server = serve(FakeProvider::new()).await;
    let response = Client::new()
        .get(format!("{}/openapi.json", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["content-type"], "application/json");
    let document: serde_json::Value = response.json().await.unwrap();
    assert_eq!(document["openapi"], "3.1.0");
    let paths = document["paths"].as_object().unwrap();
    for path in [
        "/v1/agents",
        "/v1/sessions",
        "/v1/sessions/{id}",
        "/v1/sessions/{id}/messages",
        "/v1/sessions/{id}/events",
        "/v1/sessions/{id}/log",
        "/v1/sessions/{id}/verify",
        "/v1/sessions/{id}/cancel",
        "/v1/sessions/{id}/resume",
        "/v1/sessions/{id}/fork",
        "/v1/sessions/{id}/approvals/{call_id}",
        "/v1/approvals",
        "/healthz",
        "/metrics",
    ] {
        assert!(paths.contains_key(path), "{path} fehlt in der Beschreibung");
    }
    assert!(paths["/v1/sessions/{id}"]["delete"].is_object());
}
