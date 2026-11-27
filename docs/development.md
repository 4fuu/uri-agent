# Development

UAT is a stable-Rust application with one model-tool loop, one session store,
and a small set of linked tools. The runtime does not special-case tool names;
tools are registered by built-in plugins and the linked MCP provider.

## Module map

| Path | Ownership |
| --- | --- |
| `src/main.rs` | CLI dispatch: docs, models, REPL, execute, serve |
| `src/config.rs` | CLI, layered settings, credentials, Agent Environment, MCP paths |
| `src/catalog.rs` | Model catalog, provider discovery, model limits |
| `src/model/` | Provider HTTP/WebSocket transports and retry/failure rules |
| `src/agent.rs` | `AgentHost`, services, tool selection, startup context |
| `src/runtime.rs` | Turn loop, tool dispatch, steering queue, compaction |
| `src/session.rs` | Append-only SQLite sessions, replay, usage, notes events |
| `src/compaction.rs` | Context measurement and summary checkpoint planning |
| `src/builtins/` | Tools, project instructions, MCP, notes, search, codemode |
| `src/plugin.rs` | Model-tool registry and linked-plugin boundary |
| `src/prompts.rs` | Generated text-mode system prompt |
| `src/execute.rs` | Non-interactive `-x` mode and shared turn helper |
| `src/repl.rs` | Line REPL |
| `src/serve.rs` | HTTP ingress |
| `src/retrieval/` | Local semantic code index |
| `src/tool_download.rs` | Download and verify ripgrep and binary assets |

## Native retrieval assets

Release archives bundle a fixed zvec native library, Jieba dictionaries, and a
Model2Vec embedding model. A Cargo build does not copy them next to the
executable. For a Unix development build:

```bash
target=x86_64-unknown-linux-gnu # or aarch64-unknown-linux-gnu / aarch64-apple-darwin
stage="target/retrieval-assets/$target"
python3 scripts/prepare-retrieval-assets.py --target "$target" --output "$stage"
ZVEC_LIB_DIR="$stage" cargo build --locked --package uat
cp target/debug/uat "$stage/"
LD_LIBRARY_PATH="$stage${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
  "$stage/uat" --version
```

Windows uses `x86_64-pc-windows-msvc`, `uat.exe`, and `zvec_c_api.dll`. To
exercise the native retrieval path:

```bash
UAT_TEST_RETRIEVAL_ASSETS="$stage" ZVEC_LIB_DIR="$stage" \
  LD_LIBRARY_PATH="$stage${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
  cargo test --locked --package uat --lib \
  retrieval::tests::bundled_assets_rebuild_and_search -- --ignored --exact
```

## Model-facing interface

The model sees exactly the selected tools. Built-ins register through
`Plugin::register` into `ModelToolRegistry`; a plugin declares the same
descriptors through `model_tool_descriptors`, and `PluginRegistry::install`
rejects a mismatch. A model tool implements:

```rust
#[async_trait]
trait ModelTool {
    fn descriptor(&self) -> ModelToolDescriptor;
    async fn execute(&self, arguments: &Value) -> Result<ModelToolOutput>;
}
```

Tool results are text. Binary or media content is summarized rather than added
to model history. `codemode` calls the same registry through `dispatch`, so a
tool cannot behave differently in a script.

## Invariants

- There is no URI, protocol address, help gate, command palette, or TUI layer.
- `notes`, `steer`, `codemode`, and `mcp` are ordinary registered tools.
- The model can only return text; the host does not write final answers to
  files.
- Sessions remain append-only. A turn boundary is durable before completion is
  published.
- A steer accepted while a turn is active is injected at a model boundary. If
  the session is idle, it becomes a queued prompt.
- Project instructions are frozen into the startup context and never
  reinterpreted on resume.
- Tool calls and results are recorded as paired session events.
- Peer messages and MCP content are marked untrusted. Peer content never grants
  authorization.
- `uat serve` refuses a non-loopback bind without `UAT_HTTP_TOKEN`.

## Verification

Use stable Rust:

```bash
cargo fmt --check
cargo check
cargo test --lib
```

The full suite has one ignored test that needs staged retrieval assets:

```bash
cargo test --locked --package uat --lib \
  retrieval::tests::bundled_assets_rebuild_and_search -- --ignored --exact
```

For changes that affect the binary or HTTP surface, also run:

```bash
cargo build --bin uat
./target/debug/uat --help
./target/debug/uat docs tools.md
```

Start the HTTP server and exercise `GET /v1/health`, `POST /v1/messages`, and
`GET /v1/sessions/{id}` against a local model or a stub provider. The HTTP
server refuses a non-loopback address without `UAT_HTTP_TOKEN`.

Tests must not require live credentials or network access. Unset provider API
keys when the environment has them, because model-listing tests can otherwise
see extra runnable providers.

## Change rules

- Put behavior in its owning module; do not add a wrapper when the existing
  source of truth can change.
- Keep the tool surface small. Prefer `codemode` composition or an MCP server
  over a new always-visible tool.
- Update the owning document for domain behavior and both root READMEs for
  public setup or top-level behavior.
- Do not commit credentials, session databases, generated run files, `.uat/`,
  `.amp/`, or `target/` artifacts.
- Preserve unrelated work in the tree. Remove temporary scripts and fixtures
  after validation.

## Release artifacts

Release workflows build the `uat` binary with bundled retrieval assets. The
`zvec-rust-sys` native library and model assets must match the pins in
`scripts/prepare-retrieval-assets.py`. See [Release](release.md).
