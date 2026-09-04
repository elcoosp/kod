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
        self.parse_content(&content, &source_name)
            .map(|mut skill| {
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
            .ok_or_else(|| KodError::SkillParseError {
                path: source_name.to_string(),
                reason: "Missing '## Instructions' section".to_string(),
            })?;

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
        let end_pos = rest.find("\n---").ok_or_else(|| KodError::SkillParseError {
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
}
