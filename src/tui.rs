mod animation;
mod markdown;
mod model_selector;

use self::model_selector::{ModelSelector, context_label, model_label, reasoning};
use crate::catalog::{CatalogModel, ModelCatalog, ThinkingLevel};
use crate::config::{ActiveSettings, AuthKind, ConfigManager, display_path};
use crate::keymap::{Keymap, canonical_key};
use crate::model::{clamp_thinking_level, configured_backend};
use crate::oauth::{self, OauthLogin, OauthProvider, OauthToken};
use crate::output::OutputStore;
use crate::plugin::{
    CommandRegistry, CommandSpec, CommandTarget, CoreCommand, TuiDocument, TuiPanelContext,
    TuiRegistry, TuiStatusContext, TuiStatusItem, TuiStatusTone,
};
use crate::protocol::ProtocolDescriptor;
use crate::runtime::AgentRuntime;
use crate::session::{EventKind, SessionEvent, SessionSummary, SessionUpdate};
use crate::task::{TaskManager, TaskRecord};
use crate::terminal::EmbeddedTerminal;
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use crossterm::execute;
use futures_util::StreamExt;
use portable_pty::CommandBuilder;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap,
};
use ratatui::{DefaultTerminal, Frame};
use std::io::{Write, stdout};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time;
use tui_term::widget::PseudoTerminal;
use tui_textarea::TextArea;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const BG: Color = Color::Rgb(13, 15, 18);
const SURFACE: Color = Color::Rgb(21, 24, 28);
const ROW_ACTIVE: Color = Color::Rgb(25, 30, 35);
const USER_SURFACE: Color = Color::Rgb(17, 25, 24);
const TEXT: Color = Color::Rgb(218, 223, 229);
const MUTED: Color = Color::Rgb(116, 124, 135);
const ACCENT: Color = Color::Rgb(104, 210, 194);
const WARM: Color = Color::Rgb(239, 173, 104);
const ERROR: Color = Color::Rgb(239, 108, 120);
const FLASH_DURATION: Duration = Duration::from_secs(5);
const SPLASH_DURATION: Duration = Duration::from_millis(1200);
const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(500);
const EXPANDED_PREVIEW_LINES: usize = 24;

pub struct TuiInfo {
    pub cwd: PathBuf,
    pub provider: String,
    pub model: String,
    pub thinking: ThinkingLevel,
    pub session_id: String,
    pub context_window: usize,
    pub model_ready: bool,
    pub provider_count: usize,
    pub context_tokens: usize,
    pub terminal: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TuiOutcome {
    Quit,
    NewSession,
    Resume(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BlockKind {
    User,
    Assistant,
    Reasoning,
    Tool,
    Compaction,
    Notice,
    Error,
}

struct DisplayBlock {
    kind: BlockKind,
    title: String,
    text: String,
    call_id: Option<String>,
    failed: bool,
    expanded: bool,
    tool: Option<ToolDisplay>,
    transient: bool,
    final_response: bool,
}

struct ToolDisplay {
    name: String,
    arguments: serde_json::Value,
    output: Option<String>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Overlay {
    Composer,
    Command,
    Status,
    Help,
    Protocols,
    Tasks,
    Models,
    Settings,
    Plugin,
    Document,
    Selector,
    Text,
    Oauth,
    Terminal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum JumpKind {
    All,
    Reasoning,
    Tool,
    User,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum EditingSetting {
    OutputLimit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AppHit {
    Transcript(usize),
    Palette(usize),
    Task(usize),
    Model(usize),
    Setting(usize),
    Selector(usize),
    Status,
}

#[derive(Clone)]
enum Activity {
    Thinking,
    Reasoning,
    Writing,
    Tool(String),
    Compacting,
}

impl Activity {
    fn label(&self) -> String {
        match self {
            Self::Thinking => "thinking".to_string(),
            Self::Reasoning => "reasoning".to_string(),
            Self::Writing => "writing".to_string(),
            Self::Tool(protocol) => format!("running {protocol}"),
            Self::Compacting => "compacting".to_string(),
        }
    }
}

#[derive(Clone, Copy)]
struct HitRegion<T> {
    area: Rect,
    target: T,
}

struct SelectableSurface {
    area: Rect,
    cells: Vec<Vec<String>>,
}

#[derive(Clone, Copy)]
struct TextSelection {
    start: (u16, u16),
    end: (u16, u16),
}

#[derive(Clone)]
struct CommandMatch {
    spec: CommandSpec,
    name: String,
}

struct SettingsState {
    active: ActiveSettings,
    model: Option<CatalogModel>,
    selected: usize,
    editing: Option<EditingSetting>,
    api_key: String,
    api_key_changed: bool,
    thinking: ThinkingLevel,
    output_limit: String,
}

impl SettingsState {
    async fn load(active: ActiveSettings, catalog: &ModelCatalog) -> Self {
        let model = active.catalog_model(catalog).await;
        Self {
            output_limit: active.output_limit.to_string(),
            thinking: active.thinking,
            active,
            model,
            selected: 0,
            editing: None,
            api_key: String::new(),
            api_key_changed: false,
        }
    }

    fn provider(&self) -> &str {
        &self.active.provider
    }

    fn model(&self) -> Option<&CatalogModel> {
        self.model.as_ref()
    }

    fn cycle_thinking(&mut self) {
        let current = ThinkingLevel::ALL
            .iter()
            .position(|level| *level == self.thinking)
            .unwrap_or(0);
        self.thinking = (1..=ThinkingLevel::ALL.len())
            .map(|offset| ThinkingLevel::ALL[(current + offset) % ThinkingLevel::ALL.len()])
            .find(|level| {
                let Some(model) = self.model() else {
                    return *level == ThinkingLevel::Off;
                };
                model.supports_thinking_level(*level)
            })
            .unwrap_or(ThinkingLevel::Off);
    }
}

#[derive(Clone)]
struct SelectorItem {
    id: String,
    title: String,
    description: String,
    search_text: Option<String>,
}

enum SelectorKind {
    LoginProvider,
    LoginMethod { provider: String },
    Logout,
    Resume,
    Search,
    Effort { provider: String, model: String },
}

struct SelectorState {
    kind: SelectorKind,
    title: String,
    query: String,
    items: Vec<SelectorItem>,
    visible: Vec<usize>,
    selected: usize,
}

impl SelectorState {
    fn new(kind: SelectorKind, title: impl Into<String>, items: Vec<SelectorItem>) -> Self {
        let mut selector = Self {
            kind,
            title: title.into(),
            query: String::new(),
            items,
            visible: Vec::new(),
            selected: 0,
        };
        selector.rebuild();
        selector
    }

    fn rebuild(&mut self) {
        let query = self.query.trim().to_lowercase();
        if matches!(&self.kind, SelectorKind::Search) {
            for item in &mut self.items {
                item.description = search_line_preview(
                    item.search_text.as_deref().unwrap_or_default(),
                    &query,
                    180,
                );
            }
        }
        self.visible = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                if query.is_empty() {
                    return true;
                }
                if let Some(search_text) = &item.search_text {
                    item.title.to_lowercase().contains(&query)
                        || search_text.to_lowercase().contains(&query)
                } else {
                    item.title.to_lowercase().contains(&query)
                        || item.description.to_lowercase().contains(&query)
                        || item.id.to_lowercase().contains(&query)
                }
            })
            .map(|(index, _)| index)
            .collect();
        self.selected = self.selected.min(self.visible.len().saturating_sub(1));
    }

    fn selected_item(&self) -> Option<&SelectorItem> {
        self.visible
            .get(self.selected)
            .and_then(|index| self.items.get(*index))
    }

    fn move_selection(&mut self, distance: isize) {
        self.selected = wrapped_index(self.selected, distance, self.visible.len());
    }

    fn select_from_click(&mut self, position: usize, double_click: bool) -> bool {
        self.selected = position;
        double_click || matches!(&self.kind, SelectorKind::Search)
    }
}

enum TextPurpose {
    ApiKey { provider: String },
    CopilotDomain,
    SetTerminal,
}

struct TextPrompt {
    title: String,
    message: String,
    value: String,
    secret: bool,
    purpose: TextPurpose,
}

struct OauthState {
    provider: String,
    login: OauthLogin,
    paste: String,
    message: String,
}

struct FloatTerminal {
    terminal: EmbeddedTerminal,
    area: Rect,
    last_escape: Option<Instant>,
}

/// Cumulative token usage of the whole session, replayed from usage events.
#[derive(Default)]
struct UsageTotals {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    cost: f64,
}

struct App {
    input: TextArea<'static>,
    blocks: Vec<DisplayBlock>,
    protocols: Vec<ProtocolDescriptor>,
    task_records: Vec<TaskRecord>,
    selected_task: usize,
    selected_block: usize,
    overlay: Option<Overlay>,
    overlay_scroll: u16,
    jump: JumpKind,
    busy: bool,
    activity: Option<Activity>,
    busy_since: Option<Instant>,
    frame: usize,
    composer_scroll: (u16, u16),
    transcript_body_width: usize,
    transcript_offset: usize,
    transcript_rows: usize,
    transcript_height: usize,
    transcript_follow_tail: bool,
    transcript_center_selected: bool,
    started: Instant,
    splash_skipped: bool,
    last_sequence: Option<u64>,
    applying_transient: bool,
    reasoning_folded_during_stream: bool,
    info: TuiInfo,
    flash: Option<String>,
    flash_at: Option<Instant>,
    model_selector: Option<ModelSelector>,
    catalog_refreshing: bool,
    settings: Option<SettingsState>,
    keymap: Keymap,
    command_query: String,
    command_selected: usize,
    command_stem: Option<String>,
    selector: Option<SelectorState>,
    text_prompt: Option<TextPrompt>,
    oauth: Option<OauthState>,
    pty: Option<FloatTerminal>,
    document: Option<(String, String)>,
    hit_regions: Vec<HitRegion<AppHit>>,
    last_click: Option<(AppHit, Instant)>,
    overlay_bounds: Option<Rect>,
    copy_click_release_pending: bool,
    selectable: Option<SelectableSurface>,
    selection: Option<TextSelection>,
    usage: UsageTotals,
    last_cache_hit: Option<f64>,
    branch: Option<(Instant, Option<String>)>,
    pending_transcript_click: Option<(usize, Instant)>,
    commands: Arc<CommandRegistry>,
    tui: Arc<TuiRegistry>,
    tui_document: Option<TuiDocument>,
}

impl App {
    fn new(
        protocols: Vec<ProtocolDescriptor>,
        commands: Arc<CommandRegistry>,
        tui: Arc<TuiRegistry>,
        info: TuiInfo,
        keymap: Keymap,
        draft: String,
        show_splash: bool,
    ) -> Self {
        let mut input = TextArea::default();
        if !draft.is_empty() {
            input.insert_str(&draft);
        }
        style_input(&mut input, false);
        Self {
            input,
            blocks: Vec::new(),
            protocols,
            task_records: Vec::new(),
            selected_task: 0,
            selected_block: 0,
            overlay: None,
            overlay_scroll: 0,
            jump: JumpKind::All,
            busy: false,
            activity: None,
            busy_since: None,
            frame: 0,
            composer_scroll: (0, 0),
            transcript_body_width: 72,
            transcript_offset: 0,
            transcript_rows: 0,
            transcript_height: 0,
            transcript_follow_tail: true,
            transcript_center_selected: false,
            started: Instant::now(),
            splash_skipped: !show_splash,
            last_sequence: None,
            applying_transient: false,
            reasoning_folded_during_stream: false,
            info,
            flash: None,
            flash_at: None,
            model_selector: None,
            catalog_refreshing: false,
            settings: None,
            keymap,
            command_query: String::new(),
            command_selected: 0,
            command_stem: None,
            selector: None,
            text_prompt: None,
            oauth: None,
            pty: None,
            document: None,
            hit_regions: Vec::new(),
            last_click: None,
            overlay_bounds: None,
            copy_click_release_pending: false,
            selectable: None,
            selection: None,
            usage: UsageTotals::default(),
            last_cache_hit: None,
            branch: None,
            pending_transcript_click: None,
            commands,
            tui,
            tui_document: None,
        }
    }

    fn showing_splash(&self) -> bool {
        !self.splash_skipped && self.started.elapsed() < SPLASH_DURATION
    }

    fn skip_splash(&mut self) {
        self.splash_skipped = true;
    }

    fn animations_paused(&self) -> bool {
        self.overlay == Some(Overlay::Composer)
    }

    fn apply(&mut self, event: SessionEvent) {
        let settles_model_response = matches!(
            &event.kind,
            EventKind::ModelMessage { .. } | EventKind::Error { .. } | EventKind::TurnFinished
        );
        if !self.applying_transient
            && matches!(
                &event.kind,
                EventKind::AssistantText { .. }
                    | EventKind::AssistantReasoning { .. }
                    | EventKind::ToolCall { .. }
                    | EventKind::ModelMessage { .. }
                    | EventKind::Error { .. }
                    | EventKind::TurnFinished
            )
        {
            self.clear_transient_blocks();
        }
        if settles_model_response {
            self.reasoning_folded_during_stream = false;
        }
        if self
            .last_sequence
            .is_some_and(|sequence| event.sequence <= sequence)
        {
            return;
        }
        self.last_sequence = Some(event.sequence);
        if matches!(
            &event.kind,
            EventKind::AssistantText { .. }
                | EventKind::ToolCall { .. }
                | EventKind::ModelMessage { .. }
                | EventKind::Error { .. }
                | EventKind::TurnFinished
        ) {
            self.fold_trailing_reasoning();
        }
        let select_tail =
            self.blocks.is_empty() || self.selected_block == self.blocks.len().saturating_sub(1);
        match event.kind {
            EventKind::SessionCreated { .. }
            | EventKind::SessionContext { .. }
            | EventKind::ModelSettingsChanged { .. }
            | EventKind::ModelMessage { .. }
            | EventKind::Task { .. } => {}
            EventKind::User { text } => {
                self.reasoning_folded_during_stream = false;
                self.busy = true;
                self.busy_since.get_or_insert_with(Instant::now);
                self.activity = Some(Activity::Thinking);
                self.push(BlockKind::User, "YOU", text, None, false, false);
            }
            EventKind::AssistantText { text } => {
                self.activity = Some(Activity::Writing);
                self.append_or_push(BlockKind::Assistant, "AGENT", text, true);
            }
            EventKind::AssistantReasoning { text } => {
                self.activity = Some(Activity::Reasoning);
                self.append_or_push(
                    BlockKind::Reasoning,
                    "THINKING",
                    text,
                    !self.reasoning_folded_during_stream,
                );
            }
            EventKind::ToolCall {
                call_id,
                name,
                arguments,
            } => {
                let protocol = tool_protocol(&arguments).unwrap_or_else(|| name.clone());
                self.activity = Some(Activity::Tool(protocol));
                let text = serde_json::to_string_pretty(&arguments)
                    .unwrap_or_else(|_| arguments.to_string());
                let title = tool_title(&name, &arguments);
                self.push(
                    BlockKind::Tool,
                    &title,
                    format!("CALL\n{text}"),
                    Some(call_id),
                    false,
                    false,
                );
                self.blocks.last_mut().unwrap().tool = Some(ToolDisplay {
                    name,
                    arguments,
                    output: None,
                });
            }
            EventKind::ToolResult {
                call_id,
                name,
                output,
                failed,
            } => {
                if let Some(block) = self
                    .blocks
                    .iter_mut()
                    .rev()
                    .find(|block| block.call_id.as_deref() == Some(&call_id))
                {
                    block.failed = failed;
                    if let Some(tool) = block.tool.as_mut() {
                        tool.output = Some(output.clone());
                    }
                    block.text.push_str(if failed {
                        "\n\nERROR\n"
                    } else {
                        "\n\nRESULT\n"
                    });
                    block.text.push_str(&output);
                } else {
                    let tool_output = output.clone();
                    self.push(
                        if failed {
                            BlockKind::Error
                        } else {
                            BlockKind::Tool
                        },
                        &name.to_ascii_uppercase(),
                        output,
                        Some(call_id),
                        failed,
                        true,
                    );
                    self.blocks.last_mut().unwrap().tool = Some(ToolDisplay {
                        name,
                        arguments: serde_json::Value::Null,
                        output: Some(tool_output),
                    });
                }
                self.activity = Some(Activity::Thinking);
            }
            EventKind::Notice { text } => {
                self.push(BlockKind::Notice, "SYSTEM", text, None, false, false);
            }
            EventKind::Usage {
                input,
                output,
                cache_read,
                cache_write,
                cost,
            } => {
                self.usage.input += input;
                self.usage.output += output;
                self.usage.cache_read += cache_read;
                self.usage.cache_write += cache_write;
                self.usage.cost += cost;
                let prompt_tokens = input + cache_read + cache_write;
                self.last_cache_hit =
                    (prompt_tokens > 0).then(|| cache_read as f64 / prompt_tokens as f64 * 100.0);
            }
            EventKind::Error { text } => {
                self.busy = false;
                self.activity = None;
                self.busy_since = None;
                self.push(BlockKind::Error, "ERROR", text, None, true, true);
            }
            EventKind::Compaction {
                summary,
                tokens_before,
                replacement_history: _,
                manual,
            } => {
                if manual {
                    self.busy = false;
                    self.activity = None;
                    self.busy_since = None;
                    self.set_flash("Context compacted; original events retained");
                } else {
                    self.activity = Some(Activity::Thinking);
                }
                self.push(
                    BlockKind::Compaction,
                    "COMPACTION",
                    format!(
                        "Estimated context before compaction: {tokens_before} tokens\n\n{summary}"
                    ),
                    None,
                    false,
                    false,
                );
            }
            EventKind::TurnFinished => {
                self.busy = false;
                self.activity = None;
                self.busy_since = None;
                for block in &mut self.blocks {
                    if matches!(block.kind, BlockKind::Reasoning | BlockKind::Tool) {
                        block.expanded = false;
                    }
                }
                let turn_start = self
                    .blocks
                    .iter()
                    .rposition(|block| block.kind == BlockKind::User)
                    .map_or(0, |index| index + 1);
                if let Some(block) = self.blocks[turn_start..]
                    .iter_mut()
                    .rev()
                    .find(|block| block.kind == BlockKind::Assistant)
                {
                    block.expanded = true;
                    block.final_response = true;
                }
            }
        }
        if select_tail {
            self.selected_block = self.blocks.len().saturating_sub(1);
        }
        style_input(&mut self.input, self.busy);
    }

    fn apply_transient(&mut self, kind: EventKind) {
        let last_sequence = self.last_sequence;
        self.applying_transient = true;
        self.apply(SessionEvent {
            sequence: last_sequence.map_or(0, |sequence| sequence.saturating_add(1)),
            at: chrono::Utc::now(),
            kind,
        });
        self.applying_transient = false;
        self.last_sequence = last_sequence;
    }

    fn clear_transient_blocks(&mut self) {
        self.reasoning_folded_during_stream |= self
            .blocks
            .iter()
            .any(|block| block.transient && block.kind == BlockKind::Reasoning && !block.expanded);
        self.blocks.retain(|block| !block.transient);
        self.selected_block = self.selected_block.min(self.blocks.len().saturating_sub(1));
    }

    fn fold_trailing_reasoning(&mut self) {
        if let Some(block) = self
            .blocks
            .last_mut()
            .filter(|block| block.kind == BlockKind::Reasoning)
        {
            block.expanded = false;
        }
    }

    fn finish_hydration(&mut self) {
        self.busy = false;
        self.activity = None;
        self.busy_since = None;
        for block in &mut self.blocks {
            block.expanded = matches!(
                block.kind,
                BlockKind::Assistant | BlockKind::Notice | BlockKind::Error
            );
        }
        style_input(&mut self.input, false);
    }

    fn push(
        &mut self,
        kind: BlockKind,
        title: &str,
        text: String,
        call_id: Option<String>,
        failed: bool,
        expanded: bool,
    ) {
        self.blocks.push(DisplayBlock {
            kind,
            title: title.to_string(),
            text,
            call_id,
            failed,
            expanded,
            tool: None,
            transient: self.applying_transient,
            final_response: false,
        });
    }

    fn append_or_push(&mut self, kind: BlockKind, title: &str, text: String, expanded: bool) {
        if let Some(block) = self
            .blocks
            .last_mut()
            .filter(|block| block.kind == kind && block.transient == self.applying_transient)
        {
            block.text.push_str(&text);
        } else {
            self.push(kind, title, text, None, false, expanded);
        }
    }

    fn submit(&mut self) -> Option<String> {
        if self.busy {
            self.set_flash("A turn is already running");
            return None;
        }
        let text = self.input.lines().join("\n");
        if text.trim().is_empty() {
            return None;
        }
        self.input = TextArea::default();
        self.composer_scroll = (0, 0);
        style_input(&mut self.input, true);
        self.busy = true;
        self.busy_since = Some(Instant::now());
        self.activity = Some(Activity::Thinking);
        self.overlay = None;
        Some(text)
    }

    fn draft_text(&self) -> String {
        self.input.lines().join("\n")
    }

    fn set_flash(&mut self, message: impl Into<String>) {
        self.flash = Some(message.into());
        self.flash_at = Some(Instant::now());
    }

    fn visible_flash(&self) -> Option<&str> {
        self.flash.as_deref().filter(|_| {
            self.flash_at
                .is_some_and(|created| created.elapsed() < FLASH_DURATION)
        })
    }

    fn filtered_indices(&self) -> Vec<usize> {
        self.blocks
            .iter()
            .enumerate()
            .filter(|(_, block)| match self.jump {
                JumpKind::All => true,
                JumpKind::Reasoning => block.kind == BlockKind::Reasoning,
                JumpKind::Tool => block.kind == BlockKind::Tool,
                JumpKind::User => block.kind == BlockKind::User,
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn move_selection(&mut self, distance: isize) {
        let indices = self.filtered_indices();
        if indices.is_empty() {
            return;
        }
        let current = indices
            .iter()
            .position(|index| *index == self.selected_block)
            .unwrap_or(0);
        let next = wrapped_index(current, distance, indices.len());
        self.selected_block = indices[next];
        self.transcript_follow_tail = false;
        self.transcript_center_selected = true;
    }

    fn select_search_result(&mut self, index: usize) -> bool {
        let Some(block) = self.blocks.get_mut(index) else {
            return false;
        };
        if !matches!(block.kind, BlockKind::User | BlockKind::Assistant) {
            block.expanded = true;
        }
        self.jump = JumpKind::All;
        self.selected_block = index;
        self.transcript_follow_tail = false;
        self.transcript_center_selected = true;
        true
    }

    fn jump_to(&mut self, kind: JumpKind) {
        self.jump = kind;
        let indices = self.filtered_indices();
        if indices.is_empty() {
            self.set_flash(match kind {
                JumpKind::Reasoning => "No thinking blocks in this session",
                JumpKind::Tool => "No tool calls in this session",
                JumpKind::User => "No user messages in this session",
                JumpKind::All => "No events",
            });
            self.jump = JumpKind::All;
            return;
        }
        let current = indices
            .iter()
            .position(|index| *index == self.selected_block);
        let next = match current {
            Some(index) => (index + 1) % indices.len(),
            None => indices
                .iter()
                .position(|index| *index >= self.selected_block)
                .unwrap_or(0),
        };
        self.selected_block = indices[next];
        self.transcript_follow_tail = false;
        self.transcript_center_selected = true;
    }

    fn toggle_selected(&mut self) {
        let Some(block) = self.blocks.get_mut(self.selected_block) else {
            return;
        };
        if matches!(block.kind, BlockKind::User | BlockKind::Assistant) {
            return;
        }
        block.expanded = !block.expanded;
        self.transcript_center_selected = true;
    }

    fn open_selected_document(&mut self) {
        let Some(block) = self.blocks.get(self.selected_block) else {
            return;
        };
        if matches!(block.kind, BlockKind::User | BlockKind::Assistant) {
            return;
        }
        self.document = Some((block.title.clone(), block_document(block)));
        self.overlay_scroll = 0;
        self.overlay = Some(Overlay::Document);
    }

    fn click_transcript_block(&mut self, index: usize, open_document: bool) {
        self.selected_block = index;
        self.transcript_follow_tail = false;
        if !self
            .blocks
            .get(index)
            .is_some_and(|block| !matches!(block.kind, BlockKind::User | BlockKind::Assistant))
        {
            return;
        }
        if open_document {
            self.open_selected_document();
        } else {
            self.toggle_selected();
        }
    }

    fn queue_transcript_click(&mut self, index: usize) {
        let now = Instant::now();
        if let Some((pending, at)) = self.pending_transcript_click.take() {
            if pending == index && now.duration_since(at) < DOUBLE_CLICK_INTERVAL {
                self.last_click = None;
                self.click_transcript_block(index, true);
                return;
            }
            self.click_transcript_block(pending, false);
        }
        self.selected_block = index;
        self.transcript_follow_tail = false;
        self.last_click = None;
        if self
            .blocks
            .get(index)
            .is_some_and(|block| !matches!(block.kind, BlockKind::User | BlockKind::Assistant))
        {
            self.pending_transcript_click = Some((index, now));
        }
    }

    fn confirm_pending_transcript_click(&mut self) {
        if let Some((index, _)) = self.pending_transcript_click.take() {
            self.click_transcript_block(index, false);
        }
    }

    fn confirm_pending_transcript_click_if_elapsed(&mut self) {
        if self
            .pending_transcript_click
            .is_some_and(|(_, at)| at.elapsed() >= DOUBLE_CLICK_INTERVAL)
        {
            self.confirm_pending_transcript_click();
        }
    }

    fn active_transcript_block(&self) -> Option<usize> {
        if !self.busy {
            return None;
        }
        let current_turn_start = self
            .blocks
            .iter()
            .rposition(|block| block.kind == BlockKind::User)
            .map_or(0, |index| index + 1);
        match &self.activity {
            Some(Activity::Reasoning) => self
                .blocks
                .iter()
                .rposition(|block| block.kind == BlockKind::Reasoning),
            Some(Activity::Writing) => self
                .blocks
                .iter()
                .rposition(|block| block.kind == BlockKind::Assistant),
            Some(Activity::Thinking | Activity::Tool(_)) => self
                .blocks
                .iter()
                .enumerate()
                .skip(current_turn_start)
                .find_map(|(index, block)| {
                    (block.kind == BlockKind::Tool
                        && block
                            .tool
                            .as_ref()
                            .is_some_and(|tool| tool.output.is_none()))
                    .then_some(index)
                }),
            Some(Activity::Compacting) | None => None,
        }
    }

    fn scroll_transcript(&mut self, distance: isize) {
        let max_offset = self.transcript_rows.saturating_sub(self.transcript_height);
        self.transcript_offset = if distance < 0 {
            self.transcript_offset
                .saturating_sub(distance.unsigned_abs())
        } else {
            self.transcript_offset
                .saturating_add(distance as usize)
                .min(max_offset)
        };
        self.transcript_follow_tail = self.transcript_offset >= max_offset;
        self.transcript_center_selected = false;
    }

    fn reset_command_search(&mut self) {
        self.command_query.clear();
        self.command_selected = 0;
        self.command_stem = None;
    }

    fn matching_commands(&self) -> Vec<CommandMatch> {
        matching_commands(&self.commands, &self.command_query)
    }

    fn command_completion_candidates(&self, query: &str) -> Vec<CommandSpec> {
        let matches = matching_commands(&self.commands, query);
        let canonical = matches
            .iter()
            .filter(|command| command.spec.id.to_ascii_lowercase().starts_with(query))
            .map(|command| command.spec.clone())
            .collect::<Vec<_>>();
        if !canonical.is_empty() {
            return canonical;
        }
        let aliases = matches
            .iter()
            .filter(|command| command.name.to_ascii_lowercase().starts_with(query))
            .map(|command| command.spec.clone())
            .collect::<Vec<_>>();
        if !aliases.is_empty() {
            return aliases;
        }
        matches.into_iter().map(|command| command.spec).collect()
    }

    fn complete_command(&mut self, reverse: bool) {
        if self.command_query.chars().any(char::is_whitespace) {
            return;
        }
        let typed = self
            .command_query
            .trim_start_matches([':', '：'])
            .to_ascii_lowercase();
        let stem_applies = self.command_stem.as_ref().is_some_and(|stem| {
            typed.starts_with(stem)
                || self
                    .command_completion_candidates(stem)
                    .iter()
                    .any(|command| command.id.eq_ignore_ascii_case(&typed))
        });
        if !stem_applies {
            self.command_stem = Some(typed.clone());
        }
        let stem = self.command_stem.as_deref().unwrap_or(&typed);
        let candidates = self.command_completion_candidates(stem);
        if candidates.is_empty() {
            return;
        }
        if candidates.len() == 1 {
            self.command_query.clone_from(&candidates[0].id);
            self.command_selected = 0;
            return;
        }
        let names = candidates
            .iter()
            .map(|command| command.id.clone())
            .collect::<Vec<_>>();
        let common = common_command_prefix(&names);
        if common.len() > typed.len() {
            self.command_query = common;
            self.command_selected = 0;
            return;
        }
        let current = names
            .iter()
            .position(|name| name.eq_ignore_ascii_case(&typed));
        let next = match current {
            Some(index) if reverse => (index + names.len() - 1) % names.len(),
            Some(index) => (index + 1) % names.len(),
            None if reverse => names.len() - 1,
            None => 0,
        };
        self.command_query.clone_from(&names[next]);
        self.command_selected = 0;
    }

    fn move_command_selection(&mut self, distance: isize) {
        let count = self.matching_commands().len();
        self.command_selected = wrapped_index(self.command_selected, distance, count);
    }

    fn close_floats(&mut self) {
        if let Some(oauth) = self.oauth.take() {
            oauth.login.cancel();
        }
        self.pty = None;
        self.selector = None;
        self.text_prompt = None;
        self.document = None;
        self.settings = None;
        self.model_selector = None;
        self.tui_document = None;
        self.overlay = None;
        self.overlay_scroll = 0;
    }
}

fn hit_target<T: Copy>(regions: &[HitRegion<T>], mouse: MouseEvent) -> Option<T> {
    regions
        .iter()
        .find(|region| {
            mouse.column >= region.area.x
                && mouse.column < region.area.x.saturating_add(region.area.width)
                && mouse.row >= region.area.y
                && mouse.row < region.area.y.saturating_add(region.area.height)
        })
        .map(|region| region.target)
}

fn wrapped_index(current: usize, distance: isize, count: usize) -> usize {
    if count == 0 {
        return 0;
    }
    let current = current % count;
    if distance < 0 {
        (current + count - distance.unsigned_abs() % count) % count
    } else {
        (current + distance as usize % count) % count
    }
}

fn is_double_click<T: Copy + Eq>(last_click: &mut Option<(T, Instant)>, target: T) -> bool {
    let now = Instant::now();
    let repeated = last_click.as_ref().is_some_and(|(previous, at)| {
        *previous == target && now.duration_since(*at) < DOUBLE_CLICK_INTERVAL
    });
    *last_click = (!repeated).then_some((target, now));
    repeated
}

pub struct TuiServices {
    pub runtime: Arc<AgentRuntime>,
    pub protocols: Vec<ProtocolDescriptor>,
    pub commands: Arc<CommandRegistry>,
    pub tui: Arc<TuiRegistry>,
    pub tasks: TaskManager,
    pub manager: Arc<ConfigManager>,
    pub catalog: Arc<ModelCatalog>,
    pub output: Arc<OutputStore>,
    pub info: TuiInfo,
    pub draft: String,
}

pub struct TuiTerminal {
    terminal: DefaultTerminal,
    first_session: bool,
}

impl TuiTerminal {
    pub fn new() -> Result<Self> {
        let terminal = ratatui::try_init()?;
        if let Err(error) = execute!(stdout(), EnableMouseCapture, EnableBracketedPaste) {
            ratatui::restore();
            return Err(error.into());
        }
        Ok(Self {
            terminal,
            first_session: true,
        })
    }

    pub async fn run(&mut self, services: TuiServices) -> Result<TuiOutcome> {
        let TuiServices {
            runtime,
            protocols,
            commands,
            tui,
            tasks,
            manager,
            catalog,
            output,
            mut info,
            draft,
        } = services;
        info.thinking =
            effective_thinking(&catalog, &info.provider, &info.model, info.thinking).await;
        let session = runtime.session().clone();
        let mut receiver = session.subscribe();
        let keymap = Keymap::load(Some(&info.cwd)).await?;
        let show_splash = std::mem::take(&mut self.first_session);
        let mut app = App::new(protocols, commands, tui, info, keymap, draft, show_splash);
        for event in session.snapshot().await {
            app.apply(event);
        }
        app.finish_hydration();
        if runtime.turn_running().await {
            app.busy = true;
            app.busy_since = Some(Instant::now());
            app.activity = Some(Activity::Thinking);
            style_input(&mut app.input, true);
        }

        let services = LoopServices {
            runtime,
            tasks,
            manager,
            catalog,
            output,
        };
        run_loop(&mut self.terminal, &mut app, services, &mut receiver).await
    }
}

impl Drop for TuiTerminal {
    fn drop(&mut self) {
        let _ = execute!(stdout(), DisableMouseCapture, DisableBracketedPaste);
        ratatui::restore();
    }
}

struct LoopServices {
    runtime: Arc<AgentRuntime>,
    tasks: TaskManager,
    manager: Arc<ConfigManager>,
    catalog: Arc<ModelCatalog>,
    output: Arc<OutputStore>,
}

async fn run_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    services: LoopServices,
    receiver: &mut tokio::sync::broadcast::Receiver<SessionUpdate>,
) -> Result<TuiOutcome> {
    let mut terminal_events = EventStream::new();
    let mut animation = time::interval(Duration::from_millis(90));
    animation.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let (background_tx, mut background_rx) = mpsc::unbounded_channel();
    loop {
        app.info.context_tokens = services.runtime.estimated_context();
        terminal.draw(|frame| render(frame, app))?;
        tokio::select! {
            _ = animation.tick(), if !app.animations_paused() => {
                app.confirm_pending_transcript_click_if_elapsed();
                app.frame = app.frame.wrapping_add(1);
            },
            event = terminal_events.next() => {
                let Some(event) = event else { return persist_and_exit(app, &services, TuiOutcome::Quit).await; };
                match event? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        app.confirm_pending_transcript_click();
                        if app.pty.is_none()
                            && is_ignored_tui_key(key, app.selection.is_some())
                        {
                            continue;
                        }
                        if app.showing_splash() {
                            app.skip_splash();
                        }
                        if app.pty.is_some() {
                            if handle_terminal_key(app, key)? {
                                close_pty(app, "Terminal closed");
                            }
                            continue;
                        }
                        match handle_key(app, key, &services).await {
                            Action::Continue => {}
                            other => {
                                if let Some(outcome) = apply_action(app, &services, background_tx.clone(), other).await? {
                                    return persist_and_exit(app, &services, outcome).await;
                                }
                            }
                        }
                    }
                    Event::Paste(text) => {
                        if let Some(pty) = app.pty.as_mut() {
                            if let Err(error) = pty.terminal.send_paste(&text) {
                                app.set_flash(format!("Terminal paste failed: {error:#}"));
                            }
                        } else {
                            handle_paste(app, text);
                        }
                    }
                    Event::Mouse(mouse) => {
                        if app.showing_splash() {
                            app.skip_splash();
                        }
                        if app.pty.is_some() {
                            if let Err(error) = handle_terminal_mouse(app, mouse) {
                                app.set_flash(format!("Terminal mouse input failed: {error:#}"));
                            }
                            continue;
                        }
                        match handle_mouse(app, mouse, &services).await {
                            Action::Continue => {}
                            other => {
                                if let Some(outcome) = apply_action(app, &services, background_tx.clone(), other).await? {
                                    return persist_and_exit(app, &services, outcome).await;
                                }
                            }
                        }
                    }
                    Event::FocusGained | Event::FocusLost | Event::Resize(_, _) | Event::Key(_) => {}
                }
            }
            event = receiver.recv() => match event {
                Ok(SessionUpdate::Persisted(event)) => app.apply(event),
                Ok(SessionUpdate::Transient(kind)) => app.apply_transient(kind),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    for event in services.runtime.session().snapshot().await {
                        app.apply(event);
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    return persist_and_exit(app, &services, TuiOutcome::Quit).await;
                }
            },
            Some(event) = background_rx.recv() => {
                finish_background(app, &services, event).await;
            },
        }
        if pty_finished(app)? {
            close_pty(app, "Terminal exited");
        }
    }
}

async fn persist_and_exit(
    app: &mut App,
    services: &LoopServices,
    outcome: TuiOutcome,
) -> Result<TuiOutcome> {
    let _ = services
        .runtime
        .session()
        .save_draft(&app.draft_text())
        .await;
    if matches!(&outcome, TuiOutcome::Quit) {
        services.runtime.shutdown().await;
    }
    app.close_floats();
    Ok(outcome)
}

enum BackgroundEvent {
    CatalogRefreshed(Result<ActiveSettings>),
    OauthFinished(Result<OauthToken>),
}

enum Action {
    Continue,
    Quit,
    Submit(String),
    Compact,
    OpenModels(String),
    SelectModel,
    OpenSettings,
    SaveSettings,
    RefreshCatalog,
    Resume(String),
    NewSession,
    StartOauth {
        provider: String,
        method: String,
        extra: std::collections::BTreeMap<String, String>,
    },
    StoreApiKey {
        provider: String,
        key: String,
    },
    Logout {
        provider: String,
    },
    SaveTerminal(String),
    OpenTerminal,
}

async fn apply_action(
    app: &mut App,
    services: &LoopServices,
    background_tx: mpsc::UnboundedSender<BackgroundEvent>,
    action: Action,
) -> Result<Option<TuiOutcome>> {
    match action {
        Action::Continue => Ok(None),
        Action::Quit => Ok(Some(TuiOutcome::Quit)),
        Action::NewSession => Ok(Some(TuiOutcome::NewSession)),
        Action::Resume(id) => {
            if id == app.info.session_id {
                app.set_flash("Already in this session");
                Ok(None)
            } else {
                Ok(Some(TuiOutcome::Resume(id)))
            }
        }
        Action::Submit(prompt) => {
            let _ = services.runtime.session().save_draft("").await;
            if let Err(error) = services.runtime.start_turn(prompt).await {
                app.busy = false;
                app.busy_since = None;
                app.activity = None;
                app.set_flash(format!("Cannot start turn: {error:#}"));
            }
            Ok(None)
        }
        Action::Compact => {
            start_compaction(app, services.runtime.clone());
            Ok(None)
        }
        Action::OpenModels(query) => {
            open_models(
                app,
                &services.runtime,
                &services.manager,
                &services.catalog,
                query,
            )
            .await;
            Ok(None)
        }
        Action::SelectModel => {
            select_model(
                app,
                &services.runtime,
                &services.manager,
                &services.catalog,
                &services.output,
            )
            .await;
            Ok(None)
        }
        Action::OpenSettings => {
            let active = active_for_runtime(&services.manager, &services.runtime).await?;
            app.settings = Some(SettingsState::load(active, &services.catalog).await);
            app.overlay = Some(Overlay::Settings);
            Ok(None)
        }
        Action::SaveSettings => {
            save_settings(
                app,
                &services.runtime,
                &services.manager,
                &services.catalog,
                &services.output,
            )
            .await;
            Ok(None)
        }
        Action::RefreshCatalog => {
            start_catalog_refresh(app, services, background_tx);
            Ok(None)
        }
        Action::StartOauth {
            provider,
            method,
            extra,
        } => {
            start_oauth(app, provider, method, extra, background_tx);
            Ok(None)
        }
        Action::StoreApiKey { provider, key } => {
            store_api_key(app, services, &provider, key).await;
            Ok(None)
        }
        Action::Logout { provider } => {
            logout_provider(app, services, &provider).await;
            Ok(None)
        }
        Action::SaveTerminal(command) => {
            save_terminal(app, services, command).await;
            Ok(None)
        }
        Action::OpenTerminal => {
            open_pty(app);
            Ok(None)
        }
    }
}

fn handle_paste(app: &mut App, text: String) {
    match app.overlay {
        Some(Overlay::Text) => {
            if let Some(prompt) = app.text_prompt.as_mut() {
                prompt.value.push_str(text.trim());
            }
        }
        Some(Overlay::Oauth) => {
            if let Some(oauth) = app.oauth.as_mut() {
                oauth.paste.push_str(text.trim());
            }
        }
        Some(Overlay::Models) => {
            if let Some(selector) = app.model_selector.as_mut() {
                selector.paste(text.trim());
            }
        }
        Some(Overlay::Command) => {
            app.command_query.push_str(text.trim());
            app.command_selected = 0;
            app.command_stem = None;
        }
        Some(Overlay::Selector) => {
            if let Some(selector) = app.selector.as_mut() {
                selector.query.push_str(text.trim());
                selector.rebuild();
            }
        }
        Some(Overlay::Settings) => {
            if let Some(settings) = app.settings.as_mut()
                && settings.editing == Some(EditingSetting::OutputLimit)
            {
                settings
                    .output_limit
                    .extend(text.chars().filter(char::is_ascii_digit));
            }
        }
        Some(Overlay::Composer) => {
            app.input.insert_str(text);
        }
        _ => {}
    }
}

async fn dispatch_ui_command(
    app: &mut App,
    target: CommandTarget,
    services: &LoopServices,
) -> Action {
    let previous_overlay = app.overlay;
    app.overlay = None;
    let command = match target {
        CommandTarget::Core(command) => command,
        CommandTarget::Panel(panel) => {
            let context = TuiPanelContext {
                cwd: app.info.cwd.clone(),
                session_id: app.info.session_id.clone(),
                arguments: String::new(),
            };
            match app.tui.open_panel(&panel, context).await {
                Ok(document) => {
                    app.tui_document = Some(document);
                    app.overlay_scroll = 0;
                    app.overlay = Some(Overlay::Plugin);
                }
                Err(error) => app.set_flash(format!("Plugin panel failed: {error:#}")),
            }
            return Action::Continue;
        }
    };
    match command {
        CoreCommand::Compose => {
            app.skip_splash();
            app.overlay = Some(Overlay::Composer);
            Action::Continue
        }
        CoreCommand::Copy => {
            copy_current_surface(app);
            Action::Continue
        }
        CoreCommand::Tasks => {
            app.task_records = services.tasks.list().await;
            app.selected_task = 0;
            app.overlay_scroll = 0;
            app.overlay = Some(Overlay::Tasks);
            Action::Continue
        }
        CoreCommand::Protocols => {
            app.overlay_scroll = 0;
            app.overlay = Some(Overlay::Protocols);
            Action::Continue
        }
        CoreCommand::Status => {
            if previous_overlay != Some(Overlay::Status) {
                open_status(app);
            }
            Action::Continue
        }
        CoreCommand::Models => Action::OpenModels(String::new()),
        CoreCommand::Effort => {
            open_effort(app, services).await;
            Action::Continue
        }
        CoreCommand::Settings => Action::OpenSettings,
        CoreCommand::Login => open_login(app, &services.catalog).await,
        CoreCommand::Logout => open_logout(app, &services.manager).await,
        CoreCommand::Resume => open_resume(app, services).await,
        CoreCommand::Search => {
            open_search(app);
            Action::Continue
        }
        CoreCommand::NewSession => Action::NewSession,
        CoreCommand::Compact => Action::Compact,
        CoreCommand::Help => {
            app.overlay_scroll = 0;
            app.overlay = Some(Overlay::Help);
            Action::Continue
        }
        CoreCommand::Quit => Action::Quit,
        CoreCommand::SetTerminal => {
            open_set_terminal_prompt(app);
            Action::Continue
        }
        CoreCommand::Terminal => Action::OpenTerminal,
    }
}

fn start_compaction(app: &mut App, runtime: Arc<AgentRuntime>) {
    app.busy = true;
    app.busy_since = Some(Instant::now());
    app.activity = Some(Activity::Compacting);
    app.set_flash("Compacting older model context…");
    tokio::spawn(async move {
        if let Err(error) = runtime.compact().await {
            let _ = runtime
                .session()
                .append(EventKind::Error {
                    text: format!("Context compaction failed: {error:#}"),
                })
                .await;
        }
    });
}

async fn handle_key(app: &mut App, key: KeyEvent, services: &LoopServices) -> Action {
    let key_name = key_name(key);
    if app.selection.is_some() {
        match app.keymap.action("selection", &key_name).as_deref() {
            Some("copy") => copy_current_surface(app),
            Some("close") => app.selection = None,
            _ => {}
        }
        return Action::Continue;
    }
    match app.keymap.action_chain(&[], &key_name).as_deref() {
        Some("quit") => return Action::Quit,
        Some("help") => {
            app.overlay_scroll = 0;
            app.overlay = Some(Overlay::Help);
            return Action::Continue;
        }
        Some("settings") => return Action::OpenSettings,
        Some("model") => return Action::OpenModels(String::new()),
        Some("protocols") => {
            app.overlay_scroll = 0;
            app.overlay = Some(Overlay::Protocols);
            return Action::Continue;
        }
        Some("tasks") => {
            return dispatch_ui_command(app, CommandTarget::Core(CoreCommand::Tasks), services)
                .await;
        }
        Some("status") => {
            return dispatch_ui_command(app, CommandTarget::Core(CoreCommand::Status), services)
                .await;
        }
        Some("copy") => {
            copy_current_surface(app);
            return Action::Continue;
        }
        _ => {}
    }
    if let Some(overlay) = app.overlay {
        return handle_overlay_key(app, key, &key_name, overlay, services).await;
    }
    match app.keymap.action("main", &key_name).as_deref() {
        Some("quit") => Action::Quit,
        Some("compose") => {
            app.overlay = Some(Overlay::Composer);
            Action::Continue
        }
        Some("command") => {
            app.reset_command_search();
            app.overlay = Some(Overlay::Command);
            Action::Continue
        }
        Some("jump_reasoning") => {
            app.jump_to(JumpKind::Reasoning);
            Action::Continue
        }
        Some("jump_tools") => {
            app.jump_to(JumpKind::Tool);
            Action::Continue
        }
        Some("jump_user") => {
            app.jump_to(JumpKind::User);
            Action::Continue
        }
        Some("next") => {
            app.move_selection(1);
            Action::Continue
        }
        Some("previous") => {
            app.move_selection(-1);
            Action::Continue
        }
        Some("page_down") => {
            app.move_selection(8);
            Action::Continue
        }
        Some("page_up") => {
            app.move_selection(-8);
            Action::Continue
        }
        Some("first") => {
            if let Some(index) = app.filtered_indices().first().copied() {
                app.selected_block = index;
                app.transcript_follow_tail = false;
                app.transcript_center_selected = true;
            }
            Action::Continue
        }
        Some("last") => {
            if let Some(index) = app.filtered_indices().last().copied() {
                app.selected_block = index;
                app.transcript_follow_tail = true;
                app.transcript_center_selected = true;
            }
            Action::Continue
        }
        Some("toggle") => {
            app.toggle_selected();
            Action::Continue
        }
        Some("open") => {
            app.open_selected_document();
            Action::Continue
        }
        Some("clear") => {
            app.jump = JumpKind::All;
            Action::Continue
        }
        Some("copy") => {
            copy_current_surface(app);
            Action::Continue
        }
        Some(action) => {
            if let Some(target) = app.commands.target_for_action(action) {
                dispatch_ui_command(app, target, services).await
            } else {
                Action::Continue
            }
        }
        None => Action::Continue,
    }
}

async fn handle_overlay_key(
    app: &mut App,
    key: KeyEvent,
    key_name: &str,
    overlay: Overlay,
    services: &LoopServices,
) -> Action {
    match overlay {
        Overlay::Composer => match app.keymap.action("composer", key_name).as_deref() {
            Some("submit") => app.submit().map_or(Action::Continue, Action::Submit),
            Some("newline") => {
                app.input.insert_newline();
                Action::Continue
            }
            Some("close") => {
                app.overlay = None;
                Action::Continue
            }
            Some("quit") => Action::Quit,
            _ => {
                app.input.input(key);
                Action::Continue
            }
        },
        Overlay::Command => match apply_command_key(app, key, key_name) {
            CommandKey::Quit => Action::Quit,
            CommandKey::Confirm => confirm_command(app, services).await,
            CommandKey::Continue => Action::Continue,
        },
        Overlay::Selector => handle_selector_key(app, key, key_name, services).await,
        Overlay::Text => handle_text_key(app, key, key_name),
        Overlay::Oauth => handle_oauth_key(app, key, key_name),
        Overlay::Tasks => match app
            .keymap
            .action_chain(&["tasks", "list"], key_name)
            .as_deref()
        {
            Some("quit") => Action::Quit,
            Some("close") => {
                app.overlay = None;
                Action::Continue
            }
            Some("previous") => {
                app.selected_task = wrapped_index(app.selected_task, -1, app.task_records.len());
                Action::Continue
            }
            Some("next") => {
                app.selected_task = wrapped_index(app.selected_task, 1, app.task_records.len());
                Action::Continue
            }
            Some("cancel") => {
                if let Some(id) = app
                    .task_records
                    .get(app.selected_task)
                    .map(|task| task.id.clone())
                {
                    let _ = services.tasks.cancel(&id).await;
                    app.task_records = services.tasks.list().await;
                }
                Action::Continue
            }
            Some("page_up") => {
                app.overlay_scroll = app.overlay_scroll.saturating_sub(8);
                Action::Continue
            }
            Some("page_down") => {
                app.overlay_scroll = app.overlay_scroll.saturating_add(8);
                Action::Continue
            }
            _ => Action::Continue,
        },
        Overlay::Models => {
            let Some(selector) = app.model_selector.as_mut() else {
                app.overlay = None;
                return Action::Continue;
            };
            match app.keymap.action("models", key_name).as_deref() {
                Some("quit") => Action::Quit,
                Some("close") => {
                    app.overlay = None;
                    Action::Continue
                }
                Some("previous") => {
                    selector.move_selection(-1);
                    Action::Continue
                }
                Some("next") => {
                    selector.move_selection(1);
                    Action::Continue
                }
                Some("page_up") => {
                    selector.move_selection(-10);
                    Action::Continue
                }
                Some("page_down") => {
                    selector.move_selection(10);
                    Action::Continue
                }
                Some("first") => {
                    selector.first();
                    Action::Continue
                }
                Some("last") => {
                    selector.last();
                    Action::Continue
                }
                Some("confirm") => Action::SelectModel,
                Some("backspace") => {
                    selector.backspace();
                    Action::Continue
                }
                Some("refresh") => Action::RefreshCatalog,
                _ => {
                    if let KeyCode::Char(character) = key.code
                        && !key.modifiers.intersects(
                            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                        )
                    {
                        selector.push(character);
                    }
                    Action::Continue
                }
            }
        }
        Overlay::Settings => handle_settings_key(app, key, key_name),
        Overlay::Document => match app.keymap.action("document", key_name).as_deref() {
            Some("quit") => Action::Quit,
            Some("close") => {
                app.document = None;
                app.overlay = None;
                Action::Continue
            }
            Some("scroll_up") => {
                app.overlay_scroll = app.overlay_scroll.saturating_sub(1);
                Action::Continue
            }
            Some("scroll_down") => {
                app.overlay_scroll = app.overlay_scroll.saturating_add(1);
                Action::Continue
            }
            Some("page_up") => {
                app.overlay_scroll = app.overlay_scroll.saturating_sub(8);
                Action::Continue
            }
            Some("page_down") => {
                app.overlay_scroll = app.overlay_scroll.saturating_add(8);
                Action::Continue
            }
            _ => Action::Continue,
        },
        Overlay::Terminal => Action::Continue,
        Overlay::Status | Overlay::Help | Overlay::Protocols | Overlay::Plugin => {
            match app.keymap.action("list", key_name).as_deref() {
                Some("quit") => Action::Quit,
                Some("close") => {
                    app.overlay = None;
                    Action::Continue
                }
                Some("previous") => {
                    app.overlay_scroll = app.overlay_scroll.saturating_sub(1);
                    Action::Continue
                }
                Some("next") => {
                    app.overlay_scroll = app.overlay_scroll.saturating_add(1);
                    Action::Continue
                }
                Some("page_up") => {
                    app.overlay_scroll = app.overlay_scroll.saturating_sub(8);
                    Action::Continue
                }
                Some("page_down") => {
                    app.overlay_scroll = app.overlay_scroll.saturating_add(8);
                    Action::Continue
                }
                _ => Action::Continue,
            }
        }
    }
}

async fn confirm_command(app: &mut App, services: &LoopServices) -> Action {
    let command = app.matching_commands().get(app.command_selected).cloned();
    app.overlay = None;
    app.reset_command_search();
    if let Some(command) = command {
        return dispatch_ui_command(app, command.spec.target, services).await;
    }
    Action::Continue
}

enum CommandKey {
    Continue,
    Confirm,
    Quit,
}

fn apply_command_key(app: &mut App, key: KeyEvent, key_name: &str) -> CommandKey {
    match app.keymap.action("command", key_name).as_deref() {
        Some("quit") => CommandKey::Quit,
        Some("cancel") => {
            app.reset_command_search();
            app.overlay = None;
            CommandKey::Continue
        }
        Some("backspace") => {
            app.command_query.pop();
            app.command_selected = 0;
            app.command_stem = None;
            CommandKey::Continue
        }
        Some("previous") => {
            app.move_command_selection(-1);
            CommandKey::Continue
        }
        Some("next") => {
            app.move_command_selection(1);
            CommandKey::Continue
        }
        Some("confirm") => CommandKey::Confirm,
        Some("complete") => {
            app.complete_command(false);
            CommandKey::Continue
        }
        Some("complete_previous") => {
            app.complete_command(true);
            CommandKey::Continue
        }
        _ => {
            if let KeyCode::Char(character) = key.code
                && !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
            {
                app.command_query.push(character);
                app.command_selected = 0;
                app.command_stem = None;
            }
            CommandKey::Continue
        }
    }
}

enum SelectorKey {
    Continue,
    Confirm,
    Quit,
}

async fn handle_selector_key(
    app: &mut App,
    key: KeyEvent,
    key_name: &str,
    services: &LoopServices,
) -> Action {
    match apply_selector_key(app, key, key_name) {
        SelectorKey::Quit => Action::Quit,
        SelectorKey::Confirm => confirm_selector(app, services).await,
        SelectorKey::Continue => Action::Continue,
    }
}

fn apply_selector_key(app: &mut App, key: KeyEvent, key_name: &str) -> SelectorKey {
    let Some(selector) = app.selector.as_mut() else {
        app.overlay = None;
        return SelectorKey::Continue;
    };
    match app.keymap.action("selector", key_name).as_deref() {
        Some("quit") => SelectorKey::Quit,
        Some("close") => {
            app.selector = None;
            app.overlay = None;
            SelectorKey::Continue
        }
        Some("previous") => {
            selector.move_selection(-1);
            SelectorKey::Continue
        }
        Some("next") => {
            selector.move_selection(1);
            SelectorKey::Continue
        }
        Some("confirm") => SelectorKey::Confirm,
        Some("backspace") => {
            selector.query.pop();
            selector.rebuild();
            SelectorKey::Continue
        }
        _ => {
            if let KeyCode::Char(character) = key.code
                && !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
            {
                selector.query.push(character);
                selector.rebuild();
            }
            SelectorKey::Continue
        }
    }
}

async fn confirm_selector(app: &mut App, services: &LoopServices) -> Action {
    let Some(selector) = app.selector.take() else {
        app.overlay = None;
        return Action::Continue;
    };
    let Some(item) = selector.selected_item().cloned() else {
        app.overlay = None;
        app.set_flash("Nothing is selected");
        return Action::Continue;
    };
    app.overlay = None;
    match selector.kind {
        SelectorKind::LoginProvider => open_login_method(app, item.id),
        SelectorKind::LoginMethod { provider } => match item.id.as_str() {
            "api_key" => {
                open_api_key_prompt(app, provider);
                Action::Continue
            }
            "oauth" if provider == "github-copilot" => {
                open_copilot_domain_prompt(app);
                Action::Continue
            }
            method => Action::StartOauth {
                provider,
                method: method.to_string(),
                extra: std::collections::BTreeMap::new(),
            },
        },
        SelectorKind::Logout => Action::Logout { provider: item.id },
        SelectorKind::Resume => Action::Resume(item.id),
        SelectorKind::Search => {
            let selected = item
                .id
                .parse::<usize>()
                .ok()
                .is_some_and(|index| app.select_search_result(index));
            if !selected {
                app.set_flash("Search result is no longer available");
            }
            Action::Continue
        }
        SelectorKind::Effort { provider, model } => {
            let Ok(thinking) = item.id.parse::<ThinkingLevel>() else {
                app.set_flash("The selected effort is invalid");
                return Action::Continue;
            };
            set_effort(app, services, &provider, &model, thinking).await;
            Action::Continue
        }
    }
}

fn handle_text_key(app: &mut App, key: KeyEvent, key_name: &str) -> Action {
    let Some(prompt) = app.text_prompt.as_mut() else {
        app.overlay = None;
        return Action::Continue;
    };
    match app.keymap.action("text", key_name).as_deref() {
        Some("quit") => Action::Quit,
        Some("cancel") => {
            app.text_prompt = None;
            app.overlay = None;
            Action::Continue
        }
        Some("backspace") => {
            prompt.value.pop();
            Action::Continue
        }
        Some("confirm") => {
            let prompt = app.text_prompt.take().expect("checked");
            app.overlay = None;
            match prompt.purpose {
                TextPurpose::ApiKey { provider } => Action::StoreApiKey {
                    provider,
                    key: prompt.value,
                },
                TextPurpose::CopilotDomain => Action::StartOauth {
                    provider: "github-copilot".to_string(),
                    method: "oauth".to_string(),
                    extra: std::collections::BTreeMap::from([("domain".to_string(), prompt.value)]),
                },
                TextPurpose::SetTerminal => Action::SaveTerminal(prompt.value),
            }
        }
        _ => {
            if let KeyCode::Char(character) = key.code
                && !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
            {
                prompt.value.push(character);
            }
            Action::Continue
        }
    }
}

fn handle_oauth_key(app: &mut App, key: KeyEvent, key_name: &str) -> Action {
    let Some(oauth) = app.oauth.as_mut() else {
        app.overlay = None;
        return Action::Continue;
    };
    match app.keymap.action("oauth", key_name).as_deref() {
        Some("quit") => Action::Quit,
        Some("cancel") => {
            oauth.login.cancel();
            app.oauth = None;
            app.overlay = None;
            app.set_flash("Login cancelled");
            Action::Continue
        }
        Some("backspace") => {
            oauth.paste.pop();
            Action::Continue
        }
        Some("confirm") => {
            if !oauth.paste.trim().is_empty() {
                oauth.login.submit_paste(&oauth.paste);
                oauth.message = "Exchanging authorization code…".to_string();
            }
            Action::Continue
        }
        _ => {
            if let KeyCode::Char(character) = key.code
                && !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
            {
                oauth.paste.push(character);
            }
            Action::Continue
        }
    }
}

fn handle_settings_key(app: &mut App, key: KeyEvent, key_name: &str) -> Action {
    let Some(settings) = app.settings.as_mut() else {
        app.overlay = None;
        return Action::Continue;
    };
    if settings.editing.is_some() {
        match app.keymap.action("text", key_name).as_deref() {
            Some("quit") => return Action::Quit,
            Some("cancel" | "confirm") => settings.editing = None,
            Some("backspace") => {
                if settings.editing == Some(EditingSetting::OutputLimit) {
                    settings.output_limit.pop();
                }
            }
            _ => {
                if let KeyCode::Char(character) = key.code
                    && character.is_ascii_digit()
                    && settings.editing == Some(EditingSetting::OutputLimit)
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
                {
                    settings.output_limit.push(character);
                }
            }
        }
        return Action::Continue;
    }
    match app.keymap.action("settings", key_name).as_deref() {
        Some("quit") => Action::Quit,
        Some("close") => {
            app.overlay = None;
            Action::Continue
        }
        Some("previous") => {
            settings.selected = wrapped_index(settings.selected, -1, 4);
            Action::Continue
        }
        Some("next") => {
            settings.selected = wrapped_index(settings.selected, 1, 4);
            Action::Continue
        }
        Some("edit") => match settings.selected {
            0 => Action::OpenModels(String::new()),
            1 => Action::Continue,
            2 => {
                settings.cycle_thinking();
                Action::Continue
            }
            3 => {
                settings.editing = Some(EditingSetting::OutputLimit);
                settings.output_limit.clear();
                Action::Continue
            }
            _ => Action::Continue,
        },
        Some("save") => Action::SaveSettings,
        Some("refresh") => Action::RefreshCatalog,
        _ => Action::Continue,
    }
}

async fn handle_mouse(app: &mut App, mouse: MouseEvent, services: &LoopServices) -> Action {
    if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
        app.confirm_pending_transcript_click();
    }
    if consume_copy_click_release(app, mouse) {
        return Action::Continue;
    }
    if is_selection_copy_click(app, mouse) {
        copy_current_surface(app);
        app.copy_click_release_pending = true;
        return Action::Continue;
    }
    if close_document_on_outside_click(app, mouse) {
        return Action::Continue;
    }
    if begin_direct_transcript_selection(app, mouse) {
        return Action::Continue;
    }
    if update_mouse_selection(app, mouse, app.overlay.is_none()) {
        return Action::Continue;
    }
    match mouse.kind {
        MouseEventKind::ScrollUp => match app.overlay {
            Some(Overlay::Command) => {
                app.move_command_selection(-1);
            }
            Some(Overlay::Selector) => {
                if let Some(selector) = app.selector.as_mut() {
                    selector.move_selection(-1);
                }
            }
            Some(Overlay::Tasks) => {
                app.selected_task = wrapped_index(app.selected_task, -1, app.task_records.len());
            }
            Some(Overlay::Models) => {
                if let Some(selector) = app.model_selector.as_mut() {
                    selector.move_selection(-3);
                }
            }
            Some(Overlay::Settings) => {
                if let Some(settings) = app.settings.as_mut() {
                    settings.selected = wrapped_index(settings.selected, -1, 4);
                }
            }
            Some(Overlay::Composer | Overlay::Text | Overlay::Oauth | Overlay::Terminal) => {}
            Some(_) => app.overlay_scroll = app.overlay_scroll.saturating_sub(3),
            None => app.scroll_transcript(-3),
        },
        MouseEventKind::ScrollDown => match app.overlay {
            Some(Overlay::Command) => {
                app.move_command_selection(1);
            }
            Some(Overlay::Selector) => {
                if let Some(selector) = app.selector.as_mut() {
                    selector.move_selection(1);
                }
            }
            Some(Overlay::Tasks) => {
                app.selected_task = wrapped_index(app.selected_task, 1, app.task_records.len());
            }
            Some(Overlay::Models) => {
                if let Some(selector) = app.model_selector.as_mut() {
                    selector.move_selection(3);
                }
            }
            Some(Overlay::Settings) => {
                if let Some(settings) = app.settings.as_mut() {
                    settings.selected = wrapped_index(settings.selected, 1, 4);
                }
            }
            Some(Overlay::Composer | Overlay::Text | Overlay::Oauth | Overlay::Terminal) => {}
            Some(_) => app.overlay_scroll = app.overlay_scroll.saturating_add(3),
            None => app.scroll_transcript(3),
        },
        MouseEventKind::Down(MouseButton::Left) => {
            let Some(target) = hit_target(&app.hit_regions, mouse) else {
                app.confirm_pending_transcript_click();
                return Action::Continue;
            };
            if let AppHit::Transcript(index) = target {
                app.queue_transcript_click(index);
                return Action::Continue;
            }
            app.confirm_pending_transcript_click();
            let activate = is_double_click(&mut app.last_click, target);
            match target {
                AppHit::Transcript(_) => unreachable!(),
                AppHit::Palette(index) => {
                    app.command_selected = index;
                    return confirm_command(app, services).await;
                }
                AppHit::Task(index) => app.selected_task = index,
                AppHit::Model(index) => {
                    if let Some(selector) = app.model_selector.as_mut() {
                        selector.select_position(index);
                        if activate {
                            return Action::SelectModel;
                        }
                    }
                }
                AppHit::Setting(index) => {
                    if let Some(settings) = app.settings.as_mut() {
                        settings.selected = index;
                        if activate {
                            match index {
                                0 => return Action::OpenModels(String::new()),
                                2 => settings.cycle_thinking(),
                                3 => {
                                    settings.editing = Some(EditingSetting::OutputLimit);
                                    settings.output_limit.clear();
                                }
                                _ => {}
                            }
                        }
                    }
                }
                AppHit::Selector(index) => {
                    let confirm = app
                        .selector
                        .as_mut()
                        .is_some_and(|selector| selector.select_from_click(index, activate));
                    if confirm {
                        return confirm_selector(app, services).await;
                    }
                }
                AppHit::Status => open_status(app),
            }
        }
        _ => {}
    }
    Action::Continue
}

fn is_selection_copy_click(app: &App, mouse: MouseEvent) -> bool {
    app.selection.is_some() && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Right))
}

fn consume_copy_click_release(app: &mut App, mouse: MouseEvent) -> bool {
    if !app.copy_click_release_pending {
        return false;
    }
    match mouse.kind {
        MouseEventKind::Up(_) => {
            app.copy_click_release_pending = false;
            true
        }
        MouseEventKind::Drag(_) => true,
        MouseEventKind::Down(_) => {
            app.copy_click_release_pending = false;
            false
        }
        _ => false,
    }
}

fn close_document_on_outside_click(app: &mut App, mouse: MouseEvent) -> bool {
    if app.overlay != Some(Overlay::Document)
        || !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
        || !app
            .overlay_bounds
            .is_some_and(|area| !area.contains((mouse.column, mouse.row).into()))
    {
        return false;
    }
    app.overlay = None;
    app.overlay_scroll = 0;
    app.selection = None;
    true
}

fn begin_direct_transcript_selection(app: &mut App, mouse: MouseEvent) -> bool {
    if app.overlay.is_some() || !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
        return false;
    }
    let Some(AppHit::Transcript(index)) = hit_target(&app.hit_regions, mouse) else {
        return false;
    };
    if !app
        .blocks
        .get(index)
        .is_some_and(|block| matches!(block.kind, BlockKind::User | BlockKind::Assistant))
    {
        return false;
    }
    app.confirm_pending_transcript_click();
    app.selected_block = index;
    app.transcript_follow_tail = false;
    update_mouse_selection(app, mouse, false)
}

fn open_status(app: &mut App) {
    app.overlay_scroll = 0;
    app.overlay = Some(Overlay::Status);
}

async fn open_login(app: &mut App, catalog: &ModelCatalog) -> Action {
    let current = app.info.provider.clone();
    let mut seen = std::collections::BTreeSet::new();
    let mut items = Vec::new();
    for provider in catalog.providers().await {
        seen.insert(provider.clone());
        items.push(login_provider_item(&provider, &current));
    }
    for provider in OauthProvider::ALL {
        if seen.insert(provider.id().to_string()) {
            items.push(login_provider_item(provider.id(), &current));
        }
    }
    if items.is_empty() {
        app.set_flash("No providers available to log in");
        return Action::Continue;
    }
    app.selector = Some(SelectorState::new(
        SelectorKind::LoginProvider,
        "LOGIN",
        items,
    ));
    app.overlay = Some(Overlay::Selector);
    Action::Continue
}

fn login_provider_item(provider: &str, current: &str) -> SelectorItem {
    let description = match OauthProvider::from_id(provider) {
        Some(kind) if kind.offers_api_key() => format!("OAuth or API key · {}", kind.name()),
        Some(kind) => format!("OAuth · {}", kind.name()),
        None => "API key".to_string(),
    };
    SelectorItem {
        id: provider.to_string(),
        title: provider.to_string(),
        description: if provider == current {
            format!("{description} · current")
        } else {
            description
        },
        search_text: None,
    }
}

fn open_login_method(app: &mut App, provider: String) -> Action {
    let mut items = Vec::new();
    if let Some(kind) = OauthProvider::from_id(&provider) {
        items.extend(kind.methods().iter().map(|method| SelectorItem {
            id: method.id.to_string(),
            title: method.label.to_string(),
            description: method.description.to_string(),
            search_text: None,
        }));
    }
    let offers_key = OauthProvider::from_id(&provider)
        .map(OauthProvider::offers_api_key)
        .unwrap_or(true);
    if offers_key {
        items.push(SelectorItem {
            id: "api_key".to_string(),
            title: "API key".to_string(),
            description: format!("Paste an API key for {provider}"),
            search_text: None,
        });
    }
    if items.len() == 1 && items[0].id == "api_key" {
        open_api_key_prompt(app, provider);
        return Action::Continue;
    }
    if items.is_empty() {
        app.set_flash(format!("No login methods for {provider}"));
        return Action::Continue;
    }
    app.selector = Some(SelectorState::new(
        SelectorKind::LoginMethod {
            provider: provider.clone(),
        },
        format!("LOGIN · {provider}"),
        items,
    ));
    app.overlay = Some(Overlay::Selector);
    Action::Continue
}

fn open_api_key_prompt(app: &mut App, provider: String) {
    app.text_prompt = Some(TextPrompt {
        title: format!("API KEY · {provider}"),
        message: format!("Paste the API key for {provider}. Enter saves, Esc cancels."),
        value: String::new(),
        secret: true,
        purpose: TextPurpose::ApiKey { provider },
    });
    app.overlay = Some(Overlay::Text);
}

fn open_copilot_domain_prompt(app: &mut App) {
    app.text_prompt = Some(TextPrompt {
        title: "GITHUB COPILOT".to_string(),
        message: "GitHub Enterprise URL/domain (blank for github.com)".to_string(),
        value: String::new(),
        secret: false,
        purpose: TextPurpose::CopilotDomain,
    });
    app.overlay = Some(Overlay::Text);
}

fn open_set_terminal_prompt(app: &mut App) {
    app.text_prompt = Some(TextPrompt {
        title: "SET TERMINAL".to_string(),
        message: "Command used by :terminal, for example pwsh or bash. Enter saves, Esc cancels."
            .to_string(),
        value: app.info.terminal.clone().unwrap_or_default(),
        secret: false,
        purpose: TextPurpose::SetTerminal,
    });
    app.overlay = Some(Overlay::Text);
}

async fn save_terminal(app: &mut App, services: &LoopServices, command: String) {
    let result = services.manager.set_terminal(Some(command.clone())).await;
    match result {
        Ok(active) => {
            app.info.terminal.clone_from(&active.terminal);
            app.set_flash(match active.terminal.as_deref() {
                Some(terminal) => format!("Default terminal set to {terminal}"),
                None => "Default terminal cleared".to_string(),
            });
        }
        Err(error) => app.set_flash(format!("Could not save terminal: {error:#}")),
    }
}

fn open_pty(app: &mut App) {
    let command = app.info.terminal.clone().unwrap_or_default();
    if command.trim().is_empty() {
        app.set_flash("尚未配置，请运行 :set-terminal");
        return;
    }
    app.close_floats();
    let size = crossterm::terminal::size().unwrap_or((80, 24));
    let frame = Rect::new(0, 0, size.0, size.1);
    let area = overlay_area(frame, Overlay::Terminal);
    let inner = area.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    match start_embedded_terminal(&command, &app.info.cwd, inner) {
        Ok(terminal) => {
            app.selection = None;
            app.pty = Some(FloatTerminal {
                terminal,
                area,
                last_escape: None,
            });
            app.overlay = Some(Overlay::Terminal);
        }
        Err(error) => app.set_flash(format!("Terminal failed: {error:#}")),
    }
}

fn start_embedded_terminal(command: &str, cwd: &Path, inner: Rect) -> Result<EmbeddedTerminal> {
    let builder = terminal_command(command)?;
    EmbeddedTerminal::start(builder, cwd, inner.height, inner.width)
        .with_context(|| format!("cannot start `{command}`"))
}

fn terminal_command(command: &str) -> Result<CommandBuilder> {
    let mut arguments = shell_words::split(command).context("cannot parse terminal command")?;
    if arguments.is_empty() {
        bail!("terminal command is empty");
    }
    let executable = arguments.remove(0);
    let mut builder = CommandBuilder::new(executable);
    builder.args(arguments);
    Ok(builder)
}

fn handle_terminal_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    let key_name = key_name(key);
    if app.selection.is_some() {
        match app.keymap.action("selection", &key_name).as_deref() {
            Some("copy") => copy_current_surface(app),
            Some("close") => app.selection = None,
            _ => {}
        }
        return Ok(false);
    }
    match app.keymap.action("terminal", &key_name).as_deref() {
        Some("copy") => {
            copy_current_surface(app);
            return Ok(false);
        }
        Some("close") => return Ok(true),
        Some("escape") => {
            let now = Instant::now();
            let pty = app.pty.as_mut().expect("checked by caller");
            if pty
                .last_escape
                .is_some_and(|at| now.duration_since(at) < Duration::from_millis(500))
            {
                return Ok(true);
            }
            pty.last_escape = Some(now);
            pty.terminal.send_key(key)?;
            return Ok(false);
        }
        _ => {}
    }
    let pty = app.pty.as_mut().expect("checked by caller");
    pty.last_escape = None;
    pty.terminal.send_key(key)?;
    Ok(false)
}

fn handle_terminal_mouse(app: &mut App, mouse: MouseEvent) -> Result<()> {
    if consume_copy_click_release(app, mouse) {
        return Ok(());
    }
    if is_selection_copy_click(app, mouse) {
        copy_current_surface(app);
        app.copy_click_release_pending = true;
        return Ok(());
    }
    if update_mouse_selection(app, mouse, true) {
        return Ok(());
    }
    let pty = app.pty.as_mut().expect("checked by caller");
    let inner = pty.area.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    if mouse.column >= inner.x
        && mouse.column < inner.right()
        && mouse.row >= inner.y
        && mouse.row < inner.bottom()
    {
        pty.terminal.send_mouse(
            mouse,
            mouse.column.saturating_sub(inner.x),
            mouse.row.saturating_sub(inner.y),
        )?;
    }
    Ok(())
}

fn pty_finished(app: &mut App) -> Result<bool> {
    let Some(pty) = app.pty.as_mut() else {
        return Ok(false);
    };
    Ok(pty.terminal.try_wait()?.is_some())
}

fn close_pty(app: &mut App, message: &str) {
    app.pty = None;
    if app.overlay == Some(Overlay::Terminal) {
        app.overlay = None;
    }
    app.selection = None;
    app.set_flash(message);
}

async fn open_logout(app: &mut App, manager: &ConfigManager) -> Action {
    let items = manager
        .stored_credentials()
        .await
        .into_iter()
        .map(|entry| SelectorItem {
            id: entry.provider.clone(),
            title: entry.provider,
            description: entry.kind,
            search_text: None,
        })
        .collect::<Vec<_>>();
    if items.is_empty() {
        app.set_flash("No stored credentials to remove");
        return Action::Continue;
    }
    app.selector = Some(SelectorState::new(SelectorKind::Logout, "LOGOUT", items));
    app.overlay = Some(Overlay::Selector);
    Action::Continue
}

fn open_search(app: &mut App) {
    if app.blocks.is_empty() {
        app.set_flash("No conversation text to search");
        return;
    }
    let items = app
        .blocks
        .iter()
        .enumerate()
        .map(|(index, block)| SelectorItem {
            id: index.to_string(),
            title: block.title.clone(),
            description: search_line_preview(&block.text, "", 180),
            search_text: Some(block.text.clone()),
        })
        .collect();
    app.selector = Some(SelectorState::new(SelectorKind::Search, "SEARCH", items));
    app.overlay = Some(Overlay::Selector);
}

async fn open_resume(app: &mut App, services: &LoopServices) -> Action {
    match services.runtime.session().list_for_project().await {
        Ok(sessions) => {
            if sessions.is_empty() {
                app.set_flash("No sessions in this project");
                return Action::Continue;
            }
            let current = app.info.session_id.clone();
            let mut items = Vec::with_capacity(sessions.len());
            for session in sessions {
                let thinking = effective_thinking(
                    &services.catalog,
                    &session.provider,
                    &session.model,
                    session.thinking,
                )
                .await;
                items.push(resume_item(&current, session, thinking));
            }
            app.selector = Some(SelectorState::new(SelectorKind::Resume, "RESUME", items));
            app.overlay = Some(Overlay::Selector);
        }
        Err(error) => app.set_flash(format!("Could not list sessions: {error:#}")),
    }
    Action::Continue
}

fn resume_item(current: &str, session: SessionSummary, thinking: ThinkingLevel) -> SelectorItem {
    let marker = if session.id == current { "● " } else { "" };
    SelectorItem {
        id: session.id.clone(),
        title: format!("{marker}{}", session.id),
        description: format!(
            "{}/{} · effort {} · {}",
            session.provider,
            session.model,
            thinking,
            single_line_preview(&session.preview, 48)
        ),
        search_text: None,
    }
}

fn start_oauth(
    app: &mut App,
    provider: String,
    method: String,
    extra: std::collections::BTreeMap<String, String>,
    sender: mpsc::UnboundedSender<BackgroundEvent>,
) {
    match oauth::start_login(&provider, &method, &extra) {
        Ok((login, done)) => {
            tokio::spawn(async move {
                let result = done
                    .await
                    .unwrap_or_else(|_| Err(anyhow!("OAuth login was cancelled")));
                let _ = sender.send(BackgroundEvent::OauthFinished(result));
            });
            let display = login.display();
            app.oauth = Some(OauthState {
                message: display.instructions,
                login,
                paste: String::new(),
                provider,
            });
            app.overlay = Some(Overlay::Oauth);
        }
        Err(error) => app.set_flash(format!("Could not start OAuth: {error:#}")),
    }
}

async fn store_api_key(app: &mut App, services: &LoopServices, provider: &str, key: String) {
    if key.trim().is_empty() {
        app.set_flash("API key cannot be empty");
        return;
    }
    let result = async {
        services.manager.set_api_key(provider, key).await?;
        let active = active_for_runtime(&services.manager, &services.runtime).await?;
        apply_active(
            app,
            &services.runtime,
            &services.catalog,
            &services.output,
            &active,
        )
        .await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    app.set_flash(match result {
        Ok(()) => format!("Saved API key for {provider}"),
        Err(error) => format!("Could not save API key: {error:#}"),
    });
}

async fn logout_provider(app: &mut App, services: &LoopServices, provider: &str) {
    let result = async {
        services.manager.clear_api_key(provider).await?;
        let active = active_for_runtime(&services.manager, &services.runtime).await?;
        apply_active(
            app,
            &services.runtime,
            &services.catalog,
            &services.output,
            &active,
        )
        .await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    app.set_flash(match result {
        Ok(()) => format!("Removed stored credential for {provider}"),
        Err(error) => format!("Could not log out {provider}: {error:#}"),
    });
}

async fn open_models(
    app: &mut App,
    runtime: &AgentRuntime,
    manager: &ConfigManager,
    catalog: &ModelCatalog,
    query: String,
) {
    let active = match active_for_runtime(manager, runtime).await {
        Ok(active) => active,
        Err(error) => {
            app.set_flash(format!("Could not resolve session model: {error:#}"));
            return;
        }
    };
    let selector = ModelSelector::load(catalog, &active, query).await;
    if selector.model_count() == 0 {
        app.set_flash("No runnable models cached · :settings then refresh, or add models.json");
    }
    app.model_selector = Some(selector);
    app.overlay = Some(Overlay::Models);
}

async fn select_model(
    app: &mut App,
    runtime: &AgentRuntime,
    manager: &ConfigManager,
    catalog: &ModelCatalog,
    output: &OutputStore,
) {
    let Some(model) = app
        .model_selector
        .as_ref()
        .and_then(ModelSelector::selected)
        .cloned()
    else {
        app.set_flash("No model matches the current search");
        return;
    };
    let requested = format!("{}/{}", model.provider, model.id);
    let result = async {
        manager.set_model(&model.provider, &model.id).await?;
        let active = manager.current().await;
        apply_active(app, runtime, catalog, output, &active).await?;
        Ok::<_, anyhow::Error>(active)
    }
    .await;
    match result {
        Ok(active) => {
            app.overlay = None;
            app.model_selector = None;
            app.settings = None;
            app.set_flash(
                if active.provider == model.provider && active.model == model.id {
                    format!("Model changed to {requested}")
                } else {
                    format!(
                        "Saved {requested}, but {} keeps {}/{} active",
                        active.model_source.label(),
                        active.provider,
                        active.model
                    )
                },
            );
        }
        Err(error) => app.set_flash(format!("Could not select {requested}: {error:#}")),
    }
}

async fn open_effort(app: &mut App, services: &LoopServices) {
    let active = match active_for_runtime(&services.manager, &services.runtime).await {
        Ok(active) => active,
        Err(error) => {
            app.set_flash(format!("Could not resolve session model: {error:#}"));
            return;
        }
    };
    if !active.model_configured() {
        app.set_flash("No active model; choose one with :model");
        return;
    }
    let Some(model) = active.catalog_model(&services.catalog).await else {
        app.set_flash(format!(
            "Model {}/{} is not available in the runnable Pi catalog",
            active.provider, active.model
        ));
        return;
    };
    app.selector = Some(effort_selector(&active, &model));
    app.overlay = Some(Overlay::Selector);
}

fn effort_selector(active: &ActiveSettings, model: &CatalogModel) -> SelectorState {
    let available = ThinkingLevel::ALL
        .into_iter()
        .filter(|level| model.supports_thinking_level(*level))
        .collect::<Vec<_>>();
    let effective = clamp_thinking_level(model, active.thinking);
    let selected = available
        .iter()
        .position(|level| *level == effective)
        .unwrap_or_default();
    let items = available
        .into_iter()
        .map(|level| SelectorItem {
            id: level.to_string(),
            title: level.to_string(),
            description: if level == effective {
                "current".to_string()
            } else {
                format!("available for {}/{}", model.provider, model.id)
            },
            search_text: None,
        })
        .collect();
    let mut selector = SelectorState::new(
        SelectorKind::Effort {
            provider: model.provider.clone(),
            model: model.id.clone(),
        },
        format!("EFFORT · {}/{}", model.provider, model.id),
        items,
    );
    selector.selected = selected;
    selector
}

async fn set_effort(
    app: &mut App,
    services: &LoopServices,
    provider: &str,
    model: &str,
    requested: ThinkingLevel,
) {
    let key = format!("{provider}/{model}");
    let result = async {
        services
            .manager
            .set_model_thinking(provider, model, requested)
            .await?;
        let current = services.runtime.session().model_settings().await;
        let applies_to_session = current.provider == provider && current.model == model;
        if applies_to_session {
            let active = services
                .manager
                .for_session(provider, model, requested)
                .await?;
            apply_active(
                app,
                &services.runtime,
                &services.catalog,
                &services.output,
                &active,
            )
            .await?;
        }
        Ok::<_, anyhow::Error>(applies_to_session)
    }
    .await;
    app.set_flash(match result {
        Ok(true) if app.info.thinking == requested => {
            format!("Effort for {key} set to {requested}")
        }
        Ok(true) => format!(
            "Saved {requested} for {key}; active effort is {}",
            app.info.thinking
        ),
        Ok(false) => format!("Effort for {key} saved"),
        Err(error) => format!("Could not set effort for {key}: {error:#}"),
    });
}

async fn effective_thinking(
    catalog: &ModelCatalog,
    provider: &str,
    model: &str,
    configured: ThinkingLevel,
) -> ThinkingLevel {
    catalog
        .model(provider, model)
        .await
        .map_or(configured, |model| clamp_thinking_level(&model, configured))
}

fn is_ignored_tui_key(key: KeyEvent, selection_active: bool) -> bool {
    key_name(key) == "ctrl+c" && !selection_active
}

fn key_name(key: KeyEvent) -> String {
    let mut modifiers = Vec::new();
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        modifiers.push("ctrl");
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        modifiers.push("alt");
    }
    if key.modifiers.contains(KeyModifiers::SHIFT)
        && !matches!(key.code, KeyCode::Char(character) if !character.is_ascii_alphabetic())
    {
        modifiers.push("shift");
    }
    if key.modifiers.contains(KeyModifiers::SUPER) {
        modifiers.push("super");
    }
    let code = match key.code {
        KeyCode::Backspace => "backspace".to_string(),
        KeyCode::Enter => "enter".to_string(),
        KeyCode::Left => "left".to_string(),
        KeyCode::Right => "right".to_string(),
        KeyCode::Up => "up".to_string(),
        KeyCode::Down => "down".to_string(),
        KeyCode::Home => "home".to_string(),
        KeyCode::End => "end".to_string(),
        KeyCode::PageUp => "pageup".to_string(),
        KeyCode::PageDown => "pagedown".to_string(),
        KeyCode::Tab => "tab".to_string(),
        KeyCode::BackTab => "backtab".to_string(),
        KeyCode::Delete => "delete".to_string(),
        KeyCode::Insert => "insert".to_string(),
        KeyCode::F(number) => format!("f{number}"),
        KeyCode::Char(' ') => "space".to_string(),
        KeyCode::Char(character) => {
            let name: String = character.to_lowercase().collect();
            canonical_key(&name).to_string()
        }
        KeyCode::Null => "null".to_string(),
        KeyCode::Esc => "esc".to_string(),
        KeyCode::CapsLock => "capslock".to_string(),
        KeyCode::ScrollLock => "scrolllock".to_string(),
        KeyCode::NumLock => "numlock".to_string(),
        KeyCode::PrintScreen => "printscreen".to_string(),
        KeyCode::Pause => "pause".to_string(),
        KeyCode::Menu => "menu".to_string(),
        KeyCode::KeypadBegin => "keypadbegin".to_string(),
        KeyCode::Media(_) | KeyCode::Modifier(_) => return String::new(),
    };
    if modifiers.is_empty() {
        code
    } else {
        format!("{}+{code}", modifiers.join("+"))
    }
}

async fn save_settings(
    app: &mut App,
    runtime: &AgentRuntime,
    manager: &ConfigManager,
    catalog: &ModelCatalog,
    output: &OutputStore,
) {
    let Some(settings) = app.settings.as_ref() else {
        return;
    };
    let selection = settings
        .model()
        .map(|model| (settings.provider().to_string(), model.id.clone()));
    let thinking = settings.thinking;
    let output_limit = match settings.output_limit.parse::<usize>() {
        Ok(limit) if limit >= 1024 => limit,
        Ok(_) => {
            app.set_flash("Output limit must be at least 1024 bytes");
            return;
        }
        Err(error) => {
            app.set_flash(format!("Output limit is invalid: {error}"));
            return;
        }
    };
    let api_key = settings
        .api_key_changed
        .then(|| (settings.provider().to_string(), settings.api_key.clone()));
    let result = async {
        if let Some((provider, api_key)) = api_key {
            if api_key.trim().is_empty() {
                manager.clear_api_key(&provider).await?;
            } else {
                manager.set_api_key(&provider, api_key).await?;
            }
        }
        manager.set_output_limit(output_limit).await?;
        let active = if let Some((provider, model)) = selection {
            manager
                .set_model_thinking(&provider, &model, thinking)
                .await?;
            manager.set_model(&provider, &model).await?;
            manager.for_session(&provider, &model, thinking).await?
        } else {
            active_for_runtime(manager, runtime).await?
        };
        apply_active(app, runtime, catalog, output, &active).await?;
        app.settings = Some(SettingsState::load(active, catalog).await);
        Ok::<_, anyhow::Error>(())
    }
    .await;
    app.set_flash(match result {
        Ok(()) => "Settings saved and applied".to_string(),
        Err(error) => format!("Settings were not fully applied: {error:#}"),
    });
}

fn start_catalog_refresh(
    app: &mut App,
    services: &LoopServices,
    sender: mpsc::UnboundedSender<BackgroundEvent>,
) {
    if app.catalog_refreshing {
        app.set_flash("The Pi model catalog is already refreshing");
        return;
    }
    app.catalog_refreshing = true;
    app.set_flash("Refreshing the Pi model catalog…");
    let catalog = services.catalog.clone();
    let manager = services.manager.clone();
    tokio::spawn(async move {
        let result = async {
            catalog.refresh(true).await?;
            manager.reload().await
        }
        .await;
        let _ = sender.send(BackgroundEvent::CatalogRefreshed(result));
    });
}

async fn finish_background(app: &mut App, services: &LoopServices, event: BackgroundEvent) {
    match event {
        BackgroundEvent::CatalogRefreshed(result) => {
            app.catalog_refreshing = false;
            let result = async {
                result?;
                let active = active_for_runtime(&services.manager, &services.runtime).await?;
                apply_active(
                    app,
                    &services.runtime,
                    &services.catalog,
                    &services.output,
                    &active,
                )
                .await?;
                if app.settings.is_some() {
                    app.settings =
                        Some(SettingsState::load(active.clone(), &services.catalog).await);
                }
                if let Some(query) = app
                    .model_selector
                    .as_ref()
                    .map(|selector| selector.query().to_string())
                {
                    app.model_selector =
                        Some(ModelSelector::load(&services.catalog, &active, query).await);
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            app.set_flash(match result {
                Ok(()) => "Pi model catalog refreshed".to_string(),
                Err(error) => format!("Catalog refresh failed: {error:#}"),
            });
        }
        BackgroundEvent::OauthFinished(result) => {
            let provider = app
                .oauth
                .as_ref()
                .map(|oauth| oauth.provider.clone())
                .unwrap_or_else(|| "anthropic".to_string());
            app.oauth = None;
            app.overlay = None;
            match result {
                Ok(token) => {
                    let applied = async {
                        services.manager.set_oauth(&provider, token).await?;
                        let active =
                            active_for_runtime(&services.manager, &services.runtime).await?;
                        apply_active(
                            app,
                            &services.runtime,
                            &services.catalog,
                            &services.output,
                            &active,
                        )
                        .await?;
                        Ok::<_, anyhow::Error>(())
                    }
                    .await;
                    app.set_flash(match applied {
                        Ok(()) => format!("Logged in to {provider} with OAuth"),
                        Err(error) => format!("OAuth succeeded but could not apply: {error:#}"),
                    });
                }
                Err(error) => app.set_flash(format!("OAuth failed: {error:#}")),
            }
        }
    }
}

async fn apply_active(
    app: &mut App,
    runtime: &AgentRuntime,
    catalog: &ModelCatalog,
    output: &OutputStore,
    active: &ActiveSettings,
) -> Result<()> {
    let configured = configured_backend(active, catalog, Some(runtime.session().id())).await?;
    let model_ready = configured.is_some();
    let (backend, limits) = match configured {
        Some((backend, limits)) => (Some(backend), Some(limits)),
        None => (
            None,
            active
                .catalog_model(catalog)
                .await
                .map(|model| model.limits()),
        ),
    };
    let context_window = limits
        .as_ref()
        .map_or(128_000, |limits| limits.context_window);
    runtime
        .session()
        .update_model_settings(&active.provider, &active.model, active.thinking)
        .await?;
    runtime.set_backend(backend, limits).await;
    output.set_limit(active.output_limit);
    app.info.provider.clone_from(&active.provider);
    app.info.model.clone_from(&active.model);
    app.info.thinking =
        effective_thinking(catalog, &active.provider, &active.model, active.thinking).await;
    app.info.context_window = context_window;
    app.info.model_ready = model_ready;
    app.info.provider_count = catalog.providers().await.len();
    app.info.terminal.clone_from(&active.terminal);
    Ok(())
}

async fn active_for_runtime(
    manager: &ConfigManager,
    runtime: &AgentRuntime,
) -> Result<ActiveSettings> {
    let settings = runtime.session().model_settings().await;
    manager
        .for_session(&settings.provider, &settings.model, settings.thinking)
        .await
}

fn block_document(block: &DisplayBlock) -> String {
    let mut document = format!("# {}\n", block.title);
    if let Some(call_id) = &block.call_id {
        document.push_str(&format!("\nCall ID: `{call_id}`\n"));
    }
    document.push('\n');
    document.push_str(&block.text);
    if !document.ends_with('\n') {
        document.push('\n');
    }
    document
}

fn tool_protocol(arguments: &serde_json::Value) -> Option<String> {
    let uri = arguments.get("uri")?.as_str()?;
    let separator = uri.find("://").or_else(|| uri.find(':'))?;
    (separator > 0).then(|| uri[..separator].to_string())
}

fn tool_title(name: &str, arguments: &serde_json::Value) -> String {
    let action = match name {
        "read" => "Read",
        "exec" => "Ran",
        _ => return name.to_string(),
    };
    let Some(uri) = arguments.get("uri").and_then(serde_json::Value::as_str) else {
        return action.to_string();
    };
    let (protocol, target) = uri.split_once("://").unwrap_or((uri, ""));
    if name == "exec"
        && matches!(protocol, "bash" | "pwsh")
        && let Some(command) = arguments.get("body").and_then(serde_json::Value::as_str)
    {
        return format!(
            "$ {}",
            single_line_preview(command.lines().next().unwrap_or_default(), 76)
        );
    }
    if name == "exec" && protocol == "apply_patch" {
        let files = arguments
            .get("body")
            .and_then(serde_json::Value::as_str)
            .map(patch_targets)
            .unwrap_or_default();
        if let Some(first) = files.first() {
            let more = files.len().saturating_sub(1);
            return format!(
                "Patched {}{}",
                single_line_preview(first, 64),
                if more > 0 {
                    format!(" +{more}")
                } else {
                    String::new()
                }
            );
        }
        return "Applied patch".to_string();
    }
    if name == "exec" && protocol == "replace" {
        return format!("Edited {}", single_line_preview(target, 72));
    }
    if name == "read" && protocol == "file" {
        return format!("Read {}", single_line_preview(target, 76));
    }
    if target == "help" {
        return format!("Read {protocol} help");
    }
    format!("{action} {}", single_line_preview(uri, 76))
}

fn patch_targets(patch: &str) -> Vec<String> {
    let mut targets = Vec::new();
    for line in patch.lines() {
        let path = ["*** Add File: ", "*** Update File: ", "*** Delete File: "]
            .iter()
            .find_map(|prefix| line.strip_prefix(prefix));
        if let Some(path) = path
            && !targets.iter().any(|target| target == path)
        {
            targets.push(path.to_string());
        }
    }
    targets
}

fn tool_detail_lines(
    block: &DisplayBlock,
    width: usize,
    limit: usize,
) -> (Vec<(String, Color)>, usize) {
    let mut logical = Vec::new();
    if let Some(tool) = &block.tool {
        if let Some(uri) = tool
            .arguments
            .get("uri")
            .and_then(serde_json::Value::as_str)
        {
            logical.push((format!("↳ {uri}"), MUTED));
        } else {
            logical.push((format!("↳ {}", tool.name), MUTED));
        }
        tool_argument_details(&tool.arguments, &mut logical);
        if let Some(output) = &tool.output {
            for (index, line) in output.lines().enumerate() {
                logical.push((
                    format!("{} {line}", if index == 0 { "└" } else { " " }),
                    if block.failed { ERROR } else { MUTED },
                ));
            }
        }
    } else if let Some((_, result)) = block
        .text
        .split_once("\n\nRESULT\n")
        .or_else(|| block.text.split_once("\n\nERROR\n"))
    {
        for (index, line) in result.lines().enumerate() {
            logical.push((
                format!("{} {line}", if index == 0 { "└" } else { " " }),
                if block.failed { ERROR } else { MUTED },
            ));
        }
    }
    if logical.is_empty() {
        logical.push(("Waiting for result…".to_string(), MUTED));
    }

    let mut wrapped = Vec::new();
    for (line, color) in logical {
        let lines = wrapped_block_lines(&line, width.max(1));
        wrapped.extend(lines.into_iter().map(|line| (line, color)));
    }
    let extra = wrapped.len().saturating_sub(limit);
    wrapped.truncate(limit);
    (wrapped, extra)
}

fn tool_argument_details(arguments: &serde_json::Value, lines: &mut Vec<(String, Color)>) {
    if let Some(fields) = arguments.as_object() {
        for (key, value) in fields {
            if matches!(key.as_str(), "uri" | "body") {
                continue;
            }
            lines.push((format!("  {key}: {}", json_value_summary(value)), MUTED));
        }
    }
    let Some(body) = arguments.get("body") else {
        return;
    };
    match body {
        serde_json::Value::String(value) => {
            let files = patch_targets(value);
            if !files.is_empty() {
                lines.extend(files.into_iter().map(|file| (format!("  {file}"), MUTED)));
            } else if value.lines().count() > 1 {
                lines.extend(
                    value
                        .lines()
                        .skip(1)
                        .take(3)
                        .map(|line| (format!("  {line}"), MUTED)),
                );
            }
        }
        serde_json::Value::Object(fields) => {
            for (key, value) in fields {
                lines.push((format!("  {key}: {}", json_value_summary(value)), MUTED));
            }
        }
        serde_json::Value::Array(values) => {
            lines.push((format!("  body: {} items", values.len()), MUTED));
        }
        serde_json::Value::Number(value) => lines.push((format!("  body: {value}"), MUTED)),
        serde_json::Value::Bool(value) => lines.push((format!("  body: {value}"), MUTED)),
        serde_json::Value::Null => {}
    }
}

fn json_value_summary(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(value) => single_line_preview(value, 72),
        serde_json::Value::Number(value) => value.to_string(),
        serde_json::Value::Bool(value) => value.to_string(),
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Array(values) => format!("{} items", values.len()),
        serde_json::Value::Object(values) => format!("{} fields", values.len()),
    }
}

fn render(frame: &mut Frame<'_>, app: &mut App) {
    app.hit_regions.clear();
    app.overlay_bounds = None;
    app.selectable = None;
    let area = frame.area();
    frame.render_widget(Block::new().style(Style::default().bg(BG)), area);
    if app.showing_splash() {
        render_brand(frame, app, area, true);
        return;
    }
    let idle = app.blocks.is_empty();
    let notice = status_notice(app);
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints(match (idle, notice.is_some()) {
            (true, false) => [Constraint::Min(3)].as_slice(),
            (true, true) | (false, false) => [Constraint::Min(3), Constraint::Length(1)].as_slice(),
            (false, true) => [
                Constraint::Min(3),
                Constraint::Length(1),
                Constraint::Length(1),
            ]
            .as_slice(),
        })
        .split(area);
    let content = if idle {
        render_brand(frame, app, areas[0], false);
        areas[0]
    } else {
        render_transcript(frame, app, areas[0]);
        let footer_area = if notice.is_some() { areas[2] } else { areas[1] };
        render_footer(frame, app, footer_area);
        areas[0]
    };
    if let Some((message, color)) = notice {
        frame.render_widget(
            Paragraph::new(message).style(Style::default().fg(color).bg(SURFACE)),
            areas[1],
        );
    }
    let selectable_area = if let Some(overlay) = app.overlay {
        app.hit_regions.clear();
        let area = overlay_area(frame.area(), overlay);
        app.overlay_bounds = Some(area);
        render_overlay(frame, app, overlay);
        Some(area.inner(Margin {
            horizontal: 2,
            vertical: 2,
        }))
    } else {
        Some(content)
    };
    if let Some(selectable_area) = selectable_area.filter(|area| !area.is_empty()) {
        capture_surface(frame, app, selectable_area);
        render_selection(frame, app);
    }
}

const WORDMARK_BOX_HEIGHT: u16 = 13;
const WORDMARK_BOX_WIDTH: u16 = 76;

fn wordmark_box(area: Rect) -> Rect {
    let width = area.width.clamp(1, WORDMARK_BOX_WIDTH);
    let height = area.height.clamp(1, WORDMARK_BOX_HEIGHT);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn render_brand(frame: &mut Frame<'_>, app: &mut App, area: Rect, splash: bool) {
    let brand_area = wordmark_box(area);
    let progress = (app.started.elapsed().as_secs_f32() / SPLASH_DURATION.as_secs_f32()) * 1.25;
    let mut lines = if splash && progress < 1.0 {
        animation::wordmark_reveal(app.frame, progress)
    } else {
        animation::wordmark(app.frame)
    }
    .into_iter()
    .map(|line| Line::styled(line, Style::default().fg(ACCENT)))
    .collect::<Vec<_>>();
    if splash {
        lines.extend([
            Line::default(),
            Line::styled("press any key", Style::default().fg(MUTED)),
        ]);
    } else {
        lines.push(Line::default());
        lines.extend(welcome_lines(app, brand_area.width as usize));
    }
    frame.render_widget(
        Paragraph::new(lines).alignment(Alignment::Center),
        brand_area,
    );
}

fn welcome_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let model = if app.info.model_ready {
        Line::styled(
            single_line_preview(
                &format!(
                    "{} / {} · effort {}",
                    app.info.provider, app.info.model, app.info.thinking
                ),
                width.saturating_sub(1),
            ),
            Style::default().fg(TEXT),
        )
    } else {
        Line::styled("尚未配置，请运行 :login", Style::default().fg(WARM))
    };
    let compose = app
        .keymap
        .key_for("main", "compose")
        .unwrap_or_else(|| "i".to_string());
    let command = app
        .keymap
        .key_for("main", "command")
        .unwrap_or_else(|| ":".to_string());
    let help = app
        .keymap
        .key_for("main", "help")
        .unwrap_or_else(|| "?".to_string());
    vec![
        Line::styled(
            single_line_preview(&footer_cwd(&app.info.cwd), width.saturating_sub(1)),
            Style::default().fg(MUTED),
        ),
        model,
        Line::default(),
        Line::styled(
            single_line_preview(
                &format!("{compose} compose · {command} commands · {help} help"),
                width.saturating_sub(1),
            ),
            Style::default().fg(MUTED),
        ),
    ]
}

/// Minimal conversation footer. Project, usage, and extension details stay
/// available through the bottom-anchored status panel.
fn render_footer(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let percent = context_percent(app);
    let available = area.width as usize;
    let context = single_line_preview(
        &format!(
            "{} {percent:.1}%/{}",
            animation::progress(app.frame, 8, percent / 100.0),
            format_tokens(app.info.context_window as u64),
        ),
        available,
    );
    let context_width = context.width();
    let model_limit = available.saturating_sub(context_width + 2);
    let model = single_line_preview(&compact_model(app), model_limit);
    let model_width = model.width();
    let gap = available.saturating_sub(model_width + context_width);
    let mut spans = Vec::new();
    if !model.is_empty() {
        spans.push(Span::styled(
            model,
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ));
    }
    if gap > 0 {
        spans.push(Span::raw(" ".repeat(gap)));
    }
    spans.push(Span::styled(
        context,
        Style::default()
            .fg(context_color(percent))
            .add_modifier(Modifier::BOLD),
    ));
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(SURFACE)),
        area,
    );
    app.hit_regions.push(HitRegion {
        area,
        target: AppHit::Status,
    });
}

fn compact_model(app: &App) -> String {
    if !app.info.model_ready || app.info.model.is_empty() {
        return "no-model".to_string();
    }
    let model = if app.info.provider_count > 1 {
        format!("{}/{}", app.info.provider, app.info.model)
    } else {
        app.info.model.clone()
    };
    format!("{model} · effort {}", app.info.thinking)
}

fn context_percent(app: &App) -> f64 {
    if app.info.context_window > 0 {
        app.info.context_tokens as f64 / app.info.context_window as f64 * 100.0
    } else {
        0.0
    }
}

fn context_color(percent: f64) -> Color {
    if percent > 90.0 {
        ERROR
    } else if percent > 70.0 {
        WARM
    } else {
        ACCENT
    }
}

fn status_key(app: &App) -> String {
    app.keymap
        .key_for("global", "status")
        .unwrap_or_else(|| ":status".to_string())
        .to_ascii_uppercase()
}

fn plugin_status_items(app: &App, expanded: bool) -> Vec<TuiStatusItem> {
    app.tui.status_items(&TuiStatusContext {
        cwd: app.info.cwd.clone(),
        session_id: app.info.session_id.clone(),
        expanded,
    })
}

fn status_tone_style(tone: TuiStatusTone) -> Style {
    Style::default().fg(match tone {
        TuiStatusTone::Default => TEXT,
        TuiStatusTone::Accent => ACCENT,
        TuiStatusTone::Warning => WARM,
        TuiStatusTone::Error => ERROR,
    })
}

fn render_transcript(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    if app.blocks.is_empty() {
        let unconfigured = app.info.provider.trim().is_empty() || app.info.model.trim().is_empty();
        let lines = if unconfigured {
            vec![
                Line::styled(
                    "尚未配置，请运行 :login",
                    Style::default().fg(WARM).add_modifier(Modifier::BOLD),
                ),
                Line::styled("i compose   : command   :model", Style::default().fg(MUTED)),
            ]
        } else {
            vec![
                Line::styled("No messages yet", Style::default().fg(MUTED)),
                Line::styled("i compose   : command   :login", Style::default().fg(WARM)),
            ]
        };
        frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), area);
        return;
    }
    let message_width = area.width.saturating_sub(2).max(1) as usize;
    let process_width = message_width.saturating_sub(2).max(1);
    app.transcript_body_width = process_width;
    let mut items = Vec::new();
    let mut block_for_row = Vec::new();
    let mut block_rows = vec![None; app.blocks.len()];
    let active_block = app.active_transcript_block();
    for (index, block) in app.blocks.iter().enumerate() {
        if index > 0
            && transcript_needs_gap(app.blocks[index - 1].kind, block.kind, block.final_response)
        {
            items.push(ListItem::new(Line::default()).style(Style::default().bg(BG)));
            block_for_row.push(None);
        }
        let first = items.len();
        for item in transcript_block_items(
            block,
            index == app.selected_block,
            Some(index) == active_block,
            message_width,
            process_width,
            app,
        ) {
            items.push(item);
            block_for_row.push(Some(index));
        }
        block_rows[index] = Some((first, items.len().saturating_sub(1)));
    }
    app.transcript_rows = items.len();
    app.transcript_height = area.height as usize;
    let max_offset = app.transcript_rows.saturating_sub(app.transcript_height);
    if app.transcript_follow_tail {
        app.transcript_offset = max_offset;
    } else if app.transcript_center_selected
        && let Some((first, _)) = block_rows.get(app.selected_block).copied().flatten()
        && (first < app.transcript_offset
            || first >= app.transcript_offset.saturating_add(app.transcript_height))
    {
        app.transcript_offset = first
            .saturating_sub(app.transcript_height / 2)
            .min(max_offset);
    } else {
        app.transcript_offset = app.transcript_offset.min(max_offset);
    }
    app.transcript_center_selected = false;
    let offset = app.transcript_offset;
    let visible = items
        .into_iter()
        .skip(offset)
        .take(app.transcript_height)
        .collect::<Vec<_>>();
    frame.render_widget(
        List::new(visible).block(Block::new().padding(Padding::horizontal(1))),
        area,
    );
    for (row, index) in block_for_row.into_iter().enumerate().skip(offset) {
        let y = area.y.saturating_add((row - offset) as u16);
        if y >= area.bottom() {
            break;
        }
        if let Some(index) = index {
            app.hit_regions.push(HitRegion {
                area: Rect::new(area.x, y, area.width, 1),
                target: AppHit::Transcript(index),
            });
        }
    }
}

fn transcript_needs_gap(previous: BlockKind, current: BlockKind, final_response: bool) -> bool {
    (current == BlockKind::User && previous != BlockKind::User)
        || (previous == BlockKind::User && current != BlockKind::User)
        || (current == BlockKind::Assistant
            && final_response
            && matches!(previous, BlockKind::Reasoning | BlockKind::Tool))
}

fn transcript_block_items(
    block: &DisplayBlock,
    selected: bool,
    live: bool,
    message_width: usize,
    process_width: usize,
    app: &App,
) -> Vec<ListItem<'static>> {
    let background = match block.kind {
        BlockKind::User => USER_SURFACE,
        BlockKind::Assistant => BG,
        _ if selected => ROW_ACTIVE,
        _ => BG,
    };
    let open_key = app
        .keymap
        .key_for("main", "open")
        .unwrap_or_else(|| "o".to_string())
        .to_ascii_uppercase();
    let mut rows = Vec::new();

    match block.kind {
        BlockKind::User => {
            for line in wrapped_block_lines(&block.text, message_width) {
                rows.push(transcript_item(
                    vec![Span::styled(line, Style::default().fg(TEXT))],
                    background,
                ));
            }
        }
        BlockKind::Assistant => {
            for line in markdown::render(&block.text, message_width) {
                rows.push(ListItem::new(line).style(Style::default().bg(background)));
            }
        }
        BlockKind::Reasoning => {
            rows.push(transcript_item(
                vec![
                    Span::styled("◇ ", Style::default().fg(MUTED)),
                    Span::styled(
                        if live { "Thinking…" } else { "Thought" },
                        Style::default()
                            .fg(if live { ACCENT } else { MUTED })
                            .add_modifier(Modifier::ITALIC),
                    ),
                    Span::styled(
                        if block.expanded {
                            "  ▾"
                        } else {
                            "  ▸ Enter to expand"
                        },
                        Style::default().fg(MUTED),
                    ),
                ],
                background,
            ));
            if block.expanded {
                let (lines, extra) =
                    visible_block_lines(&block.text, process_width, EXPANDED_PREVIEW_LINES);
                for line in lines {
                    rows.push(transcript_item(
                        vec![
                            Span::raw("  "),
                            Span::styled(
                                line,
                                Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
                            ),
                        ],
                        background,
                    ));
                }
                if extra > 0 {
                    rows.push(transcript_hint(extra, true, &open_key, background));
                }
            }
        }
        BlockKind::Tool => {
            let status = if live {
                animation::spinner(app.frame).to_string()
            } else if block.failed {
                "×".to_string()
            } else if block
                .tool
                .as_ref()
                .and_then(|tool| tool.output.as_ref())
                .is_some()
                || block.text.contains("\n\nRESULT\n")
            {
                "✓".to_string()
            } else {
                "·".to_string()
            };
            rows.push(transcript_item(
                vec![
                    Span::styled(
                        format!("{status} "),
                        Style::default().fg(if block.failed { ERROR } else { WARM }),
                    ),
                    Span::styled(
                        block.title.clone(),
                        Style::default()
                            .fg(if block.failed { ERROR } else { WARM })
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        if block.expanded { "  ▾" } else { "  ▸" },
                        Style::default().fg(MUTED),
                    ),
                ],
                background,
            ));
            if block.expanded {
                let (lines, extra) = tool_detail_lines(block, process_width, 8);
                for (line, color) in lines {
                    rows.push(transcript_item(
                        vec![
                            Span::raw("  "),
                            Span::styled(line, Style::default().fg(color)),
                        ],
                        background,
                    ));
                }
                if extra > 0 {
                    rows.push(transcript_hint(extra, true, &open_key, background));
                }
            }
        }
        BlockKind::Compaction | BlockKind::Notice | BlockKind::Error => {
            let color = match block.kind {
                BlockKind::Compaction => ACCENT,
                BlockKind::Notice => MUTED,
                BlockKind::Error => ERROR,
                _ => unreachable!(),
            };
            let symbol = match block.kind {
                BlockKind::Compaction => "◇",
                BlockKind::Notice => "·",
                BlockKind::Error => "×",
                _ => unreachable!(),
            };
            let limit = if block.expanded {
                EXPANDED_PREVIEW_LINES
            } else {
                1
            };
            let (lines, extra) = visible_block_lines(&block.text, process_width, limit);
            for (index, line) in lines.into_iter().enumerate() {
                rows.push(transcript_item(
                    vec![
                        Span::styled(
                            if index == 0 {
                                format!("{symbol} {}  ", block.title)
                            } else {
                                "  ".to_string()
                            },
                            Style::default().fg(color).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            line,
                            Style::default().fg(if selected { TEXT } else { MUTED }),
                        ),
                    ],
                    background,
                ));
            }
            if extra > 0 {
                rows.push(transcript_hint(
                    extra,
                    block.expanded,
                    &open_key,
                    background,
                ));
            }
        }
    }

    rows
}

fn transcript_item(spans: Vec<Span<'static>>, background: Color) -> ListItem<'static> {
    ListItem::new(Line::from(spans)).style(Style::default().bg(background))
}

fn transcript_hint(
    extra: usize,
    expanded: bool,
    open_key: &str,
    background: Color,
) -> ListItem<'static> {
    transcript_item(
        vec![
            Span::raw("  "),
            Span::styled(
                format!(
                    "… {extra} more lines · {}",
                    if expanded {
                        format!("{open_key} or double-click opens full")
                    } else {
                        "Enter expands".to_string()
                    }
                ),
                Style::default().fg(MUTED),
            ),
        ],
        background,
    )
}

fn visible_block_lines(text: &str, width: usize, limit: usize) -> (Vec<String>, usize) {
    let mut wrapped = wrapped_block_lines(text, width);
    let extra = wrapped.len().saturating_sub(limit);
    wrapped.truncate(limit);
    (wrapped, extra)
}

fn wrapped_block_lines(text: &str, width: usize) -> Vec<String> {
    let mut wrapped = Vec::new();
    for logical in text.lines() {
        if logical.is_empty() {
            wrapped.push(String::new());
        } else {
            wrapped.extend(
                textwrap::wrap(logical, width.max(1))
                    .into_iter()
                    .map(|line| line.into_owned()),
            );
        }
    }
    if wrapped.is_empty() {
        wrapped.push(String::new());
    }
    wrapped
}

fn status_notice(app: &App) -> Option<(String, Color)> {
    let (message, color) = if app.busy {
        let activity = app
            .activity
            .as_ref()
            .map(Activity::label)
            .unwrap_or_else(|| "working".to_string());
        let elapsed = app
            .busy_since
            .map(|since| format!(" {:.1}s", since.elapsed().as_secs_f32()))
            .unwrap_or_default();
        (
            format!(
                " {} {activity}{elapsed}  {}",
                animation::spinner(app.frame),
                animation::activity(app.frame, 8)
            ),
            ACCENT,
        )
    } else if let Some(flash) = app.visible_flash() {
        (
            format!(" {flash}"),
            if flash_is_error(flash) { ERROR } else { WARM },
        )
    } else if app.jump != JumpKind::All {
        let label = match app.jump {
            JumpKind::Reasoning => "thinking",
            JumpKind::Tool => "tools",
            JumpKind::User => "you",
            JumpKind::All => "",
        };
        let indices = app.filtered_indices();
        let position = indices
            .iter()
            .position(|index| *index == app.selected_block)
            .map(|index| index + 1)
            .unwrap_or(0);
        (
            format!(
                " {label} {position}/{}   esc clear   enter open",
                indices.len()
            ),
            WARM,
        )
    } else {
        return None;
    };
    Some((message, color))
}

fn flash_is_error(flash: &str) -> bool {
    let flash = flash.to_ascii_lowercase();
    flash.contains("failed")
        || flash.contains("error")
        || flash.contains("invalid")
        || flash.contains("could not")
        || flash.contains("unknown")
}

fn keymap_help(keymap: &Keymap) -> String {
    let mut output = String::new();
    for (title, mode) in [
        ("CONVERSATION", "main"),
        ("COMPOSER", "composer"),
        ("COMMAND", "command"),
        ("LISTS", "list"),
        ("SELECTOR", "selector"),
        ("MODELS", "models"),
        ("SETTINGS", "settings"),
        ("OAUTH", "oauth"),
        ("TERMINAL", "terminal"),
        ("SELECTION", "selection"),
        ("GLOBAL", "global"),
    ] {
        output.push_str(title);
        output.push('\n');
        for (key, action) in keymap.bindings_for(mode) {
            output.push_str(&format!("  {key:<16} {}\n", action.replace('_', " ")));
        }
        output.push('\n');
    }
    output
}

fn command_help(commands: &CommandRegistry) -> String {
    commands
        .list()
        .into_iter()
        .map(|command| format!("  :{:<14} {}\n", command.id, command.description))
        .collect()
}

fn common_command_prefix(names: &[String]) -> String {
    let Some(first) = names.first() else {
        return String::new();
    };
    let mut end = first.len();
    for name in &names[1..] {
        end = first
            .as_bytes()
            .iter()
            .take(end)
            .zip(name.as_bytes())
            .take_while(|(left, right)| left.eq_ignore_ascii_case(right))
            .count();
    }
    first[..end].to_string()
}

fn matching_commands(commands: &CommandRegistry, query: &str) -> Vec<CommandMatch> {
    let query = query.trim().trim_start_matches([':', '：']).to_lowercase();
    if query.is_empty() {
        return commands
            .list()
            .into_iter()
            .map(|spec| CommandMatch {
                name: spec.id.clone(),
                spec,
            })
            .collect();
    }

    let mut matches = commands
        .list()
        .into_iter()
        .filter_map(|spec| {
            let name_match = std::iter::once(&spec.id)
                .chain(spec.aliases.iter())
                .enumerate()
                .filter_map(|(index, name)| {
                    fuzzy_score(&name.to_lowercase(), &query)
                        .map(|score| (score, 0, index, name.clone()))
                })
                .min_by_key(|(score, source, index, _)| (*score, *source, *index));
            let description_match = fuzzy_score(&spec.description.to_lowercase(), &query)
                .map(|score| (score, 1, usize::MAX, spec.id.clone()));
            let (score, source, _, name) = name_match
                .into_iter()
                .chain(description_match)
                .min_by_key(|(score, source, index, _)| (*score, *source, *index))?;
            Some((score, source, spec.id.clone(), CommandMatch { spec, name }))
        })
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.cmp(&right.1))
            .then_with(|| left.2.cmp(&right.2))
    });
    matches
        .into_iter()
        .map(|(_, _, _, command)| command)
        .collect()
}

fn fuzzy_score(haystack: &str, query: &str) -> Option<usize> {
    if query.is_empty() || haystack == query {
        Some(0)
    } else if haystack.starts_with(query) {
        Some(1)
    } else if let Some(position) = haystack.find(query) {
        Some(position + 2)
    } else {
        let mut cursor = 0;
        let mut score = 100;
        for needle in query.chars() {
            let suffix = haystack.get(cursor..)?;
            let position = suffix.find(needle)?;
            score += position;
            cursor += position + needle.len_utf8();
        }
        Some(score)
    }
}

fn overlay_area(frame: Rect, overlay: Overlay) -> Rect {
    match overlay {
        Overlay::Command => centered(frame, 72, 62),
        Overlay::Status => bottom_float(frame, 14),
        Overlay::Composer => bottom_float(frame, 8),
        Overlay::Text | Overlay::Oauth => Rect::new(
            2,
            frame.height.saturating_sub(12).max(2),
            frame.width.saturating_sub(4),
            10,
        ),
        Overlay::Terminal => centered(frame, 92, 88),
        Overlay::Models | Overlay::Settings | Overlay::Selector => centered(frame, 82, 78),
        _ => centered(frame, 78, 72),
    }
}

fn bottom_float(frame: Rect, desired_height: u16) -> Rect {
    let horizontal_margin = u16::from(frame.width > 4) * 2;
    let width = frame.width.saturating_sub(horizontal_margin * 2);
    let height = desired_height.min(frame.height).max(1);
    Rect::new(
        frame.x.saturating_add(horizontal_margin),
        frame.bottom().saturating_sub(height),
        width,
        height,
    )
}

fn render_overlay(frame: &mut Frame<'_>, app: &mut App, overlay: Overlay) {
    let area = overlay_area(frame.area(), overlay);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(SURFACE).fg(TEXT))
        .padding(Padding::uniform(1));
    match overlay {
        Overlay::Composer => {
            style_input(&mut app.input, app.busy);
            frame.render_widget(&app.input, area);
            if let Some(position) =
                composer_cursor_position(&app.input, area, &mut app.composer_scroll)
            {
                frame.set_cursor_position(position);
            }
        }
        Overlay::Command => render_command(frame, app, area, block),
        Overlay::Status => render_status(frame, app, area, block),
        Overlay::Help => {
            let text = format!(
                "KEYS\n\n{}COMMANDS\n{}\nSESSION\n{}\n{}\n\nPROJECT\n{}",
                keymap_help(&app.keymap),
                command_help(&app.commands),
                app.info.session_id,
                app.info.model,
                display_path(&app.info.cwd)
            );
            frame.render_widget(
                Paragraph::new(text)
                    .block(block.title(" HELP · Esc close "))
                    .wrap(Wrap { trim: false })
                    .scroll((app.overlay_scroll, 0)),
                area,
            );
        }
        Overlay::Protocols => {
            let mut lines = Vec::new();
            for protocol in &app.protocols {
                let modes = match (protocol.can_read, protocol.can_exec) {
                    (true, true) => "read · exec",
                    (true, false) => "read",
                    (false, true) => "exec",
                    (false, false) => "—",
                };
                lines.extend([
                    Line::styled(
                        format!("{}://   {modes}", protocol.name),
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                    ),
                    Line::styled(protocol.description.clone(), Style::default().fg(TEXT)),
                    Line::default(),
                ]);
            }
            frame.render_widget(
                Paragraph::new(lines)
                    .block(block.title(" PROTOCOLS · read <name>://help "))
                    .wrap(Wrap { trim: false })
                    .scroll((app.overlay_scroll, 0)),
                area,
            );
        }
        Overlay::Tasks => render_tasks(frame, app, area, block),
        Overlay::Models => render_models(frame, app, area, block),
        Overlay::Settings => render_settings(frame, app, area, block),
        Overlay::Plugin => {
            let document = app.tui_document.as_ref();
            let title = document
                .map(|document| format!(" {} · Esc close ", document.title))
                .unwrap_or_else(|| " PLUGIN PANEL ".to_string());
            let body = document
                .map(|document| document.body.as_str())
                .unwrap_or("Plugin panel did not return content.");
            frame.render_widget(
                Paragraph::new(body)
                    .block(block.title(title))
                    .wrap(Wrap { trim: false })
                    .scroll((app.overlay_scroll, 0)),
                area,
            );
        }
        Overlay::Document => {
            let (title, body) = app
                .document
                .as_ref()
                .map(|(title, body)| (format!(" {title} · Esc close "), body.as_str()))
                .unwrap_or((" DOCUMENT ".to_string(), "Nothing to show."));
            frame.render_widget(
                Paragraph::new(body)
                    .block(block.title(title))
                    .wrap(Wrap { trim: false })
                    .scroll((app.overlay_scroll, 0)),
                area,
            );
        }
        Overlay::Selector => render_selector(frame, app, area, block),
        Overlay::Text => {
            let Some(prompt) = app.text_prompt.as_ref() else {
                return;
            };
            let value = if prompt.secret {
                format!("{}█", "•".repeat(prompt.value.chars().count().min(48)))
            } else {
                format!("{}█", prompt.value)
            };
            frame.render_widget(
                Paragraph::new(vec![
                    Line::styled(prompt.message.clone(), Style::default().fg(MUTED)),
                    Line::default(),
                    Line::styled(value, Style::default().fg(TEXT)),
                ])
                .block(block.title(format!(" {} ", prompt.title))),
                area,
            );
        }
        Overlay::Terminal => render_pty(frame, app, area),
        Overlay::Oauth => {
            let Some(oauth) = app.oauth.as_ref() else {
                return;
            };
            let display = oauth.login.display();
            let mut lines = vec![
                Line::styled(display.instructions, Style::default().fg(MUTED)),
                Line::default(),
            ];
            let device = display.user_code.clone();
            if let Some(code) = &device {
                lines.push(Line::styled(
                    format!("code  {code}"),
                    Style::default().fg(WARM).add_modifier(Modifier::BOLD),
                ));
                lines.push(Line::default());
            }
            if !display.url.is_empty() {
                lines.push(Line::styled(display.url, Style::default().fg(ACCENT)));
                lines.push(Line::default());
            }
            if device.is_none() {
                lines.push(Line::styled(
                    format!("paste {}█", oauth.paste),
                    Style::default().fg(TEXT),
                ));
            }
            frame.render_widget(
                Paragraph::new(lines)
                    .block(block.title(format!(" OAUTH · {} · Esc cancel ", oauth.provider)))
                    .wrap(Wrap { trim: false }),
                area,
            );
        }
    }
}

fn render_pty(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let resize_error = {
        let Some(pty) = app.pty.as_mut() else {
            return;
        };
        pty.area = area;
        let inner = area.inner(Margin {
            horizontal: 1,
            vertical: 1,
        });
        let resize_error = pty.terminal.resize(inner.height, inner.width).err();
        let parser = pty.terminal.screen();
        frame.render_widget(
            PseudoTerminal::new(parser.screen()).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(ACCENT))
                    .style(Style::default().bg(SURFACE))
                    .title(" TERMINAL · double Esc close · Shift-drag select "),
            ),
            area,
        );
        resize_error
    };
    if let Some(error) = resize_error {
        app.set_flash(format!("Embedded terminal resize failed: {error:#}"));
    }
}

fn render_status(frame: &mut Frame<'_>, app: &mut App, area: Rect, block: Block<'_>) {
    let branch = current_branch(app);
    let percent = context_percent(app);
    let project = branch.map_or_else(
        || display_path(&app.info.cwd),
        |branch| format!("{} · git:{branch}", display_path(&app.info.cwd)),
    );
    let state = app
        .activity
        .as_ref()
        .map(Activity::label)
        .unwrap_or_else(|| "ready".to_string());
    let cache_hit = app
        .last_cache_hit
        .map(|rate| format!("{rate:.1}%"))
        .unwrap_or_else(|| "—".to_string());
    let subscription = app.info.provider == "kimi-coding";
    let mut lines = vec![
        status_row("PROJECT", project, Style::default().fg(ACCENT)),
        status_row(
            "SESSION",
            app.info.session_id.clone(),
            Style::default().fg(TEXT),
        ),
        status_row(
            "MODEL",
            if app.info.model_ready {
                format!(
                    "{} / {} · effort {}",
                    app.info.provider, app.info.model, app.info.thinking
                )
            } else {
                "not configured · :login".to_string()
            },
            Style::default().fg(if app.info.model_ready { TEXT } else { WARM }),
        ),
        status_row("STATE", state, Style::default().fg(ACCENT)),
        status_row(
            "CONTEXT",
            format!(
                "{} / {} · {percent:.1}% · automatic compaction",
                format_tokens(app.info.context_tokens as u64),
                format_tokens(app.info.context_window as u64),
            ),
            Style::default()
                .fg(context_color(percent))
                .add_modifier(Modifier::BOLD),
        ),
        status_row(
            "TOKENS",
            format!(
                "input {} · output {} · total {}",
                format_tokens(app.usage.input),
                format_tokens(app.usage.output),
                format_tokens(app.usage.input.saturating_add(app.usage.output)),
            ),
            Style::default().fg(TEXT),
        ),
        status_row(
            "CACHE",
            format!(
                "read {} · write {} · last hit {cache_hit}",
                format_tokens(app.usage.cache_read),
                format_tokens(app.usage.cache_write),
            ),
            Style::default().fg(TEXT),
        ),
        status_row(
            "COST",
            format!(
                "${:.4}{}",
                app.usage.cost,
                if subscription { " · subscription" } else { "" }
            ),
            Style::default().fg(if subscription { ACCENT } else { TEXT }),
        ),
        status_row(
            "PROTOCOLS",
            format!("{} registered", app.protocols.len()),
            Style::default().fg(TEXT),
        ),
    ];
    let plugin_items = plugin_status_items(app, true);
    if !plugin_items.is_empty() {
        lines.push(Line::default());
        lines.push(Line::styled(
            "EXTENSIONS",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
        lines.extend(plugin_items.into_iter().map(|item| {
            status_row(
                single_line_preview(&item.label, 18),
                single_line_preview(&item.value, 256),
                status_tone_style(item.tone),
            )
        }));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(block.title(format!(" STATUS · {} toggle · Esc close ", status_key(app))))
            .wrap(Wrap { trim: false })
            .scroll((app.overlay_scroll, 0)),
        area,
    );
}

fn status_row(
    label: impl Into<String>,
    value: impl Into<String>,
    value_style: Style,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{:<11}", label.into()),
            Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
        ),
        Span::styled(value.into(), value_style),
    ])
}

fn render_command(frame: &mut Frame<'_>, app: &mut App, area: Rect, block: Block<'_>) {
    let inner = block.inner(area);
    frame.render_widget(
        block.title(" COMMAND · type to filter · Tab complete · Enter run · Esc close "),
        area,
    );
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(3)])
        .split(inner);
    frame.render_widget(
        Paragraph::new(format!("⌕ {}█", app.command_query)).style(Style::default().fg(TEXT)),
        sections[0],
    );
    let commands = app.matching_commands();
    let items = commands.iter().enumerate().map(|(index, item)| {
        let selected = index == app.command_selected;
        ListItem::new(Line::from(vec![
            Span::styled(
                if selected { "› " } else { "  " },
                Style::default().fg(ACCENT),
            ),
            Span::styled(
                format!(":{:<14}", item.name),
                Style::default()
                    .fg(if selected { ACCENT } else { TEXT })
                    .add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            Span::styled(item.spec.description.clone(), Style::default().fg(MUTED)),
        ]))
        .style(Style::default().bg(if selected { BG } else { SURFACE }))
    });
    let mut state = ListState::default().with_selected(Some(app.command_selected));
    frame.render_stateful_widget(List::new(items), sections[1], &mut state);
    for index in state.offset()..commands.len() {
        let y = sections[1]
            .y
            .saturating_add((index - state.offset()) as u16);
        if y >= sections[1].bottom() {
            break;
        }
        app.hit_regions.push(HitRegion {
            area: Rect::new(sections[1].x, y, sections[1].width, 1),
            target: AppHit::Palette(index),
        });
    }
}

fn render_selector(frame: &mut Frame<'_>, app: &mut App, area: Rect, block: Block<'_>) {
    let inner = block.inner(area);
    let Some(selector) = app.selector.as_ref() else {
        return;
    };
    let instructions = if matches!(&selector.kind, SelectorKind::Search) {
        "click or Enter jump"
    } else {
        "Enter choose"
    };
    frame.render_widget(
        block.title(format!(
            " {} · type to filter · {instructions} · Esc close ",
            selector.title,
        )),
        area,
    );
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(3)])
        .split(inner);
    frame.render_widget(
        Paragraph::new(format!("⌕ {}█", selector.query)).style(Style::default().fg(TEXT)),
        sections[0],
    );
    let items = selector
        .visible
        .iter()
        .enumerate()
        .filter_map(|(position, index)| {
            let item = selector.items.get(*index)?;
            let selected = position == selector.selected;
            Some(
                ListItem::new(Line::from(vec![
                    Span::styled(
                        if selected { "› " } else { "  " },
                        Style::default().fg(ACCENT),
                    ),
                    Span::styled(
                        format!("{:<22}", item.title),
                        Style::default().fg(if selected { ACCENT } else { TEXT }),
                    ),
                    Span::styled(item.description.clone(), Style::default().fg(MUTED)),
                ]))
                .style(Style::default().bg(if selected { BG } else { SURFACE })),
            )
        });
    let mut state = ListState::default().with_selected(Some(selector.selected));
    frame.render_stateful_widget(List::new(items), sections[1], &mut state);
    for position in state.offset()..selector.visible.len() {
        let y = sections[1]
            .y
            .saturating_add((position - state.offset()) as u16);
        if y >= sections[1].bottom() {
            break;
        }
        app.hit_regions.push(HitRegion {
            area: Rect::new(sections[1].x, y, sections[1].width, 1),
            target: AppHit::Selector(position),
        });
    }
}

fn render_models(frame: &mut Frame<'_>, app: &mut App, area: Rect, block: Block<'_>) {
    let inner = block.inner(area);
    frame.render_widget(
        block.title(" MODELS · type to search · Enter use · Ctrl+R refresh "),
        area,
    );
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(4),
            Constraint::Length(3),
        ])
        .split(inner);
    let Some(selector) = app.model_selector.as_ref() else {
        frame.render_widget(
            Paragraph::new("Model catalog is not loaded.").style(Style::default().fg(MUTED)),
            inner,
        );
        return;
    };
    let summary = if app.catalog_refreshing {
        format!("{} refreshing pi.dev", animation::spinner(app.frame))
    } else {
        format!(
            "{} matches · {} models · {} providers",
            selector.visible_len(),
            selector.model_count(),
            selector.provider_count()
        )
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("⌕  ", Style::default().fg(ACCENT)),
            Span::styled(selector.query(), Style::default().fg(TEXT)),
            Span::styled("█", Style::default().fg(ACCENT)),
        ]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(MUTED))
                .title(format!(" SEARCH · {summary} ")),
        ),
        sections[0],
    );
    let name_width: usize = if sections[1].width < 60 { 18 } else { 30 };
    let items = selector.visible().enumerate().map(|(position, model)| {
        let selected = position == selector.selected_position();
        ListItem::new(Line::from(vec![
            Span::styled(
                if selected { "› " } else { "  " },
                Style::default().fg(ACCENT),
            ),
            Span::styled(
                format!(
                    "{} {:<14}",
                    if selector.is_current(model) {
                        "●"
                    } else {
                        " "
                    },
                    model.provider
                ),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                format!(
                    "{:<name_width$}",
                    single_line_preview(model_label(model), name_width.saturating_sub(2))
                ),
                Style::default().fg(if selected { ACCENT } else { TEXT }),
            ),
            Span::styled(
                format!(
                    "{}{}",
                    context_label(model.context_window()),
                    if reasoning(model) { " · think" } else { "" }
                ),
                Style::default().fg(MUTED),
            ),
        ]))
        .style(Style::default().bg(if selected { BG } else { SURFACE }))
    });
    let mut state = ListState::default().with_selected(Some(selector.selected_position()));
    frame.render_stateful_widget(List::new(items), sections[1], &mut state);
    for position in state.offset()..selector.visible_len() {
        let y = sections[1]
            .y
            .saturating_add((position - state.offset()) as u16);
        if y >= sections[1].bottom() {
            break;
        }
        app.hit_regions.push(HitRegion {
            area: Rect::new(sections[1].x, y, sections[1].width, 1),
            target: AppHit::Model(position),
        });
    }
    let footer = if let Some(model) = selector.selected() {
        format!("{}/{} · {}", model.provider, model.id, model.api)
    } else {
        "No models match this search".to_string()
    };
    frame.render_widget(
        Paragraph::new(footer).style(Style::default().fg(MUTED)),
        sections[2],
    );
}

fn render_settings(frame: &mut Frame<'_>, app: &mut App, area: Rect, block: Block<'_>) {
    let inner = block.inner(area);
    let Some(settings) = app.settings.as_ref() else {
        frame.render_widget(Paragraph::new("Loading settings…").block(block), area);
        return;
    };
    let model = settings
        .model()
        .map(|model| {
            if model.name.is_empty() || model.name == model.id {
                model.id.clone()
            } else {
                format!("{}  ·  {}", model.name, model.id)
            }
        })
        .unwrap_or_else(|| settings.active.model.clone());
    let credential = match settings.active.auth_kind {
        AuthKind::Oauth => format!("OAuth  ·  {}", settings.active.api_key_source.label()),
        AuthKind::ApiKey => format!("API key  ·  {}", settings.active.api_key_source.label()),
        AuthKind::None => "not configured  ·  :login".to_string(),
    };
    let output_limit = if settings.editing == Some(EditingSetting::OutputLimit) {
        format!("{}█", settings.output_limit)
    } else {
        format!("{} bytes", settings.output_limit)
    };
    let rows = [
        ("Model", format!("{} / {model}", settings.provider())),
        ("Credential", credential),
        ("Thinking", settings.thinking.to_string()),
        ("Output limit", output_limit),
    ];
    let mut lines = vec![
        Line::styled(
            "Use :login / :logout for credentials. Enter edits the selected field.",
            Style::default().fg(MUTED),
        ),
        Line::default(),
    ];
    for (index, (label, value)) in rows.into_iter().enumerate() {
        let selected = settings.selected == index;
        lines.push(Line::from(vec![
            Span::styled(
                if selected { "› " } else { "  " },
                Style::default().fg(ACCENT),
            ),
            Span::styled(
                format!("{label:<14}"),
                Style::default().fg(if selected { ACCENT } else { MUTED }),
            ),
            Span::styled(value, Style::default().fg(TEXT)),
        ]));
        lines.push(Line::default());
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(block.title(" SETTINGS · s save · Esc close "))
            .wrap(Wrap { trim: false }),
        area,
    );
    for index in 0..4 {
        app.hit_regions.push(HitRegion {
            area: Rect::new(
                inner.x,
                inner.y.saturating_add(2 + index as u16 * 2),
                inner.width,
                1,
            ),
            target: AppHit::Setting(index),
        });
    }
}

fn render_tasks(frame: &mut Frame<'_>, app: &mut App, area: Rect, block: Block<'_>) {
    if app.task_records.is_empty() {
        frame.render_widget(
            Paragraph::new("No managed tasks in this process.")
                .block(block.title(" TASKS "))
                .style(Style::default().fg(MUTED)),
            area,
        );
        return;
    }
    let inner = block.inner(area);
    frame.render_widget(block.title(" TASKS · ↑/↓ select · x cancel "), area);
    let items = app.task_records.iter().enumerate().map(|(index, task)| {
        ListItem::new(format!(
            "{} {:9} {}",
            if index == app.selected_task {
                "›"
            } else {
                " "
            },
            task.status.as_str(),
            task.label
        ))
        .style(Style::default().fg(if index == app.selected_task {
            ACCENT
        } else {
            TEXT
        }))
    });
    let mut state = ListState::default().with_selected(Some(app.selected_task));
    frame.render_stateful_widget(List::new(items), inner, &mut state);
    for index in state.offset()..app.task_records.len() {
        let y = inner.y.saturating_add((index - state.offset()) as u16);
        if y >= inner.bottom() {
            break;
        }
        app.hit_regions.push(HitRegion {
            area: Rect::new(inner.x, y, inner.width, 1),
            target: AppHit::Task(index),
        });
    }
}

fn style_input(input: &mut TextArea<'static>, busy: bool) {
    let border = if busy { MUTED } else { ACCENT };
    input.set_block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border))
            .title(Line::styled(
                " MESSAGE ",
                Style::default().fg(border).add_modifier(Modifier::BOLD),
            ))
            .title_bottom(
                Line::styled(
                    " Enter send · Shift+Enter newline · Esc keep draft ",
                    Style::default().fg(MUTED),
                )
                .right_aligned(),
            )
            .style(Style::default().bg(SURFACE)),
    );
    input.set_placeholder_text(if busy {
        "A turn is already running"
    } else {
        "Ask URI Agent to build, explain, or fix…"
    });
    input.set_placeholder_style(Style::default().fg(MUTED).bg(SURFACE));
    input.set_style(Style::default().fg(TEXT).bg(SURFACE));
    input.set_cursor_line_style(Style::default().fg(TEXT).bg(SURFACE));
    input.set_cursor_style(Style::default().fg(BG).bg(border));
}

fn composer_cursor_position(
    input: &TextArea<'_>,
    area: Rect,
    scroll: &mut (u16, u16),
) -> Option<(u16, u16)> {
    let inner = input.block().map_or(area, |block| block.inner(area));
    if inner.is_empty() {
        return None;
    }
    let (logical_row, col) = input.cursor();
    let display_col = input
        .lines()
        .get(logical_row)
        .map(|line| input_line_width(line, col, input.tab_length()))
        .unwrap_or(0);
    let row = u16::try_from(logical_row).unwrap_or(u16::MAX);
    let top_row = scroll_to_cursor(scroll.0, row, inner.height);
    let top_col = scroll_to_cursor(scroll.1, display_col, inner.width);
    *scroll = (top_row, top_col);
    Some((
        inner.x.saturating_add(display_col.saturating_sub(top_col)),
        inner.y.saturating_add(row.saturating_sub(top_row)),
    ))
}

fn input_line_width(line: &str, chars: usize, tab_length: u8) -> u16 {
    let mut width = 0usize;
    for character in line.chars().take(chars) {
        if character == '\t' && tab_length > 0 {
            let tab_length = tab_length as usize;
            width += tab_length - width % tab_length;
        } else {
            width += character.width().unwrap_or(0);
        }
    }
    u16::try_from(width).unwrap_or(u16::MAX)
}

fn scroll_to_cursor(previous: u16, cursor: u16, length: u16) -> u16 {
    if cursor < previous {
        cursor
    } else if previous.saturating_add(length) <= cursor {
        cursor.saturating_add(1).saturating_sub(length)
    } else {
        previous
    }
}

fn centered(area: Rect, width_percent: u16, height_percent: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_percent) / 2),
            Constraint::Percentage(height_percent),
            Constraint::Percentage((100 - height_percent) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_percent) / 2),
            Constraint::Percentage(width_percent),
            Constraint::Percentage((100 - width_percent) / 2),
        ])
        .split(vertical[1])[1]
}

fn capture_surface(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let cells = (area.y..area.bottom())
        .map(|row| {
            (area.x..area.right())
                .map(|column| {
                    frame
                        .buffer_mut()
                        .cell((column, row))
                        .map(|cell| cell.symbol().to_string())
                        .unwrap_or_default()
                })
                .collect()
        })
        .collect();
    app.selectable = Some(SelectableSurface { area, cells });
}

fn render_selection(frame: &mut Frame<'_>, app: &App) {
    let (Some(surface), Some(selection)) = (&app.selectable, app.selection) else {
        return;
    };
    let first = selection.start;
    let second = selection.end;
    let (start, end) = if (first.1, first.0) <= (second.1, second.0) {
        (first, second)
    } else {
        (second, first)
    };
    for row in start.1..=end.1 {
        let from = if row == start.1 {
            start.0
        } else {
            surface.area.x
        };
        let to = if row == end.1 {
            end.0
        } else {
            surface.area.right().saturating_sub(1)
        };
        for column in from..=to {
            if let Some(cell) = frame.buffer_mut().cell_mut((column, row)) {
                cell.set_style(cell.style().add_modifier(Modifier::REVERSED));
            }
        }
    }
}

fn update_mouse_selection(app: &mut App, mouse: MouseEvent, require_shift: bool) -> bool {
    let Some(surface) = app.selectable.as_ref() else {
        return false;
    };
    let point = (
        mouse
            .column
            .clamp(surface.area.x, surface.area.right().saturating_sub(1)),
        mouse
            .row
            .clamp(surface.area.y, surface.area.bottom().saturating_sub(1)),
    );
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left)
            if (!require_shift || mouse.modifiers.contains(KeyModifiers::SHIFT))
                && surface.area.contains(point.into()) =>
        {
            app.selection = Some(TextSelection {
                start: point,
                end: point,
            });
            true
        }
        MouseEventKind::Drag(MouseButton::Left) if app.selection.is_some() => {
            if let Some(selection) = app.selection.as_mut() {
                selection.end = point;
            }
            true
        }
        MouseEventKind::Up(MouseButton::Left) if app.selection.is_some() => {
            let empty = if let Some(selection) = app.selection.as_mut() {
                selection.end = point;
                selection.start == selection.end
            } else {
                false
            };
            if empty {
                app.selection = None;
            }
            true
        }
        _ => false,
    }
}

fn copy_current_surface(app: &mut App) {
    let Some(surface) = app.selectable.as_ref() else {
        app.set_flash("Nothing visible can be copied");
        return;
    };
    let text = if let Some(selection) = app.selection {
        selected_surface_text(surface, selection)
    } else {
        complete_surface_text(surface)
    };
    if text.trim().is_empty() {
        app.set_flash("The selection is empty");
        return;
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let result = write!(stdout(), "\x1b]52;c;{encoded}\x07").and_then(|()| stdout().flush());
    app.set_flash(if result.is_ok() {
        format!("Copied {} characters with OSC52", text.chars().count())
    } else {
        "Could not write OSC52 clipboard data".to_string()
    });
    app.selection = None;
}

fn selected_surface_text(surface: &SelectableSurface, selection: TextSelection) -> String {
    let relative = |point: (u16, u16)| {
        (
            point.0.saturating_sub(surface.area.x) as usize,
            point.1.saturating_sub(surface.area.y) as usize,
        )
    };
    let first = relative(selection.start);
    let second = relative(selection.end);
    let ((start_x, start_y), (end_x, end_y)) = if (first.1, first.0) <= (second.1, second.0) {
        (first, second)
    } else {
        (second, first)
    };
    surface
        .cells
        .iter()
        .enumerate()
        .skip(start_y)
        .take(end_y.saturating_sub(start_y) + 1)
        .map(|(row, cells)| {
            let from = if row == start_y { start_x } else { 0 };
            let to = if row == end_y {
                end_x.saturating_add(1)
            } else {
                cells.len()
            };
            cells[from.min(cells.len())..to.min(cells.len())]
                .concat()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn complete_surface_text(surface: &SelectableSurface) -> String {
    surface
        .cells
        .iter()
        .map(|cells| cells.concat().trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// Pi's `formatCwdForFooter`: replace the home directory prefix with `~`.
fn footer_cwd(path: &Path) -> String {
    let text = display_path(path);
    let Some(home) = dirs::home_dir() else {
        return text;
    };
    let home_text = display_path(&home);
    if text == home_text {
        return "~".to_string();
    }
    let prefix = format!("{home_text}{}", std::path::MAIN_SEPARATOR);
    text.strip_prefix(&prefix)
        .map(|rest| format!("~{}{rest}", std::path::MAIN_SEPARATOR))
        .unwrap_or(text)
}

/// Pi's `formatTokens`: compact 1000-based token counts.
fn format_tokens(count: u64) -> String {
    if count < 1_000 {
        count.to_string()
    } else if count < 10_000 {
        format!("{:.1}k", count as f64 / 1_000.0)
    } else if count < 1_000_000 {
        format!("{}k", count / 1_000)
    } else if count < 10_000_000 {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    } else {
        format!("{}M", count / 1_000_000)
    }
}

const BRANCH_CACHE_TTL: Duration = Duration::from_secs(2);

fn current_branch(app: &mut App) -> Option<String> {
    let now = Instant::now();
    if let Some((checked, value)) = &app.branch
        && now.duration_since(*checked) < BRANCH_CACHE_TTL
    {
        return value.clone();
    }
    let value = git_branch(&app.info.cwd);
    app.branch = Some((now, value.clone()));
    value
}

/// Walk up from `cwd` to the nearest `.git`, supporting worktrees whose
/// `.git` is a file pointing at the real gitdir. Mirrors pi's footer branch.
fn git_branch(cwd: &Path) -> Option<String> {
    let mut current = Some(cwd);
    while let Some(directory) = current {
        let marker = directory.join(".git");
        if marker.is_dir() {
            return head_branch(&marker);
        }
        if marker.is_file() {
            let content = std::fs::read_to_string(&marker).ok()?;
            let target = content.trim().strip_prefix("gitdir: ")?;
            let target = Path::new(target);
            let gitdir = if target.is_absolute() {
                target.to_path_buf()
            } else {
                directory.join(target)
            };
            return head_branch(&gitdir);
        }
        current = directory.parent();
    }
    None
}

fn head_branch(gitdir: &Path) -> Option<String> {
    let head = std::fs::read_to_string(gitdir.join("HEAD")).ok()?;
    let head = head.trim();
    Some(
        head.strip_prefix("ref: refs/heads/")
            .unwrap_or("detached")
            .to_string(),
    )
}

fn search_line_preview(text: &str, query: &str, limit: usize) -> String {
    let line = (!query.is_empty())
        .then(|| {
            text.lines()
                .find(|line| line.to_lowercase().contains(query))
        })
        .flatten()
        .or_else(|| text.lines().find(|line| !line.trim().is_empty()))
        .unwrap_or_default();
    single_line_preview(line, limit)
}

fn single_line_preview(text: &str, limit: usize) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.width() <= limit {
        normalized
    } else if limit == 0 {
        String::new()
    } else if limit == 1 {
        "…".to_string()
    } else {
        let mut width = 0;
        let preview = normalized
            .chars()
            .take_while(|character| {
                let character_width = character.width().unwrap_or(0);
                if width + character_width > limit - 1 {
                    false
                } else {
                    width += character_width;
                    true
                }
            })
            .collect::<String>();
        preview + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ValueSource;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::collections::BTreeMap;

    fn test_app_with_splash(show_splash: bool) -> App {
        App::new(
            Vec::new(),
            Arc::new(CommandRegistry::with_core_commands()),
            Arc::new(TuiRegistry::default()),
            TuiInfo {
                cwd: PathBuf::from("/workspace"),
                provider: "test".to_string(),
                model: "model".to_string(),
                thinking: ThinkingLevel::Off,
                session_id: "session".to_string(),
                context_window: 128_000,
                model_ready: true,
                provider_count: 1,
                context_tokens: 0,
                terminal: None,
            },
            Keymap::with_defaults().unwrap(),
            String::new(),
            show_splash,
        )
    }

    fn test_app() -> App {
        test_app_with_splash(true)
    }

    fn render_to_string(app: &mut App, width: u16, height: u16) -> String {
        app.skip_splash();
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(width as usize)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn durable_assistant_text_replaces_streaming_deltas() {
        let mut app = test_app();
        app.apply_transient(EventKind::AssistantText {
            text: "streamed ".into(),
        });
        app.apply_transient(EventKind::AssistantText {
            text: "draft".into(),
        });
        assert_eq!(app.blocks.len(), 1);
        assert_eq!(app.blocks[0].text, "streamed draft");

        app.apply(SessionEvent {
            sequence: 0,
            at: chrono::Utc::now(),
            kind: EventKind::Compaction {
                summary: "overflow checkpoint".into(),
                tokens_before: 100,
                replacement_history: Vec::new(),
                manual: false,
            },
        });
        app.apply(SessionEvent {
            sequence: 1,
            at: chrono::Utc::now(),
            kind: EventKind::AssistantText {
                text: "settled response".into(),
            },
        });

        assert_eq!(app.blocks.len(), 2);
        assert_eq!(app.blocks[0].kind, BlockKind::Compaction);
        assert_eq!(app.blocks[1].text, "settled response");
        assert!(!app.blocks[1].transient);
    }

    #[test]
    fn streaming_reasoning_keeps_the_users_fold_through_settlement() {
        let mut app = test_app();
        app.apply_transient(EventKind::AssistantReasoning {
            text: "first ".into(),
        });
        assert!(app.blocks[0].expanded);

        app.toggle_selected();
        app.apply_transient(EventKind::AssistantReasoning {
            text: "second".into(),
        });
        assert!(!app.blocks[0].expanded);
        assert_eq!(app.blocks[0].text, "first second");

        app.apply(SessionEvent {
            sequence: 0,
            at: chrono::Utc::now(),
            kind: EventKind::AssistantReasoning {
                text: "first second".into(),
            },
        });
        assert_eq!(app.blocks.len(), 1);
        assert!(!app.blocks[0].transient);
        assert!(!app.blocks[0].expanded);

        app.apply(SessionEvent {
            sequence: 1,
            at: chrono::Utc::now(),
            kind: EventKind::ModelMessage {
                message: rig::message::Message::assistant("settled"),
            },
        });
        app.apply_transient(EventKind::AssistantReasoning {
            text: "next round".into(),
        });
        assert!(app.blocks.last().unwrap().expanded);
    }

    #[test]
    fn reasoning_folds_when_streaming_advances_to_text_or_a_tool() {
        let mut text_app = test_app();
        text_app.apply_transient(EventKind::AssistantReasoning {
            text: "thinking".into(),
        });
        assert!(text_app.blocks[0].expanded);
        text_app.apply_transient(EventKind::AssistantText {
            text: "answer".into(),
        });
        assert!(!text_app.blocks[0].expanded);

        let mut tool_app = test_app();
        tool_app.apply(SessionEvent {
            sequence: 0,
            at: chrono::Utc::now(),
            kind: EventKind::AssistantReasoning {
                text: "inspect".into(),
            },
        });
        assert!(tool_app.blocks[0].expanded);
        tool_app.apply(SessionEvent {
            sequence: 1,
            at: chrono::Utc::now(),
            kind: EventKind::ToolCall {
                call_id: "call".into(),
                name: "read".into(),
                arguments: serde_json::json!({"uri": "file://src/tui.rs"}),
            },
        });
        assert!(!tool_app.blocks[0].expanded);
        assert_eq!(tool_app.blocks[1].kind, BlockKind::Tool);
    }

    #[test]
    fn duplicate_settled_boundary_clears_a_stale_hydration_delta() {
        let mut app = test_app();
        let boundary = SessionEvent {
            sequence: 1,
            at: chrono::Utc::now(),
            kind: EventKind::ModelMessage {
                message: rig::message::Message::assistant("settled response"),
            },
        };
        app.apply(SessionEvent {
            sequence: 0,
            at: chrono::Utc::now(),
            kind: EventKind::AssistantText {
                text: "settled response".into(),
            },
        });
        app.apply(boundary.clone());

        // A subscriber can receive a queued delta that predates the snapshot
        // used to hydrate the transcript, followed by this duplicate boundary.
        app.apply_transient(EventKind::AssistantText {
            text: "stale streamed text".into(),
        });
        app.apply(boundary);

        assert_eq!(app.blocks.len(), 1);
        assert_eq!(app.blocks[0].text, "settled response");
        assert!(!app.blocks[0].transient);
    }

    #[test]
    fn unconfigured_brand_asks_for_login_instead_of_a_default_model() {
        let mut app = test_app();
        app.info.provider.clear();
        app.info.model.clear();
        app.info.model_ready = false;
        let rendered = render_to_string(&mut app, 100, 24);
        let compact = rendered
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>();
        assert!(compact.contains("尚未配置，请运行:login"));
        assert!(!rendered.contains("gpt-5.2"));
        assert!(!rendered.contains("openai/"));
    }

    #[test]
    fn welcome_keeps_its_layout_with_a_centered_local_key_hint() {
        let mut app = test_app();
        let rendered = render_to_string(&mut app, 100, 24);
        assert!(rendered.contains("/workspace"));
        assert!(rendered.contains("test / model · effort off"));
        assert!(rendered.contains("i compose · : commands · ? help"));
        assert!(!rendered.contains("tokens"));
        assert!(!rendered.contains("ctx "));

        let lines = rendered.lines().collect::<Vec<_>>();
        let project_row = lines
            .iter()
            .position(|line| line.contains("/workspace"))
            .expect("welcome project row");
        assert_eq!(project_row, 13);
        assert_eq!(lines[project_row].find("/workspace"), Some(45));
        assert_eq!(
            lines[project_row + 1].find("test / model · effort off"),
            Some(38)
        );
        let hint_row = project_row + 3;
        assert!(lines[hint_row].contains("i compose · : commands · ? help"));
        assert_eq!(lines[hint_row].find("i compose"), Some(35));
        assert_eq!(lines.len() - hint_row - 1, 7);
    }

    #[test]
    fn conversation_footer_only_shows_context_model_and_effort() {
        let mut app = test_app();
        app.push(
            BlockKind::Assistant,
            "AGENT",
            "answer".to_string(),
            None,
            false,
            false,
        );
        let rendered = render_to_string(&mut app, 100, 24);
        let footer = rendered
            .lines()
            .find(|line| line.contains("········ 0.0%/128k"))
            .expect("minimal conversation footer");
        // Once records exist the header and its animation are gone. The footer
        // leaves richer project and usage details to the expanded status panel.
        assert!(!rendered.contains("URI Agent"));
        assert!(!rendered.contains("ready"));
        assert!(footer.starts_with("model · effort off"));
        assert!(footer.trim_end().ends_with("········ 0.0%/128k"));
        assert!(!footer.contains("ctx"));
        assert!(footer.contains("model"));
        assert!(!footer.contains("URI"));
        assert!(!footer.contains("/workspace"));
        assert!(!footer.contains("tokens"));
        assert!(!footer.contains("F4"));
        assert!(!footer.contains('│'));
        assert!(!rendered.contains("i compose"));
        assert!(!rendered.contains(": command"));
        assert!(!rendered.contains("r thinking"));
        assert!(!rendered.contains("BROWSE"));
        assert!(!rendered.contains("INSERT"));
        assert!(!rendered.contains("event 1/1"));

        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let model = &terminal.backend().buffer()[(0, 23)];
        let context = &terminal.backend().buffer()[(82, 23)];
        assert_eq!((context.fg, context.bg), (ACCENT, SURFACE));
        assert!(context.modifier.contains(Modifier::BOLD));
        assert_eq!((model.fg, model.bg), (TEXT, SURFACE));
        assert!(model.modifier.contains(Modifier::BOLD));
        assert_eq!(terminal.backend().buffer()[(99, 23)].bg, SURFACE);

        let status_region = app
            .hit_regions
            .iter()
            .find(|region| region.target == AppHit::Status)
            .copied()
            .expect("footer mouse target");
        assert_eq!(status_region.area, Rect::new(0, 23, 100, 1));
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: status_region.area.x,
            row: status_region.area.y,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(hit_target(&app.hit_regions, click), Some(AppHit::Status));
        open_status(&mut app);
        assert!(app.overlay == Some(Overlay::Status));
    }

    #[test]
    fn compact_footer_right_aligns_context_and_handles_narrow_widths() {
        let mut app = test_app();
        app.skip_splash();
        app.push(
            BlockKind::Assistant,
            "AGENT",
            "answer".to_string(),
            None,
            false,
            true,
        );
        let backend = TestBackend::new(12, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        let footer = &terminal.backend().buffer().content()[7 * 12..8 * 12];
        assert_eq!(footer.last().unwrap().symbol(), "…");
        assert_eq!(footer.last().unwrap().fg, ACCENT);
        assert!(footer.iter().all(|cell| cell.bg == SURFACE));
        assert!(footer.iter().all(|cell| cell.fg != TEXT));
        assert_eq!(single_line_preview("模型 名称", 5), "模型…");
        assert_eq!(single_line_preview("model", 0), "");
    }

    #[test]
    fn compact_footer_stays_minimal_while_expanded_status_keeps_usage_details() {
        let mut app = test_app();
        app.info.provider_count = 2;
        app.info.context_window = 262_144;
        app.info.context_tokens = 26_214;
        app.push(
            BlockKind::User,
            "YOU",
            "hello".to_string(),
            None,
            false,
            false,
        );
        app.apply(SessionEvent {
            sequence: 1,
            at: chrono::Utc::now(),
            kind: EventKind::Usage {
                input: 1_500,
                output: 600,
                cache_read: 500,
                cache_write: 0,
                cost: 0.0123,
            },
        });
        let rendered = render_to_string(&mut app, 100, 24);
        assert!(!rendered.contains("↑1.5k"));
        assert!(!rendered.contains("↓600"));
        assert!(!rendered.contains("$0.012"));
        let footer = rendered.lines().last().unwrap();
        assert!(footer.starts_with("test/model · effort off"));
        assert!(footer.trim_end().ends_with("▓······· 10.0%/262k"));
        assert!(!rendered.contains("last hit 25.0%"));

        app.overlay = Some(Overlay::Status);
        let rendered = render_to_string(&mut app, 100, 24);
        assert!(rendered.contains("STATUS · F4 toggle"));
        assert!(rendered.contains("test / model · effort off"));
        assert!(rendered.contains("26k / 262k · 10.0%"));
        assert!(rendered.contains("read 500 · write 0 · last hit 25.0%"));
        assert!(rendered.contains("$0.0123"));
        // Usage events remain available in status without adding transcript blocks.
        assert_eq!(app.blocks.len(), 1);
    }

    #[test]
    fn transcript_uses_role_specific_blocks_and_mouse_regions() {
        let mut app = test_app();
        app.push(
            BlockKind::User,
            "YOU",
            "Inspect the renderer\nand keep navigation intact.".to_string(),
            None,
            false,
            false,
        );
        app.push(
            BlockKind::Assistant,
            "AGENT",
            "I updated the conversation hierarchy.".to_string(),
            None,
            false,
            true,
        );
        app.push(
            BlockKind::Reasoning,
            "THINKING",
            "Compare the reference implementations.".to_string(),
            None,
            false,
            false,
        );
        app.push(
            BlockKind::Tool,
            "READ · file://src/tui.rs",
            "CALL\n{}\n\nRESULT\nsource".to_string(),
            Some("call-1".to_string()),
            false,
            false,
        );
        app.selected_block = 0;

        let rendered = render_to_string(&mut app, 100, 24);
        assert!(rendered.contains("Inspect the renderer"));
        assert!(rendered.contains("I updated the conversation hierarchy."));
        assert!(rendered.contains("◇ Thought  ▸ Enter to expand"));
        assert!(rendered.contains("✓ READ · file://src/tui.rs  ▸"));
        assert!(!rendered.contains('▌'));
        assert!(!rendered.contains("› "));
        assert!(!rendered.contains("• "));
        assert!(!rendered.contains("YOU"));
        assert!(!rendered.contains("AGENT"));
        assert!(!rendered.contains("THINKING"));

        let user_rows = app
            .hit_regions
            .iter()
            .filter_map(|region| (region.target == AppHit::Transcript(0)).then_some(region.area.y))
            .collect::<Vec<_>>();
        assert_eq!(user_rows.len(), 2);
        assert!(!app.hit_regions.iter().any(|region| {
            region.area.y == user_rows[1] + 1 && matches!(region.target, AppHit::Transcript(_))
        }));

        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(
            terminal.backend().buffer()[(10, user_rows[0])].bg,
            USER_SURFACE
        );
        assert_eq!(
            terminal.backend().buffer()[(98, user_rows[0])].bg,
            USER_SURFACE
        );
        let assistant_row = app
            .hit_regions
            .iter()
            .find_map(|region| (region.target == AppHit::Transcript(1)).then_some(region.area.y))
            .unwrap();
        assert_eq!(terminal.backend().buffer()[(10, assistant_row)].bg, BG);
        let reasoning_row = app
            .hit_regions
            .iter()
            .find_map(|region| (region.target == AppHit::Transcript(2)).then_some(region.area.y))
            .unwrap();
        let tool_row = app
            .hit_regions
            .iter()
            .find_map(|region| (region.target == AppHit::Transcript(3)).then_some(region.area.y))
            .unwrap();
        assert_eq!(tool_row, reasoning_row + 1);

        app.selected_block = 1;
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(terminal.backend().buffer()[(10, assistant_row)].bg, BG);
        app.selected_block = 2;
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(
            terminal.backend().buffer()[(10, reasoning_row)].bg,
            ROW_ACTIVE
        );
    }

    #[test]
    fn user_and_assistant_content_aligns_to_both_transcript_edges() {
        let mut app = test_app();
        app.push(BlockKind::User, "YOU", "U".repeat(38), None, false, false);
        app.push(
            BlockKind::Assistant,
            "AGENT",
            "A".repeat(38),
            None,
            false,
            true,
        );
        app.skip_splash();
        let backend = TestBackend::new(40, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let user_row = app
            .hit_regions
            .iter()
            .find_map(|region| (region.target == AppHit::Transcript(0)).then_some(region.area.y))
            .unwrap();
        let assistant_row = app
            .hit_regions
            .iter()
            .find_map(|region| (region.target == AppHit::Transcript(1)).then_some(region.area.y))
            .unwrap();

        for (row, symbol) in [(user_row, "U"), (assistant_row, "A")] {
            assert_eq!(terminal.backend().buffer()[(1, row)].symbol(), symbol);
            assert_eq!(terminal.backend().buffer()[(38, row)].symbol(), symbol);
            let background = if symbol == "U" { USER_SURFACE } else { BG };
            assert_eq!(terminal.backend().buffer()[(1, row)].bg, background);
            assert_eq!(terminal.backend().buffer()[(38, row)].bg, background);
        }
    }

    #[test]
    fn reasoning_and_tool_content_aligns_to_both_transcript_edges() {
        let mut app = test_app();
        app.push(
            BlockKind::Reasoning,
            "THINKING",
            "R".repeat(36),
            None,
            false,
            true,
        );
        app.push(
            BlockKind::Tool,
            &"T".repeat(36),
            "CALL\n{}\n\nRESULT\nsource".into(),
            None,
            false,
            false,
        );
        app.selected_block = 0;
        app.skip_splash();
        let backend = TestBackend::new(40, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let reasoning_rows = app
            .hit_regions
            .iter()
            .filter_map(|region| (region.target == AppHit::Transcript(0)).then_some(region.area.y))
            .collect::<Vec<_>>();
        let tool_row = app
            .hit_regions
            .iter()
            .find_map(|region| (region.target == AppHit::Transcript(1)).then_some(region.area.y))
            .unwrap();

        assert_eq!(
            terminal.backend().buffer()[(1, reasoning_rows[0])].symbol(),
            "◇"
        );
        assert_eq!(
            terminal.backend().buffer()[(2, reasoning_rows[1])].symbol(),
            " "
        );
        assert_eq!(
            terminal.backend().buffer()[(3, reasoning_rows[1])].symbol(),
            "R"
        );
        assert_eq!(
            terminal.backend().buffer()[(38, reasoning_rows[1])].symbol(),
            "R"
        );
        assert_eq!(
            terminal.backend().buffer()[(1, reasoning_rows[1])].bg,
            ROW_ACTIVE
        );
        assert_eq!(
            terminal.backend().buffer()[(38, reasoning_rows[1])].bg,
            ROW_ACTIVE
        );
        assert_eq!(terminal.backend().buffer()[(1, tool_row)].symbol(), "✓");
        assert_eq!(terminal.backend().buffer()[(38, tool_row)].symbol(), "T");

        app.selected_block = 1;
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(terminal.backend().buffer()[(1, tool_row)].bg, ROW_ACTIVE);
        assert_eq!(terminal.backend().buffer()[(38, tool_row)].bg, ROW_ACTIVE);
    }

    #[test]
    fn assistant_reply_supports_direct_mouse_drag_selection() {
        let mut app = test_app();
        app.push(
            BlockKind::Assistant,
            "AGENT",
            "Select this response with the mouse.".into(),
            None,
            false,
            true,
        );
        render_to_string(&mut app, 80, 12);
        let row = app
            .hit_regions
            .iter()
            .find_map(|region| (region.target == AppHit::Transcript(0)).then_some(region.area.y))
            .unwrap();
        let down = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row,
            modifiers: KeyModifiers::NONE,
        };
        let drag = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 20,
            ..down
        };
        let up = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            ..drag
        };

        assert!(begin_direct_transcript_selection(&mut app, down));
        assert!(update_mouse_selection(&mut app, drag, true));
        assert!(update_mouse_selection(&mut app, up, true));
        assert!(
            app.selection
                .is_some_and(|selection| selection.start != selection.end)
        );
        assert_eq!(app.selected_block, 0);
        assert!(is_selection_copy_click(
            &app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Right),
                ..up
            }
        ));
        assert!(!is_selection_copy_click(
            &app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                ..up
            }
        ));
        app.copy_click_release_pending = true;
        assert!(consume_copy_click_release(&mut app, up));
        assert!(!app.copy_click_release_pending);

        app.selection = None;
        assert!(!is_selection_copy_click(
            &app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Right),
                ..up
            }
        ));
        assert!(begin_direct_transcript_selection(&mut app, down));
        assert!(update_mouse_selection(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                ..down
            },
            true
        ));
        assert!(app.selection.is_none());
    }

    #[test]
    fn first_process_block_has_one_blank_row_after_the_user_message() {
        let mut app = test_app();
        app.push(
            BlockKind::User,
            "YOU",
            "Inspect the renderer.".into(),
            None,
            false,
            false,
        );
        app.push(
            BlockKind::Reasoning,
            "THINKING",
            "Compare the references.".into(),
            None,
            false,
            false,
        );
        app.push(
            BlockKind::Assistant,
            "AGENT",
            "I will inspect the history.".into(),
            None,
            false,
            true,
        );

        let rendered = render_to_string(&mut app, 80, 12);
        let user_row = app
            .hit_regions
            .iter()
            .find_map(|region| (region.target == AppHit::Transcript(0)).then_some(region.area.y))
            .unwrap();
        let reasoning_row = app
            .hit_regions
            .iter()
            .find_map(|region| (region.target == AppHit::Transcript(1)).then_some(region.area.y))
            .unwrap();
        let assistant_row = app
            .hit_regions
            .iter()
            .find_map(|region| (region.target == AppHit::Transcript(2)).then_some(region.area.y))
            .unwrap();

        assert_eq!(reasoning_row, user_row + 2);
        assert_eq!(assistant_row, reasoning_row + 1);
        assert!(
            rendered
                .lines()
                .nth((user_row + 1) as usize)
                .unwrap()
                .trim()
                .is_empty()
        );
        assert!(transcript_needs_gap(
            BlockKind::User,
            BlockKind::Tool,
            false
        ));
        assert!(!transcript_needs_gap(
            BlockKind::Reasoning,
            BlockKind::Assistant,
            false
        ));
        assert!(!transcript_needs_gap(
            BlockKind::Reasoning,
            BlockKind::Tool,
            false
        ));
    }

    #[test]
    fn final_assistant_response_has_one_blank_row_after_the_process() {
        let mut app = test_app();
        app.push(
            BlockKind::Assistant,
            "AGENT",
            "I will inspect the history.".into(),
            None,
            false,
            true,
        );
        app.push(
            BlockKind::Reasoning,
            "THINKING",
            "Summarize the result.".into(),
            None,
            false,
            false,
        );
        app.push(
            BlockKind::Assistant,
            "AGENT",
            "Here is the final answer.".into(),
            None,
            false,
            true,
        );
        app.apply(SessionEvent {
            sequence: 0,
            at: chrono::Utc::now(),
            kind: EventKind::TurnFinished,
        });

        assert!(!app.blocks[0].final_response);
        assert!(app.blocks[2].final_response);
        let rendered = render_to_string(&mut app, 80, 12);
        let reasoning_row = app
            .hit_regions
            .iter()
            .find_map(|region| (region.target == AppHit::Transcript(1)).then_some(region.area.y))
            .unwrap();
        let final_row = app
            .hit_regions
            .iter()
            .find_map(|region| (region.target == AppHit::Transcript(2)).then_some(region.area.y))
            .unwrap();

        assert_eq!(final_row, reasoning_row + 2);
        assert!(
            rendered
                .lines()
                .nth((reasoning_row + 1) as usize)
                .unwrap()
                .trim()
                .is_empty()
        );
        assert!(transcript_needs_gap(
            BlockKind::Reasoning,
            BlockKind::Assistant,
            true
        ));
    }

    #[test]
    fn assistant_transcript_renders_markdown_instead_of_source_markers() {
        let mut app = test_app();
        app.push(
            BlockKind::Assistant,
            "AGENT",
            "# Result\n\n- **done** with `cargo test`\n\n[Details](https://example.com)"
                .to_string(),
            None,
            false,
            true,
        );
        let rendered = render_to_string(&mut app, 100, 24);
        assert!(rendered.contains("# Result"));
        assert!(rendered.contains("• done with cargo test"));
        assert!(rendered.contains("Details (https://example.com)"));
        assert!(!rendered.contains("**done**"));
        assert!(!rendered.contains("`cargo test`"));
    }

    #[test]
    fn expanded_status_is_bottom_anchored_and_includes_plugin_rows() {
        let mut tui = TuiRegistry::default();
        tui.register_status("build", |context: &TuiStatusContext| {
            Some(
                TuiStatusItem::new(
                    "build",
                    if context.expanded {
                        format!("clean · session {}", context.session_id)
                    } else {
                        "clean".to_string()
                    },
                )
                .with_tone(TuiStatusTone::Accent),
            )
        })
        .unwrap();
        let mut app = test_app();
        app.tui = Arc::new(tui);
        app.push(
            BlockKind::Assistant,
            "AGENT",
            "answer".to_string(),
            None,
            false,
            false,
        );
        let rendered = render_to_string(&mut app, 140, 24);
        let footer = rendered
            .lines()
            .find(|line| line.contains("········ 0.0%/128k"))
            .expect("minimal conversation footer");
        assert!(!footer.contains("build clean"));

        assert_eq!(
            overlay_area(Rect::new(0, 0, 100, 24), Overlay::Status),
            Rect::new(2, 10, 96, 14)
        );
        app.overlay = Some(Overlay::Status);
        app.overlay_scroll = 6;
        let rendered = render_to_string(&mut app, 100, 24);
        assert!(rendered.contains("EXTENSIONS"));
        assert!(rendered.contains("clean · session session"));
    }

    #[test]
    fn git_branch_reads_heads_and_worktree_pointers() {
        let workspace = tempfile::tempdir().unwrap();
        let git = workspace.path().join(".git");
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        assert_eq!(git_branch(workspace.path()).as_deref(), Some("main"));
        std::fs::write(git.join("HEAD"), "0123456789abcdef\n").unwrap();
        assert_eq!(git_branch(workspace.path()).as_deref(), Some("detached"));

        let real = workspace.path().join("real-git");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("HEAD"), "ref: refs/heads/topic\n").unwrap();
        let worktree = workspace.path().join("worktree");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(worktree.join(".git"), format!("gitdir: {}", real.display())).unwrap();
        assert_eq!(git_branch(&worktree).as_deref(), Some("topic"));
        assert_eq!(git_branch(Path::new("/definitely/not/a/repo")), None);
    }

    #[test]
    fn selected_model_is_hidden_until_credentials_are_ready() {
        let mut app = test_app();
        app.info.provider = "google".to_string();
        app.info.model = "gemini-3.1-pro-preview".to_string();
        app.info.model_ready = false;
        app.push(
            BlockKind::Assistant,
            "AGENT",
            "answer".to_string(),
            None,
            false,
            false,
        );
        let rendered = render_to_string(&mut app, 100, 24);
        assert!(rendered.contains("no-model"));
        assert!(!rendered.contains("google"));
        assert!(!rendered.contains("gemini-3.1-pro-preview"));
    }

    #[test]
    fn splash_uses_the_wordmark_then_conversation_replaces_it() {
        let mut app = test_app();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let splash = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(splash.contains("press any key"));
        assert!(!splash.contains("URI AGENT"));
        app.skip_splash();
        let rendered = render_to_string(&mut app, 80, 24);
        assert!(rendered.contains("/workspace"));
        assert!(rendered.contains("test / model · effort off"));
        assert!(rendered.contains("i compose · : commands · ? help"));
        assert!(!rendered.contains("tokens"));
        assert!(!rendered.contains("ctx "));
    }

    #[test]
    fn switched_session_opens_the_welcome_view_without_another_splash() {
        let mut app = test_app_with_splash(false);
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .chunks(80)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("/workspace"));
        assert!(rendered.contains("test / model · effort off"));
        assert!(!rendered.contains("press any key"));
    }

    #[test]
    fn composer_enter_sends_shift_enter_breaks_and_esc_keeps_draft() {
        let mut app = test_app();
        app.overlay = Some(Overlay::Composer);
        assert!(app.animations_paused());
        app.input.insert_str("first");
        assert_eq!(
            app.keymap.action("composer", "shift+enter").as_deref(),
            Some("newline")
        );
        app.input.insert_newline();
        app.input.insert_str("second");
        app.overlay = None;
        assert_eq!(app.draft_text(), "first\nsecond");
        app.overlay = Some(Overlay::Composer);
        let prompt = app.submit().unwrap();
        assert_eq!(prompt, "first\nsecond");
        assert!(app.draft_text().is_empty());
        assert!(app.overlay.is_none());
        assert!(!app.animations_paused());
    }

    #[test]
    fn ctrl_c_copies_a_selection_and_is_otherwise_ignored() {
        let app = test_app();
        assert!(is_ignored_tui_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            false
        ));
        assert!(!is_ignored_tui_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            true
        ));
        assert!(!is_ignored_tui_key(
            KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            ),
            false
        ));
        assert_eq!(app.keymap.action("main", "ctrl+c"), None);
        assert_eq!(app.keymap.action("composer", "ctrl+c"), None);
        assert_eq!(app.keymap.action("command", "ctrl+c"), None);
        assert_eq!(
            app.keymap.action("selection", "ctrl+c").as_deref(),
            Some("copy")
        );
    }

    #[test]
    fn composer_places_the_terminal_cursor_at_the_unicode_caret() {
        let mut app = test_app();
        app.skip_splash();
        app.overlay = Some(Overlay::Composer);
        app.input.insert_str("你好");
        app.input.insert_newline();
        app.input.insert_str("ok");
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        terminal.backend_mut().assert_cursor_position((5, 18));
    }

    #[test]
    fn composer_is_bottom_anchored_with_a_rounded_frame_and_placeholder() {
        let mut app = test_app();
        app.overlay = Some(Overlay::Composer);
        let rendered = render_to_string(&mut app, 80, 24);
        let rows = rendered.lines().collect::<Vec<_>>();

        assert!(rows[16].starts_with("  ╭"));
        assert!(rows[16].contains("MESSAGE"));
        assert!(rows[17].contains("Ask URI Agent to build, explain, or fix…"));
        assert!(rows[23].starts_with("  ╰"));
        assert!(rows[23].contains("Enter send · Shift+Enter newline · Esc keep draft"));
        assert!(rows[23].ends_with("╯  "));
    }

    #[test]
    fn composer_cursor_tracks_horizontal_scrolling() {
        let mut input = TextArea::default();
        style_input(&mut input, false);
        input.insert_str("123456789");
        let mut scroll = (0, 0);
        assert_eq!(
            composer_cursor_position(&input, Rect::new(2, 12, 10, 10), &mut scroll),
            Some((10, 13))
        );
        assert_eq!(scroll, (0, 2));
    }

    #[test]
    fn enter_toggles_a_folded_history_block() {
        let mut app = test_app();
        app.push(
            BlockKind::Reasoning,
            "THINKING",
            "secret chain".to_string(),
            None,
            false,
            false,
        );
        app.toggle_selected();
        assert!(app.blocks[0].expanded);
        app.toggle_selected();
        assert!(!app.blocks[0].expanded);
    }

    #[test]
    fn long_history_blocks_fold_instead_of_opening_a_document() {
        for kind in [BlockKind::Reasoning, BlockKind::Tool] {
            let mut app = test_app();
            app.transcript_body_width = 8;
            app.push(
                kind,
                "DETAIL",
                "long content ".repeat(240),
                None,
                false,
                false,
            );

            app.toggle_selected();
            assert!(app.blocks[0].expanded);
            if kind == BlockKind::Reasoning {
                let rendered = render_to_string(&mut app, 100, 60);
                assert!(rendered.contains("O or double-click opens full"));
                assert!(!rendered.contains("Enter opens"));
            }
            app.toggle_selected();

            assert!(!app.blocks[0].expanded);
            assert!(app.overlay.is_none());
            assert!(app.document.is_none());
            assert!(app.transcript_center_selected);
        }
    }

    #[test]
    fn opening_a_full_document_does_not_change_the_folded_state() {
        let mut app = test_app();
        app.push(
            BlockKind::Reasoning,
            "THINKING",
            "full thought".to_string(),
            None,
            false,
            false,
        );

        app.open_selected_document();

        assert!(!app.blocks[0].expanded);
        assert!(app.overlay == Some(Overlay::Document));
        assert_eq!(
            app.document.as_ref(),
            Some(&(
                "THINKING".to_string(),
                "# THINKING\n\nfull thought\n".to_string()
            ))
        );
    }

    #[test]
    fn clicking_outside_closes_only_the_full_document_float() {
        let mut app = test_app();
        app.overlay = Some(Overlay::Document);
        app.overlay_scroll = 6;
        app.selection = Some(TextSelection {
            start: (20, 8),
            end: (24, 8),
        });
        render_to_string(&mut app, 100, 24);
        let bounds = app.overlay_bounds.expect("rendered document bounds");
        let click = |column, row| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };

        assert!(!close_document_on_outside_click(
            &mut app,
            click(bounds.x, bounds.y)
        ));
        assert!(app.overlay == Some(Overlay::Document));

        assert!(close_document_on_outside_click(
            &mut app,
            click(bounds.x.saturating_sub(1), bounds.y)
        ));
        assert!(app.overlay.is_none());
        assert_eq!(app.overlay_scroll, 0);
        assert!(app.selection.is_none());

        app.overlay = Some(Overlay::Help);
        app.overlay_bounds = Some(bounds);
        assert!(!close_document_on_outside_click(
            &mut app,
            click(bounds.x.saturating_sub(1), bounds.y)
        ));
        assert!(app.overlay == Some(Overlay::Help));
    }

    #[test]
    fn transcript_double_click_opens_without_changing_the_folded_state() {
        let mut app = test_app();
        app.push(
            BlockKind::Reasoning,
            "THINKING",
            "full thought".to_string(),
            None,
            false,
            false,
        );

        app.queue_transcript_click(0);
        assert!(!app.blocks[0].expanded);
        assert!(app.pending_transcript_click.is_some());
        app.queue_transcript_click(0);

        assert!(!app.blocks[0].expanded);
        assert!(app.overlay == Some(Overlay::Document));
        assert!(app.document.is_some());
        assert!(app.pending_transcript_click.is_none());
        assert_eq!(app.selected_block, 0);
        assert!(!app.transcript_follow_tail);
    }

    #[test]
    fn transcript_single_click_waits_before_toggling_each_time() {
        let mut app = test_app();
        app.push(
            BlockKind::Reasoning,
            "THINKING",
            "full thought".to_string(),
            None,
            false,
            false,
        );

        app.queue_transcript_click(0);
        assert!(!app.blocks[0].expanded);
        app.pending_transcript_click.as_mut().unwrap().1 = Instant::now() - DOUBLE_CLICK_INTERVAL;
        app.confirm_pending_transcript_click_if_elapsed();
        assert!(app.blocks[0].expanded);
        assert!(app.pending_transcript_click.is_none());

        app.queue_transcript_click(0);
        assert!(app.blocks[0].expanded);
        app.pending_transcript_click.as_mut().unwrap().1 = Instant::now() - DOUBLE_CLICK_INTERVAL;
        app.confirm_pending_transcript_click_if_elapsed();
        assert!(!app.blocks[0].expanded);
        assert!(app.overlay.is_none());
    }

    #[test]
    fn user_and_assistant_messages_never_fold_or_open_a_document() {
        let mut app = test_app();
        app.transcript_body_width = 8;
        app.push(
            BlockKind::Assistant,
            "AGENT",
            "word ".repeat(60),
            None,
            false,
            true,
        );

        app.toggle_selected();
        app.open_selected_document();

        assert!(app.overlay.is_none());
        assert!(app.document.is_none());
        assert!(app.blocks[0].expanded);
        let rows = transcript_block_items(&app.blocks[0], true, false, 8, 8, &app);
        assert!(rows.len() > EXPANDED_PREVIEW_LINES);

        app.blocks.clear();
        app.push(
            BlockKind::User,
            "YOU",
            "prompt ".repeat(60),
            None,
            false,
            false,
        );
        app.toggle_selected();
        app.open_selected_document();
        assert!(!app.blocks[0].expanded);
        assert!(app.overlay.is_none());
        assert!(app.document.is_none());
        assert!(transcript_block_items(&app.blocks[0], true, false, 8, 8, &app).len() > 24);
    }

    #[test]
    fn jump_keys_move_between_matching_blocks() {
        let mut app = test_app();
        app.push(BlockKind::User, "YOU", "one".into(), None, false, false);
        app.push(
            BlockKind::Reasoning,
            "THINKING",
            "a".into(),
            None,
            false,
            false,
        );
        app.push(BlockKind::User, "YOU", "two".into(), None, false, false);
        app.push(
            BlockKind::Reasoning,
            "THINKING",
            "b".into(),
            None,
            false,
            false,
        );
        app.jump_to(JumpKind::User);
        assert_eq!(app.jump, JumpKind::User);
        assert_eq!(app.selected_block, 2);
        app.jump_to(JumpKind::User);
        assert_eq!(app.selected_block, 0);
        app.jump_to(JumpKind::Reasoning);
        assert_eq!(app.jump, JumpKind::Reasoning);
        assert_eq!(app.blocks[app.selected_block].kind, BlockKind::Reasoning);
    }

    #[test]
    fn list_selection_wraps_in_both_directions() {
        assert_eq!(wrapped_index(0, -1, 3), 2);
        assert_eq!(wrapped_index(2, 1, 3), 0);
        assert_eq!(wrapped_index(2, 4, 3), 0);
        assert_eq!(wrapped_index(8, 1, 0), 0);

        let mut app = test_app();
        app.push(BlockKind::User, "YOU", "one".into(), None, false, false);
        app.push(BlockKind::User, "YOU", "two".into(), None, false, false);
        app.selected_block = 0;
        app.move_selection(-1);
        assert_eq!(app.selected_block, 1);
        app.move_selection(1);
        assert_eq!(app.selected_block, 0);
    }

    #[test]
    fn transcript_scroll_is_independent_from_selection_and_follows_the_tail_again() {
        let mut app = test_app();
        for index in 0..30 {
            app.push(
                BlockKind::User,
                "YOU",
                format!("message {index}"),
                None,
                false,
                false,
            );
        }
        app.selected_block = 29;
        render_to_string(&mut app, 80, 12);
        let tail_offset = app.transcript_offset;
        let selected = app.selected_block;
        assert!(tail_offset > 0);

        app.scroll_transcript(-3);
        render_to_string(&mut app, 80, 12);
        assert_eq!(app.selected_block, selected);
        assert_eq!(app.transcript_offset, tail_offset - 3);
        assert!(!app.transcript_follow_tail);

        app.push(
            BlockKind::User,
            "YOU",
            "message 30".to_string(),
            None,
            false,
            false,
        );
        render_to_string(&mut app, 80, 12);
        assert_eq!(app.transcript_offset, tail_offset - 3);
        assert_eq!(app.selected_block, selected);

        app.scroll_transcript(4);
        render_to_string(&mut app, 80, 12);
        assert_eq!(app.transcript_offset, tail_offset + 1);
        assert!(app.transcript_follow_tail);

        app.push(
            BlockKind::User,
            "YOU",
            "message 31".to_string(),
            None,
            false,
            false,
        );
        render_to_string(&mut app, 80, 12);
        assert_eq!(app.transcript_offset, tail_offset + 2);

        render_to_string(&mut app, 80, 40);
        assert_eq!(app.transcript_offset, 0);
    }

    #[test]
    fn keyboard_navigation_centers_an_offscreen_transcript_block() {
        let mut app = test_app();
        for index in 0..30 {
            app.push(
                BlockKind::User,
                "YOU",
                format!("message {index}"),
                None,
                false,
                false,
            );
        }
        app.selected_block = 29;
        render_to_string(&mut app, 80, 12);
        let tail_offset = app.transcript_offset;
        app.move_selection(-1);
        render_to_string(&mut app, 80, 12);
        assert_eq!(app.transcript_offset, tail_offset);

        app.selected_block = 29;
        app.move_selection(-11);
        render_to_string(&mut app, 80, 12);

        assert_eq!(app.selected_block, 18);
        assert_eq!(app.transcript_height, 11);
        assert_eq!(app.transcript_offset, 13);
        assert!(app.hit_regions.iter().any(|region| {
            region.target == AppHit::Transcript(18)
                && region.area.y == app.transcript_height as u16 / 2
        }));
    }

    #[test]
    fn terminal_command_rejects_an_empty_command() {
        assert!(terminal_command("").is_err());
        assert!(terminal_command("   ").is_err());
        assert!(terminal_command("pwsh -NoLogo").is_ok());
    }

    #[test]
    fn command_panel_input_filters_the_selection() {
        let mut app = test_app();
        app.overlay = Some(Overlay::Command);
        app.command_selected = 2;
        assert!(matches!(
            apply_command_key(
                &mut app,
                KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
                "e"
            ),
            CommandKey::Continue
        ));
        handle_paste(&mut app, "ffort".to_string());
        assert_eq!(app.command_query, "effort");
        assert_eq!(app.command_selected, 0);
        let commands = app.matching_commands();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].spec.id, "effort");

        assert!(matches!(
            apply_command_key(
                &mut app,
                KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
                "backspace"
            ),
            CommandKey::Continue
        ));
        assert_eq!(app.command_query, "effor");

        app.command_query.push_str("t high");
        assert!(app.matching_commands().is_empty());
        app.command_query.truncate("effort".len());
        let rendered = render_to_string(&mut app, 100, 32);
        assert!(
            rendered.contains("COMMAND · type to filter · Tab complete · Enter run · Esc close")
        );
        assert!(rendered.contains("⌕ effort█"));
        assert!(rendered.contains(":effort"));
        assert!(!rendered.contains(":terminal"));
    }

    #[test]
    fn command_panel_tab_completes_and_cycles_matches() {
        let mut app = test_app();
        app.overlay = Some(Overlay::Command);
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);

        app.command_query = "te".to_string();
        assert!(matches!(
            apply_command_key(&mut app, tab, "tab"),
            CommandKey::Continue
        ));
        assert_eq!(app.command_query, "terminal");

        app.reset_command_search();
        app.command_query = "t".to_string();
        apply_command_key(&mut app, tab, "tab");
        assert_eq!(app.command_query, "tasks");
        apply_command_key(&mut app, tab, "tab");
        assert_eq!(app.command_query, "terminal");
        apply_command_key(&mut app, tab, "tab");
        assert_eq!(app.command_query, "tasks");
        apply_command_key(
            &mut app,
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE),
            "backtab",
        );
        assert_eq!(app.command_query, "terminal");
    }

    #[test]
    fn command_panel_completion_handles_common_prefixes_aliases_and_search_matches() {
        let mut app = test_app();
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);

        app.command_query = "se".to_string();
        apply_command_key(&mut app, tab, "tab");
        assert_eq!(app.command_query, "search");
        apply_command_key(&mut app, tab, "tab");
        assert_eq!(app.command_query, "set-terminal");
        apply_command_key(&mut app, tab, "tab");
        assert_eq!(app.command_query, "settings");
        apply_command_key(&mut app, tab, "tab");
        assert_eq!(app.command_query, "search");

        app.reset_command_search();
        app.command_query = "th".to_string();
        apply_command_key(&mut app, tab, "tab");
        assert_eq!(app.command_query, "effort");

        app.reset_command_search();
        app.command_query = "erm".to_string();
        apply_command_key(&mut app, tab, "tab");
        assert_eq!(app.command_query, "set-terminal");
    }

    #[test]
    fn command_panel_searches_aliases_and_wraps_selection() {
        let mut app = test_app();
        app.overlay = Some(Overlay::Command);
        assert!(
            app.matching_commands()
                .iter()
                .all(|command| command.name == command.spec.id)
        );

        let key = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE);
        let key_name = key_name(key);
        assert!(matches!(
            apply_command_key(&mut app, key, &key_name),
            CommandKey::Continue
        ));
        let commands = app.matching_commands();
        assert_eq!(app.command_query, "t");
        assert_eq!(
            commands
                .iter()
                .find(|command| command.spec.id == "effort")
                .map(|command| command.name.as_str()),
            Some("thinking")
        );

        app.command_selected = 0;
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        assert!(matches!(
            apply_command_key(&mut app, up, "up"),
            CommandKey::Continue
        ));
        assert_eq!(app.command_selected, commands.len() - 1);
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        assert!(matches!(
            apply_command_key(&mut app, down, "down"),
            CommandKey::Continue
        ));
        assert_eq!(app.command_selected, 0);

        let rendered = render_to_string(&mut app, 100, 32);
        assert!(
            rendered.contains("COMMAND · type to filter · Tab complete · Enter run · Esc close")
        );
        assert!(rendered.contains("⌕ t█"));
        assert!(rendered.contains(":thinking"));
    }

    #[test]
    fn command_panel_searches_descriptions_and_fuzzy_command_names() {
        let commands = CommandRegistry::with_core_commands();

        let description_matches = matching_commands(&commands, "asynchronous protocol");
        assert_eq!(description_matches[0].spec.id, "tasks");
        assert_eq!(description_matches[0].name, "tasks");

        let fuzzy_description_matches = matching_commands(&commands, "asynprot");
        assert_eq!(fuzzy_description_matches[0].spec.id, "tasks");

        let fuzzy_matches = matching_commands(&commands, "sttus");
        assert_eq!(fuzzy_matches[0].spec.id, "status");
    }

    #[test]
    fn effort_command_uses_a_selector_with_the_current_level_selected() {
        let active = ActiveSettings {
            provider: "openai".to_string(),
            model: "reasoning-model".to_string(),
            api_key: None,
            auth_kind: AuthKind::None,
            output_limit: 32 * 1024,
            thinking: ThinkingLevel::High,
            provider_source: ValueSource::Global,
            model_source: ValueSource::Global,
            api_key_source: ValueSource::Default,
            output_limit_source: ValueSource::Global,
            thinking_source: ValueSource::Global,
            terminal: None,
            terminal_source: ValueSource::Default,
            credential_environment: BTreeMap::new(),
        };
        let model = serde_json::from_value(serde_json::json!({
            "id": "reasoning-model",
            "name": "Reasoning model",
            "api": "openai-responses",
            "provider": "openai",
            "baseUrl": "https://example.test/v1",
            "reasoning": true
        }))
        .unwrap();
        let selector = effort_selector(&active, &model);
        assert!(matches!(selector.kind, SelectorKind::Effort { .. }));
        assert_eq!(
            selector.selected_item().map(|item| item.id.as_str()),
            Some("high")
        );
        assert_eq!(
            selector
                .selected_item()
                .map(|item| item.description.as_str()),
            Some("current")
        );
        assert_eq!(
            selector
                .items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec!["off", "minimal", "low", "medium", "high"]
        );
    }

    #[test]
    fn resume_model_description_includes_effort() {
        let item = resume_item(
            "session",
            SessionSummary {
                id: "session".to_string(),
                updated_at: chrono::Utc::now(),
                provider: "openai".to_string(),
                model: "gpt-5".to_string(),
                thinking: ThinkingLevel::Medium,
                preview: "continue the work".to_string(),
            },
            ThinkingLevel::Medium,
        );
        assert_eq!(
            item.description,
            "openai/gpt-5 · effort medium · continue the work"
        );
    }

    #[test]
    fn conversation_search_filters_full_block_text_and_jumps_to_the_result() {
        let mut app = test_app();
        app.push(
            BlockKind::User,
            "YOU",
            "earlier request".to_string(),
            None,
            false,
            false,
        );
        app.push(
            BlockKind::Reasoning,
            "THINKING",
            format!(
                "first line\n{}\nNeedle in the later conversation text",
                "padding ".repeat(40)
            ),
            None,
            false,
            false,
        );

        open_search(&mut app);
        assert!(app.overlay == Some(Overlay::Selector));
        assert!(matches!(
            app.selector.as_ref().map(|selector| &selector.kind),
            Some(SelectorKind::Search)
        ));

        handle_paste(&mut app, "needle".to_string());
        let selector = app.selector.as_ref().unwrap();
        assert_eq!(selector.visible.len(), 1);
        assert_eq!(
            selector.selected_item().map(|item| item.id.as_str()),
            Some("1")
        );
        assert_eq!(
            selector
                .selected_item()
                .map(|item| item.description.as_str()),
            Some("Needle in the later conversation text")
        );
        let rendered = render_to_string(&mut app, 100, 32);
        assert!(rendered.contains("SEARCH · type to filter · click or Enter jump · Esc close"));
        assert!(rendered.contains("Needle in the later conversation text"));

        assert!(app.selector.as_mut().unwrap().select_from_click(0, false));

        app.selector = None;
        app.overlay = None;
        assert!(app.select_search_result(1));
        assert_eq!(app.selected_block, 1);
        assert!(app.blocks[1].expanded);
        assert!(!app.transcript_follow_tail);
        assert!(app.transcript_center_selected);
    }

    #[test]
    fn conversation_search_requires_text_and_is_not_active_in_the_resume_selector() {
        let mut app = test_app();
        open_search(&mut app);
        assert!(app.overlay.is_none());
        assert_eq!(app.visible_flash(), Some("No conversation text to search"));

        app.selector = Some(SelectorState::new(
            SelectorKind::Resume,
            "RESUME",
            vec![SelectorItem {
                id: "other-session".to_string(),
                title: "other-session".to_string(),
                description: "saved conversation".to_string(),
                search_text: None,
            }],
        ));
        app.overlay = Some(Overlay::Selector);
        assert_eq!(app.keymap.action("main", ":").as_deref(), Some("command"));
        let colon = KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE);
        assert!(matches!(
            apply_selector_key(&mut app, colon, ":"),
            SelectorKey::Continue
        ));
        assert!(app.overlay == Some(Overlay::Selector));
        let selector = app.selector.as_ref().unwrap();
        assert!(matches!(selector.kind, SelectorKind::Resume));
        assert_eq!(selector.query, ":");
        let selector = app.selector.as_mut().unwrap();
        assert!(!selector.select_from_click(0, false));
        assert!(selector.select_from_click(0, true));
    }

    #[test]
    fn login_selector_types_letters_that_are_list_motion_aliases() {
        let mut app = test_app();
        app.selector = Some(SelectorState::new(
            SelectorKind::LoginProvider,
            "LOGIN",
            vec![
                SelectorItem {
                    id: "anthropic".to_string(),
                    title: "anthropic".to_string(),
                    description: "Claude".to_string(),
                    search_text: None,
                },
                SelectorItem {
                    id: "kimi".to_string(),
                    title: "kimi".to_string(),
                    description: "Kimi Code".to_string(),
                    search_text: None,
                },
            ],
        ));
        let key = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::empty());
        let name = key_name(key);
        assert!(matches!(
            apply_selector_key(&mut app, key, &name),
            SelectorKey::Continue
        ));
        let selector = app.selector.as_mut().unwrap();
        assert_eq!(selector.query, "k");
        assert_eq!(
            selector.selected_item().map(|item| item.id.as_str()),
            Some("kimi")
        );
        selector.selected = 0;
        selector.move_selection(-1);
        assert_eq!(selector.selected, selector.visible.len() - 1);
        selector.move_selection(1);
        assert_eq!(selector.selected, 0);
    }

    #[test]
    fn colon_commands_include_login_logout_resume_and_search() {
        let commands = CommandRegistry::with_core_commands();
        assert_eq!(
            commands.resolve(":login").unwrap().spec.target,
            CommandTarget::Core(CoreCommand::Login)
        );
        assert_eq!(
            commands.resolve("logout").unwrap().spec.target,
            CommandTarget::Core(CoreCommand::Logout)
        );
        assert_eq!(
            commands.resolve("resume").unwrap().spec.target,
            CommandTarget::Core(CoreCommand::Resume)
        );
        assert_eq!(
            commands.resolve("sessions").unwrap().spec.target,
            CommandTarget::Core(CoreCommand::Resume)
        );
        assert_eq!(
            commands.resolve(":insert").unwrap().spec.target,
            CommandTarget::Core(CoreCommand::Compose)
        );
        assert_eq!(
            commands.resolve(":compose").unwrap().spec.target,
            CommandTarget::Core(CoreCommand::Compose)
        );
        assert_eq!(
            commands.resolve(":terminal").unwrap().spec.target,
            CommandTarget::Core(CoreCommand::Terminal)
        );
        assert_eq!(
            commands.resolve(":search").unwrap().spec.target,
            CommandTarget::Core(CoreCommand::Search)
        );
        assert_eq!(
            commands.resolve("find").unwrap().spec.target,
            CommandTarget::Core(CoreCommand::Search)
        );
        assert_eq!(
            commands.resolve("set-terminal pwsh").unwrap().arguments,
            "pwsh"
        );
        assert!(commands.resolve("editor").is_none());
    }

    #[test]
    fn command_panel_lists_colon_commands() {
        let mut app = test_app();
        app.overlay = Some(Overlay::Command);
        let rendered = render_to_string(&mut app, 100, 32);
        assert!(rendered.contains(":login"));
        assert!(rendered.contains(":resume"));
        assert!(rendered.contains(":search"));
        assert!(rendered.contains(":insert"));
        assert!(rendered.contains(":quit"));
        assert!(!rendered.contains(":compose"));
        assert!(!rendered.contains(":thinking"));
        assert!(!rendered.contains(":terminal-set"));
        assert!(!rendered.contains("Open in editor"));

        app.command_selected = app.commands.list().len() - 1;
        let rendered = render_to_string(&mut app, 100, 32);
        assert!(rendered.contains(":terminal"));
        assert!(!rendered.contains(":term "));
    }

    #[test]
    fn settings_panel_hides_the_api_key_and_cycles_thinking() {
        let active = ActiveSettings {
            provider: "openai".to_string(),
            model: "gpt-5.2".to_string(),
            api_key: Some("super-secret-value".to_string()),
            auth_kind: AuthKind::ApiKey,
            output_limit: 32 * 1024,
            thinking: ThinkingLevel::Off,
            provider_source: ValueSource::Global,
            model_source: ValueSource::Global,
            api_key_source: ValueSource::Global,
            output_limit_source: ValueSource::Global,
            thinking_source: ValueSource::Global,
            terminal: None,
            terminal_source: ValueSource::Global,
            credential_environment: BTreeMap::new(),
        };
        let mut app = test_app();
        app.overlay = Some(Overlay::Settings);
        app.settings = Some(SettingsState {
            active,
            model: None,
            selected: 0,
            editing: None,
            api_key: String::new(),
            api_key_changed: false,
            thinking: ThinkingLevel::Off,
            output_limit: "32768".to_string(),
        });
        let rendered = render_to_string(&mut app, 100, 24);
        assert!(rendered.contains("SETTINGS"));
        assert!(rendered.contains("API key"));
        assert!(rendered.contains("Thinking"));
        assert!(rendered.contains("off"));
        assert!(!rendered.contains("super-secret-value"));
        assert!(!rendered.contains("Editor"));
        assert!(!rendered.contains("fzf"));

        let settings = app.settings.as_mut().unwrap();
        settings.model = Some(
            serde_json::from_value(serde_json::json!({
                "id": "reasoning-model",
                "name": "Reasoning model",
                "api": "openai-responses",
                "provider": "openai",
                "baseUrl": "https://example.test/v1",
                "reasoning": true
            }))
            .unwrap(),
        );
        settings.selected = 2;
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::empty());
        assert!(matches!(
            handle_settings_key(&mut app, key, &key_name(key)),
            Action::Continue
        ));
        assert_eq!(
            app.settings.as_ref().unwrap().thinking,
            ThinkingLevel::Minimal
        );
    }

    #[test]
    fn tool_call_and_result_share_one_block() {
        let mut app = test_app();
        app.apply(SessionEvent {
            sequence: 1,
            at: chrono::Utc::now(),
            kind: EventKind::ToolCall {
                call_id: "call-1".to_string(),
                name: "read".to_string(),
                arguments: serde_json::json!({"uri": "file://src/main.rs"}),
            },
        });
        app.apply(SessionEvent {
            sequence: 2,
            at: chrono::Utc::now(),
            kind: EventKind::ToolResult {
                call_id: "call-1".to_string(),
                name: "read".to_string(),
                output: "complete tool output".to_string(),
                failed: false,
            },
        });
        assert_eq!(app.blocks.len(), 1);
        assert_eq!(app.blocks[0].title, "Read src/main.rs");
        assert!(app.blocks[0].text.contains("CALL"));
        assert!(app.blocks[0].text.contains("RESULT"));
        assert!(block_document(&app.blocks[0]).contains("complete tool output"));

        let collapsed = render_to_string(&mut app, 100, 24);
        assert!(collapsed.contains("✓ Read src/main.rs"));
        assert!(!collapsed.contains("{\"uri\""));
        assert!(!collapsed.contains("CALL"));
        app.blocks[0].expanded = true;
        let expanded = render_to_string(&mut app, 100, 24);
        assert!(expanded.contains("↳ file://src/main.rs"));
        assert!(expanded.contains("└ complete tool output"));
        assert!(!expanded.contains("{\"uri\""));
    }

    #[test]
    fn tool_summaries_describe_shell_patch_and_unknown_arguments_without_json() {
        assert_eq!(
            tool_title(
                "exec",
                &serde_json::json!({"uri": "bash://run", "body": "cargo test\necho done"})
            ),
            "$ cargo test"
        );
        assert_eq!(
            tool_title(
                "exec",
                &serde_json::json!({
                    "uri": "apply_patch://run",
                    "body": "*** Begin Patch\n*** Update File: src/tui.rs\n*** Update File: Cargo.toml\n*** End Patch"
                })
            ),
            "Patched src/tui.rs +1"
        );

        let mut lines = Vec::new();
        tool_argument_details(
            &serde_json::json!({"body": {"path": "src/main.rs", "limit": 20}}),
            &mut lines,
        );
        let text = lines
            .into_iter()
            .map(|(line, _)| line)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("path: src/main.rs"));
        assert!(text.contains("limit: 20"));
        assert!(!text.contains(['{', '}', '"']));
    }

    #[test]
    fn activity_status_follows_stream_events() {
        let mut app = test_app();
        app.apply(SessionEvent {
            sequence: 1,
            at: chrono::Utc::now(),
            kind: EventKind::User {
                text: "inspect files".to_string(),
            },
        });
        assert!(app.busy);
        let rendered = render_to_string(&mut app, 100, 24);
        let rows = rendered.lines().collect::<Vec<_>>();
        assert!(rows[22].contains("thinking"));
        assert!(rows[23].starts_with("model · effort off"));
        assert!(rows[23].trim_end().ends_with("········ 0.0%/128k"));
        app.apply(SessionEvent {
            sequence: 2,
            at: chrono::Utc::now(),
            kind: EventKind::ToolCall {
                call_id: "call".to_string(),
                name: "read".to_string(),
                arguments: serde_json::json!({"uri": "file://src/main.rs"}),
            },
        });
        assert!(matches!(&app.activity, Some(Activity::Tool(name)) if name == "file"));
        app.apply(SessionEvent {
            sequence: 3,
            at: chrono::Utc::now(),
            kind: EventKind::TurnFinished,
        });
        assert!(!app.busy);
        assert!(
            app.blocks
                .iter()
                .filter(|block| block.kind == BlockKind::Tool)
                .all(|block| !block.expanded)
        );
    }

    #[test]
    fn activity_animation_stays_on_the_current_tool_instead_of_the_selection() {
        let mut app = test_app();
        app.apply(SessionEvent {
            sequence: 0,
            at: chrono::Utc::now(),
            kind: EventKind::User {
                text: "inspect files".into(),
            },
        });
        app.apply(SessionEvent {
            sequence: 1,
            at: chrono::Utc::now(),
            kind: EventKind::ToolCall {
                call_id: "old".into(),
                name: "read".into(),
                arguments: serde_json::json!({"uri": "file://old.rs"}),
            },
        });
        app.apply(SessionEvent {
            sequence: 2,
            at: chrono::Utc::now(),
            kind: EventKind::ToolResult {
                call_id: "old".into(),
                name: "read".into(),
                output: "done".into(),
                failed: false,
            },
        });
        app.apply(SessionEvent {
            sequence: 3,
            at: chrono::Utc::now(),
            kind: EventKind::ToolCall {
                call_id: "current".into(),
                name: "read".into(),
                arguments: serde_json::json!({"uri": "file://current.rs"}),
            },
        });
        app.selected_block = 1;

        assert_eq!(app.active_transcript_block(), Some(2));
        let spinner = animation::spinner(app.frame);
        let rendered = render_to_string(&mut app, 100, 24);
        assert!(rendered.contains("✓ Read old.rs"));
        assert!(rendered.contains(&format!("{spinner} Read current.rs")));
        assert!(!rendered.contains(&format!("{spinner} Read old.rs")));
    }

    #[test]
    fn key_events_have_stable_rhai_names() {
        assert_eq!(
            key_name(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL)),
            "ctrl+e"
        );
        assert_eq!(
            key_name(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
            "shift+enter"
        );
        assert_eq!(
            key_name(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::SHIFT)),
            ":"
        );
        assert_eq!(
            key_name(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT)),
            "?"
        );
        assert_eq!(
            key_name(KeyEvent::new(KeyCode::Char('：'), KeyModifiers::empty())),
            ":"
        );
        assert_eq!(
            key_name(KeyEvent::new(KeyCode::Char('？'), KeyModifiers::SHIFT)),
            "?"
        );
        assert_eq!(
            key_name(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT)),
            "shift+g"
        );
        assert_eq!(
            key_name(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::SUPER)),
            "super+c"
        );
    }

    #[test]
    fn cell_selection_preserves_lines_and_trims_padding() {
        let surface = SelectableSurface {
            area: Rect::new(10, 5, 5, 2),
            cells: vec![
                ["a", "b", "c", " ", " "]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
                ["d", "e", "f", " ", " "]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            ],
        };
        let selection = TextSelection {
            start: (11, 5),
            end: (12, 6),
        };
        assert_eq!(selected_surface_text(&surface, selection), "bc\ndef");
        assert_eq!(complete_surface_text(&surface), "abc\ndef");
    }
}
