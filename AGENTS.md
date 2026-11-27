# AGENTS.md

Guidance for coding agents working anywhere in this repository. This file is the
entry point; detailed contracts belong to the linked documents.

## Project contract

UAT is a text-only Rust coding agent for script composition. It registers a
small set of ordinary tools and a codemode:

```text
read(path, offset?, limit?)
write(path, content)
edit(path, old_text, new_text)
bash(command, cwd?, timeout_ms?)
search(query, mode?, root?, glob?, limit?)
notes(op, id?, title?, content?, offset?, limit?)
steer(op?, target?, message?, name?, delivery?, reply?, scope?, in_reply_to?)
mcp(op?, server?, tool?, arguments?, uri?, prompt?)
apply_patch(patch)
codemode(code)
```

There is no URI/protocol address form, no help-loading gate, no command palette,
and no TUI. The model-facing interface is exactly the selected tool schemas.
`codemode` runs Rhai against the same tool registry; only `text(...)`, `print`
output, and a non-void return value reach the model.

UAT exposes three entry modes:

- no prompt: a line-oriented REPL;
- `-x [PROMPT]`: one non-interactive turn, final reply to stdout, progress to
  stderr;
- `serve`: HTTP text ingress with session, steer, cancel, and health routes.

Input and output are text. UAT does not write final answers to files on its own.
Callers compose processes with scripts and HTTP.

## Read before changing

Read the applicable repository map, change rules, and verification guidance in
[`docs/development.md`](docs/development.md). Then read every focused document
that owns the affected behavior:

| Area | Authoritative detail |
| --- | --- |
| Tools, codemode, search, notes, MCP, steering | [`docs/tools.md`](docs/tools.md) |
| Models, authentication, configuration, CLI, MCP files | [`docs/configuration.md`](docs/configuration.md) |
| HTTP ingress, sessions, queue/steer, retries, compaction | [`docs/sessions.md`](docs/sessions.md) |
| Project instructions and frozen startup context | [`docs/context.md`](docs/context.md) |
| Module ownership, invariants, verification | [`docs/development.md`](docs/development.md) |
| Versioning and release | [`docs/release.md`](docs/release.md) |
| Documentation ownership or an unclear destination | [`docs/README.md`](docs/README.md) |

`uat --help` defines the exact CLI. Tool descriptors define the exact
model-facing contract. If implementation, tests, and documentation disagree,
make them consistent.

## Working rules

- Keep the tool surface small. Prefer `codemode` or MCP over another
  always-visible tool.
- Put behavior in the module that owns its state and contract. Change the
  source of truth instead of adding a wrapper or duplicate type.
- Preserve sessions, steering, notes, search, and frozen project context when
  changing the runtime.
- Do not introduce URI addresses, protocol nouns, help pages, or TUI code.
- Make the smallest complete change. Leave unrelated refactors alone.
- Follow references when removing behavior and remove code that exists only for
  that behavior.
- Do not commit credentials, generated sessions, complete-output files,
  `.uat/`, `.amp/`, or `target/` artifacts.

## Verification

Use stable Rust. Add focused tests beside changed behavior, then run the checks
required by [`docs/development.md#verification`](docs/development.md#verification).
Tests must not require live credentials or network access. Unset provider API
keys before running model-listing tests if the shell has them.

For documentation-only changes, run the embedded-docs tests and verify that
`uat docs` lists the updated document set. The full Rust suite is unnecessary
unless code or documentation generation changed.

## Documentation changes

- Keep `README.md` and `README.zh-CN.md` equivalent and focused on adoption,
  first success, critical warnings, and navigation.
- Update both root READMEs when public setup or top-level behavior changes.
- Update the owning detailed document when domain behavior changes; do not copy
  mutable detail into other layers.
- Update tool descriptors and [Tools](docs/tools.md) together when a
  model-facing operation changes.
