// MCP owns its settings workflow. The TUI only renders semantic rows and
// forwards input, so neither configuration nor transport behavior leaks into
// the core interface.

use super::config::{
    EffectiveServer, McpScope, McpServerConfig, McpTransportConfig, protocol_name,
};
use super::runtime::McpRuntime;
use crate::plugin::{
    TuiPanelContext, TuiPanelControl, TuiPanelEvent, TuiPanelHint, TuiPanelProvider, TuiPanelRow,
    TuiPanelSession, TuiPanelTone, TuiPanelView, TuiPanelWake,
};
use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub(super) struct McpPanelProvider {
    pub(super) runtime: Arc<McpRuntime>,
}

#[async_trait]
impl TuiPanelProvider for McpPanelProvider {
    async fn open(&self, context: TuiPanelContext) -> Result<Box<dyn TuiPanelSession>> {
        Ok(Box::new(
            McpPanel::load(self.runtime.clone(), context.wake).await?,
        ))
    }
}

#[derive(Clone, Default)]
pub(super) struct PanelText {
    value: String,
    cursor: usize,
}

impl PanelText {
    pub(super) fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        let cursor = value.chars().count();
        Self { value, cursor }
    }

    fn insert(&mut self, value: &str) {
        let value = value
            .chars()
            .map(|character| {
                if matches!(character, '\r' | '\n') {
                    ' '
                } else {
                    character
                }
            })
            .collect::<String>();
        let byte = character_byte(&self.value, self.cursor);
        self.value.insert_str(byte, &value);
        self.cursor += value.chars().count();
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = character_byte(&self.value, self.cursor - 1);
        let end = character_byte(&self.value, self.cursor);
        self.value.replace_range(start..end, "");
        self.cursor -= 1;
    }

    fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.value.chars().count());
    }
}

fn character_byte(value: &str, character: usize) -> usize {
    value
        .char_indices()
        .nth(character)
        .map_or(value.len(), |(byte, _)| byte)
}

#[derive(Clone, Default)]
pub(super) struct PanelPair {
    key: PanelText,
    value: PanelText,
}

impl PanelPair {
    fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: PanelText::new(key),
            value: PanelText::new(value),
        }
    }
}

#[derive(Clone)]
pub(super) enum McpDraftTransport {
    Stdio {
        command: PanelText,
        cwd: PanelText,
        args: Vec<PanelText>,
        environment: Vec<PanelPair>,
    },
    StreamableHttp {
        url: PanelText,
        headers: Vec<PanelPair>,
    },
}

impl McpDraftTransport {
    fn label(&self) -> &'static str {
        match self {
            Self::Stdio { .. } => "stdio",
            Self::StreamableHttp { .. } => "Streamable HTTP",
        }
    }

    fn toggle(&mut self) {
        *self = match self {
            Self::Stdio { .. } => Self::StreamableHttp {
                url: PanelText::new("https://"),
                headers: Vec::new(),
            },
            Self::StreamableHttp { .. } => Self::Stdio {
                command: PanelText::default(),
                cwd: PanelText::default(),
                args: Vec::new(),
                environment: Vec::new(),
            },
        };
    }
}

#[derive(Clone)]
struct DraftOrigin {
    name: String,
    scope: McpScope,
}

#[derive(Clone)]
pub(super) struct McpDraft {
    origin: Option<DraftOrigin>,
    pub(super) name: PanelText,
    pub(super) description: PanelText,
    pub(super) scope: McpScope,
    enabled: bool,
    pub(super) transport: McpDraftTransport,
    selected: usize,
}

impl McpDraft {
    pub(super) fn new() -> Self {
        Self {
            origin: None,
            name: PanelText::default(),
            description: PanelText::default(),
            scope: McpScope::Project,
            enabled: true,
            transport: McpDraftTransport::Stdio {
                command: PanelText::default(),
                cwd: PanelText::default(),
                args: Vec::new(),
                environment: Vec::new(),
            },
            selected: 0,
        }
    }

    pub(super) fn from_server(server: &EffectiveServer, config: McpServerConfig) -> Self {
        let transport = match config.transport {
            McpTransportConfig::Stdio {
                command,
                args,
                cwd,
                environment,
            } => McpDraftTransport::Stdio {
                command: PanelText::new(command),
                cwd: PanelText::new(
                    cwd.map(|path| path.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                ),
                args: args.into_iter().map(PanelText::new).collect(),
                environment: environment
                    .into_iter()
                    .map(|(key, value)| PanelPair::new(key, value))
                    .collect(),
            },
            McpTransportConfig::StreamableHttp { url, headers } => {
                McpDraftTransport::StreamableHttp {
                    url: PanelText::new(url),
                    headers: headers
                        .into_iter()
                        .map(|(key, value)| PanelPair::new(key, value))
                        .collect(),
                }
            }
        };
        Self {
            origin: Some(DraftOrigin {
                name: server.name.clone(),
                scope: server.scope,
            }),
            name: PanelText::new(&server.name),
            description: PanelText::new(config.description),
            scope: server.scope,
            enabled: config.enabled,
            transport,
            selected: 0,
        }
    }

    fn title(&self) -> String {
        self.origin.as_ref().map_or_else(
            || "ADD MCP SERVER".to_string(),
            |_| "EDIT MCP SERVER".to_string(),
        )
    }

    fn rows(&self) -> Vec<TuiPanelRow> {
        let mut rows = vec![
            self.text_row(
                "name",
                "Name",
                &self.name,
                self.origin
                    .as_ref()
                    .map(|_| "fixed after creation")
                    .unwrap_or("becomes <name>-mcp://"),
                self.origin.is_none(),
            ),
        ];
        rows.push(self.text_row(
            "description",
            "Description",
            &self.description,
            "required; included in the frozen protocol prompt",
            true,
        ));
        rows.push(
            TuiPanelRow::item("scope", "Scope", self.scope.label())
                .description("Project overrides the same User name"),
        );
        rows.push(TuiPanelRow::item(
            "enabled",
            "Enabled",
            if self.enabled { "yes" } else { "no" },
        ));
        rows.push(
            TuiPanelRow::item("transport", "Transport", self.transport.label())
                .description("Enter or ←/→ to switch"),
        );
        match &self.transport {
            McpDraftTransport::Stdio {
                command,
                cwd,
                args,
                environment,
            } => {
                rows.push(self.text_row("command", "Command", command, "executable", true));
                rows.push(self.text_row(
                    "cwd",
                    "Working directory",
                    cwd,
                    "empty = project; absolute or project-relative",
                    true,
                ));
                rows.push(
                    TuiPanelRow::item("add-arg", "Add argument", "Ctrl+N or Enter")
                        .tone(TuiPanelTone::Accent),
                );
                for (index, argument) in args.iter().enumerate() {
                    rows.push(self.text_row(
                        &format!("arg-{index}"),
                        &format!("Argument {}", index + 1),
                        argument,
                        "one process argument",
                        true,
                    ));
                }
                rows.push(
                    TuiPanelRow::item("add-env", "Add environment", "Ctrl+N or Enter")
                        .description("maps a process variable to Agent Environment")
                        .tone(TuiPanelTone::Accent),
                );
                for (index, pair) in environment.iter().enumerate() {
                    rows.push(self.text_row(
                        &format!("env-key-{index}"),
                        &format!("Environment {}", index + 1),
                        &pair.key,
                        "process variable name",
                        true,
                    ));
                    rows.push(self.text_row(
                        &format!("env-value-{index}"),
                        "Agent Environment",
                        &pair.value,
                        "saved reference, not a secret value",
                        true,
                    ));
                }
            }
            McpDraftTransport::StreamableHttp { url, headers } => {
                rows.push(self.text_row("url", "URL", url, "HTTPS, or HTTP for loopback", true));
                rows.push(
                    TuiPanelRow::item("add-header", "Add header", "Ctrl+N or Enter")
                        .description("credential values should use ${NAME}")
                        .tone(TuiPanelTone::Accent),
                );
                for (index, pair) in headers.iter().enumerate() {
                    rows.push(self.text_row(
                        &format!("header-key-{index}"),
                        &format!("Header {}", index + 1),
                        &pair.key,
                        "HTTP header name",
                        true,
                    ));
                    rows.push(self.text_row(
                        &format!("header-value-{index}"),
                        "Header template",
                        &pair.value,
                        "use ${AGENT_ENVIRONMENT_NAME} for credentials",
                        true,
                    ));
                }
            }
        }
        rows.push(
            TuiPanelRow::item("review", "Review & test", "Enter")
                .description("validates and connects before saving")
                .tone(TuiPanelTone::Accent),
        );
        if let Some(row) = rows.get_mut(self.selected) {
            row.cursor = self.text_cursor(&row.id);
        }
        rows
    }

    fn text_row(
        &self,
        id: &str,
        label: &str,
        field: &PanelText,
        description: &str,
        editable: bool,
    ) -> TuiPanelRow {
        TuiPanelRow::item(id, label, &field.value)
            .description(description)
            .selectable(editable)
            .tone(if editable {
                TuiPanelTone::Default
            } else {
                TuiPanelTone::Muted
            })
    }

    fn text_cursor(&self, id: &str) -> Option<usize> {
        self.field(id).map(|field| field.cursor)
    }

    fn field(&self, id: &str) -> Option<&PanelText> {
        match id {
            "name" if self.origin.is_none() => Some(&self.name),
            "description" => Some(&self.description),
            _ => transport_field(&self.transport, id),
        }
    }

    fn row_id(&self) -> Option<String> {
        self.rows().get(self.selected).map(|row| row.id.clone())
    }

    fn move_selection(&mut self, distance: isize) {
        let count = self.rows().len();
        if distance < 0 {
            self.selected = self.selected.saturating_sub(distance.unsigned_abs());
        } else {
            self.selected = (self.selected + distance as usize).min(count.saturating_sub(1));
        }
    }

    fn selected_text_mut(&mut self) -> Option<&mut PanelText> {
        let id = self.row_id()?;
        self.field_mut(&id)
    }

    fn field_mut(&mut self, id: &str) -> Option<&mut PanelText> {
        match id {
            "name" if self.origin.is_none() => Some(&mut self.name),
            "description" => Some(&mut self.description),
            _ => transport_field_mut(&mut self.transport, id),
        }
    }

    fn toggle_selected(&mut self) -> bool {
        match self.row_id().as_deref() {
            Some("scope") => self.scope = self.scope.other(),
            Some("enabled") => self.enabled = !self.enabled,
            Some("transport") => self.transport.toggle(),
            _ => return false,
        }
        self.selected = self.selected.min(self.rows().len().saturating_sub(1));
        true
    }

    fn add_dynamic(&mut self) {
        let selected = self.row_id().unwrap_or_default();
        let target = match &mut self.transport {
            McpDraftTransport::Stdio { environment, .. }
                if selected == "add-env" || selected.starts_with("env-") =>
            {
                environment.push(PanelPair::default());
                format!("env-key-{}", environment.len() - 1)
            }
            McpDraftTransport::Stdio { args, .. } => {
                args.push(PanelText::default());
                format!("arg-{}", args.len() - 1)
            }
            McpDraftTransport::StreamableHttp { headers, .. } => {
                headers.push(PanelPair::default());
                format!("header-key-{}", headers.len() - 1)
            }
        };
        self.selected = self
            .rows()
            .iter()
            .position(|row| row.id == target)
            .unwrap_or(self.selected);
    }

    fn remove_dynamic(&mut self) -> bool {
        let Some(id) = self.row_id() else {
            return false;
        };
        let removed = match &mut self.transport {
            McpDraftTransport::Stdio {
                args, environment, ..
            } => {
                if let Some(index) = dynamic_index(&id, "arg-") {
                    (index < args.len()).then(|| args.remove(index)).is_some()
                } else if let Some(index) =
                    dynamic_index(&id, "env-key-").or_else(|| dynamic_index(&id, "env-value-"))
                {
                    (index < environment.len())
                        .then(|| environment.remove(index))
                        .is_some()
                } else {
                    false
                }
            }
            McpDraftTransport::StreamableHttp { headers, .. } => {
                let index = dynamic_index(&id, "header-key-")
                    .or_else(|| dynamic_index(&id, "header-value-"));
                index
                    .filter(|index| *index < headers.len())
                    .map(|index| headers.remove(index))
                    .is_some()
            }
        };
        if removed {
            self.selected = self.selected.min(self.rows().len().saturating_sub(1));
        }
        removed
    }

    fn build(&self) -> Result<(String, McpServerConfig)> {
        let name = self.name.value.trim().to_string();
        protocol_name(&name)?;
        let transport = match &self.transport {
            McpDraftTransport::Stdio {
                command,
                cwd,
                args,
                environment,
            } => McpTransportConfig::Stdio {
                command: command.value.clone(),
                args: args.iter().map(|argument| argument.value.clone()).collect(),
                cwd: (!cwd.value.trim().is_empty()).then(|| PathBuf::from(cwd.value.trim())),
                environment: pair_map(environment, "environment", true)?,
            },
            McpDraftTransport::StreamableHttp { url, headers } => {
                McpTransportConfig::StreamableHttp {
                    url: url.value.trim().to_string(),
                    headers: pair_map(headers, "header", false)?,
                }
            }
        };
        let config = McpServerConfig {
            description: self.description.value.trim().to_string(),
            enabled: self.enabled,
            transport,
        };
        config.validate(&name)?;
        Ok((name, config))
    }
}

fn dynamic_index(id: &str, prefix: &str) -> Option<usize> {
    id.strip_prefix(prefix)?.parse().ok()
}

fn transport_field_mut<'a>(
    transport: &'a mut McpDraftTransport,
    id: &str,
) -> Option<&'a mut PanelText> {
    match transport {
        McpDraftTransport::Stdio {
            command,
            cwd,
            args,
            environment,
        } => match id {
            "command" => Some(command),
            "cwd" => Some(cwd),
            _ => {
                if let Some(index) = dynamic_index(id, "arg-") {
                    return args.get_mut(index);
                }
                if let Some(index) = dynamic_index(id, "env-key-") {
                    return environment.get_mut(index).map(|pair| &mut pair.key);
                }
                dynamic_index(id, "env-value-")
                    .and_then(|index| environment.get_mut(index))
                    .map(|pair| &mut pair.value)
            }
        },
        McpDraftTransport::StreamableHttp { url, headers } => match id {
            "url" => Some(url),
            _ => {
                if let Some(index) = dynamic_index(id, "header-key-") {
                    return headers.get_mut(index).map(|pair| &mut pair.key);
                }
                dynamic_index(id, "header-value-")
                    .and_then(|index| headers.get_mut(index))
                    .map(|pair| &mut pair.value)
            }
        },
    }
}

fn transport_field<'a>(transport: &'a McpDraftTransport, id: &str) -> Option<&'a PanelText> {
    match transport {
        McpDraftTransport::Stdio {
            command,
            cwd,
            args,
            environment,
        } => match id {
            "command" => Some(command),
            "cwd" => Some(cwd),
            _ => {
                if let Some(index) = dynamic_index(id, "arg-") {
                    return args.get(index);
                }
                if let Some(index) = dynamic_index(id, "env-key-") {
                    return environment.get(index).map(|pair| &pair.key);
                }
                dynamic_index(id, "env-value-")
                    .and_then(|index| environment.get(index))
                    .map(|pair| &pair.value)
            }
        },
        McpDraftTransport::StreamableHttp { url, headers } => match id {
            "url" => Some(url),
            _ => {
                if let Some(index) = dynamic_index(id, "header-key-") {
                    return headers.get(index).map(|pair| &pair.key);
                }
                dynamic_index(id, "header-value-")
                    .and_then(|index| headers.get(index))
                    .map(|pair| &pair.value)
            }
        },
    }
}

fn pair_map(
    pairs: &[PanelPair],
    label: &str,
    trim_value: bool,
) -> Result<BTreeMap<String, String>> {
    let mut values = BTreeMap::new();
    for (index, pair) in pairs.iter().enumerate() {
        let key = pair.key.value.trim();
        let value = if trim_value {
            pair.value.value.trim()
        } else {
            pair.value.value.as_str()
        };
        if key.is_empty() || value.is_empty() {
            bail!("MCP {label} {} requires both fields", index + 1);
        }
        if values.insert(key.to_string(), value.to_string()).is_some() {
            bail!("duplicate MCP {label} name {key:?}");
        }
    }
    Ok(values)
}

#[derive(Clone)]
pub(super) struct McpReview {
    pub(super) draft: McpDraft,
    pub(super) name: String,
    pub(super) config: McpServerConfig,
    pub(super) test: McpReviewTest,
    selected: usize,
}

#[derive(Clone)]
pub(super) enum McpReviewTest {
    Running,
    Finished(Result<(), String>),
}

impl McpReview {
    fn rows(&self) -> Vec<TuiPanelRow> {
        let mut rows = vec![
            TuiPanelRow::item("name", "Name", &self.name),
            TuiPanelRow::item(
                "protocol",
                "Protocol",
                format!(
                    "{}://",
                    protocol_name(&self.name).unwrap_or_else(|_| "invalid-mcp".to_string())
                ),
            ),
            TuiPanelRow::item("description", "Description", &self.config.description),
            TuiPanelRow::item("scope", "Scope", self.draft.scope.label()),
            TuiPanelRow::item(
                "enabled",
                "Enabled",
                if self.config.enabled { "yes" } else { "no" },
            ),
            TuiPanelRow::item("transport", "Transport", self.config.transport_label()),
        ];
        match &self.config.transport {
            McpTransportConfig::Stdio {
                command,
                args,
                cwd,
                environment,
            } => {
                rows.push(TuiPanelRow::item("command", "Command", command));
                rows.push(TuiPanelRow::item(
                    "cwd",
                    "Working directory",
                    cwd.as_ref()
                        .map(|path| path.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "project directory".to_string()),
                ));
                for (index, argument) in args.iter().enumerate() {
                    rows.push(TuiPanelRow::item(
                        format!("arg-{index}"),
                        format!("Argument {}", index + 1),
                        argument,
                    ));
                }
                for (target, source) in environment {
                    rows.push(
                        TuiPanelRow::item(
                            format!("environment-{target}"),
                            target,
                            format!("Agent Environment: {source}"),
                        )
                        .tone(TuiPanelTone::Muted),
                    );
                }
            }
            McpTransportConfig::StreamableHttp { url, headers } => {
                rows.push(TuiPanelRow::item("url", "URL", url));
                for (header, template) in headers {
                    rows.push(TuiPanelRow::item(
                        format!("header-{header}"),
                        header,
                        template,
                    ));
                }
            }
        }
        rows.push(match &self.test {
            McpReviewTest::Running => {
                TuiPanelRow::item("test", "Connection test", "testing…").tone(TuiPanelTone::Muted)
            }
            McpReviewTest::Finished(Ok(())) => {
                TuiPanelRow::item("test", "Connection test", "passed").tone(TuiPanelTone::Accent)
            }
            McpReviewTest::Finished(Err(error)) => {
                TuiPanelRow::item("test", "Connection test", "failed")
                    .description(error)
                    .tone(TuiPanelTone::Error)
            }
        });
        rows
    }
}

#[derive(Clone)]
pub(super) struct McpDelete {
    server: EffectiveServer,
    resurfaces_user: bool,
}

#[derive(Clone)]
pub(super) enum McpPanelMode {
    List,
    Edit(McpDraft),
    Review(McpReview),
    ConfirmDelete(McpDelete),
}

struct McpPanelUpdate {
    generation: u64,
    kind: McpPanelUpdateKind,
}

enum McpPanelUpdateKind {
    TestSelected {
        name: String,
        result: Result<(), String>,
    },
    Reconnect {
        name: String,
        result: Result<(), String>,
    },
    ReviewTest {
        name: String,
        result: Result<(), String>,
    },
}

pub(super) struct McpPanelPending {
    generation: u64,
    cancellation: CancellationToken,
}

pub(super) struct McpPanel {
    runtime: Arc<McpRuntime>,
    pub(super) session_scoped: bool,
    pub(super) servers: Vec<EffectiveServer>,
    selected: usize,
    pub(super) mode: McpPanelMode,
    pub(super) message: Option<(String, TuiPanelTone)>,
    wake: TuiPanelWake,
    updates_tx: mpsc::UnboundedSender<McpPanelUpdate>,
    updates_rx: mpsc::UnboundedReceiver<McpPanelUpdate>,
    next_generation: u64,
    pub(super) pending: Option<McpPanelPending>,
}

impl McpPanel {
    pub(super) async fn load(runtime: Arc<McpRuntime>, wake: TuiPanelWake) -> Result<Self> {
        let session_scoped = runtime.resolver.session_scoped();
        let servers = runtime.effective_servers().await?;
        runtime.refresh_configured(servers.len());
        let (updates_tx, updates_rx) = mpsc::unbounded_channel();
        Ok(Self {
            runtime,
            session_scoped,
            servers,
            selected: 0,
            mode: McpPanelMode::List,
            message: None,
            wake,
            updates_tx,
            updates_rx,
            next_generation: 0,
            pending: None,
        })
    }

    fn start_operation<F>(&mut self, future: F)
    where
        F: Future<Output = McpPanelUpdateKind> + Send + 'static,
    {
        self.cancel_pending();
        self.next_generation = self.next_generation.wrapping_add(1);
        let generation = self.next_generation;
        let cancellation = CancellationToken::new();
        self.pending = Some(McpPanelPending {
            generation,
            cancellation: cancellation.clone(),
        });
        let updates = self.updates_tx.clone();
        let wake = self.wake.clone();
        tokio::spawn(async move {
            let kind = tokio::select! {
                _ = cancellation.cancelled() => return,
                kind = future => kind,
            };
            if updates.send(McpPanelUpdate { generation, kind }).is_ok() {
                wake.wake();
            }
        });
    }

    fn cancel_pending(&mut self) {
        if let Some(pending) = self.pending.take() {
            pending.cancellation.cancel();
        }
    }

    fn drain_updates(&mut self) {
        while let Ok(update) = self.updates_rx.try_recv() {
            if self
                .pending
                .as_ref()
                .is_none_or(|pending| pending.generation != update.generation)
            {
                continue;
            }
            self.pending = None;
            match update.kind {
                McpPanelUpdateKind::TestSelected { name, result } => {
                    self.message = Some(match result {
                        Ok(()) => (
                            format!("{name}: connection test passed"),
                            TuiPanelTone::Accent,
                        ),
                        Err(error) => (format!("{name}: {error}"), TuiPanelTone::Error),
                    });
                }
                McpPanelUpdateKind::Reconnect { name, result } => {
                    self.message = Some(match result {
                        Ok(()) => (format!("{name}: reconnected"), TuiPanelTone::Accent),
                        Err(error) => (format!("{name}: {error}"), TuiPanelTone::Error),
                    });
                }
                McpPanelUpdateKind::ReviewTest { name, result } => {
                    let McpPanelMode::Review(review) = &mut self.mode else {
                        continue;
                    };
                    if review.name != name {
                        continue;
                    }
                    self.message = Some(match &result {
                        Ok(()) => ("Connection test passed".to_string(), TuiPanelTone::Accent),
                        Err(error) => (
                            format!("Test failed; saving is still available: {error}"),
                            TuiPanelTone::Warning,
                        ),
                    });
                    review.test = McpReviewTest::Finished(result);
                }
            }
        }
    }

    async fn reload(&mut self) -> Result<()> {
        self.servers = self.runtime.effective_servers().await?;
        self.runtime.refresh_configured(self.servers.len());
        self.selected = self.selected.min(self.servers.len().saturating_sub(1));
        Ok(())
    }

    fn list_view(&self) -> TuiPanelView {
        let rows = if self.servers.is_empty() {
            vec![
                TuiPanelRow::item(
                    "empty",
                    if self.session_scoped {
                        "No session MCP servers"
                    } else {
                        "No MCP servers"
                    },
                    if self.session_scoped {
                        "Managed by the ACP client"
                    } else {
                        "Ctrl+N to add one"
                    },
                )
                .selectable(false)
                .tone(TuiPanelTone::Muted),
            ]
        } else {
            self.servers
                .iter()
                .map(|server| match server.parse() {
                    Ok(config) => {
                        let (connection, connection_tone, error) =
                            match self.runtime.connection_status(&server.name) {
                                Some(Ok(())) => ("connected", TuiPanelTone::Accent, None),
                                Some(Err(error)) => {
                                    ("connection failed", TuiPanelTone::Error, Some(error))
                                }
                                None => ("not connected", TuiPanelTone::Default, None),
                            };
                        let mut description = config.description.clone();
                        if let Some(error) = error {
                            description.push_str(" · ");
                            description.push_str(&error);
                        }
                        TuiPanelRow::item(
                            server.name.clone(),
                            &server.name,
                            format!(
                                "{} · {} · {} · {connection}",
                                if self.session_scoped {
                                    "Session"
                                } else {
                                    server.scope.label()
                                },
                                config.transport_label(),
                                if config.enabled {
                                    "enabled"
                                } else {
                                    "disabled"
                                },
                            ),
                        )
                        .description(description)
                        .tone(if !config.enabled {
                            TuiPanelTone::Muted
                        } else {
                            connection_tone
                        })
                    }
                    Err(error) => TuiPanelRow::item(
                        server.name.clone(),
                        &server.name,
                        format!("{} · invalid", server.scope.label()),
                    )
                    .description(error.to_string())
                    .tone(TuiPanelTone::Error),
                })
                .collect()
        };
        TuiPanelView {
            title: "MCP SERVERS".to_string(),
            selected: (!self.servers.is_empty()).then_some(self.selected),
            rows,
            message: self.message.clone(),
            hints: if self.session_scoped {
                vec![
                    TuiPanelHint::new("T", "test").action("test"),
                    TuiPanelHint::new("R", "reconnect").action("reconnect"),
                    TuiPanelHint::new("Esc", "close").action("close"),
                ]
            } else {
                vec![
                    TuiPanelHint::new("Ctrl+N", "add").action("add"),
                    TuiPanelHint::new("Enter", "edit").action("confirm"),
                    TuiPanelHint::new("T", "test").action("test"),
                    TuiPanelHint::new("R", "reconnect").action("reconnect"),
                    TuiPanelHint::new("Space", "enable/disable").action("toggle"),
                    TuiPanelHint::new("Delete", "remove").action("remove"),
                    TuiPanelHint::new("Esc", "close").action("close"),
                ]
            },
        }
    }

    fn explain_session_scope(&mut self) {
        self.message = Some((
            "This session's MCP servers are managed by its ACP client".to_string(),
            TuiPanelTone::Muted,
        ));
    }

    async fn handle_list(&mut self, event: TuiPanelEvent) -> Result<TuiPanelControl> {
        match event {
            TuiPanelEvent::Action(action) if action == "close" => {
                self.cancel_pending();
                return Ok(TuiPanelControl::Close);
            }
            TuiPanelEvent::Action(action) if action == "previous" => {
                self.selected = self.selected.saturating_sub(1);
            }
            TuiPanelEvent::Action(action) if action == "next" => {
                self.selected = (self.selected + 1).min(self.servers.len().saturating_sub(1));
            }
            TuiPanelEvent::Page(distance) => {
                self.selected = if distance < 0 {
                    self.selected.saturating_sub(distance.unsigned_abs())
                } else {
                    (self.selected + distance as usize).min(self.servers.len().saturating_sub(1))
                };
            }
            TuiPanelEvent::Select(index) => {
                self.selected = index.min(self.servers.len().saturating_sub(1));
            }
            TuiPanelEvent::Action(action)
                if self.session_scoped
                    && matches!(action.as_str(), "add" | "confirm" | "remove" | "toggle") =>
            {
                self.explain_session_scope();
            }
            TuiPanelEvent::Activate(index) if self.session_scoped => {
                self.selected = index.min(self.servers.len().saturating_sub(1));
                self.explain_session_scope();
            }
            TuiPanelEvent::Text(' ') if self.session_scoped => self.explain_session_scope(),
            TuiPanelEvent::Action(action) if action == "add" => {
                self.cancel_pending();
                self.message = None;
                self.mode = McpPanelMode::Edit(McpDraft::new());
            }
            TuiPanelEvent::Action(action) if action == "confirm" => self.edit_selected(),
            TuiPanelEvent::Activate(index) => {
                self.selected = index.min(self.servers.len().saturating_sub(1));
                self.cancel_pending();
                self.edit_selected();
            }
            TuiPanelEvent::Action(action) if action == "remove" => {
                if let Some(server) = self.servers.get(self.selected).cloned() {
                    self.cancel_pending();
                    let resurfaces_user = server.scope == McpScope::Project
                        && self
                            .runtime
                            .store
                            .raw_at(McpScope::User, &server.name)
                            .await?
                            .is_some();
                    self.mode = McpPanelMode::ConfirmDelete(McpDelete {
                        server,
                        resurfaces_user,
                    });
                }
            }
            TuiPanelEvent::Text(character) if character.eq_ignore_ascii_case(&'t') => {
                self.test_selected();
            }
            TuiPanelEvent::Text(character) if character.eq_ignore_ascii_case(&'r') => {
                self.reconnect_selected();
            }
            TuiPanelEvent::Action(action) if action == "test" => self.test_selected(),
            TuiPanelEvent::Action(action) if action == "reconnect" => {
                self.reconnect_selected();
            }
            TuiPanelEvent::Action(action) if action == "toggle" => {
                self.toggle_selected().await?;
            }
            TuiPanelEvent::Text(' ') => self.toggle_selected().await?,
            _ => {}
        }
        Ok(TuiPanelControl::Continue)
    }

    fn edit_selected(&mut self) {
        let Some(server) = self.servers.get(self.selected).cloned() else {
            return;
        };
        self.cancel_pending();
        match server.parse() {
            Ok(config) => {
                self.message = None;
                self.mode = McpPanelMode::Edit(McpDraft::from_server(&server, config));
            }
            Err(error) => {
                self.message = Some((format!("{error:#}"), TuiPanelTone::Error));
            }
        }
    }

    fn test_selected(&mut self) {
        let Some(server) = self.servers.get(self.selected).cloned() else {
            return;
        };
        let config = match server.parse() {
            Ok(config) => config,
            Err(error) => {
                self.message = Some((format!("{}: {error:#}", server.name), TuiPanelTone::Error));
                return;
            }
        };
        self.message = Some((
            format!("{}: testing connection…", server.name),
            TuiPanelTone::Muted,
        ));
        let runtime = self.runtime.clone();
        let name = server.name;
        self.start_operation(async move {
            let result = test_with_timeout(&runtime, &name, &config)
                .await
                .map_err(|error| format!("{error:#}"));
            McpPanelUpdateKind::TestSelected { name, result }
        });
    }

    fn reconnect_selected(&mut self) {
        let Some(server) = self.servers.get(self.selected).cloned() else {
            return;
        };
        self.message = Some((
            format!("{}: reconnecting…", server.name),
            TuiPanelTone::Muted,
        ));
        let runtime = self.runtime.clone();
        let name = server.name;
        self.start_operation(async move {
            runtime.invalidate(&name).await;
            let result = runtime
                .connection(&name)
                .await
                .map(|_| ())
                .map_err(|error| format!("{error:#}"));
            McpPanelUpdateKind::Reconnect { name, result }
        });
    }

    async fn toggle_selected(&mut self) -> Result<()> {
        let Some(server) = self.servers.get(self.selected).cloned() else {
            return Ok(());
        };
        self.cancel_pending();
        let mut config = match server.parse() {
            Ok(config) => config,
            Err(error) => {
                self.message = Some((format!("{error:#}"), TuiPanelTone::Error));
                return Ok(());
            }
        };
        config.enabled = !config.enabled;
        self.runtime
            .store
            .write(server.scope, &server.name, serde_json::to_value(&config)?)
            .await?;
        self.runtime.invalidate(&server.name).await;
        self.reload().await?;
        self.message = Some((
            format!(
                "{}: {}",
                server.name,
                if config.enabled {
                    "enabled"
                } else {
                    "disabled"
                }
            ),
            TuiPanelTone::Accent,
        ));
        Ok(())
    }

    async fn handle_edit(
        &mut self,
        mut draft: McpDraft,
        event: TuiPanelEvent,
    ) -> Result<McpPanelMode> {
        match event {
            TuiPanelEvent::Action(action) if action == "close" => return Ok(McpPanelMode::List),
            TuiPanelEvent::Action(action) if action == "previous" => draft.move_selection(-1),
            TuiPanelEvent::Action(action) if action == "next" => draft.move_selection(1),
            TuiPanelEvent::Page(distance) => draft.move_selection(distance),
            TuiPanelEvent::Select(index) => {
                draft.selected = index.min(draft.rows().len().saturating_sub(1));
            }
            TuiPanelEvent::Activate(index) => {
                draft.selected = index.min(draft.rows().len().saturating_sub(1));
                if matches!(
                    draft.row_id().as_deref(),
                    Some("add-arg" | "add-env" | "add-header")
                ) {
                    draft.add_dynamic();
                } else if draft.toggle_selected() {
                    self.message = None;
                }
            }
            TuiPanelEvent::Action(action) if action == "add" => draft.add_dynamic(),
            TuiPanelEvent::Action(action) if action == "remove" => {
                if !draft.remove_dynamic() {
                    self.message = Some((
                        "Delete removes only argument, environment, or header rows".to_string(),
                        TuiPanelTone::Muted,
                    ));
                }
            }
            TuiPanelEvent::Action(action) if action == "backspace" => {
                if let Some(field) = draft.selected_text_mut() {
                    field.backspace();
                }
            }
            TuiPanelEvent::Action(action) if action == "left" => {
                if let Some(field) = draft.selected_text_mut() {
                    field.move_left();
                } else {
                    draft.toggle_selected();
                }
            }
            TuiPanelEvent::Action(action) if action == "right" => {
                if let Some(field) = draft.selected_text_mut() {
                    field.move_right();
                } else {
                    draft.toggle_selected();
                }
            }
            TuiPanelEvent::Action(action) if action == "home" => {
                if let Some(field) = draft.selected_text_mut() {
                    field.cursor = 0;
                }
            }
            TuiPanelEvent::Action(action) if action == "end" => {
                if let Some(field) = draft.selected_text_mut() {
                    field.cursor = field.value.chars().count();
                }
            }
            TuiPanelEvent::Action(action) if action == "save" => {
                return self.review(draft).await;
            }
            TuiPanelEvent::Action(action) if action == "confirm" => {
                if draft.row_id().as_deref() == Some("review") {
                    return self.review(draft).await;
                }
                if matches!(
                    draft.row_id().as_deref(),
                    Some("add-arg" | "add-env" | "add-header")
                ) {
                    draft.add_dynamic();
                } else if !draft.toggle_selected() {
                    draft.move_selection(1);
                }
            }
            TuiPanelEvent::Text(character) => {
                if let Some(field) = draft.selected_text_mut() {
                    field.insert(&character.to_string());
                }
            }
            _ => {}
        }
        Ok(McpPanelMode::Edit(draft))
    }

    pub(super) async fn review(&mut self, draft: McpDraft) -> Result<McpPanelMode> {
        let (name, config) = match draft.build() {
            Ok(built) => built,
            Err(error) => {
                self.message = Some((format!("{error:#}"), TuiPanelTone::Error));
                return Ok(McpPanelMode::Edit(draft));
            }
        };
        if let Err(error) = self.validate_unique(&draft, &name).await {
            self.message = Some((format!("{error:#}"), TuiPanelTone::Error));
            return Ok(McpPanelMode::Edit(draft));
        }
        self.message = Some(("Testing connection…".to_string(), TuiPanelTone::Muted));
        self.start_review_test(name.clone(), config.clone());
        Ok(McpPanelMode::Review(McpReview {
            draft,
            name,
            config,
            test: McpReviewTest::Running,
            selected: 0,
        }))
    }

    fn start_review_test(&mut self, name: String, config: McpServerConfig) {
        self.message = Some(("Testing connection…".to_string(), TuiPanelTone::Muted));
        let runtime = self.runtime.clone();
        self.start_operation(async move {
            let result = test_with_timeout(&runtime, &name, &config)
                .await
                .map_err(|error| format!("{error:#}"));
            McpPanelUpdateKind::ReviewTest { name, result }
        });
    }

    pub(super) async fn validate_unique(&self, draft: &McpDraft, name: &str) -> Result<()> {
        let protocol = protocol_name(name)?;
        if let Some(origin) = &draft.origin
            && origin.scope != draft.scope
            && self
                .runtime
                .store
                .raw_at(draft.scope, name)
                .await?
                .is_some()
        {
            bail!(
                "MCP server {name:?} already exists in {} scope",
                draft.scope.label()
            );
        }
        for server in self.runtime.store.effective().await?.into_values() {
            if draft
                .origin
                .as_ref()
                .is_some_and(|origin| origin.name == server.name)
            {
                continue;
            }
            if server.name == name {
                bail!("MCP server {name:?} already exists");
            }
            if protocol_name(&server.name)? == protocol {
                bail!(
                    "MCP server name collides with {:?} at {protocol}://",
                    server.name
                );
            }
        }
        Ok(())
    }

    async fn handle_review(
        &mut self,
        mut review: McpReview,
        event: TuiPanelEvent,
    ) -> Result<McpPanelMode> {
        match event {
            TuiPanelEvent::Action(action) if action == "close" => {
                self.cancel_pending();
                Ok(McpPanelMode::Edit(review.draft))
            }
            TuiPanelEvent::Action(action) if matches!(action.as_str(), "confirm" | "save") => {
                if matches!(review.test, McpReviewTest::Running) {
                    self.message = Some((
                        "Wait for the connection test, or return to edit to cancel it".to_string(),
                        TuiPanelTone::Muted,
                    ));
                    return Ok(McpPanelMode::Review(review));
                }
                self.save_review(review).await?;
                Ok(McpPanelMode::List)
            }
            TuiPanelEvent::Action(action) if action == "edit" => {
                self.cancel_pending();
                Ok(McpPanelMode::Edit(review.draft))
            }
            TuiPanelEvent::Text(character) if character.eq_ignore_ascii_case(&'e') => {
                self.cancel_pending();
                Ok(McpPanelMode::Edit(review.draft))
            }
            TuiPanelEvent::Text(character) if character.eq_ignore_ascii_case(&'t') => {
                review.test = McpReviewTest::Running;
                self.start_review_test(review.name.clone(), review.config.clone());
                Ok(McpPanelMode::Review(review))
            }
            TuiPanelEvent::Action(action) if action == "test" => {
                review.test = McpReviewTest::Running;
                self.start_review_test(review.name.clone(), review.config.clone());
                Ok(McpPanelMode::Review(review))
            }
            TuiPanelEvent::Action(action) if action == "previous" => {
                review.selected = review.selected.saturating_sub(1);
                Ok(McpPanelMode::Review(review))
            }
            TuiPanelEvent::Action(action) if action == "next" => {
                review.selected = (review.selected + 1).min(review.rows().len().saturating_sub(1));
                Ok(McpPanelMode::Review(review))
            }
            TuiPanelEvent::Page(distance) => {
                review.selected = if distance < 0 {
                    review.selected.saturating_sub(distance.unsigned_abs())
                } else {
                    (review.selected + distance as usize).min(review.rows().len().saturating_sub(1))
                };
                Ok(McpPanelMode::Review(review))
            }
            TuiPanelEvent::Select(index) => {
                review.selected = index.min(review.rows().len().saturating_sub(1));
                Ok(McpPanelMode::Review(review))
            }
            TuiPanelEvent::Activate(index) => {
                review.selected = index.min(review.rows().len().saturating_sub(1));
                if matches!(review.test, McpReviewTest::Running) {
                    self.message = Some((
                        "Wait for the connection test, or return to edit to cancel it".to_string(),
                        TuiPanelTone::Muted,
                    ));
                    Ok(McpPanelMode::Review(review))
                } else {
                    self.save_review(review).await?;
                    Ok(McpPanelMode::List)
                }
            }
            _ => Ok(McpPanelMode::Review(review)),
        }
    }

    async fn save_review(&mut self, review: McpReview) -> Result<()> {
        self.validate_unique(&review.draft, &review.name).await?;
        let value = serde_json::to_value(&review.config)?;
        if let Some(origin) = &review.draft.origin {
            if origin.name != review.name {
                bail!("existing MCP server names cannot be changed");
            }
            self.runtime
                .store
                .move_server(origin.scope, review.draft.scope, &review.name, value)
                .await?;
        } else {
            self.runtime
                .store
                .write(review.draft.scope, &review.name, value)
                .await?;
        }
        self.runtime.invalidate(&review.name).await;
        self.reload().await?;
        self.selected = self
            .servers
            .iter()
            .position(|server| server.name == review.name)
            .unwrap_or(self.selected);
        self.message = Some((
            format!(
                "Saved {}://; newly added protocols appear in new sessions",
                protocol_name(&review.name)?
            ),
            TuiPanelTone::Accent,
        ));
        Ok(())
    }

    async fn handle_delete(
        &mut self,
        delete: McpDelete,
        event: TuiPanelEvent,
    ) -> Result<McpPanelMode> {
        match event {
            TuiPanelEvent::Action(action) if action == "close" => Ok(McpPanelMode::List),
            TuiPanelEvent::Action(action) if matches!(action.as_str(), "confirm" | "remove") => {
                self.runtime
                    .store
                    .remove(delete.server.scope, &delete.server.name)
                    .await?;
                self.runtime.invalidate(&delete.server.name).await;
                self.reload().await?;
                self.message = Some((
                    if delete.resurfaces_user {
                        format!(
                            "Removed Project override; User server {:?} is now effective",
                            delete.server.name
                        )
                    } else {
                        format!("Removed MCP server {:?}", delete.server.name)
                    },
                    if delete.resurfaces_user {
                        TuiPanelTone::Warning
                    } else {
                        TuiPanelTone::Accent
                    },
                ));
                Ok(McpPanelMode::List)
            }
            _ => Ok(McpPanelMode::ConfirmDelete(delete)),
        }
    }
}

async fn test_with_timeout(
    runtime: &McpRuntime,
    name: &str,
    config: &McpServerConfig,
) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), runtime.test_config(name, config))
        .await
        .map_err(|_| anyhow!("MCP connection test timed out after 30 seconds"))?
}

#[async_trait]
impl TuiPanelSession for McpPanel {
    fn view(&mut self) -> TuiPanelView {
        self.drain_updates();
        match &self.mode {
            McpPanelMode::List => self.list_view(),
            McpPanelMode::Edit(draft) => TuiPanelView {
                title: draft.title(),
                rows: draft.rows(),
                selected: Some(draft.selected),
                message: self.message.clone(),
                hints: vec![
                    TuiPanelHint::new("Enter", "next/toggle").action("confirm"),
                    TuiPanelHint::new("Ctrl+N", "add row").action("add"),
                    TuiPanelHint::new("Delete", "remove row").action("remove"),
                    TuiPanelHint::new("Ctrl+S", "review & test").action("save"),
                    TuiPanelHint::new("Esc", "back").action("close"),
                ],
            },
            McpPanelMode::Review(review) => TuiPanelView {
                title: "REVIEW MCP SERVER".to_string(),
                rows: review.rows(),
                selected: Some(review.selected),
                message: self.message.clone(),
                hints: vec![
                    TuiPanelHint::new("Enter", "save").action("save"),
                    TuiPanelHint::new("T", "test again").action("test"),
                    TuiPanelHint::new("E/Esc", "edit").action("edit"),
                ],
            },
            McpPanelMode::ConfirmDelete(delete) => TuiPanelView {
                title: "REMOVE MCP SERVER?".to_string(),
                rows: vec![
                    TuiPanelRow::item("name", "Server", &delete.server.name),
                    TuiPanelRow::item("scope", "Scope", delete.server.scope.label()),
                    TuiPanelRow::item(
                        "effect",
                        "Effect",
                        if delete.resurfaces_user {
                            "User configuration will become effective"
                        } else {
                            "Configuration will be removed"
                        },
                    )
                    .tone(if delete.resurfaces_user {
                        TuiPanelTone::Warning
                    } else {
                        TuiPanelTone::Error
                    }),
                ],
                selected: None,
                message: Some((
                    "Enter removes and disconnects immediately; Esc cancels".to_string(),
                    TuiPanelTone::Warning,
                )),
                hints: vec![
                    TuiPanelHint::new("Enter", "remove").action("remove"),
                    TuiPanelHint::new("Esc", "cancel").action("close"),
                ],
            },
        }
    }

    async fn handle(&mut self, event: TuiPanelEvent) -> Result<TuiPanelControl> {
        self.drain_updates();
        let previous = self.mode.clone();
        let mode = std::mem::replace(&mut self.mode, McpPanelMode::List);
        let result = match mode {
            McpPanelMode::List => return self.handle_list(event).await,
            McpPanelMode::Edit(draft) => self.handle_edit(draft, event).await,
            McpPanelMode::Review(review) => self.handle_review(review, event).await,
            McpPanelMode::ConfirmDelete(delete) => self.handle_delete(delete, event).await,
        };
        match result {
            Ok(mode) => {
                self.mode = mode;
                Ok(TuiPanelControl::Continue)
            }
            Err(error) => {
                self.mode = previous;
                Err(error)
            }
        }
    }

    fn paste(&mut self, text: String) -> Result<TuiPanelControl> {
        if let McpPanelMode::Edit(draft) = &mut self.mode
            && let Some(field) = draft.selected_text_mut()
        {
            field.insert(&text);
        }
        Ok(TuiPanelControl::Continue)
    }
}

impl Drop for McpPanel {
    fn drop(&mut self) {
        self.cancel_pending();
    }
}
