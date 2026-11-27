pub mod agent;
mod atomic_file;
pub mod builtins;
pub mod catalog;
pub mod compaction;
pub mod config;
pub mod execute;
pub mod model;
pub mod oauth;
pub mod plugin;
mod process;
pub mod prompts;
pub mod repl;
mod retrieval;
pub mod runtime;
pub mod serve;
pub mod session;
#[cfg(test)]
mod test_http;
mod tool_download;

/// Lowercase hexadecimal encoding for digest and checksum bytes.
pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
