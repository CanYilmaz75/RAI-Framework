//! Gemeinsame Test-Tools und Helfer für die Lauf-Tests.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agentkit_core::{
    BoxError, Effect, Envelope, ManualClock, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec,
};
use agentkit_runtime::{Run, RunError};
use async_trait::async_trait;
use serde_json::{Value, json};

/// Wie sich ein Test-Tool verhält.
#[derive(Clone, Copy)]
pub enum Behavior {
    Echo,
    ForModel,
    Fatal,
    Panic,
    Hang,
}

/// Tool mit einstellbarem Verhalten, das seine Aufrufe zählt.
pub struct TestTool {
    spec: ToolSpec,
    behavior: Behavior,
    pub calls: Arc<AtomicUsize>,
}

impl TestTool {
    pub fn new(name: &str, effect: Effect, behavior: Behavior) -> Self {
        let schema = json!({"type": "object", "properties": {"text": {"type": "string"}}});
        let spec = ToolSpec::new(name, "Test-Tool", schema)
            .with_effect(effect)
            .with_idempotent(effect == Effect::ReadOnly);
        Self {
            spec,
            behavior,
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn reader(name: &str) -> Self {
        Self::new(name, Effect::ReadOnly, Behavior::Echo)
    }

    pub fn writer(name: &str) -> Self {
        Self::new(name, Effect::Write, Behavior::Echo)
    }

    pub fn counter(&self) -> Arc<AtomicUsize> {
        self.calls.clone()
    }
}

#[async_trait]
impl Tool for TestTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, args: Value, _cx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.behavior {
            Behavior::Echo => Ok(ToolOutput::text(format!(
                "echo: {}",
                args["text"].as_str().unwrap_or("")
            ))),
            Behavior::ForModel => Err(ToolError::ForModel("Datei nicht gefunden".into())),
            Behavior::Fatal => Err(ToolError::Fatal(BoxError::from("Platte weg"))),
            Behavior::Panic => panic!("absichtlicher Panic im Test-Tool"),
            Behavior::Hang => std::future::pending().await,
        }
    }
}

/// Liest alle Events bis zum Ende des Laufs.
pub async fn drain(run: &mut Run) -> Result<Vec<Envelope>, RunError> {
    let mut events = Vec::new();
    while let Some(item) = run.next().await {
        events.push(item?);
    }
    Ok(events)
}

pub fn kinds(events: &[Envelope]) -> Vec<&'static str> {
    events
        .iter()
        .map(|envelope| envelope.event.kind())
        .collect()
}

/// Rückt die Uhr im Hintergrund immer weiter vor, bis das Handle abgebrochen wird.
pub fn tick(clock: Arc<ManualClock>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::task::yield_now().await;
            clock.advance(Duration::from_secs(1));
        }
    })
}
