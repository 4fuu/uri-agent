mod animation;
mod composer;
mod controller;
mod markdown;
mod model_selector;
mod rate;
mod render;
#[cfg(test)]
mod tests;

use crate::catalog::{CatalogModel, CatalogRefreshReport, ModelCatalog, ThinkingLevel};
use crate::clipboard;
use crate::compaction::ContextAccuracy;
use crate::config::{
    ActiveSettings, AgentEnvironment, AuthKind, ConfigManager, display_path,
    validate_environment_name, validate_model_role_name,
};
use crate::keymap::{KeyDisplayStyle, KeyStroke, Keymap};
use crate::model::{clamp_thinking_level, configured_backend};
use crate::oauth::{self, OauthLogin, OauthProvider, OauthToken};
use crate::output::OutputStore;
use crate::plugin::{
    CommandRegistry, CommandSpec, CommandTarget, CoreCommand, TuiCompletionContext, TuiCompletions,
    TuiDocument, TuiEffect, TuiPanelContext, TuiRegistry, TuiStatusContext, TuiStatusItem,
    TuiStatusTone, TuiSubmissionContext, TuiTextPosition, TuiTextRange,
};
use crate::protocol::{ProtocolDescriptor, ProtocolRegistry};
use crate::runtime::{AgentRuntime, ImageAttachment, PendingMessage, PendingMessageKind};
use crate::session::{EventKind, SessionEvent, SessionSummary, SessionUpdate};
use crate::task::{TaskManager, TaskRecord};
use crate::terminal::EmbeddedTerminal;
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use composer::*;
use controller::*;
pub use controller::{TuiServices, TuiTerminal};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    MouseButton, MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::SetTitle;
use model_selector::{ModelSelector, context_label, model_label, reasoning};
use portable_pty::CommandBuilder;
use ratatui::buffer::CellWidth;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Scrollbar,
    ScrollbarOrientation, ScrollbarState, Wrap,
};
use ratatui::{DefaultTerminal, Frame};
use rate::*;
use render::*;
use std::collections::{BTreeMap, HashSet};
use std::io::{Write, stdout};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tokio::time;
use tui_term::widget::PseudoTerminal;
use tui_textarea::{CursorMove, TextArea, WrapMode};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const BG: Color = Color::Reset;
const SURFACE: Color = Color::Rgb(21, 24, 28);
const ROW_ACTIVE: Color = Color::Rgb(25, 30, 35);
const USER_SURFACE: Color = Color::Rgb(23, 48, 45);
const TEXT: Color = Color::Rgb(218, 223, 229);
const MUTED: Color = Color::Rgb(116, 124, 135);
const SCROLLBAR: Color = Color::Rgb(142, 150, 160);
const ACCENT: Color = Color::Rgb(104, 210, 194);
const WARM: Color = Color::Rgb(239, 173, 104);
const ERROR: Color = Color::Rgb(239, 108, 120);
const PURPLE: Color = Color::Rgb(190, 130, 255);
const FLASH_MIN_DURATION: Duration = Duration::from_secs(3);
const FLASH_MAX_DURATION: Duration = Duration::from_secs(15);
const FLASH_MILLIS_PER_CHARACTER: u64 = 50;
const SPLASH_DURATION: Duration = Duration::from_millis(1200);
const COMPLETION_DEBOUNCE: Duration = Duration::from_millis(60);
const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(500);
const SMOOTH_SCROLL_FRAME_DURATION: Duration = Duration::from_millis(16);
const SCROLL_ROWS: isize = 6;
const EXPANDED_PREVIEW_LINES: usize = 24;
const TAIL_BUTTON_LABEL: &str = " ↓ bottom ";
const FLOATING_TAIL_BUTTON_LABEL: &str = " ↓ ";
const TAIL_BUTTON_RIGHT_INSET: usize = 2;
const WEB_SEARCH_LOGIN_PROVIDERS: &[&str] = &["parallel", "exa"];
const IMAGE_TOKEN_PREFIX: &str = "[Image #";
const IMAGE_MARKER_PREFIX: &str = "[Image #";

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
    pub context_accuracy: ContextAccuracy,
    pub compaction_enabled: bool,
    pub diagnostics_path: PathBuf,
    pub terminal: Option<String>,
    pub key_display: KeyDisplayStyle,
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
    Process,
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
    protocol_help_required: bool,
    expanded: bool,
    tool: Option<ToolDisplay>,
    transient: bool,
    turn_result: bool,
    parent_process: Option<u64>,
    process: Option<ProcessDisplay>,
}

struct ToolDisplay {
    name: String,
    arguments: serde_json::Value,
    output: Option<String>,
}

struct ProcessDisplay {
    id: u64,
    steps: usize,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Overlay {
    Composer,
    Delivery,
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
    TranscriptTail,
    Completion(usize),
    Delivery(usize),
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
    Retrying {
        attempt: usize,
        max_retries: usize,
        delay_ms: u64,
    },
    Compacting,
    Interrupting,
}

impl Activity {
    fn label(&self) -> String {
        match self {
            Self::Thinking => "thinking".to_string(),
            Self::Reasoning => "reasoning".to_string(),
            Self::Writing => "writing".to_string(),
            Self::Tool(protocol) => format!("running {protocol}"),
            Self::Retrying {
                attempt,
                max_retries,
                delay_ms,
            } => format!(
                "retrying {attempt}/{max_retries} in {}",
                retry_delay_label(*delay_ms)
            ),
            Self::Compacting => "compacting".to_string(),
            Self::Interrupting => "interrupting".to_string(),
        }
    }
}

struct FlashNotice {
    message: String,
    created: Instant,
}

impl FlashNotice {
    fn visible_at(&self, now: Instant) -> bool {
        now.duration_since(self.created) < flash_duration(&self.message)
    }
}

fn retry_delay_label(delay_ms: u64) -> String {
    if delay_ms < 1_000 {
        format!("{delay_ms}ms")
    } else if delay_ms.is_multiple_of(1_000) {
        format!("{}s", delay_ms / 1_000)
    } else {
        format!("{:.1}s", delay_ms as f64 / 1_000.0)
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
    row_separators: Vec<TextRowSeparator>,
    left_padding: usize,
    scroll_origin: usize,
    overlay: Option<Overlay>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TextRowSeparator {
    None,
    Space,
    Newline,
}

#[derive(Clone, Copy)]
struct TextSelection {
    start: (u16, u16),
    end: (u16, u16),
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum TextClickTarget {
    Surface(Option<Overlay>, (u16, u16)),
    Composer((u16, u16)),
}

#[derive(Clone, Copy)]
struct TranscriptScrollbarDrag {
    row: u16,
    offset: usize,
}

#[derive(Clone, Copy)]
enum MouseScrollAnimation {
    Transcript { target: usize, direction: isize },
    Overlay { target: u16, direction: isize },
}

#[derive(Clone)]
struct ComposerView {
    inner: Rect,
    top: usize,
    rows: Vec<ComposerVisualRow>,
}

#[derive(Clone, Copy)]
struct ComposerVisualRow {
    logical_row: usize,
    start_col: usize,
    end_col: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ImageTokenSpan {
    id: u64,
    start_byte: usize,
    end_byte: usize,
    id_end_byte: usize,
    start_col: usize,
    end_col: usize,
}

#[derive(Clone, Copy)]
enum CursorSnap {
    Backward,
    Forward,
    Nearest,
}

#[derive(Clone)]
struct CommandMatch {
    spec: CommandSpec,
    name: String,
}

struct SettingsState {
    active: ActiveSettings,
    model: Option<CatalogModel>,
    environment_count: usize,
    selected: usize,
    editing: Option<EditingSetting>,
    api_key: String,
    api_key_changed: bool,
    thinking: ThinkingLevel,
    output_limit: String,
}

impl SettingsState {
    async fn load(
        active: ActiveSettings,
        catalog: &ModelCatalog,
        environment: &AgentEnvironment,
    ) -> Self {
        let model = active.catalog_model(catalog).await;
        Self {
            output_limit: active.output_limit.to_string(),
            thinking: active.thinking,
            environment_count: environment.names().await.len(),
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
    LoginMethod {
        provider: String,
    },
    Logout,
    Resume,
    Search,
    Effort {
        provider: String,
        model: String,
    },
    ModelRoleEffort {
        role: String,
        provider: String,
        model: String,
    },
    Environment {
        return_to_settings: bool,
    },
    ModelRoles,
    PluginModelRole {
        plugin: String,
        key: String,
    },
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
enum ModelSelectionTarget {
    #[default]
    Conversation,
    Role(String),
}

struct SelectorState {
    kind: SelectorKind,
    title: String,
    query: String,
    items: Vec<SelectorItem>,
    visible: Vec<usize>,
    selected: usize,
}

struct DeliveryState {
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

    fn page_selection(&mut self, distance: isize) {
        self.selected = bounded_index(self.selected, distance, self.visible.len());
    }

    fn select_from_click(&mut self, position: usize, double_click: bool) -> bool {
        self.selected = position;
        double_click || matches!(&self.kind, SelectorKind::Search)
    }
}

enum TextPurpose {
    ApiKey {
        provider: String,
    },
    CopilotDomain,
    EnvironmentName {
        return_to_settings: bool,
    },
    EnvironmentValue {
        name: String,
        return_to_settings: bool,
    },
    ModelRoleName,
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

struct ComposerCompletions {
    result: TuiCompletions,
    selected: usize,
}

struct MarqueeState {
    key: String,
    started_at_frame: usize,
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
    protocol_source: Option<Arc<ProtocolRegistry>>,
    task_records: Vec<TaskRecord>,
    selected_task: usize,
    selected_block: usize,
    overlay: Option<Overlay>,
    delivery: Option<DeliveryState>,
    overlay_scroll: u16,
    overlay_viewport_rows: usize,
    jump: JumpKind,
    busy: bool,
    activity: Option<Activity>,
    busy_since: Option<Instant>,
    frame: usize,
    marquee: Option<MarqueeState>,
    transcript_body_width: usize,
    transcript_offset: usize,
    transcript_rows: usize,
    transcript_height: usize,
    transcript_follow_tail: bool,
    transcript_center_selected: bool,
    transcript_scrollbar_area: Option<Rect>,
    transcript_scrollbar_drag: Option<TranscriptScrollbarDrag>,
    mouse_scroll_animation: Option<MouseScrollAnimation>,
    started: Instant,
    splash_skipped: bool,
    last_sequence: Option<u64>,
    applying_transient: bool,
    reasoning_folded_during_stream: bool,
    next_process_id: u64,
    info: TuiInfo,
    flashes: Vec<FlashNotice>,
    model_selector: Option<ModelSelector>,
    model_selection_target: ModelSelectionTarget,
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
    last_text_click: Option<(TextClickTarget, Instant)>,
    mouse_word_selecting: bool,
    overlay_bounds: Option<Rect>,
    copy_click_release_pending: bool,
    selectable: Option<SelectableSurface>,
    selection: Option<TextSelection>,
    composer_view: Option<ComposerView>,
    composer_mouse_selecting: bool,
    completion_generation: u64,
    completion_task: Option<tokio::task::JoinHandle<()>>,
    completions: Option<ComposerCompletions>,
    composer_images: BTreeMap<u64, ImageAttachment>,
    next_composer_image_id: u64,
    clipboard_image_loading: bool,
    usage: UsageTotals,
    token_rate: TokenRateState,
    last_cache_hit: Option<f64>,
    branch: Option<(Instant, Option<String>)>,
    last_interrupt_press: Option<(String, Instant)>,
    pending_messages: Vec<PendingMessage>,
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
            input.insert_str(strip_image_references(&draft));
        }
        style_input(&mut input, false, &keymap);
        Self {
            input,
            blocks: Vec::new(),
            protocols,
            protocol_source: None,
            task_records: Vec::new(),
            selected_task: 0,
            selected_block: 0,
            overlay: None,
            delivery: None,
            overlay_scroll: 0,
            overlay_viewport_rows: 0,
            jump: JumpKind::All,
            busy: false,
            activity: None,
            busy_since: None,
            frame: 0,
            marquee: None,
            transcript_body_width: 72,
            transcript_offset: 0,
            transcript_rows: 0,
            transcript_height: 0,
            transcript_follow_tail: true,
            transcript_center_selected: false,
            transcript_scrollbar_area: None,
            transcript_scrollbar_drag: None,
            mouse_scroll_animation: None,
            started: Instant::now(),
            splash_skipped: !show_splash,
            last_sequence: None,
            applying_transient: false,
            reasoning_folded_during_stream: false,
            next_process_id: 0,
            info,
            flashes: Vec::new(),
            model_selector: None,
            model_selection_target: ModelSelectionTarget::Conversation,
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
            last_text_click: None,
            mouse_word_selecting: false,
            overlay_bounds: None,
            copy_click_release_pending: false,
            selectable: None,
            selection: None,
            composer_view: None,
            composer_mouse_selecting: false,
            completion_generation: 0,
            completion_task: None,
            completions: None,
            composer_images: BTreeMap::new(),
            next_composer_image_id: 1,
            clipboard_image_loading: false,
            usage: UsageTotals::default(),
            token_rate: TokenRateState::default(),
            last_cache_hit: None,
            branch: None,
            last_interrupt_press: None,
            pending_messages: Vec::new(),
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
        matches!(self.overlay, Some(Overlay::Delivery))
            || (self.overlay == Some(Overlay::Composer) && self.completions.is_none())
    }

    fn marquee_elapsed(&mut self, key: String) -> usize {
        let frame = self.frame;
        let marquee = self.marquee.get_or_insert_with(|| MarqueeState {
            key: key.clone(),
            started_at_frame: frame,
        });
        if marquee.key != key {
            marquee.key = key;
            marquee.started_at_frame = frame;
        }
        frame.wrapping_sub(marquee.started_at_frame)
    }

    fn interrupt_on_double_press(&mut self, key: KeyEvent, key_name: &str) -> bool {
        if key.kind != KeyEventKind::Press {
            return false;
        }
        if !self.busy
            || matches!(self.activity, Some(Activity::Compacting))
            || self.keymap.action("global", key_name).as_deref()
                != Some("interrupt_on_double_press")
        {
            self.last_interrupt_press = None;
            return false;
        }
        let now = Instant::now();
        let repeated = self
            .last_interrupt_press
            .as_ref()
            .is_some_and(|(previous, at)| {
                previous == key_name && now.duration_since(*at) < DOUBLE_CLICK_INTERVAL
            });
        self.last_interrupt_press = (!repeated).then_some((key_name.to_string(), now));
        repeated
    }

    fn apply(&mut self, event: SessionEvent) {
        let settles_model_response = matches!(
            &event.kind,
            EventKind::ModelMessage { .. }
                | EventKind::ModelRetry { .. }
                | EventKind::Error { .. }
                | EventKind::TurnFinished
        );
        if !self.applying_transient
            && matches!(
                &event.kind,
                EventKind::AssistantText { .. }
                    | EventKind::AssistantReasoning { .. }
                    | EventKind::ToolCall { .. }
                    | EventKind::ModelMessage { .. }
                    | EventKind::ModelRetry { .. }
                    | EventKind::Compaction { .. }
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
                | EventKind::ModelRetry { .. }
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
            | EventKind::Task { .. } => {}
            EventKind::ModelSettingsChanged { .. } => self.token_rate.clear_final(),
            EventKind::ModelMessage { message } => {
                if matches!(message, rig::message::Message::Assistant { .. }) {
                    self.token_rate.finish_response(Instant::now());
                }
            }
            EventKind::User { text } => {
                self.token_rate.ensure_turn();
                self.reasoning_folded_during_stream = false;
                self.busy = true;
                self.busy_since.get_or_insert_with(Instant::now);
                self.activity = Some(Activity::Thinking);
                self.push(BlockKind::User, "YOU", text, None, false, false);
            }
            EventKind::AssistantText { text } => {
                if self.applying_transient {
                    self.token_rate
                        .observe_stream_text(&text, false, Instant::now());
                } else {
                    self.token_rate.observe_response_text(&text, false);
                }
                self.activity = Some(Activity::Writing);
                self.append_or_push(BlockKind::Assistant, "AGENT", text, true);
            }
            EventKind::AssistantReasoning { text } => {
                if self.applying_transient {
                    self.token_rate
                        .observe_stream_text(&text, true, Instant::now());
                } else {
                    self.token_rate.observe_response_text(&text, true);
                }
                self.activity = Some(Activity::Reasoning);
                self.append_or_push(
                    BlockKind::Reasoning,
                    "THINKING",
                    text,
                    !self.reasoning_folded_during_stream,
                );
            }
            EventKind::AssistantToolCallDelta { text } => {
                if self.applying_transient {
                    self.token_rate
                        .observe_stream_text(&text, false, Instant::now());
                }
            }
            EventKind::ToolCall {
                call_id,
                name,
                arguments,
            } => {
                self.token_rate.observe_response_text(&name, false);
                self.token_rate
                    .observe_response_text(&arguments.to_string(), false);
                let protocol = tool_protocol(&arguments).unwrap_or_else(|| name.clone());
                self.activity = Some(Activity::Tool(protocol));
                let title = tool_title(&name, &arguments);
                self.push(
                    BlockKind::Tool,
                    &title,
                    String::new(),
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
                protocol_help_required,
            } => {
                if let Some(block) = self
                    .blocks
                    .iter_mut()
                    .rev()
                    .find(|block| block.call_id.as_deref() == Some(&call_id))
                {
                    block.failed = failed;
                    block.protocol_help_required = protocol_help_required;
                    if let Some(tool) = block.tool.as_mut() {
                        tool.output = Some(output.clone());
                    }
                } else {
                    let tool_output = output.clone();
                    self.push(
                        if protocol_help_required {
                            BlockKind::Tool
                        } else if failed {
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
                    let block = self.blocks.last_mut().unwrap();
                    block.protocol_help_required = protocol_help_required;
                    block.tool = Some(ToolDisplay {
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
            EventKind::ModelRetry {
                attempt,
                max_retries,
                delay_ms,
                reason,
            } => {
                self.token_rate.retry_response();
                self.activity = Some(Activity::Retrying {
                    attempt,
                    max_retries,
                    delay_ms,
                });
                self.push(
                    BlockKind::Notice,
                    "MODEL RETRY",
                    format!(
                        "{reason}; retry {attempt}/{max_retries} in {}",
                        retry_delay_label(delay_ms)
                    ),
                    None,
                    false,
                    false,
                );
            }
            EventKind::Usage {
                input,
                output,
                reasoning,
                cache_read,
                cache_write,
                cost,
                context,
                ..
            } => {
                self.usage.input += input;
                self.usage.output += output;
                self.usage.cache_read += cache_read;
                self.usage.cache_write += cache_write;
                self.usage.cost += cost;
                let prompt_tokens = input + cache_read + cache_write;
                self.last_cache_hit =
                    (prompt_tokens > 0).then(|| cache_read as f64 / prompt_tokens as f64 * 100.0);
                if context {
                    self.token_rate
                        .observe_usage(output, reasoning, Instant::now());
                }
            }
            EventKind::Error { text } => {
                self.token_rate.fail_turn();
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
                self.token_rate.retry_response();
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
                    format!("Context before compaction: {tokens_before} tokens\n\n{summary}"),
                    None,
                    false,
                    false,
                );
            }
            EventKind::TurnFinished => {
                self.token_rate.finish_turn();
                self.busy = false;
                self.activity = None;
                self.busy_since = None;
                self.finish_current_turn();
            }
        }
        if select_tail {
            self.selected_block = self
                .blocks
                .iter()
                .enumerate()
                .rev()
                .find_map(|(index, _)| self.block_visible(index).then_some(index))
                .unwrap_or_default();
        }
        self.sync_composer_chrome();
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

    fn finish_current_turn(&mut self) {
        let turn_start = self
            .blocks
            .iter()
            .rposition(|block| block.kind == BlockKind::User)
            .map_or(0, |index| index + 1);
        let result_index = self.blocks[turn_start..]
            .iter()
            .rposition(|block| matches!(block.kind, BlockKind::Assistant | BlockKind::Error))
            .map(|index| turn_start + index);
        let selected_was_in_turn = self.selected_block >= turn_start;
        let selected_was_result = result_index == Some(self.selected_block);
        let result = result_index.map(|index| {
            let mut result = self.blocks.remove(index);
            result.expanded = true;
            result.turn_result = true;
            result
        });
        let process_end = self.blocks.len();
        if process_end == turn_start {
            if let Some(result) = result {
                self.blocks.push(result);
            }
            if selected_was_in_turn {
                self.selected_block = turn_start;
            }
            return;
        }

        let process_id = self.next_process_id;
        self.next_process_id += 1;
        for block in &mut self.blocks[turn_start..process_end] {
            block.parent_process = Some(process_id);
            if matches!(block.kind, BlockKind::Reasoning | BlockKind::Tool) {
                block.expanded = false;
            }
        }
        self.blocks.insert(
            turn_start,
            DisplayBlock {
                kind: BlockKind::Process,
                title: "PROCESS".to_string(),
                text: String::new(),
                call_id: None,
                failed: false,
                protocol_help_required: false,
                expanded: false,
                tool: None,
                transient: false,
                turn_result: false,
                parent_process: None,
                process: Some(ProcessDisplay {
                    id: process_id,
                    steps: process_end - turn_start,
                }),
            },
        );
        if let Some(result) = result {
            self.blocks.push(result);
        }
        if selected_was_in_turn {
            self.selected_block = if selected_was_result {
                self.blocks.len().saturating_sub(1)
            } else {
                turn_start
            };
        }
    }

    fn finish_hydration(&mut self) {
        self.token_rate.finish_hydration();
        self.busy = false;
        self.activity = None;
        self.busy_since = None;
        for block in &mut self.blocks {
            block.expanded = matches!(
                block.kind,
                BlockKind::Assistant | BlockKind::Notice | BlockKind::Error
            );
        }
        self.sync_composer_chrome();
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
            protocol_help_required: false,
            expanded,
            tool: None,
            transient: self.applying_transient,
            turn_result: false,
            parent_process: None,
            process: None,
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

    fn submit(&mut self) -> Option<(String, Vec<ImageAttachment>)> {
        if self.clipboard_image_loading {
            return None;
        }
        if self.busy {
            if matches!(self.activity, Some(Activity::Compacting)) {
                self.set_flash("Wait for compaction to finish");
                return None;
            }
            if self.draft_text().trim().is_empty() {
                return None;
            }
            self.dismiss_completions();
            self.delivery = Some(DeliveryState { selected: 0 });
            self.overlay = Some(Overlay::Delivery);
            return None;
        }
        let (text, images) = self.composer_submission();
        if text.trim().is_empty() {
            return None;
        }
        self.reset_composer_images(&text, &images);
        self.input = TextArea::default();
        self.sync_composer_chrome();
        self.composer_mouse_selecting = false;
        self.mouse_word_selecting = false;
        self.dismiss_completions();
        self.token_rate.start_turn();
        self.busy = true;
        self.busy_since = Some(Instant::now());
        self.activity = Some(Activity::Thinking);
        self.overlay = None;
        Some((text, images))
    }

    fn composer_submission(&self) -> (String, Vec<ImageAttachment>) {
        prepare_image_submission(&self.draft_text(), &self.composer_images)
    }

    fn reset_composer_images(&mut self, text: &str, images: &[ImageAttachment]) {
        self.composer_images = image_store_from_references(text, images, image_token_spans);
        self.next_composer_image_id = self
            .composer_images
            .keys()
            .next_back()
            .copied()
            .unwrap_or(0)
            .saturating_add(1);
    }

    /// Remove the images a submitted message referenced without discarding
    /// images pasted into the new draft while the turn was still starting.
    fn discard_submitted_images(&mut self, submitted_image_ids: &[u64]) {
        for id in submitted_image_ids {
            self.composer_images.remove(id);
        }
        if self.composer_images.is_empty() {
            self.next_composer_image_id = 1;
        }
    }

    fn insert_composer_text(&mut self, text: impl AsRef<str>) -> bool {
        expand_composer_selection_to_image_tokens(&mut self.input, &self.composer_images);
        let text = text.as_ref();
        let inserted = if text.contains('\r') {
            self.input
                .insert_str(text.replace("\r\n", "\n").replace('\r', "\n"))
        } else {
            self.input.insert_str(text)
        };
        self.sync_composer_chrome();
        inserted
    }

    fn insert_composer_newline(&mut self) {
        expand_composer_selection_to_image_tokens(&mut self.input, &self.composer_images);
        self.input.insert_newline();
        self.sync_composer_chrome();
    }

    fn insert_clipboard_image(&mut self, bytes: Vec<u8>) {
        let id = self.next_composer_image_id;
        self.next_composer_image_id = self.next_composer_image_id.saturating_add(1);
        let image = ImageAttachment::png(bytes);
        let label = image_token_label(id, image.dimensions());
        self.composer_images.insert(id, image);
        self.insert_composer_text(label);
    }

    fn finish_clipboard_image_read(&mut self, result: Result<Vec<u8>>) {
        self.clipboard_image_loading = false;
        match result {
            Ok(bytes) => self.insert_clipboard_image(bytes),
            Err(error) => self.set_flash(format!("Clipboard image failed: {error:#}")),
        }
        self.sync_composer_chrome();
    }

    fn finish_clipboard_read(&mut self, result: Result<clipboard::ClipboardContent>) -> bool {
        self.clipboard_image_loading = false;
        let mut text_inserted = false;
        match result {
            Ok(clipboard::ClipboardContent::Image(bytes)) => self.insert_clipboard_image(bytes),
            Ok(clipboard::ClipboardContent::Text(text)) => {
                if self.overlay == Some(Overlay::Composer) {
                    self.insert_composer_text(text);
                    text_inserted = true;
                }
            }
            Err(error) => self.set_flash(format!("Clipboard paste failed: {error:#}")),
        }
        self.sync_composer_chrome();
        text_inserted
    }

    fn sync_composer_chrome(&mut self) {
        style_input(&mut self.input, self.busy, &self.keymap);
        self.input.clear_custom_highlight();
        let highlights = self
            .input
            .lines()
            .iter()
            .enumerate()
            .flat_map(|(row, line)| {
                image_token_spans(line)
                    .into_iter()
                    .filter(|token| self.composer_images.contains_key(&token.id))
                    .map(move |token| (row, token.start_byte, token.end_byte))
            })
            .collect::<Vec<_>>();
        for (row, start, end) in highlights {
            self.input.custom_highlight(
                ((row, start), (row, end)),
                Style::default()
                    .fg(WARM)
                    .bg(ROW_ACTIVE)
                    .add_modifier(Modifier::BOLD),
                5,
            );
        }
    }

    fn clear_accepted_draft(&mut self) {
        self.input = TextArea::default();
        self.composer_images.clear();
        self.next_composer_image_id = 1;
        self.sync_composer_chrome();
        self.composer_mouse_selecting = false;
        self.mouse_word_selecting = false;
        self.dismiss_completions();
        self.delivery = None;
        self.overlay = None;
    }

    fn restore_to_draft(&mut self, text: &str) {
        let current = self.draft_text();
        let restored_text = collapse_image_markers(text, self.composer_images.len());
        let restored = [restored_text.as_str(), current.as_str()]
            .into_iter()
            .filter(|part| !part.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        self.input = TextArea::default();
        self.input.insert_str(restored);
        self.sync_composer_chrome();
        self.dismiss_completions();
        self.overlay = Some(Overlay::Composer);
    }

    fn restore_pending_to_draft(&mut self, text: &str, images: Vec<ImageAttachment>) {
        let queued_store = image_store_from_references(text, &images, image_marker_spans);
        let queued = ensure_image_markers(text, &queued_store);
        let queued = collapse_image_markers(&queued, queued_store.len());
        let (queued, mut queued_images) = prepare_composer_images(&queued, &queued_store);
        let (current, current_images) =
            prepare_composer_images(&self.draft_text(), &self.composer_images);
        let offset = queued_images.len() as u64;
        let current_ids = current_images
            .iter()
            .enumerate()
            .map(|(index, _)| (index as u64 + 1, index as u64 + 1 + offset))
            .collect::<BTreeMap<_, _>>();
        let current = rewrite_image_token_ids(&current, &current_ids);
        let restored = [queued.as_str(), current.as_str()]
            .into_iter()
            .filter(|part| !part.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        queued_images.extend(current_images);
        self.reset_composer_images(&restored, &queued_images);
        self.input = TextArea::default();
        self.input.insert_str(restored);
        self.sync_composer_chrome();
        self.dismiss_completions();
        self.overlay = Some(Overlay::Composer);
    }

    fn draft_text(&self) -> String {
        self.input.lines().join("\n")
    }

    fn edit_composer(&mut self, key: KeyEvent, action: Option<&str>) {
        let old_cursor = self.input.cursor();
        let movement = match action {
            Some("cursor_up") => Some(if self.input.cursor().0 == 0 {
                CursorMove::Head
            } else {
                CursorMove::Up
            }),
            Some("cursor_down") => Some(if self.input.cursor().0 + 1 == self.input.lines().len() {
                CursorMove::End
            } else {
                CursorMove::Down
            }),
            Some("first") => Some(CursorMove::Jump(0, 0)),
            Some("last") => Some(CursorMove::Jump(u16::MAX, u16::MAX)),
            Some("word_back") => Some(CursorMove::WordBack),
            Some("word_forward") => Some(CursorMove::WordForward),
            Some("delete_word") => {
                if composer_has_selection(&self.input) {
                    expand_composer_selection_to_image_tokens(
                        &mut self.input,
                        &self.composer_images,
                    );
                    self.input.delete_word();
                } else if !delete_adjacent_image_token(
                    &mut self.input,
                    &self.composer_images,
                    false,
                ) {
                    self.input.delete_word();
                }
                None
            }
            Some("delete_next_word") => {
                if composer_has_selection(&self.input) {
                    expand_composer_selection_to_image_tokens(
                        &mut self.input,
                        &self.composer_images,
                    );
                    self.input.delete_next_word();
                } else if !delete_adjacent_image_token(&mut self.input, &self.composer_images, true)
                {
                    self.input.delete_next_word();
                }
                None
            }
            Some("undo") => {
                self.input.undo();
                None
            }
            Some("redo") => {
                self.input.redo();
                None
            }
            _ => {
                let selected = composer_has_selection(&self.input);
                if selected && composer_key_edits(key) {
                    expand_composer_selection_to_image_tokens(
                        &mut self.input,
                        &self.composer_images,
                    );
                }
                let deleted_token = !selected
                    && match key.code {
                        KeyCode::Backspace => delete_adjacent_image_token(
                            &mut self.input,
                            &self.composer_images,
                            false,
                        ),
                        KeyCode::Delete => delete_adjacent_image_token(
                            &mut self.input,
                            &self.composer_images,
                            true,
                        ),
                        _ => false,
                    };
                if !deleted_token {
                    self.input.input(key);
                }
                None
            }
        };
        if let Some(movement) = movement {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                if !self.input.is_selecting() {
                    self.input.start_selection();
                }
            } else {
                self.input.cancel_selection();
            }
            self.input.move_cursor(movement);
        }
        let cursor = self.input.cursor();
        let direction = match key.code {
            KeyCode::Left => CursorSnap::Backward,
            KeyCode::Right => CursorSnap::Forward,
            _ if cursor < old_cursor => CursorSnap::Backward,
            _ if cursor > old_cursor => CursorSnap::Forward,
            _ => CursorSnap::Nearest,
        };
        snap_composer_cursor(&mut self.input, &self.composer_images, direction);
        self.sync_composer_chrome();
    }

    fn begin_completion_query(&mut self) -> (u64, TuiCompletionContext) {
        if let Some(task) = self.completion_task.take() {
            task.abort();
        }
        self.completion_generation = self.completion_generation.wrapping_add(1);
        self.completions = None;
        let (line, column) = self.input.cursor();
        (
            self.completion_generation,
            TuiCompletionContext {
                cwd: self.info.cwd.clone(),
                session_id: self.info.session_id.clone(),
                lines: self.input.lines().to_vec(),
                cursor: TuiTextPosition { line, column },
            },
        )
    }

    fn finish_completion_query(&mut self, generation: u64, result: Result<Option<TuiCompletions>>) {
        if generation != self.completion_generation || self.overlay != Some(Overlay::Composer) {
            return;
        }
        self.completion_task = None;
        match result {
            Ok(Some(result)) if !result.items.is_empty() => {
                self.completions = Some(ComposerCompletions {
                    result,
                    selected: 0,
                });
            }
            Ok(_) => self.completions = None,
            Err(error) => {
                self.completions = None;
                self.set_flash(format!("Composer completion failed: {error:#}"));
            }
        }
    }

    fn dismiss_completions(&mut self) {
        if let Some(task) = self.completion_task.take() {
            task.abort();
        }
        self.completion_generation = self.completion_generation.wrapping_add(1);
        self.completions = None;
    }

    fn move_completion(&mut self, amount: isize) {
        let Some(completions) = self.completions.as_mut() else {
            return;
        };
        completions.selected =
            wrapped_index(completions.selected, amount, completions.result.items.len());
    }

    fn select_completion(&mut self, index: usize) -> bool {
        let Some(completions) = self.completions.as_mut() else {
            return false;
        };
        if index >= completions.result.items.len() {
            return false;
        }
        completions.selected = index;
        self.accept_completion()
    }

    fn accept_completion(&mut self) -> bool {
        let Some(completions) = self.completions.as_ref() else {
            return false;
        };
        let Some(item) = completions.result.items.get(completions.selected) else {
            return false;
        };
        let replacement = completions.result.replacement;
        let insert_text = item.insert_text.clone();
        if !replace_composer_range(&mut self.input, replacement, &insert_text) {
            self.dismiss_completions();
            return false;
        }
        self.sync_composer_chrome();
        self.dismiss_completions();
        true
    }

    fn set_flash(&mut self, message: impl Into<String>) {
        let now = Instant::now();
        self.flashes.retain(|notice| notice.visible_at(now));
        self.flashes.push(FlashNotice {
            message: message.into(),
            created: now,
        });
    }

    fn visible_flashes(&self) -> impl DoubleEndedIterator<Item = &str> {
        let now = Instant::now();
        self.flashes
            .iter()
            .filter(move |notice| notice.visible_at(now))
            .map(|notice| notice.message.as_str())
    }

    fn prune_flashes(&mut self) {
        let now = Instant::now();
        self.flashes.retain(|notice| notice.visible_at(now));
    }

    fn filtered_indices(&self) -> Vec<usize> {
        let collapsed_processes = self.collapsed_processes();
        self.blocks
            .iter()
            .enumerate()
            .filter(|(_, block)| match self.jump {
                JumpKind::All => block
                    .parent_process
                    .is_none_or(|process| !collapsed_processes.contains(&process)),
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
        self.expand_parent_process(self.selected_block);
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
        self.expand_parent_process(index);
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
        self.expand_parent_process(self.selected_block);
        self.transcript_follow_tail = false;
        self.transcript_center_selected = true;
    }

    fn block_visible(&self, index: usize) -> bool {
        let Some(parent_id) = self
            .blocks
            .get(index)
            .and_then(|block| block.parent_process)
        else {
            return true;
        };
        self.blocks.iter().any(|block| {
            block
                .process
                .as_ref()
                .is_some_and(|process| process.id == parent_id && block.expanded)
        })
    }

    fn collapsed_processes(&self) -> HashSet<u64> {
        self.blocks
            .iter()
            .filter_map(|block| {
                block
                    .process
                    .as_ref()
                    .filter(|_| !block.expanded)
                    .map(|process| process.id)
            })
            .collect()
    }

    fn expand_parent_process(&mut self, index: usize) {
        let Some(parent_id) = self
            .blocks
            .get(index)
            .and_then(|block| block.parent_process)
        else {
            return;
        };
        if let Some(parent) = self.blocks.iter_mut().find(|block| {
            block
                .process
                .as_ref()
                .is_some_and(|process| process.id == parent_id)
        }) {
            parent.expanded = true;
        }
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
        let title = block.title.clone();
        let document = if let Some(process) = &block.process {
            let mut document = format!("# {title}\n");
            for child in self
                .blocks
                .iter()
                .filter(|child| child.parent_process == Some(process.id))
            {
                document.push('\n');
                document.push_str(&block_document_with_level(child, 2));
            }
            document
        } else {
            block_document(block)
        };
        self.document = Some((title, document));
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
            Some(Activity::Retrying { .. } | Activity::Compacting | Activity::Interrupting)
            | None => None,
        }
    }

    fn scroll_transcript(&mut self, distance: isize) {
        self.cancel_mouse_scroll_animation();
        self.transcript_offset =
            self.transcript_scroll_destination(self.transcript_offset, distance);
        let live_tail = transcript_live_tail(self.transcript_rows, self.transcript_height);
        self.transcript_follow_tail = self.transcript_offset == live_tail;
        self.transcript_center_selected = false;
    }

    fn transcript_scroll_destination(&self, start: usize, distance: isize) -> usize {
        let live_tail = transcript_live_tail(self.transcript_rows, self.transcript_height);
        let reading_end = transcript_reading_end(self.transcript_rows, self.transcript_height);
        let offset = if distance < 0 {
            start.saturating_sub(distance.unsigned_abs())
        } else {
            start.saturating_add(distance as usize).min(reading_end)
        };
        if (start < live_tail && offset > live_tail) || (start > live_tail && offset < live_tail) {
            live_tail
        } else {
            offset
        }
    }

    fn smooth_scroll_transcript(&mut self, direction: isize) {
        let start = match self.mouse_scroll_animation {
            Some(MouseScrollAnimation::Transcript {
                target,
                direction: previous_direction,
            }) if previous_direction == direction => target,
            _ => self.transcript_offset,
        };
        let target = self.transcript_scroll_destination(start, direction * SCROLL_ROWS);
        self.mouse_scroll_animation = (target != self.transcript_offset)
            .then_some(MouseScrollAnimation::Transcript { target, direction });
        if self.mouse_scroll_animation.is_some() {
            self.transcript_follow_tail = false;
            self.transcript_center_selected = false;
        }
    }

    fn smooth_scroll_overlay(&mut self, direction: isize) {
        let start = match self.mouse_scroll_animation {
            Some(MouseScrollAnimation::Overlay {
                target,
                direction: previous_direction,
            }) if previous_direction == direction => target,
            _ => self.overlay_scroll,
        };
        let distance = direction * SCROLL_ROWS;
        let target = if distance < 0 {
            start.saturating_sub(distance.unsigned_abs().min(u16::MAX as usize) as u16)
        } else {
            start.saturating_add((distance as usize).min(u16::MAX as usize) as u16)
        };
        self.mouse_scroll_animation = (target != self.overlay_scroll)
            .then_some(MouseScrollAnimation::Overlay { target, direction });
    }

    fn mouse_scroll_animating(&self) -> bool {
        self.mouse_scroll_animation.is_some()
    }

    fn advance_mouse_scroll_animation(&mut self) {
        let Some(animation) = self.mouse_scroll_animation.take() else {
            return;
        };
        match animation {
            MouseScrollAnimation::Transcript { target, direction } => {
                let reading_end =
                    transcript_reading_end(self.transcript_rows, self.transcript_height);
                let target = target.min(reading_end);
                self.transcript_offset = match self.transcript_offset.cmp(&target) {
                    std::cmp::Ordering::Less => self.transcript_offset.saturating_add(1),
                    std::cmp::Ordering::Greater => self.transcript_offset.saturating_sub(1),
                    std::cmp::Ordering::Equal => self.transcript_offset,
                };
                let finished = self.transcript_offset == target;
                let live_tail = transcript_live_tail(self.transcript_rows, self.transcript_height);
                self.transcript_follow_tail = finished && self.transcript_offset == live_tail;
                self.transcript_center_selected = false;
                if !finished {
                    self.mouse_scroll_animation =
                        Some(MouseScrollAnimation::Transcript { target, direction });
                }
            }
            MouseScrollAnimation::Overlay { target, direction } => {
                self.overlay_scroll = match self.overlay_scroll.cmp(&target) {
                    std::cmp::Ordering::Less => self.overlay_scroll.saturating_add(1),
                    std::cmp::Ordering::Greater => self.overlay_scroll.saturating_sub(1),
                    std::cmp::Ordering::Equal => self.overlay_scroll,
                };
                if self.overlay_scroll != target {
                    self.mouse_scroll_animation =
                        Some(MouseScrollAnimation::Overlay { target, direction });
                }
            }
        }
    }

    fn cancel_mouse_scroll_animation(&mut self) {
        self.mouse_scroll_animation = None;
    }

    fn set_transcript_scrollbar_offset(&mut self, offset: usize) {
        self.cancel_mouse_scroll_animation();
        let live_tail = transcript_live_tail(self.transcript_rows, self.transcript_height);
        let reading_end = transcript_reading_end(self.transcript_rows, self.transcript_height);
        self.transcript_offset = offset.min(reading_end);
        self.transcript_follow_tail = self.transcript_offset == live_tail;
        self.transcript_center_selected = false;
    }

    fn page_transcript(&mut self, direction: isize) {
        self.scroll_transcript(direction * self.transcript_height.max(1) as isize);
    }

    fn overlay_page_distance(&self, direction: isize) -> isize {
        direction * self.overlay_viewport_rows.max(1) as isize
    }

    fn page_overlay(&mut self, direction: isize) {
        let distance = self.overlay_page_distance(direction);
        if distance < 0 {
            self.overlay_scroll = self
                .overlay_scroll
                .saturating_sub(distance.unsigned_abs().min(u16::MAX as usize) as u16);
        } else {
            self.overlay_scroll = self
                .overlay_scroll
                .saturating_add((distance as usize).min(u16::MAX as usize) as u16);
        }
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

    fn page_command_selection(&mut self, distance: isize) {
        let count = self.matching_commands().len();
        self.command_selected = bounded_index(self.command_selected, distance, count);
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
        self.model_selection_target = ModelSelectionTarget::Conversation;
        self.tui_document = None;
        self.delivery = None;
        self.dismiss_completions();
        self.overlay = None;
        self.overlay_scroll = 0;
    }
}

fn block_search_text(block: &DisplayBlock) -> String {
    let Some(tool) = &block.tool else {
        return block.text.clone();
    };
    let mut arguments = redact_sensitive_arguments(&tool.arguments);
    if let Some(body) = arguments.get_mut("body")
        && let Some(body_text) = body.as_str()
        && let Ok(parsed) = serde_json::from_str(body_text)
    {
        *body = redact_sensitive_arguments(&parsed);
    }
    let mut text = serde_json::to_string(&arguments).unwrap_or_else(|_| arguments.to_string());
    if let Some(output) = &tool.output {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(output);
    }
    text
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

fn bounded_index(current: usize, distance: isize, count: usize) -> usize {
    if count == 0 {
        return 0;
    }
    let current = current.min(count - 1);
    if distance < 0 {
        current.saturating_sub(distance.unsigned_abs())
    } else {
        current.saturating_add(distance as usize).min(count - 1)
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

fn word_bounds_at(text: &str, character_index: usize) -> Option<(usize, usize)> {
    let byte_index = text.char_indices().nth(character_index)?.0;
    let (start_byte, word) = text
        .split_word_bound_indices()
        .find(|(start, word)| *start <= byte_index && byte_index < *start + word.len())?;
    if word.chars().all(char::is_whitespace) {
        return None;
    }
    let start = text[..start_byte].chars().count();
    Some((start, start + word.chars().count()))
}
