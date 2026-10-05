//! A small JSON Schema subset for validating tool arguments before a tool
//! runs: `type`, `properties`, `required`, `additionalProperties: false`,
//! `items`, `enum`, `minimum`/`maximum`, `minLength`/`maxLength`, `anyOf`.
//! Errors are phrased so a model can correct its call.

use serde_json::Value;

pub fn validate(schema: &Value, value: &Value) -> Result<(), String> {
    check(schema, value, "arguments")
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn matches_type(expected: &str, value: &Value) -> bool {
    match expected {
        "number" => value.is_number(),
        other => type_name(value) == other,
    }
}

fn check(schema: &Value, value: &Value, at: &str) -> Result<(), String> {
    let Some(schema) = schema.as_object() else {
        return Ok(()); // `true` / missing schema accepts anything
    };
    if let Some(any) = schema.get("anyOf").and_then(Value::as_array)
        && !any.iter().any(|s| check(s, value, at).is_ok())
    {
        return Err(format!("{at} does not match any allowed form"));
    }
    match schema.get("type") {
        Some(Value::String(t)) if !matches_type(t, value) => {
            return Err(format!("{at} must be {t}, not {}", type_name(value)));
        }
        Some(Value::Array(types))
            if !types
                .iter()
                .filter_map(Value::as_str)
                .any(|t| matches_type(t, value)) =>
        {
            return Err(format!("{at} has the wrong type ({})", type_name(value)));
        }
        _ => {}
    }
    if let Some(options) = schema.get("enum").and_then(Value::as_array)
        && !options.contains(value)
    {
        return Err(format!(
            "{at} must be one of {}",
            Value::Array(options.clone())
        ));
    }
    if let Some(n) = value.as_f64() {
        if let Some(min) = schema.get("minimum").and_then(Value::as_f64)
            && n < min
        {
            return Err(format!("{at} must be at least {min}"));
        }
        if let Some(max) = schema.get("maximum").and_then(Value::as_f64)
            && n > max
        {
            return Err(format!("{at} must be at most {max}"));
        }
    }
    if let Some(s) = value.as_str() {
        let len = s.chars().count() as u64;
        if let Some(min) = schema.get("minLength").and_then(Value::as_u64)
            && len < min
        {
            return Err(format!("{at} must have at least {min} characters"));
        }
        if let Some(max) = schema.get("maxLength").and_then(Value::as_u64)
            && len > max
        {
            return Err(format!("{at} must have at most {max} characters"));
        }
    }
    if let Some(object) = value.as_object() {
        let properties = schema.get("properties").and_then(Value::as_object);
        for name in schema
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if !object.contains_key(name) {
                return Err(format!("{at} is missing required field `{name}`"));
            }
        }
        for (key, item) in object {
            match properties.and_then(|p| p.get(key)) {
                Some(sub) => check(sub, item, &format!("{at}.{key}"))?,
                None if schema.get("additionalProperties") == Some(&Value::Bool(false)) => {
                    return Err(format!("{at} has unknown field `{key}`"));
                }
                None => {}
            }
        }
    }
    if let (Some(items), Some(array)) = (schema.get("items"), value.as_array()) {
        for (i, item) in array.iter().enumerate() {
            check(items, item, &format!("{at}[{i}]"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validates_objects_types_and_ranges() {
        let schema = json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "minLength": 1},
                "start_line": {"type": "integer", "minimum": 1},
                "mode": {"enum": ["a", "b"]},
                "edits": {"type": "array", "items": {
                    "type": "object",
                    "properties": {"old_text": {"type": "string"}},
                    "required": ["old_text"]
                }}
            },
            "required": ["path"],
            "additionalProperties": false
        });
        assert!(validate(&schema, &json!({"path": "a", "start_line": 3})).is_ok());
        let cases = [
            (json!({}), "missing required field `path`"),
            (json!({"path": 3}), "arguments.path must be string"),
            (json!({"path": ""}), "at least 1"),
            (json!({"path": "a", "start_line": 0}), "at least 1"),
            (json!({"path": "a", "start_line": 1.5}), "must be integer"),
            (json!({"path": "a", "mode": "c"}), "one of"),
            (json!({"path": "a", "x": 1}), "unknown field `x`"),
            (
                json!({"path": "a", "edits": [{}]}),
                "arguments.edits[0] is missing",
            ),
            (json!("text"), "must be object"),
        ];
        for (value, needle) in cases {
            let error = validate(&schema, &value).unwrap_err();
            assert!(error.contains(needle), "{value}: {error}");
        }
        assert!(
            validate(
                &json!({"anyOf": [{"type": "string"}, {"type": "null"}]}),
                &json!(null)
            )
            .is_ok()
        );
        assert!(validate(&json!(true), &json!(1)).is_ok());
    }
}
