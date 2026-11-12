//! Der Namensraum eines Laufs erreicht die Tools, auch die von Sub-Agenten.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use can_core::{AllowAll, Effect, SessionId, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec};
use can_providers::FakeProvider;
use can_runtime::{Agent, SubAgent};
use serde_json::{Value, json};

/// Merkt sich, in welchem Namensraum es aufgerufen wurde.
struct Witness {
    spec: ToolSpec,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Witness {
    fn new(seen: Arc<Mutex<Vec<String>>>) -> Self {
        let spec = ToolSpec::new(
            "zeuge",
            "merkt sich den Namensraum",
            json!({"type": "object"}),
        )
        .with_effect(Effect::ReadOnly);
        Self { spec, seen }
    }
}

#[async_trait::async_trait]
impl Tool for Witness {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, _args: Value, cx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        self.seen.lock().unwrap().push(cx.namespace.clone());
        Ok(ToolOutput::text("ok"))
    }
}

fn agent(provider: FakeProvider, tool: impl Tool + 'static) -> Agent {
    Agent::builder()
        .provider(provider)
        .model("test")
        .policy(AllowAll)
        .tool(tool)
        .build()
        .unwrap()
}

#[tokio::test]
async fn runs_carry_their_namespace_into_tools() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let script = || {
        FakeProvider::new()
            .tool_call("zeuge", json!({}))
            .text("fertig")
    };
    let scoped = agent(script(), Witness::new(seen.clone()));
    scoped
        .run_in("acme", SessionId::new(), "los")
        .await
        .unwrap()
        .outcome()
        .await
        .unwrap();
    let plain = agent(script(), Witness::new(seen.clone()));
    plain.complete(SessionId::new(), "los").await.unwrap();
    assert_eq!(*seen.lock().unwrap(), ["acme", can_core::DEFAULT_NAMESPACE]);
}

#[tokio::test]
async fn sub_agents_inherit_the_namespace() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let helper = agent(
        FakeProvider::new()
            .tool_call("zeuge", json!({}))
            .text("erledigt"),
        Witness::new(seen.clone()),
    );
    let lead = agent(
        FakeProvider::new()
            .tool_call("helfer", json!({"task": "prüfen"}))
            .text("alles gut"),
        SubAgent::new("helfer", "hilft", helper),
    );
    let mut run = lead
        .run_in("globex", SessionId::new(), "los")
        .await
        .unwrap();
    assert_eq!(run.outcome().await.unwrap().text, "alles gut");
    assert_eq!(*seen.lock().unwrap(), ["globex"]);
}
