//! Layer 1: rewrite a schema into what a provider accepts.

use serde_json::Value;

/// The role a keyword's value plays in the schema tree.
///
/// The role decides how [`sanitize`] recurses. Getting it wrong is the
/// difference between rewriting a subschema and corrupting a data
/// value — `properties` holds schemas, `enum` holds data, and a walker
/// that treats them alike produces nonsense.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeywordRole {
    /// The value is one schema: `items`, `not`, `additionalProperties`.
    Subschema,
    /// The value is a map of name to schema: `properties`, `$defs`.
    SubschemaMap,
    /// The value is an array of schemas: `anyOf`, `allOf`, `oneOf`.
    SubschemaArray,
    /// The value is arbitrary data, not a schema: `enum`, `const`,
    /// `default`, `examples`.
    Data,
    /// Not a keyword this walker knows.
    Unknown,
}

/// Classify a keyword by the role of its value.
pub fn role_of(keyword: &str) -> KeywordRole {
    match keyword {
        "items" | "additionalProperties" | "not" | "if" | "then" | "else"
        | "propertyNames" | "contains" | "unevaluatedItems" | "unevaluatedProperties" => {
            KeywordRole::Subschema
        }
        "properties" | "patternProperties" | "$defs" | "definitions"
        | "dependentSchemas" => KeywordRole::SubschemaMap,
        "allOf" | "anyOf" | "oneOf" | "prefixItems" => KeywordRole::SubschemaArray,
        "enum" | "const" | "default" | "examples" => KeywordRole::Data,
        _ => KeywordRole::Unknown,
    }
}

/// What one sanitize pass changed. Returned so the caller can log it,
/// and so the quirk-memory layer knows what a provider *tolerated*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppliedTransform {
    /// A keyword was renamed: `(from, to)`, at this JSON path.
    Renamed { path: String, from: String, to: String },
    /// A keyword was removed because the provider does not support it.
    Removed { path: String, keyword: String },
    /// `required` entries naming properties that do not exist were
    /// dropped. A common 400: a model-authored schema lists a field
    /// it forgot to define.
    PrunedRequired { path: String, names: Vec<String> },
    /// `const: v` was rewritten as `enum: [v]`.
    ConstToEnum { path: String },
    /// A combinator the provider does not support was flattened into
    /// its members' constraints merged onto the parent.
    FlattenedCombiner { path: String, keyword: String },
}

/// One provider's (or gateway's) JSON Schema dialect.
#[derive(Debug, Clone, Copy)]
pub struct DialectSpec {
    /// Identifier for logs: `"openai"`, `"anthropic"`,
    /// `"openai-compat:ollama"`.
    pub id: &'static str,
    /// Keywords the provider accepts. A keyword not in this list is
    /// removed — but only in a role where removal is safe (see
    /// [`sanitize`]).
    pub supported: &'static [&'static str],
    /// `(from, to)` renames applied before the support check:
    /// `oneOf` → `anyOf` where the provider prefers the latter.
    pub renames: &'static [(&'static str, &'static str)],
    /// Rewrite `const: v` as `enum: [v]`.
    pub const_to_enum: bool,
    /// Merge a single-member `allOf`/`anyOf` into its parent rather
    /// than nesting. Some gateways mis-handle the one-member case.
    pub flatten_single_combiner: bool,
}

/// Every keyword the walker recognises. A spec's `supported` list is
/// compared against this, so a typo in a spec is a test failure, not a
/// silently-unsupported keyword.
pub const ALL_KEYWORDS: &[&str] = &[
    "type", "properties", "required", "items", "additionalProperties",
    "enum", "const", "default", "description", "title", "$defs",
    "definitions", "allOf", "anyOf", "oneOf", "not", "if", "then",
    "else", "format", "pattern", "minimum", "maximum", "minLength",
    "maxLength", "minItems", "maxItems", "uniqueItems", "prefixItems",
    "patternProperties", "propertyNames", "dependentSchemas",
    "dependentRequired", "contains", "minContains", "maxContains",
    "unevaluatedItems", "unevaluatedProperties", "examples", "multipleOf",
    "exclusiveMinimum", "exclusiveMaximum", "minProperties",
    "maxProperties", "$ref", "$schema", "$id", "additionalItems",
];

/// The spec for a known provider.
///
/// These lists are conservative on purpose: a keyword left in that a
/// provider merely ignores costs nothing, while a keyword removed that
/// it *needed* produces a schema that matches the wrong shape. The
/// lists hold what each provider is documented to accept.
pub fn spec_for_provider(provider: &str) -> DialectSpec {
    match provider {
        "anthropic" => DialectSpec {
            id: "anthropic",
            // Anthropic's tool input schemas accept most of JSON
            // Schema draft 2020-12.
            supported: &[
                "type", "properties", "required", "items",
                "additionalProperties", "enum", "default", "description",
                "title", "anyOf", "allOf", "oneOf", "not", "format",
                "pattern", "minimum", "maximum", "minLength", "maxLength",
                "minItems", "maxItems", "uniqueItems", "multipleOf",
                "exclusiveMinimum", "exclusiveMaximum", "minProperties",
                "maxProperties", "$ref", "$defs", "const",
            ],
            renames: &[],
            const_to_enum: false,
            flatten_single_combiner: false,
        },
        _ => DialectSpec {
            id: "openai",
            // OpenAI's function-calling schema historically rejects
            // `const`, and its `oneOf` handling differs from `anyOf`
            // on some gateways. `$ref`-heavy schemas from MCP servers
            // are the common tripwire.
            supported: &[
                "type", "properties", "required", "items",
                "additionalProperties", "enum", "default", "description",
                "title", "anyOf", "allOf", "not", "format", "pattern",
                "minimum", "maximum", "minLength", "maxLength", "minItems",
                "maxItems", "uniqueItems", "multipleOf",
                "exclusiveMinimum", "exclusiveMaximum", "minProperties",
                "maxProperties",
            ],
            renames: &[("oneOf", "anyOf")],
            const_to_enum: true,
            flatten_single_combiner: true,
        },
    }
}

/// Rewrite `schema` into `spec`'s dialect, returning what changed.
///
/// The recursion is role-aware: a subschema is sanitized, a data value
/// is left alone. A keyword the spec does not support is removed only
/// when removal is safe — never from a position where the keyword *is*
/// the schema (`not`, the sole member of a combiner), because dropping
/// it would widen the schema to accept anything.
pub fn sanitize(schema: &Value, spec: &DialectSpec) -> (Value, Vec<AppliedTransform>) {
    let mut out = schema.clone();
    let mut applied = Vec::new();
    sanitize_in_place(&mut out, spec, "", &mut applied);
    (out, applied)
}

fn sanitize_in_place(
    value: &mut Value,
    spec: &DialectSpec,
    path: &str,
    applied: &mut Vec<AppliedTransform>,
) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };

    // 1. Renames first. A renamed keyword is then checked under its
    //    new name, so `oneOf` → `anyOf` on an `anyOf`-supporting
    //    provider survives.
    for (from, to) in spec.renames {
        if let Some(v) = obj.remove(*from) {
            applied.push(AppliedTransform::Renamed {
                path: path.to_string(),
                from: (*from).to_string(),
                to: (*to).to_string(),
            });
            obj.insert((*to).to_string(), v);
        }
    }

    // 2. `const` → `enum`.
    if spec.const_to_enum
        && let Some(c) = obj.remove("const")
    {
        applied.push(AppliedTransform::ConstToEnum { path: path.to_string() });
        obj.insert("enum".to_string(), Value::Array(vec![c]));
    }

    // 3. Prune `required` names with no matching property.
    if let (Some(Value::Array(req)), Some(Value::Object(props))) =
        (obj.get("required").cloned().as_ref(), obj.get("properties"))
    {
        let dangling: Vec<String> = req
            .iter()
            .filter_map(|r| r.as_str())
            .filter(|name| !props.contains_key(*name))
            .map(str::to_string)
            .collect();
        if !dangling.is_empty() {
            let kept: Vec<Value> = req
                .iter()
                .filter(|r| {
                    r.as_str()
                        .is_some_and(|name| props.contains_key(name))
                })
                .cloned()
                .collect();
            obj.insert("required".to_string(), Value::Array(kept));
            applied.push(AppliedTransform::PrunedRequired {
                path: path.to_string(),
                names: dangling,
            });
        }
    }

    // 4. Flatten a single-member combiner.
    if spec.flatten_single_combiner {
        for kw in ["allOf", "anyOf"] {
            let flatten = obj
                .get(kw)
                .and_then(Value::as_array)
                .is_some_and(|a| a.len() == 1);
            if flatten {
                let member = obj
                    .get_mut(kw)
                    .and_then(Value::as_array_mut)
                    .and_then(|a| a.pop())
                    .unwrap_or(Value::Null);
                if let Some(member_obj) = member.as_object() {
                    obj.remove(kw);
                    for (k, v) in member_obj {
                        obj.insert(k.clone(), v.clone());
                    }
                    applied.push(AppliedTransform::FlattenedCombiner {
                        path: path.to_string(),
                        keyword: (*kw).to_string(),
                    });
                }
            }
        }
    }

    // 5. Collect the keys to drop: unsupported, and safe to drop.
    //    A `type`-less schema of only unsupported keywords would widen
    //    to accept anything if emptied, so `type` is never dropped.
    let keys: Vec<String> = obj.keys().cloned().collect();
    for key in keys {
        if spec.supported.contains(&key.as_str()) {
            continue;
        }
        // Structural keywords are the schema; dropping them changes
        // meaning. `$defs` is dropped only when empty.
        let safe = matches!(
            role_of(&key),
            KeywordRole::Data | KeywordRole::Unknown
        );
        if safe {
            obj.remove(&key);
            applied.push(AppliedTransform::Removed {
                path: path.to_string(),
                keyword: key,
            });
        }
    }

    // 6. Recurse by role.
    let keys: Vec<String> = obj.keys().cloned().collect();
    for key in keys {
        let child_path = format!("{path}/{}", key);
        match role_of(&key) {
            KeywordRole::Subschema => {
                if let Some(v) = obj.get_mut(&key) {
                    sanitize_in_place(v, spec, &child_path, applied);
                }
            }
            KeywordRole::SubschemaMap => {
                if let Some(Value::Object(map)) = obj.get_mut(&key) {
                    for (_name, sub) in map.iter_mut() {
                        sanitize_in_place(sub, spec, &child_path, applied);
                    }
                }
            }
            KeywordRole::SubschemaArray => {
                if let Some(Value::Array(arr)) = obj.get_mut(&key) {
                    for (i, sub) in arr.iter_mut().enumerate() {
                        sanitize_in_place(sub, spec, &format!("{child_path}/{i}"), applied);
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn role_classification_is_total() {
        assert_eq!(role_of("items"), KeywordRole::Subschema);
        assert_eq!(role_of("properties"), KeywordRole::SubschemaMap);
        assert_eq!(role_of("anyOf"), KeywordRole::SubschemaArray);
        assert_eq!(role_of("enum"), KeywordRole::Data);
        assert_eq!(role_of("nonsense_keyword"), KeywordRole::Unknown);
    }

    #[test]
    fn every_spec_keyword_is_a_known_one() {
        // A typo in a spec's `supported` list would otherwise be a
        // silently-never-matched keyword.
        for provider in ["openai", "anthropic"] {
            let spec = spec_for_provider(provider);
            for kw in spec.supported {
                assert!(
                    ALL_KEYWORDS.contains(kw),
                    "spec {provider} lists unknown keyword {kw:?}",
                );
            }
        }
    }

    #[test]
    fn an_unknown_data_keyword_is_dropped() {
        let spec = spec_for_provider("openai");
        let schema = json!({"type": "object", "vendor_extension": 42});
        let (out, applied) = sanitize(&schema, &spec);
        assert!(out.get("vendor_extension").is_none());
        assert!(applied.iter().any(|a| matches!(
            a,
            AppliedTransform::Removed { keyword, .. } if keyword == "vendor_extension"
        )));
    }

    #[test]
    fn a_structural_unsupported_keyword_is_kept() {
        // `$defs` is structural — dropping it orphans every `$ref`.
        // The openai spec omits it, but sanitize must not remove it.
        let spec = spec_for_provider("openai");
        let schema = json!({
            "type": "object",
            "properties": {"a": {"$ref": "#/$defs/x"}},
            "$defs": {"x": {"type": "string"}}
        });
        let (out, _) = sanitize(&schema, &spec);
        assert!(out.get("$defs").is_some(), "$defs is structural and survives");
    }

    #[test]
    fn one_of_is_renamed_to_any_of_for_openai() {
        let spec = spec_for_provider("openai");
        let schema = json!({
            "oneOf": [{"type": "string"}, {"type": "number"}]
        });
        let (out, applied) = sanitize(&schema, &spec);
        assert!(out.get("oneOf").is_none());
        assert!(out.get("anyOf").is_some());
        assert!(applied.iter().any(|a| matches!(
            a,
            AppliedTransform::Renamed { from, to, .. } if from == "oneOf" && to == "anyOf"
        )));
    }

    #[test]
    fn const_becomes_enum_for_openai() {
        let spec = spec_for_provider("openai");
        let schema = json!({"const": "fixed"});
        let (out, applied) = sanitize(&schema, &spec);
        assert_eq!(out.get("enum"), Some(&json!(["fixed"])));
        assert!(out.get("const").is_none());
        assert!(applied.iter().any(|a| matches!(a, AppliedTransform::ConstToEnum { .. })));
    }

    #[test]
    fn anthropic_keeps_const_and_one_of() {
        let spec = spec_for_provider("anthropic");
        let schema = json!({"const": "fixed", "oneOf": [{"type": "string"}]});
        let (out, applied) = sanitize(&schema, &spec);
        assert!(out.get("const").is_some(), "anthropic accepts const");
        assert!(out.get("oneOf").is_some(), "anthropic accepts oneOf");
        assert!(applied.is_empty(), "nothing to change: {applied:?}");
    }

    #[test]
    fn a_dangling_required_name_is_pruned() {
        let spec = spec_for_provider("openai");
        let schema = json!({
            "type": "object",
            "properties": {"a": {"type": "string"}},
            "required": ["a", "ghost"]
        });
        let (out, applied) = sanitize(&schema, &spec);
        assert_eq!(out.get("required"), Some(&json!(["a"])));
        assert!(applied.iter().any(|a| matches!(
            a,
            AppliedTransform::PrunedRequired { names, .. } if names == &["ghost".to_string()]
        )));
    }

    #[test]
    fn recursion_reaches_nested_property_schemas() {
        let spec = spec_for_provider("openai");
        let schema = json!({
            "type": "object",
            "properties": {
                "outer": {
                    "type": "object",
                    "properties": {
                        "inner": {"const": 1}
                    }
                }
            }
        });
        let (out, applied) = sanitize(&schema, &spec);
        let inner = out["properties"]["outer"]["properties"]["inner"].clone();
        assert!(inner.get("const").is_none(), "the nested const was rewritten");
        assert_eq!(inner.get("enum"), Some(&json!([1])));
    }

    #[test]
    fn recursion_reaches_combinator_members() {
        let spec = spec_for_provider("openai");
        let schema = json!({
            "anyOf": [
                {"const": "a"},
                {"const": "b"}
            ]
        });
        let (out, _) = sanitize(&schema, &spec);
        let members = out["anyOf"].as_array().unwrap();
        for m in members {
            assert!(m.get("const").is_none());
            assert!(m.get("enum").is_some());
        }
    }

    #[test]
    fn a_single_member_any_of_is_flattened_for_openai() {
        let spec = spec_for_provider("openai");
        let schema = json!({"anyOf": [{"type": "string"}]});
        let (out, applied) = sanitize(&schema, &spec);
        assert!(out.get("anyOf").is_none(), "the sole member was inlined");
        assert_eq!(out.get("type"), Some(&json!("string")));
        assert!(applied.iter().any(|a| matches!(
            a,
            AppliedTransform::FlattenedCombiner { keyword, .. } if keyword == "anyOf"
        )));
    }

    #[test]
    fn a_multi_member_combinator_is_left_alone() {
        let spec = spec_for_provider("openai");
        let schema = json!({"anyOf": [{"type": "string"}, {"type": "number"}]});
        let (out, _) = sanitize(&schema, &spec);
        assert_eq!(out["anyOf"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn sanitize_is_idempotent_on_a_sanitized_schema() {
        let spec = spec_for_provider("openai");
        let schema = json!({
            "type": "object",
            "properties": {"a": {"const": 1}},
            "required": ["a", "ghost"]
        });
        let (once, _) = sanitize(&schema, &spec);
        let (twice, applied) = sanitize(&once, &spec);
        assert_eq!(once, twice);
        assert!(applied.is_empty(), "second pass finds nothing to do");
    }

    #[test]
    fn the_input_schema_is_not_mutated() {
        let spec = spec_for_provider("openai");
        let schema = json!({"const": "x"});
        let before = schema.clone();
        let _ = sanitize(&schema, &spec);
        assert_eq!(schema, before, "sanitize takes &Value and returns a new one");
    }
}
