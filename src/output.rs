use crate::prompts;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use tokio::fs;

pub struct OutputStore {
    directory: PathBuf,
    limit: AtomicUsize,
    sequence: AtomicU64,
}

impl OutputStore {
    pub async fn new(session_id: &str, limit: usize) -> Result<Self> {
        let base = dirs::cache_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("uri-agent")
            .join("outputs")
            .join(session_id);
        fs::create_dir_all(&base)
            .await
            .with_context(|| format!("failed to create output directory: {}", base.display()))?;
        let sequence = next_sequence(&base).await?;
        Ok(Self {
            directory: base,
            limit: AtomicUsize::new(limit),
            sequence: AtomicU64::new(sequence),
        })
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn set_limit(&self, limit: usize) {
        self.limit.store(limit.max(1024), Ordering::Relaxed);
    }

    pub fn limit(&self) -> usize {
        self.limit.load(Ordering::Relaxed)
    }

    pub async fn present(&self, content: Vec<u8>, hint: &str) -> Result<String> {
        let limit = self.limit();
        if content.len() <= limit {
            return Ok(String::from_utf8_lossy(&content).into_owned());
        }

        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        let filename = format!("{:06}-{}.txt", sequence, sanitize(hint));
        let path = self.directory.join(filename);
        fs::write(&path, &content)
            .await
            .with_context(|| format!("failed to preserve complete output: {}", path.display()))?;

        let head = limit.saturating_mul(3) / 4;
        let tail = limit.saturating_sub(head);
        let mut preview = String::from_utf8_lossy(&content[..head]).into_owned();
        if tail > 0 {
            preview.push_str("\n…\n");
            preview.push_str(&String::from_utf8_lossy(&content[content.len() - tail..]));
        }
        Ok(prompts::truncated_output(&preview, &path))
    }
}

async fn next_sequence(directory: &Path) -> Result<u64> {
    let mut entries = fs::read_dir(directory).await?;
    let mut next = 0;
    while let Some(entry) = entries.next_entry().await? {
        let filename = entry.file_name();
        let Some(sequence) = filename
            .to_str()
            .and_then(|name| name.split_once('-'))
            .and_then(|(prefix, _)| prefix.parse::<u64>().ok())
        else {
            continue;
        };
        next = next.max(sequence.saturating_add(1));
    }
    Ok(next)
}

fn sanitize(value: &str) -> String {
    let value = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    let value = value.trim_matches('-');
    if value.is_empty() {
        "output".to_string()
    } else {
        value.chars().take(48).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn oversized_output_is_preserved_and_linked() {
        let store = OutputStore {
            directory: tempfile::tempdir().unwrap().keep(),
            limit: AtomicUsize::new(16),
            sequence: AtomicU64::new(0),
        };
        let rendered = store.present(vec![b'x'; 100], "test").await.unwrap();
        assert!(rendered.contains("[output truncated]"));
        let path = store.directory.join("000000-test.txt");
        assert!(rendered.contains(&format!("file://{}", crate::config::display_path(&path))));
        assert_eq!(fs::read(path).await.unwrap(), vec![b'x'; 100]);
    }

    #[tokio::test]
    async fn output_sequence_continues_after_resume() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("000004-old.txt"), b"old")
            .await
            .unwrap();
        assert_eq!(next_sequence(directory.path()).await.unwrap(), 5);
    }
}
