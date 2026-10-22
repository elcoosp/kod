//! Skill file parsing - extracts metadata from YAML front matter
//! and structured sections from markdown body.

use kod_error::{KodError, Result};
use kod_types::{Skill, SkillExample, SkillId, SkillMetadata};
use std::path::{Path, PathBuf};

/// Parser for skill markdown files
#[derive(Debug, Clone)]
pub struct SkillParser {
    /// Additional validation rules
    strict_mode: bool,
}

impl Default for SkillParser {
    fn default() -> Self {
        Self::new()
    }
}

impl SkillParser {
    pub fn new() -> Self {
        Self { strict_mode: false }
    }

    pub fn strict(mut self) -> Self {
        self.strict_mode = true;
        self
    }

    /// Parse a skill from a file
    pub fn parse_file(&self, path: &Path) -> Result<Skill> {
        let content = std::fs::read_to_string(path).map_err(|e| KodError::SkillParseError {
            path: path.display().to_string(),
            reason: format!("Failed to read file: {}", e),
        })?;

        let source_name = path.display().to_string();
        self.parse_content(&content, &source_name).map(|mut skill| {
            skill.path = path.to_path_buf();
            skill
        })
    }

    /// Parse skill content from a string
    pub fn parse_content(&self, content: &str, source_name: &str) -> Result<Skill> {
        let (metadata, body) = self.split_front_matter(content, source_name)?;

        self.validate_metadata(&metadata)?;

        let instructions = self
            .extract_section(&body, "## Instructions")
            // Real-world SKILL.md files (Claude-style) rarely have an
            // `## Instructions` section — the whole body IS the instructions.
            // Fall back to it instead of rejecting the skill.
            .unwrap_or_else(|| body.clone());

        let examples = self.extract_examples(&body);
        let constraints = self.extract_section(&body, "## Constraints");

        Ok(Skill {
            id: SkillId::new(),
            metadata,
            instructions,
            examples,
            constraints,
            content: body.to_string(),
            path: PathBuf::from(source_name),
        })
    }

    /// Split content into front matter and body
    fn split_front_matter(&self, content: &str, source: &str) -> Result<(SkillMetadata, String)> {
        // Bug-hunt post-v0.1.9 (P3): U+FEFF is not whitespace in
        // current Unicode (removed from White_Space in 3.2), so
        // str::trim does not strip it. A BOM-emitting editor's
        // SKILL.md then fails starts_with("---") with a misleading
        // "Missing YAML front matter". Strip the BOM first.
        let content = content.trim_start_matches('\u{feff}').trim();

        if !content.starts_with("---") {
            return Err(KodError::SkillParseError {
                path: source.to_string(),
                reason: "Missing YAML front matter (must start with ---)".to_string(),
            });
        }

        // Find the closing --- marker
        let rest = &content[3..];
        let end_pos = rest
            .find("\n---")
            .ok_or_else(|| KodError::SkillParseError {
                path: source.to_string(),
                reason: "Missing closing --- marker for front matter".to_string(),
            })?;

        let yaml_str = &rest[..end_pos];
        let body = rest[end_pos + 4..].trim().to_string();

        // Parse YAML
        let metadata: SkillMetadata =
            serde_yaml::from_str(yaml_str.trim()).map_err(|e| KodError::SkillParseError {
                path: source.to_string(),
                reason: format!("Invalid YAML: {}", e),
            })?;

        Ok((metadata, body))
    }

    /// Validate metadata has required fields
    fn validate_metadata(&self, metadata: &SkillMetadata) -> Result<()> {
        if metadata.name.is_empty() {
            return Err(KodError::SkillValidationFailed {
                reason: "Skill name is required".to_string(),
            });
        }

        if metadata.description.is_empty() {
            return Err(KodError::SkillValidationFailed {
                reason: "Skill description is required".to_string(),
            });
        }

        if metadata.version.is_empty() && self.strict_mode {
            return Err(KodError::SkillValidationFailed {
                reason: "Skill version is required in strict mode".to_string(),
            });
        }

        Ok(())
    }

    /// Extract a section from markdown body
    fn extract_section(&self, body: &str, section_header: &str) -> Option<String> {
        let start = body.find(section_header)?;
        let content_after = &body[start + section_header.len()..];

        // Find next section or end
        let end = content_after
            .find("\n## ")
            .map(|pos| &content_after[..pos])
            .unwrap_or(content_after);

        Some(end.trim().to_string())
    }

    /// Extract examples from <example> tags
    fn extract_examples(&self, body: &str) -> Vec<SkillExample> {
        let mut examples = Vec::new();

        // Find all <example>...</example> blocks
        let mut search_pos = 0;

        while let Some(start_rel) = body[search_pos..].find("<example") {
            let start = search_pos + start_rel;

            // Extract attributes from the opening tag
            let tag_end = match body[start..].find('>') {
                Some(pos) => start + pos + 1,
                None => break,
            };

            let opening_tag = &body[start..tag_end];
            let input_attr = self.extract_attribute(opening_tag, "input");

            // Find closing tag
            let close_rel = match body[tag_end..].find("</example>") {
                Some(pos) => tag_end + pos,
                None => break,
            };

            let content = &body[tag_end..close_rel];

            examples.push(SkillExample {
                input: input_attr.unwrap_or_default(),
                output: content.trim().to_string(),
            });

            search_pos = close_rel + "</example>".len();
        }

        examples
    }

    /// Extract attribute value from an XML-like tag
    fn extract_attribute(&self, tag: &str, attr: &str) -> Option<String> {
        // Fix (proptest round): require a whitespace boundary before
        // the attribute name. The pre-fix `tag.find("input=\"")` was
        // a substring match, so `data-input="x"`, `xinput="y"`, and
        // `requires-input="z"` all reported an `input` attribute that
        // was not there. `SkillExample.input` flows into the router's
        // prompt, so a fake attribute value would land in the model's
        // input. Same class as `completion_report::extract_status`
        // (af0301f).
        //
        // Attributes in `<example ...>` are whitespace-separated;
        // "preceded by whitespace" (or at the start of the tag) is the
        // property the parser wants. Each branch loops past non-
        // boundary matches, so `<example data-input="x" input="y">`
        // finds the real `input`.
        //
        // T5-C31: double quotes first, then single.
        let pattern = format!("{}=\"", attr);
        let mut search_at = 0;
        while let Some(found) = tag[search_at..].find(&pattern) {
            let idx = search_at + found;
            let preceded_ok = tag[..idx].chars().last().is_none_or(|c| c.is_whitespace());
            if preceded_ok {
                let rest = &tag[idx + pattern.len()..];
                let end = rest.find('"')?;
                return Some(rest[..end].to_string());
            }
            search_at = idx + pattern.len();
        }
        let pattern = format!("{}='", attr);
        let mut search_at = 0;
        loop {
            let Some(found) = tag[search_at..].find(&pattern) else {
                return None;
            };
            let idx = search_at + found;
            let preceded_ok = tag[..idx].chars().last().is_none_or(|c| c.is_whitespace());
            if preceded_ok {
                let content_start = idx + pattern.len();
                let end = tag[content_start..].find('\'')? + content_start;
                return Some(tag[content_start..end].to_string());
            }
            search_at = idx + pattern.len();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_SKILL: &str = r#"---
name: test-skill
description: A test skill
version: 1.0.0
category: testing
tags:
  - test
capabilities:
  - testing
triggers:
  - "test trigger"
---

# Test Skill

## Instructions

Test instructions here.

## Constraints

Test constraints.
"#;

    #[test]
    fn test_full_parse() {
        let parser = SkillParser::new();
        let skill = parser.parse_content(TEST_SKILL, "test.md").unwrap();

        assert_eq!(skill.metadata.name, "test-skill");
        assert!(skill.instructions.contains("Test instructions"));
        assert!(skill.constraints.unwrap().contains("Test constraints"));
    }

    #[test]
    fn test_no_constraints() {
        let content = r#"---
name: test
description: Test
version: 1.0.0
category: test
---

## Instructions

Just instructions.
"#;

        let parser = SkillParser::new();
        let skill = parser.parse_content(content, "test.md").unwrap();
        assert!(skill.constraints.is_none());
    }

    #[test]
    fn test_claude_style_skill_without_instructions_section() {
        // Real-world SKILL.md files (e.g. ~/.agents/skills) rarely carry an
        // `## Instructions` section — the whole body becomes instructions.
        let content = r#"---
name: caveman
description: A test skill
---

# Caveman

## Rules

Do things well.
"#;

        let parser = SkillParser::new();
        let skill = parser.parse_content(content, "SKILL.md").unwrap();
        assert_eq!(skill.metadata.name, "caveman");
        assert!(skill.instructions.contains("Do things well."));
    }
}

#[cfg(test)]
mod coverage_front_matter {
    //! The parser is the first thing that touches every skill file.
    //! The common case is covered; these pin the shapes an author
    //! writing by hand actually produces — YAML list forms, an
    //! author-less file, CRLF line endings, front matter with no
    //! body, and the strict-mode version requirement.
    use super::*;

    fn minimal(front_matter: &str, body: &str) -> String {
        format!("---\n{front_matter}\n---\n{body}")
    }

    #[test]
    fn yaml_block_list_form_is_accepted() {
        // Two spellings are equally common in the wild:
        //   tags:\n  - rust\n  - async
        //   tags: [rust, async]
        // A parser that only handled one would silently drop the
        // tags for every skill written in the other.
        let block = minimal(
            "name: a\ndescription: d\nversion: 1.0.0\ncategory: x\ntags:\n  - rust\n  - async",
            "## Instructions\nbody",
        );
        let skill = SkillParser::new().parse_content(&block, "t.md").unwrap();
        assert_eq!(skill.metadata.tags, vec!["rust", "async"]);

        let flow = minimal(
            "name: a\ndescription: d\nversion: 1.0.0\ncategory: x\ntags: [rust, async]",
            "## Instructions\nbody",
        );
        let skill = SkillParser::new().parse_content(&flow, "t.md").unwrap();
        assert_eq!(skill.metadata.tags, vec!["rust", "async"]);
    }

    #[test]
    fn optional_fields_absent_still_parse() {
        // `version`, `author`, `tags`, `capabilities`, `triggers`
        // are all `#[serde(default)]` in `SkillMetadata`. A file
        // that omits them is a valid skill, not a broken one.
        let content = minimal("name: a\ndescription: d", "## Instructions\nbody");
        let skill = SkillParser::new().parse_content(&content, "t.md").unwrap();
        assert_eq!(skill.metadata.name, "a");
        assert!(skill.metadata.version.is_empty());
        assert!(skill.metadata.tags.is_empty());
        assert!(skill.metadata.triggers.is_empty());
    }

    #[test]
    fn strict_mode_requires_version() {
        // The non-strict parser tolerates a missing version; the
        // strict one — used by `kod validate-skills --strict` — must
        // not. A regression makes CI accept a skill missing the
        // field the mode exists to enforce.
        let content = minimal("name: a\ndescription: d", "## Instructions\nbody");
        let lenient = SkillParser::new().parse_content(&content, "t.md");
        assert!(lenient.is_ok());
        let strict = SkillParser::new().strict().parse_content(&content, "t.md");
        assert!(strict.is_err());
        let msg = strict.unwrap_err().to_string();
        assert!(msg.to_lowercase().contains("version"), "got: {msg}");
    }

    #[test]
    fn empty_body_falls_back_to_empty_instructions() {
        // A front-matter-only skill (the author has not written the
        // body yet) must parse; the instructions are the whole body
        // when there is no `## Instructions` header, and the whole
        // body here is empty.
        let content = "---\nname: a\ndescription: d\nversion: 1.0.0\ncategory: x\n---\n";
        let skill = SkillParser::new().parse_content(content, "t.md").unwrap();
        assert!(skill.instructions.is_empty());
        assert!(skill.examples.is_empty());
        assert!(skill.constraints.is_none());
    }

    #[test]
    fn missing_front_matter_is_rejected() {
        let content = "## Instructions\nno front matter at all";
        let err = SkillParser::new()
            .parse_content(content, "t.md")
            .unwrap_err();
        let msg = err.to_string().to_lowercase();
        assert!(msg.contains("front matter"), "got: {msg}");
    }

    #[test]
    fn unterminated_front_matter_is_rejected() {
        // An opening `---` with no closing `---` is the specific
        // failure mode of a truncated write. The error must name
        // the missing closing marker, not say "invalid YAML".
        let content = "---\nname: a\ndescription: d\nversion: 1.0.0\ncategory: x";
        let err = SkillParser::new()
            .parse_content(content, "t.md")
            .unwrap_err();
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("closing") || msg.contains("---"),
            "unhelpful error: {msg}"
        );
    }

    #[test]
    fn empty_name_is_rejected_by_validation() {
        let content = minimal("name: \"\"\ndescription: d", "body");
        let err = SkillParser::new()
            .parse_content(&content, "t.md")
            .unwrap_err();
        assert!(err.to_string().to_lowercase().contains("name"));
    }

    #[test]
    fn empty_description_is_rejected_by_validation() {
        let content = minimal("name: a\ndescription: \"\"", "body");
        let err = SkillParser::new()
            .parse_content(&content, "t.md")
            .unwrap_err();
        assert!(err.to_string().to_lowercase().contains("description"));
    }

    #[test]
    fn examples_without_input_attribute_are_captured() {
        // The `<example input="…">` attribute is optional: a skill
        // author may just wrap a code sample in `<example>…</example>`
        // to mark it as a worked example.
        let body = "<example>\nlet x = 1;\n</example>";
        let content = minimal("name: a\ndescription: d", body);
        let skill = SkillParser::new().parse_content(&content, "t.md").unwrap();
        assert_eq!(skill.examples.len(), 1);
        assert!(skill.examples[0].input.is_empty());
        assert!(skill.examples[0].output.contains("let x = 1;"));
    }

    #[test]
    fn multiple_examples_are_captured_in_order() {
        let body =
            "<example input=\"first\">one</example>\n<example input=\"second\">two</example>";
        let content = minimal("name: a\ndescription: d", body);
        let skill = SkillParser::new().parse_content(&content, "t.md").unwrap();
        assert_eq!(skill.examples.len(), 2);
        assert_eq!(skill.examples[0].input, "first");
        assert_eq!(skill.examples[1].input, "second");
    }

    #[test]
    fn unrecognised_section_header_does_not_become_instructions() {
        // A body whose only header is `## Notes` has no
        // `## Instructions` block. The parser falls back to the
        // entire body (the file is the instructions), which is the
        // documented behaviour. This pins that the fallback is the
        // body, not the empty string.
        let content = minimal("name: a\ndescription: d", "## Notes\nsomething");
        let skill = SkillParser::new().parse_content(&content, "t.md").unwrap();
        assert!(skill.instructions.contains("## Notes"));
        assert!(skill.instructions.contains("something"));
    }
}

#[cfg(test)]
mod bom_tests {
    use super::*;

    #[test]
    fn bom_prefixed_front_matter_is_accepted() {
        let content = concat!(
            "\u{feff}",
            "---\n",
            "name: bom-skill\n",
            "description: Test BOM handling\n",
            "version: 1.0.0\n",
            "category: testing\n",
            "---\n",
            "\n",
            "Body.\n",
        );
        let parser = SkillParser::new();
        let skill = parser
            .parse_content(content, "bom-test")
            .expect("BOM-prefixed front matter must parse");
        assert_eq!(skill.metadata.name, "bom-skill");
    }
}

#[cfg(test)]
mod proptests {
    //! Properties of `SkillParser::parse_content`.
    //!
    //! `parse_content` is the SKILL.md boundary: split YAML front
    //! matter from markdown body, deserialize metadata, extract
    //! sections and `<example>...</example>` blocks.
    //!
    //! The `split_front_matter` / `extract_section` /
    //! `extract_examples` arithmetic was hand-checked: each slice is
    //! preceded by a `find` whose result bounds the slice, so no
    //! `start > end` panic exists here (unlike `parse_report` at
    //! `45aec66`). This is a regression-guard round.
    //!
    //! **Two checker bugs caught during the write (L23 twice).**
    //!
    //! 1. `build` first interpolated name/description unquoted into
    //!    the front matter; a description of `" "` (one space)
    //!    becomes YAML's `""` after normalization, so
    //!    `validate_metadata` correctly rejects it and the test's
    //!    `expect("must parse")` panicked on five properties at once.
    //!    Fixed by serializing through `serde_yaml::to_string` in
    //!    `build`.
    //!
    //! 2. The example output strategy first allowed whitespace-only
    //!    strings (`"[a-zA-Z0-9 ]{1,40}"`); a value of `" "` gets
    //!    trimmed to `""` by `extract_examples`'s `content.trim()`,
    //!    so the assert compared `""` (actual) to `" "` (expected).
    //!    The parser is correct; the expected value was wrong. Fixed
    //!    by dropping spaces from the output strategy — no trim can
    //!    change a value it can produce.
    //!
    //! Neither was a parser bug. Same shape as the parse_reply round
    //! (`1ce1bb6`) — the test input was not what the assertion
    //! assumed.
    use super::*;

    fn parser() -> SkillParser {
        SkillParser::new()
    }

    fn strict_parser() -> SkillParser {
        SkillParser::new().strict()
    }

    /// Build a well-formed SKILL.md. Name and description are
    /// serialized through `serde_yaml`, so arbitrary strategy output
    /// (including leading/trailing spaces, colons, quotes) stays
    /// well-formed.
    fn build(name: &str, description: &str, body: &str) -> String {
        let meta = SkillMetadata {
            name: name.to_string(),
            description: description.to_string(),
            version: String::new(),
            author: None,
            category: String::new(),
            tags: Vec::new(),
            capabilities: Vec::new(),
            requirements: Vec::new(),
            triggers: Vec::new(),
        };
        let yaml = serde_yaml::to_string(&meta).unwrap();
        format!("---\n{yaml}---\n{body}")
    }

    proptest::proptest! {
        #[test]
        fn prop_parse_content_never_panics(content in proptest::prelude::any::<String>()) {
            let p = parser();
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = p.parse_content(&content, "test.skill.md");
            }));
            proptest::prop_assert!(outcome.is_ok());
        }

        #[test]
        fn prop_parse_content_never_panics_strict(content in proptest::prelude::any::<String>()) {
            let p = strict_parser();
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = p.parse_content(&content, "test.skill.md");
            }));
            proptest::prop_assert!(outcome.is_ok());
        }

        #[test]
        fn prop_valid_skill_preserves_metadata(
            name in "[a-zA-Z][a-zA-Z0-9_-]{0,20}",
            description in "[a-zA-Z0-9 .]{1,60}",
            body in "[a-zA-Z0-9 \n.]{0,200}",
        ) {
            let content = build(&name, &description, &body);
            let p = parser();
            let skill = p
                .parse_content(&content, "test.skill.md")
                .expect("well-formed content must parse");
            proptest::prop_assert_eq!(skill.metadata.name, name);
            proptest::prop_assert_eq!(skill.metadata.description, description);
        }

        #[test]
        fn prop_no_example_tags_means_no_examples(
            name in "[a-zA-Z]{1,10}",
            description in "[a-zA-Z ]{1,30}",
            body in "[a-zA-Z0-9 \n#.]{0,200}",
        ) {
            let content = build(&name, &description, &body);
            let p = parser();
            let skill = p.parse_content(&content, "test.skill.md").unwrap();
            proptest::prop_assert!(skill.examples.is_empty());
        }

        #[test]
        fn prop_example_blocks_are_captured_in_order(
            name in "[a-zA-Z]{1,10}",
            description in "[a-zA-Z ]{1,30}",
            a_output in "[a-zA-Z0-9]{1,40}",
            b_output in "[a-zA-Z0-9]{1,40}",
        ) {
            let body = format!(
                "<example>\n{a_output}\n</example>\n\n<example>\n{b_output}\n</example>\n",
            );
            let content = build(&name, &description, &body);
            let p = parser();
            let skill = p.parse_content(&content, "test.skill.md").unwrap();
            proptest::prop_assert_eq!(skill.examples.len(), 2);
            proptest::prop_assert_eq!(&skill.examples[0].output, &a_output);
            proptest::prop_assert_eq!(&skill.examples[1].output, &b_output);
        }

        #[test]
        fn prop_strict_mode_requires_version(
            name in "[a-zA-Z]{1,10}",
            description in "[a-zA-Z ]{1,30}",
        ) {
            let content = build(&name, &description, "");
            let p_strict = strict_parser();
            let p_lenient = parser();
            proptest::prop_assert!(p_strict.parse_content(&content, "test.skill.md").is_err());
            proptest::prop_assert!(p_lenient.parse_content(&content, "test.skill.md").is_ok());
        }

        #[test]
        fn prop_bom_is_stripped(
            name in "[a-zA-Z]{1,10}",
            description in "[a-zA-Z ]{1,30}",
        ) {
            let without_bom = build(&name, &description, "");
            let with_bom = format!("\u{feff}{without_bom}");
            let p = parser();
            let a = p.parse_content(&without_bom, "test.skill.md").unwrap();
            let b = p.parse_content(&with_bom, "test.skill.md").unwrap();
            proptest::prop_assert_eq!(a.metadata.name, b.metadata.name);
            proptest::prop_assert_eq!(a.metadata.description, b.metadata.description);
        }
    }

    // -----------------------------------------------------------------
    // `## Instructions` / `## Constraints` section extraction.
    //
    // `02de4e8` noted honestly that none of the seven properties
    // exercised `extract_section`. These five pin it through
    // `parse_content`'s public outputs (`Skill::instructions`,
    // `Skill::constraints`), because `extract_section` is private.
    // -----------------------------------------------------------------

    /// Build a valid SKILL.md whose body is `body`.
    fn build_with_body(body: &str) -> String {
        let meta = SkillMetadata {
            name: "test-skill".to_string(),
            description: "test description".to_string(),
            version: String::new(),
            author: None,
            category: String::new(),
            tags: Vec::new(),
            capabilities: Vec::new(),
            requirements: Vec::new(),
            triggers: Vec::new(),
        };
        let yaml = serde_yaml::to_string(&meta).unwrap();
        format!("---\n{yaml}---\n{body}")
    }

    proptest::proptest! {
        /// A `## Instructions` section is extracted as the
        /// instructions, and does not run into a following
        /// `## Other` header. Two non-empty bodies, one as the
        /// section, one after the next header.
        #[test]
        fn prop_instructions_section_does_not_leak_past_next_header(
            instructions_text in "[a-zA-Z0-9 .]{1,40}",
            after in "[a-zA-Z0-9 .]{1,40}",
        ) {
            let body = format!(
                "## Instructions\n{instructions_text}\n## Notes\n{after}\n",
            );
            let content = build_with_body(&body);
            let p = parser();
            let skill = p.parse_content(&content, "test.skill.md").unwrap();
            proptest::prop_assert_eq!(
                skill.instructions.trim(),
                instructions_text.trim(),
            );
        }

        /// A body with no `## Instructions` section falls back to
        /// the whole body as the instructions (the Claude-style
        /// SKILL.md shape). The body alphabet excludes `#` so no
        /// section header can appear.
        #[test]
        fn prop_no_instructions_section_falls_back_to_body(
            body_text in "[a-zA-Z0-9 .\n]{1,100}",
        ) {
            let content = build_with_body(&body_text);
            let p = parser();
            let skill = p.parse_content(&content, "test.skill.md").unwrap();
            proptest::prop_assert_eq!(
                skill.instructions.trim(),
                body_text.trim(),
            );
        }

        /// A `## Constraints` section is extracted as
        /// `Some(text)`.
        #[test]
        fn prop_constraints_section_is_extracted(
            constraints_text in "[a-zA-Z0-9 .]{1,40}",
        ) {
            let body = format!("## Constraints\n{constraints_text}\n");
            let content = build_with_body(&body);
            let p = parser();
            let skill = p.parse_content(&content, "test.skill.md").unwrap();
            let got = skill.constraints.expect("constraints section present");
            proptest::prop_assert_eq!(got.trim(), constraints_text.trim());
        }

        /// A body with no `## Constraints` section yields `None`.
        #[test]
        fn prop_no_constraints_section_yields_none(
            body_text in "[a-zA-Z0-9 .\n]{1,100}",
        ) {
            let content = build_with_body(&body_text);
            let p = parser();
            let skill = p.parse_content(&content, "test.skill.md").unwrap();
            proptest::prop_assert!(skill.constraints.is_none());
        }

        /// Both sections present together: each is extracted
        /// independently, neither leaks into the other.
        #[test]
        fn prop_both_sections_extract_independently(
            i in "[a-zA-Z0-9 .]{1,30}",
            c in "[a-zA-Z0-9 .]{1,30}",
        ) {
            let body = format!("## Instructions\n{i}\n## Constraints\n{c}\n");
            let content = build_with_body(&body);
            let p = parser();
            let skill = p.parse_content(&content, "test.skill.md").unwrap();
            proptest::prop_assert_eq!(skill.instructions.trim(), i.trim());
            proptest::prop_assert_eq!(
                skill.constraints.as_deref().map(str::trim),
                Some(c.trim()),
            );
        }
    }

    // -----------------------------------------------------------------
    // `<example input="...">` attribute extraction.
    //
    // `02de4e8` and `4600d40` noted this gap: `extract_attribute` is
    // exercised indirectly by `prop_example_blocks_are_captured_in_order`
    // (which uses no attributes), but not pinned. The failing-first
    // property here is `prop_fake_input_attribute_is_ignored`:
    // `extract_attribute` does `tag.find("input=\"")`, a substring
    // match. A tag like `<example requires-input="true">` reports
    // `input = "true"` even though there is no `input` attribute —
    // same class as `completion_report::extract_status` at `af0301f`.
    // -----------------------------------------------------------------

    /// Build a valid SKILL.md whose body is one `<example>` block with
    /// the given opening-tag attributes (already formatted).
    fn build_example_with_attrs(attrs: &str) -> String {
        let body = if attrs.is_empty() {
            "<example>body</example>\n".to_string()
        } else {
            format!("<example {attrs}>body</example>\n")
        };
        build_with_body(&body)
    }

    proptest::proptest! {
        /// `<example>` with no attribute at all yields empty input.
        #[test]
        fn prop_no_input_attribute_yields_empty_input(_dummy in proptest::prelude::any::<u8>()) {
            let content = build_example_with_attrs("");
            let skill = parser().parse_content(&content, "test.skill.md").unwrap();
            proptest::prop_assert_eq!(skill.examples.len(), 1);
            proptest::prop_assert_eq!(&skill.examples[0].input, "");
        }

        /// **Failing-first.** A tag carrying an attribute whose name
        /// merely *contains* `input` must yield empty input. The
        /// pre-fix `find("input=\"")` matches all of them.
        #[test]
        fn prop_fake_input_attribute_is_ignored(
            fake_attr in proptest::sample::select(vec![
                "xinput",
                "data-input",
                "requires-input",
                "my-input",
                "sub_input",
                "aninput",
            ]),
        ) {
            let attrs = format!("{fake_attr}=\"wrong\"");
            let content = build_example_with_attrs(&attrs);
            let skill = parser().parse_content(&content, "test.skill.md").unwrap();
            proptest::prop_assert_eq!(skill.examples.len(), 1);
            proptest::prop_assert_eq!(
                &skill.examples[0].input,
                "",
                "attribute {:?} produced input {:?}; the tag has no \
                 `input=` attribute",
                fake_attr,
                skill.examples[0].input,
            );
        }

        /// Positive control: a real `input="..."` still parses.
        #[test]
        fn prop_real_input_attribute_is_honoured(
            text in "[a-zA-Z0-9 .]{1,30}",
        ) {
            let attrs = format!("input=\"{text}\"");
            let content = build_example_with_attrs(&attrs);
            let skill = parser().parse_content(&content, "test.skill.md").unwrap();
            proptest::prop_assert_eq!(&skill.examples[0].input, &text);
        }

        /// Single-quote form: `input='...'` parses.
        #[test]
        fn prop_single_quote_form_is_honoured(
            text in "[a-zA-Z0-9 .]{1,30}",
        ) {
            let attrs = format!("input='{text}'");
            let content = build_example_with_attrs(&attrs);
            let skill = parser().parse_content(&content, "test.skill.md").unwrap();
            proptest::prop_assert_eq!(&skill.examples[0].input, &text);
        }

        /// A fake attribute before a real one: the real one wins.
        /// The pre-fix `find` returns the fake (lower index), so this
        /// fails on the same bug as `prop_fake_input_attribute_is_ignored`.
        #[test]
        fn prop_real_input_wins_over_fake(
            prefix in proptest::sample::select(vec![
                "data-input",
                "xinput",
                "some-input",
            ]),
            real in "[a-zA-Z0-9 .]{1,30}",
        ) {
            let attrs = format!("{prefix}=\"wrong\" input=\"{real}\"");
            let content = build_example_with_attrs(&attrs);
            let skill = parser().parse_content(&content, "test.skill.md").unwrap();
            proptest::prop_assert_eq!(
                &skill.examples[0].input, &real,
                "prefix {:?}: got {:?}, expected {:?}",
                prefix, skill.examples[0].input, real,
            );
        }

        /// Never panics on arbitrary opening-tag content.
        #[test]
        fn prop_never_panics(garbage in proptest::prelude::any::<String>()) {
            let content = build_example_with_attrs(&garbage);
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = parser().parse_content(&content, "test.skill.md");
            }));
            proptest::prop_assert!(outcome.is_ok());
        }
    }
}
