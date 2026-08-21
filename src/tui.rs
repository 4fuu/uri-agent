mod animation;
mod model_selector;

use self::model_selector::{ModelSelector, context_label, model_label, reasoning};
use crate::catalog::{CatalogModel, ModelCatalog};
use crate::config::{ActiveSettings, ConfigManager, ExternalMode};
use crate::keymap::Keymap;
use crate::model::configured_backend;
use crate::output::OutputStore;
use crate::plugin::{
    CommandRegistry, CommandTarget, CoreCommand, TuiDocument, TuiPanelContext, TuiRegistry,
};
use crate::protocol::ProtocolDescriptor;
use crate::runtime::AgentRuntime;
use crate::session::{EventKind, SessionEvent};
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
    Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap,
};
use ratatui::{DefaultTerminal, Frame};
use std::env;
use std::io::{Write, stdout};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::{fs, process::Command, sync::mpsc, time};
use tui_term::widget::PseudoTerminal;
use tui_textarea::TextArea;
use uuid::Uuid;

const BG: Color = Color::Rgb(13, 15, 18);
const SURFACE: Color = Color::Rgb(21, 24, 28);
const TEXT: Color = Color::Rgb(218, 223, 229);
const MUTED: Color = Color::Rgb(116, 124, 135);
const ACCENT: Color = Color::Rgb(104, 210, 194);
const WARM: Color = Color::Rgb(239, 173, 104);
const ERROR: Color = Color::Rgb(239, 108, 120);
const FLASH_DURATION: Duration = Duration::from_secs(5);

pub struct TuiInfo {
    pub cwd: PathBuf,
    pub provider: String,
    pub model: String,
    pub session_id: String,
    pub context_window: usize,
    pub model_ready: bool,
    pub editor: Option<String>,
    pub editor_mode: ExternalMode,
    pub picker: Option<String>,
    pub picker_mode: ExternalMode,
}

#[derive(Clone, Copy, Eq, PartialEq)]
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
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Overlay {
    Detail,
    Help,
    Protocols,
    Tasks,
    Models,
    Settings,
    Palette,
    Command,
    Plugin,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum EditingSetting {
    ApiKey,
    OutputLimit,
    Editor,
    Picker,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Mode {
    Browse,
    Insert,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AppHit {
    Transcript(usize),
    Composer,
    Palette(usize),
    Task(usize),
    Model(usize),
    Setting(usize),
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
            Self::Writing => "writing response".to_string(),
            Self::Tool(protocol) => format!("running {protocol}"),
            Self::Compacting => "compacting context".to_string(),
        }
    }
}

#[derive(Clone, Copy)]
struct HitRegion<T> {
    area: Rect,
    target: T,
}

struct SettingsState {
    active: ActiveSettings,
    model: Option<CatalogModel>,
    selected: usize,
    editing: Option<EditingSetting>,
    api_key: String,
    api_key_changed: bool,
    output_limit: String,
    editor: String,
    editor_mode: ExternalMode,
    picker: String,
    picker_mode: ExternalMode,
}

impl SettingsState {
    async fn load(manager: &ConfigManager, catalog: &ModelCatalog) -> Self {
        let active = manager.current().await;
        let model = active.catalog_model(catalog).await;
        let output_limit = active.output_limit.to_string();
        let editor = active.editor.clone().unwrap_or_default();
        let editor_mode = active.editor_mode;
        let picker = active.picker.clone().unwrap_or_default();
        let picker_mode = active.picker_mode;
        Self {
            active,
            model,
            selected: 0,
            editing: None,
            api_key: String::new(),
            api_key_changed: false,
            output_limit,
            editor,
            editor_mode,
            picker,
            picker_mode,
        }
    }

    fn provider(&self) -> &str {
        &self.active.provider
    }

    fn model(&self) -> Option<&CatalogModel> {
        self.model.as_ref()
    }

    fn cycle_external_mode(mode: &mut ExternalMode) {
        *mode = match mode {
            ExternalMode::Float => ExternalMode::Fullscreen,
            ExternalMode::Fullscreen => ExternalMode::Float,
        };
    }
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

enum ExternalPurpose {
    Editor { path: PathBuf, read_back: bool },
    Picker { input: PathBuf, result: PathBuf },
}

struct ExternalProcess {
    terminal: EmbeddedTerminal,
    purpose: ExternalPurpose,
    area: Rect,
    title: &'static str,
    last_escape: Option<Instant>,
}

struct App {
    input: TextArea<'static>,
    blocks: Vec<DisplayBlock>,
    protocols: Vec<ProtocolDescriptor>,
    task_records: Vec<TaskRecord>,
    selected_task: usize,
    selected_block: usize,
    mode: Mode,
    overlay: Option<Overlay>,
    overlay_scroll: u16,
    busy: bool,
    activity: Option<Activity>,
    busy_since: Option<Instant>,
    frame: usize,
    last_sequence: Option<u64>,
    info: TuiInfo,
    flash: Option<String>,
    flash_at: Option<Instant>,
    model_selector: Option<ModelSelector>,
    catalog_refreshing: bool,
    settings: Option<SettingsState>,
    keymap: Keymap,
    palette_selected: usize,
    command_line: String,
    hit_regions: Vec<HitRegion<AppHit>>,
    last_click: Option<(AppHit, Instant)>,
    selectable: Option<SelectableSurface>,
    selection: Option<TextSelection>,
    external: Option<ExternalProcess>,
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
    ) -> Self {
        let mut input = TextArea::default();
        style_input(&mut input, false);
        Self {
            input,
            blocks: Vec::new(),
            protocols,
            task_records: Vec::new(),
            selected_task: 0,
            selected_block: 0,
            mode: Mode::Browse,
            overlay: None,
            overlay_scroll: 0,
            busy: false,
            activity: None,
            busy_since: None,
            frame: 0,
            last_sequence: None,
            info,
            flash: None,
            flash_at: None,
            model_selector: None,
            catalog_refreshing: false,
            settings: None,
            keymap,
            palette_selected: 0,
            command_line: String::new(),
            hit_regions: Vec::new(),
            last_click: None,
            selectable: None,
            selection: None,
            external: None,
            commands,
            tui,
            tui_document: None,
        }
    }

    fn apply(&mut self, event: SessionEvent) {
        if self
            .last_sequence
            .is_some_and(|sequence| event.sequence <= sequence)
        {
            return;
        }
        self.last_sequence = Some(event.sequence);
        let follow =
            self.blocks.is_empty() || self.selected_block == self.blocks.len().saturating_sub(1);
        match event.kind {
            EventKind::SessionCreated { .. }
            | EventKind::SessionContext { .. }
            | EventKind::ModelMessage { .. }
            | EventKind::Task { .. } => {}
            EventKind::User { text } => {
                self.busy = true;
                self.busy_since.get_or_insert_with(Instant::now);
                self.activity = Some(Activity::Thinking);
                self.push(BlockKind::User, "YOU", text, None, false);
            }
            EventKind::AssistantText { text } => {
                self.activity = Some(Activity::Writing);
                self.append_or_push(BlockKind::Assistant, "AGENT", text);
            }
            EventKind::AssistantReasoning { text } => {
                self.activity = Some(Activity::Reasoning);
                self.append_or_push(BlockKind::Reasoning, "THINKING", text);
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
                self.push(
                    BlockKind::Tool,
                    &format!("TOOL · {name}"),
                    format!("CALL\n{text}"),
                    Some(call_id),
                    false,
                );
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
                    block.text.push_str(if failed {
                        "\n\nERROR\n"
                    } else {
                        "\n\nRESULT\n"
                    });
                    block.text.push_str(&output);
                } else {
                    self.push(
                        if failed {
                            BlockKind::Error
                        } else {
                            BlockKind::Tool
                        },
                        &format!("TOOL · {name}"),
                        output,
                        Some(call_id),
                        failed,
                    );
                }
                self.activity = Some(Activity::Thinking);
            }
            EventKind::Notice { text } => self.push(BlockKind::Notice, "SYSTEM", text, None, false),
            EventKind::Error { text } => {
                self.busy = false;
                self.activity = None;
                self.busy_since = None;
                self.push(BlockKind::Error, "ERROR", text, None, true);
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
                );
            }
            EventKind::TurnFinished => {
                self.busy = false;
                self.activity = None;
                self.busy_since = None;
            }
        }
        if follow {
            self.selected_block = self.blocks.len().saturating_sub(1);
        }
        style_input(&mut self.input, self.busy);
    }

    fn finish_hydration(&mut self) {
        self.busy = false;
        self.activity = None;
        self.busy_since = None;
        style_input(&mut self.input, false);
    }

    fn push(
        &mut self,
        kind: BlockKind,
        title: &str,
        text: String,
        call_id: Option<String>,
        failed: bool,
    ) {
        self.blocks.push(DisplayBlock {
            kind,
            title: title.to_string(),
            text,
            call_id,
            failed,
        });
    }

    fn append_or_push(&mut self, kind: BlockKind, title: &str, text: String) {
        if let Some(block) = self.blocks.last_mut().filter(|block| block.kind == kind) {
            block.text.push_str(&text);
        } else {
            self.push(kind, title, text, None, false);
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
        style_input(&mut self.input, true);
        self.busy = true;
        self.busy_since = Some(Instant::now());
        self.activity = Some(Activity::Thinking);
        self.mode = Mode::Browse;
        Some(text)
    }

    fn has_draft(&self) -> bool {
        self.input
            .lines()
            .iter()
            .any(|line| !line.trim().is_empty())
    }

    fn set_flash(&mut self, message: impl Into<String>) {
        self.flash = Some(message.into());
        self.flash_at = Some(Instant::now());
    }

    fn clear_flash(&mut self) {
        self.flash = None;
        self.flash_at = None;
    }

    fn visible_flash(&self) -> Option<&str> {
        self.flash.as_deref().filter(|_| {
            self.flash_at
                .is_some_and(|created| created.elapsed() < FLASH_DURATION)
        })
    }

    fn selected_block(&self) -> Option<&DisplayBlock> {
        self.blocks.get(self.selected_block)
    }

    fn move_selection(&mut self, distance: isize) {
        if distance < 0 {
            self.selected_block = self.selected_block.saturating_sub(distance.unsigned_abs());
        } else {
            self.selected_block = self
                .selected_block
                .saturating_add(distance as usize)
                .min(self.blocks.len().saturating_sub(1));
        }
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

fn is_double_click<T: Copy + Eq>(last_click: &mut Option<(T, Instant)>, target: T) -> bool {
    let now = Instant::now();
    let repeated = last_click.as_ref().is_some_and(|(previous, at)| {
        *previous == target && now.duration_since(*at) < Duration::from_millis(500)
    });
    *last_click = Some((target, now));
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
}

pub async fn run(services: TuiServices) -> Result<()> {
    let TuiServices {
        runtime,
        protocols,
        commands,
        tui,
        tasks,
        manager,
        catalog,
        output,
        info,
    } = services;
    let session = runtime.session().clone();
    let mut receiver = session.subscribe();
    let keymap = Keymap::load(Some(&info.cwd)).await?;
    let mut app = App::new(protocols, commands, tui, info, keymap);
    for event in session.snapshot().await {
        app.apply(event);
    }
    app.finish_hydration();

    let mut terminal = ratatui::try_init()?;
    let _restore = RestoreTerminal;
    execute!(stdout(), EnableMouseCapture, EnableBracketedPaste)?;
    let services = LoopServices {
        runtime,
        tasks,
        manager,
        catalog,
        output,
    };
    run_loop(&mut terminal, &mut app, services, &mut receiver).await
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
    receiver: &mut tokio::sync::broadcast::Receiver<SessionEvent>,
) -> Result<()> {
    let mut terminal_events = EventStream::new();
    let mut animation = time::interval(Duration::from_millis(90));
    let (background_tx, mut background_rx) = mpsc::unbounded_channel();
    loop {
        terminal.draw(|frame| render(frame, app))?;
        tokio::select! {
            _ = animation.tick() => app.frame = app.frame.wrapping_add(1),
            event = terminal_events.next() => {
                let Some(event) = event else { return Ok(()); };
                match event? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        if app.external.is_some() {
                            if handle_external_key(app, key)? {
                                cancel_external(app).await;
                            }
                            continue;
                        }
                        match handle_key(app, key, &services.tasks).await {
                            Action::Continue => {}
                            Action::Quit => return Ok(()),
                            Action::Submit(prompt) => {
                                let runtime = services.runtime.clone();
                                tokio::spawn(async move {
                                    let _ = runtime.run_turn(prompt).await;
                                });
                            }
                            Action::Compact => start_compaction(app, services.runtime.clone()),
                            Action::OpenModels(query) => {
                                open_models(app, &services.manager, &services.catalog, query).await;
                            }
                            Action::SelectModel => {
                                select_model(app, &services.runtime, &services.manager, &services.catalog, &services.output).await;
                            }
                            Action::OpenSettings => {
                                app.settings = Some(SettingsState::load(&services.manager, &services.catalog).await);
                                app.overlay = Some(Overlay::Settings);
                            }
                            Action::SaveSettings => {
                                save_settings(app, &services.runtime, &services.manager, &services.catalog, &services.output).await;
                            }
                            Action::RefreshCatalog => {
                                start_catalog_refresh(app, &services, background_tx.clone());
                            }
                            Action::ClearApiKey => {
                                clear_api_key(app, &services.runtime, &services.manager, &services.catalog, &services.output).await;
                            }
                            Action::OpenEditor { content, replace_input } => {
                                if app.info.editor_mode == ExternalMode::Fullscreen {
                                    drop(terminal_events);
                                    let result = open_editor(terminal, app.info.editor.as_deref(), &content, replace_input).await;
                                    terminal_events = EventStream::new();
                                    apply_editor_result(app, result);
                                } else if let Err(error) = open_embedded_editor(app, &content, replace_input, terminal.size()?.into()).await {
                                    app.set_flash(format!("Editor failed: {error:#}"));
                                }
                            }
                            Action::OpenPicker => {
                                if app.info.picker_mode == ExternalMode::Fullscreen {
                                    drop(terminal_events);
                                    let result = open_fullscreen_picker(terminal, app).await;
                                    terminal_events = EventStream::new();
                                    apply_picker_result(app, result);
                                } else if let Err(error) = open_embedded_picker(app, terminal.size()?.into()).await {
                                    app.set_flash(format!("Picker failed: {error:#}"));
                                }
                            }
                        }
                    }
                    Event::Paste(text) => {
                        if let Some(external) = app.external.as_mut() {
                            if let Err(error) = external.terminal.send_paste(&text) {
                                app.set_flash(format!("Terminal input failed: {error:#}"));
                            }
                        } else if let Some(settings) = app.settings.as_mut()
                            && let Some(editing) = settings.editing
                        {
                            match editing {
                                EditingSetting::ApiKey => {
                                    settings.api_key.push_str(text.trim());
                                    settings.api_key_changed = true;
                                }
                                EditingSetting::OutputLimit => settings
                                    .output_limit
                                    .extend(text.chars().filter(char::is_ascii_digit)),
                                EditingSetting::Editor => settings.editor.push_str(text.trim()),
                                EditingSetting::Picker => settings.picker.push_str(text.trim()),
                            }
                        } else if app.overlay == Some(Overlay::Models) {
                            if let Some(selector) = app.model_selector.as_mut() {
                                selector.paste(text.trim());
                            }
                        } else if app.overlay == Some(Overlay::Command) {
                            app.command_line.push_str(text.trim());
                        } else if app.overlay.is_none() && app.mode == Mode::Insert {
                            app.input.insert_str(text);
                        }
                    }
                    Event::Mouse(mouse) => {
                        if app.external.is_some() {
                            if let Err(error) = handle_external_mouse(app, mouse) {
                                app.set_flash(format!("Terminal mouse input failed: {error:#}"));
                            }
                            continue;
                        }
                        match handle_mouse(app, mouse, &services.tasks).await {
                            Action::Continue => {}
                            Action::Quit => return Ok(()),
                            Action::Submit(prompt) => {
                                let runtime = services.runtime.clone();
                                tokio::spawn(async move {
                                    let _ = runtime.run_turn(prompt).await;
                                });
                            }
                            Action::Compact => start_compaction(app, services.runtime.clone()),
                            Action::OpenModels(query) => {
                                open_models(app, &services.manager, &services.catalog, query).await;
                            }
                            Action::SelectModel => {
                                select_model(app, &services.runtime, &services.manager, &services.catalog, &services.output).await;
                            }
                            Action::OpenSettings => {
                                app.settings = Some(SettingsState::load(&services.manager, &services.catalog).await);
                                app.overlay = Some(Overlay::Settings);
                            }
                            Action::SaveSettings => {
                                save_settings(app, &services.runtime, &services.manager, &services.catalog, &services.output).await;
                            }
                            Action::RefreshCatalog => {
                                start_catalog_refresh(app, &services, background_tx.clone());
                            }
                            Action::ClearApiKey => {
                                clear_api_key(app, &services.runtime, &services.manager, &services.catalog, &services.output).await;
                            }
                            Action::OpenEditor { content, replace_input } => {
                                if app.info.editor_mode == ExternalMode::Fullscreen {
                                    drop(terminal_events);
                                    let result = open_editor(terminal, app.info.editor.as_deref(), &content, replace_input).await;
                                    terminal_events = EventStream::new();
                                    apply_editor_result(app, result);
                                } else if let Err(error) = open_embedded_editor(app, &content, replace_input, terminal.size()?.into()).await {
                                    app.set_flash(format!("Editor failed: {error:#}"));
                                }
                            }
                            Action::OpenPicker => {
                                if app.info.picker_mode == ExternalMode::Fullscreen {
                                    drop(terminal_events);
                                    let result = open_fullscreen_picker(terminal, app).await;
                                    terminal_events = EventStream::new();
                                    apply_picker_result(app, result);
                                } else if let Err(error) = open_embedded_picker(app, terminal.size()?.into()).await {
                                    app.set_flash(format!("Picker failed: {error:#}"));
                                }
                            }
                        }
                    }
                    Event::FocusGained | Event::FocusLost | Event::Resize(_, _) | Event::Key(_) => {}
                }
            }
            event = receiver.recv() => match event {
                Ok(event) => app.apply(event),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    for event in services.runtime.session().snapshot().await {
                        app.apply(event);
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
            },
            Some(event) = background_rx.recv() => {
                finish_background(app, &services, event).await;
            },
        }
        if external_finished(app)? {
            finish_external(app).await;
        }
    }
}

enum BackgroundEvent {
    CatalogRefreshed(Result<ActiveSettings>),
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
    ClearApiKey,
    OpenEditor {
        content: String,
        replace_input: bool,
    },
    OpenPicker,
}

async fn dispatch_ui_command(
    app: &mut App,
    target: CommandTarget,
    arguments: String,
    tasks: &TaskManager,
) -> Action {
    app.overlay = None;
    let command = match target {
        CommandTarget::Core(command) => command,
        CommandTarget::Panel(panel) => {
            let context = TuiPanelContext {
                cwd: app.info.cwd.clone(),
                session_id: app.info.session_id.clone(),
                arguments,
            };
            match app.tui.open_panel(&panel, context).await {
                Ok(document) => {
                    app.tui_document = Some(document);
                    app.overlay_scroll = 0;
                    app.overlay = Some(Overlay::Plugin);
                }
                Err(error) => {
                    app.set_flash(format!("Plugin panel failed: {error:#}"));
                }
            }
            return Action::Continue;
        }
    };
    match command {
        CoreCommand::Compose => {
            app.mode = Mode::Insert;
            app.clear_flash();
            Action::Continue
        }
        CoreCommand::Detail => {
            if app.selected_block().is_some() {
                app.overlay_scroll = 0;
                app.overlay = Some(Overlay::Detail);
            } else {
                app.set_flash("No event is selected");
            }
            Action::Continue
        }
        CoreCommand::Editor => {
            if let Some(block) = app.selected_block() {
                Action::OpenEditor {
                    content: block_document(block),
                    replace_input: false,
                }
            } else {
                app.set_flash("No event is selected");
                Action::Continue
            }
        }
        CoreCommand::Finder => Action::OpenPicker,
        CoreCommand::Copy => {
            copy_current_surface(app);
            Action::Continue
        }
        CoreCommand::Tasks => {
            app.task_records = tasks.list().await;
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
        CoreCommand::Models => Action::OpenModels(arguments),
        CoreCommand::Settings => Action::OpenSettings,
        CoreCommand::Compact => Action::Compact,
        CoreCommand::Help => {
            app.overlay_scroll = 0;
            app.overlay = Some(Overlay::Help);
            Action::Continue
        }
        CoreCommand::Quit => Action::Quit,
    }
}

async fn dispatch_core(app: &mut App, command: CoreCommand, tasks: &TaskManager) -> Action {
    dispatch_ui_command(app, CommandTarget::Core(command), String::new(), tasks).await
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

fn submit_draft(app: &mut App) -> Action {
    let Some(prompt) = app.submit() else {
        return Action::Continue;
    };
    let command = prompt.split_whitespace().next();
    if matches!(command, Some("/settings" | "/model" | "/login")) {
        app.busy = false;
        app.activity = None;
        app.busy_since = None;
        style_input(&mut app.input, false);
        if command == Some("/model") {
            let query = prompt
                .trim_start()
                .strip_prefix("/model")
                .unwrap_or_default()
                .trim()
                .to_string();
            return Action::OpenModels(query);
        }
        return Action::OpenSettings;
    }
    Action::Submit(prompt)
}

async fn handle_key(app: &mut App, key: KeyEvent, tasks: &TaskManager) -> Action {
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
        Some("quit") => return dispatch_core(app, CoreCommand::Quit, tasks).await,
        Some("help") => return dispatch_core(app, CoreCommand::Help, tasks).await,
        Some("settings") => return dispatch_core(app, CoreCommand::Settings, tasks).await,
        Some("model") => return dispatch_core(app, CoreCommand::Models, tasks).await,
        Some("protocols") => return dispatch_core(app, CoreCommand::Protocols, tasks).await,
        Some("tasks") => return dispatch_core(app, CoreCommand::Tasks, tasks).await,
        Some("copy") => return dispatch_core(app, CoreCommand::Copy, tasks).await,
        _ => {}
    }
    if let Some(overlay) = app.overlay {
        match overlay {
            Overlay::Palette => match app.keymap.action("palette", &key_name).as_deref() {
                Some("quit") => return Action::Quit,
                Some("close") => app.overlay = None,
                Some("previous") => app.palette_selected = app.palette_selected.saturating_sub(1),
                Some("next") => {
                    app.palette_selected = app
                        .palette_selected
                        .saturating_add(1)
                        .min(app.commands.list().len().saturating_sub(1));
                }
                Some("confirm") => {
                    if let Some(target) = app
                        .commands
                        .list()
                        .get(app.palette_selected)
                        .map(|command| command.target.clone())
                    {
                        return dispatch_ui_command(app, target, String::new(), tasks).await;
                    }
                }
                _ => {}
            },
            Overlay::Command => match app.keymap.action("command", &key_name).as_deref() {
                Some("quit") => return Action::Quit,
                Some("cancel") => {
                    app.command_line.clear();
                    app.overlay = None;
                }
                Some("backspace") => {
                    app.command_line.pop();
                }
                Some("confirm") => {
                    let entered = std::mem::take(&mut app.command_line);
                    app.overlay = None;
                    if let Some(command) = app.commands.resolve(&entered) {
                        return dispatch_ui_command(
                            app,
                            command.spec.target,
                            command.arguments,
                            tasks,
                        )
                        .await;
                    }
                    app.set_flash(format!(
                        "Unknown command :{} · press Space for commands",
                        entered.trim()
                    ));
                }
                _ => {
                    if let KeyCode::Char(character) = key.code
                        && !key.modifiers.intersects(
                            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                        )
                    {
                        app.command_line.push(character);
                    }
                }
            },
            Overlay::Tasks => match app
                .keymap
                .action_chain(&["tasks", "list"], &key_name)
                .as_deref()
            {
                Some("quit") => return Action::Quit,
                Some("close") => app.overlay = None,
                Some("previous") => app.selected_task = app.selected_task.saturating_sub(1),
                Some("next") => {
                    app.selected_task = app
                        .selected_task
                        .saturating_add(1)
                        .min(app.task_records.len().saturating_sub(1));
                }
                Some("editor") => {
                    if let Some(task) = app.task_records.get(app.selected_task) {
                        return Action::OpenEditor {
                            content: task_document(task),
                            replace_input: false,
                        };
                    }
                }
                Some("cancel") => {
                    if let Some(id) = app
                        .task_records
                        .get(app.selected_task)
                        .map(|task| task.id.clone())
                    {
                        let _ = tasks.cancel(&id).await;
                        app.task_records = tasks.list().await;
                    }
                }
                Some("page_up") => app.overlay_scroll = app.overlay_scroll.saturating_sub(8),
                Some("page_down") => app.overlay_scroll = app.overlay_scroll.saturating_add(8),
                _ => {}
            },
            Overlay::Models => {
                let Some(selector) = app.model_selector.as_mut() else {
                    app.overlay = None;
                    return Action::Continue;
                };
                match app.keymap.action("models", &key_name).as_deref() {
                    Some("quit") => return Action::Quit,
                    Some("close") => app.overlay = None,
                    Some("previous") => selector.move_selection(-1),
                    Some("next") => selector.move_selection(1),
                    Some("page_up") => selector.move_selection(-10),
                    Some("page_down") => selector.move_selection(10),
                    Some("first") => selector.first(),
                    Some("last") => selector.last(),
                    Some("confirm") => return Action::SelectModel,
                    Some("backspace") => selector.backspace(),
                    Some("refresh") => return Action::RefreshCatalog,
                    _ => {
                        if let KeyCode::Char(character) = key.code
                            && !key.modifiers.intersects(
                                KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                            )
                        {
                            selector.push(character);
                        }
                    }
                }
            }
            Overlay::Detail => match app.keymap.action("detail", &key_name).as_deref() {
                Some("quit") => return Action::Quit,
                Some("close") => app.overlay = None,
                Some("scroll_up") => app.overlay_scroll = app.overlay_scroll.saturating_sub(1),
                Some("scroll_down") => app.overlay_scroll = app.overlay_scroll.saturating_add(1),
                Some("editor") => {
                    if let Some(block) = app.selected_block() {
                        return Action::OpenEditor {
                            content: block_document(block),
                            replace_input: false,
                        };
                    }
                }
                Some("page_up") => app.overlay_scroll = app.overlay_scroll.saturating_sub(8),
                Some("page_down") => app.overlay_scroll = app.overlay_scroll.saturating_add(8),
                _ => {}
            },
            Overlay::Help | Overlay::Protocols | Overlay::Plugin => {
                match app.keymap.action("list", &key_name).as_deref() {
                    Some("quit") => return Action::Quit,
                    Some("close") => app.overlay = None,
                    Some("previous") => app.overlay_scroll = app.overlay_scroll.saturating_sub(1),
                    Some("next") => app.overlay_scroll = app.overlay_scroll.saturating_add(1),
                    Some("page_up") => app.overlay_scroll = app.overlay_scroll.saturating_sub(8),
                    Some("page_down") => app.overlay_scroll = app.overlay_scroll.saturating_add(8),
                    _ => {}
                }
            }
            Overlay::Settings => {
                let Some(settings) = app.settings.as_mut() else {
                    app.overlay = None;
                    return Action::Continue;
                };
                if let Some(editing) = settings.editing {
                    match app.keymap.action("text", &key_name).as_deref() {
                        Some("quit") => return Action::Quit,
                        Some("cancel") => {
                            settings.editing = None;
                        }
                        Some("confirm") => {
                            settings.editing = None;
                        }
                        Some("backspace") => match editing {
                            EditingSetting::ApiKey => {
                                settings.api_key.pop();
                                settings.api_key_changed = true;
                            }
                            EditingSetting::OutputLimit => {
                                settings.output_limit.pop();
                            }
                            EditingSetting::Editor => {
                                settings.editor.pop();
                            }
                            EditingSetting::Picker => {
                                settings.picker.pop();
                            }
                        },
                        _ => {
                            if let KeyCode::Char(character) = key.code
                                && !key.modifiers.intersects(
                                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                                )
                            {
                                match editing {
                                    EditingSetting::ApiKey => {
                                        settings.api_key.push(character);
                                        settings.api_key_changed = true;
                                    }
                                    EditingSetting::OutputLimit if character.is_ascii_digit() => {
                                        settings.output_limit.push(character);
                                    }
                                    EditingSetting::OutputLimit => {}
                                    EditingSetting::Editor => settings.editor.push(character),
                                    EditingSetting::Picker => settings.picker.push(character),
                                }
                            }
                        }
                    }
                    return Action::Continue;
                }
                match app.keymap.action("settings", &key_name).as_deref() {
                    Some("quit") => return Action::Quit,
                    Some("close") => app.overlay = None,
                    Some("previous") => settings.selected = settings.selected.saturating_sub(1),
                    Some("next") => settings.selected = (settings.selected + 1).min(7),
                    Some("left") if settings.selected == 0 => {
                        return Action::OpenModels(String::new());
                    }
                    Some("right") if settings.selected == 0 => {
                        return Action::OpenModels(String::new());
                    }
                    Some("left" | "right") if settings.selected == 1 => {
                        return Action::OpenModels(String::new());
                    }
                    Some("left" | "right") if settings.selected == 5 => {
                        SettingsState::cycle_external_mode(&mut settings.editor_mode);
                    }
                    Some("left" | "right") if settings.selected == 7 => {
                        SettingsState::cycle_external_mode(&mut settings.picker_mode);
                    }
                    Some("edit") => {
                        if matches!(settings.selected, 0 | 1) {
                            return Action::OpenModels(String::new());
                        }
                        if settings.selected == 5 {
                            SettingsState::cycle_external_mode(&mut settings.editor_mode);
                            return Action::Continue;
                        }
                        if settings.selected == 7 {
                            SettingsState::cycle_external_mode(&mut settings.picker_mode);
                            return Action::Continue;
                        }
                        settings.editing = Some(match settings.selected {
                            2 => EditingSetting::ApiKey,
                            3 => EditingSetting::OutputLimit,
                            4 => EditingSetting::Editor,
                            6 => EditingSetting::Picker,
                            _ => unreachable!(),
                        });
                        match settings.selected {
                            2 => settings.api_key.clear(),
                            3 => settings.output_limit.clear(),
                            4 => settings.editor.clear(),
                            6 => settings.picker.clear(),
                            _ => {}
                        }
                    }
                    Some("save") => return Action::SaveSettings,
                    Some("refresh") => return Action::RefreshCatalog,
                    Some("clear") if settings.selected == 2 => return Action::ClearApiKey,
                    _ => {}
                }
            }
        }
        return Action::Continue;
    }

    let mode = if app.mode == Mode::Insert {
        "insert"
    } else {
        "browse"
    };
    match app.keymap.action(mode, &key_name).as_deref() {
        Some("quit") => return Action::Quit,
        Some("help") => {
            app.overlay_scroll = 0;
            app.overlay = Some(Overlay::Help);
        }
        Some("settings") => return Action::OpenSettings,
        Some("protocols") => {
            app.overlay_scroll = 0;
            app.overlay = Some(Overlay::Protocols);
        }
        Some("tasks") => {
            return dispatch_core(app, CoreCommand::Tasks, tasks).await;
        }
        action if app.mode == Mode::Insert => match action {
            Some("browse") => {
                app.mode = Mode::Browse;
                if app.has_draft() {
                    app.set_flash("Draft kept · Enter submits from Browse");
                }
            }
            Some("newline") => app.input.insert_newline(),
            Some("editor") => {
                return Action::OpenEditor {
                    content: app.input.lines().join("\n"),
                    replace_input: true,
                };
            }
            Some("quit_empty") if app.input.lines().iter().all(|line| line.is_empty()) => {
                return Action::Quit;
            }
            Some("submit") => return submit_draft(app),
            _ => {
                app.clear_flash();
                app.input.input(key);
            }
        },
        Some("palette") => {
            app.palette_selected = 0;
            app.overlay = Some(Overlay::Palette);
        }
        Some("command") => {
            app.command_line.clear();
            app.overlay = Some(Overlay::Command);
        }
        Some("insert") => return dispatch_core(app, CoreCommand::Compose, tasks).await,
        Some("next") => app.move_selection(1),
        Some("previous") => app.move_selection(-1),
        Some("page_down") => app.move_selection(10),
        Some("page_up") => app.move_selection(-10),
        Some("first") => app.selected_block = 0,
        Some("last") => app.selected_block = app.blocks.len().saturating_sub(1),
        Some("submit") if app.has_draft() => return submit_draft(app),
        Some("submit") => return dispatch_core(app, CoreCommand::Detail, tasks).await,
        Some("detail") => return dispatch_core(app, CoreCommand::Detail, tasks).await,
        Some("editor") => return dispatch_core(app, CoreCommand::Editor, tasks).await,
        Some("finder") => return dispatch_core(app, CoreCommand::Finder, tasks).await,
        Some("copy") => return dispatch_core(app, CoreCommand::Copy, tasks).await,
        Some(action) => {
            if let Some(target) = app.commands.target_for_action(action) {
                return dispatch_ui_command(app, target, String::new(), tasks).await;
            }
        }
        None => {}
    }
    Action::Continue
}

async fn handle_mouse(app: &mut App, mouse: MouseEvent, tasks: &TaskManager) -> Action {
    let require_shift = !matches!(
        app.overlay,
        Some(Overlay::Detail | Overlay::Help | Overlay::Protocols | Overlay::Plugin)
    );
    if update_mouse_selection(app, mouse, require_shift) {
        return Action::Continue;
    }
    match mouse.kind {
        MouseEventKind::ScrollUp => match app.overlay {
            Some(Overlay::Palette) => {
                app.palette_selected = app.palette_selected.saturating_sub(1);
            }
            Some(Overlay::Tasks) => {
                app.selected_task = app.selected_task.saturating_sub(1);
            }
            Some(Overlay::Models) => {
                if let Some(selector) = app.model_selector.as_mut() {
                    selector.move_selection(-3);
                }
            }
            Some(Overlay::Settings) => {
                if let Some(settings) = app.settings.as_mut() {
                    settings.selected = settings.selected.saturating_sub(1);
                }
            }
            Some(Overlay::Command) => {}
            Some(_) => app.overlay_scroll = app.overlay_scroll.saturating_sub(3),
            None => app.move_selection(-3),
        },
        MouseEventKind::ScrollDown => match app.overlay {
            Some(Overlay::Palette) => {
                app.palette_selected = app
                    .palette_selected
                    .saturating_add(1)
                    .min(app.commands.list().len().saturating_sub(1));
            }
            Some(Overlay::Tasks) => {
                app.selected_task = app
                    .selected_task
                    .saturating_add(1)
                    .min(app.task_records.len().saturating_sub(1));
            }
            Some(Overlay::Models) => {
                if let Some(selector) = app.model_selector.as_mut() {
                    selector.move_selection(3);
                }
            }
            Some(Overlay::Settings) => {
                if let Some(settings) = app.settings.as_mut() {
                    settings.selected = settings.selected.saturating_add(1).min(7);
                }
            }
            Some(Overlay::Command) => {}
            Some(_) => app.overlay_scroll = app.overlay_scroll.saturating_add(3),
            None => app.move_selection(3),
        },
        MouseEventKind::Down(MouseButton::Left) => {
            let Some(target) = hit_target(&app.hit_regions, mouse) else {
                return Action::Continue;
            };
            let activate = is_double_click(&mut app.last_click, target);
            match target {
                AppHit::Transcript(index) => {
                    app.selected_block = index;
                    if activate {
                        return dispatch_core(app, CoreCommand::Detail, tasks).await;
                    }
                }
                AppHit::Composer => {
                    return dispatch_core(app, CoreCommand::Compose, tasks).await;
                }
                AppHit::Palette(index) => {
                    app.palette_selected = index;
                    if let Some(target) = app
                        .commands
                        .list()
                        .get(index)
                        .map(|command| command.target.clone())
                    {
                        return dispatch_ui_command(app, target, String::new(), tasks).await;
                    }
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
                        if activate && matches!(index, 0 | 1) {
                            return Action::OpenModels(String::new());
                        }
                    }
                }
            }
        }
        _ => {}
    }
    Action::Continue
}

async fn open_models(
    app: &mut App,
    manager: &ConfigManager,
    catalog: &ModelCatalog,
    query: String,
) {
    let active = manager.current().await;
    let selector = ModelSelector::load(catalog, &active, query).await;
    if selector.model_count() == 0 {
        app.set_flash(
            "No runnable models cached · refresh the Pi catalog or add models.json".to_string(),
        );
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
        Err(error) => {
            app.set_flash(format!("Could not select {requested}: {error:#}"));
        }
    }
}

fn key_name(key: KeyEvent) -> String {
    let mut modifiers = Vec::new();
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        modifiers.push("ctrl");
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        modifiers.push("alt");
    }
    if key.modifiers.contains(KeyModifiers::SHIFT) {
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
        KeyCode::Char(character) => character.to_lowercase().collect(),
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
    let editor = settings.editor.clone();
    let editor_mode = settings.editor_mode;
    let picker = settings.picker.clone();
    let picker_mode = settings.picker_mode;
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
        manager
            .set_external_tools(Some(editor), editor_mode, Some(picker), picker_mode)
            .await?;
        if let Some((provider, model)) = selection {
            manager.set_model(&provider, &model).await?;
        }
        let active = manager.current().await;
        apply_active(app, runtime, catalog, output, &active).await?;
        app.settings = Some(SettingsState::load(manager, catalog).await);
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
    app.set_flash("Refreshing the Pi model catalog in the background…");
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
                let active = result?;
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
                        Some(SettingsState::load(&services.manager, &services.catalog).await);
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
    }
}

async fn clear_api_key(
    app: &mut App,
    runtime: &AgentRuntime,
    manager: &ConfigManager,
    catalog: &ModelCatalog,
    output: &OutputStore,
) {
    let Some(provider) = app
        .settings
        .as_ref()
        .map(|settings| settings.provider().to_string())
    else {
        return;
    };
    let result = async {
        let active = manager.clear_api_key(&provider).await?;
        apply_active(app, runtime, catalog, output, &active).await?;
        app.settings = Some(SettingsState::load(manager, catalog).await);
        Ok::<_, anyhow::Error>(())
    }
    .await;
    app.set_flash(match result {
        Ok(()) => format!("Stored credential for {provider} cleared"),
        Err(error) => format!("Could not clear credential: {error:#}"),
    });
}

async fn apply_active(
    app: &mut App,
    runtime: &AgentRuntime,
    catalog: &ModelCatalog,
    output: &OutputStore,
    active: &ActiveSettings,
) -> Result<()> {
    let backend = configured_backend(active, catalog).await?;
    let model_ready = backend.is_some();
    let context_window = active
        .catalog_model(catalog)
        .await
        .map_or(128_000, |model| model.context_window());
    runtime.set_backend(backend, context_window).await;
    runtime
        .session()
        .update_model(&active.provider, &active.model)
        .await?;
    output.set_limit(active.output_limit);
    app.info.provider.clone_from(&active.provider);
    app.info.model.clone_from(&active.model);
    app.info.context_window = context_window;
    app.info.model_ready = model_ready;
    app.info.editor.clone_from(&active.editor);
    app.info.editor_mode = active.editor_mode;
    app.info.picker.clone_from(&active.picker);
    app.info.picker_mode = active.picker_mode;
    Ok(())
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

fn task_document(task: &TaskRecord) -> String {
    format!(
        "# Task {}\n\nProtocol: {}\nStatus: {}\nLabel: {}\n\n{}\n",
        task.id,
        task.protocol,
        task.status.as_str(),
        task.label,
        String::from_utf8_lossy(&task.content)
    )
}

async fn temporary_document(content: &str) -> Result<PathBuf> {
    let directory = env::temp_dir().join("uri-agent");
    fs::create_dir_all(&directory).await?;
    let path = directory.join(format!("view-{}.md", Uuid::now_v7().simple()));
    fs::write(&path, content).await?;
    Ok(path)
}

fn command_builder(command: &str, trailing: Option<&Path>) -> Result<CommandBuilder> {
    let mut arguments = shell_words::split(command).context("cannot parse command")?;
    if arguments.is_empty() {
        bail!("command is empty");
    }
    let executable = arguments.remove(0);
    if let Some(path) = trailing {
        arguments.push(path.to_string_lossy().into_owned());
    }
    let mut builder = CommandBuilder::new(executable);
    builder.args(arguments);
    Ok(builder)
}

async fn open_embedded_editor(
    app: &mut App,
    content: &str,
    read_back: bool,
    terminal_area: Rect,
) -> Result<()> {
    let editor = app
        .info
        .editor
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("configure Editor in Settings first"))?;
    let path = temporary_document(content).await?;
    let area = centered(terminal_area, 92, 88);
    let inner = area.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    let command = command_builder(editor, Some(&path))?;
    let terminal = match EmbeddedTerminal::start(command, &app.info.cwd, inner.height, inner.width)
    {
        Ok(terminal) => terminal,
        Err(error) => {
            let _ = fs::remove_file(&path).await;
            return Err(error).with_context(|| {
                if editor.split_whitespace().next() == Some("hx") {
                    "cannot start `hx`; install Helix or change Editor in Settings"
                } else {
                    "cannot start configured editor"
                }
            });
        }
    };
    app.overlay = None;
    app.selection = None;
    app.external = Some(ExternalProcess {
        terminal,
        purpose: ExternalPurpose::Editor { path, read_back },
        area,
        title: " EDITOR · double Esc close · Shift-drag select · Ctrl+Shift+C copy ",
        last_escape: None,
    });
    Ok(())
}

async fn picker_files(app: &App) -> Result<(PathBuf, PathBuf)> {
    if app.blocks.is_empty() {
        bail!("there are no conversation events to search");
    }
    let directory = env::temp_dir().join("uri-agent");
    fs::create_dir_all(&directory).await?;
    let id = Uuid::now_v7().simple();
    let input = directory.join(format!("picker-{id}.txt"));
    let result = directory.join(format!("picker-{id}.result"));
    let lines = app
        .blocks
        .iter()
        .enumerate()
        .map(|(index, block)| {
            format!(
                "{index}\t{}\t{}",
                block.title,
                single_line_preview(&block.text, 180)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&input, lines).await?;
    Ok((input, result))
}

fn picker_wrapper(command: &str, input: &Path, result: &Path) -> Result<Vec<String>> {
    let mut picker = shell_words::split(command).context("cannot parse picker command")?;
    if picker.is_empty() {
        bail!("picker command is empty");
    }
    #[cfg(unix)]
    {
        let mut arguments = vec![
            "sh".to_string(),
            "-c".to_string(),
            "input=$1; output=$2; shift 2; exec \"$@\" < \"$input\" > \"$output\"".to_string(),
            "uri-agent-picker".to_string(),
            input.to_string_lossy().into_owned(),
            result.to_string_lossy().into_owned(),
        ];
        arguments.append(&mut picker);
        Ok(arguments)
    }
    #[cfg(windows)]
    {
        let mut arguments = vec![
            "pwsh".to_string(),
            "-NoProfile".to_string(),
            "-Command".to_string(),
            "$inputPath=$args[0]; $outputPath=$args[1]; $exe=$args[2]; $rest=$args[3..($args.Length-1)]; Get-Content -LiteralPath $inputPath | & $exe @rest | Set-Content -NoNewline -LiteralPath $outputPath".to_string(),
            input.to_string_lossy().into_owned(),
            result.to_string_lossy().into_owned(),
        ];
        arguments.append(&mut picker);
        Ok(arguments)
    }
}

fn builder_from_arguments(mut arguments: Vec<String>) -> Result<CommandBuilder> {
    if arguments.is_empty() {
        bail!("command is empty");
    }
    let executable = arguments.remove(0);
    let mut builder = CommandBuilder::new(executable);
    builder.args(arguments);
    Ok(builder)
}

async fn open_embedded_picker(app: &mut App, terminal_area: Rect) -> Result<()> {
    let picker = app
        .info
        .picker
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("configure Picker in Settings first"))?;
    let (input, result) = picker_files(app).await?;
    let arguments = picker_wrapper(picker, &input, &result)?;
    let area = centered(terminal_area, 88, 76);
    let inner = area.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    let terminal = match EmbeddedTerminal::start(
        builder_from_arguments(arguments)?,
        &app.info.cwd,
        inner.height,
        inner.width,
    ) {
        Ok(terminal) => terminal,
        Err(error) => {
            let _ = fs::remove_file(&input).await;
            let _ = fs::remove_file(&result).await;
            return Err(error)
                .context("cannot start picker; install fzf or change Picker in Settings");
        }
    };
    app.overlay = None;
    app.selection = None;
    app.external = Some(ExternalProcess {
        terminal,
        purpose: ExternalPurpose::Picker { input, result },
        area,
        title: " FIND EVENT · Enter choose · Esc cancel · Shift-drag select ",
        last_escape: None,
    });
    Ok(())
}

async fn open_fullscreen_picker(
    terminal: &mut DefaultTerminal,
    app: &App,
) -> Result<Option<usize>> {
    let picker = app
        .info
        .picker
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("configure Picker in Settings first"))?;
    let (input, result) = picker_files(app).await?;
    let arguments = picker_wrapper(picker, &input, &result)?;
    execute!(stdout(), DisableMouseCapture, DisableBracketedPaste)?;
    ratatui::try_restore()?;
    let command_result = Command::new(&arguments[0])
        .args(&arguments[1..])
        .current_dir(&app.info.cwd)
        .status()
        .await;
    *terminal = ratatui::try_init()?;
    execute!(stdout(), EnableMouseCapture, EnableBracketedPaste)?;
    clear_terminal_screen()?;

    let selection = match command_result {
        Ok(_) => read_picker_result(&result).await,
        Err(error) => {
            Err(error).context("cannot start picker; install fzf or change Picker in Settings")
        }
    };
    let _ = fs::remove_file(input).await;
    let _ = fs::remove_file(result).await;
    selection
}

async fn read_picker_result(path: &Path) -> Result<Option<usize>> {
    let result = match fs::read_to_string(path).await {
        Ok(result) => result,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let Some(index) = result.split('\t').next().filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    Ok(Some(
        index.parse().context("picker returned an invalid event")?,
    ))
}

fn apply_editor_result(app: &mut App, result: Result<Option<String>>) {
    match result {
        Ok(Some(content)) => {
            app.input = TextArea::new(content.split('\n').map(str::to_owned).collect());
            style_input(&mut app.input, app.busy);
            app.mode = Mode::Insert;
            app.set_flash("Draft updated from editor");
        }
        Ok(None) => app.set_flash("Editor closed"),
        Err(error) => app.set_flash(format!("Editor failed: {error:#}")),
    }
}

fn apply_picker_result(app: &mut App, result: Result<Option<usize>>) {
    match result {
        Ok(Some(index)) if index < app.blocks.len() => {
            app.selected_block = index;
            app.overlay_scroll = 0;
            app.overlay = Some(Overlay::Detail);
            app.set_flash(format!("Selected event {}", index + 1));
        }
        Ok(Some(_)) => app.set_flash("Picker selected an unknown event"),
        Ok(None) => app.set_flash("Picker closed"),
        Err(error) => app.set_flash(format!("Picker failed: {error:#}")),
    }
}

fn external_finished(app: &mut App) -> Result<bool> {
    app.external
        .as_mut()
        .map(|external| external.terminal.try_wait().map(|status| status.is_some()))
        .transpose()
        .map(Option::unwrap_or_default)
}

async fn finish_external(app: &mut App) {
    let Some(external) = app.external.take() else {
        return;
    };
    let ExternalProcess {
        terminal, purpose, ..
    } = external;
    drop(terminal);
    app.selection = None;
    match purpose {
        ExternalPurpose::Editor { path, read_back } => {
            let result = if read_back {
                fs::read_to_string(&path)
                    .await
                    .map(Some)
                    .map_err(Into::into)
            } else {
                Ok(None)
            };
            let _ = fs::remove_file(path).await;
            apply_editor_result(app, result);
        }
        ExternalPurpose::Picker { input, result } => {
            let selection = read_picker_result(&result).await;
            let _ = fs::remove_file(input).await;
            let _ = fs::remove_file(result).await;
            apply_picker_result(app, selection);
        }
    }
}

async fn cancel_external(app: &mut App) {
    let Some(external) = app.external.take() else {
        return;
    };
    let ExternalProcess {
        terminal, purpose, ..
    } = external;
    drop(terminal);
    app.selection = None;
    match purpose {
        ExternalPurpose::Editor { path, .. } => {
            let _ = fs::remove_file(path).await;
        }
        ExternalPurpose::Picker { input, result } => {
            let _ = fs::remove_file(input).await;
            let _ = fs::remove_file(result).await;
        }
    }
    app.set_flash("Embedded terminal closed");
}

fn handle_external_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    if app.selection.is_some() {
        match app.keymap.action("selection", &key_name(key)).as_deref() {
            Some("copy") => copy_current_surface(app),
            Some("close") => app.selection = None,
            _ => {}
        }
        return Ok(false);
    }
    match app.keymap.action("terminal", &key_name(key)).as_deref() {
        Some("copy") => {
            copy_current_surface(app);
            return Ok(false);
        }
        Some("close") => return Ok(true),
        Some("escape") => {
            let now = Instant::now();
            let external = app.external.as_mut().expect("checked by caller");
            if external
                .last_escape
                .is_some_and(|at| now.duration_since(at) < Duration::from_millis(500))
            {
                return Ok(true);
            }
            external.last_escape = Some(now);
            external.terminal.send_key(key)?;
            return Ok(false);
        }
        _ => {}
    }
    let external = app.external.as_mut().expect("checked by caller");
    external.last_escape = None;
    external.terminal.send_key(key)?;
    Ok(false)
}

fn handle_external_mouse(app: &mut App, mouse: MouseEvent) -> Result<()> {
    if update_mouse_selection(app, mouse, true) {
        return Ok(());
    }
    let external = app.external.as_mut().expect("checked by caller");
    let inner = external.area.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    if mouse.column >= inner.x
        && mouse.column < inner.right()
        && mouse.row >= inner.y
        && mouse.row < inner.bottom()
    {
        external.terminal.send_mouse(
            mouse,
            mouse.column.saturating_sub(inner.x),
            mouse.row.saturating_sub(inner.y),
        )?;
    }
    Ok(())
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
            if let Some(selection) = app.selection.as_mut() {
                selection.end = point;
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

async fn open_editor(
    terminal: &mut DefaultTerminal,
    editor: Option<&str>,
    content: &str,
    read_back: bool,
) -> Result<Option<String>> {
    let editor = editor
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("configure Editor in Settings first"))?;
    let mut arguments = shell_words::split(editor).context("cannot parse editor command")?;
    if arguments.is_empty() {
        bail!("editor command is empty");
    }
    let executable = arguments.remove(0);
    let path = temporary_document(content).await?;
    arguments.push(path.to_string_lossy().into_owned());

    execute!(stdout(), DisableMouseCapture, DisableBracketedPaste)?;
    ratatui::try_restore()?;
    let editor_result = Command::new(&executable).args(arguments).status().await;
    *terminal = ratatui::try_init()?;
    execute!(stdout(), EnableMouseCapture, EnableBracketedPaste)?;
    clear_terminal_screen()?;

    let status = match editor_result {
        Ok(status) => status,
        Err(error) => {
            let _ = fs::remove_file(&path).await;
            let context = if executable == "hx" {
                "cannot start `hx`; install Helix or change Editor in Settings".to_string()
            } else {
                format!("cannot start `{executable}`; change Editor in Settings")
            };
            return Err(error).context(context);
        }
    };
    if !status.success() {
        let _ = fs::remove_file(&path).await;
        bail!("editor exited with {status}");
    }
    let updated = if read_back {
        Some(fs::read_to_string(&path).await?)
    } else {
        None
    };
    let _ = fs::remove_file(path).await;
    Ok(updated)
}

fn clear_terminal_screen() -> Result<()> {
    execute!(
        stdout(),
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
        crossterm::cursor::MoveTo(0, 0)
    )?;
    Ok(())
}

fn tool_protocol(arguments: &serde_json::Value) -> Option<String> {
    let uri = arguments.get("uri")?.as_str()?;
    let separator = uri.find("://").or_else(|| uri.find(':'))?;
    (separator > 0).then(|| uri[..separator].to_string())
}

fn render(frame: &mut Frame<'_>, app: &mut App) {
    app.hit_regions.clear();
    app.selectable = None;
    let area = frame.area();
    frame.render_widget(Block::new().style(Style::default().bg(BG)), area);
    let input_height = if app.mode == Mode::Insert {
        (app.input.lines().len() as u16).clamp(1, 6) + 2
    } else {
        1
    };
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(input_height),
            Constraint::Length(1),
        ])
        .split(area);
    render_transcript(frame, app, areas[0]);
    if app.mode == Mode::Insert {
        frame.render_widget(&app.input, areas[1]);
    } else {
        let composer = if app.has_draft() {
            let text = app.input.lines().join(" ");
            Line::from(vec![
                Span::styled(
                    " DRAFT  ",
                    Style::default().fg(WARM).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    single_line_preview(&text, areas[1].width.saturating_sub(10) as usize),
                    Style::default().fg(TEXT),
                ),
            ])
        } else {
            Line::styled(" compose…", Style::default().fg(MUTED))
        };
        frame.render_widget(
            Paragraph::new(composer).style(Style::default().bg(SURFACE)),
            areas[1],
        );
        app.hit_regions.push(HitRegion {
            area: areas[1],
            target: AppHit::Composer,
        });
    }
    render_statusline(frame, app, areas[2]);
    let selectable_area = if app.external.is_some() {
        app.hit_regions.clear();
        render_external(frame, app)
    } else if let Some(overlay) = app.overlay {
        app.hit_regions.clear();
        render_overlay(frame, app, overlay);
        Some(overlay_area(frame.area(), overlay).inner(Margin {
            horizontal: 2,
            vertical: 2,
        }))
    } else {
        Some(areas[0])
    };
    if let Some(selectable_area) = selectable_area.filter(|area| !area.is_empty()) {
        capture_surface(frame, app, selectable_area);
        render_selection(frame, app);
    }
}

fn render_external(frame: &mut Frame<'_>, app: &mut App) -> Option<Rect> {
    let (inner, resize_error) = {
        let external = app.external.as_mut()?;
        external.area = centered(frame.area(), 92, 88);
        let inner = external.area.inner(Margin {
            horizontal: 1,
            vertical: 1,
        });
        let resize_error = external.terminal.resize(inner.height, inner.width).err();
        frame.render_widget(Clear, external.area);
        let parser = external.terminal.screen();
        frame.render_widget(
            PseudoTerminal::new(parser.screen()).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(ACCENT))
                    .style(Style::default().bg(SURFACE))
                    .title(external.title),
            ),
            external.area,
        );
        (inner, resize_error)
    };
    if let Some(error) = resize_error {
        app.set_flash(format!("Embedded terminal resize failed: {error:#}"));
    }
    Some(inner)
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

fn render_transcript(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    if app.blocks.is_empty() {
        render_welcome(frame, app, area);
        app.hit_regions.push(HitRegion {
            area,
            target: AppHit::Composer,
        });
        return;
    }
    let preview_width = area.width.saturating_sub(25) as usize;
    let items = app.blocks.iter().enumerate().map(|(index, block)| {
        let selected = index == app.selected_block;
        let color = if block.failed {
            ERROR
        } else {
            match block.kind {
                BlockKind::User => ACCENT,
                BlockKind::Assistant => TEXT,
                BlockKind::Reasoning => MUTED,
                BlockKind::Tool => WARM,
                BlockKind::Compaction => ACCENT,
                BlockKind::Notice => MUTED,
                BlockKind::Error => ERROR,
            }
        };
        let status = if index == app.blocks.len().saturating_sub(1)
            && app.busy
            && matches!(
                block.kind,
                BlockKind::Assistant | BlockKind::Reasoning | BlockKind::Tool
            ) {
            animation::spinner(app.frame).to_string()
        } else if block.kind == BlockKind::Tool {
            if block.failed {
                "×".to_string()
            } else if block.text.contains("\n\nRESULT\n") {
                "✓".to_string()
            } else {
                "·".to_string()
            }
        } else {
            " ".to_string()
        };
        ListItem::new(Line::from(vec![
            Span::styled(
                if selected { "› " } else { "  " },
                Style::default().fg(ACCENT),
            ),
            Span::styled(
                format!("{status} {:<16}", block.title),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                single_line_preview(&block.text, preview_width),
                Style::default().fg(if selected { TEXT } else { MUTED }),
            ),
        ]))
        .style(Style::default().bg(if selected { SURFACE } else { BG }))
    });
    let mut state = ListState::default().with_selected(Some(app.selected_block));
    frame.render_stateful_widget(
        List::new(items).block(Block::new().padding(Padding::horizontal(1))),
        area,
        &mut state,
    );
    for index in state.offset()..app.blocks.len() {
        let y = area.y.saturating_add((index - state.offset()) as u16);
        if y >= area.y.saturating_add(area.height) {
            break;
        }
        app.hit_regions.push(HitRegion {
            area: Rect::new(area.x, y, area.width, 1),
            target: AppHit::Transcript(index),
        });
    }
}

fn render_welcome(frame: &mut Frame<'_>, app: &App, area: Rect) {
    if area.width < 58 || area.height < 13 {
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(
                    "URI Agent · protocol-first coding",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Line::styled(
                    format!("{}/{}", app.info.provider, app.info.model),
                    Style::default().fg(TEXT),
                ),
                Line::styled(
                    "i compose · Space commands · F1 help",
                    Style::default().fg(MUTED),
                ),
            ])
            .alignment(Alignment::Center),
            area,
        );
        return;
    }
    let width = area.width.min(76);
    let height = area.height.min(15);
    let welcome = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    let mut lines = animation::wordmark(app.frame)
        .into_iter()
        .map(|line| Line::styled(line, Style::default().fg(ACCENT)))
        .collect::<Vec<_>>();
    lines.extend([
        Line::default(),
        Line::styled(
            "PROTOCOL-FIRST CODING SURFACE",
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
        Line::styled(
            format!("{}/{}", app.info.provider, app.info.model),
            Style::default().fg(MUTED),
        ),
        Line::styled(
            single_line_preview(&app.info.cwd.display().to_string(), 68),
            Style::default().fg(MUTED),
        ),
        Line::default(),
        Line::styled(
            "i compose   Space commands   F1 help",
            Style::default().fg(WARM),
        ),
    ]);
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), welcome);
}

fn single_line_preview(text: &str, limit: usize) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= limit {
        normalized
    } else if limit <= 1 {
        "…".to_string()
    } else {
        normalized.chars().take(limit - 1).collect::<String>() + "…"
    }
}

fn render_statusline(frame: &mut Frame<'_>, app: &App, area: Rect) {
    frame.render_widget(Block::new().style(Style::default().bg(SURFACE)), area);
    let compact = area.width < 64;
    let event_width = if app.blocks.is_empty() {
        0
    } else if compact {
        8
    } else {
        14
    };
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(if compact { 8 } else { 10 }),
            Constraint::Percentage(if compact { 45 } else { 34 }),
            Constraint::Min(0),
            Constraint::Length(event_width),
        ])
        .split(area);
    let (mode, mode_color) = if app.mode == Mode::Insert {
        ("INSERT", WARM)
    } else {
        ("BROWSE", ACCENT)
    };
    frame.render_widget(
        Paragraph::new(format!(" {mode} ")).style(
            Style::default()
                .fg(BG)
                .bg(mode_color)
                .add_modifier(Modifier::BOLD),
        ),
        columns[0],
    );
    let model = format!(" {}/{}", app.info.provider, app.info.model);
    frame.render_widget(
        Paragraph::new(single_line_preview(&model, columns[1].width as usize)).style(
            Style::default()
                .fg(if app.info.model_ready { TEXT } else { WARM })
                .bg(SURFACE),
        ),
        columns[1],
    );

    let (message, message_style) = if app.busy {
        let activity = app
            .activity
            .as_ref()
            .map(Activity::label)
            .unwrap_or_else(|| "working".to_string());
        let elapsed = app
            .busy_since
            .map(|since| format!(" {:.1}s", since.elapsed().as_secs_f32()))
            .unwrap_or_default();
        let wave = if columns[2].width > 30 {
            format!("  {}", animation::activity(app.frame, 8))
        } else {
            String::new()
        };
        (
            format!(
                " {} {activity}{elapsed}{wave}",
                animation::spinner(app.frame)
            ),
            Style::default().fg(ACCENT).bg(SURFACE),
        )
    } else if let Some(flash) = app.visible_flash() {
        (
            format!(" {flash}"),
            Style::default()
                .fg(if flash_is_error(flash) { ERROR } else { WARM })
                .bg(SURFACE),
        )
    } else if app.mode == Mode::Browse && app.has_draft() {
        let line_count = app.input.lines().len();
        (
            format!(
                " draft · {line_count} line{}",
                if line_count == 1 { "" } else { "s" }
            ),
            Style::default().fg(WARM).bg(SURFACE),
        )
    } else {
        (String::new(), Style::default().bg(SURFACE))
    };
    frame.render_widget(Paragraph::new(message).style(message_style), columns[2]);

    if !app.blocks.is_empty() {
        let position = if compact {
            format!(" {}/{}", app.selected_block + 1, app.blocks.len())
        } else {
            format!(" event {}/{} ", app.selected_block + 1, app.blocks.len())
        };
        frame.render_widget(
            Paragraph::new(position)
                .alignment(Alignment::Right)
                .style(Style::default().fg(MUTED).bg(SURFACE)),
            columns[3],
        );
    }
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
        ("BROWSE", "browse"),
        ("INSERT", "insert"),
        ("DETAIL", "detail"),
        ("LIST PANELS", "list"),
        ("TASKS", "tasks"),
        ("MODELS", "models"),
        ("SETTINGS", "settings"),
        ("COMMAND PANEL", "palette"),
        ("COMMAND LINE", "command"),
        ("TEXT FIELDS", "text"),
        ("SELECTION", "selection"),
        ("EMBEDDED TERMINAL", "terminal"),
        ("GLOBAL", "global"),
    ] {
        output.push_str(title);
        output.push('\n');
        for (key, action) in keymap.bindings_for(mode) {
            output.push_str(&format!("  {key:<14} {}\n", action.replace('_', " ")));
        }
        output.push('\n');
    }
    output
}

fn command_help(commands: &CommandRegistry) -> String {
    commands
        .list()
        .into_iter()
        .map(|command| {
            let aliases = if command.aliases.is_empty() {
                String::new()
            } else {
                format!(" ({})", command.aliases.join(", "))
            };
            format!("  :{:<14} {}{}\n", command.id, command.description, aliases)
        })
        .collect()
}

fn overlay_area(frame: Rect, overlay: Overlay) -> Rect {
    if overlay == Overlay::Command {
        Rect::new(
            1,
            frame.height.saturating_sub(6),
            frame.width.saturating_sub(2),
            5,
        )
    } else if overlay == Overlay::Models && frame.width < 110 {
        centered(frame, 96, 96)
    } else if matches!(overlay, Overlay::Settings | Overlay::Models) {
        centered(frame, 82, 96)
    } else {
        centered(frame, 78, 82)
    }
}

fn render_overlay(frame: &mut Frame<'_>, app: &mut App, overlay: Overlay) {
    let area = overlay_area(frame.area(), overlay);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(SURFACE).fg(TEXT))
        .padding(Padding::uniform(1));
    match overlay {
        Overlay::Detail => {
            let (title, text) = app
                .selected_block()
                .map(|block| (format!(" {} ", block.title), block_document(block)))
                .unwrap_or_else(|| (" DETAIL ".to_string(), "No event selected.".to_string()));
            frame.render_widget(
                Paragraph::new(text)
                    .block(block.title(format!("{title}· ↑/↓ scroll · e editor · Esc close ")))
                    .wrap(Wrap { trim: false })
                    .scroll((app.overlay_scroll, 0)),
                area,
            );
        }
        Overlay::Help => {
            let text = format!(
                "ACTIVE KEYMAP\n\n{}COMMANDS\n{}\nDrag to select read-only floats; Shift-drag selects interactive panels and embedded terminals. Press y or Ctrl+Shift+C to copy with OSC52.\n\nSlash commands remain available while composing.\n\nSESSION\n{}\n{}\n\nPROJECT\n{}",
                keymap_help(&app.keymap),
                command_help(&app.commands),
                app.info.session_id,
                app.info.model,
                app.info.cwd.display()
            );
            frame.render_widget(
                Paragraph::new(text)
                    .block(block.title(" HELP "))
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
                    .block(block.title(" PROTOCOLS · ↑/↓ scroll · read <name>://help "))
                    .wrap(Wrap { trim: false })
                    .scroll((app.overlay_scroll, 0)),
                area,
            );
        }
        Overlay::Tasks => render_tasks(frame, app, area, block),
        Overlay::Models => render_models(frame, app, area, block),
        Overlay::Settings => render_settings(frame, app, area, block),
        Overlay::Palette => render_palette(frame, app, area, block),
        Overlay::Command => {
            frame.render_widget(
                Paragraph::new(format!(":{}█", app.command_line))
                    .block(block.title(" COMMAND · Enter run · Esc cancel "))
                    .style(Style::default().fg(TEXT)),
                area,
            );
        }
        Overlay::Plugin => {
            let document = app.tui_document.as_ref();
            let title = document
                .map(|document| format!(" {} · ↑/↓ scroll · Esc close ", document.title))
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
    }
}

fn render_models(frame: &mut Frame<'_>, app: &mut App, area: Rect, block: Block<'_>) {
    let inner = block.inner(area);
    frame.render_widget(
        block.title(" MODELS · type to search · ↑/↓ select · Enter use · Ctrl+R refresh "),
        area,
    );
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(4),
            Constraint::Length(4),
            Constraint::Length(1),
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

    let narrow = sections[1].width < 82;
    let provider_width: usize = if sections[1].width < 60 { 10 } else { 15 };
    let name_width: usize = if sections[1].width < 60 { 18 } else { 30 };
    let mut previous_provider = "";
    let items = selector.visible().enumerate().map(|(position, model)| {
        let selected = position == selector.selected_position();
        let provider = if model.provider == previous_provider {
            "│".to_string()
        } else {
            previous_provider = &model.provider;
            model.provider.clone()
        };
        let current = if selector.is_current(model) {
            "●"
        } else {
            " "
        };
        let capabilities = if narrow {
            context_label(model.context_window())
        } else {
            format!(
                "{}{} · {}",
                context_label(model.context_window()),
                if reasoning(model) { " · think" } else { "" },
                model.api
            )
        };
        ListItem::new(Line::from(vec![
            Span::styled(
                if selected { "› " } else { "  " },
                Style::default().fg(ACCENT),
            ),
            Span::styled(
                format!("{current} {provider:<provider_width$}"),
                Style::default().fg(if current == "●" { ACCENT } else { MUTED }),
            ),
            Span::styled(
                format!(
                    "{:<name_width$}",
                    single_line_preview(model_label(model), name_width.saturating_sub(2))
                ),
                Style::default()
                    .fg(if selected { ACCENT } else { TEXT })
                    .add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            Span::styled(capabilities, Style::default().fg(MUTED)),
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

    if let Some(model) = selector.selected() {
        let reasoning_label = if reasoning(model) {
            "reasoning"
        } else {
            "standard"
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled(
                        model_label(model),
                        Style::default().fg(WARM).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("  {}/{}", model.provider, model.id),
                        Style::default().fg(MUTED),
                    ),
                ]),
                Line::styled(
                    format!(
                        "{} context · {reasoning_label} · {}",
                        context_label(model.context_window()),
                        model.api
                    ),
                    Style::default().fg(TEXT),
                ),
                Line::styled(
                    if selector.is_current(model) {
                        if app.info.model_ready {
                            "● current provider credential is ready"
                        } else {
                            "○ current provider needs a credential · open Settings"
                        }
                    } else {
                        "Provider credentials are checked when this model is selected"
                    },
                    Style::default().fg(if selector.is_current(model) && !app.info.model_ready {
                        WARM
                    } else {
                        MUTED
                    }),
                ),
            ])
            .block(Block::default().borders(Borders::TOP).title(" SELECTED ")),
            sections[2],
        );
    } else {
        let empty = if selector.model_count() == 0 {
            "No runnable models are cached. Press Ctrl+R to fetch pi.dev, or configure models.json."
        } else {
            "No models match this search. Backspace to broaden it."
        };
        frame.render_widget(
            Paragraph::new(empty)
                .style(Style::default().fg(WARM))
                .block(Block::default().borders(Borders::TOP)),
            sections[2],
        );
    }
    let footer = if sections[3].width < 74 {
        "● current  ·  double-click applies  ·  Esc closes"
    } else {
        "● current  ·  mouse click selects  ·  double-click applies  ·  Esc closes"
    };
    frame.render_widget(
        Paragraph::new(footer)
            .alignment(Alignment::Center)
            .style(Style::default().fg(MUTED)),
        sections[3],
    );
}

fn render_settings(frame: &mut Frame<'_>, app: &mut App, area: Rect, block: Block<'_>) {
    let inner = block.inner(area);
    let Some(settings) = app.settings.as_ref() else {
        frame.render_widget(Paragraph::new("Loading settings…").block(block), area);
        return;
    };
    let provider = settings.provider();
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
    let provider = format!("{provider}  ·  Enter to choose");
    let model = format!("{model}  ·  Enter to choose");
    let key = if settings.editing == Some(EditingSetting::ApiKey) {
        format!("{}█", "•".repeat(settings.api_key.chars().count().min(36)))
    } else if settings.api_key_changed {
        format!(
            "{}  staged",
            "•".repeat(settings.api_key.chars().count().min(36))
        )
    } else if settings.active.api_key.is_some() {
        format!("configured  ·  {}", settings.active.api_key_source.label())
    } else {
        "not configured".to_string()
    };
    let output_limit = if settings.editing == Some(EditingSetting::OutputLimit) {
        format!("{}█", settings.output_limit)
    } else {
        format!("{} bytes", settings.output_limit)
    };
    let editor = if settings.editing == Some(EditingSetting::Editor) {
        format!("{}█", settings.editor)
    } else if settings.editor.is_empty() {
        "not configured".to_string()
    } else {
        settings.editor.clone()
    };
    let picker = if settings.editing == Some(EditingSetting::Picker) {
        format!("{}█", settings.picker)
    } else if settings.picker.is_empty() {
        "not configured".to_string()
    } else {
        settings.picker.clone()
    };
    let rows = [
        ("Provider", provider),
        ("Model", model),
        ("API key", key),
        ("Output limit", output_limit),
        ("Editor", editor),
        ("Editor mode", format!("‹  {}  ›", settings.editor_mode)),
        ("Picker", picker),
        ("Picker mode", format!("‹  {}  ›", settings.picker_mode)),
    ];
    let mut lines = vec![
        Line::styled(
            "Model catalog from pi.dev · supported API families only",
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
                Style::default()
                    .fg(if selected { ACCENT } else { MUTED })
                    .add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            Span::styled(value, Style::default().fg(TEXT)),
        ]));
        lines.push(Line::default());
    }
    lines.extend([
        Line::styled(
            "EFFECTIVE SOURCES",
            Style::default().fg(WARM).add_modifier(Modifier::BOLD),
        ),
        Line::styled(
            format!(
                "provider {} · model {} · limit {} · editor {} / {} · picker {} / {}",
                settings.active.provider_source.label(),
                settings.active.model_source.label(),
                settings.active.output_limit_source.label(),
                settings.active.editor_source.label(),
                settings.active.editor_mode_source.label(),
                settings.active.picker_source.label(),
                settings.active.picker_mode_source.label()
            ),
            Style::default().fg(MUTED),
        ),
        Line::styled(
            "Environment variables and command-line values override saved text files.",
            Style::default().fg(MUTED),
        ),
        Line::default(),
        Line::styled(
            "↑/↓ field  ·  Enter edit/choose  ·  x clear key  ·  s save  ·  r refresh",
            Style::default().fg(ACCENT),
        ),
    ]);
    if let Some(flash) = app.visible_flash() {
        lines.extend([
            Line::default(),
            Line::styled(flash, Style::default().fg(WARM)),
        ]);
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(block.title(" SETTINGS · Esc close "))
            .wrap(Wrap { trim: false }),
        area,
    );
    for index in 0..8 {
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
    frame.render_widget(
        block.title(" TASKS · ↑/↓ select · e editor · x cancel "),
        area,
    );
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
        .split(inner);
    let items = app.task_records.iter().enumerate().map(|(index, task)| {
        let marker = if index == app.selected_task {
            "›"
        } else {
            " "
        };
        ListItem::new(format!(
            "{marker} {:9} {}",
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
    frame.render_stateful_widget(List::new(items), columns[0], &mut state);
    for index in state.offset()..app.task_records.len() {
        let y = columns[0].y.saturating_add((index - state.offset()) as u16);
        if y >= columns[0].y.saturating_add(columns[0].height) {
            break;
        }
        app.hit_regions.push(HitRegion {
            area: Rect::new(columns[0].x, y, columns[0].width, 1),
            target: AppHit::Task(index),
        });
    }
    if let Some(task) = app.task_records.get(app.selected_task) {
        let content = format!(
            "{}://tasks/{}\n\nStatus: {}\n\n{}",
            task.protocol,
            task.id,
            task.status.as_str(),
            String::from_utf8_lossy(&task.content)
        );
        frame.render_widget(
            Paragraph::new(content)
                .style(Style::default().fg(TEXT))
                .wrap(Wrap { trim: false })
                .scroll((app.overlay_scroll, 0)),
            columns[1].inner(Margin {
                horizontal: 2,
                vertical: 0,
            }),
        );
    }
}

fn render_palette(frame: &mut Frame<'_>, app: &mut App, area: Rect, block: Block<'_>) {
    let inner = block.inner(area);
    frame.render_widget(
        block.title(" COMMANDS · ↑/↓ select · Enter or click run · Esc close "),
        area,
    );
    let commands = app.commands.list();
    let items = commands.iter().enumerate().map(|(index, item)| {
        let selected = index == app.palette_selected;
        ListItem::new(Line::from(vec![
            Span::styled(
                if selected { "› " } else { "  " },
                Style::default().fg(ACCENT),
            ),
            Span::styled(
                format!("{:<24}", item.title),
                Style::default()
                    .fg(if selected { ACCENT } else { TEXT })
                    .add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            Span::styled(item.description.clone(), Style::default().fg(MUTED)),
        ]))
        .style(Style::default().bg(if selected { BG } else { SURFACE }))
    });
    let mut state = ListState::default().with_selected(Some(app.palette_selected));
    frame.render_stateful_widget(List::new(items), inner, &mut state);
    for index in state.offset()..commands.len() {
        let y = inner.y.saturating_add((index - state.offset()) as u16);
        if y >= inner.y.saturating_add(inner.height) {
            break;
        }
        app.hit_regions.push(HitRegion {
            area: Rect::new(inner.x, y, inner.width, 1),
            target: AppHit::Palette(index),
        });
    }
}

fn style_input(input: &mut TextArea<'static>, busy: bool) {
    input.set_block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(if busy { MUTED } else { ACCENT }))
            .title(" INSERT ")
            .style(Style::default().bg(SURFACE)),
    );
    input.set_style(Style::default().fg(TEXT).bg(SURFACE));
    input.set_cursor_line_style(Style::default().fg(TEXT).bg(SURFACE));
    input.set_cursor_style(Style::default().fg(BG).bg(ACCENT));
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

struct RestoreTerminal;

impl Drop for RestoreTerminal {
    fn drop(&mut self) {
        let _ = execute!(stdout(), DisableMouseCapture, DisableBracketedPaste);
        ratatui::restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ValueSource;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn test_app() -> App {
        App::new(
            Vec::new(),
            Arc::new(CommandRegistry::with_core_commands()),
            Arc::new(TuiRegistry::default()),
            TuiInfo {
                cwd: PathBuf::from("/workspace"),
                provider: "test".to_string(),
                model: "model".to_string(),
                session_id: "session".to_string(),
                context_window: 128_000,
                model_ready: true,
                editor: None,
                editor_mode: ExternalMode::Float,
                picker: Some("fzf".to_string()),
                picker_mode: ExternalMode::Float,
            },
            Keymap::with_defaults().unwrap(),
        )
    }

    #[test]
    fn settings_panel_renders_sources_and_never_renders_the_api_key() {
        let active = ActiveSettings {
            provider: "openai".to_string(),
            model: "gpt-5.2".to_string(),
            api_key: Some("super-secret-value".to_string()),
            output_limit: 32 * 1024,
            editor: Some("nvim -f".to_string()),
            editor_mode: ExternalMode::Float,
            picker: Some("fzf".to_string()),
            picker_mode: ExternalMode::Float,
            provider_source: ValueSource::Global,
            model_source: ValueSource::Global,
            api_key_source: ValueSource::Global,
            output_limit_source: ValueSource::Global,
            editor_source: ValueSource::Global,
            editor_mode_source: ValueSource::Global,
            picker_source: ValueSource::Global,
            picker_mode_source: ValueSource::Global,
            credential_environment: BTreeMap::new(),
        };
        let model = CatalogModel {
            id: "gpt-5.2".to_string(),
            name: "GPT-5.2".to_string(),
            api: "openai-responses".to_string(),
            provider: "openai".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            headers: BTreeMap::new(),
            metadata: BTreeMap::new(),
        };
        let mut app = App::new(
            Vec::new(),
            Arc::new(CommandRegistry::with_core_commands()),
            Arc::new(TuiRegistry::default()),
            TuiInfo {
                cwd: PathBuf::from("/workspace"),
                provider: active.provider.clone(),
                model: active.model.clone(),
                session_id: "session".to_string(),
                context_window: 128_000,
                model_ready: true,
                editor: active.editor.clone(),
                editor_mode: active.editor_mode,
                picker: active.picker.clone(),
                picker_mode: active.picker_mode,
            },
            Keymap::default(),
        );
        app.overlay = Some(Overlay::Settings);
        app.settings = Some(SettingsState {
            active,
            model: Some(model),
            selected: 0,
            editing: None,
            api_key: String::new(),
            api_key_changed: false,
            output_limit: "32768".to_string(),
            editor: "nvim -f".to_string(),
            editor_mode: ExternalMode::Float,
            picker: "fzf".to_string(),
            picker_mode: ExternalMode::Float,
        });
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(rendered.contains("SETTINGS"));
        assert!(rendered.contains("GPT-5.2"));
        assert!(rendered.contains("configured  ·  settings.json"));
        assert!(!rendered.contains("super-secret-value"));
    }

    #[test]
    fn model_panel_search_metadata_and_mouse_targets_render_together() {
        let catalog_model = |provider: &str, id: &str, name: &str| CatalogModel {
            id: id.to_string(),
            name: name.to_string(),
            api: "openai-responses".to_string(),
            provider: provider.to_string(),
            base_url: String::new(),
            headers: BTreeMap::new(),
            metadata: BTreeMap::from([
                ("contextWindow".to_string(), serde_json::json!(128_000)),
                ("reasoning".to_string(), serde_json::json!(true)),
            ]),
        };
        let mut app = test_app();
        app.info.provider = "openai".to_string();
        app.info.model = "gpt-5".to_string();
        app.model_selector = Some(ModelSelector::from_models(
            vec![
                catalog_model("anthropic", "claude", "Claude Sonnet"),
                catalog_model("openai", "gpt-5", "GPT 5"),
            ],
            "openai",
            "gpt-5",
        ));
        app.overlay = Some(Overlay::Models);
        let backend = TestBackend::new(120, 36);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(rendered.contains("MODELS"));
        assert!(rendered.contains("GPT 5"));
        assert!(rendered.contains("128k"));
        assert!(rendered.contains("credential is ready"));
        assert_eq!(
            app.hit_regions
                .iter()
                .filter(|region| matches!(region.target, AppHit::Model(_)))
                .count(),
            2
        );
    }

    #[test]
    fn welcome_and_model_panel_degrade_without_panicking_on_small_terminals() {
        for (width, height) in [(48, 12), (80, 24), (140, 44)] {
            let mut app = test_app();
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
        }

        let mut app = test_app();
        app.model_selector = Some(ModelSelector::from_models(Vec::new(), "test", "model"));
        app.overlay = Some(Overlay::Models);
        let backend = TestBackend::new(48, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
    }

    #[test]
    fn activity_status_follows_stream_events() {
        let mut app = test_app();
        let event = |sequence, kind| SessionEvent {
            sequence,
            at: chrono::Utc::now(),
            kind,
        };
        app.apply(event(
            1,
            EventKind::User {
                text: "inspect files".to_string(),
            },
        ));
        assert!(app.busy);
        assert!(matches!(&app.activity, Some(Activity::Thinking)));
        app.apply(event(
            2,
            EventKind::ToolCall {
                call_id: "call".to_string(),
                name: "read".to_string(),
                arguments: serde_json::json!({"uri": "file://src/main.rs"}),
            },
        ));
        assert!(matches!(&app.activity, Some(Activity::Tool(name)) if name == "file"));
        app.apply(event(3, EventKind::TurnFinished));
        assert!(!app.busy);
        assert!(app.activity.is_none());
    }

    #[tokio::test]
    async fn insert_enter_adds_a_line_then_escape_preserves_and_browse_enter_submits() {
        let mut app = test_app();
        let tasks = TaskManager::new();
        app.mode = Mode::Insert;
        app.input.insert_str("first");

        let action = handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &tasks,
        )
        .await;
        assert!(matches!(action, Action::Continue));
        assert!(app.mode == Mode::Insert);
        assert_eq!(app.input.lines(), ["first", ""]);

        app.input.insert_str("second");
        let action = handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &tasks,
        )
        .await;
        assert!(matches!(action, Action::Continue));
        assert!(app.mode == Mode::Browse);
        assert_eq!(app.input.lines(), ["first", "second"]);

        let action = handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &tasks,
        )
        .await;
        assert!(matches!(action, Action::Submit(prompt) if prompt == "first\nsecond"));
        assert!(!app.has_draft());
    }

    #[tokio::test]
    async fn browse_enter_without_a_draft_opens_the_selected_event() {
        let mut app = test_app();
        let tasks = TaskManager::new();
        app.push(
            BlockKind::Assistant,
            "AGENT",
            "answer".to_string(),
            None,
            false,
        );

        let action = handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &tasks,
        )
        .await;

        assert!(matches!(action, Action::Continue));
        assert!(app.overlay == Some(Overlay::Detail));
    }

    #[test]
    fn conversation_chrome_has_no_persistent_shortcut_or_estimate_noise() {
        let mut app = test_app();
        app.push(
            BlockKind::Assistant,
            "AGENT",
            "answer".to_string(),
            None,
            false,
        );
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(rendered.contains("BROWSE"));
        assert!(rendered.contains("test/model"));
        assert!(rendered.contains("event 1/1"));
        assert!(!rendered.contains("ctx"));
        assert!(!rendered.contains("protocols"));
        assert!(!rendered.contains("Space commands"));
    }

    #[test]
    fn status_notices_expire_instead_of_becoming_permanent_chrome() {
        let mut app = test_app();
        app.set_flash("saved");
        assert_eq!(app.visible_flash(), Some("saved"));

        app.flash_at = Some(Instant::now() - FLASH_DURATION);
        assert_eq!(app.visible_flash(), None);
    }

    #[test]
    fn tool_call_and_result_share_one_preview_and_one_complete_detail() {
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
        assert_eq!(app.blocks[0].title, "TOOL · read");
        assert!(app.blocks[0].text.contains("CALL"));
        assert!(app.blocks[0].text.contains("RESULT"));
        let detail = block_document(&app.blocks[0]);
        assert!(detail.contains("file://src/main.rs"));
        assert!(detail.contains("complete tool output"));
        assert!(!single_line_preview(&app.blocks[0].text, 18).contains('\n'));
    }

    #[test]
    fn automatic_compaction_keeps_the_turn_busy_but_manual_compaction_finishes() {
        let mut app = test_app();
        app.busy = true;
        let compaction = |sequence, manual| SessionEvent {
            sequence,
            at: chrono::Utc::now(),
            kind: EventKind::Compaction {
                summary: "checkpoint".to_string(),
                tokens_before: 100,
                replacement_history: Vec::new(),
                manual,
            },
        };

        app.apply(compaction(1, false));
        assert!(app.busy);

        app.apply(compaction(2, true));
        assert!(!app.busy);
        assert!(app.flash.as_deref().unwrap().contains("retained"));
    }

    #[test]
    fn key_events_have_stable_rhai_names() {
        assert_eq!(
            key_name(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL)),
            "ctrl+e"
        );
        assert_eq!(
            key_name(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT)),
            "shift+g"
        );
        assert_eq!(
            key_name(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)),
            "space"
        );
    }

    #[test]
    fn colon_commands_share_the_palette_command_set() {
        let commands = CommandRegistry::with_core_commands();
        assert_eq!(
            commands.resolve(":settings").unwrap().spec.target,
            CommandTarget::Core(CoreCommand::Settings)
        );
        assert_eq!(
            commands.resolve("model").unwrap().spec.target,
            CommandTarget::Core(CoreCommand::Models)
        );
        assert_eq!(
            commands.resolve("compact").unwrap().spec.target,
            CommandTarget::Core(CoreCommand::Compact)
        );
        assert!(commands.resolve("sessions").is_none());
        assert_eq!(
            commands.resolve("q").unwrap().spec.target,
            CommandTarget::Core(CoreCommand::Quit)
        );
        assert!(commands.resolve("unknown").is_none());
    }

    #[test]
    fn command_panel_renders_clickable_items() {
        let mut app = test_app();
        app.overlay = Some(Overlay::Palette);
        app.palette_selected = 2;
        let backend = TestBackend::new(100, 32);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(rendered.contains("COMMANDS"));
        assert!(rendered.contains("Open in editor"));
        assert_eq!(
            app.hit_regions
                .iter()
                .filter(|region| matches!(region.target, AppHit::Palette(_)))
                .count(),
            app.commands.list().len()
        );
    }

    #[test]
    fn command_line_renders_entered_text() {
        let mut app = test_app();
        app.overlay = Some(Overlay::Command);
        app.command_line = "settings".to_string();
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(rendered.contains(":settings█"));
    }

    #[test]
    fn mouse_hit_testing_uses_rendered_regions() {
        let regions = [HitRegion {
            area: Rect::new(10, 5, 20, 2),
            target: AppHit::Transcript(4),
        }];
        let inside = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 12,
            row: 6,
            modifiers: KeyModifiers::NONE,
        };
        let outside = MouseEvent {
            column: 30,
            ..inside
        };

        assert_eq!(hit_target(&regions, inside), Some(AppHit::Transcript(4)));
        assert_eq!(hit_target(&regions, outside), None);
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
