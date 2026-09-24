//! The `advise` tool — a watchdog agent's channel into the primary's
//! transcript, policed by the `EmissionGuard` (delta §11.8).
//!
//! # Why this lives in kod-core, not kod-tools
//!
//! The tool needs two engine-owned resources:
//!
//! * the shared [`EmissionGuard`] whose `begin_update` is called once
//!   per turn from the engine's turn loop, and
//! * the steer queue, through which admitted notes reach the primary
//!   at the next round boundary.
//!
//! `kod-tools` is downstream of neither, and cannot reach either. The
//! engine's two existing in-core tool modules — [`crate::memory_tools`]
//! and [`crate::swarm_adapters`] — set the precedent: a tool that needs
//! engine state lives next to the engine.
//!
//! # Delivery, honestly
//!
//! The guard's [`route`] returns one of three channels: `Aside` (a
//! nit, non-interrupting), `Steer` (a concern or blocker, interrupting),
//! `Card` (any severity while the primary is idle — nothing to
//! interrupt). kod's steer mechanism drains at round boundaries, which
//! is *between* streaming turns, not mid-stream; that makes `Aside`
//! and `Steer` reach the model at the same moment even though the
//! design distinguishes them. The distinction is preserved in the
//! rendered text — an aside and a steer have different headers — so
//! the model can weigh them differently and a future pass that adds
//! true mid-stream interruption has the routing decision already made.
//!
//! `Card` reaches the same queue. When the primary is idle there is
//! nothing to interrupt, and the queue is drained the next time a turn
//! starts; from the user's perspective the note appears on the next
//! turn's context, which is what a card means.

use kod_error::Result;
use kod_swarm::advisor::{
    Admission, Advice, Delivery, EmissionGuard, PrimaryState, Severity, route,
};
use kod_tools::{Tool, ToolContext};
use kod_types::{ToolCategory, ToolDefinition, ToolId, ToolPermissions, ToolResult};
use serde_json::Value;
use std::sync::Arc;

/// Where an admitted advice goes. The tool is generic over its sink
/// so a unit test can substitute a stub; production uses
/// [`EngineAdvisorSink`].
#[async_trait::async_trait]
pub trait AdvisorSink: Send + Sync {
    /// The primary's current state, for [`route`]. A sink that cannot
    /// answer (the engine was dropped) reports [`PrimaryState::Terminal`],
    /// which routes every severity to a card — the safe answer when
    /// there is no live target.
    async fn primary_state(&self, target_key: &str) -> PrimaryState;

    /// Deliver one admitted advice.
    async fn deliver(&self, target_key: &str, advice: &Advice, delivery: Delivery);
}

/// The production sink: shares the engine's steer queue and run-state
/// flag by `Arc`, so the tool can deliver notes without holding any
/// reference to the engine itself.
///
/// # Why not `Weak<KodEngine>`
///
/// The tool registry owns the tool; the engine owns the registry. A
/// sink that held a `Weak<KodEngine>` would work (the weak reference
/// breaks the cycle), but it would also drag the whole engine type
/// into the sink, and it would need the engine to expose a
/// `self_weak()` method the workspace does not otherwise use.
///
/// The workspace's own pattern — `build_background_hook`, the
/// `SwarmNoteTool` — is to hand consumers an `Arc` of the specific
/// shared resource they need. The sink's two resources are:
///
/// * the steer queue (`Arc<RwLock<HashMap<...>>>`), which is how a
///   note reaches the primary;
/// * the run-state flag (`Arc<RwLock<bool>>`), which feeds the
///   routing decision.
///
/// Both are leaves — neither refers back to the engine — so no cycle
/// exists and no `Weak` is needed.
pub struct SteerQueueSink {
    pub steers: Arc<tokio::sync::RwLock<std::collections::HashMap<String, Vec<crate::steer::SoftInterrupt>>>>,
    pub is_running: Arc<tokio::sync::RwLock<bool>>,
}

#[async_trait::async_trait]
impl AdvisorSink for SteerQueueSink {
    async fn primary_state(&self, _target_key: &str) -> PrimaryState {
        if *self.is_running.read().await {
            PrimaryState::Streaming
        } else {
            PrimaryState::Idle
        }
    }

    async fn deliver(&self, target_key: &str, advice: &Advice, delivery: Delivery) {
        // The three delivery channels currently share the steer
        // queue; the header distinguishes them for the model. See
        // the module doc's "Delivery, honestly" section.
        let header = match delivery {
            Delivery::Aside => {
                "## Advisor aside (a non-blocking observation; weigh, do not obey)"
            }
            Delivery::Steer => {
                "## Advisor steer (a concern raised mid-work; adjust before continuing)"
            }
            Delivery::Card => "## Advisor note (raised while idle; relevant to the next turn)",
        };
        let body = format!(
            "{header}\n[severity={}] {}",
            advice.severity.as_str(),
            advice.note,
        );
        let interrupt = crate::steer::SoftInterrupt::swarm(body);
        let mut q = self.steers.write().await;
        q.entry(target_key.to_string()).or_default().push(interrupt);
    }
}

/// The `advise` tool.
///
/// A tool call carries one note and one severity; the guard admits or
/// rejects it, and the routing function picks the channel. The result
/// reports both decisions to the model so a rejected call is not
/// retried — a rejection here is the pipeline working, not a failure.
pub struct AdviseTool {
    definition: ToolDefinition,
    guard: Arc<parking_lot::Mutex<EmissionGuard>>,
    sink: Arc<dyn AdvisorSink>,
}

impl AdviseTool {
    pub fn new(
        guard: Arc<parking_lot::Mutex<EmissionGuard>>,
        sink: Arc<dyn AdvisorSink>,
    ) -> Self {
        Self {
            guard,
            sink,
            definition: ToolDefinition {
                trust_level: kod_types::trust::TrustLevel::default(),
                id: ToolId::new(),
                name: "advise".to_string(),
                // The model-facing text is deliberately terse: tool
                // schemas count against the prompt budget, and the
                // 8192-token endpoints the small local models use
                // have almost no headroom. The verbose contract lives
                // in this module's doc comment, not on the wire.
                description: "Raise an observation for the primary. \
                    Noise (`stop`, `lgtm`) and duplicates are dropped."
                    .to_string(),
                category: ToolCategory::System,
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "note": { "type": "string" },
                        "severity": {
                            "type": "string",
                            "enum": ["nit", "concern", "blocker"]
                        }
                    },
                    "required": ["note", "severity"],
                    "additionalProperties": false
                }),
                permissions: ToolPermissions::default(),
            },
        }
    }
}

impl Default for AdviseTool {
    fn default() -> Self {
        // A default constructor exists for tests and for the
        // `Default` convention the workspace's other tools follow.
        // It cannot build a functioning tool without a sink, so it
        // uses a no-op sink that discards deliveries.
        Self::new(
            Arc::new(parking_lot::Mutex::new(EmissionGuard::new())),
            Arc::new(NoopSink),
        )
    }
}

struct NoopSink;

#[async_trait::async_trait]
impl AdvisorSink for NoopSink {
    async fn primary_state(&self, _: &str) -> PrimaryState {
        PrimaryState::Streaming
    }
    async fn deliver(&self, _: &str, _: &Advice, _: Delivery) {}
}

fn parse_severity(s: &str) -> Option<Severity> {
    match s.trim().to_ascii_lowercase().as_str() {
        "nit" => Some(Severity::Nit),
        "concern" => Some(Severity::Concern),
        "blocker" => Some(Severity::Blocker),
        _ => None,
    }
}

fn admission_label(a: &Admission) -> String {
    match a {
        Admission::Accept => "accepted".to_string(),
        Admission::AcceptDisplacing(sev) => {
            format!("accepted-displacing-{}", sev.as_str())
        }
        Admission::RejectEmpty => "rejected:empty".to_string(),
        Admission::RejectNoise => "rejected:noise".to_string(),
        Admission::RejectDuplicate => "rejected:duplicate".to_string(),
        Admission::RejectBudget => "rejected:budget".to_string(),
    }
}

fn delivery_label(d: Delivery) -> &'static str {
    match d {
        Delivery::Aside => "aside",
        Delivery::Steer => "steer",
        Delivery::Card => "card",
    }
}

#[async_trait::async_trait]
impl Tool for AdviseTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, params: &Value, context: &ToolContext) -> Result<ToolResult> {
        let note = match params.get("note").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => {
                return Ok(ToolResult::Error(
                    "advise: `note` is required and must be a string".to_string(),
                ));
            }
        };
        let severity_str = match params.get("severity").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => {
                return Ok(ToolResult::Error(
                    "advise: `severity` is required and must be one of \
                     \"nit\", \"concern\", \"blocker\""
                        .to_string(),
                ));
            }
        };
        let Some(severity) = parse_severity(severity_str) else {
            return Ok(ToolResult::Error(format!(
                "advise: unknown severity {severity_str:?}; expected \
                 \"nit\", \"concern\", or \"blocker\"",
            )));
        };

        let advice = Advice::new(note, severity);

        // Stage 1–4: run the admission pipeline.
        let admission = {
            let mut guard = self.guard.lock();
            guard.admit(&advice)
        };

        if !admission.accepted() {
            // A rejection is the guard working, not a tool failure —
            // return `Success` so the model reads the reason and does
            // not retry the identical note.
            return Ok(ToolResult::Success(serde_json::json!({
                "admission": admission_label(&admission),
            })));
        }

        // Route, then deliver. `primary_state` is a cheap check on the
        // engine's run flag; the sink answers `Terminal` when the
        // engine is gone, which routes every severity to a card.
        let state = self.sink.primary_state(&context.holder).await;
        let delivery = route(&advice, state);
        self.sink
            .deliver(&context.holder, &advice, delivery)
            .await;

        // Free the pending budget slot now that the note is
        // dispatched: the doc's "routed notes can't be displaced"
        // rule. `dispatch` is a no-op for a severity not in pending
        // (blockers are never tracked), so it is safe to call
        // unconditionally.
        {
            let mut guard = self.guard.lock();
            guard.dispatch(&advice);
        }

        Ok(ToolResult::Success(serde_json::json!({
            "admission": admission_label(&admission),
            "delivery": delivery_label(delivery),
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::sync::Mutex as StdMutex;

    /// A sink that records what was delivered. Used to prove the tool
    /// routes and delivers without needing a live engine.
    struct RecordingSink {
        state: PrimaryState,
        deliveries: StdMutex<Vec<(String, Delivery, String)>>,
    }

    impl RecordingSink {
        fn new(state: PrimaryState) -> Self {
            Self {
                state,
                deliveries: StdMutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl AdvisorSink for RecordingSink {
        async fn primary_state(&self, _key: &str) -> PrimaryState {
            self.state
        }
        async fn deliver(&self, key: &str, advice: &Advice, delivery: Delivery) {
            self.deliveries.lock().unwrap().push((
                key.to_string(),
                delivery,
                advice.note.clone(),
            ));
        }
    }

    fn ctx(holder: &str) -> ToolContext {
        ToolContext::new("/tmp").with_locks(
            Arc::new(kod_tools::PathLockTable::new()),
            holder,
        )
    }

    fn build(state: PrimaryState) -> (AdviseTool, Arc<RecordingSink>) {
        let guard = Arc::new(Mutex::new(EmissionGuard::new()));
        let sink = Arc::new(RecordingSink::new(state));
        let tool = AdviseTool::new(guard, sink.clone());
        (tool, sink)
    }

    #[tokio::test]
    async fn a_real_concern_is_delivered_as_a_steer() {
        let (tool, sink) = build(PrimaryState::Streaming);
        let r = tool
            .execute(
                &serde_json::json!({
                    "note": "the parser drops trailing commas on line 42",
                    "severity": "concern",
                }),
                &ctx("session"),
            )
            .await
            .unwrap();
        let ToolResult::Success(v) = r else {
            panic!("expected Success, got {r:?}");
        };
        assert_eq!(v["admission"], "accepted");
        assert_eq!(v["delivery"], "steer");
        let d = sink.deliveries.lock().unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].0, "session");
        assert_eq!(d[0].1, Delivery::Steer);
        assert!(d[0].2.contains("parser drops trailing commas"));
    }

    #[tokio::test]
    async fn a_nit_is_delivered_as_an_aside() {
        let (tool, sink) = build(PrimaryState::Streaming);
        let r = tool
            .execute(
                &serde_json::json!({
                    "note": "consider extracting this helper",
                    "severity": "nit",
                }),
                &ctx("session"),
            )
            .await
            .unwrap();
        let ToolResult::Success(v) = r else {
            panic!("expected Success, got {r:?}");
        };
        assert_eq!(v["delivery"], "aside");
        assert_eq!(sink.deliveries.lock().unwrap()[0].1, Delivery::Aside);
    }

    #[tokio::test]
    async fn a_blocker_while_idle_is_a_card() {
        let (tool, sink) = build(PrimaryState::Idle);
        let r = tool
            .execute(
                &serde_json::json!({
                    "note": "the migration has not run yet",
                    "severity": "blocker",
                }),
                &ctx("session"),
            )
            .await
            .unwrap();
        let ToolResult::Success(v) = r else {
            panic!("expected Success, got {r:?}");
        };
        assert_eq!(v["delivery"], "card");
        assert_eq!(sink.deliveries.lock().unwrap()[0].1, Delivery::Card);
    }

    #[tokio::test]
    async fn a_noise_note_is_rejected_and_never_delivered() {
        let (tool, sink) = build(PrimaryState::Streaming);
        let r = tool
            .execute(
                &serde_json::json!({"note": "Stop.", "severity": "concern"}),
                &ctx("session"),
            )
            .await
            .unwrap();
        let ToolResult::Success(v) = r else {
            panic!("expected Success, got {r:?}");
        };
        assert_eq!(v["admission"], "rejected:noise");
        assert!(sink.deliveries.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_114_stops_incident_yields_zero_deliveries() {
        // The doc's real incident: 114 `Stop.` calls. Every one must
        // be rejected as noise.
        let (tool, sink) = build(PrimaryState::Streaming);
        for _ in 0..114 {
            let _ = tool
                .execute(
                    &serde_json::json!({"note": "Stop.", "severity": "concern"}),
                    &ctx("session"),
                )
                .await
                .unwrap();
        }
        assert!(sink.deliveries.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_duplicate_at_the_same_severity_is_rejected() {
        let (tool, sink) = build(PrimaryState::Streaming);
        let params = serde_json::json!({
            "note": "the migration has not run yet",
            "severity": "concern",
        });
        let _ = tool.execute(&params, &ctx("session")).await.unwrap();
        let r = tool.execute(&params, &ctx("session")).await.unwrap();
        let ToolResult::Success(v) = r else {
            panic!("expected Success, got {r:?}");
        };
        assert_eq!(v["admission"], "rejected:duplicate");
        assert_eq!(sink.deliveries.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_escalation_of_an_existing_key_is_admitted() {
        let (tool, sink) = build(PrimaryState::Streaming);
        let note = "the migration has not run yet";
        let _ = tool
            .execute(
                &serde_json::json!({"note": note, "severity": "nit"}),
                &ctx("session"),
            )
            .await
            .unwrap();
        let _ = tool
            .execute(
                &serde_json::json!({"note": note, "severity": "blocker"}),
                &ctx("session"),
            )
            .await
            .unwrap();
        assert_eq!(sink.deliveries.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_missing_note_is_a_tool_error_not_a_delivery() {
        let (tool, sink) = build(PrimaryState::Streaming);
        let r = tool
            .execute(
                &serde_json::json!({"severity": "concern"}),
                &ctx("session"),
            )
            .await
            .unwrap();
        assert!(matches!(r, ToolResult::Error(_)));
        assert!(sink.deliveries.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_unknown_severity_is_a_tool_error() {
        let (tool, sink) = build(PrimaryState::Streaming);
        let r = tool
            .execute(
                &serde_json::json!({"note": "x", "severity": "blocker!"}),
                &ctx("session"),
            )
            .await
            .unwrap();
        match r {
            ToolResult::Error(msg) => assert!(msg.contains("unknown severity")),
            other => panic!("expected Error, got {other:?}"),
        }
        assert!(sink.deliveries.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_target_key_is_the_context_holder() {
        // The note lands on the transcript that made the call, not
        // on a hardcoded default. Two different holders stay
        // independent.
        let (tool, sink) = build(PrimaryState::Streaming);
        let _ = tool
            .execute(
                &serde_json::json!({"note": "first note", "severity": "concern"}),
                &ctx("alpha"),
            )
            .await
            .unwrap();
        let _ = tool
            .execute(
                &serde_json::json!({"note": "second note", "severity": "concern"}),
                &ctx("beta"),
            )
            .await
            .unwrap();
        let d = sink.deliveries.lock().unwrap();
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].0, "alpha");
        assert_eq!(d[1].0, "beta");
    }

    #[tokio::test]
    async fn a_terminal_primary_routes_everything_to_a_card() {
        let (tool, sink) = build(PrimaryState::Terminal);
        let _ = tool
            .execute(
                &serde_json::json!({"note": "session ended", "severity": "blocker"}),
                &ctx("session"),
            )
            .await
            .unwrap();
        assert_eq!(sink.deliveries.lock().unwrap()[0].1, Delivery::Card);
    }

    #[tokio::test]
    async fn the_default_constructor_builds_a_functioning_tool() {
        // `Default` exists for the workspace's convention. It uses a
        // no-op sink, so nothing lands anywhere — but the call must
        // not panic.
        let tool = AdviseTool::default();
        let r = tool
            .execute(
                &serde_json::json!({"note": "an observation", "severity": "concern"}),
                &ctx("session"),
            )
            .await
            .unwrap();
        assert!(matches!(r, ToolResult::Success(_)));
    }
}
