#!/usr/bin/env bash
set -uo pipefail

CONFIG=crates/kod-config/src/config.rs
CLI=crates/kod-cli/src/commands.rs

for f in "$CONFIG" "$CLI"; do
    if [ ! -f "$f" ]; then
        echo "ERROR: missing $f"
        exit 1
    fi
done

echo "=== Pre-state: current DB path display ==="
grep -n "long_term_db_path\|Skills Directory" "$CLI"

echo
python3 - "$CONFIG" "$CLI" << 'PYEOF'
import os
import sys

config, cli = sys.argv[1], sys.argv[2]

def patch(path, old, new, label, expect=1):
    with open(path) as f:
        src = f.read()
    n = src.count(old)
    if n == 0:
        print(f"  MISS in {path}: {label}")
        return False
    if expect and n != expect:
        print(f"  ERROR: expected {expect} occurrence(s) of {label} in {path}, found {n}")
        sys.exit(2)
    tmp = path + ".tmp"
    with open(tmp, "w") as f:
        f.write(src.replace(old, new, expect if expect else n))
    os.replace(tmp, path)
    print(f"  patched {path}: {label}")
    return True

# ======================================================================
# 1. KodConfig::memory_db_path() — the effective long-term DB path.
# ======================================================================
patch(
    config,
    '''    /// Get the primary skills directory.''',
    '''    /// The effective long-term memory database path.
    ///
    /// Returns the explicit `memory.long_term_db_path` when set;
    /// otherwise the default the engine and CLI construct —
    /// `~/.kod/data/kod.redb`. A caller (the `kod config` display,
    /// the engine, a future backup command) needs the path that will
    /// actually be opened, not the raw `Option` in the config file.
    pub fn memory_db_path(&self) -> Result<PathBuf> {
        if let Some(explicit) = &self.memory.long_term_db_path {
            return Ok(PathBuf::from(explicit));
        }
        dirs::home_dir()
            .map(|h| h.join(".kod").join("data").join("kod.redb"))
            .ok_or_else(|| {
                KodError::Config("Could not determine home directory".to_string())
            })
    }

    /// Get the primary skills directory.''',
    "KodConfig::memory_db_path",
)

# ======================================================================
# 2. run_config_display: show effective values.
# ======================================================================
old_display = '''    println!("Memory:");
    println!(
        "  Short-Term Capacity: {}",
        config.memory.short_term_capacity
    );
    println!("  Long-Term DB Path: {:?}", config.memory.long_term_db_path);
    println!();
    println!("Skills:");
    println!(
        "  Skills Directory: {}",
        config
            .skills
            .skills_dir
            .clone()
            .unwrap_or_else(|| "default".to_string())
    );
    println!(
        "  Max Skills Per Query: {}",
        config.skills.max_skills_per_query
    );

    Ok(())'''

new_display = '''    println!("Memory:");
    println!(
        "  Short-Term Capacity: {}",
        config.memory.short_term_capacity
    );
    // Effective path, not the raw Option. `Long-Term DB Path: None`
    // was technically the config value but told the user nothing —
    // the engine opens `~/.kod/data/kod.redb` in that case, and a
    // user inspecting their setup needs to see where the file
    // actually lives.
    match config.memory_db_path() {
        Ok(p) => println!("  Long-Term DB Path: {}", p.display()),
        Err(_) => println!("  Long-Term DB Path: (could not determine)"),
    }
    println!();
    println!("Skills:");
    // Same reasoning: `Skills Directory: default` said nothing. Show
    // the directories discovery actually scans, marking which exist
    // and which do not — the same list `kod skills` loads from.
    match config.skills_dirs() {
        Ok(dirs) => {
            if dirs.is_empty() {
                println!("  Skills Directories: (none)");
            } else {
                println!("  Skills Directories:");
                for d in &dirs {
                    let marker = if d.is_dir() { "✓" } else { "·" };
                    println!("    {} {}", marker, d.display());
                }
                println!("    (✓ = exists and is scanned, · = not present)");
            }
        }
        Err(_) => println!("  Skills Directories: (could not determine)"),
    }
    println!(
        "  Max Skills Per Query: {}",
        config.skills.max_skills_per_query
    );

    Ok(())'''

patch(cli, old_display, new_display, "run_config_display: effective values")

print("Done.")
PYEOF

if [ $? -ne 0 ]; then
    echo "ERROR: patching failed"
    exit 1
fi

echo
echo "cargo check --workspace --all-targets 2>&1 | tail -8"
if ! cargo check --workspace --all-targets 2>&1 | tail -8; then
    echo "Compilation failed"
    exit 1
fi

echo
echo "cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -8"
if ! cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -8; then
    echo "Clippy failed"
    exit 1
fi

echo
echo "=== Behaviour check ==="
cargo build -p kod-cli 2>&1 | tail -2
./target/debug/kod config 2>&1 | sed -n '/Memory:/,$p'

cat > /tmp/kod_commit_msg.txt <<'MSG'
feat(cli): kod config shows effective paths, not raw Options

`kod config` printed the raw values from the config file:

  Memory:
    Long-Term DB Path: None
  Skills:
    Skills Directory: default

Neither told the user anything. The engine opens
`~/.kod/data/kod.redb` when long_term_db_path is None, and discovery
scans the standard skills directories (as `kod skills` itself
demonstrates) when skills_dir is None. A user inspecting `kod
config` saw "None" and "default" and had no way to find out where
either thing actually lives.

Show effective values:

  Memory:
    Long-Term DB Path: /Users/…/.kod/data/kod.redb
  Skills:
    Skills Directories:
      ✓ /Users/…/.agents/skills
      · /Users/…/.kod/skills
      · <project>/.kod/skills
      · <project>/.agents/skills
      (✓ = exists and is scanned, · = not present)

The ✓/· markers make it obvious at a glance which directories are
contributing skills — the same list `kod skills` loads from, so the
two commands cannot disagree about what discovery sees.

Adds KodConfig::memory_db_path() -> Result<PathBuf>, the effective
path resolved the same way the engine resolves it (explicit config
value, else ~/.kod/data/kod.redb). Used by the display now; a
future backup or inspect subcommand will want the same value.
MSG

git add -A
git commit -F /tmp/kod_commit_msg.txt
rm -f /tmp/kod_commit_msg.txt
