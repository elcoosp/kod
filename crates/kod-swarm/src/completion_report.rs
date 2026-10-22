//! The completion-report contract.
//!
//! Every swarm subtask prompt ends with an instruction to close the
//! final message with a report block:
//!
//! ```text
//! <completion-report status=done>
//! summary: added the retry wrapper
//! validation: cargo nextest run -p kod-provider  (12 passed)
//! followups: wire the metric into /debug
//! </completion-report>
//! ```
//!
//! The tag-delimited form is deliberate: a model asked for strict JSON
//! after a long agentic loop produces something JSON-*ish* often
//! enough that a strict parser is a liability, while a block with a
//! known open tag and a known close tag survives almost anything —
//! and when the close tag is missing entirely, the text after the
//! open tag is still usable prose.
//!
//! This module is the parser and the render-side contract. It does
//! no I/O and holds no state; the runner calls [`parse`] on each
//! agent's final message and routes the fields to the blackboard and
//! the re-planner.

/// The status an agent reports for its own subtask.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReportStatus {
    Done,
    Failed,
    Blocked,
    /// The block was absent or its status attribute was unrecognised.
    /// The report is still returned — with the raw text as its
    /// summary — so a caller never loses an agent's output to a
    /// formatting mistake.
    #[default]
    Unknown,
}

impl ReportStatus {
    fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "done" => Self::Done,
            "failed" => Self::Failed,
            "blocked" => Self::Blocked,
            _ => Self::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
            Self::Unknown => "unknown",
        }
    }
}

/// One agent's completion report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompletionReport {
    pub status: ReportStatus,
    /// What the agent says it did. Always populated — the raw text is
    /// the fallback when the block is absent.
    pub summary: String,
    /// Commands the agent ran and what came back. `None` when the
    /// agent did not state any; an empty string is treated as `None`
    /// because a `validation:` line with nothing after it carries no
    /// more information than the line's absence.
    pub validation: Option<String>,
    /// Open questions or suggested next steps. One per line in the
    /// block; split here so a consumer does not re-parse.
    pub followups: Vec<String>,
    /// Whether the `<completion-report>` block was present at all.
    ///
    /// Stored, not derived: a block whose status attribute was junk
    /// and which carried only a summary is indistinguishable from
    /// "no block" by its other fields, and the two cases call for
    /// different handling (one is an agent that tried and formatted
    /// wrong, the other is an agent that ignored the contract).
    pub had_block: bool,
}

/// The open tag without its attributes.
const OPEN_PREFIX: &str = "<completion-report";
const CLOSE_TAG: &str = "</completion-report>";

/// Parse the trailing report block out of an agent's final message.
///
/// Never fails. Three outcomes, in order of preference:
///
/// 1. The block is present and well-formed — return its fields.
/// 2. The open tag is present but the close tag is missing (the
///    stream was cut, the model forgot) — take everything after the
///    open tag's line as the block body.
/// 3. No open tag — the whole text becomes the summary, status
///    `Unknown`.
pub fn parse(text: &str) -> CompletionReport {
    let Some(open_at) = text.find(OPEN_PREFIX) else {
        return CompletionReport {
            summary: text.trim().to_string(),
            had_block: false,
            ..Default::default()
        };
    };

    // The open tag's own line carries the status attribute.
    let after_open = &text[open_at..];
    let open_line_end = after_open
        .find('>')
        .map(|i| i + 1)
        .unwrap_or(after_open.len());
    let open_line = &after_open[..open_line_end];
    let status = extract_status(open_line);

    // The body runs from the line after the open tag to the close tag,
    // or to the end of the text when the close tag is missing.
    let body_start = open_at + open_line_end;
    let body_end = text[body_start..]
        .find(CLOSE_TAG)
        .map(|i| body_start + i)
        .unwrap_or(text.len());
    let body = text[body_start..body_end].trim();

    // The prose before the block is the agent's visible answer; keep
    // it only if the block carried no summary of its own.
    let preamble = text[..open_at].trim();

    let mut summary = String::new();
    let mut validation = None;
    let mut followups = Vec::new();
    let mut in_followups = false;

    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = strip_key(line, "summary:") {
            summary = rest.to_string();
            in_followups = false;
        } else if let Some(rest) = strip_key(line, "validation:") {
            if !rest.is_empty() {
                validation = Some(rest.to_string());
            }
            in_followups = false;
        } else if let Some(rest) = strip_key(line, "followups:") {
            if !rest.is_empty() {
                followups.push(rest.to_string());
            }
            in_followups = true;
        } else if in_followups {
            // A continuation line under `followups:` is another item.
            // The block is line-oriented on purpose: a model that
            // writes three bullets after the key gets three items
            // without needing to escape anything.
            followups.push(line.to_string());
        } else if summary.is_empty() {
            // Unkeyed prose before any key: treat as the summary so
            // a block that skipped the `summary:` line still carries
            // its content.
            summary = line.to_string();
        }
    }

    if summary.is_empty() {
        summary = if preamble.is_empty() {
            body.to_string()
        } else {
            preamble.to_string()
        };
    }

    CompletionReport {
        status,
        summary,
        validation,
        followups,
        had_block: true,
    }
}

/// `summary: foo` → `Some("foo")`; anything else → `None`.
fn strip_key<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.strip_prefix(key).map(str::trim)
}

/// Pull `status=done` out of `<completion-report status=done>`. The
/// attribute may be quoted (`status="done"`) — models do both.
fn extract_status(open_line: &str) -> ReportStatus {
    // Fix (03bbb7e): require a whitespace boundary before `status`.
    // The pre-fix `find("status")` matched `xstatus=done`,
    // `some=status=done`, and `my-status=done` as if they were real
    // status attributes, so an agent whose tag carried a descriptive
    // word ending in "status" reported `Done` even though it never
    // named a status. Attributes are whitespace-separated; "preceded
    // by whitespace" is the property the parser actually wants.
    let mut search_at = 0;
    let eq = loop {
        let Some(found) = open_line[search_at..].find("status") else {
            return ReportStatus::Unknown;
        };
        let idx = search_at + found;
        let preceded_by_whitespace = open_line[..idx]
            .chars()
            .last()
            .is_none_or(|c| c.is_whitespace());
        if preceded_by_whitespace {
            break idx;
        }
        search_at = idx + "status".len();
    };
    let after = &open_line[eq + "status".len()..];
    let after = after.trim_start().strip_prefix('=').map(str::trim_start);
    let Some(after) = after else {
        return ReportStatus::Unknown;
    };
    let end = after
        .find(|c: char| c.is_whitespace() || c == '>')
        .unwrap_or(after.len());
    let value = after[..end].trim_matches('"').trim_matches('\'');
    ReportStatus::parse(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_block_parses_every_field() {
        let text = "\
Here is what I did.

<completion-report status=done>
summary: added the retry wrapper to the provider call
validation: cargo nextest run -p kod-provider  (12 passed)
followups: wire the metric into /debug
followups: add a regression test for the 429 path
</completion-report>
";
        let r = parse(text);
        assert_eq!(r.status, ReportStatus::Done);
        assert_eq!(r.summary, "added the retry wrapper to the provider call");
        assert_eq!(
            r.validation.as_deref(),
            Some("cargo nextest run -p kod-provider  (12 passed)"),
        );
        assert_eq!(
            r.followups,
            vec![
                "wire the metric into /debug".to_string(),
                "add a regression test for the 429 path".to_string(),
            ],
        );
    }

    #[test]
    fn followup_continuation_lines_become_items() {
        // The line-oriented form: three bullets after the key, no
        // repeated `followups:` prefix.
        let text = "\
<completion-report status=blocked>
summary: could not reach the server
followups:
- retry once the VPN is up
- ping the platform team
</completion-report>
";
        let r = parse(text);
        assert_eq!(r.status, ReportStatus::Blocked);
        assert_eq!(r.followups.len(), 2);
        assert_eq!(r.followups[0], "- retry once the VPN is up");
    }

    #[test]
    fn missing_close_tag_still_yields_a_report() {
        // The stream was cut, or the model forgot the close. The body
        // runs to the end of the text.
        let text = "\
<completion-report status=done>
summary: wrote the parser
validation: cargo check  (clean)
";
        let r = parse(text);
        assert_eq!(r.status, ReportStatus::Done);
        assert_eq!(r.summary, "wrote the parser");
        assert_eq!(r.validation.as_deref(), Some("cargo check  (clean)"));
    }

    #[test]
    fn absent_block_falls_back_to_the_whole_text() {
        let text = "I looked at the problem and I think it is a race.";
        let r = parse(text);
        assert_eq!(r.status, ReportStatus::Unknown);
        assert_eq!(r.summary, text);
        assert!(!r.had_block);
    }

    #[test]
    fn unrecognised_status_is_unknown_but_the_block_is_still_parsed() {
        let text = "\
<completion-report status=mostly-done>
summary: three of the four tests pass
</completion-report>
";
        let r = parse(text);
        assert_eq!(r.status, ReportStatus::Unknown);
        assert_eq!(r.summary, "three of the four tests pass");
        assert!(r.had_block, "a summary means the block was present");
    }

    #[test]
    fn quoted_status_attribute_is_accepted() {
        // Models emit both `status=done` and `status="done"`.
        let text = "<completion-report status=\"failed\">\nsummary: nope\n</completion-report>";
        assert_eq!(parse(text).status, ReportStatus::Failed);
    }

    #[test]
    fn empty_validation_line_is_treated_as_absent() {
        let text =
            "<completion-report status=done>\nsummary: ok\nvalidation:\n</completion-report>";
        assert!(parse(text).validation.is_none());
    }

    #[test]
    fn unkeyed_prose_before_any_key_becomes_the_summary() {
        let text = "\
<completion-report status=done>
I refactored the loader.
validation: cargo test  (ok)
</completion-report>
";
        let r = parse(text);
        assert_eq!(r.summary, "I refactored the loader.");
        assert_eq!(r.validation.as_deref(), Some("cargo test  (ok)"));
    }

    #[test]
    fn status_parsing_is_case_insensitive() {
        let text = "<completion-report status=DONE>\nsummary: ok\n</completion-report>";
        assert_eq!(parse(text).status, ReportStatus::Done);
    }

    #[test]
    fn the_status_accessor_round_trips_every_variant() {
        for s in [
            ReportStatus::Done,
            ReportStatus::Failed,
            ReportStatus::Blocked,
            ReportStatus::Unknown,
        ] {
            assert_eq!(ReportStatus::parse(s.as_str()), s);
        }
    }
}

#[cfg(test)]
mod proptests {
    //! Properties of `completion_report::parse`.
    //!
    //! `parse` reads the LLM subagent's final message as a tag-delimited
    //! report. Called from `kod-core/src/swarm_runner.rs:1850` and
    //! `kod-core/src/overnight.rs:176` — model-authored text, no panics
    //! permitted.
    //!
    //! The `parse` / `extract_status` arithmetic was hand-checked: each
    //! slice is preceded by a `find` whose result bounds the slice, so
    //! no panic exists here. The failing-first property is
    //! `prop_fake_status_attribute_is_unknown`: `extract_status` does
    //! `open_line.find("status")`, a substring match. An attribute
    //! like `xstatus=done` or `some=status=done` reports `Done` even
    //! though the tag has no `status` attribute. Same class as the
    //! `parse_report` panic (`45aec66`): a boundary parser accepting
    //! input it should reject.
    use super::*;
    use proptest::prelude::*;

    fn valid_block(
        status: &str,
        summary: &str,
        validation: &str,
        followup1: &str,
        followup2: &str,
    ) -> String {
        format!(
            "preamble line\n\
             <completion-report status={status}>\n\
             summary: {summary}\n\
             validation: {validation}\n\
             followups: {followup1}\n\
             {followup2}\n\
             </completion-report>",
        )
    }

    proptest! {
        /// Never panics on arbitrary input.
        #[test]
        fn prop_parse_never_panics(text in any::<String>()) {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = parse(&text);
            }));
            prop_assert!(outcome.is_ok());
        }

        /// A well-formed block round-trips.
        ///
        /// The `summary` / `validation` / `f1` / `f2` strategies all
        /// exclude whitespace on purpose (L46, three occurrences in
        /// this one round): `summary = " "` trims to `""` in the
        /// expected value but `parse` keeps the untrimmed string;
        /// `validation = " "` trims to `""` and `parse` correctly
        /// treats an empty validation as `None`; `f1 = " "` trims to
        /// `""` and `parse` correctly drops it. Every whitespace-
        /// only value the strategy produced exposed a different
        /// aspect of the same rule: the strategy must not produce a
        /// value the code's own trim step changes into something the
        /// assertion treats as different.
        #[test]
        fn prop_valid_block_round_trips(
            status in prop::sample::select(vec!["done", "failed", "blocked"]),
            summary in "[a-zA-Z0-9]{1,40}",
            validation in "[a-zA-Z0-9._-]{1,40}",
            f1 in "[a-zA-Z0-9]{1,30}",
            f2 in "[a-zA-Z0-9]{1,30}",
        ) {
            let text = valid_block(status, &summary, &validation, &f1, &f2);
            let r = parse(&text);
            prop_assert!(r.had_block);
            prop_assert_eq!(r.status, ReportStatus::parse(status));
            prop_assert_eq!(r.summary, summary.trim());
            prop_assert_eq!(r.validation.as_deref(), Some(validation.trim()));
            prop_assert_eq!(r.followups.len(), 2);
            prop_assert_eq!(&r.followups[0], f1.trim());
            prop_assert_eq!(&r.followups[1], f2.trim());
        }

        /// Absent block: summary = trimmed text, had_block = false,
        /// status = Unknown. Text alphabet excludes `<` so the open
        /// prefix cannot appear.
        #[test]
        fn prop_absent_block_falls_back(text in "[a-zA-Z0-9 \n.]{0,200}") {
            let r = parse(&text);
            prop_assert!(!r.had_block);
            prop_assert_eq!(r.status, ReportStatus::Unknown);
            prop_assert_eq!(r.summary, text.trim());
            prop_assert!(r.validation.is_none());
            prop_assert!(r.followups.is_empty());
        }

        /// A real `status=done` attribute still parses to Done — the
        /// positive control for the failing-first property below.
        #[test]
        fn prop_real_status_attribute_is_honoured(
            status in prop::sample::select(vec!["done", "failed", "blocked"]),
        ) {
            let text = format!(
                "<completion-report status={status}>\nsummary: x\n</completion-report>",
            );
            let r = parse(&text);
            prop_assert_eq!(r.status, ReportStatus::parse(status));
        }

        /// **Failing-first.** An attribute containing the substring
        /// "status" (but not being a `status=` attribute) must not
        /// produce a real status. `extract_status` uses
        /// `find("status")`, so `xstatus=done`, `some=status=done`,
        /// etc. currently report Done.
        #[test]
        fn prop_fake_status_attribute_is_unknown(
            fake_attr in prop::sample::select(vec![
                "xstatus=done",
                "some=status=done",
                "my-status=done",
                "anystatus=failed",
                "sub_status=blocked",
            ]),
        ) {
            let text = format!(
                "<completion-report {fake_attr}>\nsummary: x\n</completion-report>",
            );
            let r = parse(&text);
            prop_assert_eq!(
                r.status,
                ReportStatus::Unknown,
                "attribute {:?} produced status {:?}; the tag has no \
                 `status=` attribute",
                fake_attr,
                r.status,
            );
        }

        /// `had_block` is true iff the open prefix is present.
        #[test]
        fn prop_had_block_matches_open_prefix(text in any::<String>()) {
            let expected = text.contains(OPEN_PREFIX);
            prop_assert_eq!(parse(&text).had_block, expected);
        }
    }
}
