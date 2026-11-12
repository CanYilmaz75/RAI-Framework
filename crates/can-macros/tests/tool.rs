//! Das Makro `#[tool]` an echten Funktionen.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use can_core::{
    CallId, Effect, ManualClock, ProgressSink, SecretsHandle, SessionId, Tool, ToolCtx, ToolError,
    schema::validate,
};
use can_macros::tool;
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// Abnahme Phase 2: ein Tool in unter zehn Zeilen.
/// Zählt die Wörter eines Textes.
#[tool(effect = "read_only", idempotent)]
async fn count_words(text: String) -> Result<String, ToolError> {
    Ok(text.split_whitespace().count().to_string())
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct Range {
    from: u32,
    to: Option<u32>,
}

/// Schneidet einen Bereich aus dem Namen des Arbeitsverzeichnisses.
///
/// Zweite Zeile der Beschreibung.
#[tool(name = "slice_workdir", timeout = 5, confidential)]
pub async fn slice(
    cx: &ToolCtx,
    range: Range,
    upper: Option<bool>,
    tags: Vec<String>,
) -> Result<String, ToolError> {
    let name = cx.workdir.display().to_string();
    let end = range
        .to
        .map(|to| to as usize)
        .unwrap_or(name.len())
        .min(name.len());
    let part = name
        .get(range.from as usize..end)
        .ok_or_else(|| ToolError::ForModel("Bereich ungültig".into()))?;
    let part = if upper.unwrap_or(false) {
        part.to_uppercase()
    } else {
        part.to_owned()
    };
    Ok(format!("{part}{}", tags.join("")))
}

/// Tut nichts und braucht keine Argumente.
#[tool(effect = "external")]
async fn ping() -> Result<&'static str, ToolError> {
    Ok("pong")
}

fn ctx() -> ToolCtx {
    ToolCtx {
        session: SessionId::new(),
        call_id: CallId::new(),
        workdir: "/arbeits/verzeichnis".into(),
        cancel: CancellationToken::new(),
        clock: Arc::new(ManualClock::default()),
        progress: ProgressSink::noop(),
        secrets: SecretsHandle::empty(),
        namespace: can_core::DEFAULT_NAMESPACE.into(),
    }
}

#[test]
fn spec_comes_from_signature_doc_comment_and_attributes() {
    let spec = count_words.spec();
    assert_eq!(spec.name, "count_words");
    assert_eq!(
        spec.description,
        "Abnahme Phase 2: ein Tool in unter zehn Zeilen.\nZählt die Wörter eines Textes."
    );
    assert_eq!(
        (spec.effect, spec.idempotent, spec.timeout),
        (Effect::ReadOnly, true, Duration::from_secs(60))
    );
    assert_eq!(
        spec.input_schema.0,
        json!({
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
            "additionalProperties": false
        })
    );
}

#[test]
fn defaults_are_safe_and_attributes_override() {
    let spec = slice.spec();
    assert_eq!(spec.name, "slice_workdir");
    assert_eq!(
        (spec.effect, spec.idempotent, spec.timeout),
        (Effect::Write, false, Duration::from_secs(5))
    );
    assert!(spec.description.ends_with("Zweite Zeile der Beschreibung."));
    assert_eq!(ping.spec().effect, Effect::External);
    assert!(spec.confidential && !ping.spec().confidential);
    assert_eq!(ping.spec().input_schema.0["properties"], json!({}));
}

#[test]
fn schema_handles_structs_options_and_lists() {
    let schema = &slice.spec().input_schema.0;
    assert_eq!(
        schema["required"],
        json!(["range", "tags"]),
        "Option ist kein Pflichtfeld"
    );
    assert_eq!(schema["properties"]["tags"]["items"]["type"], "string");
    assert_eq!(
        validate(schema, &json!({"range": {"from": 1}, "tags": []})),
        Ok(())
    );
    assert_eq!(
        validate(
            schema,
            &json!({"range": {"from": 1, "to": 3}, "upper": true, "tags": ["x"]})
        ),
        Ok(())
    );
    assert!(
        validate(schema, &json!({"range": {}, "tags": []}))
            .unwrap_err()
            .contains("from")
    );
    assert!(validate(schema, &json!({"range": {"from": "eins"}, "tags": []})).is_err());
    assert!(
        validate(schema, &json!({"tags": []}))
            .unwrap_err()
            .contains("range")
    );
}

#[tokio::test]
async fn call_parses_arguments_and_converts_the_result() {
    let output = count_words
        .call(json!({"text": "eins zwei drei"}), &ctx())
        .await
        .unwrap();
    assert_eq!(output.as_text(), "3");
    assert_eq!(
        ping.call(json!({}), &ctx()).await.unwrap().as_text(),
        "pong"
    );
    assert_eq!(
        ping.call(serde_json::Value::Null, &ctx())
            .await
            .unwrap()
            .as_text(),
        "pong"
    );

    let args = json!({"range": {"from": 1, "to": 8}, "upper": true, "tags": ["!", "?"]});
    assert_eq!(
        slice.call(args, &ctx()).await.unwrap().as_text(),
        "ARBEITS!?"
    );
}

#[tokio::test]
async fn bad_arguments_and_tool_errors_are_for_the_model() {
    let missing = count_words.call(json!({}), &ctx()).await;
    assert!(matches!(missing, Err(ToolError::ForModel(m)) if m.contains("`text`")));
    let wrong = count_words.call(json!("kein objekt"), &ctx()).await;
    assert!(matches!(wrong, Err(ToolError::ForModel(m)) if m.contains("JSON-Objekt")));
    let failed = slice
        .call(json!({"range": {"from": 900}, "tags": []}), &ctx())
        .await;
    assert!(matches!(failed, Err(ToolError::ForModel(m)) if m == "Bereich ungültig"));
}

#[test]
fn tools_are_values_usable_as_trait_objects() {
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(count_words), Box::new(slice), Box::new(ping)];
    let names: Vec<&str> = tools.iter().map(|tool| tool.spec().name.as_str()).collect();
    assert_eq!(names, ["count_words", "slice_workdir", "ping"]);
}
