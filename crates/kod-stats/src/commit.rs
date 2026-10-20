//! Conventional-commit validation (borrow from oh-my-pi, delta
//! §14.4).
//!
//! # What this is
//!
//! `omp commit` runs a disposable agent that reads the diff and
//! proposes a structured commit. The *runner*, not the model, decides
//! whether a proposal is complete: a summary under 72 characters, at
//! most six detail items, a priority-scored change list, and a type
//! consistent with the paths touched.
//!
//! That validation is the half that has no model in it — pure checks
//! over a proposal struct. It lands here.
//!
//! # The type-consistency rule
//!
//! A `docs:` commit that only touches `.rs` files is wrong, and so is
//! a `build:` commit that only touches markdown. The rule maps a
//! commit type to the path patterns it implies; a proposal whose type
//! and paths disagree is rejected so the runner can re-prompt.
//!
//! # What this is NOT
//!
//! * Not the agent. Running a model over the diff is the caller's job.
//! * Not the git invocation. This validates a proposal; committing it
//!   is `git`.
//! * Not the map-reduce fallback for huge diffs; that is a separate
//!   pass.

/// A proposed commit.
#[derive(Debug, Clone, PartialEq)]
pub struct CommitProposal {
    /// The conventional-commit type (`feat`, `fix`, `docs`, …).
    pub r#type: String,
    /// An optional scope.
    pub scope: Option<String>,
    /// The one-line summary.
    pub summary: String,
    /// The detail bullets.
    pub details: Vec<String>,
    /// The paths the commit touches.
    pub changed_paths: Vec<String>,
    /// A priority-scored change list.
    pub priority_changes: Vec<PriorityChange>,
}

/// One change with a priority score.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriorityChange {
    pub description: String,
    pub score: i32,
}

/// The commit types the validator recognizes.
pub const TYPES: &[&str] = &[
    "feat", "fix", "docs", "style", "refactor", "perf", "test", "build", "ci", "chore", "revert",
];

/// The summary length cap.
pub const MAX_SUMMARY_CHARS: usize = 72;

/// The detail-item cap.
pub const MAX_DETAILS: usize = 6;

/// Priority keyword weights.
///
/// A change mentioning one of these gets the weight added to its base
/// score. The design's numbers: security +100, breaking +90, perf +80,
/// bug +70.
pub const PRIORITY_WEIGHTS: &[(&str, i32)] = &[
    ("security", 100),
    ("vulnerability", 100),
    ("cve", 100),
    ("breaking", 90),
    ("incompatible", 90),
    ("perf", 80),
    ("performance", 80),
    ("bug", 70),
    ("fix", 70),
];

/// Why a proposal was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectReason {
    /// The type is not a known conventional type.
    UnknownType(String),
    /// The summary is over the cap.
    SummaryTooLong(usize),
    /// The summary is empty.
    SummaryEmpty,
    /// Too many detail items.
    TooManyDetails(usize),
    /// No changed paths.
    NoPaths,
    /// The type and the paths disagree.
    TypePathMismatch { r#type: String, path: String },
}

impl std::fmt::Display for RejectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownType(t) => write!(f, "unknown commit type `{t}`"),
            Self::SummaryTooLong(n) => {
                write!(f, "summary is {n} chars, over the {MAX_SUMMARY_CHARS} cap")
            }
            Self::SummaryEmpty => write!(f, "summary is empty"),
            Self::TooManyDetails(n) => {
                write!(f, "{n} detail items, over the {MAX_DETAILS} cap")
            }
            Self::NoPaths => write!(f, "no changed paths"),
            Self::TypePathMismatch { r#type, path } => {
                // `r#type` cannot appear inside a format string (raw
                // identifiers are not supported there); pass it as a
                // positional argument.
                write!(f, "`{}:` commit touches `{path}`", r#type)
            }
        }
    }
}

impl std::error::Error for RejectReason {}

/// The path patterns each type implies. A type whose list is empty
/// accepts any path.
fn type_path_rule(t: &str) -> &'static [&'static str] {
    match t {
        "docs" => &[".md", ".mdx", ".rst", ".txt", "docs/"],
        "ci" => &[".github/", ".gitlab-ci", ".circleci", "ci/"],
        "build" => &[
            "Cargo.toml",
            "Cargo.lock",
            "package.json",
            "Makefile",
            "build.rs",
            "Dockerfile",
        ],
        // Other types are path-agnostic.
        _ => &[],
    }
}

/// Score a change description by its priority keywords.
///
/// The base score is the count of keyword hits; each hit adds its
/// weight.
pub fn score_change(description: &str) -> i32 {
    let lower = description.to_ascii_lowercase();
    let mut score = 0;
    for (kw, weight) in PRIORITY_WEIGHTS {
        if lower.contains(kw) {
            score += weight;
        }
    }
    score
}

/// Validate a proposal. `Ok(())` when it is complete; `Err` names the
/// first failure.
pub fn validate(proposal: &CommitProposal) -> Result<(), RejectReason> {
    if !TYPES.contains(&proposal.r#type.as_str()) {
        return Err(RejectReason::UnknownType(proposal.r#type.clone()));
    }
    let summary = proposal.summary.trim();
    if summary.is_empty() {
        return Err(RejectReason::SummaryEmpty);
    }
    let chars = summary.chars().count();
    if chars > MAX_SUMMARY_CHARS {
        return Err(RejectReason::SummaryTooLong(chars));
    }
    if proposal.details.len() > MAX_DETAILS {
        return Err(RejectReason::TooManyDetails(proposal.details.len()));
    }
    if proposal.changed_paths.is_empty() {
        return Err(RejectReason::NoPaths);
    }
    // Type-path consistency.
    let patterns = type_path_rule(&proposal.r#type);
    if !patterns.is_empty()
        && !proposal
            .changed_paths
            .iter()
            .any(|p| patterns.iter().any(|pat| p.contains(pat)))
    {
        return Err(RejectReason::TypePathMismatch {
            r#type: proposal.r#type.clone(),
            path: proposal.changed_paths[0].clone(),
        });
    }
    Ok(())
}

/// Format a validated proposal as a conventional-commit message.
pub fn format_message(proposal: &CommitProposal) -> String {
    let header = match &proposal.scope {
        Some(s) => format!("{}({s}): {}", proposal.r#type, proposal.summary.trim()),
        None => format!("{}: {}", proposal.r#type, proposal.summary.trim()),
    };
    if proposal.details.is_empty() {
        return header;
    }
    let mut out = header;
    out.push_str("\n\n");
    for d in &proposal.details {
        out.push_str("- ");
        out.push_str(d.trim());
        out.push('\n');
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proposal() -> CommitProposal {
        CommitProposal {
            r#type: "feat".to_string(),
            scope: None,
            summary: "add the widget".to_string(),
            details: vec!["one".to_string(), "two".to_string()],
            changed_paths: vec!["src/widget.rs".to_string()],
            priority_changes: vec![],
        }
    }

    #[test]
    fn a_valid_proposal_passes() {
        assert!(validate(&proposal()).is_ok());
    }

    #[test]
    fn an_unknown_type_is_rejected() {
        let mut p = proposal();
        p.r#type = "wibble".to_string();
        assert!(matches!(validate(&p), Err(RejectReason::UnknownType(_))));
    }

    #[test]
    fn every_known_type_is_accepted() {
        for t in TYPES {
            let mut p = proposal();
            p.r#type = t.to_string();
            // A path-agnostic type accepts anything; docs/ci/build
            // have a path rule, so give each one a path it accepts.
            p.changed_paths = vec![
                match *t {
                    "docs" => "README.md",
                    "ci" => ".github/workflows/ci.yml",
                    "build" => "Cargo.toml",
                    _ => "src/lib.rs",
                }
                .to_string(),
            ];
            assert!(
                validate(&p).is_ok(),
                "type {t} should be valid: {:?}",
                validate(&p)
            );
        }
    }

    #[test]
    fn an_empty_summary_is_rejected() {
        let mut p = proposal();
        p.summary = "   ".to_string();
        assert_eq!(validate(&p), Err(RejectReason::SummaryEmpty));
    }

    #[test]
    fn a_long_summary_is_rejected() {
        let mut p = proposal();
        p.summary = "x".repeat(MAX_SUMMARY_CHARS + 1);
        assert!(matches!(validate(&p), Err(RejectReason::SummaryTooLong(_))));
    }

    #[test]
    fn a_summary_at_the_cap_passes() {
        let mut p = proposal();
        p.summary = "x".repeat(MAX_SUMMARY_CHARS);
        assert!(validate(&p).is_ok());
    }

    #[test]
    fn too_many_details_are_rejected() {
        let mut p = proposal();
        p.details = (0..MAX_DETAILS + 1).map(|i| format!("d{i}")).collect();
        assert!(matches!(validate(&p), Err(RejectReason::TooManyDetails(_))));
    }

    #[test]
    fn no_paths_is_rejected() {
        let mut p = proposal();
        p.changed_paths.clear();
        assert_eq!(validate(&p), Err(RejectReason::NoPaths));
    }

    #[test]
    fn a_docs_commit_touching_only_rust_is_rejected() {
        let mut p = proposal();
        p.r#type = "docs".to_string();
        p.changed_paths = vec!["src/widget.rs".to_string()];
        assert!(matches!(
            validate(&p),
            Err(RejectReason::TypePathMismatch { .. })
        ));
    }

    #[test]
    fn a_docs_commit_touching_markdown_passes() {
        let mut p = proposal();
        p.r#type = "docs".to_string();
        p.changed_paths = vec!["README.md".to_string()];
        assert!(validate(&p).is_ok());
    }

    #[test]
    fn a_docs_commit_with_one_markdown_among_rust_passes() {
        // The rule is "at least one path matches", not "every path".
        let mut p = proposal();
        p.r#type = "docs".to_string();
        p.changed_paths = vec!["src/widget.rs".to_string(), "README.md".to_string()];
        assert!(validate(&p).is_ok());
    }

    #[test]
    fn a_ci_commit_touching_a_workflow_passes() {
        let mut p = proposal();
        p.r#type = "ci".to_string();
        p.changed_paths = vec![".github/workflows/ci.yml".to_string()];
        assert!(validate(&p).is_ok());
    }

    #[test]
    fn a_ci_commit_touching_only_rust_is_rejected() {
        let mut p = proposal();
        p.r#type = "ci".to_string();
        p.changed_paths = vec!["src/main.rs".to_string()];
        assert!(matches!(
            validate(&p),
            Err(RejectReason::TypePathMismatch { .. })
        ));
    }

    #[test]
    fn a_build_commit_touching_cargo_toml_passes() {
        let mut p = proposal();
        p.r#type = "build".to_string();
        p.changed_paths = vec!["Cargo.toml".to_string()];
        assert!(validate(&p).is_ok());
    }

    #[test]
    fn a_feat_commit_is_path_agnostic() {
        // `feat` has no path rule; any path is fine.
        let mut p = proposal();
        p.changed_paths = vec!["anything".to_string()];
        assert!(validate(&p).is_ok());
    }

    #[test]
    fn a_security_change_scores_highest() {
        assert_eq!(score_change("patch a security vulnerability"), 200);
    }

    #[test]
    fn a_breaking_change_scores_high() {
        assert_eq!(score_change("a breaking change"), 90);
    }

    #[test]
    fn an_ordinary_change_scores_zero() {
        assert_eq!(score_change("rename a variable"), 0);
    }

    #[test]
    fn scoring_is_case_insensitive() {
        assert_eq!(score_change("SECURITY fix"), 170);
    }

    #[test]
    fn the_message_has_the_conventional_shape() {
        let m = format_message(&proposal());
        assert!(m.starts_with("feat: add the widget"), "got: {m}");
        assert!(m.contains("- one"), "got: {m}");
    }

    #[test]
    fn a_scoped_message_includes_the_scope() {
        let mut p = proposal();
        p.scope = Some("widget".to_string());
        let m = format_message(&p);
        assert!(m.starts_with("feat(widget): add the widget"), "got: {m}");
    }

    #[test]
    fn a_message_with_no_details_is_just_the_header() {
        let mut p = proposal();
        p.details.clear();
        let m = format_message(&p);
        assert_eq!(m, "feat: add the widget");
    }
}

#[cfg(test)]
mod score_change_proptests {
    //! Properties for `score_change` and the `PRIORITY_WEIGHTS`
    //! constant table.
    //!
    //! `score_change` is five lines: lowercase, then sum the weight
    //! of every keyword the description contains. The productive
    //! place to find a real bug is the *table*, not the loop — an
    //! empty keyword would add a constant to every score; a
    //! whitespace-containing keyword would fail to match a
    //! caller-passing description; a negative weight would mean
    //! adding text lowers the score. Those are pins on the constant,
    //! and the properties follow from them.

    use super::*;
    use proptest::prelude::*;

    fn safe_text() -> impl Strategy<Value = String> {
        "[a-zA-Z0-9 ]{0,80}"
    }

    proptest! {
        #[test]
        fn prop_deterministic(s in safe_text()) {
            prop_assert_eq!(score_change(&s), score_change(&s));
        }

        /// Case-insensitive: `score_change` calls
        /// `to_ascii_lowercase()` on the input, so ASCII case does
        /// not affect the score.
        #[test]
        fn prop_case_insensitive_ascii(s in safe_text()) {
            let lower = s.to_ascii_lowercase();
            let upper = s.to_ascii_uppercase();
            prop_assert_eq!(score_change(&lower), score_change(&upper));
            prop_assert_eq!(score_change(&s), score_change(&lower));
        }

        /// Upper bound: no input scores more than the sum of the
        /// positive weights. `score_change` only accumulates table
        /// weights, so it cannot exceed their sum.
        #[test]
        fn prop_subset_of_positive_sum(s in safe_text()) {
            let total_positive: i32 = PRIORITY_WEIGHTS
                .iter()
                .filter(|(_, w)| *w > 0)
                .map(|(_, w)| *w)
                .sum();
            prop_assert!(
                score_change(&s) <= total_positive,
                "score {} > positive sum {} for {:?}",
                score_change(&s), total_positive, s,
            );
        }

        /// A description containing a keyword scores at least that
        /// keyword's weight. The keyword contains itself, so its own
        /// weight is always in the sum.
        #[test]
        fn prop_keyword_contributes_at_least_its_weight(
            (kw, _w) in prop::sample::select(PRIORITY_WEIGHTS.to_vec()),
        ) {
            let own_score = score_change(kw);
            let own_weight = PRIORITY_WEIGHTS
                .iter()
                .find(|(k, _)| *k == kw)
                .map(|(_, w)| *w)
                .expect("keyword present");
            prop_assert!(
                own_score >= own_weight,
                "score_change({:?}) = {} < its own weight {}",
                kw, own_score, own_weight,
            );
        }
    }

    /// Control: every keyword is non-empty. An empty keyword would
    /// be a substring of every description, adding its weight to
    /// every score — the constant-match bug in the priority scorer.
    #[test]
    fn control_no_empty_keyword() {
        for (kw, _) in PRIORITY_WEIGHTS {
            assert!(!kw.is_empty(), "empty keyword in PRIORITY_WEIGHTS");
        }
    }

    /// Control: every keyword has no leading or trailing whitespace.
    /// A keyword like `" security"` would fail to match a description
    /// that starts with the word.
    #[test]
    fn control_no_leading_or_trailing_whitespace_in_keyword() {
        for (kw, _) in PRIORITY_WEIGHTS {
            assert_eq!(
                kw.trim(),
                *kw,
                "keyword {:?} has leading/trailing whitespace",
                kw,
            );
        }
    }

    /// Control: every weight is positive. A negative weight would
    /// mean adding text to a description can lower the score, which
    /// contradicts the function's role as a priority signal.
    #[test]
    fn control_all_weights_positive() {
        for (kw, w) in PRIORITY_WEIGHTS {
            assert!(*w > 0, "keyword {:?} has non-positive weight {}", kw, w);
        }
    }

    /// Control: `score_change("")` is `0`. Follows from
    /// `control_no_empty_keyword`, but stated as its own consequence
    /// so a future change to either the table or the loop shows up
    /// here.
    #[test]
    fn control_empty_is_zero() {
        assert_eq!(score_change(""), 0);
    }

    /// Control: a description containing a superset keyword (e.g.
    /// "breaking" contains "break") accumulates both weights. This
    /// is the intended behaviour of a keyword-scoring heuristic —
    /// the more specific signal scores higher — but pinning it means
    /// a future change that switches to first-match-wins fails here
    /// and forces a decision.
    #[test]
    fn control_superset_accumulates() {
        // Find a pair of keywords in the table where one is a
        // substring of the other.
        let mut found = false;
        for (outer, _) in PRIORITY_WEIGHTS {
            for (inner, _) in PRIORITY_WEIGHTS {
                if inner != outer && outer.contains(inner) {
                    let outer_score = score_change(outer);
                    let inner_score = score_change(inner);
                    assert!(
                        outer_score > inner_score,
                        "superset {:?} scored {} (not > than subset {:?} = {})",
                        outer,
                        outer_score,
                        inner,
                        inner_score,
                    );
                    found = true;
                }
            }
        }
        // If the table has no substring pairs, this control is a
        // no-op — that is a valid table shape, not a failure.
        let _ = found;
    }
}

#[cfg(test)]
mod validate_proptests {
    //! Properties for `validate`.
    //!
    //! The single interesting invariant is **no panic on any
    //! proposal**. The function indexes `changed_paths[0]` inside
    //! the `TypePathMismatch` construction, which is safe only
    //! because `changed_paths.is_empty()` is checked earlier in the
    //! same function. A reordering makes that `[0]` panic on an
    //! empty-paths proposal whose type has a non-empty path rule.
    //!
    //! The properties below pin:
    //! * `validate` never panics on any proposal.
    //! * A proposal with empty `changed_paths` returns
    //!   `Err(NoPaths)` for every type in `TYPES` — including
    //!   types whose `type_path_rule` is non-empty. That is the
    //!   exact case that would panic under reordering.
    //!
    //! The example tests pin the accept/reject decisions on
    //! well-formed proposals. These properties pin the *robustness*
    //! of the function to inputs the model can produce.

    use super::*;
    use proptest::prelude::*;

    fn arb_type() -> impl Strategy<Value = String> {
        prop::sample::select(TYPES.to_vec()).prop_map(|s| s.to_string())
    }

    fn arb_unknown_type() -> impl Strategy<Value = String> {
        "[a-z]{1,10}".prop_filter("not a known type", |s| !TYPES.contains(&s.as_str()))
    }

    fn arb_paths(max: usize) -> impl Strategy<Value = Vec<String>> {
        prop::collection::vec("[a-zA-Z0-9/._-]{0,40}", 0..=max)
    }

    fn arb_details(max: usize) -> impl Strategy<Value = Vec<String>> {
        prop::collection::vec("[a-zA-Z0-9 .,]{0,60}", 0..=max)
    }

    fn arb_proposal() -> impl Strategy<Value = CommitProposal> {
        let t = prop_oneof![arb_type(), arb_unknown_type()];
        (
            t,
            prop::option::of("[a-z]{0,10}"),
            "[a-zA-Z0-9 .,]{0,100}",
            arb_details(10),
            arb_paths(6),
        )
            .prop_map(
                |(r#type, scope, summary, details, changed_paths)| CommitProposal {
                    r#type,
                    scope,
                    summary,
                    details,
                    changed_paths,
                    priority_changes: Vec::new(),
                },
            )
    }

    proptest! {
        /// `validate` never panics on any well-formed
        /// `CommitProposal`. This is the property that would fail if
        /// the `changed_paths.is_empty()` check were reordered after
        /// the mismatch construction: the `changed_paths[0]` index
        /// would panic on empty.
        #[test]
        fn prop_validate_never_panics(proposal in arb_proposal()) {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                validate(&proposal)
            }));
            prop_assert!(result.is_ok(), "validate panicked on {:?}", proposal);
        }

        /// A proposal with `changed_paths: []` returns `Err(NoPaths)`
        /// for every type in `TYPES`, regardless of `type_path_rule`.
        /// This is the case that would panic under reordering: for a
        /// type with a non-empty path rule, the mismatch branch
        /// would index `[0]` and panic.
        #[test]
        fn prop_empty_paths_is_no_paths(r#type in arb_type()) {
            let p = CommitProposal {
                r#type,
                scope: None,
                summary: "ok summary".to_string(),
                details: vec![],
                changed_paths: vec![],
                priority_changes: vec![],
            };
            prop_assert_eq!(validate(&p), Err(RejectReason::NoPaths));
        }
    }

    /// Control: the exact scenario that would panic if the check
    /// were reordered. For every type whose `type_path_rule` is
    /// non-empty, an empty-`changed_paths` proposal must still
    /// return `NoPaths` (not panic, and not `TypePathMismatch`).
    #[test]
    fn control_empty_paths_does_not_reach_mismatch_branch() {
        let mut triggered_at_least_once = false;
        for t in TYPES {
            if type_path_rule(t).is_empty() {
                continue;
            }
            triggered_at_least_once = true;
            let p = CommitProposal {
                r#type: t.to_string(),
                scope: None,
                summary: "ok summary".to_string(),
                details: vec![],
                changed_paths: vec![],
                priority_changes: vec![],
            };
            assert_eq!(
                validate(&p),
                Err(RejectReason::NoPaths),
                "type {:?} with empty changed_paths must return NoPaths, not panic or mismatch",
                t,
            );
        }
        assert!(
            triggered_at_least_once,
            "no type in TYPES has a non-empty type_path_rule; this control is vacuous",
        );
    }

    /// Control: a *valid* proposal is accepted. Without this, the
    /// "never panics" property could pass on a `validate` that
    /// returned `Err` unconditionally.
    #[test]
    fn control_valid_proposal_is_accepted() {
        let p = CommitProposal {
            r#type: "feat".to_string(),
            scope: None,
            summary: "add a widget".to_string(),
            details: vec!["the widget does X".to_string()],
            changed_paths: vec!["src/widget.rs".to_string()],
            priority_changes: vec![],
        };
        assert!(validate(&p).is_ok(), "expected Ok, got {:?}", validate(&p));
    }
}
