//! Plan, geänderte Freigaben und Verzweigen über die HTTP-API.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use can_providers::FakeProvider;
use common::serve;
use reqwest::StatusCode;
use serde_json::json;

#[tokio::test]
async fn approvals_can_carry_edited_arguments() {
    let provider = FakeProvider::new()
        .tool_call("save_note", json!({"title": "Entwurf"}))
        .text("Gespeichert.");
    let server = serve(provider).await;
    let session = server.create("ada-key", "Speichere eine Notiz").await;
    let approval = server.wait_approval("ada-key").await;
    let path = format!(
        "/v1/sessions/{session}/approvals/{}",
        approval["call_id"].as_str().unwrap()
    );

    // Falscher Typ und falsche Kombination werden abgewiesen; die Rückfrage bleibt offen.
    let wrong = server
        .post(
            "ada-key",
            &path,
            json!({"decision": "allow", "input": {"title": 7}}),
        )
        .await;
    assert_eq!(wrong.0, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(wrong.1["error"].as_str().unwrap().contains("title"));
    let mixed = server
        .post(
            "ada-key",
            &path,
            json!({"decision": "deny", "input": {"title": "x"}}),
        )
        .await;
    assert_eq!(mixed.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        server.get("ada-key", "/v1/approvals").await.1["approvals"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let edited = server
        .post(
            "ada-key",
            &path,
            json!({"decision": "allow", "input": {"title": "Final"}}),
        )
        .await;
    assert_eq!(edited.0, StatusCode::NO_CONTENT);
    server.wait_status("ada-key", &session, "idle").await;
    let log = server
        .get("ada-key", &format!("/v1/sessions/{session}/log"))
        .await
        .1
        .to_string();
    assert!(
        log.contains("Notiz Final gespeichert.") && !log.contains("Notiz Entwurf gespeichert.")
    );
    assert!(log.contains("\"type\":\"tool_args_edited\""));
}

#[tokio::test]
async fn sessions_can_be_forked_within_a_tenant() {
    let server = serve(FakeProvider::new().text("eins").text("zwei")).await;
    let session = server.create("ada-key", "erste Frage").await;
    server.wait_status("ada-key", &session, "idle").await;

    let fork = server
        .post(
            "ada-key",
            &format!("/v1/sessions/{session}/fork"),
            json!({"at": 3}),
        )
        .await;
    assert_eq!(fork.0, StatusCode::CREATED);
    let branch = fork.1["session_id"].as_str().unwrap().to_owned();
    assert_ne!(branch, session);
    assert_eq!(
        server.get("ada-key", "/v1/sessions").await.1["sessions"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let next = server
        .post(
            "ada-key",
            &format!("/v1/sessions/{branch}/messages"),
            json!({"input": "andere Frage"}),
        )
        .await;
    assert_eq!(next.0, StatusCode::ACCEPTED);
    server.wait_status("ada-key", &branch, "idle").await;
    let branch_log = server
        .get("ada-key", &format!("/v1/sessions/{branch}/log"))
        .await
        .1;
    assert_eq!(branch_log["events"].as_array().unwrap().len(), 8);
    let source_log = server
        .get("ada-key", &format!("/v1/sessions/{session}/log"))
        .await
        .1;
    assert_eq!(
        source_log["events"].as_array().unwrap().len(),
        4,
        "die Quelle bleibt unverändert"
    );

    // Fremde Mandanten, Leser und unbekannte Sitzungen.
    let path = format!("/v1/sessions/{session}/fork");
    assert_eq!(
        server.post("bob-key", &path, json!({"at": 3})).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        server.post("leser-key", &path, json!({"at": 3})).await.0,
        StatusCode::FORBIDDEN
    );
    let unknown = format!("/v1/sessions/{}/fork", can_core::SessionId::new());
    assert_eq!(
        server.post("ada-key", &unknown, json!({"at": 3})).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn the_plan_shows_up_on_the_session() {
    let server = serve(FakeProvider::new().text("ohne Plan")).await;
    let session = server.create("ada-key", "hi").await;
    server.wait_status("ada-key", &session, "idle").await;
    let body = server
        .get("ada-key", &format!("/v1/sessions/{session}"))
        .await
        .1;
    assert_eq!(body["plan"], json!([]));
}
