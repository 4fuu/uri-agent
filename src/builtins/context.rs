//! Durable working notes for a session. The state is fed by note events in
//! the append-only session log; the model reaches it through the `notes`
//! tool, and the runtime reads the same events when it injects context.

use crate::compaction::{self, ContextAccuracy, ContextUsage};
use crate::session::{EventKind, Session, SessionEvent};
use anyhow::{Result, anyhow, bail};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use tokio::sync::Mutex;

const MAX_ACTIVE_NOTES: usize = 20;
const NOTE_BUDGET_PERCENT: usize = 20;
const NOTE_WARNING_PERCENT: usize = 15;
const CONTEXT_SAFETY_TOKENS: usize = 4_096;
const MAX_TITLE_CHARS: usize = 120;
const DEFAULT_NOTE_READ_CHARS: usize = 7_000;
const MAX_NOTE_READ_CHARS: usize = 7_000;

#[derive(Clone)]
pub(crate) struct ContextState {
    inner: Arc<ContextStateInner>,
}

struct ContextStateInner {
    session: Session,
    context_window: AtomicUsize,
    base_context_tokens: AtomicUsize,
    usage: RwLock<ContextUsage>,
    note_write: Mutex<()>,
}

impl ContextState {
    pub(crate) fn new(session: Session) -> Self {
        Self {
            inner: Arc::new(ContextStateInner {
                session,
                context_window: AtomicUsize::new(1),
                base_context_tokens: AtomicUsize::new(0),
                usage: RwLock::new(ContextUsage {
                    tokens: 0,
                    accuracy: ContextAccuracy::Estimated,
                }),
                note_write: Mutex::new(()),
            }),
        }
    }

    pub(crate) fn update_meter(
        &self,
        context_window: usize,
        base_context_tokens: usize,
        usage: ContextUsage,
    ) {
        self.inner
            .context_window
            .store(context_window.max(1), Ordering::Release);
        self.inner
            .base_context_tokens
            .store(base_context_tokens, Ordering::Release);
        *self
            .inner
            .usage
            .write()
            .expect("context usage lock poisoned") = usage;
    }

    fn note_budget(&self) -> NoteBudget {
        let context_window = self.inner.context_window.load(Ordering::Acquire).max(1);
        let available = context_window
            .saturating_sub(self.inner.base_context_tokens.load(Ordering::Acquire))
            .saturating_sub(CONTEXT_SAFETY_TOKENS);
        let hard = available.saturating_mul(NOTE_BUDGET_PERCENT) / 100;
        let warning = available.saturating_mul(NOTE_WARNING_PERCENT) / 100;
        NoteBudget { hard, warning }
    }

    async fn events(&self) -> Result<Vec<SessionEvent>> {
        self.inner.session.snapshot().await
    }

    pub(crate) async fn notes_index(&self) -> Result<String> {
        let events = self.events().await?;
        format_notes_index(self, &events)
    }
}

#[derive(Clone, Copy)]
struct NoteBudget {
    hard: usize,
    warning: usize,
}

#[derive(Clone, Debug)]
struct NoteRevision {
    revision: u64,
    title: String,
    content: Option<String>,
    window_id: u64,
    context_sequence: u64,
    deleted: bool,
}

#[derive(Clone, Debug)]
struct NoteRecord {
    id: String,
    title: String,
    revision: u64,
    content: Option<String>,
    window_id: u64,
    context_sequence: u64,
    deleted: bool,
    revisions: Vec<NoteRevision>,
}

impl NoteRecord {
    fn summary_line(&self) -> String {
        let mut line = format!("{} · {} · revision={}", self.id, self.title, self.revision);
        if !self.deleted {
            let _ = write!(
                line,
                " · tokens≈{}",
                estimate_note_tokens(&self.title, self.content.as_deref())
            );
        }
        let _ = write!(
            line,
            " · window={} · anchor={}",
            self.window_id,
            record_id(self.context_sequence)
        );
        if self.deleted {
            line.push_str(" · deleted");
        }
        line
    }
}

fn record_id(sequence: u64) -> String {
    format!("r{sequence}")
}

fn notes_from_events(events: &[SessionEvent]) -> BTreeMap<String, NoteRecord> {
    let mut notes = BTreeMap::new();
    for event in events {
        match &event.kind {
            EventKind::ContextNote {
                id,
                revision,
                title,
                content,
                window_id,
                context_sequence,
            } => apply_note_event(
                &mut notes,
                id,
                NoteRevision {
                    revision: *revision,
                    title: title.clone(),
                    content: Some(content.clone()),
                    window_id: *window_id,
                    context_sequence: *context_sequence,
                    deleted: false,
                },
            ),
            EventKind::ContextNoteDeleted {
                id,
                revision,
                title,
                window_id,
                context_sequence,
            } => apply_note_event(
                &mut notes,
                id,
                NoteRevision {
                    revision: *revision,
                    title: title.clone(),
                    content: None,
                    window_id: *window_id,
                    context_sequence: *context_sequence,
                    deleted: true,
                },
            ),
            _ => {}
        }
    }
    notes
}

fn apply_note_event(notes: &mut BTreeMap<String, NoteRecord>, id: &str, event: NoteRevision) {
    let note = notes.entry(id.to_string()).or_insert_with(|| NoteRecord {
        id: id.to_string(),
        title: String::new(),
        revision: 0,
        content: None,
        window_id: 0,
        context_sequence: 0,
        deleted: false,
        revisions: Vec::new(),
    });
    note.title.clone_from(&event.title);
    note.revision = event.revision;
    note.content.clone_from(&event.content);
    note.window_id = event.window_id;
    note.context_sequence = event.context_sequence;
    note.deleted = event.deleted;
    note.revisions.push(event);
}

fn estimate_note_tokens(title: &str, content: Option<&str>) -> usize {
    compaction::estimate_text_tokens(title).saturating_add(
        content
            .map(compaction::estimate_text_tokens)
            .unwrap_or_default(),
    )
}

fn note_tokens(notes: &BTreeMap<String, NoteRecord>) -> usize {
    notes
        .values()
        .filter(|note| !note.deleted)
        .map(|note| estimate_note_tokens(&note.title, note.content.as_deref()))
        .sum()
}

fn format_notes_index(state: &ContextState, events: &[SessionEvent]) -> Result<String> {
    let notes = notes_from_events(events);
    if notes.is_empty() {
        return Ok(
            "No context notes. Add one with a title when durable working state is needed."
                .to_string(),
        );
    }
    let used = note_tokens(&notes);
    let budget = state.note_budget();
    let mut output = format!(
        "Context notes · approximately {used}/{} tokens · at most {MAX_ACTIVE_NOTES} active\n",
        budget.hard
    );
    for note in notes.values() {
        let _ = writeln!(output, "{}", note.summary_line());
    }
    if budget.warning > 0 && used >= budget.warning {
        output.push_str(
            "Warning: notes have reached the cleanup threshold. Consolidate, shrink, or delete stale entries.\n",
        );
    }
    Ok(output.trim_end().to_string())
}

enum NoteMutation {
    Add { title: String },
    Replace { id: String, title: String },
    Delete { id: String },
}

async fn mutate_note(
    state: &ContextState,
    mutation: NoteMutation,
    content: &str,
) -> Result<String> {
    let _write = state.inner.note_write.lock().await;
    let events = state.events().await?;
    let mut notes = notes_from_events(&events);
    let previous_used = note_tokens(&notes);
    let budget = state.note_budget();
    let window_id = state.inner.session.context_window_id().await;
    let context_sequence = state
        .inner
        .session
        .head_sequence()
        .await
        .unwrap_or_default();

    let (id, revision, title, deleted) = match mutation {
        NoteMutation::Add { title } => {
            if content.trim().is_empty() {
                bail!("context note content must not be empty; use delete for removal");
            }
            if notes.values().filter(|note| !note.deleted).count() >= MAX_ACTIVE_NOTES {
                bail!(
                    "context notes already contain the maximum of {MAX_ACTIVE_NOTES} active entries; delete or replace an existing note"
                );
            }
            let next = notes
                .keys()
                .filter_map(|id| id.strip_prefix('n')?.parse::<u64>().ok())
                .max()
                .unwrap_or_default()
                .saturating_add(1);
            let id = format!("n{next:03}");
            notes.insert(
                id.clone(),
                NoteRecord {
                    id: id.clone(),
                    title: title.clone(),
                    revision: 1,
                    content: Some(content.to_string()),
                    window_id,
                    context_sequence,
                    deleted: false,
                    revisions: Vec::new(),
                },
            );
            (id, 1, title, false)
        }
        NoteMutation::Replace { id, title } => {
            if content.trim().is_empty() {
                bail!("context note content must not be empty; use delete for removal");
            }
            let note = notes
                .get_mut(&id)
                .ok_or_else(|| anyhow!("context note not found: {id}"))?;
            if note.deleted {
                bail!("context note {id} is deleted and cannot be replaced");
            }
            note.title.clone_from(&title);
            note.revision = note.revision.saturating_add(1);
            note.content = Some(content.to_string());
            note.window_id = window_id;
            note.context_sequence = context_sequence;
            (id, note.revision, title, false)
        }
        NoteMutation::Delete { id } => {
            let note = notes
                .get_mut(&id)
                .ok_or_else(|| anyhow!("context note not found: {id}"))?;
            if note.deleted {
                return Ok(note.summary_line());
            }
            note.revision = note.revision.saturating_add(1);
            note.content = None;
            note.deleted = true;
            note.window_id = window_id;
            note.context_sequence = context_sequence;
            (id, note.revision, note.title.clone(), true)
        }
    };

    let used = note_tokens(&notes);
    if !deleted && used > budget.hard && used >= previous_used {
        bail!(
            "context note write would use approximately {used} tokens, above the hard budget of {}; delete or shrink notes and retry",
            budget.hard
        );
    }

    if deleted {
        state
            .inner
            .session
            .append(EventKind::ContextNoteDeleted {
                id: id.clone(),
                revision,
                title: title.clone(),
                window_id,
                context_sequence,
            })
            .await?;
    } else {
        state
            .inner
            .session
            .append(EventKind::ContextNote {
                id: id.clone(),
                revision,
                title: title.clone(),
                content: content.to_string(),
                window_id,
                context_sequence,
            })
            .await?;
    }

    let mut result = if deleted {
        format!(
            "{id} · {title} · revision={revision} · window={window_id} · anchor={} · deleted",
            record_id(context_sequence)
        )
    } else {
        format!(
            "{id} · {title} · revision={revision} · window={window_id} · anchor={} · notes≈{used}/{} tokens",
            record_id(context_sequence),
            budget.hard
        )
    };
    if !deleted && budget.warning > 0 && used >= budget.warning {
        let _ = write!(
            result,
            "\nWarning: notes have reached the cleanup threshold (approximately {used} tokens). Consolidate, shrink, or delete stale notes before adding more."
        );
    }
    Ok(result)
}

pub(crate) async fn notes_list_tool(state: &ContextState) -> Result<String> {
    state.notes_index().await
}

pub(crate) async fn notes_read_tool(
    state: &ContextState,
    id: &str,
    offset: Option<usize>,
    limit: Option<usize>,
) -> Result<String> {
    validate_note_id(id)?;
    let events = state.events().await?;
    let notes = notes_from_events(&events);
    let note = notes
        .get(id)
        .ok_or_else(|| anyhow!("context note not found: {id}"))?;
    if note.deleted {
        return Ok(note.summary_line());
    }
    let content = note.content.as_deref().unwrap_or_default();
    let limit = limit
        .unwrap_or(DEFAULT_NOTE_READ_CHARS)
        .clamp(1, MAX_NOTE_READ_CHARS);
    let (page, next) = character_page(content, offset, limit)?;
    let mut output = format!(
        "{} · {} · revision={} · window={} · anchor={}\n\n{}",
        note.id,
        note.title,
        note.revision,
        note.window_id,
        record_id(note.context_sequence),
        page
    );
    if let Some(offset) = next {
        let _ = write!(output, "\n\nNext: offset {offset}");
    }
    Ok(output)
}

pub(crate) async fn notes_add_tool(
    state: &ContextState,
    title: &str,
    content: &str,
) -> Result<String> {
    let title = validate_note_title(title)?;
    mutate_note(state, NoteMutation::Add { title }, content).await
}

pub(crate) async fn notes_replace_tool(
    state: &ContextState,
    id: &str,
    title: &str,
    content: &str,
) -> Result<String> {
    validate_note_id(id)?;
    let title = validate_note_title(title)?;
    mutate_note(
        state,
        NoteMutation::Replace {
            id: id.to_string(),
            title,
        },
        content,
    )
    .await
}

pub(crate) async fn notes_delete_tool(state: &ContextState, id: &str) -> Result<String> {
    validate_note_id(id)?;
    mutate_note(state, NoteMutation::Delete { id: id.to_string() }, "").await
}

fn validate_note_title(title: &str) -> Result<String> {
    let title = title.trim();
    if title.is_empty() {
        bail!("context note title is required");
    }
    if title.chars().any(char::is_control) {
        bail!("context note title must be one line without control characters");
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        bail!("context note title must not exceed {MAX_TITLE_CHARS} characters");
    }
    Ok(title.to_string())
}

fn validate_note_id(id: &str) -> Result<()> {
    if id.len() < 2
        || !id.starts_with('n')
        || !id[1..].chars().all(|character| character.is_ascii_digit())
    {
        bail!("invalid context note ID: {id}");
    }
    Ok(())
}

fn character_page(
    content: &str,
    offset: Option<usize>,
    limit: usize,
) -> Result<(String, Option<usize>)> {
    let offset = offset.unwrap_or_default();
    let total = content.chars().count();
    if offset > total {
        bail!("context note offset exceeds its content length");
    }
    let page = content.chars().skip(offset).take(limit).collect::<String>();
    let next = (offset.saturating_add(page.chars().count()) < total)
        .then_some(offset.saturating_add(page.chars().count()));
    Ok((page, next))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn note_titles_are_single_line_and_bounded() {
        assert!(validate_note_title("").is_err());
        assert!(validate_note_title("two\nlines").is_err());
        assert!(validate_note_title(&"x".repeat(MAX_TITLE_CHARS + 1)).is_err());
        assert_eq!(
            validate_note_title("  Working state  ").unwrap(),
            "Working state"
        );
    }

    #[test]
    fn character_pages_return_the_next_offset() {
        let (page, next) = character_page("abcdef", Some(1), 3).unwrap();
        assert_eq!(page, "bcd");
        assert_eq!(next, Some(4));
    }

    #[tokio::test]
    async fn notes_round_trip_through_the_session_event_log() {
        let directory = tempfile::tempdir().unwrap();
        let session = Session::open_at(
            directory.path().join("sessions.db"),
            Some("notes"),
            directory.path(),
            "test",
            "model",
            crate::session::SessionContext {
                system_prompt: "system".to_string(),
            },
        )
        .await
        .unwrap();
        let state = ContextState::new(session);
        state.update_meter(
            200_000,
            10_000,
            crate::compaction::ContextUsage {
                tokens: 10_000,
                accuracy: crate::compaction::ContextAccuracy::Estimated,
            },
        );
        let added = notes_add_tool(&state, "Working state", "implemented the text API")
            .await
            .unwrap();
        assert!(added.contains("n001"));
        let listed = notes_list_tool(&state).await.unwrap();
        assert!(listed.contains("Working state"));
        let read = notes_read_tool(&state, "n001", None, None).await.unwrap();
        assert!(read.contains("implemented the text API"));
        notes_replace_tool(&state, "n001", "Working state", "updated")
            .await
            .unwrap();
        let read = notes_read_tool(&state, "n001", None, None).await.unwrap();
        assert!(read.contains("updated"));
        assert!(!read.contains("implemented the text API"));
        notes_delete_tool(&state, "n001").await.unwrap();
        assert!(
            notes_read_tool(&state, "n001", None, None)
                .await
                .unwrap()
                .contains("deleted")
        );
    }

    #[test]
    fn note_summary_uses_the_stable_id_and_anchor() {
        let note = NoteRecord {
            id: "n001".to_string(),
            title: "Working state".to_string(),
            revision: 2,
            content: Some(json!({"ok": true}).to_string()),
            window_id: 1,
            context_sequence: 42,
            deleted: false,
            revisions: Vec::new(),
        };
        let line = note.summary_line();
        assert!(line.contains("n001"));
        assert!(line.contains("anchor=r42"));
        assert!(line.contains("revision=2"));
    }
}
