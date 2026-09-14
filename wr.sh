#!/usr/bin/env bash
set -uo pipefail

CTX=crates/kod-tools/src/context.rs

echo "=== Current dangerous-command check ==="
awk '/Refuse a small set of unambiguously destructive/,/^        Ok\(\)$/' "$CTX" | head -40

echo
echo "Patching $CTX"

python3 - "$CTX" << 'PYEOF'
import os
import sys

target = sys.argv[1]
with open(target, "r") as f:
    src = f.read()

old = '''        // Refuse a small set of unambiguously destructive commands.
        // These run through `sh -c` / `cmd /C`, so both shells' worst
        // offenders are listed. The check is a guardrail, not a sandbox:
        // `true; rm -rf /` slips past `starts_with`, and that is
        // acceptable — the real defense is that the whole tool is
        // behind ToolPermissions::execute_commands and the default is
        // off. This just stops the accidental "delete everything"
        // command from a model that read the wrong directory.
        let dangerous_patterns: &[&str] = &[
            // POSIX
            "rm -rf",
            "sudo",
            "chmod 777",
            "mkfs",
            "> /dev/sda",
            "> /dev/disk",
            // cmd.exe
            "format ",
            "del /f /q /s",
            "rd /s /q",
            "rmdir /s /q",
        ];
        for pattern in dangerous_patterns {
            if command.starts_with(pattern) {
                return Err(KodError::PermissionDenied {
                    action: "execute".to_string(),
                    reason: format!("Dangerous command pattern detected: {}", pattern),
                });
            }
        }'''

new = '''        // Refuse a small set of unambiguously destructive commands.
        //
        // These run through `sh -c` / `cmd /C`, so both shells' worst
        // offenders are listed. The check is a guardrail, not a sandbox:
        // `true; rm -rf /` slips past the prefix test, and that is
        // acceptable — the real defense is that the whole tool is
        // behind `ToolPermissions::execute_commands`, which is off by
        // default. This stops the accidental "delete everything"
        // command from a model that read the wrong directory.
        //
        // Trim leading whitespace before matching. The previous
        // `command.starts_with(pattern)` check was defeated by a
        // single leading space — `"  rm -rf /"` passed. A leading tab
        // or newline did too. Trimming does not turn this into a
        // sandbox; it removes a footgun that would have let an
        // accidental destructive command through the one layer of
        // defense that exists.
        //
        // Note about `sudo`: it can precede any of the other patterns
        // (`sudo rm -rf /`). Listing it as its own prefix is
        // deliberate — `sudo` on its own is the shape that matters
        // most; the pattern check does not scan for `sudo` mid-string,
        // matching the guardrail-not-sandbox contract above.
        let trimmed = command.trim_start();
        let dangerous_patterns: &[&str] = &[
            // POSIX
            "rm -rf",
            "rm -fr",
            "rm -r -f",
            "sudo",
            "chmod 777",
            "mkfs",
            "> /dev/sda",
            "> /dev/disk",
            // cmd.exe
            "format ",
            "del /f /q /s",
            "rd /s /q",
            "rmdir /s /q",
        ];
        for pattern in dangerous_patterns {
            if trimmed.starts_with(pattern) {
                return Err(KodError::PermissionDenied {
                    action: "execute".to_string(),
                    reason: format!(
                        "Dangerous command pattern detected: {}",
                        pattern
                    ),
                });
            }
        }'''

n = src.count(old)
if n != 1:
    print(f"ERROR: expected 1 occurrence of the check, found {n}")
    sys.exit(2)
src = src.replace(old, new, 1)

# Add tests if not present.
if "test_can_execute_command_rejects_leading_whitespace" not in src:
    anchor = '''    #[test]
    fn test_context_creation() {'''
    if anchor not in src:
        print("ERROR: test anchor not found in context.rs")
        sys.exit(2)
    new_tests = '''    /// The dangerous-command guardrail must fire on a leading-space
    /// command. Regression: the previous check used the raw string
    /// with `starts_with`, so `"  rm -rf /"` — a single space — passed
    /// the one layer of defense the tool has.
    #[test]
    fn test_can_execute_command_rejects_leading_whitespace() {
        let perms = ToolPermissions {
            execute_commands: true,
            ..Default::default()
        };
        let ctx = ToolContext::new("/tmp").with_permissions(perms);

        // Baseline: no leading whitespace -> rejected.
        assert!(ctx.can_execute_command("rm -rf /").is_err());
        // Leading space -> must also be rejected.
        assert!(
            ctx.can_execute_command("  rm -rf /").is_err(),
            "leading space must not defeat the guardrail"
        );
        // Leading tab.
        assert!(
            ctx.can_execute_command("\\trm -rf /").is_err(),
            "leading tab must not defeat the guardrail"
        );
        // Leading newline.
        assert!(
            ctx.can_execute_command("\\nrm -rf /").is_err(),
            "leading newline must not defeat the guardrail"
        );

        // rm -fr and rm -r -f are the same operation.
        assert!(ctx.can_execute_command("rm -fr /").is_err());
        assert!(ctx.can_execute_command("rm -r -f /").is_err());

        // A safe command still passes.
        assert!(ctx.can_execute_command("ls -la").is_ok());
        assert!(ctx.can_execute_command("  cargo test").is_ok());
    }

    #[test]
    fn test_context_creation() {'''
    src = src.replace(anchor, new_tests, 1)
    print("  added leading-whitespace tests")
else:
    print("  tests already present")

tmp = target + ".tmp"
with open(tmp, "w") as f:
    f.write(src)
os.replace(tmp, target)
print("Wrote", target)
PYEOF

if [ $? -ne 0 ]; then
    echo "ERROR: patching failed"
    exit 1
fi

echo
echo "cargo check --workspace --all-targets 2>&1 | tail -15"
if ! cargo check --workspace --all-targets 2>&1 | tail -15; then
    echo "Compilation failed"
    exit 1
fi

cat > /tmp/kod_commit_msg.txt <<'MSG'
fix(tools): reject destructive commands with leading whitespace

can_execute_command tested the dangerous-command list against the
raw command string with `starts_with`. A single leading space
defeated it: "  rm -rf /" passed. Same for a leading tab or
newline. Trim_start before matching.

Add rm -fr and rm -r -f to the list — the same operation as
rm -rf, in a form a shell accepts and a model occasionally emits.

The guardrail-not-sandbox contract is unchanged and the comment
now states it more explicitly: "true; rm -rf /" still slips past
the prefix test, and the real defense remains
ToolPermissions::execute_commands being off by default. This fix
removes a footgun that let an accidental destructive command
through the one layer of defense the tool has.

Adds test_can_execute_command_rejects_leading_whitespace, which
covers the three whitespace forms and the rm -fr variants, and
confirms a safe command with leading whitespace still passes.
MSG

git add -A
git commit -F /tmp/kod_commit_msg.txt
rm -f /tmp/kod_commit_msg.txt
