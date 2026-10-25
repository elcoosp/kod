//! Web tools: `web_fetch`.
//!
//! The agent already had network access *through* `execute_command` (a
//! `curl` or `wget`), but that path produces no structured output, no
//! HTML-to-text extraction, and no defence against the URL pointing at
//! `localhost`, a private RFC1918 address, or the cloud metadata
//! endpoint. This tool exists to make "read the docs at this URL" a
//! first-class, safe operation.
//!
//! # Safety
//!
//! - Gated by the `network_access` permission flag. The default
//!   `ToolContext` has it `false`, so a fresh engine cannot fetch
//!   anything without the operator opting in.
//! - Refuses `localhost`, `127.0.0.0/8`, `::1`, `10.0.0.0/8`,
//!   `172.16.0.0/12`, `192.168.0.0/16`, `169.254.0.0/16` (link-local,
//!   where cloud metadata lives), and `.local` mDNS names. The check
//!   runs on the host the URL resolves to, not only on the literal
//!   text: `http://127.0.0.1` and `http://localhost` both fail, as
//!   does `http://0x7f000001`.
//! - HTTP only, `GET` only. No redirect chain beyond what the HTTP
//!   client follows (see `reqwest`'s default 10-hop limit).
//! - Response body is capped at [`MAX_BODY_BYTES`]. A stream that
//!   would exceed it is truncated, and the result flags that.
//! - Response `Content-Type` is checked: HTML and JSON are decoded as
//!   text; anything else (image, octet-stream, video) returns an error
//!   naming the type rather than dumping bytes into the model.

use crate::{Tool, ToolContext};
use kod_error::{KodError, Result};
use kod_types::{ToolCategory, ToolDefinition, ToolId, ToolPermissions, ToolResult};
use serde_json::Value;

/// Byte cap on the fetched body. 256 KB covers a normal documentation
/// page and bounds the worst case.
const MAX_BODY_BYTES: usize = 256 * 1024;

/// T1-C15: cap on manual redirect hops. Every hop is validated and
/// re-pinned, so this is a bound on work, not a trust decision.
const MAX_REDIRECTS: usize = 5;

/// Per-request wall-clock timeout. Longer than the tool timeout default
/// (30s) because a cold TLS handshake over a slow link can be slow, but
/// short enough that a hung server fails fast.
const REQUEST_TIMEOUT_SECS: u64 = 30;

/// What one attempt at the fetch produced. `Done` means the fetch
/// succeeded and the loop is over; `Retryable` means the server
/// declined and `web_retry` decides what to do next.
pub(crate) enum FetchStep<R> {
    Done(R),
    Retryable {
        status: u16,
        retry_after_secs: Option<u64>,
        body_preview: String,
    },
}

/// Drive `fetch` per `web_retry`'s policy (delta §14.5).
///
/// Each iteration calls `fetch(attempt, user_agent)`; a `Done` returns
/// its payload, and a `Retryable` consults
/// [`crate::web_retry::decide_with_retry_after`] — a 429 retries once
/// with the same UA (honoring `Retry-After`), a bot wall rotates the
/// UA up to [`crate::web_retry::MAX_UA_ROTATIONS`] times, and anything
/// else surfaces the error.
///
/// **Termination.** `attempt` only grows. `RetrySameAgent` requires
/// `attempt == 0` and `RetryNextAgent` requires
/// `attempt < MAX_UA_ROTATIONS`, so the loop is bounded by
/// `MAX_UA_ROTATIONS + 1` iterations, whatever the fetch returns.
///
/// # Why the helper exists
///
/// The loop is the only place `web_retry` is *used*. Extracting it
/// from `execute` lets a test drive the retry policy with a synthetic
/// fetcher — no DNS, no SSRF filter, no network — and pin the wiring
/// that was missing when `web_retry` first shipped.
async fn run_retry_loop<F, Fut, R>(mut fetch: F) -> std::result::Result<R, String>
where
    F: FnMut(usize, &'static str) -> Fut,
    Fut: std::future::Future<Output = std::result::Result<FetchStep<R>, String>>,
{
    let mut attempt: usize = 0;
    loop {
        let user_agent = crate::web_retry::user_agent_for(attempt);
        match fetch(attempt, user_agent).await? {
            FetchStep::Done(r) => return Ok(r),
            FetchStep::Retryable {
                status,
                retry_after_secs,
                body_preview,
            } => match crate::web_retry::decide_with_retry_after(
                status,
                &body_preview,
                attempt,
                retry_after_secs,
            ) {
                crate::web_retry::Next::RetrySameAgent { wait_secs } => {
                    tracing::debug!(
                        attempt,
                        wait_secs,
                        status,
                        "web_fetch: rate limited; sleeping before retry",
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(wait_secs)).await;
                    attempt += 1;
                }
                crate::web_retry::Next::RetryNextAgent => {
                    tracing::debug!(attempt, status, "web_fetch: bot wall; rotating user agent");
                    attempt += 1;
                }
                crate::web_retry::Next::Return => {
                    return Err(format!("returned HTTP {}: {}", status, body_preview,));
                }
            },
        }
    }
}

/// Fetch a URL over HTTP/HTTPS and return its text content.
pub struct WebFetchTool {
    pub definition: ToolDefinition,
}

impl WebFetchTool {
    pub fn new() -> Self {
        // H-S7 / T1-C15: the initial client is no longer built here.
        // `fetch_with_safe_redirects` builds a per-hop client with the
        // address pinned from its own validation pass.

        Self {
            definition: ToolDefinition {
                trust_level: kod_types::trust::TrustLevel::ToolUntrusted,
                id: ToolId::new(),
                name: "web_fetch".to_string(),
                description: "Fetch a URL over HTTP/HTTPS and return its text. HTML is \
                    converted to plain text; JSON is returned as-is. Private and \
                    link-local addresses are refused. Requires the `network_access` \
                    permission."
                    .to_string(),
                category: ToolCategory::Web,
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "Absolute http:// or https:// URL"
                        }
                    },
                    "required": ["url"],
                    "additionalProperties": false
                }),
                permissions: ToolPermissions {
                    read_files: false,
                    write_files: false,
                    execute_commands: false,
                    network_access: true,
                    git_access: kod_types::GitAccess::None,
                    allowed_paths: Vec::new(),
                    forbidden_paths: Vec::new(),
                },
                load_mode: Default::default(),
            },
        }
    }

    /// T1-C15: follow redirects manually, re-validating and re-pinning
    /// the target address at every hop.
    ///
    /// reqwest's `redirect::Policy::custom` runs synchronously and
    /// cannot perform a DNS lookup — it sees only the literal host
    /// string of the redirect target. That accepted
    /// `http://attacker/ -> 302 -> http://metadata.google.internal/`
    /// because the string `metadata.google.internal` is not on the
    /// literal blocklist; reqwest's resolver then turned the name into
    /// 169.254.169.254 and connected to the metadata endpoint.
    ///
    /// With `Policy::none()` and this loop, on every hop:
    ///   1. `block_private_host` runs on the URL's literal host.
    ///   2. The host is re-resolved and every returned address is
    ///      checked against `block_private_ip`.
    ///   3. The connection is pinned to a validated address so a
    ///      TTL-0 DNS cannot swap in a private IP between our check
    ///      and the TCP connect.
    ///
    /// Returns the final non-redirect response, or a user-facing error
    /// string the caller surfaces via `ToolResult::Error`.
    async fn fetch_with_safe_redirects(
        &self,
        start: reqwest::Url,
        user_agent: &str,
        allowed_domains: &[String],
    ) -> std::result::Result<reqwest::Response, String> {
        let mut current = start.clone();
        for hop in 0..=MAX_REDIRECTS {
            let host = current
                .host_str()
                .ok_or_else(|| format!("URL {} has no host", current))?
                .to_string();
            if let Some(reason) = block_private_host(&host) {
                return Err(format!("refusing to fetch {}: {}", current, reason));
            }
            // SEC-B-02: re-check the operator's allow-list on EVERY
            // hop. The initial check in `execute` runs once on the
            // literal start URL; without this, a redirect to any
            // public host bypasses `allowed_domains` entirely.
            if !allowed_domains.is_empty() && !domain_allowed(&host, allowed_domains) {
                return Err(format!(
                    "redirect target {} is outside the policy allow-list {:?}",
                    current, allowed_domains
                ));
            }
            let port = current
                .port_or_known_default()
                .ok_or_else(|| format!("URL {} has no known port", current))?;
            let host_owned = host.clone();
            let lookup = tokio::task::spawn_blocking(move || {
                use std::net::ToSocketAddrs;
                (host_owned.as_str(), port)
                    .to_socket_addrs()
                    .map(|it| it.collect::<Vec<_>>())
            })
            .await
            .map_err(|e| format!("DNS lookup task failed: {e}"))?
            .map_err(|e| format!("DNS lookup failed for {host}: {e}"))?;
            if lookup.is_empty() {
                return Err(format!("{host} resolved to no addresses"));
            }
            for addr in &lookup {
                if let Some(reason) = block_private_ip(addr.ip()) {
                    return Err(format!(
                        "refusing to fetch {}: {} resolves to {} ({})",
                        current,
                        host,
                        addr.ip(),
                        reason
                    ));
                }
            }
            let pinned = lookup[0];
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
                // Delta §14.5: the UA is the caller's — the retry
                // loop in `execute` rotates it on a bot wall. The
                // first entry of `web_retry::USER_AGENTS` is kod's
                // own identifier, so the default path is unchanged.
                .user_agent(user_agent)
                // T1-C15: no auto-redirects — we follow them here so
                // every hop goes through the checks above.
                .redirect(reqwest::redirect::Policy::none())
                .resolve(&host, pinned)
                .build()
                .map_err(|e| format!("could not build request client: {e}"))?;
            let response = client
                .get(current.clone())
                .send()
                .await
                .map_err(|e| format!("request failed for {}: {}", current, e))?;
            let status = response.status();
            if !status.is_redirection() {
                return Ok(response);
            }
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| format!("{current} returned {status} with no Location header"))?;
            let next = current
                .join(location)
                .map_err(|e| format!("redirect Location {location:?} is not a valid URL: {e}"))?;
            if !matches!(next.scheme(), "http" | "https") {
                return Err(format!(
                    "redirect from {current} to non-http(s) scheme {:?}",
                    next.scheme()
                ));
            }
            if hop == MAX_REDIRECTS {
                return Err(format!(
                    "too many redirects ({MAX_REDIRECTS} hops) starting from {start}"
                ));
            }
            current = next;
        }
        unreachable!("redirect loop returns within MAX_REDIRECTS + 1 iterations")
    }
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for WebFetchTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, params: &Value, context: &ToolContext) -> Result<ToolResult> {
        let url_str = params["url"]
            .as_str()
            .ok_or_else(|| KodError::InvalidParameters {
                reason: "Missing 'url' parameter".to_string(),
            })?;

        context.can_access_network(url_str)?;

        // Domain allowlist from the policy layer (D3-C5). Empty means
        // "any public domain the SSRF filter allows"; a non-empty list
        // restricts to exactly those domains (subdomain matching:
        // `docs.rs` allows `docs.rs` and `*.docs.rs`).
        if !context.allowed_domains.is_empty() {
            let host = reqwest::Url::parse(url_str)
                .ok()
                .and_then(|u| u.host_str().map(|s| s.to_lowercase()))
                .unwrap_or_default();
            if !domain_allowed(&host, &context.allowed_domains) {
                return Ok(ToolResult::Error(format!(
                    "refusing to fetch {}: the effective policy allow-list is {:?}",
                    url_str, context.allowed_domains
                )));
            }
        }

        let url = match reqwest::Url::parse(url_str) {
            Ok(u) => u,
            Err(e) => {
                return Ok(ToolResult::Error(format!(
                    "invalid URL {:?}: {} — expected an absolute http:// or https:// URL",
                    url_str, e
                )));
            }
        };
        if !matches!(url.scheme(), "http" | "https") {
            return Ok(ToolResult::Error(format!(
                "unsupported scheme {:?}: only http and https are allowed",
                url.scheme()
            )));
        }
        if let Some(host) = url.host_str() {
            if let Some(reason) = block_private_host(host) {
                return Ok(ToolResult::Error(format!(
                    "refusing to fetch {}: {}",
                    url, reason
                )));
            }
            // Resolve the host and check every returned address too.
            // The literal-text check above catches `localhost` and
            // dotted-quad forms; the DNS check catches an A record
            // that points at 10.x when the URL spelled a public name.
            // `to_socket_addrs` is sync, so run it in a blocking
            // task — a DNS lookup that stalls must not stall the
            // async runtime.
            if let Some(port) = url.port_or_known_default() {
                let host_owned = host.to_string();
                let lookup = tokio::task::spawn_blocking(move || {
                    use std::net::ToSocketAddrs;
                    (host_owned.as_str(), port)
                        .to_socket_addrs()
                        .map(|it| it.collect::<Vec<_>>())
                })
                .await
                .unwrap_or(Ok(Vec::new()));
                if let Ok(addrs) = lookup {
                    for addr in addrs {
                        if let Some(reason) = block_private_ip(addr.ip()) {
                            return Ok(ToolResult::Error(format!(
                                "refusing to fetch {}: resolves to {} ({})",
                                url,
                                addr.ip(),
                                reason
                            )));
                        }
                    }
                }
            }
        }

        // T1-C15: `fetch_with_safe_redirects` re-validates and re-pins
        // the address at every redirect hop. reqwest's
        // `Policy::custom` runs synchronously and cannot do a DNS
        // lookup — it saw only the literal host text of a redirect
        // target, so `attacker -> 302 -> metadata.google.internal`
        // passed the check while reqwest's own resolver turned the
        // name into 169.254.169.254 and connected. Following the
        // redirect here means the SSRF check and the address pin both
        // run on every hop.
        // Delta §14.5: the fetch is retried per `web_retry`'s policy.
        // The `web_retry` module shipped with the delta but was never
        // wired into this call site, so its promises — a 429 retried
        // once honoring `Retry-After`, a bot wall retried with the
        // next user agent, bounded at `MAX_UA_ROTATIONS` — never
        // executed. `run_retry_loop` is that caller.
        let url_for_retry = url.clone();
        // SEC-B-02: thread the operator's domain allow-list into the
        // per-hop revalidation. Cloned once and re-cloned per retry
        // attempt (small Vec<String>, cheap).
        let allowed_domains_for_retry = context.allowed_domains.clone();
        let response = match run_retry_loop(|_attempt, ua| {
            let url = url_for_retry.clone();
            let ua = ua.to_string();
            let allowed = allowed_domains_for_retry.clone();
            async move {
                let resp = match self.fetch_with_safe_redirects(url, &ua, &allowed).await {
                    Ok(r) => r,
                    Err(msg) => return Err(msg),
                };
                let status = resp.status();
                if status.is_success() {
                    return Ok(FetchStep::Done(resp));
                }
                // M-33: cap the error body — a hostile server can
                // stream forever before the 300-char preview. 4 KiB
                // is enough for the bot-wall markers
                // `is_bot_block` looks for.
                let retry_after_secs = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.trim().parse::<u64>().ok());
                let mut resp = resp;
                let mut body_bytes: Vec<u8> = Vec::new();
                while body_bytes.len() < 4096 {
                    match resp.chunk().await {
                        Ok(Some(b)) => body_bytes.extend_from_slice(&b),
                        Ok(None) | Err(_) => break,
                    }
                }
                let body = String::from_utf8_lossy(&body_bytes).into_owned();
                let preview = kod_types::strutil::truncate_with_ellipsis(&body, 300, "…");
                Ok(FetchStep::Retryable {
                    status: status.as_u16(),
                    retry_after_secs,
                    body_preview: preview,
                })
            }
        })
        .await
        {
            Ok(r) => r,
            Err(msg) => {
                return Ok(ToolResult::Error(format!("{} {}", url, msg)));
            }
        };

        let status = response.status();

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let mime = content_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_lowercase();

        // Reject binary content types up front so a PNG or a video
        // does not decode to lossy garbage and confuse the model.
        let is_text = mime.is_empty()
            || mime.starts_with("text/")
            || mime == "application/json"
            || mime == "application/xml"
            || mime == "application/xhtml+xml"
            || mime.ends_with("+json")
            || mime.ends_with("+xml");
        if !is_text {
            return Ok(ToolResult::Error(format!(
                "{} returned content-type {:?}, which is not text. web_fetch is \
                 for HTML, JSON, and plain text.",
                url, mime
            )));
        }

        // Stream the body with a hard byte cap. `response.bytes()`
        // would allocate the whole body before we could check its
        // size; a 1 GB video served as text/plain would OOM us.
        use futures::StreamExt;
        let mut body = Vec::new();
        let mut truncated = false;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    let remaining = MAX_BODY_BYTES.saturating_sub(body.len());
                    if bytes.len() > remaining {
                        body.extend_from_slice(&bytes[..remaining]);
                        truncated = true;
                        break;
                    }
                    body.extend_from_slice(&bytes);
                    if body.len() >= MAX_BODY_BYTES {
                        truncated = true;
                        break;
                    }
                }
                Err(e) => {
                    // Partial content on a mid-stream error is more
                    // useful than nothing: the caller sees the
                    // prefix plus the failure.
                    return Ok(ToolResult::Error(format!(
                        "read error after {} bytes from {}: {}",
                        body.len(),
                        url,
                        e
                    )));
                }
            }
        }

        // Cut at a UTF-8 boundary; a cap landing mid-codepoint is not
        // allowed to panic.
        let text = match std::str::from_utf8(&body) {
            Ok(s) => s.to_string(),
            Err(e) => String::from_utf8_lossy(&body[..e.valid_up_to()]).into_owned(),
        };

        // HTML → text. A crude tag strip, no dependency: enough for
        // documentation pages where the model wants the prose, not
        // the layout. Anything that survived as a `&lt;` in an
        // attribute is left alone — it is rare and the model can
        // recognize it.
        let rendered = if mime.contains("html") {
            html_to_text(&text)
        } else {
            text
        };

        Ok(ToolResult::Success(serde_json::json!({
            "url": url.to_string(),
            "status": status.as_u16(),
            "content_type": mime,
            "truncated": truncated,
            "bytes": body.len(),
            "text": rendered,
        })))
    }
}

/// Reject obvious private hosts by literal text: `localhost`,
/// `*.local`, or an IPv4/IPv6 literal in a private range. Returns the
/// reason, or `None` when the host looks public.
fn block_private_host(host: &str) -> Option<String> {
    let lower = host.to_lowercase();
    if lower == "localhost"
        || lower.ends_with(".localhost")
        || lower == "ip6-localhost"
        || lower == "ip6-loopback"
    {
        return Some("localhost is not fetchable".to_string());
    }
    if lower == "metadata.google.internal" {
        return Some("cloud metadata host is not fetchable".to_string());
    }
    if lower.ends_with(".local") {
        return Some("mDNS (.local) hosts are not fetchable".to_string());
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>()
        && let Some(reason) = block_private_ip(ip)
    {
        return Some(reason);
    }
    None
}

/// Reject IPs in ranges that must not be reachable from an
/// agent-originated request.
fn block_private_ip(ip: std::net::IpAddr) -> Option<String> {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            if v4.is_loopback() {
                return Some("loopback address".to_string());
            }
            if v4.is_private() {
                return Some("private address (RFC1918)".to_string());
            }
            if v4.is_link_local() {
                return Some("link-local address (cloud metadata)".to_string());
            }
            if v4.is_broadcast() || v4.is_unspecified() || v4.is_multicast() {
                return Some("non-routable address".to_string());
            }
            // 0.0.0.0/8 is "this network" and often used to reach the
            // host; `is_unspecified` only matches the exact 0.0.0.0.
            if octets[0] == 0 {
                return Some("0.0.0.0/8 is not routable".to_string());
            }
            // 100.64.0.0/10 is CGNAT; may route to something private.
            if octets[0] == 100 && (64..128).contains(&octets[1]) {
                return Some("carrier-grade NAT range".to_string());
            }
            None
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() {
                return Some("IPv6 loopback".to_string());
            }
            if v6.is_unspecified() || v6.is_multicast() {
                return Some("IPv6 non-routable address".to_string());
            }
            let segments = v6.segments();
            // fc00::/7 unique-local.
            if segments[0] & 0xfe00 == 0xfc00 {
                return Some("IPv6 unique-local (fc00::/7)".to_string());
            }
            // fe80::/10 link-local.
            if segments[0] & 0xffc0 == 0xfe80 {
                return Some("IPv6 link-local (fe80::/10)".to_string());
            }
            // ::ffff:0:0/96 is IPv4-mapped; unwrap and re-check.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return block_private_ip(std::net::IpAddr::V4(v4));
            }
            None
        }
    }
}

/// Crude HTML → text. Drops `<script>`, `<style>`, and `<head>`
/// contents whole; collapses tags to whitespace; decodes the handful
/// of entities a documentation page is likely to contain. Deliberately
/// not a real HTML parser — the goal is "readable prose", and pulling
/// in an HTML5 parser for a tool this size is not worth the cost.
fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let bytes = html.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Skip whole elements by name when we see their open tag.
        if let Some(skip) = skip_element_at(html, i) {
            i = skip;
            continue;
        }
        let b = bytes[i];
        if b == b'<' {
            // Skip to the next '>'.
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != b'>' {
                j += 1;
            }
            // A tag boundary is a word boundary; emit a space unless
            // the previous char was already whitespace.
            if !out.ends_with(' ') && !out.ends_with('\n') && !out.is_empty() {
                out.push(' ');
            }
            i = if j < bytes.len() { j + 1 } else { bytes.len() };
            continue;
        }
        // Copy the whole codepoint: `char::from(b)` mapped each byte
        // of a multibyte sequence to its own Latin-1 codepoint, turning
        // `café` into `cafÃ©` in every fetched page.
        if b.is_ascii() {
            out.push(b as char);
            i += 1;
        } else {
            let ch = html[i..].chars().next().unwrap_or('\u{FFFD}');
            out.push(ch);
            i += ch.len_utf8();
        }
    }

    // Decode the entities a doc page actually uses. Order matters:
    // `&amp;` last, so `&amp;lt;` becomes `&lt;` (not `<`).
    let out = out
        .replace("&nbsp;", " ")
        .replace("&mdash;", "—")
        .replace("&ndash;", "–")
        .replace("&hellip;", "…")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");

    // Collapse runs of blank lines: HTML source is full of them from
    // indentation, and the model does not need three blank rows between
    // paragraphs.
    let mut collapsed = String::with_capacity(out.len());
    let mut blank_streak = 0usize;
    for line in out.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim().is_empty() {
            blank_streak += 1;
            if blank_streak <= 1 {
                collapsed.push('\n');
            }
        } else {
            blank_streak = 0;
            collapsed.push_str(trimmed);
            collapsed.push('\n');
        }
    }
    collapsed.trim().to_string()
}

/// Fuzz-only wrapper: expose the private HTML parser for external
/// fuzz targets without widening the crate's public surface.
#[doc(hidden)]
pub fn html_to_text_for_fuzz(html: &str) -> String {
    html_to_text(html)
}

/// If an open tag for a skipped element starts at byte offset `i` in
/// the lowercased HTML, return the offset just past its matching close
/// tag (or the end of the document when unmatched). The set of skipped
/// elements is fixed and small.
/// If an element named in `SKIP` opens at byte offset `i` of `html`,
/// return the byte offset just past its closing tag (or the end of the
/// input when it never closes).
///
/// Searches `html` itself, comparing the tag name ASCII
/// case-insensitively. The pre-fix version searched a `to_lowercase()`
/// copy: lowercasing can change a string's byte length (the fuzz
/// corpus had a 99-byte input that lowercased to 98), so an offset
/// that is a char boundary in the copy can land mid-codepoint in the
/// original and `html[i..]` panics. HTML tag names are ASCII, so the
/// case-insensitive comparison needs no second string.
fn skip_element_at(html: &str, i: usize) -> Option<usize> {
    const SKIP: &[&str] = &["script", "style", "head", "noscript", "svg", "template"];
    let hay = html.as_bytes();
    // The open tag starts with an ASCII `<`, which is always a char
    // boundary.
    if hay.get(i) != Some(&b'<') {
        return None;
    }
    let name_start = i + 1;
    for name in SKIP {
        let name_bytes = name.as_bytes();
        let name_end = name_start + name_bytes.len();
        // Both sides are ASCII here, or the comparison fails.
        let Some(candidate) = hay.get(name_start..name_end) else {
            continue;
        };
        if !candidate.eq_ignore_ascii_case(name_bytes) {
            continue;
        }
        // A tag name must end here -- not continue into more
        // alphanumerics (`<svgfoo` is not `<svg`).
        let after = hay.get(name_end).copied().unwrap_or(b'>');
        if after.is_ascii_alphanumeric() {
            continue;
        }
        // Find the matching close tag, byte by byte in `html` space.
        let close = format!("</{name}");
        let close_bytes = close.as_bytes();
        let mut k = name_end;
        while k + close_bytes.len() <= hay.len() {
            if hay[k..k + close_bytes.len()].eq_ignore_ascii_case(close_bytes) {
                let mut m = k + close_bytes.len();
                while m < hay.len() && hay[m] != b'>' {
                    m += 1;
                }
                return Some(if m < hay.len() { m + 1 } else { hay.len() });
            }
            k += 1;
        }
        return Some(hay.len());
    }
    None
}

/// Is `host` covered by `allowed`? A pattern matches the exact host
/// or any subdomain (`docs.rs` allows `docs.rs` and `*.docs.rs`).
/// Case-insensitive on both sides. Empty `allowed` is handled by the
/// caller (means "no restriction").
fn domain_allowed(host: &str, allowed: &[String]) -> bool {
    let host = host.to_lowercase();
    allowed.iter().any(|d| {
        let d = d.to_lowercase();
        host == d || host.ends_with(&format!(".{d}"))
    })
}

#[cfg(test)]
mod tests {

    #[test]
    fn exact_host_is_allowed() {
        let a = vec!["docs.rs".to_string()];
        assert!(domain_allowed("docs.rs", &a));
    }

    #[test]
    fn subdomain_is_allowed() {
        let a = vec!["docs.rs".to_string()];
        assert!(domain_allowed("www.docs.rs", &a));
        assert!(domain_allowed("a.b.docs.rs", &a));
    }

    #[test]
    fn unrelated_host_is_refused() {
        let a = vec!["docs.rs".to_string()];
        assert!(!domain_allowed("evil.com", &a));
    }

    #[test]
    fn substring_tail_is_not_a_subdomain() {
        // `notdocs.rs` must not match `docs.rs` via a naive
        // `ends_with("docs.rs")`; the `.`-prefix requirement blocks it.
        let a = vec!["docs.rs".to_string()];
        assert!(!domain_allowed("notdocs.rs", &a));
    }

    #[test]
    fn matching_is_case_insensitive() {
        let a = vec!["Docs.RS".to_string()];
        assert!(domain_allowed("docs.rs", &a));
        assert!(domain_allowed("WWW.DOCS.RS", &a));
    }

    #[test]
    fn empty_allow_list_is_handled_by_the_caller() {
        // `domain_allowed` is only reached when the list is
        // non-empty; with an empty slice it returns false, and the
        // caller's `is_empty()` guard means that path is never taken.
        assert!(!domain_allowed("docs.rs", &[]));
    }

    #[test]
    fn any_of_multiple_patterns_matches() {
        let a = vec!["docs.rs".to_string(), "github.com".to_string()];
        assert!(domain_allowed("api.github.com", &a));
        assert!(domain_allowed("docs.rs", &a));
        assert!(!domain_allowed("gitlab.com", &a));
    }

    /// Delta §14.5 wiring: a 429 retries once with the same UA, then
    /// succeeds. Pre-fix, `web_retry` was never called, so this fetch
    /// failed on the first 429.
    #[tokio::test]
    async fn retry_loop_retries_a_429_once() {
        let calls: std::sync::Arc<std::sync::Mutex<Vec<usize>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let calls_for_closure = calls.clone();
        let result: std::result::Result<(), String> = run_retry_loop(|attempt, _ua| {
            let calls = calls_for_closure.clone();
            async move {
                calls.lock().unwrap().push(attempt);
                if attempt == 0 {
                    Ok(FetchStep::Retryable {
                        status: 429,
                        retry_after_secs: Some(0),
                        body_preview: "rate limited".into(),
                    })
                } else {
                    Ok(FetchStep::Done(()))
                }
            }
        })
        .await;
        assert!(result.is_ok(), "429 must retry, then succeed: {result:?}");
        assert_eq!(*calls.lock().unwrap(), vec![0, 1]);
    }

    /// A bot wall (403 + a marker word) rotates the UA up to
    /// `MAX_UA_ROTATIONS`, then gives up. Pre-fix, the first 403 was
    /// returned without a rotation.
    #[tokio::test]
    async fn retry_loop_rotates_ua_three_times_on_a_bot_block() {
        let calls: std::sync::Arc<std::sync::Mutex<Vec<usize>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let calls_for_closure = calls.clone();
        let result: std::result::Result<(), String> = run_retry_loop(|attempt, _ua| {
            let calls = calls_for_closure.clone();
            async move {
                calls.lock().unwrap().push(attempt);
                Ok(FetchStep::Retryable {
                    status: 403,
                    retry_after_secs: None,
                    body_preview: "cloudflare captcha challenge".into(),
                })
            }
        })
        .await;
        assert!(result.is_err(), "a persistent bot wall must give up");
        // Initial request + MAX_UA_ROTATIONS (3) rotations.
        assert_eq!(calls.lock().unwrap().len(), 4);
    }

    /// A plain 403 (no bot-wall marker) does not rotate: one request
    /// and the error is surfaced.
    #[tokio::test]
    async fn retry_loop_gives_up_immediately_on_a_plain_403() {
        let calls: std::sync::Arc<std::sync::Mutex<Vec<usize>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let calls_for_closure = calls.clone();
        let result: std::result::Result<(), String> = run_retry_loop(|attempt, _ua| {
            let calls = calls_for_closure.clone();
            async move {
                calls.lock().unwrap().push(attempt);
                Ok(FetchStep::Retryable {
                    status: 403,
                    retry_after_secs: None,
                    body_preview: "forbidden".into(),
                })
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls.lock().unwrap().len(), 1, "no rotation on a plain 403");
    }

    /// The retry loop passes the correct UA for each attempt: the
    /// first entry of `USER_AGENTS` on attempt 0, then successive
    /// entries. Pre-fix, `fetch_with_safe_redirects` hardcoded the
    /// UA, so this could not vary.
    #[tokio::test]
    async fn retry_loop_rotates_the_user_agent_argument() {
        let seen: std::sync::Arc<std::sync::Mutex<Vec<&'static str>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_for_closure = seen.clone();
        let _: std::result::Result<(), String> = run_retry_loop(|attempt, ua| {
            let seen = seen_for_closure.clone();
            async move {
                seen.lock().unwrap().push(ua);
                if attempt < 2 {
                    Ok(FetchStep::Retryable {
                        status: 403,
                        retry_after_secs: None,
                        body_preview: "cloudflare".into(),
                    })
                } else {
                    Ok(FetchStep::Done(()))
                }
            }
        })
        .await;
        let uas = seen.lock().unwrap().clone();
        assert_eq!(uas.len(), 3);
        assert_eq!(uas[0], crate::web_retry::USER_AGENTS[0]);
        assert_eq!(uas[1], crate::web_retry::USER_AGENTS[1]);
        assert_eq!(uas[2], crate::web_retry::USER_AGENTS[2]);
    }

    #[tokio::test]
    async fn safe_redirects_rejects_loopback() {
        let tool = WebFetchTool::new();
        let url = reqwest::Url::parse("http://127.0.0.1:1/").unwrap();
        let result = tool.fetch_with_safe_redirects(url, "test-agent", &[]).await;
        assert!(result.is_err(), "loopback URL must be refused");
        let msg = result.unwrap_err();
        assert!(
            msg.contains("127.0.0.1") || msg.contains("loopback"),
            "unexpected error message: {msg}"
        );
    }

    #[tokio::test]
    async fn safe_redirects_rejects_metadata_host() {
        let tool = WebFetchTool::new();
        let url = reqwest::Url::parse("http://metadata.google.internal/").unwrap();
        let result = tool.fetch_with_safe_redirects(url, "test-agent", &[]).await;
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(msg.contains("metadata"), "got: {msg}");
    }

    use super::*;
    use kod_types::ToolPermissions;

    fn network_ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext::new(dir).with_permissions(ToolPermissions {
            network_access: true,
            ..Default::default()
        })
    }

    #[test]
    fn private_host_rejects_loopback_names() {
        assert!(block_private_host("localhost").is_some());
        assert!(block_private_host("api.localhost").is_some());
        assert!(block_private_host("printer.local").is_some());
        assert!(block_private_host("metadata.google.internal").is_some());
        assert!(block_private_host("example.com").is_none());
        assert!(block_private_host("docs.rs").is_none());
    }

    #[test]
    fn private_ip_rejects_expected_ranges() {
        use std::net::IpAddr;
        for bad in [
            "127.0.0.1",
            "10.0.0.1",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.169.254", // cloud metadata
            "::1",
            "fe80::1",
            "fc00::1",
        ] {
            let ip: IpAddr = bad.parse().unwrap();
            assert!(
                block_private_ip(ip).is_some(),
                "expected {} to be blocked",
                bad
            );
        }
        for good in ["1.1.1.1", "8.8.8.8", "2606:4700::1"] {
            let ip: IpAddr = good.parse().unwrap();
            assert!(
                block_private_ip(ip).is_none(),
                "expected {} to be allowed, got {:?}",
                good,
                block_private_ip(ip)
            );
        }
    }

    #[test]
    fn html_to_text_does_not_panic_on_multibyte_at_any_offset() {
        // Fuzz artifact: `-ĉ` (bytes [0x2d, 0xc4, 0x89]). The old
        // byte-loop in `html_to_text` called `skip_element_at` with
        // the byte offset of the second byte of `ĉ`, and the
        // `&lower[i..]` inside that function panicked on the
        // non-char-boundary slice. Exercising the three boundary
        // cases the byte loop can hit — first byte, middle byte,
        // last byte — guards the fix.
        for case in ["-\u{0109}", "a\u{0109}b", "\u{1F600}", "\u{0109}"] {
            let _ = html_to_text(case);
        }
    }

    #[test]
    fn html_to_text_survives_a_lowercase_length_change() {
        // Fuzz artifact (CI run 40219270394, html target): a 99-byte
        // input that `to_lowercase` shortens to 98. The pre-fix code
        // searched the lowercased copy and then indexed the original
        // with the offsets it returned, so an offset that is a char
        // boundary in the copy can land mid-codepoint in the original:
        // the crash was `start byte index 98 is not a char boundary;
        // it is inside '〙' (bytes 96..99)`. The same class as the
        // `-ĉ` case above, but it only reproduces when the lowercased
        // string is a *different length* -- `ĉ` is 2 bytes either way.
        // `skip_element_at` now searches `html` itself and never
        // builds the copy.
        let bytes: &[u8] = &[
            10, 10, 10, 10, 10, 10, 32, 200, 153, 226, 177, 164, 0, 0, 0, 10, 10, 10, 10, 10, 10,
            10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 96, 10, 10, 96, 10, 10, 227, 128, 153, 10,
            10, 10, 10, 0, 75, 10, 10, 10, 10, 10, 10, 10, 96, 10, 10, 10, 10, 10, 227, 128, 153,
            10, 10, 10, 10, 0, 32, 75, 10, 10, 10, 10, 10, 10, 10, 96, 10, 47, 115, 10, 10, 10, 10,
            10, 105, 109, 60, 115, 116, 121, 108, 101, 59, 10, 227, 128, 153,
        ];
        let html = std::str::from_utf8(bytes).expect("artifact is valid UTF-8");
        assert!(
            html.len() != html.to_lowercase().len(),
            "the artifact must exercise the length change",
        );
        let _ = html_to_text(html);
    }

    #[test]
    fn html_to_text_strips_tags_and_scripts() {
        let html = "<html><head><title>t</title></head><body>\
                    <h1>Hello</h1>\
                    <script>alert('x')</script>\
                    <p>World &amp; friends</p>\
                    <style>body{}</style>\
                    </body></html>";
        let text = html_to_text(html);
        assert!(text.contains("Hello"), "got: {text}");
        assert!(text.contains("World & friends"), "got: {text}");
        assert!(!text.contains("alert"), "script content leaked: {text}");
        assert!(!text.contains("body{}"), "style content leaked: {text}");
        assert!(!text.contains("<h1>"), "tag leaked: {text}");
    }

    #[test]
    fn html_to_text_collapses_blank_lines() {
        let html = "a\n\n\n\n\nb";
        let text = html_to_text(html);
        // At most one blank line survives between non-blank lines.
        assert!(!text.contains("\n\n\n"), "too many blank lines: {:?}", text);
    }

    #[tokio::test]
    async fn web_fetch_denied_without_permission() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ctx = ToolContext::new(tmp.path()).with_permissions(ToolPermissions {
            network_access: false,
            ..Default::default()
        });
        let tool = WebFetchTool::new();
        let result = tool
            .execute(&serde_json::json!({ "url": "https://example.com" }), &ctx)
            .await;
        match result {
            Err(KodError::PermissionDenied { action, .. }) => {
                assert!(action.contains("network"), "got: {action}");
            }
            other => panic!("expected PermissionDenied, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn web_fetch_rejects_localhost_before_connecting() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ctx = network_ctx(tmp.path());
        let tool = WebFetchTool::new();
        let result = tool
            .execute(
                &serde_json::json!({ "url": "http://localhost:8080/anything" }),
                &ctx,
            )
            .await
            .unwrap();
        match result {
            ToolResult::Error(msg) => {
                assert!(
                    msg.contains("localhost"),
                    "error should name the blocked host: {msg}"
                );
            }
            other => panic!("expected ToolResult::Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn web_fetch_rejects_private_ip_literal() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ctx = network_ctx(tmp.path());
        let tool = WebFetchTool::new();
        let result = tool
            .execute(
                &serde_json::json!({ "url": "http://169.254.169.254/latest/meta-data" }),
                &ctx,
            )
            .await
            .unwrap();
        match result {
            ToolResult::Error(msg) => {
                assert!(
                    msg.contains("169.254") || msg.contains("link-local"),
                    "error should identify the metadata IP: {msg}"
                );
            }
            other => panic!("expected ToolResult::Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn web_fetch_rejects_non_http_scheme() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ctx = network_ctx(tmp.path());
        let tool = WebFetchTool::new();
        let result = tool
            .execute(&serde_json::json!({ "url": "file:///etc/passwd" }), &ctx)
            .await
            .unwrap();
        match result {
            ToolResult::Error(msg) => {
                assert!(
                    msg.contains("unsupported scheme") || msg.contains("scheme"),
                    "expected scheme error: {msg}"
                );
            }
            other => panic!("expected ToolResult::Error, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod coverage_ssrf_filter {
    //! Additional cases for the SSRF filter. A regression here turns
    //! an obvious attack surface into a live one — the model can be
    //! prompted into fetching metadata endpoints, backend admin
    //! ports, or the host's own loopback — without any visible
    //! failure elsewhere in the suite. The existing tests cover the
    //! obvious ranges; the corners are what this module pins.
    use super::*;
    use std::net::IpAddr;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap_or_else(|_| panic!("bad test IP: {s}"))
    }

    #[test]
    fn ipv4_mapped_ipv6_is_unwrapped_and_rechecked() {
        // The classic SSRF bypass: reject `127.0.0.1` but forget
        // that `::ffff:127.0.0.1` is the same address in IPv6
        // spelling. The filter must unwrap the mapped form and
        // re-check it against the IPv4 rules.
        assert!(block_private_ip(ip("::ffff:127.0.0.1")).is_some());
        assert!(block_private_ip(ip("::ffff:10.0.0.1")).is_some());
        assert!(block_private_ip(ip("::ffff:169.254.169.254")).is_some());
        // And the mapped form of a public address is still allowed.
        assert!(block_private_ip(ip("::ffff:8.8.8.8")).is_none());
    }

    #[test]
    fn cgnat_range_boundaries_are_both_blocked() {
        // 100.64.0.0/10 is CGNAT — space that a home router may
        // route into a private segment. Both boundaries are inside
        // the range and must be rejected; the neighbours just
        // outside must not.
        assert!(block_private_ip(ip("100.64.0.0")).is_some());
        assert!(block_private_ip(ip("100.127.255.255")).is_some());
        assert!(block_private_ip(ip("100.63.255.255")).is_none());
        assert!(block_private_ip(ip("100.128.0.0")).is_none());
    }

    #[test]
    fn zero_network_is_rejected() {
        // 0.0.0.0/8 is "this network". `is_unspecified` only matches
        // the exact 0.0.0.0; the explicit octet check catches the
        // rest. A regression that dropped the explicit check would
        // accept 0.1.2.3.
        assert!(block_private_ip(ip("0.0.0.0")).is_some());
        assert!(block_private_ip(ip("0.1.2.3")).is_some());
        assert!(block_private_ip(ip("0.255.255.255")).is_some());
    }

    #[test]
    fn broadcast_and_multicast_are_rejected() {
        assert!(block_private_ip(ip("255.255.255.255")).is_some());
        assert!(block_private_ip(ip("224.0.0.1")).is_some());
        assert!(block_private_ip(ip("239.255.255.255")).is_some());
    }

    #[test]
    fn ipv6_unique_local_and_link_local_are_rejected() {
        // fc00::/7 (unique local) and fe80::/10 (link local) are the
        // IPv6 equivalents of RFC1918 and APIPA.
        assert!(block_private_ip(ip("fc00::1")).is_some());
        assert!(block_private_ip(ip("fd12:3456:789a::1")).is_some());
        assert!(block_private_ip(ip("fe80::1")).is_some());
        assert!(block_private_ip(ip("febf::1")).is_some());
        // Just outside the unique-local range.
        assert!(block_private_ip(ip("fe00::1")).is_none());
    }

    #[test]
    fn public_ipv6_is_allowed() {
        // Cloudflare DNS, a well-known public address.
        assert!(block_private_ip(ip("2606:4700:4700::1111")).is_none());
        // Google DNS.
        assert!(block_private_ip(ip("2001:4860:4860::8888")).is_none());
    }

    #[test]
    fn localhost_variants_are_rejected_case_insensitively() {
        assert!(block_private_host("localhost").is_some());
        assert!(block_private_host("LOCALHOST").is_some());
        assert!(block_private_host("LocalHost").is_some());
        assert!(block_private_host("api.localhost").is_some());
        assert!(block_private_host("ip6-localhost").is_some());
        assert!(block_private_host("ip6-loopback").is_some());
    }

    #[test]
    fn mdns_local_suffix_is_rejected() {
        assert!(block_private_host("printer.local").is_some());
        assert!(block_private_host("my-host.local").is_some());
        assert!(block_private_host("PRINTER.LOCAL").is_some());
        // But `.localhost` and `.local` are not the same — a domain
        // ending in `local` as a substring (e.g. `local.example`)
        // must not be caught.
        assert!(block_private_host("local.example").is_none());
    }

    #[test]
    fn google_metadata_hostname_is_rejected() {
        // The GCP metadata service is reachable at
        // `metadata.google.internal`. The address is also in the
        // link-local range, but the hostname check catches it before
        // any DNS resolution.
        assert!(block_private_host("metadata.google.internal").is_some());
    }

    #[test]
    fn public_domains_are_allowed() {
        assert!(block_private_host("docs.rs").is_none());
        assert!(block_private_host("api.github.com").is_none());
        assert!(block_private_host("example.com").is_none());
        // A subdomain whose parent happens to be one of the blocked
        // names is not blocked — only exact `localhost` and the
        // `.localhost`/`.local` suffixes.
        assert!(block_private_host("notlocalhost.example.com").is_none());
    }
}

#[cfg(test)]
mod coverage_html_conversion {
    //! The HTML→text converter is what turns a documentation page
    //! into something the model can read. A regression that left
    //! tags in the output would waste tokens on markup; one that
    //! dropped entities would corrupt quotes and code samples.
    use super::*;

    #[test]
    fn decodes_the_common_entities() {
        // The five the converter handles: `&amp;`, `&lt;`, `&gt;`,
        // `&quot;`, `&#39;`. Each appears in real documentation.
        let html = "a &amp; b &lt; c &gt; d &quot;e&quot; &#39;f&#39;";
        let text = html_to_text(html);
        assert!(text.contains("a & b < c > d \"e\" 'f'"), "got: {text}");
    }

    #[test]
    fn decodes_nbsp_mdash_and_hellip() {
        let html = "a&nbsp;b&mdash;c&hellip;";
        let text = html_to_text(html);
        assert!(text.contains("a b—c…"), "got: {text}");
    }

    #[test]
    fn amp_is_decoded_after_other_entities() {
        // `&amp;lt;` is the escaped form of the literal string
        // `&lt;`. Decoding `&amp;` first would produce `<`, which
        // is wrong. The order in the converter is deliberate.
        let text = html_to_text("&amp;lt;");
        assert_eq!(text, "&lt;");
    }

    #[test]
    fn svg_and_template_content_is_dropped() {
        // Both can be huge and are never prose.
        let html = "<svg><path d=\"M0 0\"/></svg>between<template>tpl</template>";
        let text = html_to_text(html);
        assert!(!text.contains("M0 0"), "svg leaked: {text}");
        assert!(!text.contains("tpl"), "template leaked: {text}");
        assert!(text.contains("between"), "between lost: {text}");
    }

    #[test]
    fn noscript_content_is_dropped() {
        let html = "before<noscript>enable js</noscript>after";
        let text = html_to_text(html);
        assert!(!text.contains("enable js"), "noscript leaked: {text}");
        assert!(text.contains("before"));
        assert!(text.contains("after"));
    }

    #[test]
    fn unterminated_script_element_drops_the_tail() {
        // A malformed server response can send `<script>` without a
        // closing tag. The converter treats an unmatched open tag
        // as "swallow everything to the end", which is the safest
        // degradation.
        let html = "visible<script>var x = 1; // never closes";
        let text = html_to_text(html);
        assert!(text.contains("visible"));
        assert!(!text.contains("var x"), "script body leaked: {text}");
    }
    #[test]
    fn consecutive_tags_do_not_produce_multiple_spaces() {
        // `<b></b><i></i>text` must not produce "  text" (leading
        // spaces) or "  " between two text runs. The tag-boundary
        // space rule is deliberately "one, and only when the
        // previous char is not already whitespace".
        let text = html_to_text("<b></b><i></i>text");
        assert!(text.starts_with("text"), "leading space: {text:?}");
    }

    #[test]
    fn attributes_with_angle_brackets_are_handled() {
        // A `<` inside an attribute value is legal-ish HTML and
        // common in templating. The converter's tag skipper
        // terminates at the first `>`, which may cut the tag
        // short — the observed output is documented here.
        let html = "<a title=\"a > b\">link</a>";
        let text = html_to_text(html);
        assert!(text.contains("link"));
    }
}
