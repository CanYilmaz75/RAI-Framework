//! Sub-Agenten sind Tools: ein Agent mit eigenem Budget und eigener Policy hinter dem `Tool`-Trait.

use can_core::{Effect, FinishReason, SessionId, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec};
use serde_json::{Value, json};

use crate::agent::Agent;

/// Reicht eine Aufgabe an einen anderen Agenten weiter und liefert dessen Endantwort.
///
/// Der Sub-Agent läuft ohne Mensch: Rückfragen seiner Policy werden zu Ablehnungen. Seine
/// Limits sind sein eigenes Budget, unabhängig vom aufrufenden Lauf.
pub struct SubAgent {
    spec: ToolSpec,
    agent: Agent,
}

impl SubAgent {
    /// Tool mit dem Argument `task`. Wirkung zunächst `External`, also mit Rückfrage.
    pub fn new(name: impl Into<String>, description: impl Into<String>, agent: Agent) -> Self {
        let schema = json!({
            "type": "object",
            "properties": {"task": {"type": "string", "description": "Vollständige Aufgabe für den Sub-Agenten"}},
            "required": ["task"],
            "additionalProperties": false
        });
        let timeout = agent.config().limits.timeout;
        let spec = ToolSpec::new(name, description, schema)
            .with_effect(Effect::External)
            .with_timeout(timeout);
        Self { spec, agent }
    }

    /// Setzt die Wirkungsklasse, etwa `ReadOnly` für einen Sub-Agenten, der nur liest.
    pub fn with_effect(mut self, effect: Effect) -> Self {
        self.spec = self.spec.with_effect(effect);
        self
    }
}

#[async_trait::async_trait]
impl Tool for SubAgent {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, args: Value, cx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let task = args["task"]
            .as_str()
            .ok_or_else(|| ToolError::ForModel("Argument `task` fehlt.".into()))?;
        let failed = |error: &dyn std::fmt::Display| {
            ToolError::ForModel(format!("Sub-Agent fehlgeschlagen: {error}"))
        };
        let mut run = self
            .agent
            .run_unattended(SessionId::new(), task)
            .await
            .map_err(|error| failed(&error))?;
        let cancel = run.cancel_token();
        let outcome = tokio::select! {
            outcome = run.outcome() => outcome,
            _ = cx.cancel.cancelled() => {
                cancel.cancel();
                return Err(ToolError::ForModel("Sub-Agent abgebrochen.".into()));
            }
        };
        let outcome = outcome.map_err(|error| failed(&error))?;
        match outcome.reason {
            FinishReason::Completed => Ok(ToolOutput::text(outcome.text)),
            reason => Err(ToolError::ForModel(format!(
                "Sub-Agent endete ohne Ergebnis: {reason:?}"
            ))),
        }
    }
}
