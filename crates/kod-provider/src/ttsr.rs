//! TTSR: time-traveling stream rules (borrow from oh-my-pi, delta
//! §14.3).
//!
//! # What this is
//!
//! A rule matched against the model's *streaming output*. When a rule
//! fires, the caller aborts the stream, injects the rule's content as
//! a corrective message, and retries — the rule "travels back in time"
//! into the generation that triggered it.
//!
//! # Example rules
//!
//! * "the diff must not contain a `TODO`" — matched against the text
//!   of an `edit` tool call's arguments as they stream.
//! * "always run the test suite after editing a `.rs` file" — matched
//!   against the tool call's target, so the reminder fires before the
//!   turn ends.
//! * "never write a secret-shaped string in prose" — matched against
//!   assistant text.
//!
//! # Scope and interruption
//!
//! A rule names where it matches ([`RuleScope`]: assistant prose,
//! reasoning, or a tool call whose name/path matches a pattern) and how
//! it interrupts ([`InterruptMode`]: never, prose-only, tool-only, or
//! always). A rule that matches but whose interrupt mode says "do not
//! interrupt here" still records the hit — a caller can log it without
//! aborting.
//!
//! # Repeat control
//!
//! A rule that fires every turn is noise. [`RepeatMode::Once`] fires
//! once per session; [`RepeatMode::Gap`] fires again only after `n`
//! turns have passed since the last fire.
//!
//! # What this is NOT
//!
//! * Not the abort. It answers "did a rule fire, and with what
//!   correction"; cancelling the stream is the caller's job.
//! * Not the AST conditions. The design matches an optional ast-grep
//!   pattern against a reconstructed edit snapshot. kod has tree-sitter
//!   only behind `kod-lsp`; the AST half is a documented follow-up.
//!   The regex half — which covers the shipped rules — lands here.

use regex::Regex;

/// Where a rule matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleScope {
    /// Assistant prose (`StreamChunk::Text`).
    Text,
    /// Reasoning (`StreamChunk::Text` when the caller separates it).
    Thinking,
    /// A tool call's streamed arguments. `tool_pattern` matches the
    /// tool name (a regex); `path_pattern` matches the file path when
    /// the caller can extract one from the partial arguments.
    Tool {
        tool_pattern: String,
        path_pattern: Option<String>,
    },
}

/// How a rule interrupts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptMode {
    /// Record the hit but never abort.
    Never,
    /// Abort only when the match is in prose.
    ProseOnly,
    /// Abort only when the match is in a tool call.
    ToolOnly,
    /// Abort on any match.
    Always,
}

/// Whether a fired rule may fire again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepeatMode {
    /// Once per session.
    Once,
    /// Again only after `n` turns since the last fire.
    Gap(u32),
}

/// One rule.
#[derive(Debug, Clone)]
pub struct Rule {
    pub id: String,
    /// The pattern matched against the streamed text. A regex.
    pub pattern: String,
    /// What the caller injects when the rule fires.
    pub correction: String,
    pub scope: RuleScope,
    pub interrupt: InterruptMode,
    pub repeat: RepeatMode,
}

/// A compiled rule (the regex pre-built).
struct CompiledRule {
    rule: Rule,
    regex: Regex,
    tool_regex: Option<Regex>,
    path_regex: Option<Regex>,
    /// The turn number of the last fire, or `None` for never.
    last_fired_turn: Option<u32>,
    /// How many times it has fired.
    fires: u32,
}

/// A rule fired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FiredRule {
    pub id: String,
    pub correction: String,
    /// Whether the caller should abort the stream.
    pub interrupt: bool,
}

/// The rule engine.
pub struct TtsrEngine {
    rules: Vec<CompiledRule>,
    /// The current turn, advanced by the caller once per turn.
    turn: u32,
}

impl TtsrEngine {
    /// Build the engine, compiling each rule's regex. A rule whose
    /// pattern does not compile is dropped with a warning — a bad rule
    /// must not stop the stream.
    pub fn new(rules: Vec<Rule>) -> Self {
        let mut compiled = Vec::new();
        for rule in rules {
            let regex = match Regex::new(&rule.pattern) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(rule = %rule.id, error = %e, "ttsr: bad pattern; rule dropped");
                    continue;
                }
            };
            let tool_regex = match &rule.scope {
                RuleScope::Tool { tool_pattern, .. } => match Regex::new(tool_pattern) {
                    Ok(r) => Some(r),
                    Err(e) => {
                        tracing::warn!(rule = %rule.id, error = %e, "ttsr: bad tool pattern; rule dropped");
                        continue;
                    }
                },
                _ => None,
            };
            let path_regex = match &rule.scope {
                RuleScope::Tool { path_pattern: Some(p), .. } => match Regex::new(p) {
                    Ok(r) => Some(r),
                    Err(e) => {
                        tracing::warn!(rule = %rule.id, error = %e, "ttsr: bad path pattern; rule dropped");
                        continue;
                    }
                },
                _ => None,
            };
            compiled.push(CompiledRule {
                rule,
                regex,
                tool_regex,
                path_regex,
                last_fired_turn: None,
                fires: 0,
            });
        }
        Self { rules: compiled, turn: 0 }
    }

    /// Advance to the next turn.
    pub fn begin_turn(&mut self) {
        self.turn += 1;
    }

    /// How many rules are loaded.
    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    /// Match assistant prose.
    pub fn observe_text(&mut self, text: &str) -> Vec<FiredRule> {
        self.observe(text, MatchSite::Text, None, None)
    }

    /// Match a tool call's streamed text.
    ///
    /// `tool_name` and `path` are the caller's best extraction from the
    /// partial arguments; a `None` path means the caller could not
    /// extract one yet, and a rule with a `path_pattern` does not fire.
    pub fn observe_tool(&mut self, tool_name: &str, path: Option<&str>, text: &str) -> Vec<FiredRule> {
        self.observe(text, MatchSite::Tool, Some(tool_name), path)
    }

    fn observe(
        &mut self,
        text: &str,
        site: MatchSite,
        tool_name: Option<&str>,
        path: Option<&str>,
    ) -> Vec<FiredRule> {
        let mut fired = Vec::new();
        for cr in &mut self.rules {
            if !scope_allows(&cr.rule.scope, site) {
                continue;
            }
            // A tool rule's name/path filters.
            if let RuleScope::Tool { .. } = &cr.rule.scope {
                if let (Some(re), Some(name)) = (&cr.tool_regex, tool_name)
                    && !re.is_match(name)
                {
                    continue;
                }
                if let Some(pre) = &cr.path_regex {
                    match path {
                        Some(p) if pre.is_match(p) => {}
                        _ => continue,
                    }
                }
            }
            if !cr.regex.is_match(text) {
                continue;
            }
            // Repeat control.
            match cr.rule.repeat {
                RepeatMode::Once => {
                    if cr.fires > 0 {
                        continue;
                    }
                }
                RepeatMode::Gap(n) => {
                    if let Some(last) = cr.last_fired_turn
                        && self.turn.saturating_sub(last) < n
                    {
                        continue;
                    }
                }
            }
            cr.last_fired_turn = Some(self.turn);
            cr.fires += 1;
            let interrupt = interrupt_allows(cr.rule.interrupt, site);
            fired.push(FiredRule {
                id: cr.rule.id.clone(),
                correction: cr.rule.correction.clone(),
                interrupt,
            });
        }
        fired
    }
}

/// Which site a match came from, for the interrupt decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MatchSite {
    Text,
    Tool,
}

fn scope_allows(scope: &RuleScope, site: MatchSite) -> bool {
    match (scope, site) {
        (RuleScope::Text, MatchSite::Text) => true,
        // `Thinking` shares the text site: the caller that wants to
        // separate reasoning does so before calling.
        (RuleScope::Thinking, MatchSite::Text) => true,
        (RuleScope::Tool { .. }, MatchSite::Tool) => true,
        _ => false,
    }
}

fn interrupt_allows(mode: InterruptMode, site: MatchSite) -> bool {
    match (mode, site) {
        (InterruptMode::Never, _) => false,
        (InterruptMode::Always, _) => true,
        (InterruptMode::ProseOnly, MatchSite::Text) => true,
        (InterruptMode::ProseOnly, MatchSite::Tool) => false,
        (InterruptMode::ToolOnly, MatchSite::Tool) => true,
        (InterruptMode::ToolOnly, MatchSite::Text) => false,
    }
}

/// A convenience: build the shipped rules the design names.
pub fn builtin_rules() -> Vec<Rule> {
    vec![
        Rule {
            id: "no-todo-in-diff".to_string(),
            pattern: r"\bTODO\b".to_string(),
            correction: "Do not leave a TODO in the code you write. Implement it, or \
                         open an issue and link it."
                .to_string(),
            scope: RuleScope::Tool {
                tool_pattern: "edit|write_file|patch_file".to_string(),
                path_pattern: None,
            },
            interrupt: InterruptMode::ToolOnly,
            repeat: RepeatMode::Once,
        },
        Rule {
            id: "no-secret-shaped-prose".to_string(),
            pattern: r"(sk-[A-Za-z0-9]{20,}|ghp_[A-Za-z0-9]{36,}|AKIA[0-9A-Z]{16})"
                .to_string(),
            correction: "Do not write a credential-shaped string in your reply. \
                         Refer to the secret by name, not by value."
                .to_string(),
            scope: RuleScope::Text,
            interrupt: InterruptMode::Always,
            repeat: RepeatMode::Once,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(id: &str, pattern: &str, scope: RuleScope, interrupt: InterruptMode, repeat: RepeatMode) -> Rule {
        Rule {
            id: id.to_string(),
            pattern: pattern.to_string(),
            correction: format!("{id} correction"),
            scope,
            interrupt,
            repeat,
        }
    }

    #[test]
    fn a_text_rule_fires_on_prose() {
        let mut e = TtsrEngine::new(vec![rule(
            "r",
            "TODO",
            RuleScope::Text,
            InterruptMode::Always,
            RepeatMode::Once,
        )]);
        let fired = e.observe_text("there is a TODO here");
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].id, "r");
        assert!(fired[0].interrupt);
    }

    #[test]
    fn a_text_rule_does_not_fire_on_a_tool_call() {
        let mut e = TtsrEngine::new(vec![rule(
            "r",
            "TODO",
            RuleScope::Text,
            InterruptMode::Always,
            RepeatMode::Once,
        )]);
        assert!(e.observe_tool("edit", None, "TODO").is_empty());
    }

    #[test]
    fn a_tool_rule_fires_on_a_matching_tool() {
        let mut e = TtsrEngine::new(vec![rule(
            "r",
            "TODO",
            RuleScope::Tool {
                tool_pattern: "edit".to_string(),
                path_pattern: None,
            },
            InterruptMode::ToolOnly,
            RepeatMode::Once,
        )]);
        let fired = e.observe_tool("edit", None, "content with TODO");
        assert_eq!(fired.len(), 1);
    }

    #[test]
    fn a_tool_rule_does_not_fire_on_a_non_matching_tool() {
        let mut e = TtsrEngine::new(vec![rule(
            "r",
            "TODO",
            RuleScope::Tool {
                tool_pattern: "edit".to_string(),
                path_pattern: None,
            },
            InterruptMode::ToolOnly,
            RepeatMode::Once,
        )]);
        assert!(e.observe_tool("read_file", None, "TODO").is_empty());
    }

    #[test]
    fn a_path_pattern_filters_a_tool_rule() {
        let mut e = TtsrEngine::new(vec![rule(
            "r",
            "TODO",
            RuleScope::Tool {
                tool_pattern: "edit".to_string(),
                path_pattern: Some(r"\.rs$".to_string()),
            },
            InterruptMode::ToolOnly,
            RepeatMode::Once,
        )]);
        assert!(e.observe_tool("edit", Some("src/a.rs"), "TODO").len() == 1);
        assert!(e.observe_tool("edit", Some("README.md"), "TODO").is_empty());
        // A missing path does not fire a path-filtered rule.
        assert!(e.observe_tool("edit", None, "TODO").is_empty());
    }

    #[test]
    fn interrupt_never_records_without_interrupting() {
        let mut e = TtsrEngine::new(vec![rule(
            "r",
            "TODO",
            RuleScope::Text,
            InterruptMode::Never,
            RepeatMode::Once,
        )]);
        let fired = e.observe_text("TODO");
        assert_eq!(fired.len(), 1);
        assert!(!fired[0].interrupt);
    }

    #[test]
    fn prose_only_does_not_interrupt_a_tool_match() {
        let mut e = TtsrEngine::new(vec![rule(
            "r",
            "TODO",
            RuleScope::Tool {
                tool_pattern: "edit".to_string(),
                path_pattern: None,
            },
            InterruptMode::ProseOnly,
            RepeatMode::Once,
        )]);
        let fired = e.observe_tool("edit", None, "TODO");
        assert_eq!(fired.len(), 1);
        assert!(!fired[0].interrupt, "prose-only must not interrupt a tool match");
    }

    #[test]
    fn tool_only_does_not_interrupt_a_prose_match() {
        let mut e = TtsrEngine::new(vec![rule(
            "r",
            "TODO",
            RuleScope::Text,
            InterruptMode::ToolOnly,
            RepeatMode::Once,
        )]);
        let fired = e.observe_text("TODO");
        assert_eq!(fired.len(), 1);
        assert!(!fired[0].interrupt);
    }

    #[test]
    fn a_once_rule_fires_once() {
        let mut e = TtsrEngine::new(vec![rule(
            "r",
            "TODO",
            RuleScope::Text,
            InterruptMode::Always,
            RepeatMode::Once,
        )]);
        assert_eq!(e.observe_text("TODO").len(), 1);
        assert!(e.observe_text("TODO again").is_empty());
    }

    #[test]
    fn a_gap_rule_refires_after_the_gap() {
        let mut e = TtsrEngine::new(vec![rule(
            "r",
            "TODO",
            RuleScope::Text,
            InterruptMode::Always,
            RepeatMode::Gap(3),
        )]);
        // Turn 0: fires.
        assert_eq!(e.observe_text("TODO").len(), 1);
        // Turns 1, 2: suppressed.
        for _ in 0..2 {
            e.begin_turn();
            assert!(e.observe_text("TODO").is_empty());
        }
        // Turn 3: gap elapsed, fires again.
        e.begin_turn();
        assert_eq!(e.observe_text("TODO").len(), 1);
    }

    #[test]
    fn a_bad_pattern_is_dropped() {
        let mut e = TtsrEngine::new(vec![rule(
            "bad",
            "(",
            RuleScope::Text,
            InterruptMode::Always,
            RepeatMode::Once,
        )]);
        assert_eq!(e.rule_count(), 0);
        assert!(e.observe_text("anything").is_empty());
    }

    #[test]
    fn a_bad_tool_pattern_drops_the_rule() {
        let e = TtsrEngine::new(vec![rule(
            "bad",
            "x",
            RuleScope::Tool {
                tool_pattern: "(".to_string(),
                path_pattern: None,
            },
            InterruptMode::ToolOnly,
            RepeatMode::Once,
        )]);
        assert_eq!(e.rule_count(), 0);
    }

    #[test]
    fn multiple_rules_all_fire() {
        let mut e = TtsrEngine::new(vec![
            rule("a", "TODO", RuleScope::Text, InterruptMode::Always, RepeatMode::Once),
            rule("b", "FIXME", RuleScope::Text, InterruptMode::Always, RepeatMode::Once),
        ]);
        let fired = e.observe_text("TODO and FIXME");
        assert_eq!(fired.len(), 2);
    }

    #[test]
    fn the_builtin_rules_compile() {
        let e = TtsrEngine::new(builtin_rules());
        assert_eq!(e.rule_count(), 2);
    }

    #[test]
    fn the_builtin_todo_rule_matches_an_edit() {
        let mut e = TtsrEngine::new(builtin_rules());
        let fired = e.observe_tool("edit", None, "fn x() { // TODO }");
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].id, "no-todo-in-diff");
        assert!(fired[0].interrupt);
    }

    #[test]
    fn the_builtin_secret_rule_matches_prose() {
        let mut e = TtsrEngine::new(builtin_rules());
        let fired = e.observe_text("the key is sk-abcdefghijklmnopqrstuv");
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].id, "no-secret-shaped-prose");
        assert!(fired[0].interrupt);
    }
}
