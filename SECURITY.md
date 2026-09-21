# Security Policy

## Reporting a vulnerability

Please report security issues privately via GitHub's
[private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability)
on this repository.

Do **not** open a public issue for a vulnerability that could affect
other users before a fix is available. Include:

- a description of the issue and the impact,
- a minimal reproduction (a config, a prompt, or a test),
- the affected version (`kod --verbose` prints the commit hash),
- any suggested mitigation.

You can expect an acknowledgement within 5 business days.

## Scope

KOD is a local-first agent harness. It reads and writes files, runs
shell commands, and — when explicitly enabled — fetches URLs. The
security boundary is the boundary between **the model's output** and
**the user's machine**.

### What KOD defends against

- **Prompt-injected tool calls.** The policy engine (`ToolPolicy`,
  presets `read-only` / `standard` / `yolo`) gates every tool call
  before it runs. `standard` — the default — requires user approval
  for writes and shell execution.
- **Path traversal.** `ToolContext::resolve_path` canonicalizes
  paths, rejects symlinks that escape the workspace, and enforces
  the caller's `allowed_paths` / `forbidden_paths` globs. The policy
  engine resolves paths the same way, so `src/../secrets/x` cannot
  match a `src/**` allowlist and reach a file outside `src/`.
- **Project-level policy escalation.** A `.kod/policy.toml` shipped
  by a cloned repository can only **narrow** the effective policy
  (a stricter preset, an `ask` or `deny` per-tool mode). It cannot
  widen it.
- **Hook injection.** Hook templates are config-trusted; hook
  *arguments* (tool call parameters, model-controlled) are passed
  as environment variables (`$KOD_PATH`, `$KOD_COMMAND`, …) and are
  never spliced into the command string.
- **Sandbox escape.** On Linux and macOS, `execute_command` runs
  under bubblewrap, landlock, or seatbelt (whichever is available).
  `Require` fails loudly when none is available; `Auto` warns.
- **`.git` mutation via file tools.** `write_file` and `patch_file`
  refuse any path inside `.git/`. Git mutations must go through the
  git tools, which are gated on `GitAccess::Write`.
- **SSRF.** `web_fetch` refuses private hosts and addresses,
  re-validates on every redirect hop, and pins the connection to
  the address it validated (so DNS rebinding cannot flip the
  destination between the check and the connect).
- **Secret exfiltration.** `execute_command` runs with a small
  environment; variables whose names match `KEY`/`TOKEN`/`SECRET`
  and the `ANTHROPIC_*`/`OPENAI_*`/`AWS_*`/`GCP_*`/`GOOGLE_*`/
  `AZURE_*` prefixes are stripped before the child sees them.
  Redaction is also applied to tool results before they reach the
  prompt (Tier 1.3).
- **Known-secret files.** `read_file` on `.env`, `*.pem`,
  `id_rsa*`, `.aws/credentials`, and similar paths is gated by
  `ReadProtection` (default: redact; `refuse` is stricter).
- **Corrupt session logs.** A truncated last line is tolerated;
  the reader keeps every complete line.

### What KOD does **not** defend against

- **A user who runs `--preset yolo`.** The default `standard` preset
  is a safety rail, not a sandbox. `yolo` disables it deliberately.
- **A hostile model server.** If you point `base_url` at a server
  you do not control, that server sees every prompt and can return
  arbitrary tool calls. Run against a model you trust.
- **A malicious `mcp` server you explicitly registered.** MCP
  servers are child processes; they are trusted the moment they are
  spawned. Review a server's source before adding it.
- **A user who writes an insecure hook.** A `pre_tool_use` template
  that runs `sh -c "$KOD_CONTENT"` is exactly as dangerous as it
  looks; hooks are config-trusted code.
- **Local privilege escalation.** The sandbox bounds what a tool
  *call* can do; it does not protect against a compromise of the
  `kod` binary itself.

## Tiers of concern, by feature

| Feature | Trust boundary | Notes |
|---|---|---|
| `read_file` / `list_files` / `grep` | `ReadProtection` + policy | Redaction for known-secret paths |
| `write_file` / `patch_file` | Policy + `.git/` deny | Atomic writes |
| `execute_command` | Sandbox + env stripping + policy | Config-trusted binaries allowlist |
| `web_fetch` | SSRF filter + policy domain allowlist | Per-hop re-validation |
| `git_*` | `GitAccess::Write` policy | Bypasses the sandbox by design |
| Hooks | Config-trusted | Arguments env-var only |
| MCP servers | User-registered child processes | Inherit the process UID |
| Skills | Loaded from `cwd/.kod/skills`, `~/.kod/skills` | Hot-reloadable |
| Policy (`standard` preset) | `Ask` for writes + shell | User approves per call |

## Verifying a release

Every GitHub release ships an archive, a `.sha256`, and (when the
maintainer has configured signing) a `.minisig`. Verify with:

```sh
sha256sum -c kod-<target>.tar.gz.sha256
minisign -Vm kod-<target>.tar.gz -P <pubkey>
```
