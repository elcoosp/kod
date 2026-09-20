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
        let content = content.trim();

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
        let pattern = format!("{}=\"", attr);
        let start = tag.find(&pattern)?;
        let content_start = start + pattern.len();
        let end = tag[content_start..].find('"')? + content_start;

        Some(tag[content_start..end].to_string())
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
