# kod-mcp

Minimal Model Context Protocol (MCP) client over stdio for KOD.

Speaks the MCP request/response protocol to external servers
(filesystem, git, github, etc.) so the KOD engine can expose them as
tools. Handles the process lifecycle, request/response framing, and
the pending-request map behind a mutex.

## Part of KOD

Reached transitively through `kod-core-tools`.

See the [KOD workspace](https://github.com/elcoosp/kod).

