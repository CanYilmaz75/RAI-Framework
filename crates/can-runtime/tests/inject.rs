//! Nachrichten, die während eines Laufs dazukommen.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use can_core::{ContentBlock, Decision, Event, FinishReason, Message, Role, SessionId};
use can_providers::FakeProvider;
use can_runtime::{Agent, RunError};
use common::{TestTool, drain};
use serde_json::json;

/// Waits for the first approval request and returns its call id.
async fn wait_for_approval(run: &mut can_runtime::Run) -> can_core::CallId {
    loop {
        if let Event::ApprovalNeeded { call_id } = run.next().await.unwrap().unwrap().event {
            return call_id;
        }
    }
}

#[tokio::test]
async fn a_message_sent_during_tools_lands_after_the_results() {
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call("schreib", json!({"text": "a"}))
            .text("verstanden"),
    );
    let agent = Agent::builder()
        .provider_arc(provider.clone())
        .model("test")
        .tool(TestTool::writer("schreib"))
        .build()
        .unwrap();
    let mut run = agent.run(SessionId::new(), "los").await.unwrap();
    let call_id = wait_for_approval(&mut run).await;
    run.inject("Bitte auf Deutsch antworten.").await.unwrap();
    run.resolve(call_id, Decision::Allow).await.unwrap();
    let rest = drain(&mut run).await.unwrap();
    assert_eq!(run.outcome().await.unwrap().reason, FinishReason::Completed);
    assert!(rest.iter().any(|e| matches!(&e.event, Event::MessageInjected { message } if message.text() == "Bitte auf Deutsch antworten.")));

    // Reihenfolge, die das Modell sieht: Aufruf, Ergebnis, dann die neue Nachricht.
    let request = provider.requests().pop().unwrap();
    let roles: Vec<Role> = request.messages.iter().map(|m| m.role).collect();
    assert_eq!(roles, [Role::User, Role::Assistant, Role::Tool, Role::User]);
    assert!(matches!(
        request.messages[2].content[0],
        ContentBlock::ToolResult { .. }
    ));
    assert_eq!(
        request.messages[3],
        Message::user("Bitte auf Deutsch antworten.")
    );
}

#[tokio::test]
async fn a_message_after_the_end_is_refused() {
    let agent = Agent::builder()
        .provider(FakeProvider::new().text("fertig"))
        .model("test")
        .build()
        .unwrap();
    let mut run = agent.run(SessionId::new(), "los").await.unwrap();
    let controller = run.controller();
    run.outcome().await.unwrap();
    assert!(matches!(
        controller.inject("zu spät").await,
        Err(RunError::Closed)
    ));
}
