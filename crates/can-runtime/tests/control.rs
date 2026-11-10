//! Freigaben, Abbruch und Zeitlimits.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use can_core::{
    ContentBlock, Decision, Effect, Event, FinishReason, LimitKind, Limits, ManualClock, Role,
    SessionId, Timestamp,
};
use can_providers::FakeProvider;
use can_runtime::{Agent, Run};
use common::{Behavior, TestTool, drain, kinds, tick};
use serde_json::json;

/// Liest Events, bis eine Freigabe nötig ist, und beantwortet sie.
async fn answer_approval(run: &mut Run, decision: Decision) {
    loop {
        let envelope = run.next().await.unwrap().unwrap();
        if let Event::ApprovalNeeded { call_id } = envelope.event {
            run.resolve(call_id, decision).await.unwrap();
            return;
        }
    }
}

fn denial_seen_by_model(provider: &FakeProvider) -> bool {
    let request = provider.requests().pop().unwrap();
    request
        .messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .any(|m| {
            matches!(&m.content[0], ContentBlock::ToolResult { output, is_error: true, .. }
            if output.as_text().contains("abgelehnt"))
        })
}

#[tokio::test]
async fn write_tool_asks_and_runs_after_approval() {
    let provider = FakeProvider::new()
        .tool_call("schreib", json!({"text": "x"}))
        .text("gespeichert");
    let tool = TestTool::writer("schreib");
    let calls = tool.counter();
    let agent = Agent::builder()
        .provider(provider)
        .model("test")
        .tool(tool)
        .build()
        .unwrap();
    let mut run = agent.run(SessionId::new(), "speichere x").await.unwrap();
    answer_approval(&mut run, Decision::Allow).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "vor der Freigabe läuft nichts"
    );
    let rest = drain(&mut run).await.unwrap();
    assert_eq!(
        kinds(&rest)[..3],
        ["approval_resolved", "tool_started", "tool_finished"]
    );
    assert!(matches!(
        rest[0].event,
        Event::ApprovalResolved {
            decision: Decision::Allow,
            by: can_core::Approver::Human,
            ..
        }
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(run.outcome().await.unwrap().text, "gespeichert");
}

/// Abnahme Phase 2: ein abgelehnter Aufruf läuft nie, und das Modell sieht die Ablehnung.
#[tokio::test]
async fn denied_call_never_runs_and_model_sees_the_denial() {
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call("schreib", json!({}))
            .text("dann eben nicht"),
    );
    let tool = TestTool::writer("schreib");
    let calls = tool.counter();
    let agent = Agent::builder()
        .provider_arc(provider.clone())
        .model("test")
        .tool(tool)
        .build()
        .unwrap();
    let mut run = agent.run(SessionId::new(), "los").await.unwrap();
    answer_approval(&mut run, Decision::Deny).await;
    let rest = drain(&mut run).await.unwrap();
    assert!(!kinds(&rest).contains(&"tool_started"));
    assert_eq!(run.outcome().await.unwrap().text, "dann eben nicht");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(denial_seen_by_model(&provider));
}

#[tokio::test]
async fn complete_treats_ask_as_deny() {
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call("schreib", json!({}))
            .text("ok"),
    );
    let tool = TestTool::writer("schreib");
    let calls = tool.counter();
    let agent = Agent::builder()
        .provider_arc(provider.clone())
        .model("test")
        .tool(tool)
        .build()
        .unwrap();
    let outcome = agent.complete(SessionId::new(), "los").await.unwrap();
    assert_eq!(outcome.reason, FinishReason::Completed);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(denial_seen_by_model(&provider));
}

#[tokio::test]
async fn cancel_stops_a_hanging_tool() {
    let provider = FakeProvider::new().tool_call("haengt", json!({}));
    let agent = Agent::builder()
        .provider(provider)
        .model("test")
        .tool(TestTool::new("haengt", Effect::ReadOnly, Behavior::Hang))
        .build()
        .unwrap();
    let mut run = agent.run(SessionId::new(), "los").await.unwrap();
    loop {
        let envelope = run.next().await.unwrap().unwrap();
        if matches!(envelope.event, Event::ToolStarted { .. }) {
            break;
        }
    }
    run.cancel();
    assert_eq!(run.outcome().await.unwrap().reason, FinishReason::Cancelled);
}

#[tokio::test]
async fn cancel_while_waiting_for_approval() {
    let provider = FakeProvider::new().tool_call("schreib", json!({}));
    let agent = Agent::builder()
        .provider(provider)
        .model("test")
        .tool(TestTool::writer("schreib"))
        .build()
        .unwrap();
    let mut run = agent.run(SessionId::new(), "los").await.unwrap();
    loop {
        if matches!(
            run.next().await.unwrap().unwrap().event,
            Event::ApprovalNeeded { .. }
        ) {
            break;
        }
    }
    run.cancel_token().cancel();
    assert_eq!(run.outcome().await.unwrap().reason, FinishReason::Cancelled);
}

#[tokio::test]
async fn tool_timeout_becomes_an_error_result_without_real_waiting() {
    let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call("haengt", json!({}))
            .text("weiter"),
    );
    let agent = Agent::builder()
        .provider_arc(provider.clone())
        .model("test")
        .tool(TestTool::new("haengt", Effect::ReadOnly, Behavior::Hang))
        .clock(clock.clone())
        .build()
        .unwrap();
    let ticker = tick(clock);
    let outcome = agent.complete(SessionId::new(), "los").await.unwrap();
    ticker.abort();
    assert_eq!(outcome.text, "weiter");
    let request = provider.requests().pop().unwrap();
    let result = &request
        .messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Tool)
        .unwrap()
        .content[0];
    assert!(
        matches!(result, ContentBlock::ToolResult { output, is_error: true, .. }
        if output.as_text().contains("Zeitlimit von 60 s"))
    );
}

#[tokio::test]
async fn run_deadline_ends_the_run() {
    let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
    let agent = Agent::builder()
        .provider(FakeProvider::new().tool_call("haengt", json!({})))
        .model("test")
        .tool(TestTool::new("haengt", Effect::ReadOnly, Behavior::Hang))
        .limits(Limits {
            timeout: Duration::from_secs(10),
            ..Limits::default()
        })
        .clock(clock.clone())
        .build()
        .unwrap();
    let ticker = tick(clock);
    let outcome = agent.complete(SessionId::new(), "los").await.unwrap();
    ticker.abort();
    assert_eq!(
        outcome.reason,
        FinishReason::LimitReached {
            limit: LimitKind::Time
        }
    );
}

#[tokio::test]
async fn deadline_also_applies_while_waiting_for_approval() {
    let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
    let agent = Agent::builder()
        .provider(FakeProvider::new().tool_call("schreib", json!({})))
        .model("test")
        .tool(TestTool::writer("schreib"))
        .limits(Limits {
            timeout: Duration::from_secs(10),
            ..Limits::default()
        })
        .clock(clock.clone())
        .build()
        .unwrap();
    let mut run = agent.run(SessionId::new(), "los").await.unwrap();
    let ticker = tick(clock);
    let outcome = run.outcome().await.unwrap();
    ticker.abort();
    assert_eq!(
        outcome.reason,
        FinishReason::LimitReached {
            limit: LimitKind::Time
        }
    );
}
