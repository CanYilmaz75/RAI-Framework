//! Tool `write_file`, über das Makro `#[tool]` definiert.

use can_core::{ToolCtx, ToolError};
use can_macros::tool;

use crate::fs;

/// Schreibt eine Textdatei im Arbeitsverzeichnis. Eine vorhandene Datei wird überschrieben.
#[tool(effect = "write", idempotent)]
pub async fn write_file(cx: &ToolCtx, path: String, content: String) -> Result<String, ToolError> {
    fs::write_text(cx, &path, &content).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::TempDir;
    use can_core::{Effect, Tool};
    use serde_json::json;

    #[test]
    fn spec_is_a_write_that_may_be_repeated() {
        let spec = write_file.spec();
        assert_eq!(
            (spec.name.as_str(), spec.effect, spec.idempotent),
            ("write_file", Effect::Write, true)
        );
        assert_eq!(spec.input_schema.0["required"], json!(["path", "content"]));
    }

    #[tokio::test]
    async fn writes_inside_and_refuses_outside() {
        let dir = TempDir::new();
        let args = json!({"path": "a.txt", "content": "eins"});
        assert!(
            write_file
                .call(args, &dir.ctx())
                .await
                .unwrap()
                .as_text()
                .starts_with("4 Bytes")
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "eins"
        );
        let escape = write_file
            .call(json!({"path": "/tmp/raus.txt", "content": "x"}), &dir.ctx())
            .await;
        assert!(matches!(escape, Err(ToolError::ForModel(m)) if m.contains("außerhalb")));
    }
}
