# KOD TUI commands

Every slash command the TUI understands, plus the keybindings you
will actually use. The in-session `/help` overlay is the source of
truth; this file is a broader reference.

## Command palette

**Ctrl+K** opens a fuzzy-searchable list of every slash command plus
a small set of keybindings. Type to filter; ↑/↓ move; Enter inserts
the selected command into the input box; Esc closes.

The palette is the recommended way to discover what is available.
Press it any time you cannot remember a command.

---

## Session

| Command | What it does |
|---|---|
| `/help` | Show the help overlay. |
| `/clear` | Ask for confirmation, then wipe the chat. |
| `/clearall` | Wipe chat + memory + checkpoints. |
| `/undo` | Restore the chat cleared by `/clear`. |
| `/quit` | Quit; asks when a generation is running. |
| `/reset` | Clear transient state (input, search, expansions). |

## Prompts and messages

| Command | What it does |
|---|---|
| `/goal <text>` | Set a goal the agent works toward. `/goal clear` removes. |
| `/steer <instruction>` | Redirect the running prompt. |
| `/cancel` | Stop the running prompt. |
| `/retry` | Reconnect and resend the last prompt. |
| `/regenerate` | Regenerate the last assistant reply. |
| `/edit` | Load the last user message back into the input box. |
| `/delete` | Remove the last user+assistant exchange. |
| `/refine <instruction>` | Refine the last assistant reply. |
| `/summarize` | LLM-summarize the session. |
| `/raw` | Print the last assistant reply raw. |
| `/copy` | Copy the last assistant reply. |

## Model and skills

| Command | What it does |
|---|---|
| `/model` | List available models or switch: `/model <name>`. |
| `/skills` | List loaded skills, or `/skills <name>` for the body. |
| `/tools` | Toggle tool output details. |

## Policy and safety

| Command | What it does |
|---|---|
| `/policy` | Tool policy: `/policy [show | forget <n>]`. |
| `/trust` | Show or clear the current round's taint. `/trust [show | clear]`. |
| `/learned` | List or clear session-scoped learned approvals. `/learned [show | clear]`. |
| `/budget` | Session cost and limits. `/budget [raise <usd> | reset]`. |
| `/limits` | Per-tool counters. `/limits [show | reset]`. |
| `/redact test <string>` | Preview what would be redacted. |
| `/redact list` | Active redaction rules with hit counts. |

## Plan, decisions, memory

| Command | What it does |
|---|---|
| `/plan` | Show the plan. `/plan [next | skip | note <text> | clear]`. |
| `/decisions` | Durable decisions. `/decisions [show | drop <id> | clear]`. |
| `/memory` | Long-term memory. `/memory [search <q> | delete <id> | clear | eval]`. |
| `/remember <text>` | Store a durable fact. Requires an engine. |

## Swarm

| Command | What it does |
|---|---|
| `/swarm <goal>` | Run N agents on a goal. |
| `/blackboard` | View or clear the shared swarm blackboard. `/blackboard [show | clear]`. |

## Jev (TypeSafe AI)

| Command | What it does |
|---|---|
| `/jev` | Status: enabled, model, cache, thresholds. |
| `/jev stats` | Decision summary for the session. |
| `/jev cache clear` | Drop the decision cache. |
| `/jev test` | Ping the endpoint and print the result. |
| `/jev tune` | Show all thresholds. |
| `/jev tune set <name> <value>` | Set a threshold and persist. |
| `/jev tune reset` | Restore defaults. |

## Observability

| Command | What it does |
|---|---|
| `/trace` | Live tree of the current turn. |
| `/trace last` | Previous completed turn. |
| `/trace list` | Table of the last 20 turns. |
| `/trace <id>` | One turn by id. |
| `/log [N]` | Recent session-log entries. |
| `/debug last-prompt` | The exact prompt the model received. |
| `/stats` | Roles, tools, tokens, elapsed. |
| `/whoami` | Session summary. |
| `/context` | Context-window usage. |
| `/doctor` | Diagnostics report. |
| `/init` | Onboarding info. |

## Files and diffs

| Command | What it does |
|---|---|
| `/map [max_chars]` | Repository map. |
| `/diff` | Most recent file diff. |
| `/check` | Run the project compiler/linter. |
| `/checkpoints` | List file checkpoints. |
| `/rollback [id]` | Restore a file from a checkpoint. |
| `/attach <path>` | Attach a file to the next prompt. |
| `/git-status` | `git status --porcelain=v2` in the current dir. |

## Sessions and export

| Command | What it does |
|---|---|
| `/export [path]` | Export session as markdown. |
| `/export-html [path]` | Export as a self-contained HTML file. |
| `/save <path>` | Save session to a JSON file. |
| `/load <path>` | Load session from a JSON file. |
| `/handoff` | Write a handoff doc and start fresh with it. |
| `/branch [label]` | Drop a branch-point marker. |
| `/fork [label]` | Save the current chat as a restorable fork. |
| `/compact` | Compact session history now. |

## Search and navigation

| Command | What it does |
|---|---|
| `/search <text>` | Search chat; `n`/`N` jump. |
| `/grep <regex>` | Regex search the chat history. |
| `/pin <n>` | Pin a message so it survives compaction. |
| `/unpin <n>` | Remove a pin. |

## Theme

| Command | What it does |
|---|---|
| `/theme [dark | light]` | Switch theme. |

---

## Keybindings

The bindings below are always active. `~/.config/kod/tui_keys.toml`
and `.kod-keys.toml` can rebind the single-key ones.

| Key | Action |
|---|---|
| `i` | Enter insert mode |
| `Esc` | Leave insert mode / cancel the running prompt |
| `Enter` | Send (insert mode) |
| `Ctrl+J` / `Shift+Enter` | Newline in the input box |
| `Ctrl+K` | Command palette |
| `Ctrl+E` | Edit your last message |
| `Ctrl+U` | Delete to the start of the line |
| `Ctrl+W` | Delete the previous word |
| `j` / `k` or wheel | Scroll chat |
| `g` / `G` | Oldest / newest |
| `PgUp` / `PgDn` / `Home` / `End` | Scroll |
| `t` | Toggle tool output |
| `e` | Edit your last message |
| `u` | Undo a `/clear` |
| `f` | Start a search |
| `n` / `N` | Next / previous search hit |
| `y` | Copy the last assistant reply |
| `Tab` / `a` | Agent panel |
| `p` | Plan panel |
| `?` | Help overlay |
| `q` | Quit |

### Approval dialog keys

| Key | Action |
|---|---|
| `y` | Approve |
| `n` | Deny |
| `a` | Deny-always (session) |
| `l` | Learn an allow for this exact call |
| `e` | Edit the call's arguments before deciding |
| `h` | Partial-hunk selection for `patch_file` |
| `↑` / `↓` | Navigate the batch |
| `Esc` | Deny the current and every remaining item |

### Hunk-selection keys

| Key | Action |
|---|---|
| `Space` / `x` | Toggle the current hunk |
| `↑` / `↓` | Move between hunks |
| `Enter` | Commit the filtered patch |
| `Esc` | Cancel back to the dialog |
