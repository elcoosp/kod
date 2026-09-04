use kod_skills::parser::SkillParser;

const VALID_SKILL: &str = r#"---
name: rust-refactoring
description: Rust code refactoring with LSP integration
version: 1.2.0
author: kod-team
category: coding
tags:
  - rust
  - refactoring
  - lsp
capabilities:
  - code-refactoring
  - symbol-rename
requirements:
  - rust-analyzer
  - cargo
triggers:
  - "refactor rust"
  - "rename symbol"
---

# Rust Refactoring Skill

## Instructions

You are an expert Rust refactoring assistant. When activated:

1. **Analyze the code structure** using LSP tools
2. **Identify refactoring opportunities** based on Rust idioms
3. **Propose changes** with explanations
4. **Apply changes** using hash-anchored edits
5. **Verify changes** don't break references

## Examples

<example input="Refactor this function to use iterators">
Before:
```rust
fn sum_numbers(nums: &Vec<i32>) -> i32 {
    let mut sum = 0;
    for num in nums {
        sum += num;
    }
    sum
}
```

After:
```rust
fn sum_numbers(nums: &[i32]) -> i32 {
    nums.iter().sum()
}
```
</example>

## Constraints

- Never change public API without explicit request
- Preserve existing tests
"#;

#[test]
fn test_parse_valid_skill() {
    let parser = SkillParser::new();
    let skill = parser.parse_content(VALID_SKILL, "test.md").unwrap();

    assert_eq!(skill.metadata.name, "rust-refactoring");
    assert_eq!(skill.metadata.version, "1.2.0");
    assert_eq!(skill.metadata.author, Some("kod-team".to_string()));
    assert_eq!(skill.metadata.category, "coding");

    assert_eq!(skill.metadata.tags.len(), 3);
    assert!(skill.metadata.tags.contains(&"rust".to_string()));
    assert!(skill.metadata.tags.contains(&"refactoring".to_string()));

    assert_eq!(skill.metadata.capabilities.len(), 2);
    assert!(
        skill
            .metadata
            .capabilities
            .contains(&"code-refactoring".to_string())
    );

    assert_eq!(skill.metadata.triggers.len(), 2);
    assert!(
        skill
            .metadata
            .triggers
            .contains(&"refactor rust".to_string())
    );
}

#[test]
fn test_parse_instructions() {
    let parser = SkillParser::new();
    let skill = parser.parse_content(VALID_SKILL, "test.md").unwrap();

    assert!(
        skill
            .instructions
            .contains("expert Rust refactoring assistant")
    );
    assert!(skill.instructions.contains("LSP tools"));
    assert!(!skill.instructions.contains("Examples"));
}

#[test]
fn test_parse_constraints() {
    let parser = SkillParser::new();
    let skill = parser.parse_content(VALID_SKILL, "test.md").unwrap();

    let constraints = skill.constraints.expect("Should have constraints");
    assert!(constraints.contains("Never change public API"));
    assert!(constraints.contains("Preserve existing tests"));
}

#[test]
fn test_skill_id_generated() {
    let parser = SkillParser::new();
    let skill = parser.parse_content(VALID_SKILL, "test.md").unwrap();

    assert!(!skill.id.as_uuid().is_nil());
}

#[test]
fn test_parse_invalid_yaml() {
    let invalid = "---\ninvalid: yaml: [unclosed\n---\n# Body";
    let parser = SkillParser::new();
    let result = parser.parse_content(invalid, "test.md");

    assert!(result.is_err());
}

#[test]
fn test_parse_missing_front_matter() {
    let no_front_matter = "# Just markdown without front matter";
    let parser = SkillParser::new();
    let result = parser.parse_content(no_front_matter, "test.md");

    assert!(result.is_err());
}
