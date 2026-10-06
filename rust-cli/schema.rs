//! A small JSON Schema validator for exactly the keywords the MCP tools use.
//!
//! The same schema objects are sent to clients in `tools/list`, used to validate every request
//! and used to validate every response, so the contract a client reads is the contract the
//! server enforces. A schema that uses a keyword this validator does not implement is rejected
//! instead of ignored: a constraint that is written down but not enforced would be a hole.

use serde_json::Value;

const ANNOTATIONS: &[&str] = &["description", "title", "default", "$schema", "examples"];
const KEYWORDS: &[&str] = &[
    "type",
    "enum",
    "const",
    "minimum",
    "maximum",
    "minLength",
    "maxLength",
    "required",
    "properties",
    "additionalProperties",
    "items",
    "prefixItems",
    "minItems",
    "maxItems",
];

/// How many problems are reported at most, and how much of an offending value is quoted.
const MAX_ERRORS: usize = 8;
const QUOTE_CHARS: usize = 40;

pub fn validate(schema: &Value, value: &Value) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    check(schema, value, "$", &mut errors);
    if errors.is_empty() { Ok(()) } else { Err(errors) }
}

/// Checks a schema itself: only known keywords, and every `required` name is a declared property.
pub fn lint(schema: &Value) -> Result<(), String> {
    lint_at(schema, "$")
}

fn lint_at(schema: &Value, path: &str) -> Result<(), String> {
    let Some(object) = schema.as_object() else {
        return Err(format!("{path}: a schema must be an object"));
    };
    for key in object.keys() {
        if !KEYWORDS.contains(&key.as_str()) && !ANNOTATIONS.contains(&key.as_str()) {
            return Err(format!("{path}: unsupported schema keyword `{key}`"));
        }
    }
    if let Some(required) = object.get("required") {
        let properties = object.get("properties").and_then(Value::as_object);
        for name in required.as_array().ok_or(format!("{path}: `required` must be an array"))? {
            let name = name.as_str().ok_or(format!("{path}: `required` holds strings"))?;
            if !properties.is_some_and(|p| p.contains_key(name)) {
                return Err(format!("{path}: `{name}` is required but not a declared property"));
            }
        }
    }
    if let Some(properties) = object.get("properties").and_then(Value::as_object) {
        for (name, sub) in properties {
            lint_at(sub, &format!("{path}.{name}"))?;
        }
    }
    if let Some(items) = object.get("items") {
        lint_at(items, &format!("{path}[]"))?;
    }
    if let Some(prefix) = object.get("prefixItems") {
        for (index, sub) in prefix.as_array().ok_or(format!("{path}: `prefixItems` must be an array"))?.iter().enumerate() {
            lint_at(sub, &format!("{path}[{index}]"))?;
        }
    }
    if let Some(extra) = object.get("additionalProperties").filter(|e| !e.is_boolean()) {
        lint_at(extra, &format!("{path}.*"))?;
    }
    Ok(())
}

fn push(errors: &mut Vec<String>, message: String) {
    if errors.len() < MAX_ERRORS {
        errors.push(message);
    }
}

/// A short, bounded rendering of a value for error messages, so a huge input cannot make its
/// own error message huge.
pub fn quote(value: &Value) -> String {
    let text = value.to_string();
    if text.chars().count() <= QUOTE_CHARS {
        text
    } else {
        let head: String = text.chars().take(QUOTE_CHARS).collect();
        format!("{head}…")
    }
}

fn kind_of(value: &Value) -> &'static str {
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

fn type_matches(name: &str, value: &Value) -> bool {
    match name {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "number" => value.is_number(),
        "string" => value.is_string(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => false,
    }
}

fn check(schema: &Value, value: &Value, path: &str, errors: &mut Vec<String>) {
    let Some(object) = schema.as_object() else {
        push(errors, format!("{path}: a schema must be an object"));
        return;
    };
    for key in object.keys() {
        if !KEYWORDS.contains(&key.as_str()) && !ANNOTATIONS.contains(&key.as_str()) {
            push(errors, format!("{path}: unsupported schema keyword `{key}`"));
            return;
        }
    }

    if let Some(expected) = object.get("type") {
        let names: Vec<&str> = match expected {
            Value::String(s) => vec![s.as_str()],
            Value::Array(list) => list.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        if !names.iter().any(|n| type_matches(n, value)) {
            push(
                errors,
                format!("{path}: expected {}, got {} {}", names.join(" or "), kind_of(value), quote(value)),
            );
            return;
        }
    }
    if let Some(allowed) = object.get("enum").and_then(Value::as_array).filter(|a| !a.contains(value)) {
        let list: Vec<String> = allowed.iter().map(|v| v.to_string()).collect();
        push(errors, format!("{path}: {} is not one of {}", quote(value), list.join(", ")));
        return;
    }
    if let Some(expected) = object.get("const").filter(|e| *e != value) {
        push(errors, format!("{path}: must be {}", quote(expected)));
        return;
    }

    if let Some(number) = value.as_f64().filter(|_| value.is_number()) {
        // Integers are compared exactly, so 2^53 + 1 does not round into range.
        let exact = value.as_i64();
        if let Some(min) = object.get("minimum") {
            let below = match (exact, min.as_i64()) {
                (Some(v), Some(m)) => v < m,
                _ => min.as_f64().is_some_and(|m| number < m),
            };
            if below {
                push(errors, format!("{path}: {number} is less than the minimum {min}"));
            }
        }
        if let Some(max) = object.get("maximum") {
            let above = match (exact, max.as_i64()) {
                (Some(v), Some(m)) => v > m,
                _ => max.as_f64().is_some_and(|m| number > m),
            };
            if above {
                push(errors, format!("{path}: {number} is greater than the maximum {max}"));
            }
        }
    }

    if let Some(text) = value.as_str() {
        let length = text.chars().count() as u64;
        if let Some(min) = object.get("minLength").and_then(Value::as_u64).filter(|m| length < *m) {
            push(errors, format!("{path}: length {length} is below the minimum {min}"));
        }
        if let Some(max) = object.get("maxLength").and_then(Value::as_u64).filter(|m| length > *m) {
            push(errors, format!("{path}: length {length} is above the maximum {max}"));
        }
    }

    if let Some(items) = value.as_array() {
        let length = items.len() as u64;
        if let Some(min) = object.get("minItems").and_then(Value::as_u64).filter(|m| length < *m) {
            push(errors, format!("{path}: {length} items, at least {min} required"));
        }
        if let Some(max) = object.get("maxItems").and_then(Value::as_u64).filter(|m| length > *m) {
            push(errors, format!("{path}: {length} items, at most {max} allowed"));
        }
        if let Some(item_schema) = object.get("items") {
            for (index, item) in items.iter().enumerate() {
                check(item_schema, item, &format!("{path}[{index}]"), errors);
            }
        }
        if let Some(Value::Array(positional)) = object.get("prefixItems") {
            for (index, (sub, item)) in positional.iter().zip(items).enumerate() {
                check(sub, item, &format!("{path}[{index}]"), errors);
            }
        }
    }

    if let Some(map) = value.as_object() {
        let properties = object.get("properties").and_then(Value::as_object);
        if let Some(Value::Array(required)) = object.get("required") {
            for name in required.iter().filter_map(Value::as_str) {
                if !map.contains_key(name) {
                    push(errors, format!("{path}: missing required `{name}`"));
                }
            }
        }
        for (name, member) in map {
            match properties.and_then(|p| p.get(name)) {
                Some(sub) => check(sub, member, &format!("{path}.{name}"), errors),
                None => match object.get("additionalProperties") {
                    Some(Value::Bool(false)) => {
                        let allowed: Vec<&str> =
                            properties.map(|p| p.keys().map(String::as_str).collect()).unwrap_or_default();
                        push(
                            errors,
                            format!("{path}: unknown parameter `{name}` (allowed: {})", allowed.join(", ")),
                        );
                    }
                    Some(sub) if sub.is_object() => check(sub, member, &format!("{path}.{name}"), errors),
                    _ => {}
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn problems(schema: Value, value: Value) -> Vec<String> {
        validate(&schema, &value).err().unwrap_or_default()
    }

    #[test]
    fn types_are_strict() {
        let int = json!({"type": "integer"});
        assert!(validate(&int, &json!(3)).is_ok());
        assert!(validate(&int, &json!(-3)).is_ok());
        assert!(validate(&int, &json!(3.5)).is_err());
        assert!(validate(&int, &json!(3.0)).is_err(), "a float is not an integer, even when whole");
        assert!(validate(&int, &json!("3")).is_err());
        assert!(validate(&int, &json!(null)).is_err());
        assert!(validate(&int, &json!(true)).is_err());
        let either = json!({"type": ["integer", "null"]});
        assert!(validate(&either, &json!(null)).is_ok());
        assert!(validate(&either, &json!(4)).is_ok());
        assert!(validate(&either, &json!("4")).is_err());
        assert!(validate(&json!({"type": "boolean"}), &json!(0)).is_err());
        assert!(validate(&json!({"type": "array"}), &json!({})).is_err());
        assert!(validate(&json!({"type": "object"}), &json!([])).is_err());
    }

    #[test]
    fn bounds_are_inclusive_and_exact() {
        let schema = json!({"type": "integer", "minimum": 1, "maximum": 10});
        assert!(validate(&schema, &json!(1)).is_ok());
        assert!(validate(&schema, &json!(10)).is_ok());
        assert!(problems(schema.clone(), json!(0))[0].contains("less than the minimum 1"));
        assert!(problems(schema.clone(), json!(11))[0].contains("greater than the maximum 10"));
        // A value just past i64 precision for f64 must not round into range.
        let wide = json!({"type": "integer", "maximum": 9007199254740992i64});
        assert!(validate(&wide, &json!(9007199254740993i64)).is_err());
        assert!(validate(&schema, &json!(i64::MIN)).is_err());
        assert!(validate(&schema, &json!(u64::MAX)).is_err());
    }

    #[test]
    fn string_length_counts_characters_not_bytes() {
        let schema = json!({"type": "string", "minLength": 2, "maxLength": 3});
        assert!(validate(&schema, &json!("äö")).is_ok(), "two characters, four bytes");
        assert!(validate(&schema, &json!("👋👋👋")).is_ok());
        assert!(validate(&schema, &json!("👋👋👋👋")).is_err());
        assert!(validate(&schema, &json!("a")).is_err());
        assert!(validate(&schema, &json!("")).is_err());
    }

    #[test]
    fn enums_and_consts() {
        let schema = json!({"enum": ["a", "b"]});
        assert!(validate(&schema, &json!("a")).is_ok());
        assert!(problems(schema.clone(), json!("c"))[0].contains("is not one of"));
        assert!(validate(&schema, &json!(1)).is_err());
        assert!(validate(&json!({"const": 3}), &json!(3)).is_ok());
        assert!(validate(&json!({"const": 3}), &json!(4)).is_err());
    }

    #[test]
    fn objects_require_and_reject_unknown_members() {
        let schema = json!({
            "type": "object",
            "properties": {"a": {"type": "integer"}, "b": {"type": "string"}},
            "required": ["a"],
            "additionalProperties": false
        });
        assert!(validate(&schema, &json!({"a": 1})).is_ok());
        assert!(validate(&schema, &json!({"a": 1, "b": "x"})).is_ok());
        assert!(problems(schema.clone(), json!({}))[0].contains("missing required `a`"));
        let unknown = problems(schema.clone(), json!({"a": 1, "c": 2}));
        assert!(unknown[0].contains("unknown parameter `c`") && unknown[0].contains("a, b"));
        assert!(problems(schema.clone(), json!({"a": "x"}))[0].starts_with("$.a: expected integer"));
        assert!(validate(&schema, &json!({"a": null})).is_err(), "null is not a missing value");
        assert!(validate(&schema, &json!([1])).is_err());
    }

    #[test]
    fn arrays_check_length_and_every_item() {
        let schema = json!({"type": "array", "items": {"type": "integer"}, "minItems": 1, "maxItems": 2});
        assert!(validate(&schema, &json!([1])).is_ok());
        assert!(validate(&schema, &json!([])).is_err());
        assert!(validate(&schema, &json!([1, 2, 3])).is_err());
        assert!(problems(schema, json!([1, "x"]))[0].starts_with("$[1]: expected integer"));
    }

    #[test]
    fn positional_items_are_checked_one_by_one() {
        let schema = json!({
            "type": "array",
            "prefixItems": [{"type": "string"}, {"type": ["integer", "null"]}],
            "minItems": 2,
            "maxItems": 2
        });
        assert!(validate(&schema, &json!(["a", 1])).is_ok());
        assert!(validate(&schema, &json!(["a", null])).is_ok());
        assert!(problems(schema.clone(), json!([1, 1]))[0].starts_with("$[0]: expected string"));
        assert!(problems(schema.clone(), json!(["a", "b"]))[0].starts_with("$[1]: expected integer or null"));
        assert!(validate(&schema, &json!(["a"])).is_err());
        assert!(validate(&schema, &json!(["a", 1, 2])).is_err());
        assert!(lint(&schema).is_ok());
        assert!(lint(&json!({"prefixItems": [{"format": "x"}]})).is_err());
    }

    #[test]
    fn a_schema_with_an_unknown_keyword_is_refused_not_ignored() {
        let schema = json!({"type": "string", "pattern": "^a"});
        assert!(problems(schema.clone(), json!("b"))[0].contains("unsupported schema keyword `pattern`"));
        assert!(lint(&schema).is_err());
        assert!(lint(&json!({"type": "string", "description": "fine", "default": "x"})).is_ok());
        let dangling = json!({"type": "object", "properties": {"a": {}}, "required": ["b"]});
        assert!(lint(&dangling).unwrap_err().contains("`b` is required but not a declared property"));
        let nested = json!({"type": "object", "properties": {"a": {"type": "array", "items": {"format": "x"}}}});
        assert!(lint(&nested).unwrap_err().contains("$.a[]"));
    }

    #[test]
    fn error_messages_stay_short_however_large_the_input() {
        let huge = json!("x".repeat(1_000_000));
        let messages = problems(json!({"type": "integer"}), huge);
        assert!(messages[0].len() < 200, "{}", messages[0].len());
        let many: Vec<Value> = (0..100).map(|_| json!("x")).collect();
        let messages = problems(json!({"type": "array", "items": {"type": "integer"}}), Value::Array(many));
        assert_eq!(messages.len(), MAX_ERRORS);
    }
}
