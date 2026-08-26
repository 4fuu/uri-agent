use super::file::resolve_path;
use super::{EditableText, atomic_write, normalize_line_endings};
use crate::plugin::{ModelTool, ModelToolDescriptor, Plugin, PluginHost};
use crate::protocol::ProtocolRegistry;
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tokio::fs;

#[derive(Clone)]
pub(super) struct ReplaceTool {
    cwd: PathBuf,
}

impl ReplaceTool {
    pub(super) fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }
}

impl Plugin for ReplaceTool {
    fn model_tool_descriptors(&self) -> Vec<ModelToolDescriptor> {
        vec![<Self as ModelTool>::descriptor(self)]
    }

    fn register(&self, host: &mut PluginHost<'_>) -> Result<()> {
        host.model_tools.register(self.clone())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplaceArguments {
    path: String,
    old_text: String,
    new_text: String,
}

#[async_trait]
impl ModelTool for ReplaceTool {
    fn descriptor(&self) -> ModelToolDescriptor {
        ModelToolDescriptor {
            name: "replace".to_string(),
            description: "Replace one exact text match in a UTF-8 file atomically. Relative paths resolve from the startup working directory. The old text must be nonempty and occur exactly once; missing or ambiguous matches leave the file unchanged.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Project-relative or absolute file path."},
                    "old_text": {"type": "string", "description": "Exact nonempty text to replace. Must occur once."},
                    "new_text": {"type": "string", "description": "Replacement text."}
                },
                "required": ["path", "old_text", "new_text"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, arguments: &Value, _protocols: &ProtocolRegistry) -> Result<String> {
        let arguments: ReplaceArguments =
            serde_json::from_value(arguments.clone()).context("invalid replace tool arguments")?;
        if arguments.path.is_empty() {
            bail!("replace path cannot be empty");
        }
        let path = resolve_path(&self.cwd, &arguments.path);
        replace_exact(&path, &arguments.old_text, &arguments.new_text).await?;
        Ok(format!("Updated {}", path.display()))
    }
}

async fn replace_exact(path: &Path, old_text: &str, new_text: &str) -> Result<()> {
    if old_text.is_empty() {
        bail!("old_text cannot be empty");
    }

    let original = fs::read_to_string(path)
        .await
        .with_context(|| format!("cannot read {}", path.display()))?;
    let original = EditableText::new(&original);
    let old_text = normalize_line_endings(old_text.strip_prefix('\u{feff}').unwrap_or(old_text));
    let new_text = normalize_line_endings(new_text.strip_prefix('\u{feff}').unwrap_or(new_text));
    if old_text.is_empty() {
        bail!("old_text cannot be empty");
    }
    let content = original.normalized();
    let first = content
        .find(&old_text)
        .ok_or_else(|| anyhow!("old_text was not found in {}", path.display()))?;
    let next_start = first + old_text.chars().next().map_or(0, char::len_utf8);
    if content[next_start..].contains(&old_text) {
        bail!("old_text appears more than once in {}", path.display());
    }

    let mut updated = String::with_capacity(content.len() - old_text.len() + new_text.len());
    updated.push_str(&content[..first]);
    updated.push_str(&new_text);
    updated.push_str(&content[first + old_text.len()..]);
    atomic_write(path, original.restore(&updated).as_bytes()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::OutputStore;
    use crate::task::TaskManager;
    use std::sync::Arc;

    #[tokio::test]
    async fn direct_tool_returns_the_completed_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file.txt");
        fs::write(&path, "alpha beta\n").await.unwrap();
        let output_store = Arc::new(
            OutputStore::new(&format!("replace-{}", uuid::Uuid::now_v7().simple()), 1024)
                .await
                .unwrap(),
        );
        let protocols = ProtocolRegistry::new(output_store.clone(), TaskManager::new());
        let tool = ReplaceTool::new(directory.path());

        let output = tool
            .execute(
                &json!({
                    "path": "file.txt",
                    "old_text": "beta",
                    "new_text": "gamma"
                }),
                &protocols,
            )
            .await
            .unwrap();

        assert!(output.contains("Updated"));
        assert_eq!(fs::read_to_string(path).await.unwrap(), "alpha gamma\n");
        let _ = fs::remove_dir_all(output_store.directory()).await;
    }

    #[tokio::test]
    async fn exact_replace_is_atomic_and_rejects_missing_or_ambiguous_matches() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file.txt");
        fs::write(&path, "alpha beta").await.unwrap();

        replace_exact(&path, "beta", "gamma").await.unwrap();
        assert_eq!(fs::read_to_string(&path).await.unwrap(), "alpha gamma");

        let missing = replace_exact(&path, "missing", "x").await.unwrap_err();
        assert!(missing.to_string().contains("was not found"));

        fs::write(&path, "aaa").await.unwrap();
        let ambiguous = replace_exact(&path, "aa", "x").await.unwrap_err();
        assert!(ambiguous.to_string().contains("more than once"));
        assert_eq!(fs::read_to_string(path).await.unwrap(), "aaa");
    }

    #[tokio::test]
    async fn exact_replace_matches_lf_text_and_preserves_crlf_and_bom() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file.txt");
        fs::write(&path, "\u{feff}alpha\r\nbeta\r\ngamma\r\n")
            .await
            .unwrap();

        replace_exact(&path, "alpha\nbeta\n", "ALPHA\nBETA\n")
            .await
            .unwrap();

        assert_eq!(
            fs::read_to_string(path).await.unwrap(),
            "\u{feff}ALPHA\r\nBETA\r\ngamma\r\n"
        );
    }

    #[tokio::test]
    async fn exact_replace_ignores_fragment_bom_and_preserves_final_newline_state() {
        let directory = tempfile::tempdir().unwrap();
        let with_newline = directory.path().join("with-newline.txt");
        fs::write(&with_newline, "\u{feff}alpha\r\nbeta\r\n")
            .await
            .unwrap();

        replace_exact(
            &with_newline,
            "\u{feff}alpha\nbeta\n",
            "\u{feff}ALPHA\nBETA",
        )
        .await
        .unwrap();
        assert_eq!(
            fs::read_to_string(with_newline).await.unwrap(),
            "\u{feff}ALPHA\r\nBETA\r\n"
        );

        let without_newline = directory.path().join("without-newline.txt");
        fs::write(&without_newline, "alpha\nbeta").await.unwrap();
        replace_exact(&without_newline, "alpha\nbeta", "ALPHA\nBETA\n")
            .await
            .unwrap();
        assert_eq!(
            fs::read_to_string(without_newline).await.unwrap(),
            "ALPHA\nBETA"
        );
    }

    #[tokio::test]
    async fn exact_replace_detects_duplicates_across_line_ending_variants() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file.txt");
        let original = "hello\r\nworld\r\n---\nhello\nworld\n";
        fs::write(&path, original).await.unwrap();

        let error = replace_exact(&path, "hello\nworld\n", "replacement\n")
            .await
            .unwrap_err();

        assert!(error.to_string().contains("more than once"));
        assert_eq!(fs::read_to_string(path).await.unwrap(), original);
    }
}
