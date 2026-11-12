//! Strukturierte Ausgabe: die Endantwort als JSON, geprüft gegen ein Schema.

use can_core::{FinishReason, SessionId, schema::validate};
use serde_json::Value;

use crate::agent::Agent;
use crate::run::RunError;

/// So oft bekommt das Modell seinen Fehler zurück, bevor aufgegeben wird.
const MAX_CORRECTIONS: usize = 2;

/// Holt das JSON aus der Antwort; ein umschließender Codeblock wird entfernt.
fn parse(text: &str) -> Result<Value, String> {
    let trimmed = text.trim();
    let inner = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|rest| rest.strip_suffix("```"))
        .unwrap_or(trimmed);
    serde_json::from_str(inner.trim()).map_err(|error| format!("kein gültiges JSON: {error}"))
}

impl Agent {
    /// Wie [`Agent::complete`], aber die Endantwort muss JSON nach `schema` sein. Passt sie
    /// nicht, bekommt das Modell den Fehler und darf sich bis zu zweimal korrigieren.
    pub async fn complete_structured(
        &self,
        session: SessionId,
        input: &str,
        schema: &Value,
    ) -> Result<Value, RunError> {
        let mut prompt = format!(
            "{input}\n\nAntworte am Ende ausschließlich mit JSON nach diesem Schema, ohne weiteren Text:\n{schema}"
        );
        let mut problem = String::new();
        for _ in 0..=MAX_CORRECTIONS {
            let outcome = self.complete(session, prompt).await?;
            if outcome.reason != FinishReason::Completed {
                return Err(RunError::Structured(format!(
                    "der Lauf endete mit {:?}",
                    outcome.reason
                )));
            }
            match parse(&outcome.text).and_then(|value| validate(schema, &value).map(|()| value)) {
                Ok(value) => return Ok(value),
                Err(error) => {
                    prompt = format!(
                        "Deine Antwort passt nicht zum Schema: {error}. Antworte nur mit korrigiertem JSON."
                    );
                    problem = error;
                }
            }
        }
        Err(RunError::Structured(problem))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use can_providers::FakeProvider;
    use serde_json::json;
    use std::sync::Arc;

    fn schema() -> Value {
        json!({"type": "object", "properties": {"stadt": {"type": "string"}, "einwohner": {"type": "integer"}},
               "required": ["stadt", "einwohner"], "additionalProperties": false})
    }

    fn agent(provider: Arc<FakeProvider>) -> Agent {
        Agent::builder()
            .provider_arc(provider)
            .model("test")
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn valid_json_is_returned_even_inside_a_code_block() {
        let provider = Arc::new(
            FakeProvider::new().text("```json\n{\"stadt\": \"Köln\", \"einwohner\": 1080000}\n```"),
        );
        let value = agent(provider.clone())
            .complete_structured(SessionId::new(), "Größte Stadt in NRW?", &schema())
            .await
            .unwrap();
        assert_eq!(value, json!({"stadt": "Köln", "einwohner": 1_080_000}));
        assert!(
            provider.requests()[0].messages[0]
                .text()
                .contains("\"required\":[\"stadt\",\"einwohner\"]")
        );
    }

    #[tokio::test]
    async fn the_model_gets_its_error_back_and_may_correct_itself() {
        let provider = Arc::new(
            FakeProvider::new()
                .text("Köln hat viele Einwohner.")
                .text("{\"stadt\": \"Köln\"}")
                .text("{\"stadt\": \"Köln\", \"einwohner\": 1}"),
        );
        let value = agent(provider.clone())
            .complete_structured(SessionId::new(), "?", &schema())
            .await
            .unwrap();
        assert_eq!(value["einwohner"], 1);
        let requests = provider.requests();
        assert!(
            requests[1]
                .messages
                .last()
                .unwrap()
                .text()
                .contains("kein gültiges JSON")
        );
        assert!(
            requests[2]
                .messages
                .last()
                .unwrap()
                .text()
                .contains("einwohner: Pflichtfeld fehlt")
        );
    }

    #[tokio::test]
    async fn gives_up_after_two_corrections() {
        let provider = Arc::new(
            FakeProvider::new()
                .text("nein")
                .text("nein")
                .text("nein")
                .text("{}"),
        );
        let result = agent(provider.clone())
            .complete_structured(SessionId::new(), "?", &schema())
            .await;
        assert!(matches!(result, Err(RunError::Structured(m)) if m.contains("kein gültiges JSON")));
        assert_eq!(provider.requests().len(), 3);
    }
}
