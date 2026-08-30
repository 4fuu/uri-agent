# ACP v1 implementation TODO

- [x] Add `--acpv1` as an stdio-only ACP v1 entry point before project or TUI startup.
- [x] Keep ACP transport and schema mapping in a dedicated adapter; keep ACP names out of model and tool runtime code.
- [x] Expose frontend-neutral typed prompt, ordered update, completion, cancellation, and session lifecycle APIs.
- [x] Create native depth-1 sessions from the ACP-requested absolute `cwd`; preserve normal provider and model defaults.
- [x] Bind ACP-provided MCP servers to the owning session without changing `mcp.json` or exposing secrets in events or diagnostics.
- [x] Project live and replayed native session events to ACP updates without duplicate streamed content.
- [x] Support new, load/resume, list, close, prompt, and cancellation with correct ordering and cleanup.
- [x] Make an ACP-created session resume through the TUI with the same frozen context, model history, protocol set, and reconstructible MCP capabilities after ACP releases it.
- [x] Keep the default TUI, background mode, model prompt, tool schemas, stored sessions, and configured MCP behavior unchanged.
- [x] Add protocol, persistence, cancellation, MCP lifecycle, compatibility, and stdio integration tests; update CLI and detailed documentation.

## Cautions

- Stdout is reserved for ACP JSON-RPC; diagnostics belong on stderr.
- Never persist ACP-supplied secret values in session events, protocol records, or diagnostic output.
- ACP prompt completion follows durable turn settlement, including cancellation and tool cleanup.
- Existing sessions must remain readable, and ACP-specific capabilities must be advertised only when implemented.
- Sequential ACP-to-TUI ownership is required; simultaneous cross-process control of one session is not part of this change.
