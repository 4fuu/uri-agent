use super::file::resolve_path;
use super::{render_task, render_task_list, split_wait, task_response};
use crate::prompts;
use crate::protocol::{Protocol, ProtocolContext, ProtocolDescriptor, ProtocolRequest};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use serde_json::Value;
use std::path::{Path, PathBuf};
use tokio::fs;
use uuid::Uuid;

pub struct EditProtocol {
    cwd: PathBuf,
}

impl EditProtocol {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }
}

#[async_trait]
impl Protocol for EditProtocol {
    fn descriptor(&self) -> ProtocolDescriptor {
        ProtocolDescriptor {
            name: "edit".to_string(),
            description: "Atomically create, replace, or make an exact text edit to a file."
                .to_string(),
            can_read: true,
            can_exec: true,
        }
    }

    async fn read(
        &self,
        request: ProtocolRequest<'_>,
        context: ProtocolContext,
    ) -> Result<Vec<u8>> {
        match request.target {
            "help" => Ok(prompts::EDIT_HELP.as_bytes().to_vec()),
            "tasks" => Ok(render_task_list(&context.tasks, "edit").await),
            target => {
                let id = target
                    .strip_prefix("tasks/")
                    .ok_or_else(|| anyhow!("expected edit://help or edit://tasks/<id>"))?;
                render_task(&context.tasks, "edit", id).await
            }
        }
    }

    async fn exec(
        &self,
        request: ProtocolRequest<'_>,
        context: ProtocolContext,
    ) -> Result<Vec<u8>> {
        let (target, wait) = split_wait(request.target)?;
        if target.is_empty() {
            bail!("edit target path cannot be empty");
        }
        let body = request.body.cloned();
        let path = resolve_path(&self.cwd, target);
        let label = format!("edit {}", path.display());
        let record = context.tasks.allocate("edit", label).await;
        let id = record.id.clone();
        let tasks = context.tasks.clone();
        tasks
            .spawn(record, async move {
                let body = body.ok_or_else(|| anyhow!("edit body is required"))?;
                apply_edit(&path, body).await?;
                Ok(format!("Updated {}\n", path.display()).into_bytes())
            })
            .await;
        task_response(&context.tasks, "edit", &id, wait).await
    }
}

async fn apply_edit(path: &Path, body: Value) -> Result<()> {
    let object = body
        .as_object()
        .ok_or_else(|| anyhow!("edit body must be an object"))?;
    if let Some(content) = object.get("content") {
        let content = content
            .as_str()
            .ok_or_else(|| anyhow!("edit content must be a string"))?;
        return atomic_write(path, content.as_bytes()).await;
    }

    let old_text = object
        .get("old_text")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("edit body requires content, or old_text and new_text"))?;
    let new_text = object
        .get("new_text")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("new_text must be a string"))?;
    if old_text.is_empty() {
        bail!("old_text cannot be empty");
    }
    let original = fs::read_to_string(path)
        .await
        .with_context(|| format!("cannot read {}", path.display()))?;
    let first = original
        .find(old_text)
        .ok_or_else(|| anyhow!("old_text was not found in {}", path.display()))?;
    let next_start = first + old_text.chars().next().map_or(0, char::len_utf8);
    if original[next_start..].contains(old_text) {
        bail!("old_text appears more than once in {}", path.display());
    }
    let mut updated = String::with_capacity(original.len() - old_text.len() + new_text.len());
    updated.push_str(&original[..first]);
    updated.push_str(new_text);
    updated.push_str(&original[first + old_text.len()..]);
    atomic_write(path, updated.as_bytes()).await
}

async fn atomic_write(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("file has no parent directory: {}", path.display()))?;
    fs::create_dir_all(parent)
        .await
        .with_context(|| format!("cannot create {}", parent.display()))?;
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let temporary = parent.join(format!(".{filename}.{}.tmp", Uuid::now_v7().simple()));
    fs::write(&temporary, content)
        .await
        .with_context(|| format!("cannot write {}", temporary.display()))?;
    if let Ok(metadata) = fs::metadata(path).await {
        fs::set_permissions(&temporary, metadata.permissions()).await?;
    }
    if let Err(error) = fs::rename(&temporary, path).await {
        let _ = fs::remove_file(&temporary).await;
        return Err(error).with_context(|| format!("cannot replace {}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn complete_and_exact_edits_are_atomic_and_reject_ambiguous_matches() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested/file.txt");
        apply_edit(&path, serde_json::json!({"content": "alpha beta"}))
            .await
            .unwrap();
        apply_edit(
            &path,
            serde_json::json!({"old_text": "beta", "new_text": "gamma"}),
        )
        .await
        .unwrap();
        assert_eq!(fs::read_to_string(&path).await.unwrap(), "alpha gamma");

        apply_edit(&path, serde_json::json!({"content": "aaa"}))
            .await
            .unwrap();
        let error = apply_edit(
            &path,
            serde_json::json!({"old_text": "aa", "new_text": "x"}),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("more than once"));
        assert_eq!(fs::read_to_string(path).await.unwrap(), "aaa");
    }
}
