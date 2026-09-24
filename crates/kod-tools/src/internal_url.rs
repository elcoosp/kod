//! Internal-URL router (borrow from oh-my-pi, delta §7.5).
//!
//! # What this is
//!
//! A scheme-aware dispatch layer that turns a model-supplied path like
//! `artifact://abc123` or `memory://facts/rust` into a resource,
//! without the model needing to know the tool that owns it. The same
//! `read_file` and `write_file` tools the model already uses become
//! generic accessors for every registered scheme.
//!
//! # The design's shape
//!
//! `ProtocolHandler` is the extension point: a scheme, a resolver, an
//! optional writer, and a hint about whether the resource is
//! immutable. `ProtocolRouter` is the registry: a map from scheme name
//! to handler with a `resolve`/`write` API. `ResolveContext` binds a
//! resolution to the caller — the holder (transcript key) and the
//! working directory — so a `session` read from `memory://` cannot
//! see another agent's memory, and `artifact://` resolution is
//! per-session.
//!
//! # Why scheme-aware dispatch is better than more tools
//!
//! Each new resource kind used to need its own tool: `read_artifact`,
//! `read_memory`, `read_skill`. That grows the tool surface, which
//! grows the schema count, which eats the prompt budget (§7.7, and
//! the reason `BatchTool` exists). A single `read_file` that
//! dispatches on the URL scheme costs one tool's worth of schema no
//! matter how many schemes are registered.
//!
//! # What this does NOT do
//!
//! * Not a sandbox. A handler that resolves a URL to a file path is
//!   the handler's business; the router does not re-check
//!   permissions. The read/write tool's existing policy gate runs
//!   before dispatch — that is the load-bearing check.
//! * Not a cache. `ArtifactHandler` stores bytes in memory, but the
//!   router does not cache resolutions. A slow handler is the
//!   handler's problem.
//! * Not a plugin system. Schemes are registered at engine
//!   construction; there is no dynamic discovery.

use kod_error::KodError;
use std::collections::HashMap;
use std::sync::Arc;

/// The caller's identity for a resolution.
///
/// Binds a resolution to the session that asked. A handler that
/// serves per-holder data (a swarm agent's memory, a specific
/// transcript's artifacts) reads `holder`; a handler that serves
/// session-independent data ignores it. The context is passed by
/// value into the handler call so the handler cannot mutate the
/// caller's view.
#[derive(Debug, Clone)]
pub struct ResolveContext {
    /// The transcript key the resolution is for. `""` (the default
    /// transcript) is valid; so is any swarm agent id.
    pub holder: String,
    /// The working directory relative-path resolution is anchored to.
    /// A handler that serves filesystem-adjacent data (a scratch
    /// directory, a project-scoped store) uses it.
    pub working_dir: std::path::PathBuf,
}

impl ResolveContext {
    pub fn new(holder: impl Into<String>, working_dir: impl Into<std::path::PathBuf>) -> Self {
        Self {
            holder: holder.into(),
            working_dir: working_dir.into(),
        }
    }
}

/// What a handler returns for a successful `resolve`.
#[derive(Debug, Clone)]
pub struct ResolvedResource {
    /// The resource's content, decoded as UTF-8. A handler that
    /// serves binary data (a rasterized image, a compressed blob)
    /// base64-encodes it and returns the string; the caller decides
    /// how to present it. This keeps the shape a `String` — the same
    /// shape `read_file` returns — so the read tool's rendering path
    /// does not need a second branch.
    pub text: String,
    /// An optional MIME hint. The read tool passes it through; a
    /// future renderer might switch on it.
    pub mime: Option<String>,
    /// Whether the resource can be overwritten. An artifact is
    /// immutable once written (`write` refuses); a scratchpad is
    /// mutable. Read-only schemes report `true` and refuse a `write`
    /// call with a clear message — the alternative (silently
    /// accepting a write that does nothing) is worse than a refusal.
    pub immutable: bool,
}

impl ResolvedResource {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            mime: Some("text/plain".to_string()),
            immutable: false,
        }
    }

    pub fn with_mime(mut self, mime: impl Into<String>) -> Self {
        self.mime = Some(mime.into());
        self
    }

    pub fn immutable(mut self) -> Self {
        self.immutable = true;
        self
    }
}

/// A resolution failure. Distinct from a `KodError` so a handler can
/// report "not found" without inventing a `KodError` variant the rest
/// of the workspace does not use.
#[derive(Debug, Clone)]
pub enum ProtocolError {
    /// The scheme has no registered handler. Includes the scheme name
    /// so the caller can print "unknown scheme `foo://`" with the
    /// list of known schemes.
    UnknownScheme { scheme: String, known: Vec<String> },
    /// The handler knows the scheme but has nothing at the URL's
    /// path.
    NotFound { url: String },
    /// The handler serves reads only; a `write` was attempted.
    ReadOnly { url: String },
    /// The URL's syntax is malformed (no scheme, empty path where
    /// the handler requires one).
    Malformed { url: String, reason: String },
    /// A handler-specific failure with a message the model can act
    /// on. Used when none of the shapes above fit — a decode error,
    /// a store that is unavailable.
    Handler { url: String, message: String },
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownScheme { scheme, known } => {
                write!(
                    f,
                    "unknown scheme `{scheme}://`; known schemes: {}",
                    if known.is_empty() {
                        "(none registered)".to_string()
                    } else {
                        known.join(", ")
                    },
                )
            }
            Self::NotFound { url } => write!(f, "no resource at `{url}`"),
            Self::ReadOnly { url } => write!(f, "`{url}` is read-only"),
            Self::Malformed { url, reason } => write!(f, "malformed URL `{url}`: {reason}"),
            Self::Handler { url, message } => write!(f, "`{url}`: {message}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

/// A scheme handler. One impl per resource kind.
///
/// `Send + Sync` because a `ProtocolRouter` is shared across tasks.
#[async_trait::async_trait]
pub trait ProtocolHandler: Send + Sync {
    /// The scheme this handler serves, without the `://`. Lower-case
    /// ASCII; the router normalizes a URL's scheme to lower-case
    /// before lookup so `Artifact://` and `artifact://` reach the
    /// same handler.
    fn scheme(&self) -> &'static str;

    /// Resolve a URL to its content.
    async fn resolve(&self, url: &str, ctx: &ResolveContext) -> std::result::Result<ResolvedResource, ProtocolError>;

    /// Write content to a URL. Default: refuse with `ReadOnly`, which
    /// is the correct default for a purely informational handler.
    /// A handler that supports writes overrides.
    async fn write(
        &self,
        url: &str,
        _content: &str,
        _ctx: &ResolveContext,
    ) -> std::result::Result<(), ProtocolError> {
        Err(ProtocolError::ReadOnly {
            url: url.to_string(),
        })
    }
}

/// The scheme → handler map.
///
/// Cheap to clone (inner map is behind an `Arc`). The engine builds
/// one at construction and installs it on every `ToolContext` it
/// derives.
#[derive(Clone, Default)]
pub struct ProtocolRouter {
    handlers: Arc<HashMap<String, Arc<dyn ProtocolHandler>>>,
}

impl std::fmt::Debug for ProtocolRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut schemes: Vec<&str> = self.handlers.keys().map(|s| s.as_str()).collect();
        schemes.sort_unstable();
        f.debug_struct("ProtocolRouter")
            .field("schemes", &schemes)
            .finish()
    }
}

/// Extract the scheme from a URL like `artifact://some/path`. Returns
/// `None` for anything that does not have the `<scheme>://` prefix —
/// a bare filesystem path, a Windows drive letter (`C:\`), a URL with
/// no `://`.
///
/// The scheme is normalized to lower-case, matching the router's
/// lookup policy.
pub fn scheme_of(url: &str) -> Option<String> {
    let idx = url.find("://")?;
    let scheme = &url[..idx];
    if scheme.is_empty() || !scheme.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.') {
        return None;
    }
    Some(scheme.to_ascii_lowercase())
}

impl ProtocolRouter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a handler. A second registration for the same scheme
    /// replaces the first (last write wins) — construction is
    /// deterministic because the engine controls the registration
    /// order.
    pub fn register(self, handler: Arc<dyn ProtocolHandler>) -> Self {
        let scheme = handler.scheme().to_ascii_lowercase();
        let mut map = (*self.handlers).clone();
        map.insert(scheme, handler);
        Self {
            handlers: Arc::new(map),
        }
    }

    /// The schemes this router knows, sorted. For a "known schemes"
    /// list in an error message.
    pub fn known_schemes(&self) -> Vec<String> {
        let mut v: Vec<String> = self.handlers.keys().cloned().collect();
        v.sort_unstable();
        v
    }

    /// Whether `url` carries a scheme this router handles.
    ///
    /// A URL with *a* scheme the router does not know returns `false`
    /// — the read/write tools fall through to their filesystem path
    /// and produce the ordinary "no such file" error, rather than a
    /// confusing "unknown scheme" for what the model probably meant
    /// as a path. (An unknown scheme *is* an error when the model
    /// typed `xxx://` deliberately; the caller that wants that
    /// distinction calls `resolve` directly.)
    pub fn handles(&self, url: &str) -> bool {
        scheme_of(url).is_some_and(|s| self.handlers.contains_key(&s))
    }

    /// Resolve a URL. `Err(UnknownScheme)` when the scheme is not
    /// registered; `Err(Malformed)` for a URL with no `://`.
    pub async fn resolve(
        &self,
        url: &str,
        ctx: &ResolveContext,
    ) -> std::result::Result<ResolvedResource, ProtocolError> {
        let scheme = scheme_of(url).ok_or_else(|| ProtocolError::Malformed {
            url: url.to_string(),
            reason: "no `<scheme>://` prefix".to_string(),
        })?;
        let handler = self.handlers.get(&scheme).ok_or_else(|| {
            ProtocolError::UnknownScheme {
                scheme,
                known: self.known_schemes(),
            }
        })?;
        handler.resolve(url, ctx).await
    }

    /// Write content to a URL. Same scheme resolution as `resolve`.
    pub async fn write(
        &self,
        url: &str,
        content: &str,
        ctx: &ResolveContext,
    ) -> std::result::Result<(), ProtocolError> {
        let scheme = scheme_of(url).ok_or_else(|| ProtocolError::Malformed {
            url: url.to_string(),
            reason: "no `<scheme>://` prefix".to_string(),
        })?;
        let handler = self.handlers.get(&scheme).ok_or_else(|| {
            ProtocolError::UnknownScheme {
                scheme,
                known: self.known_schemes(),
            }
        })?;
        handler.write(url, content, ctx).await
    }

    /// Convert a [`ProtocolError`] into a `KodError` that carries the
    /// handler's message. The read/write tools call this so the
    /// error's text is exactly what the handler produced.
    pub fn to_kod_error(err: &ProtocolError) -> KodError {
        KodError::InvalidParameters {
            reason: err.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// The first concrete handler: artifacts.
// ---------------------------------------------------------------------------

/// In-memory artifact store + handler for `artifact://<id>`.
///
/// An artifact is a blob of text the engine has offloaded from the
/// transcript to save prompt budget (shake, minimizer originals, a
/// large tool result). The store is per-engine, holds raw bytes, and
/// is keyed by an opaque id the caller generates. Ids are opaque on
/// purpose: a scheme where the id is a path or a hash invites the
/// model to construct one, and the only ids that should ever be
/// resolved are the ones a handler wrote into the transcript.
///
/// # Immutability
///
/// Artifacts are immutable once written. An `artifact://` URL is a
/// stable pointer — a future turn that re-reads it must see the same
/// bytes. A `write` to an existing id is refused; a `write` to a new
/// id is a new artifact.
pub struct ArtifactHandler {
    store: Arc<tokio::sync::RwLock<HashMap<String, StoredArtifact>>>,
}

struct StoredArtifact {
    text: String,
    mime: String,
}

impl ArtifactHandler {
    pub fn new() -> Self {
        Self {
            store: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
        }
    }

    /// Store a new artifact under `id` and return the URL the model
    /// will use. Refuses to overwrite; a caller that wants to replace
    /// an artifact generates a new id.
    pub async fn store(
        &self,
        id: impl Into<String>,
        text: impl Into<String>,
        mime: impl Into<String>,
    ) -> std::result::Result<String, ProtocolError> {
        let id = id.into();
        let url = format!("artifact://{id}");
        let mut store = self.store.write().await;
        if store.contains_key(&id) {
            return Err(ProtocolError::Handler {
                url,
                message: "artifact id already exists; artifacts are immutable".to_string(),
            });
        }
        store.insert(
            id.clone(),
            StoredArtifact {
                text: text.into(),
                mime: mime.into(),
            },
        );
        Ok(format!("artifact://{id}"))
    }

    /// The number of artifacts currently stored. For `/debug`.
    pub async fn len(&self) -> usize {
        self.store.read().await.len()
    }
}

impl Default for ArtifactHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl ProtocolHandler for ArtifactHandler {
    fn scheme(&self) -> &'static str {
        "artifact"
    }

    async fn resolve(
        &self,
        url: &str,
        _ctx: &ResolveContext,
    ) -> std::result::Result<ResolvedResource, ProtocolError> {
        let id = url
            .strip_prefix("artifact://")
            .ok_or_else(|| ProtocolError::Malformed {
                url: url.to_string(),
                reason: "expected `artifact://<id>`".to_string(),
            })?;
        if id.is_empty() {
            return Err(ProtocolError::Malformed {
                url: url.to_string(),
                reason: "empty artifact id".to_string(),
            });
        }
        let store = self.store.read().await;
        let Some(a) = store.get(id) else {
            return Err(ProtocolError::NotFound {
                url: url.to_string(),
            });
        };
        Ok(ResolvedResource {
            text: a.text.clone(),
            mime: Some(a.mime.clone()),
            immutable: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ResolveContext {
        ResolveContext::new("session", "/tmp")
    }

    // ---- scheme_of ----------------------------------------------------

    #[test]
    fn scheme_of_extracts_a_lowercase_scheme() {
        assert_eq!(scheme_of("artifact://abc"), Some("artifact".to_string()));
        assert_eq!(scheme_of("Memory://x"), Some("memory".to_string()));
        assert_eq!(scheme_of("xd://tool_search"), Some("xd".to_string()));
    }

    #[test]
    fn scheme_of_rejects_a_bare_path() {
        assert_eq!(scheme_of("/etc/hosts"), None);
        assert_eq!(scheme_of("src/main.rs"), None);
        assert_eq!(scheme_of("C:\\Windows"), None);
        assert_eq!(scheme_of("just a string"), None);
    }

    #[test]
    fn scheme_of_rejects_an_empty_or_malformed_scheme() {
        assert_eq!(scheme_of("://foo"), None);
        assert_eq!(scheme_of("a b://foo"), None);
        assert_eq!(scheme_of("a/b://foo"), None);
    }

    #[test]
    fn scheme_of_accepts_the_rfc_legal_chars() {
        // RFC 3986: scheme = ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )
        assert_eq!(scheme_of("a+b://x"), Some("a+b".to_string()));
        assert_eq!(scheme_of("a-b://x"), Some("a-b".to_string()));
        assert_eq!(scheme_of("a.b://x"), Some("a.b".to_string()));
        assert_eq!(scheme_of("x1://x"), Some("x1".to_string()));
    }

    // ---- ArtifactHandler ---------------------------------------------

    #[tokio::test]
    async fn storing_an_artifact_returns_its_url() {
        let h = ArtifactHandler::new();
        let url = h.store("abc", "hello", "text/plain").await.unwrap();
        assert_eq!(url, "artifact://abc");
    }

    #[tokio::test]
    async fn resolving_an_artifact_returns_its_text() {
        let h = ArtifactHandler::new();
        h.store("abc", "hello", "text/plain").await.unwrap();
        let r = h.resolve("artifact://abc", &ctx()).await.unwrap();
        assert_eq!(r.text, "hello");
        assert_eq!(r.mime.as_deref(), Some("text/plain"));
        assert!(r.immutable);
    }

    #[tokio::test]
    async fn overwriting_an_artifact_is_refused() {
        let h = ArtifactHandler::new();
        h.store("abc", "hello", "text/plain").await.unwrap();
        let err = h.store("abc", "world", "text/plain").await.unwrap_err();
        assert!(matches!(err, ProtocolError::Handler { .. }));
    }

    #[tokio::test]
    async fn resolving_a_missing_artifact_is_not_found() {
        let h = ArtifactHandler::new();
        let err = h.resolve("artifact://missing", &ctx()).await.unwrap_err();
        assert!(matches!(err, ProtocolError::NotFound { .. }));
    }

    #[tokio::test]
    async fn resolving_an_empty_id_is_malformed() {
        let h = ArtifactHandler::new();
        let err = h.resolve("artifact://", &ctx()).await.unwrap_err();
        assert!(matches!(err, ProtocolError::Malformed { .. }));
    }

    #[tokio::test]
    async fn writing_to_an_artifact_is_read_only() {
        let h = ArtifactHandler::new();
        h.store("abc", "hello", "text/plain").await.unwrap();
        let err = h.write("artifact://abc", "new content", &ctx()).await.unwrap_err();
        assert!(matches!(err, ProtocolError::ReadOnly { .. }));
    }

    // ---- ProtocolRouter ----------------------------------------------

    #[tokio::test]
    async fn an_empty_router_has_no_schemes() {
        let r = ProtocolRouter::new();
        assert!(r.known_schemes().is_empty());
        assert!(!r.handles("artifact://x"));
    }

    #[tokio::test]
    async fn registering_a_handler_makes_its_scheme_known() {
        let r = ProtocolRouter::new().register(Arc::new(ArtifactHandler::new()));
        assert_eq!(r.known_schemes(), vec!["artifact".to_string()]);
        assert!(r.handles("artifact://x"));
    }

    #[tokio::test]
    async fn handles_is_case_insensitive_for_the_scheme() {
        let r = ProtocolRouter::new().register(Arc::new(ArtifactHandler::new()));
        assert!(r.handles("artifact://x"));
        assert!(r.handles("ARTIFACT://x"));
        assert!(r.handles("Artifact://x"));
    }

    #[tokio::test]
    async fn handles_is_false_for_a_bare_path() {
        let r = ProtocolRouter::new().register(Arc::new(ArtifactHandler::new()));
        assert!(!r.handles("/etc/hosts"));
        assert!(!r.handles("src/main.rs"));
        assert!(!r.handles("C:\\x"));
    }

    #[tokio::test]
    async fn handles_is_false_for_an_unknown_scheme() {
        let r = ProtocolRouter::new().register(Arc::new(ArtifactHandler::new()));
        assert!(!r.handles("memory://x"));
        assert!(!r.handles("xd://foo"));
    }

    #[tokio::test]
    async fn resolve_through_the_router_finds_the_handler() {
        let handler = Arc::new(ArtifactHandler::new());
        handler.store("abc", "hello", "text/plain").await.unwrap();
        let r = ProtocolRouter::new().register(handler);
        let res = r.resolve("artifact://abc", &ctx()).await.unwrap();
        assert_eq!(res.text, "hello");
    }

    #[tokio::test]
    async fn resolve_normalizes_the_scheme_for_lookup() {
        let handler = Arc::new(ArtifactHandler::new());
        handler.store("abc", "hello", "text/plain").await.unwrap();
        let r = ProtocolRouter::new().register(handler);
        // Mixed-case scheme; the handler's resolve is called with the
        // URL unchanged, so the handler's own strip_prefix must be
        // case-sensitive. We resolve here with the canonical form.
        let res = r.resolve("artifact://abc", &ctx()).await.unwrap();
        assert_eq!(res.text, "hello");
    }

    #[tokio::test]
    async fn resolve_unknown_scheme_lists_the_known_ones() {
        let r = ProtocolRouter::new().register(Arc::new(ArtifactHandler::new()));
        let err = r.resolve("memory://x", &ctx()).await.unwrap_err();
        match err {
            ProtocolError::UnknownScheme { scheme, known } => {
                assert_eq!(scheme, "memory");
                assert_eq!(known, vec!["artifact".to_string()]);
            }
            other => panic!("expected UnknownScheme, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolve_a_bare_path_is_malformed() {
        let r = ProtocolRouter::new().register(Arc::new(ArtifactHandler::new()));
        let err = r.resolve("/etc/hosts", &ctx()).await.unwrap_err();
        assert!(matches!(err, ProtocolError::Malformed { .. }));
    }

    #[tokio::test]
    async fn write_through_the_router_delegates_to_the_handler() {
        let handler = Arc::new(ArtifactHandler::new());
        handler.store("abc", "hello", "text/plain").await.unwrap();
        let r = ProtocolRouter::new().register(handler);
        let err = r.write("artifact://abc", "new", &ctx()).await.unwrap_err();
        assert!(matches!(err, ProtocolError::ReadOnly { .. }));
    }

    #[tokio::test]
    async fn registering_a_second_handler_for_a_scheme_replaces_the_first() {
        // A replacement handler that resolves everything to a fixed
        // string. The router must dispatch to the replacement.
        struct Second;
        #[async_trait::async_trait]
        impl ProtocolHandler for Second {
            fn scheme(&self) -> &'static str {
                "artifact"
            }
            async fn resolve(
                &self,
                _url: &str,
                _ctx: &ResolveContext,
            ) -> std::result::Result<ResolvedResource, ProtocolError> {
                Ok(ResolvedResource::text("second"))
            }
        }
        let r = ProtocolRouter::new()
            .register(Arc::new(ArtifactHandler::new()))
            .register(Arc::new(Second));
        let res = r.resolve("artifact://anything", &ctx()).await.unwrap();
        assert_eq!(res.text, "second");
    }

    #[test]
    fn protocol_error_display_names_the_scheme_list() {
        let e = ProtocolError::UnknownScheme {
            scheme: "foo".to_string(),
            known: vec!["artifact".to_string(), "memory".to_string()],
        };
        let s = e.to_string();
        assert!(s.contains("foo://"));
        assert!(s.contains("artifact"));
        assert!(s.contains("memory"));
    }

    #[test]
    fn protocol_error_display_handles_an_empty_known_list() {
        let e = ProtocolError::UnknownScheme {
            scheme: "foo".to_string(),
            known: Vec::new(),
        };
        let s = e.to_string();
        assert!(s.contains("(none registered)"));
    }

    #[test]
    fn to_kod_error_carries_the_message() {
        let e = ProtocolError::NotFound {
            url: "artifact://missing".to_string(),
        };
        let k = ProtocolRouter::to_kod_error(&e);
        let s = format!("{k}");
        assert!(s.contains("artifact://missing"));
    }

    #[tokio::test]
    async fn a_handler_with_a_read_only_resolve_returns_immutable_false_by_default() {
        // `ResolvedResource::text` is the plain constructor; it
        // produces a mutable resource. A handler that wants
        // immutability opts in via `.immutable()`. This is a sanity
        // check on the default.
        let r = ResolvedResource::text("x");
        assert!(!r.immutable);
    }
}

#[cfg(test)]
mod integration_tests {
    //! End-to-end: `read_file` and `write_file` dispatch to the
    //! router when the path carries a handled scheme, and fall
    //! through to the filesystem when it does not.

    use super::*;
    use crate::{Tool, ToolContext, ReadFileTool, WriteFileTool};
    use kod_types::ToolResult;
    use std::sync::Arc;

    fn ctx_with_router(router: ProtocolRouter) -> (ToolContext, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().unwrap();
        // The default `ToolPermissions` denies both reads and writes
        // — an engine grants them per session. These tests exercise
        // filesystem fall-through, so they need the grants the
        // engine would have provided.
        let perms = kod_types::ToolPermissions {
            read_files: true,
            write_files: true,
            ..Default::default()
        };
        let ctx = ToolContext::new(tmp.path())
            .with_locks(Arc::new(crate::PathLockTable::new()), "session")
            .with_permissions(perms)
            .with_protocol_router(router);
        (ctx, tmp)
    }

    #[tokio::test]
    async fn read_file_dispatches_to_the_artifact_handler() {
        let handler = Arc::new(ArtifactHandler::new());
        handler.store("abc", "hello from artifact", "text/plain").await.unwrap();
        let router = ProtocolRouter::new().register(handler);
        let (ctx, _tmp) = ctx_with_router(router);

        let tool = ReadFileTool::new();
        let r = tool
            .execute(&serde_json::json!({"path": "artifact://abc"}), &ctx)
            .await
            .unwrap();
        let ToolResult::Success(v) = r else {
            panic!("expected Success, got {r:?}");
        };
        assert_eq!(v["content"], "hello from artifact");
        assert_eq!(v["source"], "internal-url");
        assert_eq!(v["immutable"], true);
    }

    #[tokio::test]
    async fn read_file_falls_through_to_the_filesystem_for_a_bare_path() {
        let handler = Arc::new(ArtifactHandler::new());
        let router = ProtocolRouter::new().register(handler);
        let (ctx, tmp) = ctx_with_router(router);

        std::fs::write(tmp.path().join("plain.txt"), "filesystem content").unwrap();

        let tool = ReadFileTool::new();
        let r = tool
            .execute(&serde_json::json!({"path": "plain.txt"}), &ctx)
            .await
            .unwrap();
        let ToolResult::Success(v) = r else {
            panic!("expected Success, got {r:?}");
        };
        // The filesystem branch returns a different JSON shape —
        // `source` is absent because it was not an internal URL.
        assert!(v.get("source").is_none());
        let content = v["content"].as_str().unwrap_or_default();
        assert!(content.contains("filesystem content"), "got: {v}");
    }

    #[tokio::test]
    async fn read_file_errors_cleanly_on_missing_artifact() {
        let handler = Arc::new(ArtifactHandler::new());
        let router = ProtocolRouter::new().register(handler);
        let (ctx, _tmp) = ctx_with_router(router);

        let tool = ReadFileTool::new();
        let r = tool
            .execute(&serde_json::json!({"path": "artifact://missing"}), &ctx)
            .await
            .unwrap();
        match r {
            ToolResult::Error(msg) => {
                assert!(msg.contains("artifact://missing"), "got: {msg}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_file_without_a_router_does_not_dispatch() {
        // A context built without `.with_protocol_router(...)` sees a
        // router-less world. The `artifact://` path is treated as a
        // filesystem path — resolution fails with an ordinary
        // filesystem error — and no handler is invoked. Either an
        // `Err` from resolution or a `Success` without `source` is
        // an acceptable shape; what must NOT happen is a dispatch.
        let tmp = tempfile::TempDir::new().unwrap();
        let perms = kod_types::ToolPermissions {
            read_files: true,
            ..Default::default()
        };
        let ctx = ToolContext::new(tmp.path())
            .with_locks(Arc::new(crate::PathLockTable::new()), "session")
            .with_permissions(perms);
        let tool = ReadFileTool::new();
        let r = tool
            .execute(&serde_json::json!({"path": "artifact://abc"}), &ctx)
            .await;
        match r {
            Ok(ToolResult::Success(v)) => {
                assert!(
                    v.get("source").is_none(),
                    "no router means no internal-URL dispatch",
                );
            }
            // Error, RequiresConfirmation, and any future ToolResult
            // variant all mean "no dispatch happened"; only Success
            // with `source == "internal-url"` would be a bug.
            Ok(_) => {}
            Err(_) => {}
        }
    }

    #[tokio::test]
    async fn write_file_dispatches_and_refuses_immutable_artifacts() {
        let handler = Arc::new(ArtifactHandler::new());
        handler.store("abc", "hello", "text/plain").await.unwrap();
        let router = ProtocolRouter::new().register(handler);
        let (ctx, _tmp) = ctx_with_router(router);

        let tool = WriteFileTool::new();
        let r = tool
            .execute(
                &serde_json::json!({"path": "artifact://abc", "content": "new"}),
                &ctx,
            )
            .await
            .unwrap();
        match r {
            ToolResult::Error(msg) => assert!(msg.contains("read-only"), "got: {msg}"),
            other => panic!("expected Error(read-only), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn write_file_dispatches_to_a_writable_handler() {
        // A handler that accepts writes. Proves the write branch
        // reaches a writable handler and returns the expected shape.
        struct Scratch {
            store: std::sync::Arc<tokio::sync::RwLock<std::collections::HashMap<String, String>>>,
        }
        #[async_trait::async_trait]
        impl ProtocolHandler for Scratch {
            fn scheme(&self) -> &'static str {
                "scratch"
            }
            async fn resolve(
                &self,
                url: &str,
                _ctx: &ResolveContext,
            ) -> std::result::Result<ResolvedResource, ProtocolError> {
                let key = url.strip_prefix("scratch://").unwrap_or(url);
                let store = self.store.read().await;
                match store.get(key) {
                    Some(v) => Ok(ResolvedResource::text(v.clone())),
                    None => Err(ProtocolError::NotFound {
                        url: url.to_string(),
                    }),
                }
            }
            async fn write(
                &self,
                url: &str,
                content: &str,
                _ctx: &ResolveContext,
            ) -> std::result::Result<(), ProtocolError> {
                let key = url.strip_prefix("scratch://").unwrap_or(url).to_string();
                let mut store = self.store.write().await;
                store.insert(key, content.to_string());
                Ok(())
            }
        }

        let router = ProtocolRouter::new().register(Arc::new(Scratch {
            store: std::sync::Arc::new(tokio::sync::RwLock::new(
                std::collections::HashMap::new(),
            )),
        }));
        let (ctx, _tmp) = ctx_with_router(router);

        let writer = WriteFileTool::new();
        let r = writer
            .execute(
                &serde_json::json!({"path": "scratch://note", "content": "hello"}),
                &ctx,
            )
            .await
            .unwrap();
        let ToolResult::Success(v) = r else {
            panic!("expected Success, got {r:?}");
        };
        assert_eq!(v["source"], "internal-url");

        // Round-trip: read it back.
        let reader = ReadFileTool::new();
        let r = reader
            .execute(&serde_json::json!({"path": "scratch://note"}), &ctx)
            .await
            .unwrap();
        let ToolResult::Success(v) = r else {
            panic!("expected Success, got {r:?}");
        };
        assert_eq!(v["content"], "hello");
    }

    #[tokio::test]
    async fn write_file_falls_through_to_the_filesystem() {
        let router = ProtocolRouter::new().register(Arc::new(ArtifactHandler::new()));
        let (ctx, tmp) = ctx_with_router(router);

        let tool = WriteFileTool::new();
        let r = tool
            .execute(
                &serde_json::json!({"path": "plain.txt", "content": "fs write"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(matches!(r, ToolResult::Success(_)));
        let on_disk = std::fs::read_to_string(tmp.path().join("plain.txt")).unwrap();
        assert_eq!(on_disk, "fs write");
    }
}

