# ACP v1

URI Agent can run as a stable Agent Client Protocol (ACP) v1 agent over
standard input and output. This mode lets an ACP client own the conversation
while URI Agent continues to use its normal model runtime, tools, frozen
startup context, and durable sessions.

## Start the agent

Configure model credentials in the TUI before using ACP. The configured default
model and thinking level initialize each new ACP session; clients that support
ACP session configuration can choose another authenticated model and thinking
level before sending the first prompt. Normal command-line model overrides can
also supply the initial selection. ACP mode has no interactive login.

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

Every new, loaded, or resumed ACP session supplies an absolute `cwd`. URI Agent
canonicalizes it and keeps a separate project runtime for each directory. One
ACP process can therefore own sessions from multiple independent projects;
configuration, plugins, MCP connections, Skills, and Agent state remain
isolated by project.

ACP mode supports session creation, load with history replay, resume,
project-filtered or process-wide listing, close, prompt, and cancellation.
Additional working directories and list pagination are not supported. A list
request without `cwd` merges sessions from every project runtime initialized by
that process and returns an empty list before the first project is initialized.

`session/new` reserves an ID and keeps the working directory, MCP profile,
provider, model, and thinking level in process memory. The pending session is
visible to that process's `session/list` and can be released with
`session/close`, but it does not yet exist in the native session database and
does not survive process exit.

The first `session/prompt` creates the depth-1 URI Agent session with the same
reserved ID and persists its frozen setup together with the accepted input. A
startup or persistence failure before input acceptance leaves the pending ACP
session available for correction or retry. Once input is accepted, provider,
model, and thinking level are frozen; `session/set_config_option` cannot switch
them during the conversation. ACP selections are session-local and never
change User, Project, command-line, or process defaults.

`session/close` releases process ownership but does not delete a materialized
session's history. After the ACP client closes that session or the ACP process
exits, open it in the TUI with:

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

When the first prompt materializes the session, MCP values are stored in a
private SQLite session record so a later TUI open can reconstruct the
capabilities. Literal environment and header values do not enter append-only
events or frozen protocol records. The TUI's `:mcp` panel lists these servers
without displaying literal values; it permits test and reconnect operations
but not edits. Connection errors omit session MCP transport details so literals
do not enter diagnostics or transcript errors.
The ACP client remains the owner of that profile. Private session records are
plaintext in the session database, not encrypted; filesystem permissions are
their protection boundary.

URI Agent and each MCP child run with the authority of their processes. Use
only projects and server definitions you trust.
