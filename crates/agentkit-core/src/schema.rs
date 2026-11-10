//! Prüft Tool-Argumente gegen ein JSON-Schema und baut Objekt-Schemas für das `#[tool]`-Makro.
//!
//! Unterstützt wird die Teilmenge, die Tool-Schemas üblicherweise nutzen: `type`, `enum`,
//! `const`, `properties`, `required`, `additionalProperties`, `items`, `minimum`, `maximum`,
//! `anyOf`, `oneOf`, `allOf` und lokale `$ref`. Unbekannte Schlüsselwörter werden ignoriert.

use serde_json::{Map, Value, json};

/// Prüft `value` gegen `schema`. Der Fehler nennt den Feldpfad und geht so ans Modell zurück.
pub fn validate(schema: &Value, value: &Value) -> Result<(), String> {
    check(schema, schema, value, "", 0)
}

fn check(
    root: &Value,
    schema: &Value,
    value: &Value,
    path: &str,
    depth: u32,
) -> Result<(), String> {
    let at = if path.is_empty() { "Argumente" } else { path };
    let schema = match schema {
        Value::Bool(true) => return Ok(()),
        Value::Bool(false) => return Err(format!("{at}: hier ist kein Wert erlaubt")),
        Value::Object(schema) => schema,
        _ => return Ok(()),
    };
    if depth > 64 {
        return Err(format!("{at}: Schema ist zu tief verschachtelt"));
    }
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let target = reference
            .strip_prefix('#')
            .and_then(|pointer| root.pointer(pointer));
        let target =
            target.ok_or_else(|| format!("{at}: Schema-Verweis {reference} nicht auflösbar"))?;
        check(root, target, value, path, depth + 1)?;
    }
    if let Some(expected) = schema.get("type") {
        let names: Vec<&str> = match expected {
            Value::String(name) => vec![name.as_str()],
            Value::Array(names) => names.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        if !names.is_empty() && !names.iter().any(|name| has_type(value, name)) {
            return Err(format!(
                "{at}: erwartet {}, bekommen {}",
                names.join(" oder "),
                type_name(value)
            ));
        }
    }
    if let Some(options) = schema.get("enum").and_then(Value::as_array)
        && !options.contains(value)
    {
        return Err(format!(
            "{at}: {value} ist keiner der erlaubten Werte {}",
            Value::Array(options.clone())
        ));
    }
    if let Some(constant) = schema.get("const")
        && constant != value
    {
        return Err(format!("{at}: erwartet {constant}"));
    }
    if let Some(number) = value.as_f64() {
        if schema
            .get("minimum")
            .and_then(Value::as_f64)
            .is_some_and(|minimum| number < minimum)
        {
            return Err(format!(
                "{at}: {value} ist kleiner als das Minimum {}",
                schema["minimum"]
            ));
        }
        if schema
            .get("maximum")
            .and_then(Value::as_f64)
            .is_some_and(|maximum| number > maximum)
        {
            return Err(format!(
                "{at}: {value} ist größer als das Maximum {}",
                schema["maximum"]
            ));
        }
    }
    if let Value::Object(object) = value {
        check_object(root, schema, object, path, depth)?;
    }
    if let (Value::Array(items), Some(item_schema)) = (value, schema.get("items")) {
        for (index, item) in items.iter().enumerate() {
            check(
                root,
                item_schema,
                item,
                &format!("{path}[{index}]"),
                depth + 1,
            )?;
        }
    }
    for keyword in ["anyOf", "oneOf"] {
        if let Some(options) = schema.get(keyword).and_then(Value::as_array)
            && !options
                .iter()
                .any(|option| check(root, option, value, path, depth + 1).is_ok())
        {
            return Err(format!("{at}: passt zu keiner der erlaubten Formen"));
        }
    }
    if let Some(parts) = schema.get("allOf").and_then(Value::as_array) {
        for part in parts {
            check(root, part, value, path, depth + 1)?;
        }
    }
    Ok(())
}

fn check_object(
    root: &Value,
    schema: &Map<String, Value>,
    object: &Map<String, Value>,
    path: &str,
    depth: u32,
) -> Result<(), String> {
    let join = |key: &str| {
        if path.is_empty() {
            key.to_owned()
        } else {
            format!("{path}.{key}")
        }
    };
    let properties = schema.get("properties").and_then(Value::as_object);
    for required in schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(key) = required.as_str()
            && !object.contains_key(key)
        {
            return Err(format!("{}: Pflichtfeld fehlt", join(key)));
        }
    }
    for (key, value) in object {
        match properties.and_then(|properties| properties.get(key)) {
            Some(property) => check(root, property, value, &join(key), depth + 1)?,
            None => match schema.get("additionalProperties") {
                Some(Value::Bool(false)) => return Err(format!("{}: unbekanntes Feld", join(key))),
                Some(extra @ Value::Object(_)) => check(root, extra, value, &join(key), depth + 1)?,
                _ => {}
            },
        }
    }
    Ok(())
}

fn has_type(value: &Value, name: &str) -> bool {
    match name {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "number" => value.is_number(),
        "integer" => {
            value.is_i64() || value.is_u64() || value.as_f64().is_some_and(|n| n.fract() == 0.0)
        }
        _ => true,
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Baut ein Objekt-Schema aus den Schemas einzelner Felder: `(Name, Schema, Pflichtfeld)`.
///
/// Geteilte Definitionen (`$defs`) der Feld-Schemas wandern an die Wurzel, damit ihre
/// Verweise gültig bleiben; `$schema` und `title` der Felder entfallen.
pub fn object_schema(fields: Vec<(&str, Value, bool)>) -> Value {
    let mut properties = Map::new();
    let mut required = Vec::new();
    let mut definitions = Map::new();
    for (name, schema, is_required) in fields {
        let mut schema = match schema {
            Value::Object(schema) => schema,
            other => {
                properties.insert(name.to_owned(), other);
                continue;
            }
        };
        schema.remove("$schema");
        schema.remove("title");
        if let Some(Value::Object(shared)) = schema.remove("$defs") {
            definitions.extend(shared);
        }
        properties.insert(name.to_owned(), Value::Object(schema));
        if is_required {
            required.push(json!(name));
        }
    }
    let mut object = json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    });
    if !definitions.is_empty() {
        object["$defs"] = Value::Object(definitions);
    }
    object
}

#[cfg(test)]
#[path = "schema_tests.rs"]
mod tests;
