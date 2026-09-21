pub mod acp;
pub mod agent;
mod atomic_file;
pub mod builtins;
pub mod catalog;
pub mod clipboard;
pub mod compaction;
pub mod config;
pub mod herdr;
pub mod keymap;
pub mod model;
pub mod oauth;
pub mod output;
pub mod plugin;
pub mod plugin_state;
mod process;
pub mod prompts;
pub mod protocol;
mod retrieval;
pub mod runtime;
pub mod session;
pub mod skill;
pub mod task;
pub mod terminal;
mod text_metrics;
mod tool_download;
pub mod tui;
mod update;
pub mod wasm_plugin;

/// Lowercase hexadecimal encoding for digest and checksum bytes.
pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
