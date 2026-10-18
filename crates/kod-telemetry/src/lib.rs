//! OTLP GenAI telemetry (borrow from oh-my-pi, delta §14.5).
//!
//! # What this is
//!
//! Turn-level and tool-level telemetry exported as OTLP/HTTP JSON
//! to a collector the operator names via `OTEL_EXPORTER_OTLP_ENDPOINT`.
//! The wire format is OTLP's `ExportLogsServiceRequest` — the
//! simplest of the three OTLP signals to get exactly right, and
//! sufficient: every field the design names is an attribute on one
//! log record.
//!
//! # Why OTLP/JSON by hand instead of `opentelemetry-rust`
//!
//! The `opentelemetry` crate surface changes on every minor release,
//! pulls in a global tracer-provider singleton, and requires a
//! periodic-exporter task whose shutdown ordering interacts with the
//! engine's. The design wants "env-gated lazy export, flush at turn
//! boundaries" — a shape this crate delivers in a few hundred lines
//! with no global state: one `Telemetry` value, an optional
//! `reqwest::Client`, and a fire-and-forget POST per record.
//!
//! The codebase already prefers this shape: `kod-minimize` rolls its
//! own TOML pipeline engine, `kod-core`'s snapcompact hand-rolls a
//! PNG encoder. A dependency the size of the OTLP SDK is only
//! justified when the ecosystem's other pieces (samplers, processors,
//! propagators) are wanted; here they are not.
//!
//! # Semconv
//!
//! Attribute names follow the OpenTelemetry GenAI semantic
//! conventions (`gen_ai.system`, `gen_ai.request.model`,
//! `gen_ai.usage.input_tokens`, …). A collector that understands the
//! semconv sees a familiar shape; one that does not still sees
//! well-formed OTLP.
//!
//! # Disabled means disabled
//!
//! [`Telemetry::from_env`] returns a disabled instance when
//! `OTEL_EXPORTER_OTLP_ENDPOINT` is unset or empty. Every `record_*`
//! on a disabled instance is a no-op — not "spawn a task and drop
//! the error", an actual return before any allocation. A session
//! that never opted in pays nothing.

pub mod otlp;
pub mod types;

pub use types::{TelemetryConfig, ToolRecord, TurnRecord};

use std::sync::Arc;

/// The live telemetry handle.
///
/// Cloning is cheap (an `Arc` bump); every method is `&self` and
/// fire-and-forget. A `Telemetry` value is either fully configured
/// or disabled; there is no partial state.
/// Cap on concurrent OTLP POSTs. Under a chatty model + slow
/// collector, the pre-fix code spawned one task per record; each
/// task could park for the 5s client timeout, so normal load could
/// accumulate hundreds of concurrent POSTs. Bounded at this cap;
/// payloads beyond it are dropped with a counter increment.
const MAX_CONCURRENT_POSTS: usize = 32;

#[derive(Clone)]
pub struct Telemetry {
    inner: Arc<Inner>,
}

struct Inner {
    config: TelemetryConfig,
    client: reqwest::Client,
    /// The URL each record POSTs to: `<endpoint>/v1/logs`.
    logs_url: String,

    /// Bounds the number of concurrent OTLP POST tasks. Acquired
    /// before `tokio::spawn` so the "check then spawn" race cannot
    /// exceed the cap.
    post_semaphore: std::sync::Arc<tokio::sync::Semaphore>,
    /// Number of OTLP payloads dropped because the semaphore was
    /// full. Read by tests and by a user looking for missing spans.
    dropped_posts: std::sync::atomic::AtomicU64,
}

impl std::fmt::Debug for Telemetry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Telemetry")
            .field("enabled", &self.inner.config.endpoint.is_some())
            .field("service_name", &self.inner.config.service_name)
            .finish()
    }
}

impl Telemetry {
    /// The disabled handle: every `record_*` returns immediately.
    /// This is what a session that never set `OTEL_EXPORTER_OTLP_ENDPOINT`
    /// uses.
    pub fn disabled() -> Self {
        Self {
            inner: Arc::new(Inner {
                post_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(
                    MAX_CONCURRENT_POSTS,
                )),
                dropped_posts: std::sync::atomic::AtomicU64::new(0),
                config: TelemetryConfig {
                    endpoint: None,
                    service_name: "kod".to_string(),
                    headers: Vec::new(),
                },
                client: reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(5))
                    .connect_timeout(std::time::Duration::from_secs(2))
                    .build()
                    .unwrap_or_default(),
                logs_url: String::new(),
            }),
        }
    }

    /// Read the environment and build a handle.
    ///
    /// Recognised variables (the OTLP spec's names):
    ///
    /// * `OTEL_EXPORTER_OTLP_ENDPOINT` — collector base URL. Absent
    ///   or empty means disabled.
    /// * `OTEL_EXPORTER_OTLP_LOGS_ENDPOINT` — overrides the endpoint
    ///   for logs specifically. The design uses logs as the signal
    ///   carrier, so this wins over the base URL when present.
    /// * `OTEL_SERVICE_NAME` — `service.name` resource attribute.
    ///   Defaults to `kod`.
    /// * `OTEL_EXPORTER_OTLP_HEADERS` — comma-separated `key=value`
    ///   pairs sent as HTTP headers on every POST (a collector that
    ///   requires a bearer token).
    ///
    /// A URL that does not parse as an absolute `http(s)` URL
    /// disables the handle with a `tracing::warn!` — a typo must not
    /// silently drop every record.
    pub fn from_env() -> Self {
        let raw = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok();
        let raw = raw.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let Some(base) = raw else {
            return Self::disabled();
        };
        let logs_override = std::env::var("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let service_name = std::env::var("OTEL_SERVICE_NAME")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "kod".to_string());
        let headers = parse_headers(std::env::var("OTEL_EXPORTER_OTLP_HEADERS").ok().as_deref());
        let base = base.trim_end_matches('/');
        let logs_url = logs_override.unwrap_or_else(|| format!("{base}/v1/logs"));
        // Validate the URL now: a bad shape disables rather than
        // failing at every record.
        if reqwest::Url::parse(&logs_url).is_err() {
            tracing::warn!(
                url = %logs_url,
                "OTEL_EXPORTER_OTLP endpoint is not a valid URL; telemetry disabled",
            );
            return Self::disabled();
        }
        Self {
            inner: Arc::new(Inner {
                post_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(
                    MAX_CONCURRENT_POSTS,
                )),
                dropped_posts: std::sync::atomic::AtomicU64::new(0),
                config: TelemetryConfig {
                    endpoint: Some(base.to_string()),
                    service_name,
                    headers,
                },
                // M-6: a timeout — a wedged collector otherwise left
                // each spawned POST parked forever, accumulating
                // tasks/sockets unboundedly.
                client: reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(5))
                    .connect_timeout(std::time::Duration::from_secs(2))
                    .build()
                    .unwrap_or_default(),
                logs_url,
            }),
        }
    }

    /// Whether the handle is enabled.
    pub fn is_enabled(&self) -> bool {
        self.inner.config.endpoint.is_some()
    }

    pub fn config(&self) -> &TelemetryConfig {
        &self.inner.config
    }

    /// Emit one turn-completion record. Fire-and-forget: the POST
    /// runs on a spawned task; a failed POST logs at debug and is
    /// otherwise invisible. A turn must never fail because its
    /// telemetry could not be delivered.
    pub fn record_turn(&self, record: TurnRecord) {
        if !self.is_enabled() {
            return;
        }
        let payload = otlp::turn_payload(&self.inner.config, &record);
        self.spawn_post(payload);
    }

    /// Emit one tool-call record.
    pub fn record_tool(&self, record: ToolRecord) {
        if !self.is_enabled() {
            return;
        }
        let payload = otlp::tool_payload(&self.inner.config, &record);
        self.spawn_post(payload);
    }

    fn spawn_post(&self, payload: serde_json::Value) {
        // Bug-hunt post-v0.1.9 (P2): closes the deferred T5-C26 note.
        // Bound at MAX_CONCURRENT_POSTS: acquire a permit before
        // spawning (so the check-then-spawn race cannot exceed the
        // cap), hold it for the full request, drop the payload when
        // the semaphore is full.
        let Ok(permit) = self.inner.post_semaphore.clone().try_acquire_owned() else {
            self.inner
                .dropped_posts
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::debug!(
                limit = MAX_CONCURRENT_POSTS,
                "OTLP POST dropped: cap reached"
            );
            return;
        };
        let url = self.inner.logs_url.clone();
        let client = self.inner.client.clone();
        let headers = self.inner.config.headers.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let mut req = client
                .post(&url)
                .header("content-type", "application/json")
                .json(&payload);
            for (k, v) in headers {
                req = req.header(k, v);
            }
            match req.send().await {
                Ok(resp) if resp.status().is_success() => {}
                Ok(resp) => {
                    tracing::debug!(
                        status = %resp.status(),
                        "OTLP collector returned a non-success status",
                    );
                }
                Err(e) => {
                    tracing::debug!(error = %e, "OTLP POST failed");
                }
            }
        });
    }
}

/// Parse `key=value,key2=value2` into pairs. Whitespace around keys
/// and values is trimmed; an entry with no `=` is skipped.
fn parse_headers(raw: Option<&str>) -> Vec<(String, String)> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    raw.split(',')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            let k = k.trim();
            let v = v.trim();
            if k.is_empty() {
                None
            } else {
                Some((k.to_string(), v.to_string()))
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_handle_is_disabled() {
        let t = Telemetry::disabled();
        assert!(!t.is_enabled());
    }

    #[test]
    fn disabled_records_are_no_ops() {
        let t = Telemetry::disabled();
        // These would panic if any of them tried to touch a URL.
        t.record_turn(TurnRecord::default());
        t.record_tool(ToolRecord::default());
    }

    #[test]
    fn parse_headers_reads_pairs() {
        let h = parse_headers(Some("authorization=Bearer abc,x-tenant=acme"));
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].0, "authorization");
        assert_eq!(h[0].1, "Bearer abc");
        assert_eq!(h[1].0, "x-tenant");
        assert_eq!(h[1].1, "acme");
    }

    #[test]
    fn parse_headers_skips_an_entry_without_an_equals() {
        let h = parse_headers(Some("good=1,bad,other=2"));
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].0, "good");
        assert_eq!(h[1].0, "other");
    }

    #[test]
    fn parse_headers_of_none_is_empty() {
        assert!(parse_headers(None).is_empty());
    }

    #[test]
    fn parse_headers_trims_whitespace() {
        let h = parse_headers(Some("  a = b  , c=d "));
        assert_eq!(h[0], ("a".to_string(), "b".to_string()));
        assert_eq!(h[1], ("c".to_string(), "d".to_string()));
    }
}

#[cfg(test)]
mod post_semaphore_is_bounded {
    // Bug-hunt post-v0.1.9 (P2): closes T5-C26. spawn_post now
    // acquires a semaphore permit before spawning, holds it for the
    // request, drops the payload when full.
    //
    // The absence check ("T5-C26 comment gone") is scoped to the
    // spawn_post region — an unscoped check would match the string
    // needle in *this test's own source*, which the file read would
    // include. Same self-reference trap fixed in round 76.
    #[test]
    fn post_semaphore_is_bounded() {
        let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs"))
            .expect("read lib.rs");

        assert!(
            src.contains("MAX_CONCURRENT_POSTS: usize"),
            "cap constant must exist"
        );
        assert!(
            src.contains("post_semaphore: std::sync::Arc<tokio::sync::Semaphore>"),
            "struct field must be typed"
        );
        assert!(
            src.contains("dropped_posts: std::sync::atomic::AtomicU64"),
            "drop counter must be typed"
        );
        assert!(
            src.contains("try_acquire_owned"),
            "spawn_post must acquire a permit before spawning"
        );
        assert!(
            src.contains("let _permit = permit"),
            "permit must be held for the full request"
        );

        // Absence check, scoped to the spawn_post body so this test's
        // own needle literal (which lives outside the region) cannot
        // satisfy it.
        let start = src.find("fn spawn_post").expect("spawn_post present");
        let tail = &src[start..];
        let end = tail
            .find("\n    }\n")
            .map(|i| start + i)
            .unwrap_or(src.len());
        let region = &src[start..end];
        assert!(
            !region.contains("no queue cap"),
            "the old 'no queue cap' note must be gone from spawn_post once the fix lands"
        );
    }
}
