//! Getrennte Arbeitsverzeichnisse, Schutz vor Datenabfluss und Inhaltsfilter.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use can_core::{
    AllowAll, BlockList, ContentBlock, Decision, Effect, Event, FinishReason, Role, SessionId,
    Tool, ToolCtx, ToolError, ToolOutput, ToolSpec,
};
use can_providers::FakeProvider;
use can_runtime::{Agent, RunError};
use common::{Behavior, TestTool, drain};
use serde_json::{Value, json};

/// Schreibt eine Datei ins Arbeitsverzeichnis und meldet dessen Pfad.
struct Touch(ToolSpec);

#[async_trait]
impl Tool for Touch {
    fn spec(&self) -> &ToolSpec {
        &self.0
    }

    async fn call(&self, _args: Value, cx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        std::fs::write(cx.workdir.join("spur.txt"), "x")
            .map_err(|error| ToolError::ForModel(error.to_string()))?;
        Ok(ToolOutput::text(cx.workdir.display().to_string()))
    }
}

#[tokio::test]
async fn every_session_gets_its_own_workdir() {
    let base = std::env::temp_dir().join(format!("can-isolation-{}", SessionId::new()));
    std::fs::create_dir_all(&base).unwrap();
    let spec = ToolSpec::new("spur", "hinterlässt eine Spur", json!({"type": "object"}))
        .with_effect(Effect::ReadOnly);
    let provider = FakeProvider::new()
        .tool_call("spur", json!({}))
        .text("a")
        .tool_call("spur", json!({}))
        .text("b");
    let agent = Agent::builder()
        .provider(provider)
        .model("test")
        .tool(Touch(spec))
        .workdir(&base)
        .isolate_workdir(true)
        .build()
        .unwrap();
    let (first, second) = (SessionId::new(), SessionId::new());
    agent.complete(first, "los").await.unwrap();
    agent.complete(second, "los").await.unwrap();
    let base = base.canonicalize().unwrap();
    for session in [first, second] {
        assert!(base.join(session.to_string()).join("spur.txt").exists());
    }
    assert!(
        !base.join("spur.txt").exists(),
        "nichts landet im gemeinsamen Verzeichnis"
    );
    std::fs::remove_dir_all(base).unwrap();
}

fn guarded(
    provider: Arc<FakeProvider>,
    guard: Option<Decision>,
) -> (Agent, Arc<std::sync::atomic::AtomicUsize>) {
    let akte = ToolSpec::new("akte", "liest eine Personalakte", json!({"type": "object"}))
        .with_effect(Effect::ReadOnly)
        .with_confidential(true);
    let senden = TestTool::new("senden", Effect::External, Behavior::Echo);
    let sent = senden.counter();
    let mut builder = Agent::builder()
        .provider_arc(provider)
        .model("test")
        .tool(Touch(akte))
        .tool(senden)
        .tool(TestTool::reader("lies"))
        .policy(AllowAll)
        .workdir(std::env::temp_dir());
    if let Some(guard) = guard {
        builder = builder.exfiltration_guard(guard);
    }
    (builder.build().unwrap(), sent)
}

fn last_result(provider: &FakeProvider) -> (String, bool) {
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
async fn after_confidential_data_external_tools_are_blocked() {
    // Das Modell sendet erst etwas (erlaubt), liest dann die Akte und will wieder senden.
    let script = || {
        Arc::new(
            FakeProvider::new()
                .tool_call("senden", json!({"text": "harmlos"}))
                .tool_call("akte", json!({}))
                .tool_call("lies", json!({}))
                .tool_call("senden", json!({"text": "Inhalt der Akte"}))
                .text("fertig"),
        )
    };
    let provider = script();
    let (agent, sent) = guarded(provider.clone(), Some(Decision::Deny));
    let outcome = agent.complete(SessionId::new(), "los").await.unwrap();
    assert_eq!(outcome.reason, FinishReason::Completed);
    assert_eq!(
        sent.load(Ordering::SeqCst),
        1,
        "nur der Aufruf vor der Akte ging hinaus"
    );
    let (text, is_error) = last_result(&provider);
    assert!(is_error && text.contains("abgelehnt"), "{text}");

    // Ohne Schutz gehen beide Aufrufe hinaus.
    let provider = script();
    let (agent, sent) = guarded(provider, None);
    agent.complete(SessionId::new(), "los").await.unwrap();
    assert_eq!(sent.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn guard_with_ask_turns_into_a_question_and_survives_the_next_run() {
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call("akte", json!({}))
            .text("gelesen")
            .tool_call("senden", json!({}))
            .text("gesendet"),
    );
    let (agent, sent) = guarded(provider, Some(Decision::Ask));
    let session = SessionId::new();
    agent.complete(session, "lies").await.unwrap();
    // Zweiter Lauf derselben Sitzung: die vertraulichen Daten stehen noch im Verlauf.
    let mut run = agent.run(session, "sende").await.unwrap();
    let mut asked = false;
    while let Some(envelope) = run.next().await {
        if let Event::ApprovalNeeded { call_id } = envelope.unwrap().event {
            asked = true;
            run.resolve(call_id, Decision::Allow).await.unwrap();
        }
    }
    assert!(asked, "trotz AllowAll wird nachgefragt");
    assert_eq!(sent.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn content_filter_blocks_inputs_and_model_answers() {
    let build = |provider: FakeProvider| {
        Agent::builder()
            .provider(provider)
            .model("test")
            .moderator(BlockList::new(["Projekt Zeus"]))
            .build()
            .unwrap()
    };
    let agent = build(FakeProvider::new().text("ok"));
    let blocked = agent
        .run(SessionId::new(), "Erzähl mir von projekt zeus")
        .await;
    assert!(matches!(blocked, Err(RunError::Blocked(reason)) if reason.contains("projekt zeus")));
    assert_eq!(
        agent
            .complete(SessionId::new(), "Hallo")
            .await
            .unwrap()
            .text,
        "ok"
    );

    let agent = build(FakeProvider::new().text("Über Projekt Zeus weiß ich alles."));
    let mut run = agent.run(SessionId::new(), "Hallo").await.unwrap();
    let events = drain(&mut run).await.unwrap();
    let outcome = run.outcome().await.unwrap();
    assert!(
        matches!(outcome.reason, FinishReason::Error { message } if message.contains("Inhaltsfilter"))
    );
    let stored = serde_json::to_string(
        &events
            .iter()
            .filter(|e| e.event.is_durable())
            .collect::<Vec<_>>(),
    )
    .unwrap();
    assert!(
        !stored.contains("weiß ich alles"),
        "die blockierte Antwort steht nicht im Log"
    );
}
