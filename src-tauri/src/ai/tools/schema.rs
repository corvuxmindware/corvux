//! Lightweight JSON-schema validation for tool args.
//!
//! Tools advertise an [`input_schema`](super::registry::Tool::input_schema)
//! that the system prompt blurb shows to the LLM. Until now nothing
//! validated the args the LLM actually sent — a `roost_read` call with
//! `path: 123` would propagate the wrong type into the tool, which would
//! either fail loudly inside `execute` or, worse, silently misbehave.
//!
//! This module catches the common shapes early so the LLM gets a
//! structured "missing required field X" / "X must be a string" error
//! it can recover from in one retry.
//!
//! # What we check
//!
//! - The schema's top-level `type` (always `"object"` in this codebase).
//! - `required: [...]` — every named field is present in the args.
//! - For every key the schema declares in `properties`, if the args
//!   carry that key, its JSON type matches `properties.<key>.type`.
//!
//! # What we don't check
//!
//! Everything else: `enum`, `pattern`, numeric ranges, nested objects,
//! `additionalProperties`, `format`. Those don't appear in any tool
//! schema in this codebase today; if they start to, we either extend
//! this module or pull in the `jsonschema` crate. Don't pre-build
//! against schemas we never use.

use serde_json::Value;

/// Validate `args` against `schema`. Returns `Ok(())` if every required
/// field is present and every typed field matches its declared type.
/// Returns `Err(messages)` with one message per violation — the caller
/// joins them into the tool error so the LLM sees every problem in one
/// shot rather than play whack-a-mole across retries.
pub fn validate_args(args: &Value, schema: &Value) -> Result<(), Vec<String>> {
    let mut errors: Vec<String> = Vec::new();

    // Top-level type check. We expect every tool schema to be an object;
    // if a schema author writes something else we surface that as a
    // *fatal* developer error (the LLM can't recover from a malformed
    // schema), but in practice this branch never fires.
    if let Some(t) = schema.get("type").and_then(|v| v.as_str()) {
        if t != "object" {
            errors.push(format!(
                "tool schema declares top-level type `{t}`; expected `object`"
            ));
            return Err(errors);
        }
    }
    if !args.is_object() {
        errors.push(format!(
            "args must be a JSON object, got {}",
            json_type(args)
        ));
        // Append a recovery hint so the LLM sees what the tool actually
        // wants. Without this, small models loop emitting the same
        // intent-only `args: null` call because the bare error tells them
        // nothing about which fields to supply.
        if let Some(hint) = build_args_hint(schema) {
            errors.push(hint);
        }
        return Err(errors);
    }
    let args_obj = args.as_object().expect("checked above");

    // Required fields.
    let mut had_missing_required = false;
    if let Some(required) = schema.get("required").and_then(|v| v.as_array()) {
        for name in required {
            let Some(name) = name.as_str() else {
                continue;
            };
            if !args_obj.contains_key(name) {
                errors.push(format!("missing required field `{name}`"));
                had_missing_required = true;
            }
        }
    }

    // Per-property type check (only for keys actually present in args —
    // optional fields skip silently).
    if let Some(props) = schema.get("properties").and_then(|v| v.as_object()) {
        for (name, prop_schema) in props {
            let Some(actual) = args_obj.get(name) else {
                continue;
            };
            let Some(expected_type) = prop_schema.get("type").and_then(|v| v.as_str()) else {
                continue;
            };
            if !type_matches(actual, expected_type) {
                errors.push(format!(
                    "field `{name}` must be {}, got {}",
                    expected_type,
                    json_type(actual)
                ));
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        // Same reasoning as the !is_object branch: when required fields
        // are missing, hand the LLM a concrete example so it can retry
        // with valid args instead of re-emitting the same broken shape.
        if had_missing_required {
            if let Some(hint) = build_args_hint(schema) {
                errors.push(hint);
            }
        }
        Err(errors)
    }
}

/// Build a recovery hint listing the tool's required fields (with types)
/// and a placeholder example object. Returns None when the schema has no
/// usable `properties` block.
fn build_args_hint(schema: &Value) -> Option<String> {
    let props = schema.get("properties").and_then(|v| v.as_object())?;
    let required: Vec<&str> = schema
        .get("required")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    if required.is_empty() {
        let listed: Vec<String> = props
            .iter()
            .take(6)
            .map(|(name, prop)| {
                let ty = prop
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("string");
                format!("`{name}` ({ty})")
            })
            .collect();
        if listed.is_empty() {
            return None;
        }
        return Some(format!("accepts optional fields: {}", listed.join(", ")));
    }

    let field_list: Vec<String> = required
        .iter()
        .map(|name| {
            let ty = props
                .get(*name)
                .and_then(|p| p.get("type"))
                .and_then(|v| v.as_str())
                .unwrap_or("string");
            format!("`{name}` ({ty})")
        })
        .collect();

    let mut example = serde_json::Map::new();
    for name in &required {
        let ty = props
            .get(*name)
            .and_then(|p| p.get("type"))
            .and_then(|v| v.as_str())
            .unwrap_or("string");
        let placeholder = match ty {
            "string" => Value::String("...".into()),
            "integer" | "number" => Value::Number(serde_json::Number::from(0)),
            "boolean" => Value::Bool(false),
            "array" => Value::Array(vec![]),
            "object" => Value::Object(serde_json::Map::new()),
            _ => Value::Null,
        };
        example.insert((*name).to_string(), placeholder);
    }
    let example_json = serde_json::to_string(&Value::Object(example)).ok()?;

    Some(format!(
        "required fields: {}. example args: {}",
        field_list.join(", "),
        example_json
    ))
}

fn type_matches(value: &Value, expected: &str) -> bool {
    match expected {
        "string" => value.is_string(),
        "number" => value.is_number(),
        // JSON Schema's `integer` matches any number whose fractional part is
        // zero. JSON allows `5.0` to match `integer`; we accept both `5` and
        // `5.0` here for parity.
        "integer" => match value {
            Value::Number(n) => {
                n.as_i64().is_some() || n.as_f64().is_some_and(|f| f.fract() == 0.0)
            }
            _ => false,
        },
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "null" => value.is_null(),
        // Unknown type token in the schema — accept rather than reject so a
        // typo in a tool author's schema doesn't lock out every call.
        _ => true,
    }
}

fn json_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "limit": { "type": "integer" },
                "include_archived": { "type": "boolean" }
            },
            "required": ["path"]
        })
    }

    #[test]
    fn valid_args_pass() {
        let args = json!({ "path": "notes/foo.md" });
        assert!(validate_args(&args, &schema()).is_ok());
    }

    #[test]
    fn valid_args_with_optional_pass() {
        let args = json!({
            "path": "notes/foo.md",
            "limit": 10,
            "include_archived": false
        });
        assert!(validate_args(&args, &schema()).is_ok());
    }

    #[test]
    fn missing_required_field_errors() {
        let args = json!({ "limit": 5 });
        let err = validate_args(&args, &schema()).unwrap_err();
        assert!(err
            .iter()
            .any(|m| m.contains("missing required field `path`")));
    }

    #[test]
    fn wrong_type_errors() {
        let args = json!({ "path": 42 });
        let err = validate_args(&args, &schema()).unwrap_err();
        assert!(err.iter().any(|m| m.contains("`path` must be string")));
    }

    #[test]
    fn integer_accepts_whole_floats() {
        let args = json!({ "path": "x", "limit": 5.0 });
        assert!(validate_args(&args, &schema()).is_ok());
    }

    #[test]
    fn integer_rejects_fractional_floats() {
        let args = json!({ "path": "x", "limit": 5.5 });
        let err = validate_args(&args, &schema()).unwrap_err();
        assert!(err.iter().any(|m| m.contains("`limit` must be integer")));
    }

    #[test]
    fn args_must_be_object() {
        let args = json!("not an object");
        let err = validate_args(&args, &schema()).unwrap_err();
        assert!(err.iter().any(|m| m.contains("must be a JSON object")));
    }

    #[test]
    fn unknown_property_passes() {
        // We don't enforce additionalProperties — extra keys are just
        // ignored by the tool.
        let args = json!({ "path": "x", "completely_made_up": true });
        assert!(validate_args(&args, &schema()).is_ok());
    }

    #[test]
    fn multiple_errors_collect() {
        let args = json!({ "limit": "not a number" });
        let err = validate_args(&args, &schema()).unwrap_err();
        // missing-path + limit-wrong-type + recovery hint (appended
        // because a required field was missing).
        assert_eq!(err.len(), 3, "expected missing-path + limit-type + hint");
        assert!(err
            .iter()
            .any(|m| m.contains("missing required field `path`")));
        assert!(err.iter().any(|m| m.contains("`limit` must be integer")));
        assert!(err.iter().any(|m| m.contains("example args")));
    }

    #[test]
    fn null_args_includes_recovery_hint() {
        let err = validate_args(&Value::Null, &schema()).unwrap_err();
        assert!(err.iter().any(|m| m.contains("must be a JSON object")));
        // Hint must name the required field with its type, plus a
        // placeholder example — this is the whole point of the fix:
        // small models loop on `args: null` without it.
        assert!(err.iter().any(|m| m.contains("`path` (string)")));
        assert!(err.iter().any(|m| m.contains("example args")));
    }

    #[test]
    fn missing_required_field_includes_example() {
        let args = json!({});
        let err = validate_args(&args, &schema()).unwrap_err();
        assert!(err.iter().any(|m| m.contains("example args")));
        // The placeholder object should be valid JSON containing `path`.
        let hint = err.iter().find(|m| m.contains("example args")).unwrap();
        assert!(hint.contains("\"path\""));
    }

    #[test]
    fn type_only_error_skips_example_hint() {
        // When no required field is missing, we don't append the hint
        // (the wrong-type error already names the field — adding the
        // hint here would just bloat the message).
        let args = json!({ "path": "x", "limit": "not a number" });
        let err = validate_args(&args, &schema()).unwrap_err();
        assert!(!err.iter().any(|m| m.contains("example args")));
    }

    #[test]
    fn schema_with_no_required_falls_back_to_optional_list() {
        let s = json!({
            "type": "object",
            "properties": { "q": { "type": "string" }, "k": { "type": "integer" } }
        });
        let err = validate_args(&Value::Null, &s).unwrap_err();
        assert!(err.iter().any(|m| m.contains("optional fields")));
        assert!(err.iter().any(|m| m.contains("`q`")));
    }

    #[test]
    fn schema_without_required_array_passes_when_empty_args() {
        let s = json!({
            "type": "object",
            "properties": { "q": { "type": "string" } }
        });
        assert!(validate_args(&json!({}), &s).is_ok());
    }
}
