//! Schwärzung von Secrets und Neustart nach einem mittendrin abgebrochenen Antwort-Strom.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use can_core::{
    Capabilities, Chunk, Effect, Event, FinishReason, MemoryStore, Provider, ProviderError,
    ProviderStream, Request, Secret, SecretsHandle, SessionId, StopReason, Store, Tool, ToolCtx,
    ToolError, ToolOutput, ToolSpec, Usage,
};
use can_providers::FakeProvider;
use can_runtime::Agent;
use common::drain;
use serde_json::{Value, json};

/// Tool, das einen freigegebenen Schlüssel ausplaudert.
struct Leaky(ToolSpec);

#[async_trait]
impl Tool for Leaky {
    fn spec(&self) -> &ToolSpec {
        &self.0
    }

    async fn call(&self, _args: Value, cx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text(format!(
            "Der Schlüssel lautet {}",
            cx.secrets.get("API_KEY").unwrap_or("?")
        )))
    }
}

#[tokio::test]
async fn secrets_never_reach_the_log_the_channel_or_the_model() {
    let secret = "sk-ganz-geheim-4711";
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call("plaudern", json!({}))
            .text(&format!("Ich wiederhole: {secret}")),
    );
    let store = Arc::new(MemoryStore::new());
    let spec = ToolSpec::new("plaudern", "plaudert", json!({"type": "object"}))
        .with_effect(Effect::ReadOnly);
    let agent = Agent::builder()
        .provider_arc(provider.clone())
        .model("test")
        .tool(Leaky(spec))
        .secrets(SecretsHandle::from_pairs([(
            "API_KEY",
            Secret::new(secret.to_owned()),
        )]))
        .store_arc(store.clone())
        .build()
        .unwrap();
    let session = SessionId::new();
    let mut run = agent.run(session, "los").await.unwrap();
    let events = drain(&mut run).await.unwrap();
    let outcome = run.outcome().await.unwrap();

    let seen = serde_json::to_string(&events).unwrap();
    let stored = serde_json::to_string(&store.load(session).await.unwrap()).unwrap();
    let sent = serde_json::to_string(&provider.requests()).unwrap();
    for (place, text) in [
        ("Kanal", &seen),
        ("Store", &stored),
        ("Modell", &sent),
        ("Ergebnis", &outcome.text),
    ] {
        assert!(!text.contains(secret), "{place} enthält das Secret");
    }
    assert!(stored.contains("Der Schlüssel lautet [geschwärzt]"));
    assert_eq!(outcome.text, "Ich wiederhole: [geschwärzt]");
}

/// Liefert bei den ersten Versuchen einen Teil der Antwort und bricht dann ab.
struct Flaky {
    failures: usize,
    attempts: AtomicUsize,
    transient: bool,
}

#[async_trait]
impl Provider for Flaky {
    fn capabilities(&self, model: &str) -> Capabilities {
        FakeProvider::new().capabilities(model)
    }

    async fn stream(&self, _req: Request) -> Result<ProviderStream, ProviderError> {
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
        let mut items = vec![Ok(Chunk::TextDelta {
            text: "Die Antwort ".into(),
        })];
        if attempt < self.failures {
            let error = if self.transient {
                ProviderError::Network("Verbindung weg".into())
            } else {
                ProviderError::Protocol("kaputt".into())
            };
            items.push(Err(error));
        } else {
            items.push(Ok(Chunk::TextDelta {
                text: "ist 42.".into(),
            }));
            items.push(Ok(Chunk::Done {
                usage: Usage::default(),
                stop: StopReason::EndTurn,
            }));
        }
        Ok(Box::pin(futures::stream::iter(items)))
    }
}

async fn run_flaky(failures: usize, transient: bool) -> (FinishReason, String, usize, usize) {
    let provider = Arc::new(Flaky {
        failures,
        attempts: AtomicUsize::new(0),
        transient,
    });
    let agent = Agent::builder()
        .provider_arc(provider.clone())
        .model("test")
        .build()
        .unwrap();
    let mut run = agent.run(SessionId::new(), "los").await.unwrap();
    let events = drain(&mut run).await.unwrap();
    let outcome = run.outcome().await.unwrap();
    let responses = events
        .iter()
        .filter(|e| matches!(e.event, Event::ModelResponded { .. }))
        .count();
    (
        outcome.reason,
        outcome.text,
        provider.attempts.load(Ordering::SeqCst),
        responses,
    )
}

#[tokio::test]
async fn interrupted_stream_discards_the_partial_answer_and_restarts_the_step() {
    let (reason, text, attempts, responses) = run_flaky(2, true).await;
    assert_eq!(reason, FinishReason::Completed);
    assert_eq!(
        text, "Die Antwort ist 42.",
        "die Teilantwort steht nicht doppelt im Verlauf"
    );
    assert_eq!((attempts, responses), (3, 1));
}

#[tokio::test]
async fn restarts_are_bounded_and_only_for_transient_errors() {
    let (reason, _, attempts, _) = run_flaky(5, true).await;
    assert!(matches!(reason, FinishReason::Error { .. }));
    assert_eq!(attempts, 3, "ein Versuch plus zwei Neustarts");

    let (reason, _, attempts, _) = run_flaky(1, false).await;
    assert!(matches!(reason, FinishReason::Error { message } if message.contains("kaputt")));
    assert_eq!(attempts, 1);
}

/// Tool mit sehr großer Ausgabe.
struct Verbose(ToolSpec);

#[async_trait]
impl Tool for Verbose {
    fn spec(&self) -> &ToolSpec {
        &self.0
    }

    async fn call(&self, _args: Value, _cx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text(format!(
            "ANFANG{}ENDE",
            "x".repeat(200_000)
        )))
    }
}

#[tokio::test]
async fn large_tool_output_is_shortened_for_the_model_and_kept_whole_as_blob() {
    let provider = Arc::new(FakeProvider::new().tool_call("viel", json!({})).text("ok"));
    let store = Arc::new(MemoryStore::new());
    let spec =
        ToolSpec::new("viel", "viel Text", json!({"type": "object"})).with_effect(Effect::ReadOnly);
    let agent = Agent::builder()
        .provider_arc(provider.clone())
        .model("test")
        .tool(Verbose(spec))
        .store_arc(store.clone())
        .build()
        .unwrap();
    let session = SessionId::new();
    agent.complete(session, "los").await.unwrap();

    let log = store.load(session).await.unwrap();
    let output = log
        .iter()
        .find_map(|envelope| match &envelope.event {
            Event::ToolFinished { output, .. } => Some(output.as_text()),
            _ => None,
        })
        .unwrap();
    assert!(output.len() < 70_000, "{}", output.len());
    assert!(output.starts_with("ANFANG") && output.contains("ENDE\n[Volle Ausgabe: 200010 Bytes"));
    let digest = output.rsplit("Blob ").next().unwrap().trim_end_matches(']');
    let blob = store
        .get_blob(&can_core::Digest(digest.to_owned()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(blob.len(), 200_010);
}

#[tokio::test]
async fn a_second_writer_cannot_take_over_a_live_session() {
    use can_core::{Decision, StoreError};
    use can_runtime::RunError;

    let store = Arc::new(MemoryStore::new());
    let session = SessionId::new();
    let build = |provider: FakeProvider, owner: &str| {
        let spec = ToolSpec::new("schreib", "schreibt", json!({"type": "object"}));
        Agent::builder()
            .provider(provider)
            .model("test")
            .tool(Verbose(spec))
            .store_arc(store.clone())
            .owner(owner)
            .build()
            .unwrap()
    };
    // Worker A bleibt an der Rückfrage stehen und hält die Sperre.
    let first = build(
        FakeProvider::new()
            .tool_call("schreib", json!({}))
            .text("fertig"),
        "worker-a",
    );
    let mut run = first.run(session, "los").await.unwrap();
    let call_id = loop {
        if let Event::ApprovalNeeded { call_id } = run.next().await.unwrap().unwrap().event {
            break call_id;
        }
    };

    let second = build(FakeProvider::new(), "worker-b");
    let stolen = second.resume(session).await.unwrap().outcome().await;
    assert!(
        matches!(stolen, Err(RunError::Store(StoreError::Locked { owner, .. })) if owner == "worker-a")
    );

    // A arbeitet ungestört weiter; danach ist die Sitzung frei.
    run.resolve(call_id, Decision::Allow).await.unwrap();
    assert_eq!(run.outcome().await.unwrap().reason, FinishReason::Completed);
    let third = build(FakeProvider::new().text("weiter"), "worker-b");
    assert_eq!(
        third.complete(session, "und jetzt?").await.unwrap().text,
        "weiter"
    );
}
