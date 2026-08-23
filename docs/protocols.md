# Protocols, tasks, output, and Skills

URI Agent keeps the model-facing tool surface fixed while allowing the application to register new capabilities. This document describes that boundary, built-in protocols, asynchronous execution, output preservation, and Skill loading.

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
| `uri-agent-docs` | `read` | Read version-matched URI Agent documentation embedded in the binary |
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

### `uri-agent-docs`

The Markdown files under `docs/` are embedded in the binary at build time, so
they remain readable from any startup working directory and match the running
URI Agent version. Start with the embedded documentation index:

```text
read("uri-agent-docs://README.md")
```

Other targets are the exact, case-sensitive filenames linked by that index,
such as `uri-agent-docs://protocols.md`. Read `uri-agent-docs://help` for the
complete filename list. Paths, query parameters, and execution are not
supported.

### `file`

Relative paths resolve from the canonical startup working directory; absolute paths remain absolute. Reading a directory returns a sorted, bounded listing. Text reads accept a one-based line range:

```text
read("file://src/main.rs?offset=1&limit=200")
```

File content is returned without line numbers by default. Add `line_numbers=true` when one-based line prefixes are useful:

```text
read("file://src/main.rs?offset=1&limit=200&line_numbers=true")
```

`file://help` reports the accepted range options, active limits, and current working directory.

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

URI Agent injects the latest values from its global Agent environment manager into every new shell command. Managed values override inherited process variables with the same name. The user-controlled `:terminal` is separate and does not receive them; see [Agent environment](configuration.md#agent-environment).

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

If the wait window expires, the task keeps running and the response still includes its task URI. `?wait=N` belongs to `bash` and `pwsh`; it is not a registry option. Read the active shell protocol's help for the accepted range.

Cancellation must terminate the spawned process, not only the Rust future that waits for it.

## Complete output preservation

When protocol output exceeds the active inline limit, URI Agent:

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

## Extension protocols

Capabilities may register protocols through linked Rust extensions or trusted runtime-loaded WASM modules. Both remain behind `read` and `exec`. See [WASM plugins](plugins.md) for installation, reload, ABI, permissions, and SDK usage; linked first-party extension internals belong to the [development guide](development.md#linked-rust-extensions).
