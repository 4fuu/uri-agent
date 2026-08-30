mod agents;
mod apply_patch;
mod file;
mod grep;
mod https;
mod mcp;
pub(crate) mod model_tools;
mod replace;
mod sessions;
mod shell;
mod tasks;
mod title;
mod uri_agent_docs;

pub use mcp::{
    SessionMcpProfile, SessionMcpServer, SessionMcpTransport, session_profile_owner,
    session_profile_record,
};
pub(crate) const MCP_SESSION_PROFILE_OWNER: &str = mcp::SESSION_PROFILE_OWNER;

use crate::config::display_path;
use crate::plugin::PluginRegistry;
use anyhow::{Context, Result, anyhow};
use std::path::Path;
use tokio::fs;
use uuid::Uuid;

#[derive(Clone, Copy)]
enum LineEnding {
    Lf,
    Crlf,
}

pub(super) struct EditableText {
    normalized: String,
    has_bom: bool,
    has_final_newline: bool,
    line_ending: LineEnding,
}

impl EditableText {
    pub(super) fn new(raw: &str) -> Self {
        let (has_bom, text) = raw
            .strip_prefix('\u{feff}')
            .map_or((false, raw), |text| (true, text));
        let line_ending = text.find('\n').map_or(LineEnding::Lf, |index| {
            if index > 0 && text.as_bytes()[index - 1] == b'\r' {
                LineEnding::Crlf
            } else {
                LineEnding::Lf
            }
        });
        Self {
            normalized: normalize_line_endings(text),
            has_bom,
            has_final_newline: text.ends_with(['\n', '\r']),
            line_ending,
        }
    }

    pub(super) fn normalized(&self) -> &str {
        &self.normalized
    }

    pub(super) fn restore(&self, normalized: &str) -> String {
        let mut normalized = normalized.to_string();
        if self.has_final_newline {
            if !normalized.ends_with('\n') {
                normalized.push('\n');
            }
        } else {
            normalized.truncate(normalized.trim_end_matches('\n').len());
        }
        let content = match self.line_ending {
            LineEnding::Lf => normalized,
            LineEnding::Crlf => normalized.replace('\n', "\r\n"),
        };
        if !self.has_bom {
            return content;
        }
        let mut restored = String::with_capacity('\u{feff}'.len_utf8() + content.len());
        restored.push('\u{feff}');
        restored.push_str(&content);
        restored
    }
}

pub(super) fn normalize_line_endings(text: &str) -> String {
    if !text.contains('\r') {
        return text.to_string();
    }
    text.replace("\r\n", "\n").replace('\r', "\n")
}

pub fn plugins(cwd: &Path, config_directory: &Path) -> PluginRegistry {
    plugins_with_session_profile(cwd, config_directory, None)
}

pub(crate) fn plugins_with_session_profile(
    cwd: &Path,
    config_directory: &Path,
    mcp_profile: Option<serde_json::Value>,
) -> PluginRegistry {
    let mut plugins = PluginRegistry::new();
    add_agent_plugins(&mut plugins, cwd, config_directory, mcp_profile);
    plugins.add(title::TerminalTitlePlugin);
    plugins
}

fn add_agent_plugins(
    plugins: &mut PluginRegistry,
    cwd: &Path,
    config_directory: &Path,
    mcp_profile: Option<serde_json::Value>,
) {
    plugins.add(model_tools::ProtocolToolsPlugin);
    plugins.add(agents::AgentsPlugin::new(cwd));
    plugins.add(mcp::McpPlugin::with_session_profile(
        cwd,
        config_directory,
        mcp_profile,
    ));
    plugins.add(uri_agent_docs::UriAgentDocsProtocol);
    plugins.add(file::FileProtocol::new(cwd));
    plugins.add(grep::GrepProtocol::new(cwd));
    plugins.add(https::HttpsProtocol::new());
    plugins.add(replace::ReplaceTool::new(cwd));
    plugins.add(apply_patch::ApplyPatchTool::new(cwd));
    plugins.add(sessions::SessionsPlugin::new(cwd));
    plugins.add(tasks::TasksProtocol);
    shell::add_plugins(plugins, cwd);
}

async fn atomic_write(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("file has no parent directory: {}", display_path(path)))?;
    fs::create_dir_all(parent)
        .await
        .with_context(|| format!("cannot create {}", display_path(parent)))?;
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let temporary = parent.join(format!(".{filename}.{}.tmp", Uuid::now_v7().simple()));
    fs::write(&temporary, content)
        .await
        .with_context(|| format!("cannot write {}", display_path(&temporary)))?;
    if let Ok(metadata) = fs::metadata(path).await {
        fs::set_permissions(&temporary, metadata.permissions()).await?;
    }
    if let Err(error) = fs::rename(&temporary, path).await {
        let _ = fs::remove_file(&temporary).await;
        return Err(error).with_context(|| format!("cannot replace {}", display_path(path)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editable_text_normalizes_and_restores_bom_and_line_endings() {
        let crlf = EditableText::new("\u{feff}one\r\ntwo\r\n");
        assert_eq!(crlf.normalized(), "one\ntwo\n");
        assert_eq!(crlf.restore("ONE\ntwo\n"), "\u{feff}ONE\r\ntwo\r\n");
        assert_eq!(crlf.restore("ONE\ntwo"), "\u{feff}ONE\r\ntwo\r\n");

        let crlf_first = EditableText::new("one\r\ntwo\nthree\r");
        assert_eq!(crlf_first.normalized(), "one\ntwo\nthree\n");
        assert_eq!(
            crlf_first.restore("one\nTWO\nthree\n"),
            "one\r\nTWO\r\nthree\r\n"
        );

        let lf_first = EditableText::new("one\ntwo\r\nthree\r");
        assert_eq!(lf_first.normalized(), "one\ntwo\nthree\n");
        assert_eq!(lf_first.restore("one\nTWO\nthree\n"), "one\nTWO\nthree\n");

        let no_final_newline = EditableText::new("one\r\ntwo");
        assert_eq!(no_final_newline.restore("one\nTWO\n\n"), "one\r\nTWO");
    }

    #[test]
    fn built_in_distribution_separates_protocols_from_direct_edit_tools() {
        let directory = tempfile::tempdir().unwrap();
        let plugins = plugins(directory.path(), directory.path());
        let names = plugins
            .protocol_descriptors()
            .unwrap()
            .into_iter()
            .map(|descriptor| descriptor.name)
            .collect::<Vec<_>>();

        assert!(names.iter().any(|name| name == "uri-agent-docs"));
        assert!(names.iter().any(|name| name == "file"));
        assert!(names.iter().any(|name| name == "grep"));
        assert!(names.iter().any(|name| name == "https"));
        assert!(names.iter().any(|name| name == "tasks"));
        assert!(!names.iter().any(|name| name == "replace"));
        assert!(!names.iter().any(|name| name == "apply_patch"));

        let model_tools = plugins
            .model_tool_descriptors()
            .unwrap()
            .into_iter()
            .map(|descriptor| descriptor.name)
            .collect::<Vec<_>>();
        assert_eq!(model_tools, ["apply_patch", "exec", "read", "replace"]);
    }
}
