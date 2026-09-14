#!/usr/bin/env bash
set -uo pipefail

COMPILE_OK=true
TARGET=crates/kod-core/src/router.rs

if [ ! -f Cargo.toml ] || [ ! -f "$TARGET" ]; then
    echo "ERROR: run from the kod workspace root ($TARGET missing)"
    exit 1
fi

echo "Patching $TARGET: include skill examples and constraints in the prompt"

python3 - "$TARGET" << 'PYEOF'
import os
import sys

target = sys.argv[1]
with open(target, "r") as f:
    content = f.read()

def patch(old, new, label, expect=1):
    global content
    n = content.count(old)
    if n == 0:
        print(f"ERROR: old snippet not found: {label}")
        sys.exit(2)
    if expect and n != expect:
        print(f"ERROR: expected {expect} occurrence(s) of {label}, found {n}")
        sys.exit(2)
    content = content.replace(old, new, expect if expect else n)
    print(f"Patched: {label}")

# --- 1. build_prompt: render examples + constraints alongside instructions
patch(
    '''            let matches = matcher.find_relevant_skills(input).await;
            if !matches.is_empty() {
                prompt.push_str("## Relevant Skills\\n\\n");
                for skill_match in matches.iter().take(self.config.max_skills_per_query) {
                    // Tell the agent where the skill lives so it can read
                    // reference files with the correct absolute path instead of
                    // guessing relative to the project root.
                    let base_dir = skill_match
                        .skill
                        .path
                        .parent()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| ".".to_string());
                    prompt.push_str(&format!(
                        "### {}\\nSkill location: {}\\n\\n{}\\n\\n",
                        skill_match.skill.metadata.name, base_dir, skill_match.skill.instructions
                    ));
                }
            }''',
    '''            let matches = matcher.find_relevant_skills(input).await;
            if !matches.is_empty() {
                prompt.push_str("## Relevant Skills\\n\\n");
                for skill_match in matches.iter().take(self.config.max_skills_per_query) {
                    let skill = &skill_match.skill;
                    // Tell the agent where the skill lives so it can read
                    // reference files with the correct absolute path instead of
                    // guessing relative to the project root.
                    let base_dir = skill
                        .path
                        .parent()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| ".".to_string());
                    prompt.push_str(&format!(
                        "### {}\\nSkill location: {}\\n\\n{}\\n\\n",
                        skill.metadata.name, base_dir, skill.instructions
                    ));

                    // The parser splits a skill file into instructions,
                    // examples, and constraints. Only the instructions
                    // section was being injected, so a skill whose value
                    // lives in its worked examples (code-review's
                    // <example> blocks, python-testing's parametrize
                    // sample) arrived at the model as bare prose — the
                    // model had no way to reproduce the skill author's
                    // intent. Append the examples and constraints too.
                    //
                    // These are secondary; cap them so a verbose skill
                    // cannot dominate the prompt. The instructions are
                    // the primary content and stay uncapped.
                    const MAX_SKILL_EXAMPLES: usize = 5;
                    const MAX_EXAMPLE_CHARS: usize = 1_500;
                    if !skill.examples.is_empty() {
                        prompt.push_str("Examples:\\n\\n");
                        for ex in skill.examples.iter().take(MAX_SKILL_EXAMPLES) {
                            if !ex.input.trim().is_empty() {
                                prompt.push_str(&format!("Input: {}\\n", ex.input.trim()));
                            }
                            let out = ex.output.trim();
                            let shown = if out.len() > MAX_EXAMPLE_CHARS {
                                format!("{}…", truncate_chars(out, MAX_EXAMPLE_CHARS))
                            } else {
                                out.to_string()
                            };
                            prompt.push_str(&format!("Output:\\n{}\\n\\n", shown));
                        }
                        let extra = skill.examples.len().saturating_sub(MAX_SKILL_EXAMPLES);
                        if extra > 0 {
                            prompt.push_str(&format!(
                                "…and {} more example(s) in the skill file.\\n\\n",
                                extra
                            ));
                        }
                    }
                    if let Some(constraints) = &skill.constraints
                        && !constraints.trim().is_empty()
                    {
                        prompt.push_str(&format!(
                            "Constraints:\\n{}\\n\\n",
                            constraints.trim()
                        ));
                    }
                }
            }''',
    "build_prompt renders examples + constraints",
)

# --- 2. Local truncate_chars helper (engine's is pub(crate) in kod-core)
patch(
    '''/// Types of tasks that can be routed''',
    '''/// Truncate a UTF-8 string to at most `max` bytes at a char boundary.
/// Local to this module so the router does not need a public dependency
/// on `kod_core::engine`'s helper.
fn truncate_chars(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Types of tasks that can be routed''',
    "router truncate_chars helper",
)

# --- 3. Extend the existing test to cover examples + constraints
patch(
    '''        let matcher = router.skill_matcher.as_ref().expect("matcher present");
        matcher
            .add_skill(Skill {
                id: SkillId::new(),
                metadata: SkillMetadata {
                    name: "ui-ux-designer".to_string(),
                    description: "Design help".to_string(),
                    version: "1.0.0".to_string(),
                    author: None,
                    category: "test".to_string(),
                    tags: Vec::new(),
                    capabilities: Vec::new(),
                    requirements: Vec::new(),
                    triggers: Vec::new(),
                },
                instructions: String::new(),
                examples: Vec::new(),
                constraints: None,
                content: String::new(),
                path: temp_dir.path().to_path_buf(),
            })
            .await;''',
    '''        let matcher = router.skill_matcher.as_ref().expect("matcher present");
        matcher
            .add_skill(Skill {
                id: SkillId::new(),
                metadata: SkillMetadata {
                    name: "ui-ux-designer".to_string(),
                    description: "Design help".to_string(),
                    version: "1.0.0".to_string(),
                    author: None,
                    category: "test".to_string(),
                    tags: Vec::new(),
                    capabilities: Vec::new(),
                    requirements: Vec::new(),
                    triggers: Vec::new(),
                },
                instructions: "Do design well.".to_string(),
                examples: vec![kod_types::SkillExample {
                    input: "make a login form".to_string(),
                    output: "Use a single column with a labeled email field.".to_string(),
                }],
                constraints: Some("Never use more than two fonts.".to_string()),
                content: String::new(),
                path: temp_dir.path().to_path_buf(),
            })
            .await;''',
    "seed test skill with examples + constraints",
)

patch(
    '''        assert!(
            with_skill.contains("Skill location:"),
            "skill location not injected: {with_skill}"
        );
        assert!(
            with_skill.contains("what can you do?"),
            "history not carried: {with_skill}"
        );
    }''',
    '''        assert!(
            with_skill.contains("Skill location:"),
            "skill location not injected: {with_skill}"
        );
        assert!(
            with_skill.contains("what can you do?"),
            "history not carried: {with_skill}"
        );
        // The examples and constraints the parser extracted must reach
        // the prompt. Regression: build_prompt only injected the
        // instructions section, so skills whose value is in their
        // worked examples arrived at the model as bare prose.
        assert!(
            with_skill.contains("make a login form"),
            "example input missing from prompt: {with_skill}"
        );
        assert!(
            with_skill.contains("labeled email field"),
            "example output missing from prompt: {with_skill}"
        );
        assert!(
            with_skill.contains("Never use more than two fonts"),
            "constraints missing from prompt: {with_skill}"
        );
    }''',
    "assert examples + constraints present",
)

tmp = target + ".tmp"
with open(tmp, "w") as f:
    f.write(content)
os.replace(tmp, target)
print("Patched", target)
PYEOF

if [ $? -ne 0 ]; then
    echo "ERROR: patching failed"
    exit 1
fi

echo "cargo check --workspace"
if ! cargo check --workspace 2>&1; then
    echo "Compilation failed"
    exit 1
fi

echo "cargo clippy --workspace --all-targets -- -D warnings"
if ! cargo clippy --workspace --all-targets -- -D warnings 2>&1; then
    echo "Clippy failed"
    exit 1
fi

echo "Committing."
git add -A
git commit -m "fix(core): inject skill examples and constraints into the prompt

SkillParser splits a skill file into three fields: \`instructions\`
(the \`## Instructions\` section), \`examples\` (parsed from
\`<example>\` blocks anywhere in the body), and \`constraints\` (the
\`## Constraints\` section). TaskRouter::build_prompt injected only
\`skill.instructions\`.

The visible effect: a skill like \`code-review.md\` arrived at the
model as a bare paragraph of prose. Its worked \`<example>\` blocks —
the whole reason the skill exists — were parsed and thrown away.
The same was true for \`python-testing.md\`'s parametrize sample and
\`rust-refactoring.md\`'s before/after pairs. The model saw the
guidance but never the demonstrations of it, so it could not
reproduce the skill author's intended shape for an answer.

Extend the per-skill block to render instructions, then examples,
then constraints. Examples cap at 5 blocks with a per-example
1_500-char trim so a verbose skill cannot dominate the prompt; the
instructions remain uncapped as the primary content. When examples
are dropped past the cap, a short \"…and N more example(s)\" line
tells the model what is in the file — and the skill location line
already lets it read the rest.

Add a local char-boundary-safe \`truncate_chars\` helper so the
router does not need a public dependency on the engine's helper.

Extends test_build_prompt_carries_identity_and_skill_inventory:
the seeded skill now carries an example and a constraint, and the
test asserts both reach the prompt."
