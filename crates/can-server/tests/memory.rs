//! Das Gedächtnis über die API: pro Mandant getrennt, Schreiben braucht die Rolle Operator.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use can_providers::FakeProvider;
use common::serve;
use reqwest::StatusCode;
use serde_json::json;

#[tokio::test]
async fn memories_are_searchable_per_tenant() {
    let server = serve(FakeProvider::new()).await;
    for text in [
        "Kunde Weber bevorzugt E-Mail",
        "Rechnungen gehen an die Buchhaltung",
    ] {
        let (status, body) = server
            .post("ada-key", "/v1/memories", json!({"text": text}))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    let (status, _) = server
        .post(
            "bob-key",
            "/v1/memories",
            json!({"text": "Globex mag Faxe"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = server
        .get("ada-key", "/v1/memories?query=Weber%20E-Mail")
        .await;
    assert_eq!(status, StatusCode::OK);
    let found = body["memories"].as_array().unwrap();
    assert_eq!(found[0]["text"], "Kunde Weber bevorzugt E-Mail");

    let (_, all) = server.get("ada-key", "/v1/memories").await;
    let all = all["memories"].as_array().unwrap();
    assert_eq!(all.len(), 2, "nur die eigenen Einträge");
    assert_eq!(
        all[0]["text"], "Rechnungen gehen an die Buchhaltung",
        "neueste zuerst"
    );
    let (_, other) = server.get("bob-key", "/v1/memories?query=Weber").await;
    assert!(other["memories"].as_array().unwrap().is_empty());

    // Fremde Einträge lassen sich nicht löschen, eigene schon.
    let id = all[0]["id"].as_str().unwrap();
    let client = reqwest::Client::new();
    let delete = |key: &'static str| {
        client
            .delete(format!("{}/v1/memories/{id}", server.base))
            .bearer_auth(key)
            .send()
    };
    assert_eq!(
        delete("bob-key").await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        delete("ada-key").await.unwrap().status(),
        StatusCode::NO_CONTENT
    );
    let (_, rest) = server.get("ada-key", "/v1/memories").await;
    assert_eq!(rest["memories"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn viewers_read_but_do_not_write() {
    let server = serve(FakeProvider::new()).await;
    let (status, _) = server
        .post("leser-key", "/v1/memories", json!({"text": "geht nicht"}))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = server
        .post("ada-key", "/v1/memories", json!({"text": "  "}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, body) = server.get("leser-key", "/v1/memories").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["memories"].as_array().unwrap().is_empty());
}
