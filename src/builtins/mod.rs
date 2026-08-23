mod agents;
mod apply_patch;
mod bin_hints;
mod file;
mod https;
mod replace;
mod shell;
mod uri_agent_docs;

use crate::plugin::PluginRegistry;
use crate::task::{TaskManager, TaskRecord};
use anyhow::{Context, Result, anyhow, bail};
use std::fmt::Write as _;
use std::path::Path;
use tokio::fs;
use uuid::Uuid;

pub fn plugins(cwd: &Path) -> PluginRegistry {
    let mut plugins = PluginRegistry::new();
    plugins.add(agents::AgentsPlugin::new(cwd));
    plugins.add(bin_hints::BinHintsPlugin);
    plugins.add(uri_agent_docs::UriAgentDocsProtocol);
    plugins.add(file::FileProtocol::new(cwd));
    plugins.add(https::HttpsProtocol::new());
    plugins.add(replace::ReplaceProtocol::new(cwd));
    plugins.add(apply_patch::ApplyPatchProtocol::new(cwd));
    shell::add_plugins(&mut plugins, cwd);
    plugins
}

async fn render_task(tasks: &TaskManager, protocol: &str, id: &str) -> Result<Vec<u8>> {
    let record = tasks
        .get(id)
        .await
        .ok_or_else(|| anyhow!("task not found: {id}"))?;
    if record.protocol != protocol {
        bail!("task {id} belongs to {}://", record.protocol);
    }
    Ok(render_record(&record).into_bytes())
}

async fn render_task_list(tasks: &TaskManager, protocol: &str) -> Vec<u8> {
    let records = tasks.list().await;
    let mut output = String::new();
    for record in records
        .into_iter()
        .filter(|record| record.protocol == protocol)
    {
        let _ = writeln!(
            output,
            "{}  {:9}  {}",
            record.id,
            record.status.as_str(),
            record.label
        );
    }
    if output.is_empty() {
        output.push_str("No tasks.\n");
    }
    output.into_bytes()
}

fn render_record(record: &TaskRecord) -> String {
    let finished = record
        .finished_at
        .map(|value| value.to_rfc3339())
        .unwrap_or_else(|| "—".to_string());
    let mut output = format!(
        "Task: {}\nStatus: {}\nLabel: {}\nStarted: {}\nFinished: {}\n",
        record.id,
        record.status.as_str(),
        record.label,
        record.started_at.to_rfc3339(),
        finished
    );
    if !record.content.is_empty() {
        output.push('\n');
        output.push_str(&String::from_utf8_lossy(&record.content));
    }
    output
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

    #[test]
    fn built_in_distribution_declares_document_file_web_and_edit_plugins() {
        let directory = tempfile::tempdir().unwrap();
        let plugins = plugins(directory.path());
        let names = plugins
            .protocol_descriptors()
            .unwrap()
            .into_iter()
            .map(|descriptor| descriptor.name)
            .collect::<Vec<_>>();

        assert!(names.iter().any(|name| name == "uri-agent-docs"));
        assert!(names.iter().any(|name| name == "file"));
        assert!(names.iter().any(|name| name == "https"));
        assert!(names.iter().any(|name| name == "replace"));
        assert!(names.iter().any(|name| name == "apply_patch"));
        assert!(!names.iter().any(|name| name == "edit"));
    }
}
