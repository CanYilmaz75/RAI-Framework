//! Nachrichten an eine Sitzung, in der gerade ein Lauf unterwegs ist.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use can_providers::FakeProvider;
use common::serve;
use reqwest::StatusCode;
use serde_json::json;

#[tokio::test]
async fn reject_inject_and_queue() {
    let provider = FakeProvider::new()
        .tool_call("save_note", json!({"title": "Einkauf"}))
        .text("erste Antwort")
        .text("zweite Antwort");
    let server = serve(provider).await;
    let session = server.create("ada-key", "Speichere eine Notiz").await;
    let approval = server.wait_approval("ada-key").await;
    let path = format!("/v1/sessions/{session}/messages");

    let rejected = server.post("ada-key", &path, json!({"input": "x"})).await;
    assert_eq!(
        rejected.0,
        StatusCode::CONFLICT,
        "ohne Modus wird abgelehnt"
    );
    let injected = server
        .post(
            "ada-key",
            &path,
            json!({"input": "Und fasse kurz zusammen.", "mode": "inject"}),
        )
        .await;
    assert_eq!(
        (injected.0, injected.1["delivery"].as_str()),
        (StatusCode::ACCEPTED, Some("injected"))
    );
    let queued = server
        .post(
            "ada-key",
            &path,
            json!({"input": "Zweite Frage", "mode": "queue"}),
        )
        .await;
    assert_eq!(
        (queued.1["delivery"].as_str(), queued.1["queued"].as_u64()),
        (Some("queued"), Some(1))
    );

    let resolve = format!(
        "/v1/sessions/{session}/approvals/{}",
        approval["call_id"].as_str().unwrap()
    );
    assert_eq!(
        server
            .post("ada-key", &resolve, json!({"decision": "allow"}))
            .await
            .0,
        StatusCode::NO_CONTENT
    );

    // Der eingereihte zweite Lauf folgt von selbst.
    for _ in 0..400 {
        let log = server
            .get("ada-key", &format!("/v1/sessions/{session}/log"))
            .await
            .1
            .to_string();
        if log.contains("zweite Antwort") && log.matches("\"type\":\"run_finished\"").count() == 2 {
            assert_eq!(log.matches("\"type\":\"run_started\"").count(), 2);
            assert!(
                log.contains("\"type\":\"message_injected\"")
                    && log.contains("Und fasse kurz zusammen.")
            );
            assert!(log.contains("Zweite Frage"));
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("der eingereihte Lauf kam nie");
}

#[tokio::test]
async fn interrupt_cancels_and_starts_over() {
    let provider = FakeProvider::new()
        .tool_call("save_note", json!({"title": "x"}))
        .text("neu angefangen");
    let server = serve(provider).await;
    let session = server.create("ada-key", "Speichere etwas").await;
    server.wait_approval("ada-key").await;
    let path = format!("/v1/sessions/{session}/messages");
    let interrupted = server
        .post(
            "ada-key",
            &path,
            json!({"input": "Lass das, mach etwas anderes.", "mode": "interrupt"}),
        )
        .await;
    assert_eq!(interrupted.1["delivery"], "interrupting");

    for _ in 0..400 {
        let log = server
            .get("ada-key", &format!("/v1/sessions/{session}/log"))
            .await
            .1
            .to_string();
        if log.contains("neu angefangen") {
            assert!(
                log.contains("\"type\":\"cancelled\""),
                "der erste Lauf endete als abgebrochen"
            );
            assert!(log.contains("Lass das, mach etwas anderes."));
            assert!(
                !log.contains("Notiz x gespeichert"),
                "das Tool des ersten Laufs lief nie"
            );
            server.wait_status("ada-key", &session, "idle").await;
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("der neue Lauf kam nie");
}

#[tokio::test]
async fn without_a_running_run_every_mode_starts_one() {
    let server = serve(FakeProvider::new().text("eins").text("zwei")).await;
    let session = server.create("ada-key", "hi").await;
    server.wait_status("ada-key", &session, "idle").await;
    let posted = server
        .post(
            "ada-key",
            &format!("/v1/sessions/{session}/messages"),
            json!({"input": "weiter", "mode": "inject"}),
        )
        .await;
    assert_eq!(posted.1["delivery"], "started");
    let bad = server
        .post(
            "ada-key",
            &format!("/v1/sessions/{session}/messages"),
            json!({"input": "x", "mode": "drängeln"}),
        )
        .await;
    assert!(bad.0.is_client_error());
}
