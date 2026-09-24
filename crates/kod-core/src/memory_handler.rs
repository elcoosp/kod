//! `memory://` handler for the internal-URL router (delta §7.5).
//!
//! # Why a separate module
//!
//! The `MemorySaveTool` / `MemorySearchTool` in [`crate::memory_tools`]
//! are the *tool* surface for memory — the model calls them by name.
//! The `memory://` scheme is the *URL* surface: the same data reachable
//! through `read_file`, so a model that is already reading files does
//! not need a second tool to recall something.
//!
//! Both surfaces go through the same `TaskRouter` method
//! (`search_long_term`), so the retrieval semantics are identical.
//! The handler is a thin adapter.
//!
//! # URL shape
//!
//! `memory://search/<query>` — URL-decode `<query>`, run the hybrid
//! retrieval, return the top-k entries as a JSON document. `<query>`
//! may contain `%20` (space) and any other percent-encoded byte.
//!
//! # Not implemented
//!
//! * `memory://<id>` — a direct fetch by id. `TaskRouter` has no
//!   `get_by_id` method (only `search_long_term`), and adding one
//!   would require touching `MemoryManager`'s public API. When the
//!   manager grows a getter, this scheme grows the shape. Documented
//!   here so a future reader does not assume the shape exists.
//! * `memory://recent` — a "list the newest N" shape. The retrieval
//!   layer is query-driven; a recent-list would need a different
//!   manager method. Same deal.
//!
//! # Read-only
//!
//! The handler refuses writes. `memory_save` remains the write path —
//! writes through `read_file`'s sibling `write_file` would be a
//! surprise capability.

use crate::router::TaskRouter;
use kod_tools::internal_url::{
    ProtocolError, ProtocolHandler, ResolveContext, ResolvedResource,
};
use std::sync::Arc;

/// The default number of entries a search returns. Matches the
/// `memory_search` tool's default `k`.
const DEFAULT_K: usize = 5;

/// A `ProtocolHandler` that resolves `memory://search/<query>` by
/// delegating to the router's long-term retrieval.
pub struct MemoryHandler {
    router: Arc<TaskRouter>,
}

impl MemoryHandler {
    pub fn new(router: Arc<TaskRouter>) -> Self {
        Self { router }
    }
}

/// Decode a percent-encoded URL segment. Only `%XX` escapes and `+`
/// (as space) are handled; every other byte passes through. This is
/// the shape a model is likely to produce with a URL-encoded query,
/// not a full RFC 3986 decoder.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[async_trait::async_trait]
impl ProtocolHandler for MemoryHandler {
    fn scheme(&self) -> &'static str {
        "memory"
    }

    async fn resolve(
        &self,
        url: &str,
        _ctx: &ResolveContext,
    ) -> Result<ResolvedResource, ProtocolError> {
        let path = url.strip_prefix("memory://").ok_or_else(|| {
            ProtocolError::Malformed {
                url: url.to_string(),
                reason: "expected `memory://<shape>`".to_string(),
            }
        })?;

        let query = path.strip_prefix("search/").ok_or_else(|| {
            ProtocolError::Malformed {
                url: url.to_string(),
                reason: "expected `memory://search/<query>`; \
                         a direct `memory://<id>` shape is not supported"
                    .to_string(),
            }
        })?;
        if query.is_empty() {
            return Err(ProtocolError::Malformed {
                url: url.to_string(),
                reason: "`memory://search/` requires a query after the slash".to_string(),
            });
        }
        let decoded = percent_decode(query);

        if !self.router.has_memory() {
            return Err(ProtocolError::Handler {
                url: url.to_string(),
                message: "memory is disabled in this session \
                          (RouterConfig::enable_memory = false)"
                    .to_string(),
            });
        }

        let entries = self.router.search_long_term(&decoded, DEFAULT_K).await;
        let json: Vec<serde_json::Value> = entries
            .iter()
            .map(|e| {
                serde_json::json!({
                    "id": format!("{:?}", e.id),
                    "content": e.content,
                    "timestamp": e.timestamp.to_string(),
                    "relevance": e.relevance,
                    "tags": e.metadata.tags,
                    "project_key": e.metadata.project_key,
                    "superseded_by": e.superseded_by.as_ref().map(|id| format!("{id:?}")),
                })
            })
            .collect();
        let text = serde_json::to_string_pretty(&json).unwrap_or_else(|_| "[]".to_string());

        Ok(ResolvedResource {
            text,
            mime: Some("application/json".to_string()),
            immutable: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decode_handles_common_escapes() {
        assert_eq!(percent_decode("hello%20world"), "hello world");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("rust%2Bserde"), "rust+serde");
        assert_eq!(percent_decode("plain"), "plain");
    }

    #[test]
    fn percent_decode_tolerates_a_truncated_escape() {
        // "%2" has no second hex digit; the % passes through.
        assert_eq!(percent_decode("a%2"), "a%2");
        assert_eq!(percent_decode("a%"), "a%");
        // "%ZZ" is not hex; the % passes through.
        assert_eq!(percent_decode("%ZZ"), "%ZZ");
    }

    #[test]
    fn percent_decode_handles_utf8() {
        // %C3%A9 = é (UTF-8 two-byte sequence).
        assert_eq!(percent_decode("caf%C3%A9"), "café");
    }

    #[tokio::test]
    async fn a_bare_memory_url_is_malformed() {
        // Without a router we cannot construct a working handler, but
        // we can exercise the parse path with a stub. Build a handler
        // against a real router that is memory-disabled — the parse
        // error fires before the memory check.
        let tmp = tempfile::TempDir::new().unwrap();
        let router = Arc::new(
            TaskRouter::new(
                crate::router::RouterConfig {
                    embedder: None,
                    skill_threshold: 0.3,
                    context_window: 8192,
                    short_term_capacity: 100,
                    working_dir: tmp.path().to_path_buf(),
                    enable_memory: false,
                    max_skills_per_query: 3,
                },
                tmp.path().join("m.redb"),
            )
            .unwrap(),
        );
        let h = MemoryHandler::new(router);
        let ctx = ResolveContext::new("session", "/tmp");

        let err = h.resolve("memory://", &ctx).await.unwrap_err();
        assert!(matches!(err, ProtocolError::Malformed { .. }));

        let err = h.resolve("memory://foo", &ctx).await.unwrap_err();
        match err {
            ProtocolError::Malformed { reason, .. } => {
                assert!(reason.contains("search/"), "got: {reason}");
            }
            other => panic!("expected Malformed, got {other:?}"),
        }

        let err = h.resolve("memory://search/", &ctx).await.unwrap_err();
        assert!(matches!(err, ProtocolError::Malformed { .. }));
    }

    #[tokio::test]
    async fn search_with_memory_disabled_reports_the_reason() {
        let tmp = tempfile::TempDir::new().unwrap();
        let router = Arc::new(
            TaskRouter::new(
                crate::router::RouterConfig {
                    embedder: None,
                    skill_threshold: 0.3,
                    context_window: 8192,
                    short_term_capacity: 100,
                    working_dir: tmp.path().to_path_buf(),
                    enable_memory: false,
                    max_skills_per_query: 3,
                },
                tmp.path().join("m.redb"),
            )
            .unwrap(),
        );
        let h = MemoryHandler::new(router);
        let ctx = ResolveContext::new("session", "/tmp");
        let err = h.resolve("memory://search/anything", &ctx).await.unwrap_err();
        match err {
            ProtocolError::Handler { message, .. } => {
                assert!(message.contains("disabled"), "got: {message}");
            }
            other => panic!("expected Handler, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn scheme_is_memory() {
        let tmp = tempfile::TempDir::new().unwrap();
        let router = Arc::new(
            TaskRouter::new(
                crate::router::RouterConfig {
                    embedder: None,
                    skill_threshold: 0.3,
                    context_window: 8192,
                    short_term_capacity: 100,
                    working_dir: tmp.path().to_path_buf(),
                    enable_memory: false,
                    max_skills_per_query: 3,
                },
                tmp.path().join("m.redb"),
            )
            .unwrap(),
        );
        let h = MemoryHandler::new(router);
        assert_eq!(h.scheme(), "memory");
    }
}
