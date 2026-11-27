//! Project-level instructions from `AGENTS.md`, frozen into new sessions.

use crate::config::display_path;
use crate::plugin::{Plugin, PluginHost};
use anyhow::Result;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

pub(super) struct ProjectInstructions {
    cwd: PathBuf,
}

impl ProjectInstructions {
    pub(super) fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }
}

impl Plugin for ProjectInstructions {
    fn system_prompt_fragments(&self) -> Vec<String> {
        let path = self.cwd.join("AGENTS.md");
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == ErrorKind::NotFound => return Vec::new(),
            Err(error) => {
                return vec![format!("cannot read {}: {error}", display_path(&path))];
            }
        };
        let mut fragment = String::from(
            "<project_rule_md>\n\
             The following content is from the project's AGENTS.md. Follow these instructions.\n\n",
        );
        fragment.push_str(&content);
        if !content.is_empty() && !content.ends_with('\n') {
            fragment.push('\n');
        }
        fragment.push_str("</project_rule_md>");
        vec![fragment]
    }

    fn register(&self, _host: &mut PluginHost<'_>) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_agents_file_becomes_wrapped_system_prompt_content() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("AGENTS.md"),
            "Build carefully.\nVerify work.",
        )
        .unwrap();
        let plugin = ProjectInstructions::new(directory.path());
        assert_eq!(
            plugin.system_prompt_fragments(),
            [String::from(
                "<project_rule_md>\n\
                 The following content is from the project's AGENTS.md. Follow these instructions.\n\n\
                 Build carefully.\n\
                 Verify work.\n\
                 </project_rule_md>"
            )]
        );
    }

    #[test]
    fn missing_project_agents_file_contributes_no_prompt_content() {
        let directory = tempfile::tempdir().unwrap();
        let plugin = ProjectInstructions::new(directory.path());
        assert!(plugin.system_prompt_fragments().is_empty());
    }
}
