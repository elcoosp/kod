//! `kod skills` subcommands: list / validate / new / show / edit / remove.

use super::*;

pub async fn run_skills_list(json: bool) -> Result<()> {
    let config = KodConfig::load_default()?;
    let skills_dirs = config.skills_dirs()?;

    if json {
        let skills = kod_skills::load_from_dirs(&skills_dirs).await?;
        let arr: Vec<serde_json::Value> = skills
            .iter()
            .map(|s| {
                serde_json::json!({
                    "name": s.metadata.name,
                    "description": s.metadata.description,
                    "version": s.metadata.version,
                    "category": s.metadata.category,
                    "tags": s.metadata.tags,
                    "capabilities": s.metadata.capabilities,
                    "triggers": s.metadata.triggers,
                    "path": s.path.display().to_string(),
                })
            })
            .collect();
        let out = serde_json::to_string_pretty(&arr)
            .map_err(|e| KodError::Serialization(e.to_string()))?;
        println!("{}", out);
        return Ok(());
    }

    let existing: Vec<_> = skills_dirs.iter().filter(|d| d.is_dir()).collect();
    if existing.is_empty() {
        println!("No skills directories found. Checked:");
        for d in &skills_dirs {
            println!("  {}", d.display());
        }
        println!("No skills available.");
        return Ok(());
    }

    let skills = kod_skills::load_from_dirs(&skills_dirs).await?;
    if skills.is_empty() {
        println!("Skills directories exist but contain no parseable .md skills:");
        for d in &existing {
            println!("  {}", d.display());
        }
        return Ok(());
    }

    println!("Available skills ({}):", skills.len());
    for skill in &skills {
        println!(
            "  - {}: {}",
            skill.metadata.name, skill.metadata.description
        );
    }

    Ok(())
}

pub async fn run_skills_validate() -> Result<()> {
    let config = KodConfig::load_default()?;
    let skills_dirs = config.skills_dirs()?;
    let parser = kod_skills::SkillParser::new();

    let mut total = 0usize;
    let mut ok = 0usize;
    let mut failed: Vec<(std::path::PathBuf, String)> = Vec::new();

    for dir in &skills_dirs {
        if !dir.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(dir)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            if entry.path().extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            total += 1;
            match parser.parse_file(entry.path()) {
                Ok(skill) => {
                    ok += 1;
                    println!("✓ {} ({})", skill.metadata.name, entry.path().display());
                }
                Err(e) => {
                    let msg = e.to_string();
                    failed.push((entry.path().to_path_buf(), msg.clone()));
                    println!("✗ {} — {}", entry.path().display(), msg);
                }
            }
        }
    }

    if total == 0 {
        println!("No skill files found. Checked:");
        for d in &skills_dirs {
            println!("  {}", d.display());
        }
        return Ok(());
    }

    println!();
    println!(
        "{} skill file(s) checked: {} parsed, {} failed.",
        total,
        ok,
        failed.len()
    );

    if !failed.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}

pub async fn run_skills_new(name: &str) -> Result<()> {
    // Validate the name: kebab-case, [a-z0-9-].
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(KodError::Config("skill name is required".to_string()));
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(KodError::Config(format!(
            "invalid skill name {:?}: use lowercase letters, digits, and hyphens only",
            trimmed
        )));
    }

    let config = KodConfig::load_default()?;
    let dirs = config.skills_dirs()?;

    // Prefer a project-local path if the cwd is inside one; else the
    // first home-level path.
    let cwd = std::env::current_dir().ok();
    let target_dir = dirs
        .iter()
        .find(|d| {
            cwd.as_ref()
                .map(|c| d.starts_with(c) || d.parent().map(|p| p.starts_with(c)).unwrap_or(false))
                .unwrap_or(false)
        })
        .or_else(|| dirs.first())
        .cloned()
        .ok_or_else(|| {
            KodError::Config("could not determine a skills directory to write to".to_string())
        })?;

    std::fs::create_dir_all(&target_dir).map_err(KodError::Io)?;
    let path = target_dir.join(format!("{trimmed}.md"));
    if path.exists() {
        return Err(KodError::Config(format!(
            "{} already exists — refusing to overwrite",
            path.display()
        )));
    }

    let title = to_title_case(trimmed);
    // H-C4: build the frontmatter line by line. The pre-fix
    // `format!` carried 9-space continuation indents straight from
    // the source, so a fresh skill's `name:` and `description:` were
    // indented 9 spaces and `kod validate-skills` rejected the file
    // immediately. Explicit `push_str` calls make the output
    // indentation-invariant.
    let mut body = String::new();
    body.push_str("---\n");
    body.push_str(&format!("name: {trimmed}\n"));
    body.push_str("description: \"TODO: one-sentence description of what this skill does\"\n");
    body.push_str("version: 0.1.0\n");
    body.push_str("category: general\n");
    body.push_str("tags: []\n");
    body.push_str("capabilities: []\n");
    body.push_str("triggers:\n");
    body.push_str("  - \"TODO trigger phrase\"\n");
    body.push_str("---\n\n");
    body.push_str(&format!("# {title}\n\n"));
    body.push_str("## Instructions\n\n");
    body.push_str("Describe the skill's guidance here. The model reads this section\n");
    body.push_str("when the skill's triggers match the user's request.\n\n");
    body.push_str("## Examples\n\n");
    body.push_str("<example input=\"A sample user request\">\n");
    body.push_str("A sample response that demonstrates the skill.\n");
    body.push_str("</example>\n\n");
    body.push_str("## Constraints\n\n");
    body.push_str("Optional. Rules the model must respect when applying the skill.\n");

    std::fs::write(&path, body.as_bytes()).map_err(KodError::Io)?;

    println!("Created {}", path.display());
    println!();
    println!("Edit it to fill in the description, triggers, and instructions.");
    println!("Validate with: kod validate-skills");
    Ok(())
}

pub async fn run_skills_remove(name: &str, yes: bool) -> Result<()> {
    let path = match find_skill_path(name).await? {
        Some(p) => p,
        None => {
            eprintln!(
                "No skill named {:?} in any configured skills directory.",
                name
            );
            std::process::exit(1);
        }
    };

    // Safety check: the resolved path must live inside one of the
    // configured skills directories.
    let config = KodConfig::load_default()?;
    let dirs = config.skills_dirs()?;
    let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
    let safe = dirs.iter().any(|d| {
        let cd = std::fs::canonicalize(d).unwrap_or_else(|_| d.clone());
        canonical.starts_with(&cd)
    });
    if !safe {
        eprintln!(
            "Refusing to delete {}: it is not inside a configured skills directory.",
            path.display()
        );
        std::process::exit(1);
    }

    if !yes {
        eprint!("Delete {}? [y/N] ", path.display());
        use std::io::Write;
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            eprintln!("(input error — aborting)");
            std::process::exit(1);
        }
        if !matches!(line.trim().to_lowercase().as_str(), "y" | "yes") {
            println!("Aborted.");
            return Ok(());
        }
    }

    std::fs::remove_file(&path).map_err(KodError::Io)?;
    println!("Deleted {}", path.display());
    Ok(())
}

pub async fn run_skills_edit(name: &str) -> Result<()> {
    let path = match find_skill_path(name).await? {
        Some(p) => p,
        None => {
            eprintln!(
                "No skill named {:?} in any configured skills directory.",
                name
            );
            std::process::exit(1);
        }
    };

    let editor = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .unwrap_or_else(|_| "vi".to_string());
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "{} {}",
            editor,
            shell_quote(&path.to_string_lossy())
        ))
        .status();
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(KodError::Internal(format!(
            "editor {:?} exited {:?}",
            editor,
            s.code()
        ))),
        Err(e) => Err(KodError::Internal(format!(
            "could not launch {:?}: {}",
            editor, e
        ))),
    }
}

pub async fn run_skills_show(name: &str) -> Result<()> {
    let path = match find_skill_path(name).await? {
        Some(p) => p,
        None => {
            eprintln!(
                "No skill named {:?} in any configured skills directory.",
                name
            );
            std::process::exit(1);
        }
    };
    let content = std::fs::read_to_string(&path).map_err(KodError::Io)?;
    print!("{}", content);
    if !content.ends_with('\n') {
        println!();
    }
    Ok(())
}

pub(super) async fn find_skill_path(name: &str) -> Result<Option<std::path::PathBuf>> {
    let config = KodConfig::load_default()?;
    let dirs = config.skills_dirs()?;
    for dir in &dirs {
        if !dir.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(dir)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            if entry.path().extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            let stem = entry
                .path()
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            if stem == name {
                return Ok(Some(entry.path().to_path_buf()));
            }
        }
    }
    // Fall back to a content scan for a matching `name:` field.
    let parser = kod_skills::SkillParser::new();
    for dir in &dirs {
        if !dir.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(dir)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            if entry.path().extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            if let Ok(skill) = parser.parse_file(entry.path())
                && skill.metadata.name == name
            {
                return Ok(Some(entry.path().to_path_buf()));
            }
        }
    }
    Ok(None)
}

pub(super) fn to_title_case(s: &str) -> String {
    s.split('-')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
