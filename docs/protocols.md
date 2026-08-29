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
call `read("<protocol>://help", "")`. The runtime blocks calls that skip this
first help read and returns the exact help address required. Each protocol is
tracked independently.

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
| `grep` | `read` | Search file contents with bounded ripgrep results and optional glob filtering |
| `sessions` | `read` | Discover, search, and read bounded saved session history without changing it |
| `https` | `read` | Search through a logged-in web provider and read HTTPS pages as text |
| `tasks` | `read`, `exec` | Inspect and cancel background tasks from every protocol |
| `bash` | `read`, `exec` | Run Bash commands in the foreground or as managed background tasks when Bash is enabled |
| `pwsh` | `read`, `exec` | Run PowerShell 7 commands in the foreground or as managed background tasks when `pwsh` is enabled |
| `wasm_plugin` | `read`, `exec` | Reload trusted WASM protocols and direct tools and return the completed reload report |
| `<name>-skill` | `read` | Load one [discovered Skill](context.md#skills) and its bundled files |

Shell plugins detect their own executables at startup. On Windows, the `pwsh`
plugin also verifies that PowerShell 7 or newer can start. A valid `pwsh`
plugin suppresses `bash`; otherwise `pwsh` remains disabled, a startup warning
is shown, and `bash` remains available when installed. On non-Windows
platforms, only the `bash` plugin is considered; `pwsh` is not started.

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

`grep` puts the search pattern in the string body and searches a project-relative
or absolute file or directory. An empty target searches the startup working
directory. On Unix, the root accepts the same `~` and `~/` home-relative paths
as `file`. Search patterns are regular expressions by default. If a pattern is
not valid as a regular expression, the protocol retries it as literal text;
`literal=true` always uses literal matching. It invokes ripgrep directly
without a shell and accepts `glob`, `literal`, `ignore_case`, `context`, and
`limit` options:

```text
read("grep://src?glob=**/*.rs&limit=100", "ProtocolRequest")
read("grep://src/tui/app.rs", "fn push(")
read("grep://?literal=true&ignore_case=true", "exact text")
```

URI Agent uses a working `rg` from `PATH` when available. Otherwise the linked
grep plugin silently installs its pinned platform archive after checking the
release checksum and executable version. Read `grep://help` for the exact
limits and output shape.

### `sessions`

The read-only `sessions` protocol discovers and searches saved URI Agent
sessions. Discovery is scoped to the current project by default. Put discovery
options in the URI query: `scope=all` searches every project, an optional
percent-encoded `cwd` narrows that cross-project scope, and `limit` and `offset`
bound the result page. The search body is only the plain search text:

```text
read("sessions://recent", "")
read("sessions://search", "refresh token")
read("sessions://search?scope=all&limit=20", "billing migration")
```

Read an exact session ID to retrieve its newest visible records. Put
`include_tools`, `limit`, and the returned `before` pagination cursor in the URI
query; session reads use an empty body:

```text
read("sessions://<session-id>", "")
read("sessions://<session-id>?include_tools=true&limit=20", "")
```

User, assistant, and terminal error text is returned by default. Thinking,
usage, model replay payloads, compaction summaries, TUI metadata, model and
provider names, message counts, and per-record timestamps are excluded; tool
calls and results are excluded unless requested. Discovery returns the session
ID, title, and working directory only when needed to distinguish cross-project
results. Records retain role, sequence cursor, failure state, and text. Results
are bounded and marked as untrusted reference data. Archive access opens the
SQLite database read-only and does not initialize, migrate, resume, append,
rename, or delete sessions. Read `sessions://help` for the exact query
parameters and limits.

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
changing files. A commit failure rolls back every affected file. `apply_patch`
is registered only as a direct tool; it has no protocol route. The same grammar
is included in its active tool schema.

### `bash` and `pwsh`

Shell bodies must be command strings containing at least one non-whitespace
character:

```text
exec("bash://run", "cargo test")
exec("pwsh://run", "cargo test")
```

Commands run from the startup working directory. Bash starts without profile or rc files; PowerShell starts without a profile and reads the script from standard input.

URI Agent injects the latest values from its global Agent environment manager into every new shell command. Managed values override inherited process variables with the same name. The user-controlled `:terminal` is separate and does not receive them; see [Agent environment](configuration.md#agent-environment).

PowerShell source and plain-text output use UTF-8. Command status follows the final PowerShell or native command, including the native command's exact exit code.

Successful stdout-only output is returned directly. A stderr-only result is
prefixed by `stderr:`, and both labels are retained when both streams exist.
Silent success returns `(no output)`. Nonzero exits and timeouts retain their
terminal state and any observed output; the generic `Error:` marker remains so
all model providers distinguish failures from successful text.

## Managed tasks

Protocol execution returns its final result directly by default. Bash and PowerShell commands therefore start in the foreground. If a command is still running after about 60 seconds, URI Agent converts that same process into a managed background task without restarting it:

```text
exec("bash://run", "cargo test")
→ <stdout>

# If it remains active past the foreground window:
→ Background task accepted: tasks://<id>
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
exec("tasks://<id>/cancel", "")
```

Task acceptance is not success. A model-facing task record exposes `pending`,
`running`, `completed`, `failed`, or `cancelled` state, its originating
protocol, label, bounded latest output while active, and complete terminal
output. Internal timestamps and duration do not enter the model result. Task
IDs increase within their session as lowercase hexadecimal values: they start
at `001`, remain at least three digits wide, expand after `fff`, and continue
after the highest restored ID. Completed, failed, and cancelled reports remain
available when their session is resumed, including after an application
restart. A task process itself never resumes; work interrupted by process exit
is restored as cancelled. At most 16 background tasks may be pending or
running at once; an explicit background request fails at capacity, while
automatic conversion keeps waiting in the foreground.

When a background task reaches `completed`, `failed`, or `cancelled`, URI Agent
sends the model an automatic hidden plain-text notification containing the
`tasks://` URI, status, and at most the latest 20 lines and 4,000 characters of
output. A complete-record read instruction appears only when that output was
truncated. The notification continues the active turn at its next model
boundary or starts a model turn when idle, so the model must not poll. Task
output is identified as untrusted data. Reading an individual terminal task or
a summary that already presents it suppresses the duplicate notification.
Notifications are delivered in batches of at most 10 and approximately 16,000
output characters.

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
