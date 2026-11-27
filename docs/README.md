# UAT documentation

The root [README](../README.md) explains what UAT is and the shortest path to a
working session. This directory owns the detailed behavior.

| Topic | Document |
| --- | --- |
| Tools, codemode, search, notes, MCP, steering | [Tools](tools.md) |
| Models, credentials, settings, environment variables, MCP config | [Configuration](configuration.md) |
| Sessions, HTTP API, resume, compaction | [Sessions and HTTP](sessions.md) |
| Project instructions and frozen startup context | [Context](context.md) |
| Module ownership, change rules, verification | [Development](development.md) |
| Versioning and release workflow | [Release](release.md) |

## Sources of truth

- `uat --help` defines the command-line interface.
- `uat docs [<topic>]` lists or prints the documents embedded in this binary.
- The model-tool schemas in `src/builtins/` define exact tool arguments.
- [README.md](README.md) is the index.

## Documentation rules

- Describe only behavior that exists on the current branch.
- Keep the root READMEs equivalent and focused on adoption and navigation.
- Put mutable details in one owning document, then link to it instead of
  copying them into other layers.
- When a tool's model-facing operations change, update its descriptor and this
  documentation together.
