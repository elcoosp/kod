//! Surgical elision of heavy text regions (borrow from oh-my-pi,
//! delta §3.2).
//!
//! # What this is
//!
//! The rung above [supersede pruning](crate::prune) on the mechanical
//! context-reduction ladder. Where prune blanks a *stale* tool result,
//! shake blanks a *heavy* region of any message, regardless of whether
//! it is stale. Two cases:
//!
//! * **Whole tool bodies.** A `grep` that returned 800 matches, a
//!   `read_file` of a generated lockfile, a compiler dump. Nothing is
//!   wrong with the result — it is simply too big to keep paying for
//!   on every turn.
//! * **Fenced code and XML blocks inside user/assistant messages.**
//!   A user pastes a 400-line stack trace in a fence; the model's own
//!   reply embeds a large `<analysis>` block. The surrounding prose is
//!   the part that matters, and the block itself can be elided with a
//!   marker pointing at the recovery path.
//!
//! # What this is *not*
//!
//! Shake does not summarize. It does not call an LLM. It replaces an
//! oversized region with a placeholder that names its size and (for a
//! fence) its language, and leaves recovery to the caller's file-read
//! tools. That is the whole point of the doc's "mechanical, no-LLM
//! context reduction": the bytes go, the fact that they *existed* and
//! *what they were* stays visible.
//!
//! # Conservative scan
//!
//! The doc's §3.2 rules, implemented literally:
//!
//! * An **unterminated fence** stops the scan of that message. Not
//!   just the region is ineligible — everything after the opener is
//!   extended to the end of the containing block, so a ` ``` ` that
//!   follows an unclosed ` ```` ` is literal content of the first
//!   fence, not a new opener. This is CommonMark's rule, and it is
//!   what the doc's §3.2 "unterminated fence ⇒ ineligible" reads
//!   literally once you ask what a markdown renderer would have
//!   shown as a code block.
//! * **XML is suppressed inside fences.** The fence scanner runs
//!   first, marks its byte ranges, and the XML scanner skips any
//!   candidate whose opening tag starts inside a marked range.
//! * **Overlap is containment.** A fence that fully contains an XML
//!   block produces one region (the fence), never two overlapping
//!   ones. The check is structural (the fence range contains the XML
//!   range) and cheap, and it means the caller's splice loop never
//!   has to reason about overlap.
//!
//! # What is deliberately not here
//!
//! The doc's §3.2 places "artifact protection" in the *rescue* preset:
//! content that was already offloaded to an `artifact://` document by
//! a prior shake pass is not shaken again. kod has no internal-URL
//! layer yet (that is §7.5), so nothing offloads and nothing needs
//! protecting. When the internal-URL layer lands, [`ShakeConfig`]
//! gains an `artifact_protection: bool` field, the placeholder gains a
//! handle to the offloaded copy, and the rescue preset turns it on.
//! The field's absence is named here so a reader does not wonder.

use kod_types::{ChatMessage, MessageId, MessageRole};
use std::collections::HashSet;

/// A byte range in a message body. Always lands on char boundaries;
/// the scanners refuse a region that would not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    /// Inclusive start byte offset.
    pub start: usize,
    /// Exclusive end byte offset.
    pub end: usize,
}

impl ByteRange {
    pub fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }
    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }
}

/// What kind of region was elided. Carried on the action so a caller
/// that wants to render its own placeholder (or route the elided
/// content somewhere different per kind) has the information without
/// re-parsing the placeholder text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegionKind {
    /// A fenced code block. `language` is the fence's info string,
    /// trimmed, if any.
    Fence { language: Option<String> },
    /// An XML-tagged block, e.g. `<analysis>...</analysis>`. `tag` is
    /// the tag name without angle brackets.
    Xml { tag: String },
    /// A whole tool-result body.
    ToolBody,
}

impl RegionKind {
    /// The placeholder text that replaces the region.
    ///
    /// Renders byte size and a rough token estimate; names the region
    /// kind so a reader can tell what was there; points at recovery.
    /// Kept short on purpose — a placeholder that is itself a
    /// paragraph defeats the goal.
    pub fn placeholder(&self, bytes: usize) -> String {
        let tokens = bytes / 4;
        match self {
            RegionKind::Fence {
                language: Some(lang),
            } => format!(
                "[… {bytes} bytes (~{tokens} tokens) elided by shake ({lang} code fence); \
                 re-read the file if needed]",
            ),
            RegionKind::Fence { language: None } => format!(
                "[… {bytes} bytes (~{tokens} tokens) elided by shake (code fence); \
                 re-read the file if needed]",
            ),
            RegionKind::Xml { tag } => format!(
                "[… {bytes} bytes (~{tokens} tokens) elided by shake (<{tag}> block); \
                 re-read the file if needed]",
            ),
            RegionKind::ToolBody => format!(
                "[… {bytes} bytes (~{tokens} tokens) elided by shake; \
                 re-read the file if needed]",
            ),
        }
    }
}

/// One elision the plan wants to make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShakeAction {
    pub message_id: MessageId,
    pub range: ByteRange,
    pub kind: RegionKind,
    /// Pre-rendered placeholder. Duplicated from `kind` for the
    /// caller's convenience — the caller mutates the message body,
    /// and reaching back into the kind to render a placeholder at
    /// splice time would mean re-deriving the byte count from the
    /// original content, which the caller may no longer have.
    pub placeholder: String,
}

/// A shake plan: the ordered list of elisions to apply.
///
/// Ordered **descending by `start` within each message**, so a caller
/// can splice a message's actions into the content in the order given
/// without re-sorting, and earlier byte offsets stay valid as later
/// ones are removed. Across messages, the plan preserves transcript
/// order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShakePlan {
    pub actions: Vec<ShakeAction>,
}

impl ShakePlan {
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }
    pub fn len(&self) -> usize {
        self.actions.len()
    }

    /// Total bytes the plan will free: the sum of `range.len()` minus
    /// the placeholder length for each action. Negative contributions
    /// are saturated to zero — a placeholder longer than the region
    /// it replaces would be a bug, not a savings, and the gate below
    /// would reject it anyway.
    pub fn estimated_savings_bytes(&self) -> u64 {
        self.actions
            .iter()
            .map(|a| {
                (a.range.len() as u64).saturating_sub(a.placeholder.len() as u64)
            })
            .sum()
    }

    /// [`Self::estimated_savings_bytes`] in the workspace's rough
    /// char-per-token convention.
    pub fn estimated_savings_tokens(&self) -> u64 {
        self.estimated_savings_bytes() / 4
    }
}

/// Rules that decide which regions a shake pass will elide.
#[derive(Debug, Clone)]
pub struct ShakeConfig {
    /// Minimum size, in tokens, of a *fenced block* to be eligible.
    /// The doc's §3.2 `fenceMinTokens`.
    ///
    /// 400 tokens ≈ 1 600 chars. A four-line code snippet is ~50
    /// tokens; a snippet worth eliding is a page of code, a stack
    /// trace, or a pasted log.
    pub fence_min_tokens: u64,

    /// Minimum size, in tokens, of an *XML block* to be eligible.
    /// Deliberately separate from `fence_min_tokens`: XML blocks in
    /// this workspace are usually structural (`<analysis>`,
    /// `<advisory>`) and shorter than a code fence, so a shared
    /// threshold would either over-elide them or under-elide fences.
    ///
    /// Default equals `fence_min_tokens` — the doc's table does not
    /// distinguish them, so keeping the same value preserves the
    /// doc's intent while leaving the knob in place for a caller that
    /// wants to differ.
    pub xml_min_tokens: u64,

    /// Minimum size, in tokens, of a *whole tool-result body* to be
    /// eligible. Larger than the fence threshold on purpose: a
    /// tool body is not a *block* inside prose, it is the entire
    /// content, and eliding a 400-token `read_file` result would
    /// discard the fresh read the model just performed.
    ///
    /// 2 000 tokens ≈ 8 000 chars. That is a small file, a large grep
    /// result, or a compiler dump — the case shake exists for.
    pub tool_body_min_tokens: u64,

    /// Never touch a region in a message whose suffix (everything
    /// after the message) contains this many tokens. Same semantics
    /// as [`crate::prune::PruneConfig::protect_tokens`] but with a
    /// lower default: shake produces smaller, more scattered
    /// mutations than prune, so a smaller protected window is
    /// appropriate.
    pub protect_tokens: u64,

    /// Whole-plan savings gate. A shake pass that frees fewer than
    /// this many tokens is not worth the cache churn it causes. Same
    /// semantics as [`crate::prune::PruneConfig::minimum_savings`].
    pub minimum_savings: u64,

    /// Cache-warm guard threshold. Same semantics as
    /// [`crate::prune::PruneConfig::cache_warm_suffix_tokens`],
    /// including the interaction with `protect_tokens` documented
    /// there: a warm prefix with default settings admits no shake
    /// targets, because the protect window and the cache-warm window
    /// do not overlap.
    pub cache_warm_suffix_tokens: u64,

    /// Message ids that must never be elided.
    ///
    /// The doc's `keepBoundaryId`: the compaction-boundary message is
    /// what a downstream summarizer anchors on, and eliding a region
    /// inside it would corrupt the anchor. kod has no
    /// compaction-boundary concept yet, so a caller fills this set
    /// from whatever mechanism it invents; until then the set is
    /// empty and the rule is inert.
    pub boundary_ids: HashSet<MessageId>,
}

impl Default for ShakeConfig {
    fn default() -> Self {
        Self {
            fence_min_tokens: 400,
            xml_min_tokens: 400,
            tool_body_min_tokens: 2_000,
            protect_tokens: 16_000,
            minimum_savings: 4_000,
            cache_warm_suffix_tokens: 8_000,
            boundary_ids: HashSet::new(),
        }
    }
}

impl ShakeConfig {
    /// The doc's `/shake` preset: protect almost nothing, elide
    /// anything over the fence threshold. Intended for a user who
    /// has hit a hard context limit and wants the maximum available
    /// reduction, accepting that they may lose a recent snippet.
    pub fn aggressive() -> Self {
        Self {
            protect_tokens: 4_000,
            minimum_savings: 0,
            ..Self::default()
        }
    }

    /// The doc's rescue preset: no protection, no savings gate, the
    /// last rung before a hard context-window rejection. Fence
    /// threshold unchanged so the *kind* of content elided does not
    /// broaden — a rescue that shreds prose to save a turn is not a
    /// rescue, it is a different failure.
    ///
    /// The doc's "artifact protection" clause in this preset is not
    /// representable here; see the module doc.
    pub fn rescue() -> Self {
        Self {
            protect_tokens: 0,
            minimum_savings: 0,
            ..Self::default()
        }
    }
}

/// Walk `transcript` and produce a plan for every region the
/// config's rules admit.
///
/// `suffix_tokens_after` and `prefix_is_warm` carry the same meanings
/// as they do in [`crate::prune::plan_prune`]: the caller owns the
/// token estimator and the cache ledger, this module stays a pure
/// function of its inputs.
pub fn plan_shake(
    transcript: &[ChatMessage],
    config: &ShakeConfig,
    suffix_tokens_after: impl Fn(usize) -> u64,
    prefix_is_warm: bool,
) -> ShakePlan {
    // All candidates, grouped by the transcript index of the
    // message they belong to. The savings gate is a whole-plan
    // decision and needs the sum before it can fire, so candidates
    // are collected first.
    let mut candidates: Vec<(usize, ShakeAction)> = Vec::new();

    for (i, msg) in transcript.iter().enumerate() {
        if config.boundary_ids.contains(&msg.id) {
            continue;
        }

        // Suffix filters apply per-message, before region scanning:
        // an entire message inside the protected window is skipped
        // without the cost of scanning it.
        let suffix = suffix_tokens_after(i);
        if suffix < config.protect_tokens {
            continue;
        }
        if prefix_is_warm && suffix > config.cache_warm_suffix_tokens {
            continue;
        }

        match msg.role {
            MessageRole::Tool => {
                // Whole-body elision. The body itself is the region;
                // the min-tokens rule gates on its total size.
                let bytes = msg.content.len();
                let tokens = bytes as u64 / 4;
                if tokens < config.tool_body_min_tokens {
                    continue;
                }
                let kind = RegionKind::ToolBody;
                candidates.push((
                    i,
                    ShakeAction {
                        message_id: msg.id.clone(),
                        range: ByteRange {
                            start: 0,
                            end: bytes,
                        },
                        placeholder: kind.placeholder(bytes),
                        kind,
                    },
                ));
            }
            MessageRole::User | MessageRole::Assistant => {
                // In-place region elision. The scanners run in a
                // fixed order — fences first, then XML with fences
                // masked — so a region is only ever claimed by one
                // scanner.
                let regions = scan_regions(&msg.content);
                for region in regions {
                    let bytes = region.range.len();
                    let tokens = bytes as u64 / 4;
                    let min = match &region.kind {
                        RegionKind::Fence { .. } => config.fence_min_tokens,
                        RegionKind::Xml { .. } => config.xml_min_tokens,
                        RegionKind::ToolBody => config.tool_body_min_tokens,
                    };
                    if tokens < min {
                        continue;
                    }
                    candidates.push((
                        i,
                        ShakeAction {
                            message_id: msg.id.clone(),
                            range: region.range,
                            placeholder: region.kind.placeholder(bytes),
                            kind: region.kind,
                        },
                    ));
                }
            }
            // System messages are the invariant prefix's home; the
            // model's identity and tool inventory live there. Shake
            // does not touch them — a system message that is too big
            // is a different problem (a repo map that should be
            // budgeted down, a tool inventory that should be trimmed)
            // and solving it here would silently change the cacheable
            // prefix on every turn.
            MessageRole::System => {}
            // Agent messages are display-only in kod's transcript;
            // they never reach the wire. No region to elide.
            MessageRole::Agent(_) => {}
        }
    }

    // Whole-plan savings gate.
    let total_freed: u64 = candidates
        .iter()
        .map(|(_, a)| {
            (a.range.len() as u64).saturating_sub(a.placeholder.len() as u64)
        })
        .sum();
    if total_freed / 4 < config.minimum_savings {
        return ShakePlan::default();
    }

    // Order the actions for splicing: within a message, descending
    // by range start; across messages, ascending by transcript
    // index. A caller that walks the plan in order and splices each
    // message from the end backwards does not need to re-sort or
    // re-index.
    candidates.sort_by(|(ia, a), (ib, b)| {
        ia.cmp(ib).then_with(|| b.range.start.cmp(&a.range.start))
    });

    ShakePlan {
        actions: candidates.into_iter().map(|(_, a)| a).collect(),
    }
}

/// One region found in a message body, before eligibility rules.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Region {
    range: ByteRange,
    kind: RegionKind,
}

/// Find every fenced code block and XML block in `content`.
///
/// Fences are found first; their byte ranges are then passed to the
/// XML scanner as masks so a tag inside a fence is never treated as
/// an XML block. The returned regions are non-overlapping, and by
/// construction the fence regions come before any XML region that
/// follows them.
fn scan_regions(content: &str) -> Vec<Region> {
    let fences = find_fences(content);
    let mut out = fences.clone();
    out.extend(find_xml_blocks(content, &fences));
    out
}

// ---------------------------------------------------------------------------
// Fence scanner
// ---------------------------------------------------------------------------

/// A fenced code block, as found by the scanner. `ByteRange` is the
/// whole block including both fence lines.
fn find_fences(content: &str) -> Vec<Region> {
    let mut out = Vec::new();
    let bytes = content.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        let line_start = i;

        // Skip leading whitespace on the line. CommonMark allows up
        // to three leading spaces on a fence; being generous with
        // the whitespace run costs nothing and matches what a
        // markdown renderer will accept.
        let mut j = line_start;
        while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
            j += 1;
        }

        // A fence marker is three or more consecutive backticks or
        // tildes.
        let mut opened: Option<(u8, usize, String)> = None;
        if j < bytes.len() {
            let ch = bytes[j];
            if ch == b'`' || ch == b'~' {
                let mut k = j;
                while k < bytes.len() && bytes[k] == ch {
                    k += 1;
                }
                let marker_len = k - j;
                if marker_len >= 3 {
                    // Info string: everything from the end of the
                    // marker to the end of the line, trimmed.
                    let info_end = content[k..].find('\n').map(|n| k + n).unwrap_or(bytes.len());
                    let info = content[k..info_end].trim().to_string();
                    opened = Some((ch, marker_len, info));
                }
            }
        }

        let Some((ch, marker_len, info)) = opened else {
            // Not a fence opener. Advance to the next line.
            match content[i..].find('\n') {
                Some(nl) => i = i + nl + 1,
                None => break,
            }
            continue;
        };

        // We have an opener. Find the matching closer.
        let after_info_line = match content[i..].find('\n') {
            Some(nl) => i + nl + 1,
            None => bytes.len(),
        };
        match find_fence_closer(content, after_info_line, ch, marker_len) {
            Some((_, close_end)) => {
                // The whole fenced block: from the opener's line
                // start (before any leading whitespace) through the
                // closing fence's line end (past its newline).
                out.push(Region {
                    range: ByteRange {
                        start: line_start,
                        end: close_end,
                    },
                    kind: RegionKind::Fence {
                        language: if info.is_empty() { None } else { Some(info) },
                    },
                });
                i = close_end;
            }
            None => {
                // Unterminated fence: bail out of the whole scan of
                // this message.
                //
                // This is CommonMark's rule, and it is the doc's
                // §3.2 "unterminated fence ⇒ ineligible" read
                // literally: an unclosed fence is extended to the end
                // of its containing block, so a ` ``` ` that follows
                // it is *literal content* of the unterminated fence,
                // not a new opener. Treating it as a new opener would
                // elide a region the user's markdown renderer would
                // never have shown as a block.
                break;
            }
        }
    }

    out
}

/// Find the closing fence for an opener of `ch` at least `marker_len`
/// long, starting the search at `from`. Returns `(line_start, line_end)`
/// of the closing line on success.
///
/// A closer must be:
///
/// * at the start of a line, after optional whitespace;
/// * the same character as the opener;
/// * at least as long as the opener;
/// * followed only by whitespace to end-of-line (no info string).
///
/// The last rule is what keeps `` ```rust `` from closing a fence — a
/// common markdown mistake the strict reading protects against.
fn find_fence_closer(
    content: &str,
    from: usize,
    ch: u8,
    marker_len: usize,
) -> Option<(usize, usize)> {
    let bytes = content.as_bytes();
    let mut i = from;
    while i < bytes.len() {
        let line_start = i;
        let mut j = line_start;
        while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
            j += 1;
        }
        if j < bytes.len() && bytes[j] == ch {
            let mut k = j;
            while k < bytes.len() && bytes[k] == ch {
                k += 1;
            }
            if k - j >= marker_len {
                let line_end = content[k..].find('\n').map(|n| k + n).unwrap_or(bytes.len());
                if content[k..line_end].trim().is_empty() {
                    let end = if line_end < bytes.len() {
                        line_end + 1
                    } else {
                        line_end
                    };
                    return Some((line_start, end));
                }
            }
        }
        match content[i..].find('\n') {
            Some(nl) => i = i + nl + 1,
            None => break,
        }
    }
    None
}

// ---------------------------------------------------------------------------
// XML scanner
// ---------------------------------------------------------------------------

/// Find XML-tagged blocks whose opener is at the start of a line
/// (after optional whitespace) and whose closer is likewise.
///
/// Conservative on purpose. The workspace's own rendered content —
/// `<advisory>`, `<analysis>`, `<completion-report>`, `<memories>` —
/// all take this shape; inline `<tag>` mentions in prose do not, and
/// the line-start rule keeps those from being misread.
///
/// Attributes are not supported. A tag with an attribute
/// (`<foo bar="1">`) is skipped, not misparsed. The workspace's
/// blocks do not carry attributes; when one does, extending
/// `parse_open_tag` is the fix, not loosening the line-start rule.
fn find_xml_blocks(content: &str, fences: &[Region]) -> Vec<Region> {
    let mut out = Vec::new();
    let bytes = content.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        let line_start = i;
        let mut j = line_start;
        while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
            j += 1;
        }

        // Skip if this position is inside any fence.
        if in_any_range(j, fences) {
            match content[i..].find('\n') {
                Some(nl) => i = i + nl + 1,
                None => break,
            }
            continue;
        }

        if j < bytes.len() && bytes[j] == b'<' {
            if let Some((name, open_end)) = parse_open_tag(content, j)
                && let Some(close_end) = find_close_tag(content, open_end, &name)
            {
                // The whole block including the closer's newline.
                let end = if close_end < bytes.len() && bytes[close_end] == b'\n' {
                    close_end + 1
                } else {
                    close_end
                };
                out.push(Region {
                    range: ByteRange {
                        start: line_start,
                        end,
                    },
                    kind: RegionKind::Xml { tag: name },
                });
                i = end;
                continue;
            }
        }

        match content[i..].find('\n') {
            Some(nl) => i = i + nl + 1,
            None => break,
        }
    }

    out
}

/// Parse an opening tag at `start`. Returns `(tag_name, end_byte)`
/// where `end_byte` is just past the closing `>`.
///
/// Recognized form: `<name>`. Rejected: `</name>`, `<!--`, `<?`,
/// `<!`, any tag with whitespace or `=` before the `>`.
fn parse_open_tag(content: &str, start: usize) -> Option<(String, usize)> {
    if !content[start..].starts_with('<') {
        return None;
    }
    let mut chars = content[start..].char_indices();
    chars.next(); // consume '<'
    let (_, first) = chars.next()?;
    if first == '/' || first == '!' || first == '?' || first == ' ' || first == '\t' {
        return None;
    }
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }

    let mut name = String::new();
    name.push(first);
    let mut pos = start + 1 + first.len_utf8();

    while pos < content.len() {
        let c = content[pos..].chars().next()?;
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            name.push(c);
            pos += c.len_utf8();
        } else {
            break;
        }
    }

    // The only terminator we accept is '>' with no preceding
    // whitespace or attribute. This is the "no attributes" rule.
    if content[pos..].starts_with('>') {
        Some((name, pos + 1))
    } else {
        None
    }
}

/// Find `</name>` at the start of a line (after optional whitespace),
/// starting the search at `from`. Returns the byte offset just past
/// the `>`, or `None`.
fn find_close_tag(content: &str, from: usize, name: &str) -> Option<usize> {
    let needle = format!("</{name}>");
    let mut search_from = from;
    while let Some(rel) = content[search_from..].find(&needle) {
        let abs = search_from + rel;
        // Is the closer at the start of a line (after whitespace)?
        let line_start = content[..abs].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let before = &content[line_start..abs];
        if before.chars().all(|c| c == ' ' || c == '\t') {
            return Some(abs + needle.len());
        }
        search_from = abs + needle.len();
    }
    None
}

/// Whether `pos` falls inside any of the given ranges.
fn in_any_range(pos: usize, ranges: &[Region]) -> bool {
    ranges.iter().any(|r| r.range.start <= pos && pos < r.range.end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kod_types::MessageId;
    use time::OffsetDateTime;

    fn user(content: &str) -> ChatMessage {
        ChatMessage::text(
            MessageId::new(),
            MessageRole::User,
            content,
            OffsetDateTime::now_utc(),
        )
    }

    fn assistant(content: &str) -> ChatMessage {
        ChatMessage::text(
            MessageId::new(),
            MessageRole::Assistant,
            content,
            OffsetDateTime::now_utc(),
        )
    }

    fn tool(content: &str) -> ChatMessage {
        ChatMessage::text(
            MessageId::new(),
            MessageRole::Tool,
            content,
            OffsetDateTime::now_utc(),
        )
    }

    /// A config that admits every candidate regardless of gate. Used
    /// by tests that isolate a scanner rule rather than a gate.
    fn ungated() -> ShakeConfig {
        ShakeConfig {
            protect_tokens: 0,
            minimum_savings: 0,
            ..ShakeConfig::default()
        }
    }

    /// Suffix estimator that reports a large number, clearing any
    /// protect window a test uses.
    fn big_suffix() -> impl Fn(usize) -> u64 {
        |_| 1_000_000
    }

    // -----------------------------------------------------------------
    // Fence scanner
    // -----------------------------------------------------------------

    #[test]
    fn a_fenced_block_is_found_by_the_scanner() {
        let content = "before\n```rust\nfn main() {}\n```\nafter";
        let fences = find_fences(content);
        assert_eq!(fences.len(), 1);
        let r = &fences[0];
        assert_eq!(&content[r.range.start..r.range.end], "```rust\nfn main() {}\n```\n");
        match &r.kind {
            RegionKind::Fence { language } => assert_eq!(language.as_deref(), Some("rust")),
            other => panic!("expected fence, got {other:?}"),
        }
    }

    #[test]
    fn a_tilde_fenced_block_is_found() {
        let content = "~~~\nbody\n~~~\n";
        let fences = find_fences(content);
        assert_eq!(fences.len(), 1);
        match &fences[0].kind {
            RegionKind::Fence { language } => assert!(language.is_none()),
            other => panic!("expected fence, got {other:?}"),
        }
    }

    #[test]
    fn a_closer_must_use_the_same_character() {
        // An opener of ``` cannot be closed by ~~~.
        let content = "```\nbody\n~~~\nmore\n";
        assert!(find_fences(content).is_empty());
    }

    #[test]
    fn a_closer_must_be_at_least_as_long_as_the_opener() {
        // Opener of 4 backticks; a 3-backtick closer does not close it.
        let content = "````\nbody\n```\nstill inside\n````\n";
        let fences = find_fences(content);
        assert_eq!(fences.len(), 1);
        let r = &fences[0];
        assert_eq!(
            &content[r.range.start..r.range.end],
            "````\nbody\n```\nstill inside\n````\n",
        );
    }

    #[test]
    fn a_closer_with_an_info_string_is_not_a_closer() {
        // ``````rust`` at the end of a fence is a common mistake; the
        // conservative reading refuses to close on it.
        let content = "```\nbody\n```rust\nstill inside\n```\n";
        let fences = find_fences(content);
        assert_eq!(fences.len(), 1);
        let r = &fences[0];
        assert!(r.range.end >= content.len() - 2, "fence should span to the end");
    }

    #[test]
    fn an_opener_with_no_closer_anywhere_is_not_a_fence() {
        // The doc's §3.2 "unterminated fence ⇒ ineligible" rule.
        // Content with an opener and no matching closer at all: the
        // scanner finds no fences.
        let content = "before\n```rust\nnothing closes this\n";
        let fences = find_fences(content);
        assert!(fences.is_empty(), "no closer ⇒ no fence: got {fences:?}");
    }

    #[test]
    fn an_unterminated_opener_swallows_the_rest_of_the_message() {
        // The doc's §3.2 rule, and CommonMark's: an unclosed fence is
        // extended to the end of the containing block. A ` ``` `
        // that appears after it is *literal content* of the
        // unterminated fence, not a new opener. The scanner
        // therefore bails at the first unterminated opener and
        // reports no fences for the whole message — there is no
        // "later well-formed pair" to find, because the earlier
        // opener swallows everything.
        //
        // Fixture shape (all one unterminated block under CommonMark):
        //
        //   ````            opener (4 backticks)
        //   content
        //   ```             literal content of the unterminated fence
        //   more content
        //   end
        //   ```             literal content, not a closer (3 < 4)
        //   fenced body
        //   ```             literal content
        let content =
            "````\ncontent\n```\nmore content\nend\n```\nfenced body\n```\n";
        let fences = find_fences(content);
        assert!(
            fences.is_empty(),
            "an unterminated opener swallows the rest: got {fences:?}",
        );
    }

    #[test]
    fn two_fences_are_both_found() {
        let content = "```\na\n```\nmid\n```\nb\n```\n";
        let fences = find_fences(content);
        assert_eq!(fences.len(), 2);
    }

    #[test]
    fn a_fence_at_the_start_of_content_is_found() {
        let content = "```\nbody\n```\n";
        assert_eq!(find_fences(content).len(), 1);
    }

    #[test]
    fn a_fence_at_the_end_of_content_with_no_trailing_newline_is_found() {
        let content = "```\nbody\n```";
        let fences = find_fences(content);
        assert_eq!(fences.len(), 1);
        assert_eq!(fences[0].range.end, content.len());
    }

    #[test]
    fn fence_markers_below_three_characters_are_not_fences() {
        let content = "``\nnot a fence\n``\n";
        assert!(find_fences(content).is_empty());
    }

    // -----------------------------------------------------------------
    // XML scanner
    // -----------------------------------------------------------------

    #[test]
    fn an_xml_block_is_found() {
        let content = "prose\n<advisory>\nbody\n</advisory>\nmore prose";
        let regions = find_xml_blocks(content, &[]);
        assert_eq!(regions.len(), 1);
        match &regions[0].kind {
            RegionKind::Xml { tag } => assert_eq!(tag, "advisory"),
            other => panic!("expected xml, got {other:?}"),
        }
    }

    #[test]
    fn xml_tags_not_at_line_start_are_ignored() {
        // An inline `<foo>` in prose is not an XML block.
        let content = "see the <foo> tag for details\n";
        assert!(find_xml_blocks(content, &[]).is_empty());
    }

    #[test]
    fn xml_with_attributes_is_skipped() {
        // The conservative reading: a tag with `=` before `>` is not
        // recognized. A stricter parse may be added later, but the
        // current form is what the workspace emits.
        let content = "<foo bar=\"1\">\nbody\n</foo>\n";
        assert!(find_xml_blocks(content, &[]).is_empty());
    }

    #[test]
    fn xml_with_no_matching_closer_is_ignored() {
        let content = "<advisory>\nbody without close\n";
        assert!(find_xml_blocks(content, &[]).is_empty());
    }

    #[test]
    fn xml_inside_a_fence_is_suppressed() {
        // The doc's §3.2 rule. A fence region is passed as a mask;
        // a tag inside it is not treated as an XML block.
        let content = "```\n<advisory>\nbody\n</advisory>\n```\n";
        let fences = find_fences(content);
        assert_eq!(fences.len(), 1);
        let xml = find_xml_blocks(content, &fences);
        assert!(xml.is_empty(), "XML inside a fence must be masked: {xml:?}");
    }

    #[test]
    fn xml_outside_a_fence_after_a_fenced_region_is_still_found() {
        let content = "```\ncode\n```\n<advisory>\nbody\n</advisory>\n";
        let fences = find_fences(content);
        let xml = find_xml_blocks(content, &fences);
        assert_eq!(xml.len(), 1);
        match &xml[0].kind {
            RegionKind::Xml { tag } => assert_eq!(tag, "advisory"),
            other => panic!("expected xml, got {other:?}"),
        }
    }

    #[test]
    fn a_hyphenated_tag_name_is_accepted() {
        let content = "<some-tag>\nbody\n</some-tag>\n";
        let xml = find_xml_blocks(content, &[]);
        assert_eq!(xml.len(), 1);
        match &xml[0].kind {
            RegionKind::Xml { tag } => assert_eq!(tag, "some-tag"),
            other => panic!("expected xml, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // Whole-message scanning via `scan_regions`
    // -----------------------------------------------------------------

    #[test]
    fn scan_regions_returns_fences_before_xml() {
        // The doc's "overlap is containment": a fence that contains an
        // XML block produces one region, not two. The implementation
        // achieves this by masking, not by overlap detection.
        let content = "before\n```\n<advisory>\nbody\n</advisory>\n```\nafter\n<context>\nx\n</context>\n";
        let regions = scan_regions(content);
        // One fence + one trailing XML block.
        assert_eq!(regions.len(), 2, "got: {regions:?}");
        assert!(matches!(&regions[0].kind, RegionKind::Fence { .. }));
        assert!(matches!(&regions[1].kind, RegionKind::Xml { tag } if tag == "context"));
    }

    #[test]
    fn scan_regions_is_empty_for_plain_prose() {
        assert!(scan_regions("just prose, nothing special").is_empty());
    }

    // -----------------------------------------------------------------
    // plan_shake gates
    // -----------------------------------------------------------------

    #[test]
    fn a_large_fenced_block_in_a_user_message_is_elided() {
        // 2000 chars ≈ 500 tokens, over the 400-token fence threshold.
        let big = "x".repeat(2_000);
        let content = format!("see the trace below\n```\n{big}\n```\nend of message");
        let log = vec![user(&content)];
        let plan = plan_shake(&log, &ungated(), big_suffix(), false);
        assert_eq!(plan.len(), 1);
        let a = &plan.actions[0];
        assert_eq!(a.message_id, log[0].id);
        // The fence region includes the fence markers.
        let sliced = &content[a.range.start..a.range.end];
        assert!(sliced.starts_with("```"));
        assert!(sliced.ends_with("```\n"));
    }

    #[test]
    fn a_large_fenced_block_in_an_assistant_message_is_elided() {
        // Symmetric with `a_large_fenced_block_in_a_user_message_is_elided`.
        // The doc's §3.2 names "large fenced/XML blocks in
        // user/assistant messages" as the target; a regression that
        // only elided user content would pass the user test and fail
        // this one.
        let big = "x".repeat(2_000);
        let content = format!("Here is my analysis:\n```\n{big}\n```\nThat's the conclusion.");
        let log = vec![assistant(&content)];
        let plan = plan_shake(&log, &ungated(), big_suffix(), false);
        assert_eq!(plan.len(), 1);
        let a = &plan.actions[0];
        assert_eq!(a.message_id, log[0].id);
        assert!(matches!(a.kind, RegionKind::Fence { .. }));
    }

    #[test]
    fn a_small_fenced_block_is_not_elided() {
        let content = "hello\n```\nlet x = 1;\n```\nworld";
        let log = vec![user(content)];
        let plan = plan_shake(&log, &ungated(), big_suffix(), false);
        assert!(plan.is_empty());
    }

    #[test]
    fn an_oversized_tool_body_is_elided_whole() {
        // 20 000 chars ≈ 5 000 tokens, over the 2 000-token
        // tool-body threshold.
        let big = "y".repeat(20_000);
        let log = vec![tool(&big)];
        let plan = plan_shake(&log, &ungated(), big_suffix(), false);
        assert_eq!(plan.len(), 1);
        let a = &plan.actions[0];
        assert_eq!(a.range.start, 0);
        assert_eq!(a.range.end, big.len());
        assert!(matches!(a.kind, RegionKind::ToolBody));
    }

    #[test]
    fn a_medium_tool_body_is_not_elided() {
        // 4 000 chars ≈ 1 000 tokens, under the 2 000-token
        // tool-body threshold. A fence-threshold rule would have
        // elided this; the separate knob prevents that.
        let body = "y".repeat(4_000);
        let log = vec![tool(&body)];
        let plan = plan_shake(&log, &ungated(), big_suffix(), false);
        assert!(plan.is_empty(), "medium tool body should be left alone");
    }

    #[test]
    fn the_protect_window_skips_a_message() {
        // A protected message's suffix is under the threshold; the
        // fence inside it is not touched regardless of size.
        let big = "x".repeat(2_000);
        let content = format!("```\n{big}\n```");
        let log = vec![user(&content)];
        let cfg = ShakeConfig {
            protect_tokens: 16_000,
            minimum_savings: 0,
            ..ShakeConfig::default()
        };
        // Suffix 1 000 is under the protect window.
        let plan = plan_shake(&log, &cfg, |_| 1_000, false);
        assert!(plan.is_empty());
    }

    #[test]
    fn the_cache_warm_guard_skips_a_message() {
        // Same interaction as in prune: the guard fires when the
        // prefix is warm AND the suffix exceeds the cache-warm
        // threshold. With default ShakeConfig (protect 16 000,
        // cache-warm 8 000) the two windows do not overlap, so a
        // warm prefix admits nothing. The narrow-band test would
        // require a non-default config, as it does for prune.
        let big = "x".repeat(2_000);
        let content = format!("```\n{big}\n```");
        let log = vec![user(&content)];
        let cfg = ShakeConfig {
            protect_tokens: 0,
            minimum_savings: 0,
            cache_warm_suffix_tokens: 8_000,
            ..ShakeConfig::default()
        };
        // Warm prefix + huge suffix => guard fires.
        let plan = plan_shake(&log, &cfg, |_| 100_000, true);
        assert!(plan.is_empty(), "warm prefix + large suffix must skip");
    }

    #[test]
    fn the_savings_gate_rejects_a_small_plan() {
        // One fence that frees ~500 tokens, under the 4 000-token
        // whole-plan gate. The plan is rejected.
        let big = "x".repeat(2_000);
        let content = format!("```\n{big}\n```");
        let log = vec![user(&content)];
        let plan = plan_shake(&log, &ShakeConfig::default(), big_suffix(), false);
        assert!(plan.is_empty(), "a small plan must not clear the gate");
    }

    #[test]
    fn the_savings_gate_admits_a_large_plan() {
        // A very large fence frees enough on its own.
        let huge = "x".repeat(100_000);
        let content = format!("```\n{huge}\n```");
        let log = vec![user(&content)];
        let plan = plan_shake(&log, &ShakeConfig::default(), big_suffix(), false);
        assert_eq!(plan.len(), 1);
    }

    #[test]
    fn boundary_ids_are_skipped() {
        let big = "x".repeat(100_000);
        let content = format!("```\n{big}\n```");
        let msg = user(&content);
        let mut cfg = ShakeConfig {
            protect_tokens: 0,
            minimum_savings: 0,
            ..ShakeConfig::default()
        };
        cfg.boundary_ids.insert(msg.id.clone());
        let log = vec![msg];
        let plan = plan_shake(&log, &cfg, big_suffix(), false);
        assert!(plan.is_empty(), "a boundary message must be left alone");
    }

    #[test]
    fn system_messages_are_never_shaken() {
        // Even a system message with a giant fenced block is left
        // alone: the system prompt is the cacheable prefix's home.
        let big = "x".repeat(100_000);
        let content = format!("```\n{big}\n```");
        let sys = ChatMessage::text(
            MessageId::new(),
            MessageRole::System,
            content,
            OffsetDateTime::now_utc(),
        );
        let log = vec![sys];
        let plan = plan_shake(&log, &ungated(), big_suffix(), false);
        assert!(plan.is_empty(), "system messages are off limits");
    }

    #[test]
    fn an_empty_transcript_yields_an_empty_plan() {
        let plan = plan_shake(&[], &ungated(), big_suffix(), false);
        assert!(plan.is_empty());
    }

    #[test]
    fn two_fences_in_one_message_produce_two_actions_ordered_descending() {
        // A splice that walks the plan in order must hit the later
        // fence first, so an earlier byte offset stays valid.
        let big = "x".repeat(2_000);
        let content =
            format!("first\n```\n{big}\n```\nmid\n```\n{big}\n```\nend");
        let log = vec![user(&content)];
        let plan = plan_shake(&log, &ungated(), big_suffix(), false);
        assert_eq!(plan.len(), 2);
        assert!(
            plan.actions[0].range.start > plan.actions[1].range.start,
            "actions within a message must be ordered descending by start",
        );
    }

    // -----------------------------------------------------------------
    // Plan arithmetic
    // -----------------------------------------------------------------

    #[test]
    fn savings_are_bytes_elided_minus_placeholder() {
        let a = ShakeAction {
            message_id: MessageId::new(),
            range: ByteRange { start: 0, end: 1_000 },
            kind: RegionKind::ToolBody,
            placeholder: "x".repeat(100),
        };
        let plan = ShakePlan { actions: vec![a] };
        assert_eq!(plan.estimated_savings_bytes(), 900);
        assert_eq!(plan.estimated_savings_tokens(), 225);
    }

    // -----------------------------------------------------------------
    // Presets match the doc
    // -----------------------------------------------------------------

    #[test]
    fn default_preset_matches_the_documented_numbers() {
        let c = ShakeConfig::default();
        assert_eq!(c.fence_min_tokens, 400);
        assert_eq!(c.protect_tokens, 16_000);
        assert_eq!(c.minimum_savings, 4_000);
    }

    #[test]
    fn aggressive_preset_matches_the_documented_numbers() {
        let c = ShakeConfig::aggressive();
        assert_eq!(c.protect_tokens, 4_000);
        assert_eq!(c.minimum_savings, 0);
        // Fence threshold unchanged from default — the preset makes
        // shake *less protective*, not *broader*.
        assert_eq!(c.fence_min_tokens, 400);
    }

    #[test]
    fn rescue_preset_matches_the_documented_numbers() {
        let c = ShakeConfig::rescue();
        assert_eq!(c.protect_tokens, 0);
        assert_eq!(c.minimum_savings, 0);
        assert_eq!(c.fence_min_tokens, 400);
    }

    // -----------------------------------------------------------------
    // Placeholder rendering
    // -----------------------------------------------------------------

    #[test]
    fn a_fence_placeholder_names_the_language() {
        let p = RegionKind::Fence {
            language: Some("rust".into()),
        }
        .placeholder(4_000);
        assert!(p.contains("rust"), "language missing: {p}");
        assert!(p.contains("4000 bytes"), "size missing: {p}");
        assert!(p.contains("1000 tokens"), "token estimate missing: {p}");
    }

    #[test]
    fn an_xml_placeholder_names_the_tag() {
        let p = RegionKind::Xml {
            tag: "advisory".into(),
        }
        .placeholder(1_600);
        assert!(p.contains("advisory"), "tag missing: {p}");
        assert!(p.contains("1600 bytes"), "size missing: {p}");
    }

    #[test]
    fn a_tool_body_placeholder_points_at_recovery() {
        let p = RegionKind::ToolBody.placeholder(8_000);
        assert!(p.contains("re-read the file"), "recovery hint missing: {p}");
    }
}
