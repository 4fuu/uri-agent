# Protocols, tasks, and output

URI Agent keeps the model-facing tool surface fixed while allowing the application to register new capabilities. This document describes routing, built-in protocols, execution semantics, managed tasks, and output preservation. Skill discovery and resources are documented in [Startup context and Skills](context.md).

For the exact runtime syntax of a protocol, read `<protocol>://help`. Those help routes are the canonical model-facing operation reference.

## Fixed model interface

The model receives exactly two tool definitions:

```text
read(uri: string, body: BodyEnvelope)
exec(uri: string, body: BodyEnvelope)
```

`BodyEnvelope` is always present and has the concrete shape
`{"kind":"none|text|json","value":"..."}`. Use `none` with an empty value when
the protocol takes no body, `text` for a literal string body, and `json` with
the complete JSON serialization of any JSON body. URI Agent decodes this
model-facing envelope before protocol dispatch, so protocols and plugins still
receive an optional arbitrary JSON value.

`read` is used for resources, help, task snapshots, and completed output. `exec` starts work through protocols that support execution. A protocol may implement `read`, `exec`, or both.

Model dispatch and the [protocol registry](../src/protocol.rs) apply four routing rules:

1. Decode the required model-facing body envelope before entering the registry.
2. Split an address only at the first `://` and use the part before that
   delimiter as the registered protocol name.
3. Pass the entire remainder to the protocol as an opaque target. The registry
   does not URL-decode, normalize, or parse options from it.
4. Pass the decoded optional JSON body to the selected protocol unchanged.

For example, the target received by `capture` here is exactly `a://b?not=a url`:

```text
read("capture://a://b?not=a url", {"kind":"none","value":""})
```

Protocols own their target syntax. `file` interprets `?offset`, `?limit`, and
`?line_numbers`; `bash` and `pwsh` interpret `?background=true` and
`?timeout=<seconds>`; the registry treats all of those characters as opaque.

Protocol names must be unique. Registration fails rather than silently replacing an existing protocol.

## Built-in protocols

| Protocol | Operations | Responsibility |
| --- | --- | --- |
| `uri-agent-docs` | `read` | Read version-matched URI Agent documentation embedded in the binary |
| `file` | `read` | Read files and bounded directory listings |
| `sessions` | `read` | Discover, search, and read bounded saved session history without changing it |
| `https` | `read` | Search through a logged-in web provider and read HTTPS pages as text |
| `replace` | `read`, `exec` | Atomically replace one exact text match |
| `apply_patch` | `read`, `exec` | Apply Codex-style add, delete, update, and move patches |
| `tasks` | `read`, `exec` | Inspect and cancel background tasks from every protocol |
| `bash` | `read`, `exec` | Run Bash commands in the foreground or as managed background tasks when Bash is enabled |
| `pwsh` | `read`, `exec` | Run PowerShell 7 commands in the foreground or as managed background tasks when `pwsh` is enabled |
| `wasm_plugin` | `read`, `exec` | Reload trusted WASM protocols and return the completed reload report |
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
read("uri-agent-docs://README.md", {"kind":"none","value":""})
```

Other targets are the exact, case-sensitive filenames linked by that index,
such as `uri-agent-docs://protocols.md`. Read `uri-agent-docs://help` for the
complete filename list. Paths, query parameters, and execution are not
supported.

### `file`

Relative paths resolve from the canonical startup working directory; absolute paths remain absolute. Reading a directory returns a sorted, bounded listing. Text reads accept a one-based line range:

```text
read("file://src/main.rs?offset=1&limit=200", {"kind":"none","value":""})
```

File content is returned without line numbers by default. Add `line_numbers=true` when one-based line prefixes are useful:

```text
read("file://src/main.rs?offset=1&limit=200&line_numbers=true", {"kind":"none","value":""})
```

`file://help` reports the accepted range options, active limits, and current working directory.

### `sessions`

The read-only `sessions` protocol discovers and searches saved URI Agent sessions. Discovery is scoped to the current project by default; `scope: "all"` searches every project, and an optional `cwd` narrows that cross-project scope:

```text
read("sessions://recent", {"kind":"none","value":""})
read("sessions://search", {"kind":"json","value":"{\"query\":\"refresh token\"}"})
read("sessions://search", {"kind":"json","value":"{\"query\":\"billing migration\",\"scope\":\"all\"}"})
```

Read an exact session ID to retrieve its newest visible records. Use the returned `before` cursor to page backward, and request `include_tools` only when tool evidence is needed:

```text
read("sessions://<session-id>", {"kind":"none","value":""})
read("sessions://<session-id>", {"kind":"json","value":"{\"include_tools\":true,\"limit\":20}"})
```

User, assistant, and terminal error text is returned by default. Thinking, usage, model replay payloads, compaction summaries, and TUI metadata are always excluded; tool calls and results are excluded unless requested. Results are bounded and marked as untrusted reference data. Archive access opens the SQLite database read-only and does not initialize, migrate, resume, append, rename, or delete sessions. Read `sessions://help` for the body fields and limits.

### `https`

The read-only `https` protocol searches and extracts pages through Parallel or
Exa when that provider is logged in. Page reads use the first configured
provider in stable order (Parallel, then Exa) and try the next configured
provider after an API failure:

```text
read("https://www.rust-lang.org/", {"kind":"none","value":""})
```

Its reserved search route accepts the nonempty search query as a string body.
The URI accepts an optional result limit from 1 through 20, an optional
provider (`parallel` or `exa`), and provider-specific options:

```text
read("https://search?limit=10&provider=parallel", {"kind":"text","value":"stable Rust release notes"})
```

Without `provider`, search tries configured providers in stable order:
Parallel, then Exa, and falls back after a provider failure. An explicit
provider is used without fallback. A key saved by selecting either provider in
`:login`, or supplied through `PARALLEL_API_KEY` or `EXA_API_KEY`, makes that
provider available for both search and extraction.

`https://help` stays concise and shows common options for the first logged-in
provider. `https://help/parallel` and `https://help/exa` own each provider's
supported model-facing search options. When neither provider is configured,
help directs the model to ask the user to run `:login` without requesting a key
in the conversation. Page reads then remain available through direct local
HTTPS fetching: HTML is cleaned and converted to Markdown, JSON is
pretty-printed, and other textual resources are returned as text. Local reads
do not execute JavaScript or extract PDFs. Redirects must remain on HTTPS,
requests time out after 30 seconds, and response bodies are limited to 5 MiB.

### `replace`

`replace` performs an exact replacement and returns after the write succeeds:

```text
exec(
  "replace://src/config.rs",
  {"kind":"json","value":"{\"old_text\":\"one unique match\",\"new_text\":\"replacement\"}"}
)
```

`old_text` must be nonempty and occur exactly once. Missing and ambiguous matches fail directly from `exec` without changing the file. A successful write atomically replaces the destination file.

### `apply_patch`

`apply_patch` accepts a patch string and returns the final summary after applying it:

```text
exec("apply_patch://apply", {"kind":"text","value":"*** Begin Patch\n...\n*** End Patch"})
```

It supports adding, deleting, updating, and moving files. Writes are atomic per file and operations run in patch order, but the whole patch is not transactional: failure in a later operation does not undo earlier successful operations. Read `apply_patch://help` for the complete file-operation and hunk grammar.

### `bash` and `pwsh`

Shell bodies must be command strings:

```text
exec("bash://run", {"kind":"text","value":"cargo test"})
exec("pwsh://run", {"kind":"text","value":"cargo test"})
```

Commands run from the startup working directory. Bash starts without profile or rc files; PowerShell starts without a profile and reads the script from standard input.

URI Agent injects the latest values from its global Agent environment manager into every new shell command. Managed values override inherited process variables with the same name. The user-controlled `:terminal` is separate and does not receive them; see [Agent environment](configuration.md#agent-environment).

PowerShell source and plain-text output use UTF-8. Command status follows the final PowerShell or native command, including the native command's exact exit code.

## Managed tasks

Protocol execution returns its final result directly by default. Bash and PowerShell commands therefore start in the foreground. If a command is still running after about 60 seconds, URI Agent converts that same process into a managed background task without restarting it:

```text
exec("bash://run", {"kind":"text","value":"cargo test"})
→ Exit: exit status: 0
  ...

# If it remains active past the foreground window:
→ Background task accepted: tasks://<id>
```

Use `background=true` when the command should become a task immediately:

```text
exec("bash://run?background=true", {"kind":"text","value":"cargo test"})
```

Foreground and background commands share one execution deadline. `timeout` is an integer number of seconds, omission defaults to 1,800 seconds (30 minutes), and `timeout=0` disables the deadline:

```text
exec("bash://run?timeout=120", {"kind":"text","value":"cargo test"})
```

The deadline is unchanged when an operation moves to the background. Timing out produces a failed task for background work or a direct `exec` error for foreground work, and terminates the process tree. Interrupting a shell tool call before automatic background conversion cancels its process rather than leaving detached work behind. Shell help tells the model not to create another background layer inside the command.

The shared `tasks` protocol covers work from every protocol:

```text
read("tasks://summary", {"kind":"none","value":""})
read("tasks://<id>", {"kind":"none","value":""})
exec("tasks://<id>/cancel", {"kind":"none","value":""})
```

Task acceptance is not success. A task record exposes `pending`, `running`, `completed`, `failed`, or `cancelled` state, its originating protocol, label, duration, bounded latest output while active, and complete terminal output. Task IDs increase within their in-process manager as lowercase hexadecimal values: they start at `001`, remain at least three digits wide, and expand after `fff`. Settled background records remain available for the lifetime of the session runtime. At most 16 background tasks may be pending or running at once; an explicit background request fails at capacity, while automatic conversion keeps waiting in the foreground.

When a background task reaches `completed`, `failed`, or `cancelled`, URI Agent sends the model an automatic hidden notification containing the `tasks://` URI, status, and at most the latest 20 lines and 4,000 characters of output. The notification continues the active turn at its next model boundary or starts a model turn when idle, so the model must not poll. Task output is identified as untrusted data. Reading an individual terminal task or a summary that already presents it suppresses the duplicate notification. Notifications are delivered in batches of at most 10 and approximately 16,000 output characters.

Process shutdown cancels and joins active managed tasks. Shell cancellation and timeout terminate the spawned process tree, not only the Rust future that waits for it.

## Complete output preservation

When protocol output exceeds the active inline limit, URI Agent:

1. stores the complete bytes under the platform cache directory at `uri-agent/outputs/<session-id>/`;
2. returns a head-and-tail preview;
3. includes a readable `file://` address for the complete output.

This presentation behavior is shared by protocol reads and executions. Adjust the limit through `:settings`, configuration, `URI_AGENT_OUTPUT_LIMIT`, or `--output-limit`; see [Models and configuration](configuration.md).

## Extension protocols

Capabilities may register protocols through linked Rust extensions, trusted runtime-loaded WASM modules, or discovered Skills. All remain behind `read` and `exec`. See [WASM plugins](plugins.md) for installation, reload, ABI, permissions, and SDK usage; [Startup context and Skills](context.md#skills) for Skill behavior; and the [development guide](development.md#linked-rust-extensions) for linked first-party internals.
