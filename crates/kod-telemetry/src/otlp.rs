//! The OTLP/HTTP JSON wire shape.
//!
//! One `ResourceLogs` per POST; one `ScopeLogs` for `kod.gen_ai`; one
//! `LogRecord` per `record_*` call. Every field the design names is
//! an attribute on the record body.
//!
//! The shapes are what `protobuf`-JSON mapping produces for
//! `ExportLogsServiceRequest`; see the OTLP spec, "OTLP/HTTP
//! JSON-encoded". Numeric attributes use `intValue` (a string in the
//! JSON form, to preserve 64-bit precision); string attributes use
//! `stringValue`.

use serde_json::{Value, json};

use crate::types::{TelemetryConfig, ToolRecord, TurnRecord};

/// Nano-second Unix time for `now`.
fn now_nanos() -> u128 {
    let now = time::OffsetDateTime::now_utc();
    // `unix_timestamp_nanos` returns i128; the epoch is well past
    // zero for any real clock, so the cast is safe.
    now.unix_timestamp_nanos().max(0) as u128
}

/// The resource block: `service.name` plus any caller-supplied
/// attributes.
fn resource(cfg: &TelemetryConfig) -> Value {
    json!({
        "attributes": [
            {"key": "service.name", "value": {"stringValue": cfg.service_name}},
            {"key": "telemetry.sdk.name", "value": {"stringValue": "kod-telemetry"}},
        ],
    })
}

fn string_attr(key: &str, v: &str) -> Value {
    json!({"key": key, "value": {"stringValue": v}})
}

fn int_attr(key: &str, v: u64) -> Value {
    json!({"key": key, "value": {"intValue": v.to_string()}})
}

fn bool_attr(key: &str, v: bool) -> Value {
    json!({"key": key, "value": {"boolValue": v}})
}

fn scope_logs(record: Value) -> Value {
    json!({
        "scope": {"name": "kod.gen_ai", "version": env!("CARGO_PKG_VERSION")},
        "logRecords": [record],
    })
}

/// One log record wrapping a set of attributes.
fn log_record(time_nanos: u128, body: &str, severity: u8, attrs: Vec<Value>) -> Value {
    json!({
        "timeUnixNano": time_nanos.to_string(),
        "severityNumber": severity,
        "severityText": if severity >= 17 { "ERROR" } else { "INFO" },
        "body": {"stringValue": body},
        "attributes": attrs,
    })
}

/// The full `ExportLogsServiceRequest` for one turn.
pub fn turn_payload(cfg: &TelemetryConfig, r: &TurnRecord) -> Value {
    let mut attrs = Vec::with_capacity(12);
    attrs.push(string_attr("gen_ai.system", &r.system));
    attrs.push(string_attr("gen_ai.request.model", &r.model));
    attrs.push(string_attr("gen_ai.endpoint", &r.endpoint));
    attrs.push(int_attr("gen_ai.usage.input_tokens", r.prompt_tokens));
    attrs.push(int_attr("gen_ai.usage.output_tokens", r.completion_tokens));
    attrs.push(int_attr(
        "gen_ai.usage.cache_read_tokens",
        r.cache_read_tokens,
    ));
    attrs.push(int_attr(
        "gen_ai.usage.cache_creation_tokens",
        r.cache_creation_tokens,
    ));
    attrs.push(int_attr("kod.turn.duration_ms", r.duration_ms));
    if let Some(t) = r.ttft_ms {
        attrs.push(int_attr("gen_ai.server.time_to_first_token_ms", t));
    }
    if let Some(s) = &r.stop_reason {
        attrs.push(string_attr("gen_ai.response.finish_reasons", s));
    }
    attrs.push(bool_attr("kod.turn.error", r.error));

    let severity = if r.error { 17 } else { 9 };
    let body = if r.error {
        "turn completed with error"
    } else {
        "turn completed"
    };
    json!({
        "resourceLogs": [{
            "resource": resource(cfg),
            "scopeLogs": [scope_logs(log_record(now_nanos(), body, severity, attrs))],
        }],
    })
}

/// The full `ExportLogsServiceRequest` for one tool call.
pub fn tool_payload(cfg: &TelemetryConfig, r: &ToolRecord) -> Value {
    let attrs = vec![
        string_attr("gen_ai.tool.name", &r.name),
        string_attr("kod.tool.status", &r.status),
        int_attr("kod.tool.duration_ms", r.duration_ms),
    ];
    let severity = if r.status == "error" || r.status == "aborted" {
        17
    } else {
        9
    };
    json!({
        "resourceLogs": [{
            "resource": resource(cfg),
            "scopeLogs": [scope_logs(log_record(
                now_nanos(),
                "tool call completed",
                severity,
                attrs,
            ))],
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> TelemetryConfig {
        TelemetryConfig {
            endpoint: Some("http://localhost:4318".to_string()),
            service_name: "kod-test".to_string(),
            headers: Vec::new(),
        }
    }

    fn turn() -> TurnRecord {
        TurnRecord {
            system: "anthropic".to_string(),
            model: "claude-sonnet-4".to_string(),
            endpoint: "anthropic".to_string(),
            prompt_tokens: 1234,
            completion_tokens: 56,
            cache_read_tokens: 100,
            cache_creation_tokens: 200,
            ttft_ms: Some(120),
            duration_ms: 1500,
            stop_reason: Some("end_turn".to_string()),
            error: false,
        }
    }

    #[test]
    fn a_turn_payload_has_the_resource_shape() {
        let p = turn_payload(&cfg(), &turn());
        let resource_logs = p["resourceLogs"].as_array().expect("array");
        assert_eq!(resource_logs.len(), 1);
        let resource = &resource_logs[0]["resource"];
        assert_eq!(
            resource["attributes"][0]["value"]["stringValue"],
            "kod-test",
        );
    }

    #[test]
    fn a_turn_payload_carries_the_gen_ai_attributes() {
        let p = turn_payload(&cfg(), &turn());
        let attrs = p["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0]["attributes"]
            .as_array()
            .expect("attributes");
        let get = |k: &str| -> Option<&Value> {
            attrs.iter().find(|a| a["key"] == k).map(|a| &a["value"])
        };
        assert_eq!(get("gen_ai.system").unwrap()["stringValue"], "anthropic");
        assert_eq!(
            get("gen_ai.request.model").unwrap()["stringValue"],
            "claude-sonnet-4",
        );
        assert_eq!(
            get("gen_ai.usage.input_tokens").unwrap()["intValue"],
            "1234",
        );
        assert_eq!(
            get("gen_ai.usage.cache_read_tokens").unwrap()["intValue"],
            "100",
        );
        assert_eq!(get("kod.turn.duration_ms").unwrap()["intValue"], "1500",);
    }

    #[test]
    fn the_ttft_attribute_is_present_only_when_set() {
        let p = turn_payload(&cfg(), &turn());
        let attrs = p["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0]["attributes"]
            .as_array()
            .unwrap()
            .clone();
        assert!(
            attrs
                .iter()
                .any(|a| a["key"] == "gen_ai.server.time_to_first_token_ms"),
        );

        let mut no_ttft = turn();
        no_ttft.ttft_ms = None;
        let p = turn_payload(&cfg(), &no_ttft);
        let attrs = p["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0]["attributes"]
            .as_array()
            .unwrap();
        assert!(
            !attrs
                .iter()
                .any(|a| a["key"] == "gen_ai.server.time_to_first_token_ms"),
        );
    }

    #[test]
    fn an_error_turn_is_marked_at_error_severity() {
        let mut e = turn();
        e.error = true;
        let p = turn_payload(&cfg(), &e);
        let rec = &p["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0];
        assert_eq!(rec["severityNumber"], 17);
        assert_eq!(rec["severityText"], "ERROR");
        assert_eq!(rec["body"]["stringValue"], "turn completed with error");
    }

    #[test]
    fn a_tool_payload_has_the_tool_attributes() {
        let r = ToolRecord {
            name: "read_file".to_string(),
            status: "ok".to_string(),
            duration_ms: 12,
        };
        let p = tool_payload(&cfg(), &r);
        let attrs = p["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0]["attributes"]
            .as_array()
            .unwrap()
            .clone();
        let get = |k: &str| attrs.iter().find(|a| a["key"] == k).cloned();
        assert_eq!(
            get("gen_ai.tool.name").unwrap()["value"]["stringValue"],
            "read_file"
        );
        assert_eq!(
            get("kod.tool.status").unwrap()["value"]["stringValue"],
            "ok"
        );
        assert_eq!(
            get("kod.tool.duration_ms").unwrap()["value"]["intValue"],
            "12"
        );
    }

    #[test]
    fn a_failed_tool_is_error_severity() {
        let r = ToolRecord {
            name: "execute_command".to_string(),
            status: "error".to_string(),
            duration_ms: 5,
        };
        let p = tool_payload(&cfg(), &r);
        let rec = &p["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0];
        assert_eq!(rec["severityNumber"], 17);
    }

    #[test]
    fn every_payload_is_serialisable_to_json_text() {
        // A `json!` that fails to serialize would be a shape bug; the
        // round-trip proves the value is well-formed.
        let t = serde_json::to_string(&turn_payload(&cfg(), &turn())).unwrap();
        assert!(t.contains("resourceLogs"));
        let tool = serde_json::to_string(&tool_payload(&cfg(), &ToolRecord::default())).unwrap();
        assert!(tool.contains("resourceLogs"));
    }
}

#[cfg(test)]
mod otlp_proptests {
    //! Properties for `turn_payload` and `tool_payload`.
    //!
    //! Both functions build a nested OTLP-shaped `serde_json::Value`
    //! from a config and a record, with `now_nanos()` embedded at
    //! the log-record level. That timestamp makes two calls
    //! trivially unequal, so the determinism properties normalize
    //! every `"timeUnixNano"` key to `"0"` before comparing.
    //!
    //! The example tests use `.any()`-style "contains at least this
    //! attribute" checks. The properties here use **exact key sets**,
    //! which is a strictly stronger check: it catches a copy-paste
    //! bug where a field is emitted twice or not at all, both of
    //! which pass a "contains" test.
    //!
    //! Helpers (`collect_attribute_keys`, `attr_value`,
    //! `normalize_timestamps`) walk the tree rather than assert a
    //! specific nesting. `scope_logs` and `log_record` are
    //! file-private; if their shape changes, these properties follow
    //! without edits. `collect_attribute_keys` deliberately walks
    //! only `logRecords` entries — the resource block's
    //! `service.name` has its own property.

    use super::*;
    use proptest::prelude::*;
    use serde_json::Value;
    use std::collections::BTreeSet;

    // ---- tree-walking helpers -------------------------------------------

    /// Every attribute key inside a `logRecords` entry anywhere in
    /// the tree. The `resource` block's `service.name` is deliberately
    /// **excluded** — the exact-set properties below are about the
    /// log record's attributes, and `service.name` has its own
    /// property (`prop_service_name_is_carried`). Walking the whole
    /// tree over-counts: a populated turn has 11 log-record
    /// attributes and 1 resource attribute, not 11 total.
    fn collect_attribute_keys(v: &Value, out: &mut BTreeSet<String>) {
        match v {
            Value::Object(map) => {
                if let Some(records) = map.get("logRecords").and_then(|lr| lr.as_array()) {
                    for record in records {
                        if let Some(arr) = record.get("attributes").and_then(|a| a.as_array()) {
                            for attr in arr {
                                if let Some(k) = attr.get("key").and_then(|k| k.as_str()) {
                                    out.insert(k.to_string());
                                }
                            }
                        }
                    }
                }
                for v in map.values() {
                    collect_attribute_keys(v, out);
                }
            }
            Value::Array(arr) => {
                for v in arr {
                    collect_attribute_keys(v, out);
                }
            }
            _ => {}
        }
    }

    /// Replace every `"timeUnixNano"` value with `"0"` so two calls
    /// made at different wall-clock instants compare equal.
    fn normalize_timestamps(v: &mut Value) {
        match v {
            Value::Object(map) => {
                for (k, val) in map.iter_mut() {
                    if k == "timeUnixNano" {
                        *val = Value::String("0".to_string());
                    } else {
                        normalize_timestamps(val);
                    }
                }
            }
            Value::Array(arr) => {
                for v in arr {
                    normalize_timestamps(v);
                }
            }
            _ => {}
        }
    }

    /// Find the first attribute value for `key` anywhere in the tree.
    /// Used to pin a specific field's value without asserting a path.
    fn attr_value<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
        match v {
            Value::Object(map) => {
                if let Some(arr) = map.get("attributes").and_then(|a| a.as_array()) {
                    for attr in arr {
                        if attr.get("key").and_then(|k| k.as_str()) == Some(key) {
                            return attr.get("value");
                        }
                    }
                }
                map.values().find_map(|v| attr_value(v, key))
            }
            Value::Array(arr) => arr.iter().find_map(|v| attr_value(v, key)),
            _ => None,
        }
    }

    /// Find the first `severityNumber` in the tree.
    fn severity_number(v: &Value) -> Option<u64> {
        match v {
            Value::Object(map) => {
                if let Some(n) = map.get("severityNumber").and_then(|n| n.as_u64()) {
                    return Some(n);
                }
                map.values().find_map(severity_number)
            }
            Value::Array(arr) => arr.iter().find_map(severity_number),
            _ => None,
        }
    }

    // ---- generators ----------------------------------------------------

    fn arb_cfg() -> impl Strategy<Value = TelemetryConfig> {
        (
            "[a-z][a-z0-9.-]{0,20}",
            prop::option::of("[a-z][a-z0-9.-]{0,20}"),
        )
            .prop_map(|(service_name, endpoint)| TelemetryConfig {
                endpoint,
                service_name,
                headers: Vec::new(),
            })
    }

    fn arb_turn() -> impl Strategy<Value = TurnRecord> {
        (
            "[a-z][a-z0-9-]{0,20}",
            "[a-z][a-z0-9.-]{0,20}",
            "[a-z][a-z0-9-]{0,20}",
            any::<u64>(),
            any::<u64>(),
            any::<u64>(),
            any::<u64>(),
            prop::option::of(any::<u64>()),
            any::<u64>(),
            prop::option::of("[a-z_]{0,20}"),
            any::<bool>(),
        )
            .prop_map(
                |(
                    system,
                    model,
                    endpoint,
                    prompt_tokens,
                    completion_tokens,
                    cache_read_tokens,
                    cache_creation_tokens,
                    ttft_ms,
                    duration_ms,
                    stop_reason,
                    error,
                )| TurnRecord {
                    system,
                    model,
                    endpoint,
                    prompt_tokens,
                    completion_tokens,
                    cache_read_tokens,
                    cache_creation_tokens,
                    ttft_ms,
                    duration_ms,
                    stop_reason,
                    error,
                },
            )
    }

    fn arb_tool() -> impl Strategy<Value = ToolRecord> {
        (
            "[a-z_]{1,20}",
            prop::sample::select(vec![
                "ok".to_string(),
                "error".to_string(),
                "skipped".to_string(),
                "aborted".to_string(),
            ]),
            any::<u64>(),
        )
            .prop_map(|(name, status, duration_ms)| ToolRecord {
                name,
                status,
                duration_ms,
            })
    }

    // ---- properties ----------------------------------------------------

    proptest! {
        /// `turn_payload` is deterministic modulo the embedded
        /// timestamp. Two calls with the same inputs produce the
        /// same JSON after `timeUnixNano` is zeroed.
        #[test]
        fn prop_turn_deterministic(cfg in arb_cfg(), r in arb_turn()) {
            let mut a = turn_payload(&cfg, &r);
            let mut b = turn_payload(&cfg, &r);
            normalize_timestamps(&mut a);
            normalize_timestamps(&mut b);
            prop_assert_eq!(a, b);
        }

        /// The attribute key set is exactly the expected set:
        /// nine fixed keys, plus `time_to_first_token_ms` if the
        /// record has `ttft_ms`, plus `finish_reasons` if it has
        /// `stop_reason`. No extra keys, no missing keys.
        ///
        /// This is the property that would catch a copy-paste bug
        /// (a field emitted twice) or a forgotten field (a new
        /// `TurnRecord` field never added to the payload).
        #[test]
        fn prop_turn_attribute_key_set_exact(cfg in arb_cfg(), r in arb_turn()) {
            let payload = turn_payload(&cfg, &r);
            let mut keys = BTreeSet::new();
            collect_attribute_keys(&payload, &mut keys);

            let mut expected: BTreeSet<String> = [
                "gen_ai.system",
                "gen_ai.request.model",
                "gen_ai.endpoint",
                "gen_ai.usage.input_tokens",
                "gen_ai.usage.output_tokens",
                "gen_ai.usage.cache_read_tokens",
                "gen_ai.usage.cache_creation_tokens",
                "kod.turn.duration_ms",
                "kod.turn.error",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect();
            if r.ttft_ms.is_some() {
                expected.insert("gen_ai.server.time_to_first_token_ms".to_string());
            }
            if r.stop_reason.is_some() {
                expected.insert("gen_ai.response.finish_reasons".to_string());
            }

            prop_assert_eq!(keys, expected);
        }

        /// `ttft_ms: Some(x)` and the `time_to_first_token_ms`
        /// attribute agree: present iff `Some`, value matches `x`.
        /// `ttft_ms: None` means the attribute is absent.
        #[test]
        fn prop_ttft_presence_matches_option(cfg in arb_cfg(), r in arb_turn()) {
            let payload = turn_payload(&cfg, &r);
            let key = "gen_ai.server.time_to_first_token_ms";
            match r.ttft_ms {
                Some(x) => {
                    let v = attr_value(&payload, key)
                        .expect("ttft_ms is Some but attribute absent");
                    // OTLP JSON encodes int64 as a *string* in
                    // `intValue` because JSON numbers lose
                    // precision above 2^53. Handle bare number,
                    // intValue-as-number, and intValue-as-string.
                    let as_u64 = v
                        .as_u64()
                        .or_else(|| v.get("intValue").and_then(|iv| iv.as_u64()))
                        .or_else(|| {
                            v.get("intValue")
                                .and_then(|iv| iv.as_str())
                                .and_then(|s| s.parse::<u64>().ok())
                        })
                        .expect("attribute value not an integer");
                    prop_assert_eq!(as_u64, x);
                }
                None => {
                    prop_assert!(
                        attr_value(&payload, key).is_none(),
                        "ttft_ms is None but attribute present",
                    );
                }
            }
        }

        /// Error severity: `error == true` maps to severity 17
        /// (`ERROR`), `false` to 9 (`INFO`).
        #[test]
        fn prop_turn_severity_reflects_error(cfg in arb_cfg(), r in arb_turn()) {
            let payload = turn_payload(&cfg, &r);
            let s = severity_number(&payload).expect("no severityNumber in payload");
            let expected = if r.error { 17 } else { 9 };
            prop_assert_eq!(s, expected);
        }

        /// `tool_payload` is deterministic modulo the timestamp.
        #[test]
        fn prop_tool_deterministic(cfg in arb_cfg(), r in arb_tool()) {
            let mut a = tool_payload(&cfg, &r);
            let mut b = tool_payload(&cfg, &r);
            normalize_timestamps(&mut a);
            normalize_timestamps(&mut b);
            prop_assert_eq!(a, b);
        }

        /// The tool attribute key set is exactly three keys — name,
        /// status, duration. No extra, no missing.
        #[test]
        fn prop_tool_attribute_key_set_exact(cfg in arb_cfg(), r in arb_tool()) {
            let payload = tool_payload(&cfg, &r);
            let mut keys = BTreeSet::new();
            collect_attribute_keys(&payload, &mut keys);

            let expected: BTreeSet<String> = ["gen_ai.tool.name", "kod.tool.status", "kod.tool.duration_ms"]
                .iter()
                .map(|s| s.to_string())
                .collect();
            prop_assert_eq!(keys, expected);
        }

        /// Tool severity: `status in {"error", "aborted"}` -> 17,
        /// everything else -> 9. Pins the current status list; a
        /// caller introducing a new error-class status without
        /// adding it here is a visible decision, not a silent
        /// behaviour change.
        #[test]
        fn prop_tool_severity_reflects_status(cfg in arb_cfg(), r in arb_tool()) {
            let payload = tool_payload(&cfg, &r);
            let s = severity_number(&payload).expect("no severityNumber in payload");
            let expected = if r.status == "error" || r.status == "aborted" { 17 } else { 9 };
            prop_assert_eq!(s, expected, "status {:?}", r.status);
        }

        /// `service_name` is carried through to the resource block.
        /// Same service name in, same service name out — the whole
        /// point of `TelemetryConfig::service_name`.
        #[test]
        fn prop_service_name_is_carried(
            service_name in "[a-zA-Z][a-zA-Z0-9.-]{0,20}",
            r in arb_turn(),
        ) {
            let cfg = TelemetryConfig {
                endpoint: None,
                service_name: service_name.clone(),
                headers: Vec::new(),
            };
            let payload = turn_payload(&cfg, &r);
            let v = attr_value(&payload, "service.name")
                .expect("service.name attribute absent from resource");
            let s = v.as_str()
                .or_else(|| v.get("stringValue").and_then(|sv| sv.as_str()))
                .expect("service.name not a string");
            prop_assert_eq!(s, service_name.as_str());
        }
    }

    /// Control (L43): the populated-turn case has all eleven
    /// attribute keys, so the exact-set property above is not
    /// checking an empty set.
    #[test]
    fn control_populated_turn_has_full_attribute_set() {
        let cfg = TelemetryConfig {
            endpoint: None,
            service_name: "test".to_string(),
            headers: Vec::new(),
        };
        let r = TurnRecord {
            system: "anthropic".to_string(),
            model: "claude-3-5-sonnet".to_string(),
            endpoint: "default".to_string(),
            prompt_tokens: 100,
            completion_tokens: 50,
            cache_read_tokens: 20,
            cache_creation_tokens: 0,
            ttft_ms: Some(150),
            duration_ms: 1200,
            stop_reason: Some("end_turn".to_string()),
            error: false,
        };
        let payload = turn_payload(&cfg, &r);
        let mut keys = BTreeSet::new();
        collect_attribute_keys(&payload, &mut keys);
        assert_eq!(keys.len(), 11, "expected 11 attributes, got {:?}", keys);
    }

    /// Control: the tool payload always has exactly three
    /// attributes. If a future change adds a conditional attribute,
    /// this fails and the exact-set property above becomes a
    /// decision point.
    #[test]
    fn control_populated_tool_has_three_attributes() {
        let cfg = TelemetryConfig {
            endpoint: None,
            service_name: "test".to_string(),
            headers: Vec::new(),
        };
        let r = ToolRecord {
            name: "read_file".to_string(),
            status: "ok".to_string(),
            duration_ms: 42,
        };
        let payload = tool_payload(&cfg, &r);
        let mut keys = BTreeSet::new();
        collect_attribute_keys(&payload, &mut keys);
        assert_eq!(keys.len(), 3, "expected 3 attributes, got {:?}", keys);
    }
}
