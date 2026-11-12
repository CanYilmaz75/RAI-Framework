//! Hintergrund-Sub-Agenten mit eigener Sitzung.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use can_core::{
    CallId, Effect, ManualClock, ProgressSink, SecretsHandle, SessionId, Tool, ToolCtx, ToolError,
};
use can_providers::FakeProvider;
use can_runtime::{Agent, BackgroundAgents, TaskStatus};
use common::{Behavior, TestTool};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn ctx(session: SessionId) -> ToolCtx {
    ToolCtx {
        session,
        call_id: CallId::new(),
        workdir: std::env::temp_dir(),
        cancel: CancellationToken::new(),
        clock: Arc::new(can_runtime::SystemClock),
        progress: ProgressSink::noop(),
        secrets: SecretsHandle::empty(),
        namespace: can_core::DEFAULT_NAMESPACE.into(),
    }
}

struct Helper {
    tools: Vec<Arc<dyn Tool>>,
    agents: BackgroundAgents,
    provider: Arc<FakeProvider>,
}

fn helper(provider: FakeProvider, tool: Option<TestTool>) -> Helper {
    let provider = Arc::new(provider);
    let mut builder = Agent::builder()
        .provider_arc(provider.clone())
        .model("klein");
    if let Some(tool) = tool {
        builder = builder.tool(tool);
    }
    let agents = BackgroundAgents::new("helfer", "recherchiert", builder.build().unwrap());
    Helper {
        tools: agents.tools(),
        agents,
        provider,
    }
}

impl Helper {
    fn tool(&self, action: &str) -> &Arc<dyn Tool> {
        self.tools
            .iter()
            .find(|tool| tool.spec().name == format!("helfer__{action}"))
            .unwrap()
    }

    async fn call(&self, action: &str, args: Value, session: SessionId) -> Result<String, String> {
        match self.tool(action).call(args, &ctx(session)).await {
            Ok(output) => Ok(output.as_text()),
            Err(ToolError::ForModel(message)) => Err(message),
            Err(other) => panic!("{other:?}"),
        }
    }

    async fn start(&self, task: &str, session: SessionId) -> String {
        let reply = self
            .call("start", json!({"task": task}), session)
            .await
            .unwrap();
        reply.split_whitespace().nth(1).unwrap().to_owned()
    }
}

#[test]
fn five_tools_with_sensible_effects() {
    let helper = helper(FakeProvider::new(), None);
    let specs: Vec<(String, Effect)> = helper
        .tools
        .iter()
        .map(|t| (t.spec().name.clone(), t.spec().effect))
        .collect();
    assert_eq!(
        specs,
        [
            ("helfer__start".to_owned(), Effect::External),
            ("helfer__check".to_owned(), Effect::ReadOnly),
            ("helfer__message".to_owned(), Effect::External),
            ("helfer__cancel".to_owned(), Effect::Write),
            ("helfer__list".to_owned(), Effect::ReadOnly),
        ]
    );
}

#[tokio::test]
async fn a_task_runs_in_the_background_and_reports_its_result() {
    let provider = FakeProvider::new()
        .tool_call("dauert", json!({}))
        .text("Ergebnis: 42");
    let helper = helper(
        provider,
        Some(TestTool::new("dauert", Effect::ReadOnly, Behavior::Sleep)),
    );
    let parent = SessionId::new();
    let id = helper.start("Finde die Antwort", parent).await;

    // Das Tool schläft 100 ms: direkt nach dem Start läuft die Aufgabe noch.
    let early = helper
        .call("check", json!({"task_id": id}), parent)
        .await
        .unwrap();
    assert!(early.ends_with("läuft (Finde die Antwort)"), "{early}");
    let done = helper
        .call("check", json!({"task_id": id, "wait_seconds": 10}), parent)
        .await
        .unwrap();
    assert_eq!(done, format!("{id}: fertig\nErgebnis: 42"));
    let listed = helper.call("list", json!({}), parent).await.unwrap();
    assert!(listed.starts_with(&id));
}

#[tokio::test]
async fn tasks_belong_to_the_session_that_started_them() {
    let helper = helper(FakeProvider::new().text("geheim"), None);
    let (owner, stranger) = (SessionId::new(), SessionId::new());
    let id = helper.start("x", owner).await;
    assert!(
        helper
            .call("check", json!({"task_id": id}), stranger)
            .await
            .unwrap_err()
            .contains("Unbekannte Aufgabe")
    );
    assert_eq!(
        helper.call("list", json!({}), stranger).await.unwrap(),
        "Keine Aufgaben."
    );
    assert!(
        helper
            .call("check", json!({"task_id": "kaputt"}), owner)
            .await
            .unwrap_err()
            .contains("Ungültige")
    );
    assert!(
        helper
            .call("start", json!({"task": "  "}), owner)
            .await
            .unwrap_err()
            .contains("leer")
    );
}

#[tokio::test]
async fn a_message_continues_a_finished_task_with_its_history() {
    let helper = helper(
        FakeProvider::new()
            .text("erste Antwort")
            .text("zweite Antwort"),
        None,
    );
    let parent = SessionId::new();
    let id = helper.start("erste Frage", parent).await;
    helper
        .call("check", json!({"task_id": id, "wait_seconds": 10}), parent)
        .await
        .unwrap();

    let reply = helper
        .call(
            "message",
            json!({"task_id": id, "message": "Und weiter?"}),
            parent,
        )
        .await
        .unwrap();
    assert!(reply.contains("fortgesetzt"), "{reply}");
    let done = helper
        .call("check", json!({"task_id": id, "wait_seconds": 10}), parent)
        .await
        .unwrap();
    assert!(done.ends_with("zweite Antwort"));
    let history: Vec<String> = helper.provider.requests()[1]
        .messages
        .iter()
        .map(|m| m.text())
        .collect();
    assert_eq!(
        history,
        ["erste Frage", "erste Antwort", "Und weiter?"],
        "der Sub-Agent kennt seinen Verlauf"
    );
}

#[tokio::test]
async fn a_running_task_can_be_cancelled() {
    let provider = FakeProvider::new().tool_call("haengt", json!({}));
    let helper = helper(
        provider,
        Some(TestTool::new("haengt", Effect::ReadOnly, Behavior::Hang)),
    );
    let parent = SessionId::new();
    let id = helper.start("dauert ewig", parent).await;
    assert!(
        helper
            .call("cancel", json!({"task_id": id}), parent)
            .await
            .unwrap()
            .contains("abgebrochen")
    );
    let after = helper
        .call("check", json!({"task_id": id, "wait_seconds": 10}), parent)
        .await
        .unwrap();
    assert!(after.contains("fehlgeschlagen: Cancelled"), "{after}");
    assert_eq!(
        helper.agents.status(parent, id.parse().unwrap()),
        Some(TaskStatus::Failed {
            reason: "Cancelled".into()
        })
    );
    assert!(
        helper
            .call("cancel", json!({"task_id": id}), parent)
            .await
            .unwrap()
            .contains("läuft nicht mehr")
    );
}

#[tokio::test]
async fn waiting_respects_the_clock() {
    let provider = FakeProvider::new().tool_call("haengt", json!({}));
    let helper = helper(
        provider,
        Some(TestTool::new("haengt", Effect::ReadOnly, Behavior::Hang)),
    );
    let parent = SessionId::new();
    let id = helper.start("dauert", parent).await;
    let clock = Arc::new(ManualClock::default());
    let mut cx = ctx(parent);
    cx.clock = clock.clone();
    let check = helper.tool("check").clone();
    let waiting = tokio::spawn(async move {
        check
            .call(json!({"task_id": id, "wait_seconds": 30}), &cx)
            .await
    });
    tokio::task::yield_now().await;
    clock.advance(std::time::Duration::from_secs(31));
    let text = waiting.await.unwrap().unwrap().as_text();
    assert!(
        text.contains("läuft"),
        "nach Ablauf der Wartezeit kommt der Zwischenstand: {text}"
    );
}
