# URI Agent plugin SDK

This crate provides the Rust guest types, exports, and host calls for trusted
URI Agent Extism plugins. It implements ABI v6 only; older ABI and the former
subagent API are unsupported. Runtime installation and limits are documented
in [WASM plugins](../docs/plugins.md).

## Minimal plugin

Configure a WASI `cdylib` guest that depends on the SDK and `serde_json`:

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
serde_json = "1"
uri-agent-plugin-sdk = "2026.830.0"
```

Define a manifest and handler, then use `define_plugin!` to generate `uri_agent_manifest` and `uri_agent_handle`:

```rust
use uri_agent_plugin_sdk::{define_plugin, HandlerRequest, HandlerResult,
    Operation, PluginManifest, ProtocolDescriptor};

fn manifest() -> PluginManifest {
    PluginManifest::new([ProtocolDescriptor::new(
        "example", "Read example://help before use", true, false)])
}

fn handle(request: HandlerRequest) -> HandlerResult {
    match request {
        HandlerRequest::Protocol { operation: Operation::Read, target, .. }
            if target == "help" => Ok(b"# example\n".to_vec()),
        _ => Err("unsupported request".into()),
    }
}
define_plugin!(manifest(), handle);
```

Every protocol implements `<protocol>://help`; bodies are strings, including
`""` when empty. `read(uri, body)` and `exec(uri, body)` call static built-ins.
Register structured operations with `with_model_tools([ModelToolDescriptor])`
and handle `HandlerRequest::ModelTool`; schemas must be strict top-level JSON
objects.

## Agents

Request access with `.request_agent_access()`. `AgentSpec::new(provider, model,
thinking, working_directory, parent_session_id)` defaults to inherited prompt
and all capabilities. Refine it with `append_system_prompt`,
`replace_system_prompt`, `with_tools`, `with_protocols`, and
`with_max_output_tokens`. The parent must be a persisted same-project depth-1
session; plugin Agents are depth 2.

```rust
use uri_agent_plugin_sdk::{AgentHandle, AgentSpec, SubmitKind};

let spec = AgentSpec::new(provider, model, thinking, cwd, parent_session_id)
    .with_tools(["replace"])
    .with_protocols(["file"])
    .with_max_output_tokens(4096);
let agent = AgentHandle::create(spec, true)?;
let submission_id = agent.submit("Review the change", SubmitKind::Prompt)?;
let status = agent.status()?;
let result = agent.result()?;
let cancelled = agent.cancel()?;
agent.close()?;
```

`AgentHandle::open(session_id, compaction_callback)` reopens an eligible child.
Handles are opaque and expose only `session_id()`. `Prompt` starts idle work or
queues a turn; `Steer` targets the next model boundary while work is active and
is accepted as Prompt when the Agent is idle. Accepted input is durable.
Provider/model and thinking freeze after the first durably accepted submission.

When the callback boolean is true, handle
`HandlerRequest::Event { event: PluginEvent::Compacted { .. } }`. Return JSON
bytes encoding `Option<AgentSpecPatch>`; patches may alter only system prompt,
tools, and protocols after summary generation. URI Agent atomically commits the
patch with the compaction checkpoint.

## State and resident lifecycle

Request `.request_state_access()` before calling `plugin_state_get`,
`plugin_state_put`, `plugin_state_delete`, `plugin_state_list`, or
`plugin_state_compare_and_set`. Choose `PluginStateScope::Global` or `Project`.
Entries include a revision; compare-and-set accepts an expected revision (or
`None` for creation) and returns `None` on mismatch. JSON values are capped at
1 MiB and live in separate SQLite state, not sessions or plugin settings.

Opt in with `.with_resident()` and handle `PluginEvent::Resident` events:
`ResidentEvent::Start`, `Wake`, and `Shutdown`. Return JSON bytes encoding
`ResidentResponse`; `wake_after_ms` requests another wake. Non-resident plugins
remain request-driven.

The SDK also exposes dynamic `model_role`, permission-free `plugin_setting` /
`set_plugin_setting`, and permission-gated environment and credential calls.
Permissions are requested with `request_environment_access()` and
`request_credentials_access()`.

Build for WASI:

```bash
rustup target add wasm32-wasip1
cargo build --release --target wasm32-wasip1
```

See the complete [buildable example](../examples/wasm-plugin/).
