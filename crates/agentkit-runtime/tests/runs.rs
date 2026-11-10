//! Ganze Läufe mit dem Fake-Provider: vom Start bis zum Ergebnis.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use agentkit_core::{
    AllowAll, ContentBlock, Effect, Event, FinishReason, LimitKind, Limits, MemoryStore, Message,
    Provider, ProviderError, Role, RunState, SessionId, Store,
};
use agentkit_providers::FakeProvider;
use agentkit_runtime::{Agent, RunError};
use common::{Behavior, TestTool, drain, kinds};
use serde_json::json;

/// Text des letzten Tool-Ergebnisses, das das Modell in seiner letzten Anfrage gesehen hat.
fn last_tool_result(provider: &FakeProvider) -> (String, bool) {
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
        other => panic!("kein Tool-Ergebnis: {other:?}"),
    }
}

/// Abnahme Phase 1: Modell → Tool-Aufruf → Tool-Ergebnis → Endantwort.
#[tokio::test]
async fn model_tool_result_answer() {
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call("lies", json!({"text": "README"}))
            .text("Die Datei sagt: echo."),
    );
    let tool = TestTool::reader("lies");
    let calls = tool.counter();
    let store = Arc::new(MemoryStore::new());
    let agent = Agent::builder()
        .provider_arc(provider.clone())
        .model("test")
        .system("Du hilfst.")
        .tool(tool)
        .store_arc(store.clone())
        .build()
        .unwrap();

    let session = SessionId::new();
    let mut run = agent.run(session, "Was steht in README?").await.unwrap();
    let events = drain(&mut run).await.unwrap();
    let outcome = run.outcome().await.unwrap();

    assert_eq!(
        kinds(&events),
        [
            "run_started",
            "model_requested",
            "model_responded",
            "tool_requested",
            "approval_resolved",
            "tool_started",
            "tool_finished",
            "model_requested",
            "text_delta",
            "model_responded",
            "run_finished",
        ]
    );
    assert_eq!(outcome.reason, FinishReason::Completed);
    assert_eq!(outcome.text, "Die Datei sagt: echo.");
    assert_eq!((outcome.steps, outcome.usage.total()), (2, 30));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        last_tool_result(&provider),
        ("echo: README".to_owned(), false)
    );

    // Der Store enthält genau die dauerhaften Events, lückenlos, und ergibt denselben Zustand.
    let log = store.load(session).await.unwrap();
    let durable: Vec<_> = events
        .into_iter()
        .filter(|e| e.event.is_durable())
        .collect();
    assert_eq!(log, durable);
    assert!(log.iter().enumerate().all(|(i, e)| e.seq == i as u64));
    let state = RunState::fold(agent.config().clone(), &log);
    assert_eq!(state.last_assistant_text(), "Die Datei sagt: echo.");
    assert_eq!(state.messages.len(), 4);
}

#[tokio::test]
async fn second_run_continues_the_session() {
    let provider = Arc::new(FakeProvider::new().text("eins").text("zwei"));
    let agent = Agent::builder()
        .provider_arc(provider.clone())
        .model("test")
        .build()
        .unwrap();
    let session = SessionId::new();
    assert_eq!(
        agent.complete(session, "erste Frage").await.unwrap().text,
        "eins"
    );
    assert_eq!(
        agent.complete(session, "zweite Frage").await.unwrap().text,
        "zwei"
    );
    let second = &provider.requests()[1];
    assert_eq!(
        second.messages,
        vec![
            Message::user("erste Frage"),
            Message::assistant("eins"),
            Message::user("zweite Frage")
        ]
    );
}

#[tokio::test]
async fn unknown_tool_and_tool_errors_go_back_to_the_model() {
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call("gibt_es_nicht", json!({}))
            .tool_call("kaputt", json!({}))
            .tool_call("panik", json!({}))
            .text("verstanden"),
    );
    let agent = Agent::builder()
        .provider_arc(provider.clone())
        .model("test")
        .tool(TestTool::new(
            "kaputt",
            Effect::ReadOnly,
            Behavior::ForModel,
        ))
        .tool(TestTool::new("panik", Effect::ReadOnly, Behavior::Panic))
        .build()
        .unwrap();
    let outcome = agent.complete(SessionId::new(), "los").await.unwrap();
    assert_eq!(outcome.reason, FinishReason::Completed);

    let results: Vec<(String, bool)> = provider.requests()[3]
        .messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .map(|m| match &m.content[0] {
            ContentBlock::ToolResult {
                output, is_error, ..
            } => (output.as_text(), *is_error),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        results[0],
        ("Unbekanntes Tool: gibt_es_nicht".to_owned(), true)
    );
    assert_eq!(results[1], ("Datei nicht gefunden".to_owned(), true));
    assert!(results[2].0.contains("abgestürzt") && results[2].1);
}

/// Ungültige Argumente vom Modell sind ein Tool-Fehler mit Feldpfad, kein Absturz.
#[tokio::test]
async fn invalid_arguments_are_reported_to_the_model_and_the_tool_never_runs() {
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call("lies", json!({"text": 42}))
            .turn(vec![
                agentkit_core::Chunk::ToolCallStart {
                    index: 0,
                    id: agentkit_core::CallId::from_parts(7, 7),
                    name: "lies".into(),
                },
                agentkit_core::Chunk::ToolCallArgs {
                    index: 0,
                    delta: "{kaputtes json".into(),
                },
                agentkit_core::Chunk::Done {
                    usage: agentkit_core::Usage::default(),
                    stop: agentkit_core::StopReason::ToolUse,
                },
            ])
            .text("ok"),
    );
    let tool = TestTool::reader("lies");
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
    let requests = provider.requests();
    let result =
        |request: &agentkit_core::Request| match &request.messages.last().unwrap().content[0] {
            ContentBlock::ToolResult {
                output,
                is_error: true,
                ..
            } => output.as_text(),
            other => panic!("{other:?}"),
        };
    assert_eq!(
        result(&requests[1]),
        "Ungültige Argumente: text: erwartet string, bekommen number"
    );
    assert_eq!(
        result(&requests[2]),
        "Ungültige Argumente: Argumente: erwartet object, bekommen string"
    );
}

#[tokio::test]
async fn fatal_tool_error_and_model_failure_end_the_run() {
    let provider = FakeProvider::new().tool_call("fatal", json!({}));
    let agent = Agent::builder()
        .provider(provider)
        .model("test")
        .tool(TestTool::new("fatal", Effect::ReadOnly, Behavior::Fatal))
        .build()
        .unwrap();
    let outcome = agent.complete(SessionId::new(), "los").await.unwrap();
    assert_eq!(
        outcome.reason,
        FinishReason::Error {
            message: "Platte weg".into()
        }
    );

    let provider = FakeProvider::new().fail(ProviderError::Auth);
    let agent = Agent::builder()
        .provider(provider)
        .model("test")
        .build()
        .unwrap();
    let outcome = agent.complete(SessionId::new(), "los").await.unwrap();
    assert_eq!(
        outcome.reason,
        FinishReason::Error {
            message: "Anmeldung fehlgeschlagen".into()
        }
    );
}

#[tokio::test]
async fn step_limit_stops_a_looping_model() {
    let mut provider = FakeProvider::new();
    for _ in 0..5 {
        provider = provider.tool_call("lies", json!({}));
    }
    let tool = TestTool::reader("lies");
    let calls = tool.counter();
    let agent = Agent::builder()
        .provider(provider)
        .model("test")
        .tool(tool)
        .limits(Limits {
            max_steps: 3,
            ..Limits::default()
        })
        .build()
        .unwrap();
    let outcome = agent.complete(SessionId::new(), "los").await.unwrap();
    assert_eq!(
        outcome.reason,
        FinishReason::LimitReached {
            limit: LimitKind::Steps
        }
    );
    assert_eq!((outcome.steps, calls.load(Ordering::SeqCst)), (3, 3));
}

#[tokio::test]
async fn several_calls_run_in_model_order() {
    let provider = Arc::new(
        FakeProvider::new()
            .tool_calls(vec![
                ("a", json!({"text": "1"})),
                ("b", json!({"text": "2"})),
            ])
            .text("fertig"),
    );
    let agent = Agent::builder()
        .provider_arc(provider.clone())
        .model("test")
        .tool(TestTool::reader("a"))
        .tool(TestTool::writer("b"))
        .policy(AllowAll)
        .build()
        .unwrap();
    let mut run = agent.run(SessionId::new(), "los").await.unwrap();
    let events = drain(&mut run).await.unwrap();
    let tool_events: Vec<&str> = kinds(&events)
        .into_iter()
        .filter(|kind| kind.starts_with("tool_"))
        .collect();
    assert_eq!(
        tool_events,
        [
            "tool_requested",
            "tool_requested",
            "tool_started",
            "tool_finished",
            "tool_started",
            "tool_finished"
        ]
    );
    let results = provider.requests()[1]
        .messages
        .last()
        .unwrap()
        .content
        .clone();
    let texts: Vec<String> = results
        .iter()
        .map(|block| match block {
            ContentBlock::ToolResult { output, .. } => output.as_text(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(texts, ["echo: 1", "echo: 2"]);
}

#[tokio::test]
async fn interrupted_session_must_be_resumed_not_restarted() {
    let store = Arc::new(MemoryStore::new());
    let session = SessionId::new();
    let build = |provider: FakeProvider| {
        Agent::builder()
            .provider(provider)
            .model("test")
            .tool(TestTool::writer("schreib"))
            .store_arc(store.clone())
            .build()
            .unwrap()
    };
    // Der Lauf bleibt an der Freigabe hängen; der Prozess „stürzt ab“, ohne ihn zu beenden.
    let agent = build(FakeProvider::new().tool_call("schreib", json!({})));
    let mut run = agent.run(session, "los").await.unwrap();
    loop {
        let envelope = run.next().await.unwrap().unwrap();
        if matches!(envelope.event, Event::ApprovalNeeded { .. }) {
            break;
        }
    }
    // `forget` statt `drop`: ein verworfenes Handle würde den Lauf sauber abbrechen.
    std::mem::forget(run);

    let agent = build(FakeProvider::new());
    assert!(matches!(
        agent.run(session, "nochmal").await,
        Err(RunError::Interrupted)
    ));
    assert!(matches!(
        agent.resume(SessionId::new()).await,
        Err(RunError::NothingToResume)
    ));
}

#[test]
fn build_collects_all_problems() {
    let mut no_tools = FakeProvider::new().capabilities("m");
    no_tools.tools = false;
    let error = Agent::builder()
        .provider(FakeProvider::new().with_capabilities(no_tools))
        .tool(TestTool::reader("a"))
        .tool(TestTool::reader("a"))
        .limits(Limits {
            max_steps: 0,
            ..Limits::default()
        })
        .workdir("/gibt/es/nicht")
        .build()
        .err()
        .unwrap();
    assert_eq!(error.problems.len(), 5, "{error}");
    assert!(error.to_string().contains("Tool-Name doppelt vergeben: a"));

    let error = Agent::builder().model("m").build().err().unwrap();
    assert_eq!(error.problems, vec!["kein Provider angegeben".to_owned()]);
}
