//! Tool inputs fixed to the types the tool's schema asks for, before they
//! are read: models (local ones especially) send `"42"` for an integer,
//! `"true"` for a boolean, a JSON-encoded string for an array or object, or
//! one value where an array is expected. Each of those failed the call with
//! a type error and cost a step to make again.
//!
//! Only unambiguous repairs are made; anything else is passed on as it was
//! and fails as before, with the tool's own message.

use serde_json::{Map, Number, Value};

/// `input` with its values coerced to `schema` (a JSON Schema object).
#[must_use]
pub fn coerce_to_schema(input: &Value, schema: &Value) -> Value {
    coerce(input, schema).unwrap_or_else(|| input.clone())
}

/// The coerced value, or `None` when nothing needed changing.
fn coerce(value: &Value, schema: &Value) -> Option<Value> {
    let types = schema_types(schema);
    if types.is_empty() || types.iter().any(|kind| matches_type(value, kind)) {
        // Right type already: recurse into containers.
        return match value {
            Value::Object(map) => coerce_object(map, schema).map(Value::Object),
            Value::Array(items) => coerce_items(items, schema).map(Value::Array),
            _ => None,
        };
    }
    types.iter().find_map(|kind| convert(value, kind, schema))
}

fn schema_types(schema: &Value) -> Vec<&str> {
    match schema.get("type") {
        Some(Value::String(kind)) => vec![kind.as_str()],
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

fn matches_type(value: &Value, kind: &str) -> bool {
    match kind {
        "string" => value.is_string(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "null" => value.is_null(),
        _ => true,
    }
}

fn convert(value: &Value, kind: &str, schema: &Value) -> Option<Value> {
    match (kind, value) {
        ("integer", Value::String(text)) => text.trim().parse::<i64>().ok().map(Value::from),
        ("integer", Value::Number(number)) => number
            .as_f64()
            .filter(|float| float.fract() == 0.0 && float.abs() < 9.0e15)
            .map(|float| Value::from(float as i64)),
        ("number", Value::String(text)) => text
            .trim()
            .parse::<f64>()
            .ok()
            .and_then(Number::from_f64)
            .map(Value::Number),
        ("boolean", Value::String(text)) => match text.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" => Some(Value::Bool(true)),
            "false" | "no" | "0" => Some(Value::Bool(false)),
            _ => None,
        },
        ("boolean", Value::Number(number)) => match number.as_i64() {
            Some(1) => Some(Value::Bool(true)),
            Some(0) => Some(Value::Bool(false)),
            _ => None,
        },
        ("string", Value::Number(_) | Value::Bool(_)) => Some(Value::String(value.to_string())),
        ("array" | "object", Value::String(text)) => {
            let parsed = serde_json::from_str::<Value>(text.trim()).ok();
            match parsed {
                Some(parsed) if matches_type(&parsed, kind) => {
                    Some(coerce_to_schema(&parsed, schema))
                }
                // A bare string where an array of strings is expected.
                _ if kind == "array" && item_accepts_string(schema) => {
                    Some(Value::Array(vec![value.clone()]))
                }
                _ => None,
            }
        }
        ("array", Value::Number(_) | Value::Bool(_) | Value::Object(_)) => {
            let item = schema
                .get("items")
                .map_or_else(|| value.clone(), |items| coerce_to_schema(value, items));
            Some(Value::Array(vec![item]))
        }
        _ => None,
    }
}

fn item_accepts_string(schema: &Value) -> bool {
    schema.get("items").is_none_or(|items| {
        let types = schema_types(items);
        types.is_empty() || types.contains(&"string")
    })
}

fn coerce_object(map: &Map<String, Value>, schema: &Value) -> Option<Map<String, Value>> {
    let properties = schema.get("properties")?.as_object()?;
    let mut changed = false;
    let mut out = map.clone();
    for (key, value) in map {
        let Some(property) = properties.get(key) else {
            continue;
        };
        if let Some(coerced) = coerce(value, property) {
            out.insert(key.clone(), coerced);
            changed = true;
        }
    }
    changed.then_some(out)
}

fn coerce_items(items: &[Value], schema: &Value) -> Option<Vec<Value>> {
    let item_schema = schema.get("items")?;
    let mut changed = false;
    let out = items
        .iter()
        .map(|item| {
            coerce(item, item_schema).map_or_else(
                || item.clone(),
                |coerced| {
                    changed = true;
                    coerced
                },
            )
        })
        .collect();
    changed.then_some(out)
}

#[cfg(test)]
mod tests {
    use super::coerce_to_schema;
    use serde_json::json;

    #[test]
    fn values_are_coerced_to_the_schema_types() {
        let schema = json!({
            "type": "object",
            "properties": {
                "limit": {"type": "integer"},
                "ratio": {"type": "number"},
                "background": {"type": "boolean"},
                "paths": {"type": "array", "items": {"type": "string"}},
                "todos": {"type": "array", "items": {
                    "type": "object",
                    "properties": {"done": {"type": "boolean"}}
                }},
                "name": {"type": "string"},
                "mode": {"type": ["string", "null"]}
            }
        });
        let input = json!({
            "limit": "40",
            "ratio": "0.5",
            "background": "true",
            "paths": "src/main.rs",
            "todos": "[{\"done\": \"false\"}]",
            "name": 7,
            "mode": null,
            "extra": "kept"
        });
        assert_eq!(
            coerce_to_schema(&input, &schema),
            json!({
                "limit": 40,
                "ratio": 0.5,
                "background": true,
                "paths": ["src/main.rs"],
                "todos": [{"done": false}],
                "name": "7",
                "mode": null,
                "extra": "kept"
            })
        );
    }

    #[test]
    fn ambiguous_values_are_left_alone() {
        let schema = json!({
            "type": "object",
            "properties": {
                "limit": {"type": "integer"},
                "flag": {"type": "boolean"},
                "ids": {"type": "array", "items": {"type": "integer"}}
            }
        });
        let input = json!({"limit": "forty", "flag": "maybe", "ids": "1, 2"});
        assert_eq!(coerce_to_schema(&input, &schema), input);
    }
}
