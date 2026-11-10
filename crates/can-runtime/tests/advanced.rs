//! Lange Läufe, parallele Tools und Sub-Agenten.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use async_trait::async_trait;
use can_core::{
    AllowAll, CallId, Capabilities, Chunk, ContentBlock, Effect, Event, FinishReason, Limits,
    Provider, ProviderError, ProviderStream, Request, Role, SessionId, StopReason, Usage,
};
use can_providers::FakeProvider;
use can_runtime::{Agent, SubAgent, estimate_tokens};
use common::{Behavior, TestTool, drain};
use serde_json::json;

const WINDOW: u32 = 6_000;

/// Modell-Ersatz, das 200-mal ein Tool mit großem Argument aufruft und dann antwortet.
#[derive(Default)]
struct LongTask {
    turns: AtomicUsize,
    summaries: AtomicUsize,
    largest: AtomicU64,
}

#[async_trait]
impl Provider for LongTask {
    fn capabilities(&self, _model: &str) -> Capabilities {
        Capabilities {
            context_window: WINDOW,
            max_output: 500,
            ..FakeProvider::new().capabilities("m")
        }
    }

    async fn stream(&self, req: Request) -> Result<ProviderStream, ProviderError> {
        self.largest
            .fetch_max(estimate_tokens(&req), Ordering::SeqCst);
        let done = |stop| Chunk::Done {
            usage: Usage::default(),
            stop,
        };
        let chunks = if req
            .system
            .as_deref()
            .is_some_and(|system| system.starts_with("Du verdichtest"))
        {
            self.summaries.fetch_add(1, Ordering::SeqCst);
            vec![
                Chunk::TextDelta {
                    text: "Bisher lief alles nach Plan.".into(),
                },
                done(StopReason::EndTurn),
            ]
        } else {
            let turn = self.turns.fetch_add(1, Ordering::SeqCst);
            assert_eq!(
                req.messages[0].text(),
                "Arbeite 200 Schritte ab.",
                "die Aufgabe bleibt immer erhalten"
            );
            if turn < 200 {
                let args =
                    json!({"text": format!("Schritt {turn} {}", "x".repeat(1_500))}).to_string();
                vec![
                    Chunk::ToolCallStart {
                        index: 0,
                        id: CallId::from_parts(1, turn as u128),
                        name: "lies".into(),
                    },
                    Chunk::ToolCallArgs {
                        index: 0,
                        delta: args,
                    },
                    done(StopReason::ToolUse),
                ]
            } else {
                vec![
                    Chunk::TextDelta {
                        text: "Alle 200 Schritte erledigt.".into(),
                    },
                    done(StopReason::EndTurn),
                ]
            }
        };
        Ok(Box::pin(futures::stream::iter(chunks.into_iter().map(Ok))))
    }
}

/// Abnahme Phase 5: Ein Lauf mit 200 Schritten bleibt unter dem Kontextlimit.
#[tokio::test]
async fn two_hundred_steps_stay_below_the_context_limit() {
    let provider = Arc::new(LongTask::default());
    let agent = Agent::builder()
        .provider_arc(provider.clone())
        .model("test")
        .tool(TestTool::reader("lies"))
        .limits(Limits {
            max_steps: 250,
            ..Limits::default()
        })
        .build()
        .unwrap();
    let mut run = agent
        .run(SessionId::new(), "Arbeite 200 Schritte ab.")
        .await
        .unwrap();
    let events = drain(&mut run).await.unwrap();
    let outcome = run.outcome().await.unwrap();

    assert_eq!(
        (outcome.reason, outcome.steps),
        (FinishReason::Completed, 201)
    );
    assert_eq!(outcome.text, "Alle 200 Schritte erledigt.");
    let largest = provider.largest.load(Ordering::SeqCst);
    assert!(
        largest <= u64::from(WINDOW),
        "größte Anfrage: {largest} Token"
    );
    let compactions = events
        .iter()
        .filter(|e| matches!(e.event, Event::Compacted { .. }))
        .count();
    assert!(
        compactions >= 10 && compactions == provider.summaries.load(Ordering::SeqCst),
        "{compactions}"
    );
    // Ohne Verdichtung wären es rund 200 × 800 Token gewesen.
    assert!(
        events
            .iter()
            .filter(|e| matches!(e.event, Event::ToolFinished { .. }))
            .count()
            == 200
    );
}

async fn elapsed_for(tool: &str) -> std::time::Duration {
    let calls = vec![(tool, json!({})), (tool, json!({})), (tool, json!({}))];
    let agent = Agent::builder()
        .provider(FakeProvider::new().tool_calls(calls).text("fertig"))
        .model("test")
        .tool(TestTool::new("lies", Effect::ReadOnly, Behavior::Sleep))
        .tool(TestTool::new("schreib", Effect::Write, Behavior::Sleep))
        .policy(AllowAll)
        .build()
        .unwrap();
    let start = tokio::time::Instant::now();
    let outcome = agent.complete(SessionId::new(), "los").await.unwrap();
    assert_eq!(outcome.reason, FinishReason::Completed);
    start.elapsed()
}

/// Abnahme Phase 5: Drei parallele Tools sind messbar schneller als nacheinander.
#[tokio::test(start_paused = true)]
async fn three_read_only_tools_run_in_parallel_writes_in_sequence() {
    let parallel = elapsed_for("lies").await;
    let sequential = elapsed_for("schreib").await;
    assert!(parallel.as_millis() < 150, "parallel: {parallel:?}");
    assert!(
        sequential.as_millis() >= 300,
        "nacheinander: {sequential:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn at_most_four_tools_run_at_once() {
    let calls = (0..6).map(|_| ("lies", json!({}))).collect();
    let agent = Agent::builder()
        .provider(FakeProvider::new().tool_calls(calls).text("fertig"))
        .model("test")
        .tool(TestTool::new("lies", Effect::ReadOnly, Behavior::Sleep))
        .build()
        .unwrap();
    let start = tokio::time::Instant::now();
    agent.complete(SessionId::new(), "los").await.unwrap();
    let elapsed = start.elapsed().as_millis();
    assert!(
        (200..300).contains(&elapsed),
        "sechs Aufrufe in zwei Wellen: {elapsed} ms"
    );
}

fn tool_result(provider: &FakeProvider) -> (String, bool) {
    let request = provider.requests().pop().unwrap();
    let message = request
        .messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Tool)
        .unwrap();
    match &message.content[0] {
        ContentBlock::ToolResult {
            output, is_error, ..
        } => (output.as_text(), *is_error),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn sub_agent_is_a_tool_with_its_own_budget_and_policy() {
    // Der Experte ruft ein Schreib-Tool auf; ohne Mensch lehnt seine eigene Policy das ab.
    let expert_provider = Arc::new(
        FakeProvider::new()
            .tool_call("schreib", json!({}))
            .text("Die Antwort ist 42."),
    );
    let expert_tool = TestTool::writer("schreib");
    let expert_writes = expert_tool.counter();
    let expert = Agent::builder()
        .provider_arc(expert_provider.clone())
        .model("klein")
        .tool(expert_tool)
        .build()
        .unwrap();

    let provider = Arc::new(
        FakeProvider::new()
            .tool_call("experte", json!({"task": "Rechne es aus."}))
            .text("42."),
    );
    let agent = Agent::builder()
        .provider_arc(provider.clone())
        .model("gross")
        .tool(SubAgent::new("experte", "Fragt einen Experten.", expert))
        .policy(AllowAll)
        .build()
        .unwrap();
    let outcome = agent
        .complete(SessionId::new(), "Was ist die Antwort?")
        .await
        .unwrap();

    assert_eq!(outcome.text, "42.");
    assert_eq!(
        tool_result(&provider),
        ("Die Antwort ist 42.".to_owned(), false)
    );
    assert_eq!(
        expert_provider.requests()[0].messages[0].text(),
        "Rechne es aus."
    );
    assert_eq!(
        expert_writes.load(Ordering::SeqCst),
        0,
        "die Policy des Sub-Agenten gilt"
    );
    assert_eq!(
        outcome.steps, 2,
        "die Schritte des Sub-Agenten zählen nicht zum Budget des Aufrufers"
    );
}

#[tokio::test]
async fn sub_agent_that_exhausts_its_budget_reports_an_error() {
    let mut looping = FakeProvider::new();
    for _ in 0..3 {
        looping = looping.tool_call("lies", json!({}));
    }
    let expert = Agent::builder()
        .provider(looping)
        .model("klein")
        .tool(TestTool::reader("lies"))
        .limits(Limits {
            max_steps: 2,
            ..Limits::default()
        })
        .build()
        .unwrap();
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call("experte", json!({"task": "x"}))
            .text("schade"),
    );
    let agent = Agent::builder()
        .provider_arc(provider.clone())
        .model("gross")
        .tool(
            SubAgent::new("experte", "Fragt einen Experten.", expert).with_effect(Effect::ReadOnly),
        )
        .build()
        .unwrap();
    assert_eq!(
        agent.complete(SessionId::new(), "los").await.unwrap().text,
        "schade"
    );
    let (text, is_error) = tool_result(&provider);
    assert!(is_error && text.contains("LimitReached"), "{text}");
}
