use crate::session::{EventKind, SessionEvent};
use anyhow::{Result, anyhow, bail};
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum RecordType {
    User,
    Assistant,
    ToolCall,
    ToolResult,
    Error,
}

impl RecordType {
    pub(super) const ALL: [Self; 5] = [
        Self::User,
        Self::Assistant,
        Self::ToolCall,
        Self::ToolResult,
        Self::Error,
    ];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
            Self::Error => "error",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "user" => Ok(Self::User),
            "assistant" => Ok(Self::Assistant),
            "tool_call" => Ok(Self::ToolCall),
            "tool_result" => Ok(Self::ToolResult),
            "error" => Ok(Self::Error),
            _ => bail!(
                "record type must be user, assistant, tool_call, tool_result, or error: {value}"
            ),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RecordTypes {
    selected: HashSet<RecordType>,
}

impl RecordTypes {
    pub(super) fn all() -> Self {
        Self {
            selected: RecordType::ALL.into_iter().collect(),
        }
    }

    pub(super) fn messages() -> Self {
        Self {
            selected: [RecordType::User, RecordType::Assistant, RecordType::Error]
                .into_iter()
                .collect(),
        }
    }

    pub(super) fn parse(value: &str) -> Result<Self> {
        if value.is_empty() {
            bail!("record types must not be empty");
        }
        let mut selected = HashSet::new();
        for value in value.split(',') {
            if value.is_empty() {
                bail!("record types must be a comma-separated list without empty entries");
            }
            selected.insert(RecordType::parse(value)?);
        }
        Ok(Self { selected })
    }

    pub(super) fn contains(&self, record_type: RecordType) -> bool {
        self.selected.contains(&record_type)
    }

    pub(super) fn query_value(&self) -> String {
        RecordType::ALL
            .into_iter()
            .filter(|record_type| self.contains(*record_type))
            .map(RecordType::label)
            .collect::<Vec<_>>()
            .join(",")
    }
}

impl Default for RecordTypes {
    fn default() -> Self {
        Self::all()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ConversationRecord {
    pub(super) sequence: u64,
    pub(super) window_id: u64,
    pub(super) record_type: RecordType,
    pub(super) name: Option<String>,
    pub(super) text: String,
    pub(super) failed: bool,
}

impl ConversationRecord {
    pub(super) fn header(&self) -> String {
        let mut output = format!(
            "[{} id={} window={}",
            self.record_type.label(),
            record_id(self.sequence),
            self.window_id
        );
        if let Some(name) = self.name.as_deref() {
            output.push_str(" name=");
            output.push_str(name);
        }
        if self.failed && self.record_type == RecordType::ToolResult {
            output.push_str(" error=true");
        }
        output.push(']');
        output
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct WindowRange {
    pub(super) id: u64,
    pub(super) start: u64,
    pub(super) end: u64,
}

pub(super) fn window_ranges(events: &[SessionEvent]) -> Vec<WindowRange> {
    let mut ranges = Vec::new();
    let mut current_id = 1;
    let mut start = 0;
    for event in events {
        if let EventKind::ContextRollover { window_id, .. } = &event.kind {
            ranges.push(WindowRange {
                id: current_id,
                start,
                end: event.sequence,
            });
            current_id = *window_id;
            start = event.sequence.saturating_add(1);
        }
    }
    ranges.push(WindowRange {
        id: current_id,
        start,
        end: events
            .last()
            .map_or(start, |event| event.sequence.saturating_add(1)),
    });
    ranges
}

pub(super) fn conversation_records(
    events: &[SessionEvent],
    types: &RecordTypes,
) -> Vec<ConversationRecord> {
    let private_calls = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::ToolCall {
                call_id, arguments, ..
            } if arguments
                .get("uri")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|uri| uri.starts_with("context://")) =>
            {
                Some(call_id.clone())
            }
            _ => None,
        })
        .collect::<HashSet<_>>();
    let ranges = window_ranges(events);

    events
        .iter()
        .filter_map(|event| {
            let (record_type, name, text, failed) = match &event.kind {
                EventKind::User { text } => (RecordType::User, None, text.clone(), false),
                EventKind::AssistantText { text } => {
                    (RecordType::Assistant, None, text.clone(), false)
                }
                EventKind::Error { text } => (RecordType::Error, None, text.clone(), true),
                EventKind::ToolCall {
                    call_id,
                    name,
                    arguments,
                } if !private_calls.contains(call_id) => (
                    RecordType::ToolCall,
                    Some(name.clone()),
                    serde_json::to_string(arguments)
                        .unwrap_or_else(|_| "[unserializable arguments]".to_string()),
                    false,
                ),
                EventKind::ToolResult {
                    call_id,
                    name,
                    output,
                    failed,
                    ..
                } if !private_calls.contains(call_id) => (
                    RecordType::ToolResult,
                    Some(name.clone()),
                    output.clone(),
                    *failed,
                ),
                _ => return None,
            };
            if !types.contains(record_type) || text.trim().is_empty() {
                return None;
            }
            let window_id = ranges
                .iter()
                .find(|range| range.start <= event.sequence && event.sequence < range.end)
                .map_or(1, |range| range.id);
            Some(ConversationRecord {
                sequence: event.sequence,
                window_id,
                record_type,
                name,
                text,
                failed,
            })
        })
        .collect()
}

pub(super) fn record_id(sequence: u64) -> String {
    format!("r{sequence}")
}

pub(super) fn parse_record_id(value: &str) -> Result<u64> {
    let value = value.strip_prefix('r').unwrap_or(value);
    if value.is_empty() || !value.chars().all(|character| character.is_ascii_digit()) {
        return Err(anyhow!("record ID must use the form r<number>"));
    }
    value
        .parse()
        .map_err(|_| anyhow!("record ID is outside the supported range"))
}

pub(super) fn validate_anchor(events: &[SessionEvent], anchor: u64) -> Result<()> {
    if events.iter().any(|event| event.sequence == anchor) {
        Ok(())
    } else {
        bail!("record anchor not found: {}", record_id(anchor))
    }
}

pub(super) fn records_around(
    records: &[ConversationRecord],
    anchor: u64,
    before: usize,
    after: usize,
) -> Vec<ConversationRecord> {
    let anchor_start = records.partition_point(|record| record.sequence < anchor);
    let anchor_end = records.partition_point(|record| record.sequence <= anchor);
    let start = anchor_start.saturating_sub(before);
    let end = anchor_end.saturating_add(after).min(records.len());
    records[start..end].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn records_share_ids_types_windows_and_private_context_filtering() {
        let at = Utc::now();
        let events = vec![
            SessionEvent {
                sequence: 1,
                at,
                kind: EventKind::User {
                    text: "first".to_string(),
                },
            },
            SessionEvent {
                sequence: 2,
                at,
                kind: EventKind::ContextRollover {
                    window_id: 2,
                    tokens_before: 100,
                    replacement_history: Vec::new(),
                    manual: false,
                },
            },
            SessionEvent {
                sequence: 3,
                at,
                kind: EventKind::ToolCall {
                    call_id: "private".to_string(),
                    name: "exec".to_string(),
                    arguments: serde_json::json!({
                        "uri": "context://notes/add?title=Secret",
                        "body": "deleted body"
                    }),
                },
            },
            SessionEvent {
                sequence: 4,
                at,
                kind: EventKind::ToolResult {
                    call_id: "private".to_string(),
                    name: "exec".to_string(),
                    output: "private result".to_string(),
                    failed: false,
                    protocol_help_required: false,
                },
            },
            SessionEvent {
                sequence: 5,
                at,
                kind: EventKind::ToolResult {
                    call_id: "public".to_string(),
                    name: "read".to_string(),
                    output: "public result".to_string(),
                    failed: true,
                    protocol_help_required: false,
                },
            },
        ];

        let records = conversation_records(&events, &RecordTypes::all());
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].header(), "[user id=r1 window=1]");
        assert_eq!(
            records[1].header(),
            "[tool_result id=r5 window=2 name=read error=true]"
        );
        assert!(!records.iter().any(|record| record.text.contains("deleted")));
    }

    #[test]
    fn record_type_filters_and_around_counts_are_stable() {
        let types = RecordTypes::parse("user,tool_result").unwrap();
        assert!(types.contains(RecordType::User));
        assert!(types.contains(RecordType::ToolResult));
        assert!(!types.contains(RecordType::Assistant));
        assert_eq!(types.query_value(), "user,tool_result");
        assert_eq!(parse_record_id("r42").unwrap(), 42);
        assert_eq!(parse_record_id("42").unwrap(), 42);
        assert!(parse_record_id("note-42").is_err());

        let records = (1..=5)
            .map(|sequence| ConversationRecord {
                sequence,
                window_id: 1,
                record_type: RecordType::User,
                name: None,
                text: sequence.to_string(),
                failed: false,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            records_around(&records, 3, 1, 1)
                .iter()
                .map(|record| record.sequence)
                .collect::<Vec<_>>(),
            [2, 3, 4]
        );
    }
}
