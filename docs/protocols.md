# Protocols, tasks, and output

URI Agent keeps protocol routing small while allowing plugins to register both protocols and typed direct tools. This document describes routing, built-in protocols, direct editing tools, execution semantics, managed tasks, and output preservation. Skill discovery and resources are documented in [Startup context and Skills](context.md).

For the exact runtime syntax of a protocol, read `<protocol>://help`. Those help routes are the canonical model-facing operation reference.

## Model interface

The linked built-in plugins register four tools:

```text
read(uri: string, body: string)
exec(uri: string, body: string)
replace(path: string, old_text: string, new_text: string)
apply_patch(patch: string)
```

`body` is always required and always a string. Pass `""` when a protocol takes
no body, pass plain text for textual input, and pass the complete serialized
JSON text when a protocol requires structured input. Protocols receive that
string unchanged, including an empty string.

Before any other call to a protocol in a session, the model must successfully
call `read("<protocol>://help", "")`. A protocol may declare shared help
prerequisites; those help pages must be read first, in the declared order, and
its own help remains mandatory afterward. The runtime blocks calls that skip
the next required help read and returns its exact address. Successful help
reads are tracked for the session.

`read` is used for resources, help, task snapshots, and completed output. A
linked protocol read may return supported images alongside its textual result;
the model receives those images as typed tool-result content. `exec` starts
work through protocols that support execution. `replace` and `apply_patch`
avoid nesting edit payloads inside a serialized protocol body. Runtime-loaded
WASM plugins may add more typed direct tools, so the active tool set is not
limited to these four. A protocol may implement `read`, `exec`, or both.

Model dispatch and the [protocol registry](../src/protocol.rs) apply three routing rules:

1. Split an address only at the first `://` and use the part before that
   delimiter as the registered protocol name.
2. Pass the entire remainder to the protocol as an opaque target. The registry
   does not URL-decode, normalize, or parse options from it.
3. Pass the required string body to the selected protocol unchanged.

For example, the target received by `capture` here is exactly `a://b?not=a url`:

```text
read("capture://a://b?not=a url", "")
```

Protocols own their target syntax. `file` interprets `?offset`, `?limit`, and
`?line_numbers`; `bash` and `pwsh` interpret `?background=true` and
`?timeout=<seconds>`; the registry treats all of those characters as opaque.

Protocol names must be unique. Registration fails rather than silently replacing an existing protocol.

## Built-in protocols

| Protocol | Operations | Responsibility |
| --- | --- | --- |
| `uri-agent-docs` | `read` | Read version-matched URI Agent documentation embedded in the binary |
| `file` | `read` | Read files, bounded directory listings, and recursive glob results |
| `grep` | `read`, `exec` | Search files with exact ripgrep matching or on-demand semantic retrieval |
| `sessions` | `read`, `exec` | Discover and search saved sessions while keeping the source archive read-only |
| `context` | `read`, `exec` | Inspect context usage, maintain titled notes, search prior windows, and request rollover |
| `https` | `read` | Search through a logged-in web provider and read HTTPS pages as text |
| `tasks` | `read`, `exec` | Inspect and cancel background tasks from every protocol |
| `bash` | `read`, `exec` | Run Bash commands in the foreground or as managed background tasks when Bash is enabled |
| `pwsh` | `read`, `exec` | Run PowerShell 7 commands in the foreground or as managed background tasks when `pwsh` is enabled |
| `wasm_plugin` | `read`, `exec` | Reload trusted WASM protocols and direct tools and return the completed reload report |
| `<name>-skill` | `read` | Load one [discovered Skill](context.md#skills) and its bundled files |
| `mcp` | `read` | Provide shared help for configured MCP protocols; registered only when at least one MCP server is frozen into a new session |
| `<name>-mcp` | `read`, `exec` | Access one configured MCP server's tools, resources, templates, and prompts |

Shell plugins detect their own executables at startup. On Windows, the `pwsh`
plugin also verifies that PowerShell 7 or newer can start. A valid `pwsh`
plugin suppresses `bash`; otherwise `pwsh` remains disabled, a startup warning
is shown, and `bash` remains available when installed. On non-Windows
platforms, only the `bash` plugin is considered; `pwsh` is not started.

### MCP servers

The linked MCP plugin turns each enabled server recorded for a new session into
one protocol. Names use the Skill normalization rule—lowercase ASCII letters
and numbers with other runs replaced by `-`—and gain `-mcp` when absent. A
server named `GitHub` therefore becomes `github-mcp://`. A normalized collision
is an error rather than an implicit replacement.

When at least one MCP server is frozen into a new session, `mcp://` is
registered alongside the server protocols. Read its shared route and argument
contract once, then read each server's generated help before using that server:

```text
read("mcp://help", "")
read("github-mcp://help", "")
```

The runtime enforces that order. `mcp://` exposes no route other than `help`
and does not connect to a server. Each `<name>-mcp://help` connects lazily and
contains only that record's frozen description plus current handshake metadata
and server instructions, marked as untrusted external content; it does not
repeat the shared routes or argument rules.

Catalogs and individual schemas remain behind separate reads:

```text
read("github-mcp://tools", "")
read("github-mcp://tools/search_repositories", "")
read("github-mcp://resources", "")
read("github-mcp://resource-templates", "")
read("github-mcp://prompts", "")
```

Tool and prompt arguments use their current JSON Schema. Put scalar values in
the URI query, repeat one key for an array, and use `/` between nested object
property names. Names and values use form URL encoding:

```text
exec("github-mcp://tools/search?query=uri-agent&limit=10&labels=rust&labels=agent", "")
exec("postgres-mcp://tools/query?options%2FreadOnly=true&_body=sql", "SELECT 1")
exec("workflow-mcp://tools/run?_json=true", "{\"steps\":[{\"kind\":\"build\"}]}")
```

`_body=<schema/path>` may bind exactly one string field to the raw string body.
For schemas that cannot be represented by scalar query paths, including
compositions, references, and arrays of objects, `_json=true` passes a complete
JSON argument object from the body and cannot be combined with another query
argument for validation by the server. Otherwise, the body must be empty. In
query mode, unknown paths, duplicate scalar keys, missing required values,
malformed encoding, and values that cannot be coerced to the schema's string,
boolean, integer, or number type fail directly.

Resources are read with
`read("<name>-mcp://resources/read?uri=<percent-encoded-resource-uri>", "")`;
prompts are obtained from `read("<name>-mcp://prompts/<name>?<arguments>",
"")`. Text is returned normally. MCP image, audio, and blob content is
preserved and returned as a `file://` address. Calls that remain active for
about 60 seconds become ordinary managed tasks; cancelling one closes that
session's MCP connection.

Connections belong to one Agent session and are created only by a
server-specific help read, a server protocol operation, Test, or Reconnect.
Every operation resolves the server's current configuration and Agent
Environment revision before reusing a connection. A change to either reconnects
the server; a removed, disabled,
invalid, or unavailable server returns its error without automatic retry or
fallback. Connection initialization times out after 30 seconds, and a stalled
server does not block initialization of other servers. Configuration and the
`:mcp` panel are documented in [Models and
configuration](configuration.md#mcp-servers) and [Terminal interface](interface.md#mcp-server-manager).

### `uri-agent-docs`

The Markdown files under `docs/` are embedded in the binary at build time, so
they remain readable from any startup working directory and match the running
URI Agent version. Start with the embedded documentation index:

```text
read("uri-agent-docs://README.md", "")
```

Other targets are the exact, case-sensitive filenames linked by that index,
such as `uri-agent-docs://protocols.md`. Read `uri-agent-docs://help` for the
complete filename list. Paths, query parameters, and execution are not
supported.

### `file`

Relative paths resolve from the canonical startup working directory; absolute
paths remain absolute. On Unix, `~` and paths beginning with `~/` resolve from
the current user's home directory; `~user` remains an ordinary relative path.
Reading a directory returns a sorted, bounded listing. PNG, JPEG, GIF, and WebP
files are detected from their signatures rather than their extensions and are
returned directly as model-visible images:

```text
read("file://screenshots/error.png", "")
```

Image reads require a model whose catalog input includes `image` and reject all
query parameters. Other regular files remain text reads and accept a one-based
line range:

```text
read("file://src/main.rs?offset=1&limit=200", "")
```

File content is returned without line numbers by default. Add `line_numbers=true` when one-based line prefixes are useful:

```text
read("file://src/main.rs?offset=1&limit=200&line_numbers=true", "")
```

Add `glob=<pattern>` to a directory target to list matching files recursively
with standard ignore rules. Results are sorted and paginated with the same
one-based `offset` and bounded `limit`. Query values use standard
percent-encoding:

```text
read("file://src?glob=**/*.rs&limit=200", "")
```

`file://help` reports image support, the accepted range and glob options,
active limits, and current working directory. Paginated file, directory, and
glob results omit a redundant remaining-count field and return the exact next
address. Empty directories return `No entries.` and empty globs return `No
matches.`; an empty file remains empty content.

### `grep`

`grep` puts the search text in the string body and searches a project-relative
or absolute file or directory. An empty target searches the startup working
directory. On Unix, the root accepts the same `~` and `~/` home-relative paths
as `file`.

Exact search remains the default. Its pattern is a regular expression unless
`literal=true`, and an invalid regular expression is retried as literal text.
This path invokes ripgrep directly without a shell and accepts `glob`,
`literal`, `ignore_case`, `context`, and `limit`:

```text
read("grep://src?glob=**/*.rs&limit=100", "ProtocolRequest")
read("grep://src/tui/app.rs", "fn push(")
read("grep://?literal=true&ignore_case=true", "exact text")
```

Semantic search uses a private sidecar index for one exact root and optional
glob. A ranked read creates or incrementally refreshes that cache itself:

```text
read("grep://src?mode=semantic&glob=**/*.rs", "credential refresh flow")
read("grep://src?mode=hybrid&glob=**/*.rs&limit=20", "会话检索")
read("grep://src?mode=status&glob=**/*.rs", "")
exec("grep://src?mode=index&glob=**/*.rs", "")
```

`semantic` ranks Model2Vec embeddings with zvec cosine search. `hybrid` fuses
that rank with Jieba BM25 using reciprocal-rank fusion. Use exact search for
known identifiers and literals, prefer hybrid for conceptual searches, and use
semantic when relevant results are likely to use different wording. Ranked
search defaults to 7 results and allows at most 50. It searches only after the
source catalog and cache agree, and retries a bounded number of times if files
change during indexing. Small operations return results directly; after about
60 seconds the same operation continues as a managed task whose completion
carries the search result. If the notification marks its output truncated,
follow its `tasks://` instruction once instead of rerunning the search. Do not
call `mode=status` or `mode=index` before a ranked search: status is diagnostic,
while index is optional prewarming or forced recovery and performs an atomic
full rebuild. Results remain in ranked order but omit backend scores because
cosine and hybrid fusion scores are not comparable. The scanner follows
standard ignore files, skips binary and non-UTF-8 data and files over 1 MiB,
and stores overlapping source fragments under stable file groups with
fragment-accurate line ranges. Source files are never changed.

URI Agent uses a working `rg` from `PATH` when available. Otherwise the linked
grep plugin silently installs its pinned platform archive after checking the
release checksum and executable version. Read `grep://help` for the exact
limits and output shape.

### `context`

The built-in `context` protocol is the recovery surface for hard context-window rollovers. It reports estimated remaining context, stores at most 20 active titled notes, exposes note revision anchors, and provides bounded window listings, history pages, search, and reads around an anchor. Conversation records use session-local IDs such as `r42`; user-history listing and search return the same IDs while excluding internal model-facing messages. Add and replace operations require a percent-encoded `title` query and put the complete note content in the string body. Note IDs are generated by the host, remain stable across replacement, and are never reused.

Current note titles and content share a model-relative budget. Growth above the hard limit fails; shrinking and deletion remain available. Deletion preserves the ID, latest title, revision metadata, anchors, and an `已删除` marker while making note content unreadable. Ordinary conversation records around those anchors remain available. Large live notes are read in character-based pages rather than given an individual storage limit.

History reads and search accept comma-separated `types` values from `user`,
`assistant`, `tool_call`, `tool_result`, and `error`; all types are included by
default. Exact search uses record-ID `before` pagination. History search spans
all windows when `window` is omitted and narrows to one window when supplied.
`mode=semantic` and `mode=hybrid` provide ranked search with `offset`
pagination; they automatically create or incrementally refresh the current
session's sidecar before searching. Record-type and window filters are applied
inside both vector and keyword retrieval before ranking. Results contain the
actual matching fragment in ranked order without exposing backend scores. A
long refresh continues as a managed task whose completion carries the search
result; if the notification is truncated, follow its `tasks://` instruction
once rather than rerunning the search. Do not read or execute
`context://history/index` before a ranked search: its read form is diagnostic,
while its `exec` form is optional prewarming or a forced full rebuild.

An anchor read accepts bounded `before` and `after` record counts. Note mutation
events are sidecar state and do not alter model replay or remove their tool
call/result pair from the active provider prefix. Recovery views omit every
`context://` or `sessions://` call and correlated result so deleted note bodies
cannot be reconstructed and searches do not invalidate their own index.

When `rollover` is the active strategy, `exec("context://rollover", "<optional handoff>")` requests a fresh model window. The runtime applies it only after all tool calls from that response have correlated durable results. The new hidden bootstrap requires another `context://help` read, followed by the note index, active notes, original user-statement history, and any needed prior-window or anchor-centered records. Notes, handoffs, and history are untrusted reference data. Read `context://help` for the exact routes, query fields, limits, and budget behavior.

### `sessions`

The `sessions` protocol discovers and searches saved URI Agent sessions while
keeping the SQLite archive read-only. Discovery is scoped to the current
project by default. Put discovery options in the URI query: `scope=all`
searches every project, an optional percent-encoded `cwd` narrows that
cross-project scope, and `limit` and `offset` bound the result page. The search
body is only the plain search text; exact search remains the default:

```text
read("sessions://recent", "")
read("sessions://search", "refresh token")
read("sessions://search?scope=all&limit=20", "billing migration")
```

Semantic search keeps separate sidecar indexes for the requested project,
all-session, or narrowed working-directory scope. The default remains the
current project. A ranked read automatically creates or incrementally refreshes
its selected cache; explicit indexing is optional prewarming or forced repair:

```text
read("sessions://search?mode=semantic&scope=all&limit=20", "credential renewal")
read("sessions://search?mode=hybrid&types=user,assistant", "会话迁移")
read("sessions://index?scope=all", "")
exec("sessions://index?scope=all", "")
```

Record-type filtering runs inside vector and keyword retrieval before ranking,
and scope is fixed by the selected index rather than post-filtering a global
top-k result. Only newly appended searchable records are loaded and embedded
during a normal refresh; warm search does not load every archived transcript.
Small operations return directly, while a long refresh continues as a managed
task whose completion carries the search result. If the notification is
truncated, follow its `tasks://` instruction once rather than rerunning the
search. Do not read or execute `sessions://index` before a ranked search; those
operations are diagnostic and optional forced rebuilding respectively. Results
remain in ranked order but omit backend scores. Index creation reads only the
same conversation-record projection exposed by the protocol; it neither
resumes nor changes a session.

Search results identify matching conversation records with the same
session-local `r<number>` anchors used by `context`. Read an exact session ID to
retrieve its newest records, or append `/around/<record-id>` to inspect a
bounded neighborhood. Put `types`, `limit`, and the returned `before` record-ID
cursor in the URI query; session reads use an empty body:

```text
read("sessions://<session-id>", "")
read("sessions://<session-id>?types=user,assistant&limit=20", "")
read("sessions://<session-id>/around/r42?before=8&after=4", "")
```

All record types are returned by default. `types` uses the same values and
semantics as `context`; the legacy `include_tools=false` selects user,
assistant, and error records, while `include_tools=true` selects every type.
It cannot be combined with `types`. Thinking, usage, model replay payloads,
compaction summaries, TUI metadata, model and provider names, message counts,
and per-record timestamps are excluded. Discovery returns the session ID,
title, and working directory only when needed to distinguish cross-project
results. Records retain type, anchor, context-window ID, failure state, and
text. Results are bounded and marked as untrusted reference data. Archive
access opens the SQLite database read-only and does not initialize, migrate,
resume, append, rename, or delete sessions. Read `sessions://help` for the exact
query parameters and limits.

All `context://` and `sessions://` calls and their correlated results remain
excluded even when every record type is selected. A deleted note body cannot
be recovered through the session archive protocol, and semantic search cannot
make its own indexed corpus stale.

### `https`

The read-only `https` protocol searches and extracts pages through Parallel,
Exa, or TinyFish when that provider is logged in. Page reads use the first
configured provider in stable order (Parallel, then Exa, then TinyFish) and try
the next configured provider after an API failure:

```text
read("https://www.rust-lang.org/", "")
```

Its reserved search route accepts the nonempty search query as a string body.
The URI accepts an optional result limit from 1 through 20, an optional
provider (`parallel`, `exa`, or `tinyfish`), and provider-specific options:

```text
read("https://search?limit=10&provider=parallel", "stable Rust release notes")
```

Without `provider`, search tries configured providers in stable order —
Parallel, then Exa, then TinyFish — and falls back after a provider failure. An
explicit provider is used without fallback. A key saved by selecting a provider
in `:login`, or supplied through `PARALLEL_API_KEY`, `EXA_API_KEY`, or
`TINYFISH_API_KEY`, makes that provider available for both search and
extraction.

`https://help` stays concise and shows common options for the first logged-in
provider. `https://help/parallel`, `https://help/exa`, and
`https://help/tinyfish` own each provider's supported model-facing search
options. When no provider is configured, help directs the model to ask the user
to run `:login` without requesting a key in the conversation. Page reads then remain available through direct local
HTTPS fetching: HTML is cleaned and converted to Markdown, JSON is
pretty-printed, and other textual resources are returned as text. Local reads
do not execute JavaScript or extract PDFs. Redirects must remain on HTTPS,
requests time out after 30 seconds, and response bodies are limited to 5 MiB.
Search and page results are explicitly framed as untrusted web content. Search
output omits the echoed query, provider, and provider request ID; the request ID
and selected provider remain available in the per-session diagnostic log.

### `replace`

Prefer the typed `replace` tool. It performs an exact replacement and returns
after the write succeeds without requiring JSON-in-string serialization:

```text
replace({"path":"src/config.rs","old_text":"one unique match","new_text":"replacement"})
```

`old_text` must be nonempty and occur exactly once. Missing and ambiguous
matches fail directly without changing the file. A successful write atomically
replaces the destination file. `replace` is registered only as a direct tool;
it has no protocol route. Relative and absolute paths follow `file` semantics,
including `~` and `~/` home expansion on Unix. Its active tool schema is the
exact model-facing argument contract.

### `apply_patch`

The typed `apply_patch` tool accepts one complete patch string and returns the
final summary after applying it:

```text
apply_patch({"patch":"*** Begin Patch\n...\n*** End Patch"})
```

The patch supports this grammar:

```text
*** Begin Patch
*** Add File: <path>
+<new content>
*** Update File: <path>
*** Move to: <new path>
@@ <optional landmark>
-<old line>
+<new line>
*** Delete File: <path>
*** End Patch
```

`*** Move to` is optional and may appear only immediately after an Update File
header. Update lines start with a space for context, `-` for removal, or `+` for
addition. `*** End of File` may anchor a chunk at EOF. Every Add File content
line starts with `+`. Relative paths resolve from the startup working directory;
absolute paths are accepted. On Unix, `~` and paths beginning with `~/` resolve
from the current user's home directory, while `~user` is not expanded.

URI Agent parses and applies the complete patch to an in-memory plan before
changing files. A commit failure rolls back every affected file.

The summary lists one line per file: `A <path> (+n)` for an addition, `M
<path> (+a/-b)` for a modification, `D <path> (-n)` for a deletion, and
`= <path> (unchanged)` for an update that matched but made no net change. A
patch with no net changes returns `Patch made no changes:` instead of `Applied
patch:`. Matching is line-exact apart from trailing whitespace, so CRLF and LF
files both match, and original line endings and BOM are preserved.

`apply_patch` is registered only as a direct tool; it has no protocol route.
The same grammar is included in its active tool schema.

### `bash` and `pwsh`

Shell bodies must be command strings containing at least one non-whitespace
character:

```text
exec("bash://run", "cargo test")
exec("pwsh://run", "cargo test")
```

Commands run from the startup working directory. Bash starts without profile or rc files; PowerShell starts without a profile and reads the script from standard input.

Bash exports `COLUMNS=4096` so width-aware tools (ps, git, docker, kubectl) do
not fall back to 80-column truncation when stdout is not a terminal.

URI Agent injects the latest values from its global Agent environment manager into every new shell command. Managed values override inherited process variables with the same name. The user-controlled `:terminal` is separate and does not receive them; see [Agent environment](configuration.md#agent-environment).

PowerShell source and plain-text output use UTF-8. Command status follows the final PowerShell or native command, including the native command's exact exit code.

Successful stdout-only output is returned directly. A stderr-only result is
prefixed by `stderr:`, and both labels are retained when both streams exist.
Silent success returns `(no output)`. Nonzero exits and timeouts retain their
terminal state and any observed output; the generic `Error:` marker remains so
all model providers distinguish failures from successful text.

## Managed tasks

Protocol execution returns its final result directly by default. Ranked searches and shell commands therefore start in the foreground. If an operation is still running after about 60 seconds, URI Agent continues that same operation as a managed background task without restarting it:

```text
exec("bash://run", "cargo test")
→ <stdout>

# If it remains active past the foreground window:
→ Background task started: tasks://<id>
```

Use `background=true` when the command should become a task immediately:

```text
exec("bash://run?background=true", "cargo test")
```

Foreground and background commands share one execution deadline. `timeout` is an integer number of seconds, omission defaults to 1,800 seconds (30 minutes), and `timeout=0` disables the deadline:

```text
exec("bash://run?timeout=120", "cargo test")
```

The deadline is unchanged when an operation moves to the background. Timing out produces a failed task for background work or a direct `exec` error for foreground work, and terminates the process tree. Interrupting a shell tool call before automatic background conversion cancels its process rather than leaving detached work behind. When the root shell exits, URI Agent drains ready output briefly and terminates descendants that remain in the execution boundary instead of letting inherited output handles keep them alive. Shell help tells the model not to create another background layer inside the command.

The shared `tasks` protocol covers work from every protocol:

```text
read("tasks://summary", "")
read("tasks://<id>", "")
read("tasks://<id>?wait=30", "")
exec("tasks://<id>/cancel", "")
```

Task acceptance is not success. A model-facing task record exposes `pending`,
`running`, `completed`, `failed`, or `cancelled` state, its originating
protocol, label, bounded latest output while active, and complete terminal
output. A read without `wait` returns immediately. `wait` accepts an integer
number of seconds, clamped to the range 1 through 300. If the task finishes
during the wait, the read returns its complete terminal output. If the wait
expires, it returns current status and bounded latest output while the task
keeps running. Internal
timestamps and duration do not enter the model result. Task IDs increase within
their session as lowercase hexadecimal values: they start at `001`, remain at
least three digits wide, expand after `fff`, and continue after the highest
restored ID. Completed, failed, and cancelled reports remain available when
their session is resumed, including after an application restart. A task
process itself never resumes; work interrupted by process exit is restored as
cancelled. At most 16 background tasks may be pending or running at once; an
explicit background request fails at capacity, while automatic conversion
keeps waiting in the foreground.

When a background task reaches `completed`, `failed`, or `cancelled`, URI Agent
sends the model an automatic hidden plain-text notification containing the
`tasks://` URI, status, and at most the latest 20 lines and 4,000 characters of
output. A complete-record read instruction appears only when that output was
truncated. The notification continues the active turn at its next model
boundary or starts a model turn when idle. When the result is needed before
continuing, the model may use one bounded wait instead of polling or rerunning
the operation. Task output is identified as untrusted data. Reading an
individual terminal task or a summary that already presents it suppresses the
duplicate notification. Notifications are delivered in batches of at most 10
and approximately 16,000 output characters.

Process shutdown cancels and joins active managed tasks. Shell cancellation and timeout terminate the spawned process tree and reap the root process before the task reaches its terminal state, rather than only dropping the Rust future that waits for it.

## Complete output preservation

When a successful tool result or formatted failure exceeds the active inline
limit, URI Agent:

1. stores the complete bytes under the platform cache directory at `uri-agent/outputs/<session-id>/`;
2. returns a head-and-tail preview;
3. includes a readable `file://` address for the complete output.

This presentation behavior is shared by protocol reads, executions, dynamic
WASM tools, and failures. Adjust the limit through `:settings`, configuration,
`URI_AGENT_OUTPUT_LIMIT`, or `--output-limit`; see [Models and
configuration](configuration.md).

Each session also has `diagnostics.jsonl` in this output directory. The log
contains tool lifecycle timing, call IDs, tool names, argument field names and
sizes, result sizes and state, plus selected internal provider identifiers. It
does not copy raw arguments, credentials, environment values, or successful
tool output. The `:status` panel shows its path.

## Extensions

Capabilities may register protocols or typed direct tools through linked Rust
extensions and trusted runtime-loaded WASM modules; discovered Skills register
read-only protocols. Prefer a protocol for operations with a simple string
input and a direct tool when typed or escape-heavy arguments would otherwise
need nested serialization. See [WASM plugins](plugins.md) for installation,
reload, ABI, permissions, and SDK usage; [Startup context and
Skills](context.md#skills) for Skill behavior; and the [development
guide](development.md#linked-rust-extensions) for linked first-party internals.
