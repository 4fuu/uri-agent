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

Protocols own their target syntax. `file` interprets `?offset` and `?limit`; `bash` and `pwsh` interpret `?wait`; the registry treats all of those characters as opaque.

Protocol names must be unique. Registration fails rather than silently replacing an existing protocol.

## Built-in protocols

| Protocol | Operations | Responsibility |
| --- | --- | --- |
| `file` | `read` | Read files and bounded directory listings |
| `replace` | `read`, `exec` | Atomically replace one exact text match |
| `apply_patch` | `read`, `exec` | Apply Codex-style add, delete, update, and move patches |
| `bash` | `read`, `exec` | Run Bash commands as managed tasks when Bash exists |
| `pwsh` | `read`, `exec` | Run PowerShell 7 commands as managed tasks when `pwsh` exists |
| `<name>-skill` | `read` | Load one discovered Skill and its bundled files |

`bash` and `pwsh` are detected at startup and registered only when the corresponding executable is available.

### `file`

Relative paths resolve from the canonical startup working directory; absolute paths remain absolute. Reading a directory returns a sorted, bounded listing. Text reads accept a one-based line range:

```text
read("file://src/main.rs?offset=1&limit=200")
```

The default limit is 200 lines and the protocol clamps requested limits to 2,000 lines. Read `file://help` for the current contract.

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

Shell bodies may be a command string or an object containing a `command` string:

```text
exec("bash://run", "cargo test")
exec("pwsh://run", {"command":"cargo test"})
```

Commands run from the startup working directory. Bash starts without profile or rc files; PowerShell starts without a profile and reads the script from standard input.

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

## Rust extensions

First-party capabilities use the same plugin path exposed to linked Rust extensions:

1. A [`Plugin`](../src/plugin.rs) declares protocol descriptors before a new session's prompt is frozen.
2. `PluginRegistry` validates descriptor names and rejects duplicates.
3. The plugin installs protocols, commands, panel providers, or status providers through `PluginHost`.
4. Registered protocols remain behind `read` and `exec`; registered commands join the searchable command panel and key-bindable command registry.

TUI extensions return generic documents and semantic status items. Status providers run while frames are drawn, so they must be fast and non-blocking. They receive `TuiStatusContext`, whose `expanded` flag allows concise footer content and richer content in the status panel.

URI Agent does not currently load native dynamic libraries. Third-party Rust extensions must be linked during application assembly.

Keep plugin-specific behavior inside registered protocols, commands, or panel providers. Generic rendering belongs in the TUI; extension-specific branches do not.
