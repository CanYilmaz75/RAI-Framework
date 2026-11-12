//! Gemeinsamer Aufbau der Server-Tests: ein Dienst auf einem freien Port mit Fake-Provider.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use can_core::{MemoryStore, ToolError};
use can_macros::tool;
use can_providers::FakeProvider;
use can_runtime::{Agent, SystemClock};
use can_server::{App, Directory, MemoryDirectory, Quota, Role, RunManager, router};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};

/// Legt eine Notiz ab (hier nur zum Schein).
#[tool(effect = "write")]
async fn save_note(title: String) -> Result<String, ToolError> {
    Ok(format!("Notiz {title} gespeichert."))
}

pub struct Server {
    pub base: String,
    pub app: Arc<App>,
    pub store: Arc<MemoryStore>,
    pub sessions: Arc<MemoryDirectory>,
}

pub fn directory() -> Directory {
    Directory::new()
        .tenant(
            "acme",
            Quota {
                max_concurrent_runs: 1,
                max_tokens: 0,
                max_cost_cents: 0,
            },
        )
        .key("ada-key", "acme", "ada", Role::Operator)
        .key("leser-key", "acme", "lea", Role::Viewer)
        .key("bob-key", "globex", "bob", Role::Admin)
}

pub fn agent(provider: FakeProvider, store: Arc<MemoryStore>) -> Agent {
    Agent::builder()
        .provider(provider)
        .model("test")
        .tool(save_note)
        .store_arc(store)
        .owner("worker-1")
        .build()
        .unwrap()
}

pub async fn serve_with(
    provider: FakeProvider,
    store: Arc<MemoryStore>,
    sessions: Arc<MemoryDirectory>,
) -> Server {
    let agents = HashMap::from([("helfer".to_owned(), agent(provider, store.clone()))]);
    let runs = RunManager::new(agents, store.clone(), sessions.clone());
    let app = Arc::new(App {
        runs,
        directory: directory(),
        clock: Arc::new(SystemClock),
        memory: Arc::new(can_core::InMemoryBank::new()),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let routes = router(app.clone());
    tokio::spawn(async move { axum::serve(listener, routes).await.unwrap() });
    Server {
        base,
        app,
        store,
        sessions,
    }
}

pub async fn serve(provider: FakeProvider) -> Server {
    serve_with(
        provider,
        Arc::new(MemoryStore::new()),
        Arc::new(MemoryDirectory::new()),
    )
    .await
}

impl Server {
    pub async fn post(&self, key: &str, path: &str, body: Value) -> (StatusCode, Value) {
        let response = Client::new()
            .post(format!("{}{path}", self.base))
            .bearer_auth(key)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    pub async fn get(&self, key: &str, path: &str) -> (StatusCode, Value) {
        let response = Client::new()
            .get(format!("{}{path}", self.base))
            .bearer_auth(key)
            .send()
            .await
            .unwrap();
        let status = response.status();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    pub async fn create(&self, key: &str, input: &str) -> String {
        let (status, body) = self
            .post(
                key,
                "/v1/sessions",
                json!({"agent": "helfer", "input": input}),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["session_id"].as_str().unwrap().to_owned()
    }

    /// Wartet, bis die Sitzung den Zustand erreicht hat.
    pub async fn wait_status(&self, key: &str, session: &str, wanted: &str) {
        for _ in 0..400 {
            let (_, body) = self.get(key, &format!("/v1/sessions/{session}")).await;
            if body["session"]["status"] == wanted {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("Sitzung {session} wurde nie {wanted}");
    }

    /// Wartet auf die erste offene Rückfrage des Mandanten.
    pub async fn wait_approval(&self, key: &str) -> Value {
        for _ in 0..400 {
            let (_, body) = self.get(key, "/v1/approvals").await;
            if let Some(first) = body["approvals"].get(0) {
                return first.clone();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("keine Rückfrage aufgetaucht");
    }
}
