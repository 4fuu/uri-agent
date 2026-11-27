mod apply_patch;
pub(crate) mod collaboration;
pub(crate) mod context;
mod grep;
mod mcp;
mod project;
pub(crate) mod traditional;
mod uat_docs;

pub(crate) use mcp::{GLOBAL_CONFIG as MCP_GLOBAL_CONFIG, PROJECT_CONFIG as MCP_PROJECT_CONFIG};
pub use uat_docs::docs_output;

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
    if text.contains("\r\n") {
        let mut normalized = text.replace("\r\n", "\n");
        if normalized.contains('\r') {
            normalized = normalized.replace('\r', "\n");
        }
        normalized
    } else {
        text.replace('\r', "\n")
    }
}

pub fn plugins(cwd: &Path, config_directory: &Path) -> PluginRegistry {
    let mut plugins = PluginRegistry::new();
    plugins.add(project::ProjectInstructions::new(cwd));
    plugins.add(mcp::McpPlugin::new(cwd, config_directory));
    plugins.add(apply_patch::ApplyPatchTool::new(cwd));
    plugins.add(traditional::TraditionalToolsPlugin::new(cwd));
    plugins
}

pub(super) fn resolve_path(cwd: &Path, target: &str) -> Result<std::path::PathBuf> {
    #[cfg(unix)]
    if target == "~" || target.starts_with("~/") {
        let home = dirs::home_dir().context("cannot determine the home directory")?;
        let relative = Path::new(target)
            .strip_prefix("~")
            .expect("home-relative path starts with a tilde component");
        return Ok(home.join(relative));
    }

    let path = Path::new(target);
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else if target.is_empty() {
        Ok(cwd.to_path_buf())
    } else {
        Ok(cwd.join(path))
    }
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
}
