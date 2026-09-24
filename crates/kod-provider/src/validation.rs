//! A JSON Schema subset validator.
//!
//! `run_structured` needs to check a model's output against the schema
//! it was asked to satisfy. A full JSON Schema implementation is a
//! large dependency for the handful of keywords kod's schemas use, so
//! this covers exactly those: `type`, `required`, `properties`,
//! `enum`, and the array/string bounds. A schema using a keyword
//! outside the subset validates permissively — the alternative is
//! rejecting a reply for failing a check the validator does not
//! understand, which is worse than not checking.
//!
//! The error messages name the JSON path, so a retry prompt can quote
//! them and the model knows what to fix.

use serde_json::Value;

/// Validate `value` against `schema`. `Err` carries every failure, one
/// per line, each naming its path.
pub fn validate(schema: &Value, value: &Value) -> Result<(), String> {
    let mut errors = Vec::new();
    check(schema, value, "$", &mut errors);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

fn check(schema: &Value, value: &Value, path: &str, errors: &mut Vec<String>) {
    let Some(obj) = schema.as_object() else {
        return;
    };

    // `type`
    if let Some(t) = obj.get("type") {
        let want = match t {
            Value::String(s) => vec![s.as_str()],
            Value::Array(a) => a.iter().filter_map(|v| v.as_str()).collect(),
            _ => Vec::new(),
        };
        if !want.is_empty() && !want.iter().any(|w| type_matches(w, value)) {
            errors.push(format!(
                "{path}: expected {}, got {}",
                want.join(" | "),
                type_name(value),
            ));
            // A wrong type makes every nested check meaningless.
            return;
        }
    }

    // `enum`
    if let Some(Value::Array(allowed)) = obj.get("enum")
        && !allowed.contains(value)
    {
        errors.push(format!("{path}: not one of the allowed values"));
    }

    // `required` is checked independently of `properties`: a schema
    // that names required fields without spelling out their
    // properties is common (`{"type":"object","required":["a"]}`) and
    // skipping its check would let a missing field through.
    if let (Some(Value::Array(req)), Some(v)) = (obj.get("required"), value.as_object()) {
        for r in req.iter().filter_map(|v| v.as_str()) {
            if !v.contains_key(r) {
                errors.push(format!("{path}: missing required field `{r}`"));
            }
        }
    }

    // `properties` recurses into the fields that are present.
    if let Some(Value::Object(props)) = obj.get("properties")
        && let Some(v) = value.as_object()
    {
        for (name, sub) in props {
            if let Some(child) = v.get(name) {
                check(sub, child, &format!("{path}.{name}"), errors);
            }
        }
    }

    // Array item checks.
    if let Some(items) = obj.get("items")
        && let Some(arr) = value.as_array()
    {
        for (i, item) in arr.iter().enumerate() {
            check(items, item, &format!("{path}[{i}]"), errors);
        }
    }

    // String bounds.
    if let Some(s) = value.as_str() {
        if let Some(Value::Number(n)) = obj.get("minLength")
            && let Some(min) = n.as_u64()
            && (s.chars().count() as u64) < min
        {
            errors.push(format!("{path}: shorter than minLength {min}"));
        }
        if let Some(Value::Number(n)) = obj.get("maxLength")
            && let Some(max) = n.as_u64()
            && (s.chars().count() as u64) > max
        {
            errors.push(format!("{path}: longer than maxLength {max}"));
        }
    }

    // Array bounds.
    if let Some(arr) = value.as_array() {
        if let Some(Value::Number(n)) = obj.get("minItems")
            && let Some(min) = n.as_u64()
            && (arr.len() as u64) < min
        {
            errors.push(format!("{path}: fewer than minItems {min}"));
        }
        if let Some(Value::Number(n)) = obj.get("maxItems")
            && let Some(max) = n.as_u64()
            && (arr.len() as u64) > max
        {
            errors.push(format!("{path}: more than maxItems {max}"));
        }
    }
}

fn type_matches(want: &str, value: &Value) -> bool {
    match want {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.is_i64() || value.is_u64(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        // An unknown type name is not a mismatch — see the module doc.
        _ => true,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_conforming_value_passes() {
        let schema = json!({
            "type": "object",
            "required": ["name"],
            "properties": {"name": {"type": "string"}}
        });
        assert!(validate(&schema, &json!({"name": "x"})).is_ok());
    }

    #[test]
    fn a_missing_required_field_names_it() {
        let schema = json!({"type": "object", "required": ["name"]});
        let err = validate(&schema, &json!({})).unwrap_err();
        assert!(err.contains("missing required field `name`"), "got: {err}");
    }

    #[test]
    fn a_wrong_nested_type_names_the_path() {
        let schema = json!({
            "type": "object",
            "properties": {"count": {"type": "integer"}}
        });
        let err = validate(&schema, &json!({"count": "three"})).unwrap_err();
        assert!(err.contains("$.count"), "got: {err}");
        assert!(err.contains("integer"), "got: {err}");
    }

    #[test]
    fn an_enum_violation_is_reported() {
        let schema = json!({"enum": ["a", "b"]});
        assert!(validate(&schema, &json!("c")).is_err());
        assert!(validate(&schema, &json!("a")).is_ok());
    }

    #[test]
    fn array_items_are_checked_per_index() {
        let schema = json!({"type": "array", "items": {"type": "string"}});
        let err = validate(&schema, &json!(["ok", 3])).unwrap_err();
        assert!(err.contains("$[1]"), "got: {err}");
    }

    #[test]
    fn an_unknown_type_keyword_validates_permissively() {
        // A schema using a construct the validator does not know must
        // not reject a reply for it.
        let schema = json!({"type": "object", "patternProperties": {"^x": {"type": "string"}}});
        assert!(validate(&schema, &json!({"x": 1})).is_ok());
    }

    #[test]
    fn string_bounds_are_counted_in_chars_not_bytes() {
        let schema = json!({"type": "string", "minLength": 2});
        assert!(validate(&schema, &json!("éé")).is_ok(), "2 chars, 4 bytes");
        assert!(validate(&schema, &json!("é")).is_err(), "1 char");
    }

    #[test]
    fn every_failure_is_collected() {
        let schema = json!({"type": "object", "required": ["a", "b"]});
        let err = validate(&schema, &json!({})).unwrap_err();
        assert!(err.contains("`a`") && err.contains("`b`"), "got: {err}");
    }
}
