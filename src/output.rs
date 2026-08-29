use crate::config::display_path;
use crate::prompts;
use anyhow::{Context, Result};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, OnceCell};

pub struct OutputStore {
    directory: PathBuf,
    limit: AtomicUsize,
    sequence: OnceCell<AtomicU64>,
    diagnostic_write: Mutex<()>,
}

impl OutputStore {
    pub async fn new(session_id: &str, limit: usize) -> Result<Self> {
        let base = dirs::cache_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("uri-agent")
            .join("outputs")
            .join(session_id);
        Ok(Self {
            directory: base,
            limit: AtomicUsize::new(limit),
            sequence: OnceCell::new(),
            diagnostic_write: Mutex::new(()),
        })
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn diagnostics_path(&self) -> PathBuf {
        self.directory.join("diagnostics.jsonl")
    }

    pub fn set_limit(&self, limit: usize) {
        self.limit.store(limit.max(1024), Ordering::Relaxed);
    }

    pub fn limit(&self) -> usize {
        self.limit.load(Ordering::Relaxed)
    }

    pub(crate) async fn preserve(&self, content: &[u8], hint: &str) -> Result<PathBuf> {
        let sequence = self
            .sequence
            .get_or_try_init(|| async {
                fs::create_dir_all(&self.directory).await.with_context(|| {
                    format!(
                        "failed to create output directory: {}",
                        display_path(&self.directory)
                    )
                })?;
                Ok::<_, anyhow::Error>(AtomicU64::new(next_sequence(&self.directory).await?))
            })
            .await?
            .fetch_add(1, Ordering::Relaxed);
        let filename = format!("{:06}-{}.txt", sequence, sanitize(hint));
        let path = self.directory.join(filename);
        fs::write(&path, content).await.with_context(|| {
            format!(
                "failed to preserve complete output: {}",
                display_path(&path)
            )
        })?;
        Ok(path)
    }

    pub async fn present(&self, content: Vec<u8>, hint: &str) -> Result<String> {
        let limit = self.limit();
        if content.len() <= limit {
            return Ok(String::from_utf8_lossy(&content).into_owned());
        }

        let content_bytes = content.len();
        let path = self.preserve(&content, hint).await?;

        let head = limit.saturating_mul(3) / 4;
        let tail = limit.saturating_sub(head);
        let mut preview = String::from_utf8_lossy(&content[..head]).into_owned();
        if tail > 0 {
            preview.push_str("\n…\n");
            preview.push_str(&String::from_utf8_lossy(&content[content.len() - tail..]));
        }
        let _ = self
            .record_diagnostic(
                "output_preserved",
                serde_json::json!({
                    "hint": hint,
                    "content_bytes": content_bytes,
                    "inline_limit": limit,
                }),
            )
            .await;
        Ok(prompts::truncated_output(&preview, &path))
    }

    pub(crate) async fn record_diagnostic(&self, event: &str, fields: Value) -> Result<()> {
        let _guard = self.diagnostic_write.lock().await;
        fs::create_dir_all(&self.directory).await.with_context(|| {
            format!(
                "failed to create diagnostic directory: {}",
                display_path(&self.directory)
            )
        })?;
        let mut record = Map::new();
        record.insert(
            "timestamp".to_string(),
            Value::String(chrono::Utc::now().to_rfc3339()),
        );
        record.insert("event".to_string(), Value::String(event.to_string()));
        if let Value::Object(fields) = fields {
            for (name, value) in fields {
                if !record.contains_key(&name) {
                    record.insert(name, value);
                }
            }
        }
        let mut line =
            serde_json::to_vec(&record).context("failed to serialize diagnostic event")?;
        line.push(b'\n');
        let path = self.diagnostics_path();
        let mut options = fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            options.mode(0o600);
        }
        let mut file = options
            .open(&path)
            .await
            .with_context(|| format!("failed to open diagnostic log: {}", display_path(&path)))?;
        file.write_all(&line)
            .await
            .with_context(|| format!("failed to write diagnostic log: {}", display_path(&path)))?;
        file.flush()
            .await
            .with_context(|| format!("failed to flush diagnostic log: {}", display_path(&path)))
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
    async fn output_directory_is_created_only_when_content_is_preserved() {
        let session_id = format!("lazy{}", uuid::Uuid::now_v7().simple());
        let store = OutputStore::new(&session_id, 16).await.unwrap();
        let directory = store.directory().to_path_buf();
        let _ = fs::remove_dir_all(&directory).await;

        assert!(!directory.exists());
        assert_eq!(
            store.present(b"short".to_vec(), "test").await.unwrap(),
            "short"
        );
        assert!(!directory.exists());
        assert!(
            store
                .present(vec![b'x'; 100], "test")
                .await
                .unwrap()
                .contains("[output truncated]")
        );
        assert!(directory.exists());
        let _ = fs::remove_dir_all(directory).await;
    }

    #[tokio::test]
    async fn oversized_output_is_preserved_and_linked() {
        let store = OutputStore {
            directory: tempfile::tempdir().unwrap().keep(),
            limit: AtomicUsize::new(16),
            sequence: OnceCell::new(),
            diagnostic_write: Mutex::new(()),
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

    #[tokio::test]
    async fn diagnostics_are_json_lines_without_raw_tool_content() {
        let store = OutputStore {
            directory: tempfile::tempdir().unwrap().keep(),
            limit: AtomicUsize::new(16),
            sequence: OnceCell::new(),
            diagnostic_write: Mutex::new(()),
        };

        store
            .record_diagnostic(
                "tool_call_finished",
                serde_json::json!({
                    "call_id": "call-1",
                    "tool": "exec",
                    "argument_keys": ["body", "uri"],
                    "failed": false,
                    "output_bytes": 12
                }),
            )
            .await
            .unwrap();

        let log = fs::read_to_string(store.diagnostics_path()).await.unwrap();
        let lines = log.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 1);
        let event: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(event["event"], "tool_call_finished");
        assert_eq!(event["call_id"], "call-1");
        assert_eq!(event["argument_keys"], serde_json::json!(["body", "uri"]));
        assert!(event.get("arguments").is_none());
        assert!(event.get("output").is_none());
        assert!(event["timestamp"].as_str().is_some());
    }
}
