//! Beispiel-Tool `read_file`, von Hand implementiert.

use async_trait::async_trait;
use can_core::{Effect, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::fs;

/// Liest eine Textdatei im Arbeitsverzeichnis.
pub struct ReadFile {
    spec: ToolSpec,
}

#[derive(Deserialize)]
struct Args {
    path: String,
}

impl ReadFile {
    /// Das Tool mit seiner festen Spec: lesend und idempotent.
    pub fn new() -> Self {
        let schema = json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Pfad relativ zum Arbeitsverzeichnis"}
            },
            "required": ["path"],
            "additionalProperties": false
        });
        let spec = ToolSpec::new(
            "read_file",
            "Liest eine Textdatei im Arbeitsverzeichnis.",
            schema,
        )
        .with_effect(Effect::ReadOnly)
        .with_idempotent(true);
        Self { spec }
    }
}

impl Default for ReadFile {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ReadFile {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, args: Value, cx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let args: Args = serde_json::from_value(args)
            .map_err(|error| ToolError::ForModel(format!("Ungültige Argumente: {error}")))?;
        fs::read_text(cx, &args.path).await.map(ToolOutput::text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::TempDir;

    #[test]
    fn spec_is_read_only_and_idempotent() {
        let tool = ReadFile::default();
        let spec = tool.spec();
        assert_eq!(spec.name, "read_file");
        assert_eq!((spec.effect, spec.idempotent), (Effect::ReadOnly, true));
        assert_eq!(spec.input_schema.0["required"], json!(["path"]));
    }

    #[tokio::test]
    async fn reads_a_file() {
        let dir = TempDir::new();
        dir.write("notiz.txt", b"Geheimzahl 4711");
        let output = ReadFile::new()
            .call(json!({"path": "notiz.txt"}), &dir.ctx())
            .await
            .unwrap();
        assert_eq!(output.as_text(), "Geheimzahl 4711");
    }

    #[tokio::test]
    async fn bad_arguments_and_escapes_are_errors_for_the_model() {
        let dir = TempDir::new();
        let tool = ReadFile::new();
        let missing = tool.call(json!({}), &dir.ctx()).await;
        assert!(
            matches!(missing, Err(ToolError::ForModel(m)) if m.contains("Ungültige Argumente"))
        );
        let escape = tool
            .call(json!({"path": "../../etc/passwd"}), &dir.ctx())
            .await;
        assert!(matches!(escape, Err(ToolError::ForModel(m)) if m.contains("außerhalb")));
    }
}
