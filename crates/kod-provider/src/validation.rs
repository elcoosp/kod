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

#[cfg(test)]
mod validation_proptests {
    //! Properties for the JSON Schema subset validator.
    //!
    //! The 8 example tests pin specific schemas. The properties
    //! below pin the *contract* the module doc claims:
    //!
    //! * A conforming value passes.
    //! * Every `type` mismatch is rejected with a path.
    //! * `enum`, `required`, `properties`, and `items` reject their
    //!   documented violations.
    //! * Multiple failures are all collected (not just the first).
    //!
    //! The controls are the sharpest pins: **each keyword applies
    //! only to its own type**, per the JSON Schema spec.
    //! `minLength` on a number is a no-op (not a rejection for
    //! being shorter than 5); `required` on a non-object is a
    //! no-op; `items` on a non-array is a no-op; `properties` on a
    //! non-object is a no-op. A hand-rolled validator that
    //! computes "length" of a non-string as 0, or treats `required`
    //! as "the key must exist anywhere", would reject these and
    //! the controls would fail.
    //!
    //! **No bug expected** on the properties. The controls are the
    //! place a bug would show up; if one fails, the fix depends on
    //! the `check` body (which applies the keyword unconditionally
    //! or guards it on the value's type), so the script should stop
    //! before fixing.

    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    // ---- properties: the documented contract ----------------------

    proptest! {
        /// A conforming object passes every keyword the schema
        /// uses. The schema is generated from the value, so it is
        /// conforming by construction; a failure means the
        /// validator rejects its own output.
        #[test]
        fn prop_object_conforms(
            name in "[a-z]{1,10}",
            count in 0u32..=1000,
        ) {
            let value = json!({ "name": name, "count": count });
            let schema = json!({
                "type": "object",
                "required": ["name", "count"],
                "properties": {
                    "name": { "type": "string" },
                    "count": { "type": "integer" }
                }
            });
            prop_assert!(validate(&schema, &value).is_ok());
        }

        /// A wrong nested type is rejected and the error names the
        /// path. The path contains `name` (the offending field).
        #[test]
        fn prop_wrong_nested_type_rejected(count in 0u32..=1000) {
            let value = json!({ "name": count, "count": count });
            let schema = json!({
                "type": "object",
                "properties": { "name": { "type": "string" } }
            });
            let err = validate(&schema, &value).unwrap_err();
            prop_assert!(err.contains("name"), "error {:?} does not name 'name'", err);
        }

        /// An `enum` violation is rejected. The error names the
        /// path (or the allowed values).
        #[test]
        fn prop_enum_violation_rejected(
            allowed in prop::collection::vec("[a-z]{1,4}", 1..=3),
            outside in "[A-Z]{1,4}",
        ) {
            let schema = json!({ "enum": allowed });
            let value = json!(outside);
            prop_assert!(validate(&schema, &value).is_err());
        }

        /// A `required` violation is rejected. The error names the
        /// missing key.
        #[test]
        fn prop_required_violation_rejected(missing in "[a-z]{1,10}") {
            let schema = json!({
                "type": "object",
                "required": [missing.clone()],
            });
            let value = json!({});
            let err = validate(&schema, &value).unwrap_err();
            prop_assert!(err.contains(&missing), "error {:?} does not name {:?}", err, missing);
        }

        /// `items` is checked per index: one bad element fails the
        /// whole array and the error names the index.
        #[test]
        fn prop_items_per_index(
            good in prop::collection::vec(0u32..=100, 1..=5),
            bad_at in 0usize..=4usize,
        ) {
            let mut arr: Vec<serde_json::Value> = good.iter().map(|n| json!(n)).collect();
            if bad_at >= arr.len() {
                return Ok(());
            }
            arr[bad_at] = json!("not a number");
            let schema = json!({
                "type": "array",
                "items": { "type": "integer" }
            });
            prop_assert!(validate(&schema, &json!(arr)).is_err());
        }

        /// Multiple failures are all collected: a value with two
        /// violations yields an error string with at least two
        /// lines.
        #[test]
        fn prop_multiple_errors_collected(_dummy in 0u8..1) {
            let schema = json!({
                "type": "object",
                "required": ["a", "b"],
            });
            let value = json!({});
            let err = validate(&schema, &value).unwrap_err();
            prop_assert!(err.lines().count() >= 2, "only one error line: {:?}", err);
        }
    }

    // ---- controls: keyword applies only to its own type ------------

    /// `minLength` on a number is a no-op. JSON Schema: string
    /// bounds apply only to strings.
    #[test]
    fn control_min_length_on_number_is_noop() {
        let schema = json!({ "minLength": 100 });
        let value = json!(42);
        assert!(
            validate(&schema, &value).is_ok(),
            "minLength on a number should be a no-op, got {:?}",
            validate(&schema, &value),
        );
    }

    /// `minItems` on a string is a no-op.
    #[test]
    fn control_min_items_on_string_is_noop() {
        let schema = json!({ "minItems": 100 });
        let value = json!("hello");
        assert!(
            validate(&schema, &value).is_ok(),
            "minItems on a string should be a no-op, got {:?}",
            validate(&schema, &value),
        );
    }

    /// `required` on a non-object is a no-op. JSON Schema:
    /// `required` applies only to objects.
    #[test]
    fn control_required_on_non_object_is_noop() {
        let schema = json!({ "required": ["x"] });
        let value = json!(42);
        assert!(
            validate(&schema, &value).is_ok(),
            "required on a number should be a no-op, got {:?}",
            validate(&schema, &value),
        );
    }

    /// `properties` on a non-object is a no-op.
    #[test]
    fn control_properties_on_non_object_is_noop() {
        let schema = json!({
            "properties": { "x": { "type": "string" } }
        });
        let value = json!(42);
        assert!(
            validate(&schema, &value).is_ok(),
            "properties on a number should be a no-op, got {:?}",
            validate(&schema, &value),
        );
    }

    /// An unknown keyword is permissive: the module doc says so
    /// explicitly. `oneOf` is not in the subset.
    #[test]
    fn control_unknown_keyword_permissive() {
        let schema = json!({ "oneOf": [{ "type": "string" }] });
        let value = json!(42);
        assert!(
            validate(&schema, &value).is_ok(),
            "unknown keyword should be permissive, got {:?}",
            validate(&schema, &value),
        );
    }
}
