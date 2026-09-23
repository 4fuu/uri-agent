# Protocols, tasks, and output

URI Agent keeps the initial model interface small and loads operational detail
only when a capability is needed. This document explains that design and the
stable behavior shared across protocols. For exact addresses, query fields,
limits, and examples, load the protocol's page through the `help` tool;
direct-tool schemas are authoritative for their arguments.

## Model interface

Linked built-ins register four tools:

```text
help(protocols: string[])
protocol(request: string)
replace(path: string, old_text: string, new_text: string)
apply_patch(patch: string)
```

`help` loads the model-facing contract pages of the named protocols in one
call; it loads at most the first eight requested names and reports any
remaining names in the result for a follow-up call. It is the only way to load
a contract: the `protocol` tool rejects any call whose contract has not been
loaded yet, and the exact `<name>://help` address is not readable through
it. Loaded contracts stay loaded for the rest of the session and are
restored on resume.

`protocol` calls use one fixed request format: a `*** Begin Request` line, one
`*** Read: <protocol>://<target>` or `*** Exec: <protocol>://<target>` line,
optional raw body lines, and a `*** End Request` line. Every line between the
operation line and `*** End Request` is the request body and is passed
verbatim. Leave no lines there when the operation takes no body. Complete
serialized JSON is that body only when a protocol explicitly requires it. The
four structural lines must match byte for byte. A leading `*** Body:` line is
still accepted and ignored so earlier requests keep working; new requests
should not include it. Runtime-loaded WASM plugins may add typed direct tools.

A protocol may declare shared-help prerequisites; `help` loads them
automatically ahead of the requested protocol, and using the dependent protocol
requires those shared pages to be loaded.

Routing is deliberately generic:

1. split the address only at the first `://`;
2. use the prefix as the registered protocol name;
3. pass the opaque remainder and string body to that protocol unchanged.

The registry does not parse protocol-specific paths or query fields. Protocol
names are unique, and duplicate registration fails rather than replacing an
existing capability.

## Built-in capabilities

| Capability | Operations | Purpose |
| --- | --- | --- |
| `uri-agent-docs` | `read` | Read version-matched documentation embedded in the binary |
| `file` | `read` | Read files, directories, globs, and supported images |
| `search` | `read`, `exec` | Run ripgrep, semantic, or hybrid project search |
| `context` | `read`, `exec` | Inspect active context, maintain current-session notes, recover history, and read saved sessions |
| `collaboration` | `read`, `exec` | Name this session, inspect active participants and model status, and exchange Queue or Steer messages |
| `https` | `read` | Search the web and extract HTTPS pages |
| `finder` | `exec` | Delegate a multi-step lookup to a finder Agent and return its final answer |
| `tasks` | `read`, `exec` | Inspect, wait for, feed input to, and cancel managed work |
| `bash` or `pwsh` | `read`, `exec` | Run shell commands, optionally with runtime input |
| `wasm_plugin` | `read`, `exec` | Inspect and reload trusted WASM plugins |
| `<name>-skill` | `read` | Load a discovered [Skill](context.md#skills) and its resources |
| `mcp` | `read` | Load shared MCP routing and argument help |
| `<name>-mcp` | `read`, `exec` | Use one configured MCP server |

Shell availability is platform-dependent. Windows prefers PowerShell 7 and
falls back to Bash when PowerShell cannot start; other platforms use Bash when
available.

### MCP

Each enabled MCP server recorded for a new session becomes a normalized
`<name>-mcp` protocol. The shared `mcp` help page defines common routing and
argument encoding; each server help page adds its frozen description, current
handshake metadata, and server instructions. Loading a server's help through
the `help` tool loads the shared page automatically. Connections are lazy and
belong to one Agent session.

Tool and prompt catalogs remain behind protocol reads, and each operation uses
the server's current JSON Schema. Simple values can be represented in the URI;
complex arguments can use a complete JSON body. Read the active help before
constructing either form rather than relying on copied static syntax.

Every operation resolves current server and Agent Environment configuration.
Changing either reconnects the server; removing or disabling a server already
recorded by a session makes later calls fail. MCP content and instructions are
untrusted external data. Configure servers through `:mcp` or the files
described in [Models and configuration](configuration.md#mcp-servers).

### Files, search, and saved context

`file` resolves relative paths from the canonical startup directory and leaves
absolute paths absolute. It reads bounded text ranges, efficient text-file
tails, bounded directory or glob listings, and PNG, JPEG, GIF, or WebP images.
Image reads require a model whose catalog declares image input.

`search` uses ripgrep (`rg`) for regular-expression or literal search. Semantic
and hybrid modes use private, disposable sidecar indexes. A ranked read creates
or incrementally refreshes its selected root and glob cache automatically;
explicit indexing is only for prewarming or repair. Use exact search for known
identifiers, hybrid search for most conceptual queries, and semantic search
when relevant text is likely to use different wording.

```text
*** Begin Request
*** Read: search://src
ProtocolRequest
*** End Request

*** Begin Request
*** Read: search://src?mode=hybrid&glob=**/*.rs
credential refresh flow
*** End Request
```

`context` exposes bounded recovery information for the active conversation,
including context usage, titled notes, prior windows, user statements, search,
and record neighborhoods. Under `context://sessions/...`, it also discovers and
searches saved conversations and reads their transcript or notes without
resuming them. Saved-session state is read-only; note mutation remains limited
to the current session. Project scope is the default and broader discovery or
search scope must be requested explicitly. Exact search needs no index, while
semantic and hybrid reads maintain disposable scope-specific caches
automatically. Results are bounded and marked as untrusted reference data.
All model-facing saved-session addresses use `context://sessions/...`.

### Delegated search

`finder` runs one delegated lookup per call. The request body is a complete
natural-language question; `finder://<root>` restricts code search to one
project-relative or absolute directory with the same root rules as `search://`,
while web reads stay unscoped. Each call starts a depth-2 Agent with read-only
search, file, web, and task capabilities, a finder system prompt, and the model
configured for the `finder` role; the reply ceiling is that model's own catalog
output limit. The calling model receives the finder's final reply directly or
through a background task after the foreground grace period.

The protocol is registered only for new depth-1 sessions whose `finder` role
resolves ([Model roles](configuration.md#model-roles-and-plugin-settings)); it
starts unassigned, so finder is absent until configured. The protocol keeps it on resume, and calls fail if the role no longer resolves.
Finder replies are untrusted data from another model. Exact syntax and limits
live in the finder help page.

### Collaboration

`collaboration` connects depth-1 TUI sessions already running in local URI
Agent processes. A session can persist a short human name; active-name
collisions receive numeric suffixes. Participant reads report the stable
session ID, working directory, bounded first-request summary, provider/model,
`idle` or `working` status, queue depth, and last heartbeat. Names resolve only
while active; stable IDs remain suitable for `context://sessions/...` reads.

Messages use a plain-text request body and target one active name or
session ID.
`queue` durably schedules a later turn, while `steer` targets the next model
boundary and becomes a queued turn if the target is idle. The host wraps the
body in an XML envelope containing trusted source metadata, including the
stable source session ID and a generated message ID. Peer content inside that
envelope remains untrusted and grants no user authorization. A requested reply
adds an exact ID-based reply route but neither waits nor guarantees a response.

Acceptance means that the message was committed to the target's durable input
queue. Collaboration does not start a stopped process, broadcast, transfer
files, or wait for remote completion. ACP-owned and child Agent sessions do not
join live collaboration. Exact routes, name rules, options, limits, and the XML
behavior are defined by the collaboration help page.

`uri-agent-docs` reads the Markdown files embedded at build time. Start at
`uri-agent-docs://README.md` for the version-matched documentation index.

### Web access

`https` uses configured Parallel, Exa, or TinyFish credentials for search and
page extraction. Without an explicit provider, it tries configured providers
in stable order and falls back after provider failures. When no provider is
configured, ordinary page reads can still use direct local HTTPS fetching.

HTML is converted to Markdown, JSON is formatted, and other textual responses
remain text. Direct fetching does not execute JavaScript or extract PDFs.
Redirects remain on HTTPS, and returned web content is untrusted. Provider
options and current limits live in the `https` help page and provider pages
such as `https://help/parallel`.

## Editing tools

The typed `replace` tool requires a nonempty `old_text` that occurs exactly
once. Missing or ambiguous matches fail without changing the file; successful
writes are atomic.

The typed `apply_patch` tool supports adding, updating, moving, and deleting
multiple files in one Codex-format patch. URI Agent preflights the complete
in-memory plan before writing, so parsing and planning failures leave files
unchanged. If writing fails after some changes, it attempts to roll them back
and reports any rollback failure. A plan that leaves the original files
unchanged is reported explicitly. The active tool schema owns the exact grammar
and argument contract.

Both tools resolve relative paths from the startup directory and accept
absolute paths. On Unix, `~` and `~/` expand to the current user's home.
Symbolic-link paths are rejected.

## Shell execution and managed tasks

Shell commands start in the foreground and return their final output directly.
They run from the startup directory without Bash or PowerShell profile files.
Each new command receives the latest values from the Agent Environment manager;
the interactive `:terminal` is separate and does not receive them. On Windows,
each spawned process tree receives its own hidden console instead of the
terminal's console, so commands can neither read nor clobber interactive
terminal input; a program that waits for console input never receives it and
keeps running until its timeout or background promotion ends the wait, unless
the command was started with `interactive=true`.

Long-running operations may continue as managed tasks without restarting. A
foreground operation that outlives its grace period moves to the background
even when the background task limit is already reached. Shell help also
supports requesting immediate background execution and setting the shared
deadline. Cancellation and timeout terminate the owned process tree and wait
for root-process cleanup.

Shell help also supports `interactive=true` for commands that consume runtime
input such as prompts, passwords, or REPLs. An interactive command always runs
as a managed task with its stdin kept open, and its script text is delivered
through a short-lived private temporary file so stdin stays reserved for the
program. Input, end-of-file, and interrupt operations for such tasks go
through the `tasks` protocol. On Unix, interrupt sends SIGINT to the process
group and the child restores the default SIGINT disposition so scripts can
trap it; on Windows, interrupt terminates the process tree like cancellation.

The `tasks` protocol reports `pending`, `running`, `completed`, `failed`, or
`cancelled` state, exposes bounded live output, preserves complete terminal
output, supports a bounded wait, delivers input to interactive tasks, and
cancels active work. Acceptance into the
task manager is not completion. Terminal task records survive session resume;
their processes do not, so work interrupted by process exit is restored as
cancelled.

When a task settles, URI Agent notifies the model automatically with a bounded
output tail. If that notification says the result was truncated, follow its
single `tasks://` read instruction instead of polling or rerunning the work.

## Complete output and diagnostics

When a tool result exceeds the configured inline limit, URI Agent stores the
complete bytes under the session output directory and returns a readable
head-and-tail preview with a `file://` address. This applies to protocol calls,
WASM tools, and formatted failures.

Each session output directory also contains `diagnostics.jsonl`. Diagnostics
record lifecycle metadata such as call IDs, field names and sizes, timing,
state, and selected provider identifiers. They do not copy raw arguments,
credentials, environment values, or successful tool output. `:status` shows
the path, and [Models and configuration](configuration.md) describes the inline
limit.

## Extensions

Use a protocol when a capability has a simple string input and a typed direct
tool when common calls would otherwise require complex or escape-heavy nested
serialization. Linked Rust extensions and trusted WASM modules can register
both; Skills register read-only protocols. See [WASM plugins](plugins.md),
[Startup context and Skills](context.md), and the [development
guide](development.md#linked-rust-extensions).
