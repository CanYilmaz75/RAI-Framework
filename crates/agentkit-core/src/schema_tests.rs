use super::*;

fn file_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string"},
            "lines": {"type": ["integer", "null"], "minimum": 1, "maximum": 100},
            "mode": {"enum": ["text", "hex"]},
            "tags": {"type": "array", "items": {"type": "string"}},
            "range": {"$ref": "#/$defs/Range"}
        },
        "required": ["path"],
        "additionalProperties": false,
        "$defs": {"Range": {"type": "object", "properties": {"from": {"type": "integer"}}, "required": ["from"]}}
    })
}

fn error(value: Value) -> String {
    validate(&file_schema(), &value).unwrap_err()
}

#[test]
fn valid_arguments_pass() {
    let full = json!({"path": "a", "lines": 3, "mode": "hex", "tags": ["x"], "range": {"from": 1, "to": 2}});
    assert_eq!(validate(&file_schema(), &full), Ok(()));
    assert_eq!(
        validate(&file_schema(), &json!({"path": "a", "lines": null})),
        Ok(())
    );
    assert_eq!(
        validate(&file_schema(), &json!({"path": "a", "lines": 2.0})),
        Ok(())
    );
}

#[test]
fn errors_name_the_field_path() {
    assert_eq!(
        error(json!("text")),
        "Argumente: erwartet object, bekommen string"
    );
    assert_eq!(error(json!({})), "path: Pflichtfeld fehlt");
    assert_eq!(
        error(json!({"path": 7})),
        "path: erwartet string, bekommen number"
    );
    assert_eq!(
        error(json!({"path": "a", "extra": 1})),
        "extra: unbekanntes Feld"
    );
    assert_eq!(
        error(json!({"path": "a", "lines": 0})),
        "lines: 0 ist kleiner als das Minimum 1"
    );
    assert_eq!(
        error(json!({"path": "a", "lines": 101})),
        "lines: 101 ist größer als das Maximum 100"
    );
    assert_eq!(
        error(json!({"path": "a", "lines": 1.5})),
        "lines: erwartet integer oder null, bekommen number"
    );
    assert!(error(json!({"path": "a", "mode": "bin"})).starts_with("mode: \"bin\" ist keiner"));
    assert_eq!(
        error(json!({"path": "a", "tags": ["x", 2]})),
        "tags[1]: erwartet string, bekommen number"
    );
    assert_eq!(
        error(json!({"path": "a", "range": {}})),
        "range.from: Pflichtfeld fehlt"
    );
}

#[test]
fn combinators_const_and_boolean_schemas() {
    let any = json!({"anyOf": [{"type": "string"}, {"type": "integer"}]});
    assert_eq!(validate(&any, &json!(3)), Ok(()));
    assert_eq!(
        validate(&any, &json!(true)).unwrap_err(),
        "Argumente: passt zu keiner der erlaubten Formen"
    );
    let all = json!({"allOf": [{"type": "integer"}, {"minimum": 5}]});
    assert!(validate(&all, &json!(4)).is_err());
    assert_eq!(validate(&all, &json!(5)), Ok(()));
    assert!(validate(&json!({"const": "x"}), &json!("y")).is_err());
    assert_eq!(validate(&json!(true), &json!(1)), Ok(()));
    assert!(validate(&json!(false), &json!(1)).is_err());
    let typed_extra = json!({"type": "object", "additionalProperties": {"type": "string"}});
    assert_eq!(
        validate(&typed_extra, &json!({"a": 1})).unwrap_err(),
        "a: erwartet string, bekommen number"
    );
}

#[test]
fn broken_or_recursive_references_fail_cleanly() {
    assert!(
        validate(&json!({"$ref": "#/$defs/Fehlt"}), &json!(1))
            .unwrap_err()
            .contains("nicht auflösbar")
    );
    let endless = json!({"$ref": "#"});
    assert!(
        validate(&endless, &json!(1))
            .unwrap_err()
            .contains("zu tief")
    );
}

#[test]
fn object_schema_collects_fields_and_hoists_definitions() {
    let nested = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "Range",
        "$ref": "#/$defs/Range",
        "$defs": {"Range": {"type": "object"}}
    });
    let schema = object_schema(vec![
        ("path", json!({"type": "string", "title": "String"}), true),
        ("range", nested, false),
        ("any", json!(true), true),
    ]);
    assert_eq!(schema["properties"]["path"], json!({"type": "string"}));
    assert_eq!(
        schema["properties"]["range"],
        json!({"$ref": "#/$defs/Range"})
    );
    assert_eq!(schema["properties"]["any"], json!(true));
    assert_eq!(schema["required"], json!(["path"]));
    assert_eq!(schema["$defs"]["Range"], json!({"type": "object"}));
    assert_eq!(
        validate(&schema, &json!({"path": "a", "range": {}})),
        Ok(())
    );
    assert!(validate(&schema, &json!({"range": {}})).is_err());
}
