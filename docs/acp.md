# ACP v1

URI Agent can run as a stable Agent Client Protocol (ACP) v1 agent over
standard input and output. This mode lets an ACP client own the conversation
while URI Agent continues to use its normal model runtime, tools, frozen
startup context, and durable sessions.

## Start the agent

Configure a provider, model, and credentials in the TUI before using ACP, or
provide the normal provider and model command-line overrides. ACP mode has no
interactive login or model selector.

Configure the ACP client to launch:

```text
uri-agent --acpv1
```

`--acpv1` reserves stdout for ACP JSON-RPC and does not initialize the terminal
interface or background resident mode. It conflicts with `--cwd`,
`--continue-session`, `--session`, and `--background`; the ACP request supplies
the session working directory. Model, provider, thinking, credential, catalog,
offline, and output-limit overrides remain available.

Each newline-delimited JSON-RPC message on stdin is limited to 16 MiB. URI
Agent closes the ACP transport if a client exceeds that limit.

## Project and session lifecycle

Every new, loaded, or resumed ACP session supplies an absolute `cwd`. The first
such request binds the process to that canonical project directory. Later
requests in the same process must resolve to the same directory. Start one URI
Agent process per project.

ACP mode supports session creation, load with history replay, resume, project
listing, close, prompt, and cancellation. Additional working directories and
list pagination are not supported. A list request without `cwd` returns an
empty list until the process is project-bound.

An ACP-created session is an ordinary persisted depth-1 URI Agent session,
including when it has no prompts. `session/close` releases the in-process Agent
but does not delete its history. After the ACP client closes the session or the
ACP process exits, open it in the TUI with:

```bash
uri-agent --cwd /absolute/project/path --session <session-id>
```

The TUI restores the same frozen system prompt, Skill snapshots, model history,
protocol set, provider, model, thinking level, and session-scoped MCP profile.
ACP and TUI ownership is sequential; do not open the same session concurrently
in separate processes.

## Prompt and update mapping

Prompts accept text, resource links, and JPEG, PNG, GIF, or WebP images whose
declared media type matches their data. Audio, embedded resources, and other
content block types are rejected. Resource links enter the user prompt as
Markdown links.

Durable assistant text and reasoning, tool calls and results, and usage are
projected to ACP session updates. URI Agent may discard provisional model
output while retrying a provider request, whereas ACP content chunks are
append-only. The adapter therefore publishes assistant content only after its
native session event commits. Prompt responses settle after the complete turn,
including tool cleanup and durable cancellation records.

An ACP prompt is accepted only while its session has no unrelated active turn
or queued native input. This keeps ACP cancellation and completion attached to
that exact prompt instead of an earlier recovered turn.

## MCP servers

ACP clients may provide stdio or Streamable HTTP MCP servers when creating,
loading, or resuming a session. HTTP+SSE is not supported.

For a newly created ACP session, the supplied MCP list is the complete
session-scoped MCP profile, including when the list is empty. URI Agent does
not merge it with User or Project `mcp.json`. Server names determine the frozen
protocol set. Load and resume must provide the same names, but may rotate
commands, arguments, URLs, headers, and credential values. ACP cannot inject an
MCP profile into an existing native session that uses configured MCP.

Session MCP values are stored in a private SQLite session record so a later TUI
open can reconstruct the capabilities. Literal environment and header values
do not enter append-only events or frozen protocol records. The TUI's `:mcp`
panel lists these servers without displaying literal values; it permits test
and reconnect operations but not edits. Connection errors omit session MCP
transport details so literals do not enter diagnostics or transcript errors.
The ACP client remains the owner of that profile. Private session records are
plaintext in the session database, not encrypted; filesystem permissions are
their protection boundary.

URI Agent and each MCP child run with the authority of their processes. Use
only projects and server definitions you trust.
