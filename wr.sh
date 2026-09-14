#!/usr/bin/env bash
set -uo pipefail

run_with_timeout() {
    local secs="$1"; shift
    if command -v timeout >/dev/null 2>&1; then
        timeout "$secs" "$@"; return $?
    fi
    if command -v gtimeout >/dev/null 2>&1; then
        gtimeout "$secs" "$@"; return $?
    fi
    "$@" &
    local pid=$!
    ( sleep "$secs"
      if kill -0 "$pid" 2>/dev/null; then
          kill -TERM "$pid" 2>/dev/null
          sleep 2
          kill -KILL "$pid" 2>/dev/null
      fi ) &
    local watchdog=$!
    wait "$pid"; local rc=$?
    kill "$watchdog" 2>/dev/null; wait "$watchdog" 2>/dev/null
    [ "$rc" -ge 128 ] && return 124
    return "$rc"
}

COMPILE_OK=true
INCOMPLETE=false
TOOLS=crates/kod-tools/src/tools.rs
CTX=crates/kod-tools/src/context.rs

for f in "$TOOLS" "$CTX"; do
    if [ ! -f "$f" ]; then
        echo "ERROR: missing $f — run from the kod workspace root"
        exit 1
    fi
done

echo "Patching $TOOLS (shell selection) and $CTX (dangerous-command list)"

python3 - "$TOOLS" "$CTX" << 'PYEOF'
import os
import sys

tools, ctx = sys.argv[1], sys.argv[2]

def patch(path, old, new, label, expect=1):
    with open(path, "r") as f:
        content = f.read()
    n = content.count(old)
    if n == 0:
        print(f"ERROR: old snippet not found in {path}: {label}")
        sys.exit(2)
    if expect and n != expect:
        print(f"ERROR: expected {expect} occurrence(s) of {label} in {path}, found {n}")
        sys.exit(2)
    patched = content.replace(old, new, expect if expect else n)
    tmp = path + ".tmp"
    with open(tmp, "w") as f:
        f.write(patched)
    os.replace(tmp, path)
    print(f"Patched {path}: {label}")

# --- 1. tools.rs: description mentions which shell runs ----------------
patch(
    tools,
    '''                description: "Execute a shell command".to_string(),''',
    '''                description: "Execute a shell command via `sh -c` on Unix and `cmd /C` on Windows. The command runs in the working directory and inherits no shell aliases or profile; write POSIX syntax on Unix and cmd.exe syntax on Windows.".to_string(),''',
    "execute_command description",
)

# --- 2. tools.rs: pick the shell via cfg!(windows) ---------------------
patch(
    tools,
    '''        context.can_execute_command(command)?;

        // Spawn with piped stdio so each stream is capped independently
        // and the child is killed the moment output runs away.
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .stdin(std::process::Stdio::null())''',
    '''        context.can_execute_command(command)?;

        // Pick the platform shell. The previous code hard-coded `sh -c`,
        // which silently broke the x86_64-pc-windows-msvc release target
        // CI builds: spawn succeeded, `sh` was not found, and the caller
        // saw a generic "no such file or directory" with no hint that
        // the tool had chosen the wrong interpreter.
        //
        // `cmd /C` is the closest Windows analogue of `sh -c`: it runs
        // the command and exits. Neither shell loads a user profile, so
        // aliases and rc files are not in scope.
        let (shell, shell_flag) = if cfg!(windows) {
            ("cmd", "/C")
        } else {
            ("sh", "-c")
        };

        // Spawn with piped stdio so each stream is capped independently
        // and the child is killed the moment output runs away.
        let mut child = tokio::process::Command::new(shell)
            .arg(shell_flag)
            .arg(command)
            .stdin(std::process::Stdio::null())''',
    "execute_command shell selection",
)

# --- 3. context.rs: expand the dangerous-command list ------------------
patch(
    ctx,
    '''        // Check for dangerous commands
        let dangerous_patterns = ["rm -rf", "sudo", "chmod 777", "> /dev/sda"];
        for pattern in &dangerous_patterns {
            if command.starts_with(pattern) {
                return Err(KodError::PermissionDenied {
                    action: "execute".to_string(),
                    reason: format!("Dangerous command pattern detected: {}", pattern),
                });
            }
        }''',
    '''        // Refuse a small set of unambiguously destructive commands.
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
        }''',
    "dangerous-command list",
)

print("All patches applied.")
PYEOF

if [ $? -ne 0 ]; then
    echo "ERROR: patching failed"
    exit 1
fi

echo "Checking compilation"
if ! cargo check --workspace 2>&1; then
    echo "Compilation failed – will skip commit"
    COMPILE_OK=false
fi

if [ "$INCOMPLETE" = true ] || [ "$COMPILE_OK" = false ]; then
    echo "Skipping tests and commit due to incomplete files or compilation errors"
    exit 1
fi

echo "Running kod-tools tests (120s wall clock)"
if ! run_with_timeout 120 cargo test -p kod-tools 2>&1; then
    echo "kod-tools tests failed or hung. Paste the full output for a surgical fix."
    exit 1
fi

echo "Running workspace tests (300s wall clock)"
if ! run_with_timeout 300 cargo test --workspace 2>&1; then
    echo "Workspace tests failed or hung. Paste the full output for a surgical fix."
    exit 1
fi

echo "Running clippy with -D warnings"
if ! cargo clippy --workspace --all-targets -- -D warnings 2>&1; then
    echo "Clippy failed. Paste the full output for a surgical fix."
    exit 1
fi

echo "All checks passed. Committing."
git add -A
git commit -m "fix(tools): use cmd /C on Windows in execute_command

execute_command hard-coded sh -c. On the x86_64-pc-windows-msvc
target that CI builds, this spawned a process, failed to find sh,
and returned a generic 'no such file or directory' with no hint
that the tool had picked the wrong interpreter. Every Windows
release build was shipping a tool that could never succeed.

Select the shell at runtime via cfg!(windows): cmd /C on Windows,
sh -c elsewhere. Update the tool description to say which shell
runs.

Expand the dangerous-command guard in ToolContext::can_execute_command
with the cmd.exe equivalents (format, del /f /q /s, rd /s /q,
rmdir /s /q) so the refusal list is meaningful on both platforms.
The check is documented as a guardrail, not a sandbox — the real
defense is ToolPermissions::execute_commands being off by default."
