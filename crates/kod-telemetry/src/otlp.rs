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
    attrs.push(int_attr(
        "gen_ai.usage.output_tokens",
        r.completion_tokens,
    ));
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
        assert_eq!(
            get("kod.turn.duration_ms").unwrap()["intValue"],
            "1500",
        );
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
        assert_eq!(get("gen_ai.tool.name").unwrap()["value"]["stringValue"], "read_file");
        assert_eq!(get("kod.tool.status").unwrap()["value"]["stringValue"], "ok");
        assert_eq!(get("kod.tool.duration_ms").unwrap()["value"]["intValue"], "12");
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
        let tool = serde_json::to_string(&tool_payload(
            &cfg(),
            &ToolRecord::default(),
        ))
        .unwrap();
        assert!(tool.contains("resourceLogs"));
    }
}
