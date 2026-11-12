//! Gedächtnis-Tools: `remember`, `recall` und `forget` über eine [`MemoryBank`].
//!
//! Der Namensraum kommt aus dem Lauf (`ToolCtx::namespace`), nicht vom Modell: ein Agent sieht
//! nur das Gedächtnis seines Mandanten.

use std::sync::Arc;

use can_core::{Effect, MemoryBank, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec};
use serde_json::{Value, json};

/// So viele Treffer liefert `recall` ohne Angabe.
const DEFAULT_LIMIT: u64 = 5;
/// Höchstens so viele Treffer pro Suche.
const MAX_LIMIT: u64 = 20;
/// Längster speicherbarer Eintrag in Zeichen.
const MAX_TEXT: usize = 4_000;

#[derive(Clone, Copy)]
enum Action {
    Remember,
    Recall,
    Forget,
}

/// Eines der drei Gedächtnis-Tools.
pub struct MemoryTool {
    spec: ToolSpec,
    bank: Arc<dyn MemoryBank>,
    action: Action,
}

/// Die drei Tools über derselben Ablage.
pub fn memory_tools(bank: Arc<dyn MemoryBank>) -> Vec<Arc<dyn Tool>> {
    [Action::Remember, Action::Recall, Action::Forget]
        .into_iter()
        .map(|action| Arc::new(MemoryTool::new(bank.clone(), action)) as Arc<dyn Tool>)
        .collect()
}

impl MemoryTool {
    fn new(bank: Arc<dyn MemoryBank>, action: Action) -> Self {
        let spec = match action {
            Action::Remember => ToolSpec::new(
                "remember",
                "Merkt sich eine Tatsache dauerhaft, über diese Sitzung hinaus. Ein Eintrag \
                 pro Tatsache, vollständig und ohne Bezug auf den Gesprächsverlauf formuliert.",
                json!({
                    "type": "object",
                    "properties": {"text": {"type": "string", "description": "Die Tatsache"}},
                    "required": ["text"],
                    "additionalProperties": false
                }),
            )
            .with_effect(Effect::Write),
            Action::Recall => ToolSpec::new(
                "recall",
                "Durchsucht das dauerhafte Gedächtnis nach Stichworten und liefert die \
                 passendsten Einträge mit Kennung.",
                json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Stichworte"},
                        "limit": {"type": "integer", "minimum": 1, "maximum": MAX_LIMIT}
                    },
                    "required": ["query"],
                    "additionalProperties": false
                }),
            )
            .with_effect(Effect::ReadOnly)
            .with_idempotent(true),
            Action::Forget => ToolSpec::new(
                "forget",
                "Löscht einen Eintrag aus dem dauerhaften Gedächtnis, etwa wenn er nicht mehr stimmt.",
                json!({
                    "type": "object",
                    "properties": {"id": {"type": "string", "description": "Kennung aus `recall`"}},
                    "required": ["id"],
                    "additionalProperties": false
                }),
            )
            .with_effect(Effect::Write)
            .with_idempotent(true),
        };
        Self { spec, bank, action }
    }
}

fn failed(error: impl std::fmt::Display) -> ToolError {
    ToolError::ForModel(format!("Gedächtnis nicht erreichbar: {error}"))
}

#[async_trait::async_trait]
impl Tool for MemoryTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, args: Value, cx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let namespace = cx.namespace.as_str();
        let text = match self.action {
            Action::Remember => {
                let text = args["text"].as_str().unwrap_or_default().trim();
                if text.is_empty() {
                    return Err(ToolError::ForModel("`text` ist leer.".into()));
                }
                if text.chars().count() > MAX_TEXT {
                    return Err(ToolError::ForModel(format!(
                        "Eintrag zu lang; höchstens {MAX_TEXT} Zeichen."
                    )));
                }
                let id = self
                    .bank
                    .add(namespace, text, cx.clock.now())
                    .await
                    .map_err(failed)?;
                format!("Gemerkt als {id}.")
            }
            Action::Recall => {
                let query = args["query"].as_str().unwrap_or_default();
                let limit = args["limit"]
                    .as_u64()
                    .unwrap_or(DEFAULT_LIMIT)
                    .clamp(1, MAX_LIMIT);
                let limit = usize::try_from(limit).unwrap_or(1);
                let found = self
                    .bank
                    .search(namespace, query, limit)
                    .await
                    .map_err(failed)?;
                if found.is_empty() {
                    "Nichts gefunden.".to_owned()
                } else {
                    found
                        .iter()
                        .map(|memory| format!("[{}] {}", memory.id, memory.text))
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            }
            Action::Forget => {
                let id = args["id"].as_str().unwrap_or_default();
                if self.bank.delete(namespace, id).await.map_err(failed)? {
                    format!("{id} vergessen.")
                } else {
                    format!("Keinen Eintrag {id} gefunden.")
                }
            }
        };
        Ok(ToolOutput::text(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::TempDir;
    use can_core::InMemoryBank;

    fn text(output: ToolOutput) -> String {
        output.as_text()
    }

    #[tokio::test]
    async fn remember_recall_forget_roundtrip() {
        let dir = TempDir::new();
        let bank: Arc<dyn MemoryBank> = Arc::new(InMemoryBank::new());
        let tools = memory_tools(bank.clone());
        let names: Vec<&str> = tools.iter().map(|t| t.spec().name.as_str()).collect();
        assert_eq!(names, ["remember", "recall", "forget"]);
        let cx = dir.ctx();
        let saved = text(
            tools[0]
                .call(json!({"text": "Kunde Müller zahlt per Rechnung"}), &cx)
                .await
                .unwrap(),
        );
        assert!(saved.starts_with("Gemerkt als"));
        let found = text(
            tools[1]
                .call(json!({"query": "Wie zahlt Müller?"}), &cx)
                .await
                .unwrap(),
        );
        assert!(found.contains("Kunde Müller zahlt per Rechnung"), "{found}");
        let id = bank.all(&cx.namespace).await.unwrap()[0].id.clone();
        let gone = text(tools[2].call(json!({"id": id}), &cx).await.unwrap());
        assert!(gone.ends_with("vergessen."));
        let found = text(
            tools[1]
                .call(json!({"query": "Müller"}), &cx)
                .await
                .unwrap(),
        );
        assert_eq!(found, "Nichts gefunden.");
        let missing = text(tools[2].call(json!({"id": "x"}), &cx).await.unwrap());
        assert!(missing.starts_with("Keinen Eintrag"));
    }

    #[tokio::test]
    async fn namespaces_are_separate_and_bad_input_is_rejected() {
        let dir = TempDir::new();
        let bank: Arc<dyn MemoryBank> = Arc::new(InMemoryBank::new());
        let tools = memory_tools(bank);
        let mut cx = dir.ctx();
        cx.namespace = "mandant-a".into();
        tools[0]
            .call(json!({"text": "Geheimprojekt Adler"}), &cx)
            .await
            .unwrap();
        cx.namespace = "mandant-b".into();
        let found = text(tools[1].call(json!({"query": "Adler"}), &cx).await.unwrap());
        assert_eq!(found, "Nichts gefunden.");
        assert!(tools[0].call(json!({"text": "  "}), &cx).await.is_err());
        let long = "x".repeat(MAX_TEXT + 1);
        assert!(tools[0].call(json!({"text": long}), &cx).await.is_err());
    }
}
