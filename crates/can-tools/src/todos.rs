//! Planungs-Tool `write_todos`: der Agent hält seine Aufgaben und ihren Stand fest.
//!
//! Das Tool selbst bestätigt nur. Die Laufzeit macht aus seinem Argument das Event
//! `PlanUpdated`; so steht der Plan im Log und im Zustand der Sitzung.

use can_core::{
    Effect, PLAN_TOOL, Todo, TodoStatus, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec,
};
use serde::Deserialize;
use serde_json::{Value, json};

/// Schreibt den Plan der Sitzung neu.
pub struct WriteTodos {
    spec: ToolSpec,
}

#[derive(Deserialize)]
struct Args {
    todos: Vec<Todo>,
}

impl WriteTodos {
    /// Das Tool mit seiner festen Spec: es ändert nichts außerhalb der Sitzung.
    pub fn new() -> Self {
        let schema = json!({
            "type": "object",
            "properties": {"todos": {
                "type": "array",
                "description": "Der vollständige Plan; er ersetzt den bisherigen.",
                "items": {
                    "type": "object",
                    "properties": {
                        "content": {"type": "string", "description": "Was zu tun ist"},
                        "status": {"enum": ["pending", "in_progress", "completed"]}
                    },
                    "required": ["content", "status"],
                    "additionalProperties": false
                }
            }},
            "required": ["todos"],
            "additionalProperties": false
        });
        let description = "Hält den Plan für eine mehrstufige Aufgabe fest und aktualisiert ihn. Immer die \
                           ganze Liste senden. Höchstens eine Aufgabe sollte in Arbeit sein.";
        let spec = ToolSpec::new(PLAN_TOOL, description, schema)
            .with_effect(Effect::ReadOnly)
            .with_idempotent(true);
        Self { spec }
    }
}

impl Default for WriteTodos {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for WriteTodos {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, args: Value, _cx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let args: Args = serde_json::from_value(args)
            .map_err(|error| ToolError::ForModel(format!("Ungültige Argumente: {error}")))?;
        let count = |status| {
            args.todos
                .iter()
                .filter(|todo| todo.status == status)
                .count()
        };
        Ok(ToolOutput::text(format!(
            "Plan gespeichert: {} offen, {} in Arbeit, {} erledigt.",
            count(TodoStatus::Pending),
            count(TodoStatus::InProgress),
            count(TodoStatus::Completed)
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::TempDir;
    use can_core::schema::validate;

    #[tokio::test]
    async fn confirms_the_plan_and_rejects_bad_statuses() {
        let (tool, dir) = (WriteTodos::default(), TempDir::new());
        assert_eq!(
            (tool.spec().name.as_str(), tool.spec().effect),
            ("write_todos", Effect::ReadOnly)
        );
        let plan = json!({"todos": [
            {"content": "Datei lesen", "status": "completed"},
            {"content": "Fehler beheben", "status": "in_progress"},
            {"content": "Tests schreiben", "status": "pending"},
            {"content": "Bericht", "status": "pending"}
        ]});
        assert_eq!(validate(&tool.spec().input_schema.0, &plan), Ok(()));
        let output = tool.call(plan, &dir.ctx()).await.unwrap();
        assert_eq!(
            output.as_text(),
            "Plan gespeichert: 2 offen, 1 in Arbeit, 1 erledigt."
        );
        let bad = json!({"todos": [{"content": "x", "status": "fast fertig"}]});
        assert!(validate(&tool.spec().input_schema.0, &bad).is_err());
        assert!(matches!(
            tool.call(bad, &dir.ctx()).await,
            Err(ToolError::ForModel(_))
        ));
    }
}
