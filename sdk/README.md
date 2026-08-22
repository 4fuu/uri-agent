# URI Agent plugin SDK

This directory contains Rust guest types and host calls for URI Agent Extism
WebAssembly protocol plugins. Plugins export `uri_agent_manifest` and
`uri_agent_handle`, and may use `read` and `exec` to call URI Agent's built-in
protocols.

Use `define_plugin!(manifest(), handler)` to generate the ABI exports, then
build a `cdylib` for `wasm32-wasip1`. See the buildable
[`examples/wasm-plugin`](../examples/wasm-plugin/) project.

The canonical build and installation instructions are published to the model at
`wasm_plugin://help` and documented in the repository's
[`docs/protocols.md`](../docs/protocols.md#wasm-plugins).
