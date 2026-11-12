//! Geplante Läufe: Auslösung, keine Doppelstarts, Kontingent und Sicht pro Mandant.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use can_core::MemoryStore;
use can_providers::FakeProvider;
use can_server::schedule::{ScheduleConfig, fire_due};
use can_server::{MemoryDirectory, SessionDirectory};
use common::{Server, serve_scheduled};
use reqwest::StatusCode;
use serde_json::json;

/// Donnerstag, 12.11.2026, 07:00 UTC in Minuten seit 1970.
const SEVEN: u64 = 20_769 * 24 * 60 + 7 * 60;

fn morning() -> ScheduleConfig {
    ScheduleConfig {
        name: "morgenbericht".into(),
        cron: "0 7 * * 1-5".parse().unwrap(),
        agent: "helfer".into(),
        tenant: "acme".into(),
        user: "zeitplan".into(),
        input: "Speichere den Bericht".into(),
    }
}

async fn server(provider: FakeProvider) -> Server {
    serve_scheduled(
        provider,
        Arc::new(MemoryStore::new()),
        Arc::new(MemoryDirectory::new()),
        vec![morning()],
    )
    .await
}

#[tokio::test]
async fn a_due_schedule_starts_exactly_one_run() {
    let server = server(FakeProvider::new().text("Bericht fertig.")).await;
    assert_eq!(
        fire_due(&server.app, SEVEN - 1).await,
        0,
        "06:59 passt nicht"
    );
    assert_eq!(fire_due(&server.app, SEVEN).await, 1);
    assert_eq!(
        fire_due(&server.app, SEVEN).await,
        0,
        "dieselbe Minute nur einmal"
    );
    // Ein zweiter Worker am selben Sitzungsverzeichnis startet in derselben Minute nichts mehr.
    let other = serve_scheduled(
        FakeProvider::new(),
        Arc::new(MemoryStore::new()),
        server.sessions.clone(),
        vec![morning()],
    )
    .await;
    assert_eq!(fire_due(&other.app, SEVEN).await, 0);
    let elsewhere = other.app.schedules.last("morgenbericht").unwrap();
    assert_eq!(elsewhere.outcome, "elsewhere");
    let sessions = server.sessions.list("acme").await.unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(
        (sessions[0].user.as_str(), sessions[0].agent.as_str()),
        ("zeitplan", "helfer")
    );
    let id = sessions[0].id.to_string();
    server.wait_status("ada-key", &id, "idle").await;

    let (status, body) = server.get("ada-key", "/v1/schedules").await;
    assert_eq!(status, StatusCode::OK);
    let schedule = &body["schedules"][0];
    assert_eq!(schedule["name"], "morgenbericht");
    assert_eq!(schedule["cron"], "0 7 * * 1-5");
    assert_eq!(schedule["last"]["outcome"], "started");
    assert_eq!(schedule["last"]["session_id"], json!(id));
    assert!(schedule["next_run"].as_u64().is_some());
    let (_, other) = server.get("bob-key", "/v1/schedules").await;
    assert_eq!(
        other,
        json!({"schedules": []}),
        "fremder Mandant sieht nichts"
    );
}

#[tokio::test]
async fn a_full_quota_skips_the_firing() {
    // Der erste Lauf wartet auf eine Freigabe und belegt das Kontingent von einem Lauf.
    let provider = FakeProvider::new()
        .tool_call("save_note", json!({"title": "Bericht"}))
        .text("ok");
    let server = server(provider).await;
    assert_eq!(fire_due(&server.app, SEVEN).await, 1);
    server.wait_approval("ada-key").await;
    let next_day = SEVEN + 24 * 60;
    assert_eq!(fire_due(&server.app, next_day).await, 0);
    let last = server.app.schedules.last("morgenbericht").unwrap();
    assert_eq!(last.outcome, "skipped");
    assert!(last.reason.unwrap().contains("gleichzeitiger"));
    assert_eq!(server.sessions.list("acme").await.unwrap().len(), 1);
}

#[tokio::test]
async fn the_loop_fires_when_the_minute_comes() {
    use can_core::{InMemoryBank, ManualClock, Timestamp};
    use can_server::{App, Schedules};
    let server = server(FakeProvider::new().text("Bericht fertig.")).await;
    // 06:59:30: noch nichts fällig.
    let clock = Arc::new(ManualClock::new(Timestamp((SEVEN - 1) * 60_000 + 30_000)));
    let app = Arc::new(App {
        runs: server.app.runs.clone(),
        directory: common::directory(),
        clock: clock.clone(),
        memory: Arc::new(InMemoryBank::new()),
        schedules: Arc::new(Schedules::new(vec![morning()])),
    });
    let task = tokio::spawn(can_server::schedule::run(app.clone()));
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(server.sessions.list("acme").await.unwrap().is_empty());
    clock.advance(std::time::Duration::from_secs(30));
    let mut started = false;
    for _ in 0..100 {
        if server.sessions.list("acme").await.unwrap().len() == 1 {
            started = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    task.abort();
    assert!(started, "um 07:00 startet der Lauf");
    assert_eq!(
        app.schedules.last("morgenbericht").unwrap().outcome,
        "started"
    );
}
