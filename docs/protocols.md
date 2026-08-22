# Protocols, tasks, Skills, and extensions

URI Agent keeps the model-facing tool surface fixed while allowing the application to register new capabilities. This document describes that boundary, the built-in protocols, asynchronous execution, Skill loading, and Rust extension registration.

For the exact runtime syntax of a protocol, read `<protocol>://help`. Those help routes are the canonical model-facing operation reference.

## Fixed model interface

The model receives exactly two tool definitions:

```text
read(uri: string, body?: any)
exec(uri: string, body?: any)
```

`read` is used for resources, help, task snapshots, and completed output. `exec` starts work through protocols that support execution. A protocol may implement `read`, `exec`, or both.

The [protocol registry](../src/protocol.rs) applies four routing rules:

1. Split an address only at the first `://`.
2. Use the part before that delimiter as the registered protocol name.
3. Pass the entire remainder to the protocol as an opaque target. The registry does not URL-decode, normalize, or parse options from it.
4. Accept any JSON value as the optional `body` and pass it to the selected protocol unchanged.

For example, the target received by `capture` here is exactly `a://b?not=a url`:

```text
read("capture://a://b?not=a url")
```

Protocols own their target syntax. `file` interprets `?offset`, `?limit`, and
`?line_numbers`; `bash` and `pwsh` interpret `?wait`; the registry treats all
of those characters as opaque.

Protocol names must be unique. Registration fails rather than silently replacing an existing protocol.

## Built-in protocols

| Protocol | Operations | Responsibility |
| --- | --- | --- |
| `file` | `read` | Read files and bounded directory listings |
| `replace` | `read`, `exec` | Atomically replace one exact text match |
| `apply_patch` | `read`, `exec` | Apply Codex-style add, delete, update, and move patches |
| `bash` | `read`, `exec` | Run Bash commands as managed tasks when Bash is enabled |
| `pwsh` | `read`, `exec` | Run PowerShell 7 commands as managed tasks when `pwsh` is enabled |
| `<name>-skill` | `read` | Load one discovered Skill and its bundled files |

Shell plugins detect their own executables at startup. On Windows, the `pwsh`
plugin also verifies that PowerShell 7 or newer can start. A valid `pwsh`
plugin suppresses `bash`; otherwise `pwsh` remains disabled, a startup warning
is shown, and `bash` remains available when installed. On non-Windows
platforms, only the `bash` plugin is considered; `pwsh` is not started.

### `file`

Relative paths resolve from the canonical startup working directory; absolute paths remain absolute. Reading a directory returns a sorted, bounded listing. Text reads accept a one-based line range:

```text
read("file://src/main.rs?offset=1&limit=200")
```

File content is returned without line numbers by default. Add
`line_numbers=true` when one-based line prefixes are useful:

```text
read("file://src/main.rs?offset=1&limit=200&line_numbers=true")
```

The default limit is 200 lines and the protocol clamps requested limits to
2,000 lines. `file://help` also reports the current working directory, using a
normal display path rather than a Windows verbatim-path prefix. Read
`file://help` for the current contract.

### `replace`

`replace` starts an asynchronous exact replacement:

```text
exec(
  "replace://src/config.rs",
  {"old_text":"one unique match","new_text":"replacement"}
)
```

`old_text` must be nonempty and occur exactly once. Missing and ambiguous matches fail without changing the file. A successful write atomically replaces the destination file.

### `apply_patch`

`apply_patch` accepts a patch string and starts an asynchronous multi-file operation:

```text
exec("apply_patch://apply", "*** Begin Patch\n...\n*** End Patch")
```

It supports adding, deleting, updating, and moving files. Writes are atomic per file and operations run in patch order, but the whole patch is not transactional: failure in a later operation does not undo earlier successful operations. Read `apply_patch://help` for the complete file-operation and hunk grammar.

### `bash` and `pwsh`

Shell bodies must be command strings:

```text
exec("bash://run", "cargo test")
exec("pwsh://run", "cargo test")
```

Commands run from the startup working directory. Bash starts without profile or rc files; PowerShell starts without a profile and reads the script from standard input.

PowerShell source and plain-text output use UTF-8. Its task status follows the final PowerShell or native command, including the native command's exact exit code.

## Built-in project instructions

A prompt-only built-in plugin reads `AGENTS.md` from the canonical project directory when the file exists. It appends the file's content to the bottom of a new session's system prompt in this form:

```text
<project_rule_md>
The following content is from the project's AGENTS.md. Follow these instructions.

<AGENTS.md content>
</project_rule_md>
```

This plugin does not register a protocol, command, panel, or status provider. A missing `AGENTS.md` contributes no prompt content; other read failures stop session startup with an error. Because the complete prompt is frozen, later changes to `AGENTS.md` apply only to new sessions.

## Built-in binary hints

A second prompt-only built-in plugin scans `PATH` once while constructing a new session's frozen system prompt. It detects these names in fixed display order:

```text
rg, fd, fdfind, sd, bat, batcat, eza, exa, lsd, delta,
jq, yq, fzf, xh, hyperfine, dust, duf, procs, btm, zoxide,
doggo, gping, hexyl, choose, sad, ast-grep, broot, tokei, watchexec, glow
```

When at least one is available, the plugin contributes this exact prompt fragment, with the detected names joined by ` / `:

```text
These faster cross-platform tools are available: `rg` / `fd` / `bat`. Prefer them over their classical Unix equivalents.
```

Matching is case-insensitive and output follows the fixed list rather than `PATH` order. Duplicate names are removed, while aliases such as `fd` and `fdfind` remain separate when both exist. Missing and unreadable directories are ignored. On Unix, a match must resolve to a regular file with at least one execute bit; on Windows, its final extension must appear in `PATHEXT`, whose default is `.COM;.EXE;.BAT;.CMD` when unset.

No match contributes no prompt content. The plugin never invokes a detected program and registers no protocol, command, panel, status provider, key binding, or configuration. Changes to installed binaries or `PATH` take effect only in a new session; resumed sessions reuse their frozen prompt.

## Managed tasks

Execution is asynchronous by default. An accepted request normally returns before the operation is complete:

```text
exec("bash://run", "cargo test")
→ Task accepted: <id>
→ Read status: bash://tasks/<id>
```

Acceptance is not success. Read the returned route to observe `pending`, `running`, `completed`, `failed`, or `cancelled` status and the eventual content. Protocols expose task lists and individual tasks through their own read routes; the shared task manager does not create a generic model-facing task protocol.

Shell protocols offer a bounded wait when the immediate result is useful:

```text
exec("bash://?wait=30", "cargo test")
```

`N` is an integer number of seconds from 0 through 300. If the wait window expires, the task keeps running and the response still includes its task URI. `?wait=N` belongs to `bash` and `pwsh`; it is not a registry option and must not become one.

Cancellation must terminate the spawned process, not only the Rust future that waits for it.

## Complete output preservation

The inline output limit defaults to 32 KiB and cannot be set below 1,024 bytes. When protocol output exceeds the active limit, URI Agent:

1. stores the complete bytes under the platform cache directory at `uri-agent/outputs/<session-id>/`;
2. returns a head-and-tail preview;
3. includes a readable `file://` address for the complete output.

This presentation behavior is shared by protocol reads and executions. Adjust the limit through `:settings`, configuration, `URI_AGENT_OUTPUT_LIMIT`, or `--output-limit`; see [Models and configuration](configuration.md).

## Skills

URI Agent discovers Skills once when it starts, in this priority order:

```text
<project>/.agents/skills
<project>/.claude/skills
<project>/.codex/skills
~/.agents/skills
~/.claude/skills
~/.codex/skills
```

Each root may contain a `SKILL.md` directly or in one of its immediate child directories. Deeper recursive discovery is not performed.

A Skill starts with YAML frontmatter containing nonempty `name` and `description` values:

```yaml
---
name: Code Review
description: Review a change for correctness and regressions.
---
```

The name is lowercased, runs of characters outside ASCII letters and numbers become separators, and `-skill` is appended if absent. The example therefore registers:

```text
code-review-skill://help
code-review-skill://scripts/check.py
```

The first Skill for a normalized protocol name wins. Later duplicates are skipped with a notice. A Skill that collides with an already registered protocol is also skipped with a notice.

`<name>-skill://help` reads the Skill's `SKILL.md`; other targets read files relative to its directory. Absolute resource targets are rejected. Canonical path checks, including checks after following symlinks, keep resource reads inside the Skill directory.

### Frozen session behavior

When a session is created, URI Agent freezes:

- the complete generated system prompt;
- each selected Skill's name and description;
- each selected Skill's canonical `SKILL.md` path.

Resume reuses this snapshot instead of rediscovering current context. A same-named Skill elsewhere cannot replace the frozen one. Help and resources are still read from the frozen path, so removing that file produces an explicit error. A historical session without frozen context is invalid rather than being reinterpreted with current startup state.

## WASM plugins

URI Agent loads trusted [Extism](https://extism.org/) modules from
`<config>/wasm-plugins/`. The built-in `wasm_plugin` protocol exposes exactly two
operations:

```text
read("wasm_plugin://help")
exec("wasm_plugin://reload")
```

There are no install, update, remove, or list operations, no `--wasm-plugin`
flag, no `wasmPlugins` setting, and no package manifest. `wasm_plugin://help`
publishes the actual persistent directory and the current build/install
contract. The agent owns repository discovery, source review, cloning, and
building through the normal file and shell protocols. It builds in a temporary
directory, writes a temporary file beside the destination, atomically renames
that file to `<name>.wasm`, and then reloads.

URI Agent scans only non-hidden regular `.wasm` files directly inside the
persistent directory. Nested files and temporary suffixes such as `.wasm.tmp`
are ignored. Removing a file disables it at the next reload; atomically
replacing a file updates it.

### Reload lifecycle

Reload follows Pi's rebuild-then-replace resource lifecycle. URI Agent reads
the complete directory in stable path order, constructs fresh Extism runtimes,
validates their manifests and protocol names, and assembles a complete dynamic
protocol set without mutating the active set. Invalid modules and modules that
collide with built-ins, Skills, or an earlier module are skipped with
diagnostics. Only after discovery finishes does URI Agent swap the complete set
in one operation. A directory-level failure leaves the old set active. Calls
that already captured an old protocol keep its old runtime until they finish;
new calls see the replacement set.

The model tool schemas do not change because every protocol remains behind
`read` and `exec`. Reload reports the active protocol names and tells the model
to read each new `<protocol>://help`. Diagnostic strings are stored as JSON in
the session output directory rather than embedded in model-facing text;
`wasm_plugin://help` reports their count and links the file as a `file://`
address. The TUI protocol overlay reads the live registry, so it also reflects
the replacement set. Frozen session prompts expose the stable `wasm_plugin`
manager rather than embedding a mutable dynamic protocol list. Its help includes
the current active names and last reload diagnostic file. New and resumed
sessions load the current persistent plugin set; after any change, help and the
reload result are the source of truth.

### ABI version 1

ABI version 1 lets a module contribute protocols. It does not contribute system
prompt fragments, commands, panels, or status providers. A plugin export is an
Extism bytes-in/bytes-out function and can be implemented with any compatible
PDK.

Every module exports `uri_agent_manifest`, which takes no input and returns this
JSON shape:

```json
{
  "abi_version": 1,
  "protocols": [
    {
      "name": "example",
      "description": "Read and execute example resources",
      "can_read": true,
      "can_exec": true
    }
  ]
}
```

Protocol names must be unique within the module and must satisfy the normal
registry rules. Descriptions must be nonempty. Every protocol must set
`can_read` to `true` and implement `read("<protocol>://help")`; `can_exec` may be
either `true` or `false`.

A module that declares a protocol also exports `uri_agent_handle`. URI Agent
calls it with JSON containing the selected protocol, operation, original URI,
opaque target, and optional body:

```json
{
  "protocol": "example",
  "operation": "read",
  "uri": "example://a://b?x=1",
  "target": "a://b?x=1",
  "body": {"key": "value"}
}
```

`operation` is `read` or `exec`; `body` is `null` when the tool call omitted it.

The handler's returned bytes become the protocol result. Returning an Extism
error fails the tool call. A plugin remains instantiated until the next reload,
so its in-memory state survives calls; calls into one module are serialized.
Plugins must implement their own `<protocol>://help` response through the same
handler and must describe every supported address and body shape there.

WASM is a portable ABI, not a sandbox in this feature. Plugins must be treated
as trusted code. They receive WASI, unrestricted outbound HTTP, and writable
host filesystem access on Unix. The SDK's `read` and `exec` host calls route to
URI Agent's static built-in protocols, including file and available shell
protocols, with URI Agent's user permissions. Host calls reject dynamic WASM
protocols and `wasm_plugin` itself to prevent recursive entry into a module
runtime.

Reliability limits still apply: each guest call has a 30-second wall-clock
timeout, a 100-million fuel limit, a 16 MiB WebAssembly memory ceiling, and a
1 MiB Extism variable store. Modules and responses are limited to 16 MiB;
manifests are limited to 256 KiB and 64 protocols.

### Rust guest example

The workspace includes the guest crate
[`uri-agent-plugin-sdk`](../sdk/) and a buildable
[`examples/wasm-plugin`](../examples/wasm-plugin/) project. An external plugin
can depend directly on the Git repository; it needs no package manifest:

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
uri-agent-plugin-sdk = { git = "https://github.com/4fuu/uri-agent" }
```

```rust
use uri_agent_plugin_sdk::{
    HandlerRequest, HandlerResult, PluginManifest, ProtocolDescriptor,
    define_plugin,
};

fn manifest() -> PluginManifest {
    PluginManifest::new([ProtocolDescriptor::new(
        "example",
        "Read example://help before use",
        true,
        false,
    )])
}

fn handle(request: HandlerRequest) -> HandlerResult {
    match request.target.as_str() {
        "help" => Ok(b"# example\n\nDescribe every supported address here.\n".to_vec()),
        _ => Err(format!("unsupported address: {}", request.uri)),
    }
}

define_plugin!(manifest(), handle);
```

The SDK generates `uri_agent_manifest` and `uri_agent_handle`, provides typed
request and manifest values, and exposes built-in host calls as
`uri_agent_plugin_sdk::read` and `uri_agent_plugin_sdk::exec`. Build with:

```text
rustup target add wasm32-wasip1
cargo build --release --target wasm32-wasip1
```

The WASI target lets ordinary Rust filesystem APIs use the host paths granted
by URI Agent.

## Linked Rust extensions

First-party capabilities use the same plugin path exposed to linked Rust extensions:

1. A [`Plugin`](../src/plugin.rs) may declare protocol descriptors, contribute startup notices, and contribute an optional system prompt fragment before a new session's prompt is frozen.
2. `PluginRegistry` validates descriptor names, rejects duplicates, collects startup notices, and appends prompt fragments in plugin registration order after the protocol list.
3. The plugin installs protocols, commands, panel providers, or status providers through `PluginHost`; a prompt-only plugin may perform no runtime registration.
4. Registered protocols remain behind `read` and `exec`; registered commands join the searchable command panel and key-bindable command registry.

TUI extensions return generic documents and semantic status items. Status providers run while frames are drawn, so they must be fast and non-blocking. They receive `TuiStatusContext`, whose `expanded` flag allows concise footer content and richer content in the status panel.

URI Agent does not load native dynamic libraries. Third-party Rust extensions must be linked during application assembly; use the WASM ABI for runtime-loaded protocol plugins.

Keep operational plugin behavior inside registered protocols, commands, or panel providers. Use prompt fragments only for startup context that must be available before a tool call. Generic rendering belongs in the TUI; extension-specific branches do not.
