# Protocols, tasks, and output

URI Agent keeps the initial model interface small and loads operational detail
only when a capability is needed. This document explains that design and the
stable behavior shared across protocols. For exact addresses, query fields,
limits, and examples, read the active `<protocol>://help`; direct-tool schemas
are authoritative for their arguments.

## Model interface

Linked built-ins register four tools:

```text
read(uri: string, body: string)
exec(uri: string, body: string)
replace(path: string, old_text: string, new_text: string)
apply_patch(patch: string)
```

`read` and `exec` always require a string body. Use `""` when an operation has
no body, plain text for textual input, and complete serialized JSON only when a
protocol explicitly requires it. Runtime-loaded WASM plugins may add typed
direct tools.

Before using a protocol, the model must successfully read its exact
`<protocol>://help` address with an empty body. A protocol may declare ordered
shared-help prerequisites, but its own help remains mandatory. Successful help
reads are remembered within the session and restored on resume.

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
| `grep` | `read`, `exec` | Run exact, semantic, or hybrid project search |
| `sessions` | `read`, `exec` | Discover and search saved conversations without changing them |
| `context` | `read`, `exec` | Inspect context, maintain notes, recover history, and request rollover |
| `https` | `read` | Search the web and extract HTTPS pages |
| `tasks` | `read`, `exec` | Inspect, wait for, and cancel managed work |
| `bash` or `pwsh` | `read`, `exec` | Run shell commands |
| `wasm_plugin` | `read`, `exec` | Inspect and reload trusted WASM plugins |
| `<name>-skill` | `read` | Load a discovered [Skill](context.md#skills) and its resources |
| `mcp` | `read` | Load shared MCP routing and argument help |
| `<name>-mcp` | `read`, `exec` | Use one configured MCP server |

Shell availability is platform-dependent. Windows prefers PowerShell 7 and
falls back to Bash when PowerShell cannot start; other platforms use Bash when
available.

### MCP

Each enabled MCP server recorded for a new session becomes a normalized
`<name>-mcp` protocol. The shared `mcp://help` page defines common routing and
argument encoding; each server help page adds its frozen description, current
handshake metadata, and server instructions. Connections are lazy and belong
to one Agent session.

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

`grep` uses ripgrep for exact regular-expression or literal search. Semantic
and hybrid modes use private, disposable sidecar indexes. A ranked read creates
or incrementally refreshes its selected root and glob cache automatically;
explicit indexing is only for prewarming or repair. Use exact search for known
identifiers, hybrid search for most conceptual queries, and semantic search
when relevant text is likely to use different wording.

```text
read("grep://src", "ProtocolRequest")
read("grep://src?mode=hybrid&glob=**/*.rs", "credential refresh flow")
```

`context` exposes bounded recovery information for the active conversation,
including context usage, titled notes, prior windows, user statements, search,
and record neighborhoods. Notes have stable host-generated IDs and revision
anchors. Deleting a note hides its body from model-facing recovery routes but
is not secure erasure from the append-only session database.

`sessions` discovers and searches saved conversations. Project scope is the
default; broader scope must be requested explicitly. Exact search needs no
index, while semantic and hybrid reads maintain disposable scope-specific
caches automatically. Archive reads and indexing never resume or modify a
session. Results from both history protocols are bounded and marked as
untrusted reference data.

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
options and current limits live in `https://help` and its provider-specific
help pages.

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

## Shell execution and managed tasks

Shell commands start in the foreground and return their final output directly.
They run from the startup directory without Bash or PowerShell profile files.
Each new command receives the latest values from the Agent Environment manager;
the interactive `:terminal` is separate and does not receive them.

Long-running operations may continue as managed tasks without restarting.
Shell help also supports requesting immediate background execution and setting
the shared deadline. Cancellation and timeout terminate the owned process tree
and wait for root-process cleanup.

The `tasks` protocol reports `pending`, `running`, `completed`, `failed`, or
`cancelled` state, exposes bounded live output, preserves complete terminal
output, supports a bounded wait, and cancels active work. Acceptance into the
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
