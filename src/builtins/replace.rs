use super::file::resolve_path;
use super::{EditableText, atomic_write, normalize_line_endings};
use crate::plugin::{Plugin, PluginHost};
use crate::protocol::{Protocol, ProtocolContext, ProtocolDescriptor, ProtocolRequest};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use serde_json::Value;
use std::path::{Path, PathBuf};
use tokio::fs;

const HELP: &str = r#"# replace

Replace one exact text match and return the final result.

Call `exec` with `replace://<path>` and encode the replacement object as JSON:

```text
exec("replace://<path>", {"kind":"json","value":"{\"old_text\":\"<old text>\",\"new_text\":\"<replacement>\"}"})
```

Replace `<path>` with the project-relative or absolute path of the file to edit,
`<old text>` with the exact project text to find, and `<replacement>` with its
new content. Relative paths resolve from the startup working directory.
`old_text` must be nonempty and occur exactly once. The file is replaced
atomically. `exec` returns after the replacement succeeds; validation and write
errors are returned directly.

`read` supports only `replace://help`.
"#;

#[derive(Clone)]
pub(super) struct ReplaceProtocol {
    cwd: PathBuf,
}

impl ReplaceProtocol {
    pub(super) fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }
}

impl Plugin for ReplaceProtocol {
    fn protocol_descriptors(&self) -> Vec<ProtocolDescriptor> {
        vec![self.descriptor()]
    }

    fn register(&self, host: &mut PluginHost<'_>) -> Result<()> {
        host.protocols.register(self.clone())
    }
}

#[async_trait]
impl Protocol for ReplaceProtocol {
    fn descriptor(&self) -> ProtocolDescriptor {
        ProtocolDescriptor {
            name: "replace".to_string(),
            description: "Atomically replace one exact text match in a file.".to_string(),
            can_read: true,
            can_exec: true,
        }
    }

    async fn read(
        &self,
        request: ProtocolRequest<'_>,
        _context: ProtocolContext,
    ) -> Result<Vec<u8>> {
        if request.target != "help" {
            bail!("expected replace://help");
        }
        Ok(HELP.as_bytes().to_vec())
    }

    async fn exec(
        &self,
        request: ProtocolRequest<'_>,
        _context: ProtocolContext,
    ) -> Result<Vec<u8>> {
        if request.target.is_empty() {
            bail!("replace target path cannot be empty");
        }
        let body = request
            .body
            .ok_or_else(|| anyhow!("replace body is required"))?;
        let path = resolve_path(&self.cwd, request.target);
        replace_exact(&path, body).await?;
        Ok(format!("Updated {}\n", path.display()).into_bytes())
    }
}

async fn replace_exact(path: &Path, body: &Value) -> Result<()> {
    let object = body
        .as_object()
        .ok_or_else(|| anyhow!("replace body must be an object"))?;
    let old_text = object
        .get("old_text")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("replace body requires an old_text string"))?;
    let new_text = object
        .get("new_text")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("replace body requires a new_text string"))?;
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
    use crate::task::TaskManager;

    #[tokio::test]
    async fn protocol_exec_returns_the_completed_replacement_directly() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file.txt");
        fs::write(&path, "alpha beta\n").await.unwrap();
        let protocol = ReplaceProtocol::new(directory.path());
        let tasks = TaskManager::new();
        let context = ProtocolContext {
            tasks: tasks.clone(),
        };
        let body = serde_json::json!({"old_text": "beta", "new_text": "gamma"});
        let help = protocol
            .read(
                ProtocolRequest {
                    uri: "replace://help",
                    target: "help",
                    body: None,
                },
                context.clone(),
            )
            .await
            .unwrap();
        assert!(
            String::from_utf8(help)
                .unwrap()
                .contains("replace://<path>")
        );

        let output = protocol
            .exec(
                ProtocolRequest {
                    uri: "replace://file.txt",
                    target: "file.txt",
                    body: Some(&body),
                },
                context.clone(),
            )
            .await
            .unwrap();

        assert!(String::from_utf8(output).unwrap().contains("Updated"));
        assert_eq!(fs::read_to_string(path).await.unwrap(), "alpha gamma\n");
        assert!(tasks.list().await.is_empty());
        assert!(
            protocol
                .read(
                    ProtocolRequest {
                        uri: "replace://tasks",
                        target: "tasks",
                        body: None,
                    },
                    context,
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("replace://help")
        );
    }

    #[tokio::test]
    async fn exact_replace_is_atomic_and_rejects_missing_or_ambiguous_matches() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file.txt");
        fs::write(&path, "alpha beta").await.unwrap();

        replace_exact(
            &path,
            &serde_json::json!({"old_text": "beta", "new_text": "gamma"}),
        )
        .await
        .unwrap();
        assert_eq!(fs::read_to_string(&path).await.unwrap(), "alpha gamma");

        let missing = replace_exact(
            &path,
            &serde_json::json!({"old_text": "missing", "new_text": "x"}),
        )
        .await
        .unwrap_err();
        assert!(missing.to_string().contains("was not found"));

        fs::write(&path, "aaa").await.unwrap();
        let ambiguous = replace_exact(
            &path,
            &serde_json::json!({"old_text": "aa", "new_text": "x"}),
        )
        .await
        .unwrap_err();
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

        replace_exact(
            &path,
            &serde_json::json!({
                "old_text": "alpha\nbeta\n",
                "new_text": "ALPHA\nBETA\n"
            }),
        )
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
            &serde_json::json!({
                "old_text": "\u{feff}alpha\nbeta\n",
                "new_text": "\u{feff}ALPHA\nBETA"
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            fs::read_to_string(with_newline).await.unwrap(),
            "\u{feff}ALPHA\r\nBETA\r\n"
        );

        let without_newline = directory.path().join("without-newline.txt");
        fs::write(&without_newline, "alpha\nbeta").await.unwrap();
        replace_exact(
            &without_newline,
            &serde_json::json!({"old_text": "alpha\nbeta", "new_text": "ALPHA\nBETA\n"}),
        )
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

        let error = replace_exact(
            &path,
            &serde_json::json!({"old_text": "hello\nworld\n", "new_text": "replacement\n"}),
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("more than once"));
        assert_eq!(fs::read_to_string(path).await.unwrap(), original);
    }
}
