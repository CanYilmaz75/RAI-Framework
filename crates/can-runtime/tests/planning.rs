//! Plan des Agenten als Events und Freigaben mit geänderten Argumenten.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use can_core::{
    ContentBlock, Event, FinishReason, MemoryStore, Role, RunState, SessionId, Store, TodoStatus,
};
use can_providers::FakeProvider;
use can_runtime::Agent;
use can_tools::WriteTodos;
use common::{TestTool, drain};
use serde_json::json;

#[tokio::test]
async fn the_plan_is_recorded_as_events_and_survives_in_the_state() {
    let first = json!({"todos": [{"content": "Lesen", "status": "in_progress"}, {"content": "Schreiben", "status": "pending"}]});
    let second = json!({"todos": [{"content": "Lesen", "status": "completed"}, {"content": "Schreiben", "status": "in_progress"}]});
    let provider = FakeProvider::new()
        .tool_call("write_todos", first)
        .tool_call("write_todos", second)
        .text("fertig");
    let store = Arc::new(MemoryStore::new());
    let agent = Agent::builder()
        .provider(provider)
        .model("test")
        .tool(WriteTodos::new())
        .store_arc(store.clone())
        .build()
        .unwrap();
    let session = SessionId::new();
    let mut run = agent.run(session, "Mach einen Plan").await.unwrap();
    let events = drain(&mut run).await.unwrap();
    assert_eq!(run.outcome().await.unwrap().reason, FinishReason::Completed);

    let plans: Vec<Vec<(String, TodoStatus)>> = events
        .iter()
        .filter_map(|envelope| match &envelope.event {
            Event::PlanUpdated { todos } => Some(
                todos
                    .iter()
                    .map(|t| (t.content.clone(), t.status))
                    .collect(),
            ),
            _ => None,
        })
        .collect();
    assert_eq!(
        plans.len(),
        2,
        "ohne Rückfrage: das Planungs-Tool ändert nichts außerhalb der Sitzung"
    );
    assert_eq!(
        plans[1],
        vec![
            ("Lesen".to_owned(), TodoStatus::Completed),
            ("Schreiben".to_owned(), TodoStatus::InProgress)
        ]
    );
    // Der Plan folgt direkt auf das Ergebnis des Tools und steht nach dem Falten im Zustand.
    let kinds: Vec<&str> = events
        .iter()
        .map(|envelope| envelope.event.kind())
        .collect();
    let finished = kinds
        .iter()
        .position(|kind| *kind == "tool_finished")
        .unwrap();
    assert_eq!(kinds[finished + 1], "plan_updated");
    let state = RunState::fold(agent.config().clone(), &store.load(session).await.unwrap());
    assert_eq!(state.plan.len(), 2);
    assert_eq!(state.plan[0].status, TodoStatus::Completed);
}

#[tokio::test]
async fn a_failed_plan_call_changes_nothing() {
    // `status` fehlt: die Schema-Prüfung lehnt ab, bevor das Tool läuft.
    let provider = FakeProvider::new()
        .tool_call("write_todos", json!({"todos": [{"content": "x"}]}))
        .text("ok");
    let agent = Agent::builder()
        .provider(provider)
        .model("test")
        .tool(WriteTodos::new())
        .build()
        .unwrap();
    let mut run = agent.run(SessionId::new(), "los").await.unwrap();
    let events = drain(&mut run).await.unwrap();
    assert!(
        !events
            .iter()
            .any(|envelope| matches!(envelope.event, Event::PlanUpdated { .. }))
    );
}

fn tool_result(provider: &FakeProvider) -> String {
    let request = provider.requests().pop().unwrap();
    let message = request
        .messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Tool)
        .unwrap();
    match &message.content[0] {
        ContentBlock::ToolResult { output, .. } => output.as_text(),
        other => panic!("{other:?}"),
    }
}

async fn wait_for_approval(run: &mut can_runtime::Run) -> can_core::CallId {
    loop {
        if let Event::ApprovalNeeded { call_id } = run.next().await.unwrap().unwrap().event {
            return call_id;
        }
    }
}

#[tokio::test]
async fn approval_can_change_the_arguments_and_the_model_is_told() {
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call("schreib", json!({"text": "an alle"}))
            .text("gesendet"),
    );
    let tool = TestTool::writer("schreib");
    let calls = tool.counter();
    let agent = Agent::builder()
        .provider_arc(provider.clone())
        .model("test")
        .tool(tool)
        .build()
        .unwrap();
    let mut run = agent
        .run(SessionId::new(), "Schreib eine Nachricht")
        .await
        .unwrap();
    let call_id = wait_for_approval(&mut run).await;
    run.approve_with(call_id, json!({"text": "nur ans Team"}))
        .await
        .unwrap();
    let rest = drain(&mut run).await.unwrap();

    let kinds: Vec<&str> = rest.iter().map(|envelope| envelope.event.kind()).collect();
    assert_eq!(
        kinds[..4],
        [
            "tool_args_edited",
            "approval_resolved",
            "tool_started",
            "tool_finished"
        ]
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let result = tool_result(&provider);
    assert!(
        result.contains("echo: nur ans Team"),
        "das Tool lief mit den geänderten Argumenten: {result}"
    );
    assert!(result.starts_with(
        "[Ein Mensch hat die Argumente vor der Ausführung geändert in: {\"text\":\"nur ans Team\"}]"
    ));
}

#[tokio::test]
async fn edited_arguments_must_match_the_schema_or_the_question_stays_open() {
    let provider = FakeProvider::new()
        .tool_call("schreib", json!({"text": "a"}))
        .text("ok");
    let tool = TestTool::writer("schreib");
    let calls = tool.counter();
    let agent = Agent::builder()
        .provider(provider)
        .model("test")
        .tool(tool)
        .build()
        .unwrap();
    let mut run = agent.run(SessionId::new(), "los").await.unwrap();
    let call_id = wait_for_approval(&mut run).await;
    run.approve_with(call_id, json!({"text": 42}))
        .await
        .unwrap();
    tokio::task::yield_now().await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "ungültige Argumente starten nichts"
    );
    // Die Rückfrage ist noch offen und lässt sich regulär beantworten.
    run.controller()
        .approve_with(call_id, json!({"text": "b"}))
        .await
        .unwrap();
    assert_eq!(run.outcome().await.unwrap().reason, FinishReason::Completed);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

/// Verzweigen: die neue Sitzung übernimmt den Verlauf bis zur Position, die Quelle bleibt.
#[tokio::test]
async fn fork_copies_the_log_up_to_a_position_and_both_sessions_continue() {
    let provider = Arc::new(
        FakeProvider::new()
            .text("eins")
            .text("zwei")
            .text("zwei-b")
            .text("drei"),
    );
    let store = Arc::new(MemoryStore::new());
    let agent = Agent::builder()
        .provider_arc(provider.clone())
        .model("test")
        .store_arc(store.clone())
        .build()
        .unwrap();
    let source = SessionId::new();
    agent.complete(source, "erste Frage").await.unwrap();
    agent.complete(source, "zweite Frage").await.unwrap();

    // Positionen 0 bis 3 sind der erste Lauf.
    let fork = agent.fork(source, 3).await.unwrap();
    assert_ne!(fork, source);
    assert_eq!(store.load(fork).await.unwrap().len(), 4);
    assert_eq!(
        store.load(source).await.unwrap().len(),
        8,
        "die Quelle bleibt unverändert"
    );

    assert_eq!(
        agent
            .complete(fork, "andere zweite Frage")
            .await
            .unwrap()
            .text,
        "zwei-b"
    );
    let texts: Vec<String> = provider.requests()[2]
        .messages
        .iter()
        .map(|m| m.text())
        .collect();
    assert_eq!(
        texts,
        ["erste Frage", "eins", "andere zweite Frage"],
        "der Zweig kennt nur den ersten Lauf"
    );
    assert_eq!(
        agent.complete(source, "dritte Frage").await.unwrap().text,
        "drei"
    );
    assert_eq!(provider.requests()[3].messages.len(), 5);
}

#[tokio::test]
async fn fork_in_the_middle_of_a_run_closes_it_cleanly() {
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call("schreib", json!({"text": "x"}))
            .text("neu angefangen"),
    );
    let store = Arc::new(MemoryStore::new());
    let agent = Agent::builder()
        .provider_arc(provider.clone())
        .model("test")
        .tool(TestTool::writer("schreib"))
        .store_arc(store.clone())
        .build()
        .unwrap();
    let source = SessionId::new();
    let mut run = agent.run(source, "los").await.unwrap();
    wait_for_approval(&mut run).await;

    // Schnitt nach der Tool-Anforderung, vor jeder Freigabe.
    let fork = agent.fork(source, 3).await.unwrap();
    let log = store.load(fork).await.unwrap();
    assert!(matches!(
        log.last().unwrap().event,
        Event::RunFinished {
            reason: FinishReason::Cancelled
        }
    ));
    assert!(
        log.iter()
            .enumerate()
            .all(|(position, envelope)| envelope.seq == position as u64)
    );
    assert_eq!(
        agent.complete(fork, "anders").await.unwrap().text,
        "neu angefangen"
    );
    // Der offene Aufruf hat im Zweig ein Ergebnis, sonst wäre der Verlauf für das Modell ungültig.
    let request = provider.requests().pop().unwrap();
    assert!(request.messages.iter().any(|m| m.role == Role::Tool));

    assert!(matches!(
        agent.fork(SessionId::new(), 5).await,
        Err(can_runtime::RunError::NothingToFork)
    ));
    run.cancel();
}
