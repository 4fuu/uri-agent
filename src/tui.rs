use crate::catalog::{CatalogModel, ModelCatalog};
use crate::config::{ActiveSettings, ConfigManager};
use crate::keymap::Keymap;
use crate::model::configured_backend;
use crate::output::OutputStore;
use crate::protocol::ProtocolDescriptor;
use crate::runtime::AgentRuntime;
use crate::session::{EventKind, Session, SessionChoice, SessionEvent, SessionSummary};
use crate::task::{TaskManager, TaskRecord, TaskStatus};
use anyhow::{Context, Result, anyhow, bail};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use crossterm::execute;
use futures_util::StreamExt;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap,
};
use ratatui::{DefaultTerminal, Frame};
use std::collections::HashMap;
use std::env;
use std::io::stdout;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::{fs, io::AsyncWriteExt, process::Command, time};
use tui_textarea::TextArea;
use uuid::Uuid;

const BG: Color = Color::Rgb(13, 15, 18);
const SURFACE: Color = Color::Rgb(21, 24, 28);
const TEXT: Color = Color::Rgb(218, 223, 229);
const MUTED: Color = Color::Rgb(116, 124, 135);
const ACCENT: Color = Color::Rgb(104, 210, 194);
const WARM: Color = Color::Rgb(239, 173, 104);
const ERROR: Color = Color::Rgb(239, 108, 120);

pub struct TuiInfo {
    pub cwd: PathBuf,
    pub provider: String,
    pub model: String,
    pub session_id: String,
    pub editor: Option<String>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum BlockKind {
    User,
    Assistant,
    Reasoning,
    Tool,
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
    Settings,
    Palette,
    Command,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UiCommand {
    Compose,
    Detail,
    Editor,
    Sessions,
    Tasks,
    Protocols,
    Settings,
    Help,
    Quit,
}

struct PaletteItem {
    command: UiCommand,
    name: &'static str,
    description: &'static str,
}

const PALETTE_ITEMS: [PaletteItem; 9] = [
    PaletteItem {
        command: UiCommand::Compose,
        name: "Compose message",
        description: "enter Insert mode",
    },
    PaletteItem {
        command: UiCommand::Detail,
        name: "Open event detail",
        description: "inspect the selected event",
    },
    PaletteItem {
        command: UiCommand::Editor,
        name: "Open in Helix",
        description: "use the configured external editor",
    },
    PaletteItem {
        command: UiCommand::Sessions,
        name: "Switch session",
        description: "return to the Sessions screen",
    },
    PaletteItem {
        command: UiCommand::Tasks,
        name: "Managed tasks",
        description: "inspect asynchronous protocol work",
    },
    PaletteItem {
        command: UiCommand::Protocols,
        name: "Protocols",
        description: "show registered read and exec routes",
    },
    PaletteItem {
        command: UiCommand::Settings,
        name: "Settings",
        description: "provider, model, credential, limits, editor",
    },
    PaletteItem {
        command: UiCommand::Help,
        name: "Help",
        description: "active keymap and command reference",
    },
    PaletteItem {
        command: UiCommand::Quit,
        name: "Quit",
        description: "close URI Agent",
    },
];

#[derive(Clone, Copy, Eq, PartialEq)]
enum EditingSetting {
    ProviderSearch,
    ModelSearch,
    ApiKey,
    OutputLimit,
    Editor,
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
    Setting(usize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PickerHit {
    Session(usize),
    Directory(usize),
    ChooseDirectory,
}

#[derive(Clone, Copy)]
struct HitRegion<T> {
    area: Rect,
    target: T,
}

struct SettingsState {
    active: ActiveSettings,
    providers: Vec<String>,
    provider_index: usize,
    models: Vec<CatalogModel>,
    model_index: usize,
    selected: usize,
    editing: Option<EditingSetting>,
    api_key: String,
    api_key_changed: bool,
    output_limit: String,
    editor: String,
    search: String,
}

impl SettingsState {
    async fn load(manager: &ConfigManager, catalog: &ModelCatalog) -> Self {
        let active = manager.current().await;
        let mut providers = catalog.providers().await;
        if !providers.contains(&active.provider) {
            providers.push(active.provider.clone());
            providers.sort();
        }
        let provider_index = providers
            .iter()
            .position(|provider| provider == &active.provider)
            .unwrap_or(0);
        let models = catalog.models(&active.provider).await;
        let model_index = models
            .iter()
            .position(|model| model.id == active.model)
            .unwrap_or(0);
        let output_limit = active.output_limit.to_string();
        let editor = active.editor.clone().unwrap_or_default();
        Self {
            active,
            providers,
            provider_index,
            models,
            model_index,
            selected: 0,
            editing: None,
            api_key: String::new(),
            api_key_changed: false,
            output_limit,
            editor,
            search: String::new(),
        }
    }

    fn provider(&self) -> &str {
        self.providers
            .get(self.provider_index)
            .map(String::as_str)
            .unwrap_or(&self.active.provider)
    }

    fn model(&self) -> Option<&CatalogModel> {
        self.models.get(self.model_index)
    }

    async fn cycle_provider(&mut self, direction: isize, catalog: &ModelCatalog) {
        self.provider_index = shifted(self.provider_index, self.providers.len(), direction);
        let provider = self.provider().to_string();
        self.models = catalog.models(&provider).await;
        self.model_index = self.models.len().saturating_sub(1);
    }

    fn cycle_model(&mut self, direction: isize) {
        self.model_index = shifted(self.model_index, self.models.len(), direction);
    }

    fn provider_search_match(&self) -> Option<usize> {
        if self.search.is_empty() {
            return Some(self.provider_index);
        }
        let query = self.search.to_ascii_lowercase();
        self.providers
            .iter()
            .position(|provider| provider.eq_ignore_ascii_case(&query))
            .or_else(|| {
                self.providers
                    .iter()
                    .position(|provider| provider.to_ascii_lowercase().starts_with(&query))
            })
            .or_else(|| {
                self.providers
                    .iter()
                    .position(|provider| provider.to_ascii_lowercase().contains(&query))
            })
    }

    fn model_search_match(&self) -> Option<usize> {
        if self.search.is_empty() {
            return (!self.models.is_empty()).then_some(self.model_index);
        }
        let query = self.search.to_ascii_lowercase();
        self.models
            .iter()
            .position(|model| model.id.eq_ignore_ascii_case(&query))
            .or_else(|| {
                self.models.iter().position(|model| {
                    model.id.to_ascii_lowercase().starts_with(&query)
                        || model.name.to_ascii_lowercase().starts_with(&query)
                })
            })
            .or_else(|| {
                self.models.iter().position(|model| {
                    model.id.to_ascii_lowercase().contains(&query)
                        || model.name.to_ascii_lowercase().contains(&query)
                })
            })
    }
}

struct App {
    input: TextArea<'static>,
    blocks: Vec<DisplayBlock>,
    protocols: Vec<ProtocolDescriptor>,
    tasks: HashMap<String, (String, TaskStatus)>,
    task_records: Vec<TaskRecord>,
    selected_task: usize,
    selected_block: usize,
    mode: Mode,
    overlay: Option<Overlay>,
    overlay_scroll: u16,
    busy: bool,
    frame: usize,
    last_sequence: Option<u64>,
    info: TuiInfo,
    flash: Option<String>,
    settings: Option<SettingsState>,
    keymap: Keymap,
    palette_selected: usize,
    command_line: String,
    hit_regions: Vec<HitRegion<AppHit>>,
    last_click: Option<(AppHit, Instant)>,
}

impl App {
    fn new(protocols: Vec<ProtocolDescriptor>, info: TuiInfo, keymap: Keymap) -> Self {
        let mut input = TextArea::default();
        style_input(&mut input, false);
        Self {
            input,
            blocks: Vec::new(),
            protocols,
            tasks: HashMap::new(),
            task_records: Vec::new(),
            selected_task: 0,
            selected_block: 0,
            mode: Mode::Browse,
            overlay: None,
            overlay_scroll: 0,
            busy: false,
            frame: 0,
            last_sequence: None,
            info,
            flash: None,
            settings: None,
            keymap,
            palette_selected: 0,
            command_line: String::new(),
            hit_regions: Vec::new(),
            last_click: None,
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
            EventKind::SessionCreated { .. } | EventKind::ModelMessage { .. } => {}
            EventKind::User { text } => {
                self.busy = true;
                self.push(BlockKind::User, "YOU", text, None, false);
            }
            EventKind::AssistantText { text } => {
                self.append_or_push(BlockKind::Assistant, "AGENT", text);
            }
            EventKind::AssistantReasoning { text } => {
                self.append_or_push(BlockKind::Reasoning, "THINKING", text);
            }
            EventKind::ToolCall {
                call_id,
                name,
                arguments,
            } => {
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
            }
            EventKind::Task {
                id, label, status, ..
            } => {
                self.tasks.insert(id, (label, status));
            }
            EventKind::Notice { text } => self.push(BlockKind::Notice, "SYSTEM", text, None, false),
            EventKind::Error { text } => {
                self.busy = false;
                self.push(BlockKind::Error, "ERROR", text, None, true);
            }
            EventKind::TurnFinished => self.busy = false,
        }
        if follow {
            self.selected_block = self.blocks.len().saturating_sub(1);
        }
        style_input(&mut self.input, self.busy);
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
            self.flash = Some("A turn is already running".to_string());
            return None;
        }
        let text = self.input.lines().join("\n");
        if text.trim().is_empty() {
            return None;
        }
        self.input = TextArea::default();
        style_input(&mut self.input, true);
        self.busy = true;
        self.mode = Mode::Browse;
        Some(text)
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

pub struct SessionLaunch {
    pub cwd: PathBuf,
    pub session: SessionChoice,
}

pub enum TuiExit {
    Quit,
    Sessions,
}

struct SessionPicker {
    sessions: Vec<SessionSummary>,
    selected: usize,
    directory: Option<DirectoryPicker>,
    base: PathBuf,
    flash: Option<String>,
    keymap: Keymap,
    hit_regions: Vec<HitRegion<PickerHit>>,
    last_click: Option<(PickerHit, Instant)>,
}

struct DirectoryPicker {
    current: PathBuf,
    entries: Vec<PathBuf>,
    selected: usize,
    searching: bool,
    query: String,
}

impl DirectoryPicker {
    async fn open(current: PathBuf) -> Result<Self> {
        let mut picker = Self {
            current,
            entries: Vec::new(),
            selected: 0,
            searching: false,
            query: String::new(),
        };
        picker.reload().await?;
        Ok(picker)
    }

    async fn reload(&mut self) -> Result<()> {
        let mut directory = fs::read_dir(&self.current)
            .await
            .with_context(|| format!("cannot read {}", self.current.display()))?;
        let mut entries = Vec::new();
        while let Some(entry) = directory.next_entry().await? {
            if entry.file_type().await?.is_dir() {
                entries.push(entry.path());
            }
        }
        entries.sort_by(|left, right| {
            left.file_name()
                .unwrap_or_default()
                .cmp(right.file_name().unwrap_or_default())
        });
        self.entries = entries;
        self.selected = self.selected.min(self.visible_len().saturating_sub(1));
        Ok(())
    }

    fn visible(&self) -> Vec<&PathBuf> {
        let query = self.query.to_ascii_lowercase();
        self.entries
            .iter()
            .filter(|path| {
                query.is_empty()
                    || path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.to_ascii_lowercase().contains(&query))
            })
            .collect()
    }

    fn visible_len(&self) -> usize {
        self.visible().len()
    }

    fn selected_path(&self) -> Option<PathBuf> {
        self.visible()
            .get(self.selected)
            .map(|path| (*path).clone())
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

async fn selected_session_launch(picker: &mut SessionPicker) -> Result<Option<SessionLaunch>> {
    let Some(session) = picker.sessions.get(picker.selected) else {
        picker.directory = Some(DirectoryPicker::open(picker.base.clone()).await?);
        return Ok(None);
    };
    match session.cwd.canonicalize() {
        Ok(cwd) if cwd.is_dir() => Ok(Some(SessionLaunch {
            cwd,
            session: SessionChoice::Existing(session.id.clone()),
        })),
        _ => {
            picker.flash = Some(format!(
                "Project directory is unavailable: {}",
                session.cwd.display()
            ));
            Ok(None)
        }
    }
}

pub async fn select_session(base: &Path) -> Result<Option<SessionLaunch>> {
    let base = base.canonicalize().unwrap_or_else(|_| base.to_path_buf());
    let mut picker = SessionPicker {
        sessions: Session::list(&base).await?,
        selected: 0,
        directory: None,
        base,
        flash: None,
        keymap: Keymap::load(None).await?,
        hit_regions: Vec::new(),
        last_click: None,
    };
    let mut terminal = ratatui::try_init()?;
    let _restore = RestoreTerminal;
    execute!(stdout(), EnableMouseCapture, EnableBracketedPaste)?;
    let mut events = EventStream::new();
    loop {
        terminal.draw(|frame| render_session_picker(frame, &mut picker))?;
        let Some(event) = events.next().await else {
            return Ok(None);
        };
        match event? {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                let key_name = key_name(key);
                if picker.directory.is_some() {
                    let searching = picker
                        .directory
                        .as_ref()
                        .is_some_and(|directory| directory.searching);
                    let action = picker
                        .keymap
                        .action(if searching { "text" } else { "directory" }, &key_name);
                    if searching {
                        let directory = picker.directory.as_mut().unwrap();
                        match action.as_deref() {
                            Some("confirm" | "cancel") => directory.searching = false,
                            Some("backspace") => {
                                directory.query.pop();
                                directory.selected = 0;
                            }
                            Some("quit") => return Ok(None),
                            _ => {
                                if let KeyCode::Char(character) = key.code
                                    && !key.modifiers.intersects(
                                        KeyModifiers::CONTROL
                                            | KeyModifiers::ALT
                                            | KeyModifiers::SUPER,
                                    )
                                {
                                    directory.query.push(character);
                                    directory.selected = 0;
                                }
                            }
                        }
                        continue;
                    }
                    match action.as_deref() {
                        Some("quit") => return Ok(None),
                        Some("cancel" | "close") => picker.directory = None,
                        Some("next") => {
                            let directory = picker.directory.as_mut().unwrap();
                            directory.selected = directory
                                .selected
                                .saturating_add(1)
                                .min(directory.visible_len().saturating_sub(1));
                        }
                        Some("previous") => {
                            let directory = picker.directory.as_mut().unwrap();
                            directory.selected = directory.selected.saturating_sub(1);
                        }
                        Some("search") => {
                            let directory = picker.directory.as_mut().unwrap();
                            directory.searching = true;
                            directory.query.clear();
                            directory.selected = 0;
                        }
                        Some("parent") => {
                            let directory = picker.directory.as_mut().unwrap();
                            if let Some(parent) = directory.current.parent() {
                                directory.current = parent.to_path_buf();
                                directory.query.clear();
                                directory.reload().await?;
                            }
                        }
                        Some("open") => {
                            let selected = picker
                                .directory
                                .as_ref()
                                .and_then(DirectoryPicker::selected_path);
                            if let Some(selected) = selected {
                                picker.directory = Some(DirectoryPicker::open(selected).await?);
                            }
                        }
                        Some("select") => {
                            let cwd = picker.directory.as_ref().unwrap().current.clone();
                            return Ok(Some(SessionLaunch {
                                cwd,
                                session: SessionChoice::New,
                            }));
                        }
                        Some("fzf") => {
                            let root = picker.directory.as_ref().unwrap().current.clone();
                            drop(events);
                            let selected = pick_directory_with_fzf(&mut terminal, &root).await;
                            events = EventStream::new();
                            match selected {
                                Ok(Some(cwd)) => {
                                    return Ok(Some(SessionLaunch {
                                        cwd,
                                        session: SessionChoice::New,
                                    }));
                                }
                                Ok(None) => picker.flash = Some("fzf cancelled".to_string()),
                                Err(error) => {
                                    picker.flash = Some(format!("fzf unavailable: {error:#}"))
                                }
                            }
                        }
                        _ => {}
                    }
                    continue;
                }
                match picker.keymap.action("sessions", &key_name).as_deref() {
                    Some("quit") => return Ok(None),
                    Some("new") => {
                        picker.directory = Some(DirectoryPicker::open(picker.base.clone()).await?);
                        picker.flash = None;
                    }
                    Some("next") => {
                        picker.selected = picker
                            .selected
                            .saturating_add(1)
                            .min(picker.sessions.len().saturating_sub(1));
                    }
                    Some("previous") => {
                        picker.selected = picker.selected.saturating_sub(1);
                    }
                    Some("first") => picker.selected = 0,
                    Some("last") => {
                        picker.selected = picker.sessions.len().saturating_sub(1);
                    }
                    Some("refresh") => {
                        picker.sessions = Session::list(&picker.base).await?;
                        picker.selected =
                            picker.selected.min(picker.sessions.len().saturating_sub(1));
                    }
                    Some("open") => {
                        if let Some(launch) = selected_session_launch(&mut picker).await? {
                            return Ok(Some(launch));
                        }
                    }
                    _ => {}
                }
            }
            Event::Paste(text)
                if picker
                    .directory
                    .as_ref()
                    .is_some_and(|directory| directory.searching) =>
            {
                let directory = picker.directory.as_mut().unwrap();
                directory.query.push_str(text.trim());
                directory.selected = 0;
            }
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => {
                    if let Some(directory) = picker.directory.as_mut() {
                        directory.selected = directory.selected.saturating_sub(3);
                    } else {
                        picker.selected = picker.selected.saturating_sub(3);
                    }
                }
                MouseEventKind::ScrollDown => {
                    if let Some(directory) = picker.directory.as_mut() {
                        directory.selected = directory
                            .selected
                            .saturating_add(3)
                            .min(directory.visible_len().saturating_sub(1));
                    } else {
                        picker.selected = picker
                            .selected
                            .saturating_add(3)
                            .min(picker.sessions.len().saturating_sub(1));
                    }
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(target) = hit_target(&picker.hit_regions, mouse) {
                        let activate = is_double_click(&mut picker.last_click, target);
                        match target {
                            PickerHit::Session(index) => {
                                picker.selected = index;
                                if activate
                                    && let Some(launch) =
                                        selected_session_launch(&mut picker).await?
                                {
                                    return Ok(Some(launch));
                                }
                            }
                            PickerHit::Directory(index) => {
                                let selected = picker.directory.as_mut().and_then(|directory| {
                                    directory.selected = index;
                                    activate.then(|| directory.selected_path()).flatten()
                                });
                                if let Some(selected) = selected {
                                    picker.directory = Some(DirectoryPicker::open(selected).await?);
                                }
                            }
                            PickerHit::ChooseDirectory => {
                                let cwd = picker.directory.as_ref().unwrap().current.clone();
                                return Ok(Some(SessionLaunch {
                                    cwd,
                                    session: SessionChoice::New,
                                }));
                            }
                        }
                    }
                }
                _ => {}
            },
            Event::FocusGained
            | Event::FocusLost
            | Event::Paste(_)
            | Event::Resize(_, _)
            | Event::Key(_) => {}
        }
    }
}

async fn pick_directory_with_fzf(
    terminal: &mut DefaultTerminal,
    root: &Path,
) -> Result<Option<PathBuf>> {
    Command::new("fzf")
        .arg("--version")
        .output()
        .await
        .context("install fzf to use recursive directory search")?;
    let candidates = directory_candidates(root).await?;
    execute!(stdout(), DisableMouseCapture, DisableBracketedPaste)?;
    ratatui::try_restore()?;
    let result = async {
        let mut child = Command::new("fzf")
            .args(["--read0", "--print0", "--scheme=path", "--prompt=project> "])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("cannot open fzf input"))?
            .write_all(&candidates)
            .await?;
        Ok::<_, anyhow::Error>(child.wait_with_output().await?)
    }
    .await;
    *terminal = ratatui::try_init()?;
    execute!(stdout(), EnableMouseCapture, EnableBracketedPaste)?;
    terminal.clear()?;

    let output = result.context("cannot run fzf directory search")?;
    if !output.status.success() {
        return Ok(None);
    }
    let selected = String::from_utf8(output.stdout)?
        .trim_end_matches('\0')
        .to_string();
    if selected.is_empty() {
        return Ok(None);
    }
    let path = PathBuf::from(selected);
    let path = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };
    Ok(Some(path.canonicalize()?))
}

async fn directory_candidates(root: &Path) -> Result<Vec<u8>> {
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut output = Vec::new();
        let mut pending = vec![root];
        let mut count = 0usize;
        while let Some(directory) = pending.pop() {
            output.extend_from_slice(directory.to_string_lossy().as_bytes());
            output.push(0);
            count += 1;
            if count >= 100_000 {
                break;
            }
            let Ok(entries) = std::fs::read_dir(&directory) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if !file_type.is_dir() {
                    continue;
                }
                let name = entry.file_name();
                if matches!(name.to_str(), Some(".git" | "node_modules" | "target")) {
                    continue;
                }
                pending.push(entry.path());
            }
        }
        output
    })
    .await
    .context("directory search worker failed")
}

pub async fn run(
    runtime: Arc<AgentRuntime>,
    protocols: Vec<ProtocolDescriptor>,
    tasks: TaskManager,
    manager: Arc<ConfigManager>,
    catalog: Arc<ModelCatalog>,
    output: Arc<OutputStore>,
    info: TuiInfo,
) -> Result<TuiExit> {
    let session = runtime.session().clone();
    let mut receiver = session.subscribe();
    let keymap = Keymap::load(Some(&info.cwd)).await?;
    let mut app = App::new(protocols, info, keymap);
    for event in session.snapshot().await {
        app.apply(event);
    }

    let mut terminal = ratatui::try_init()?;
    let _restore = RestoreTerminal;
    execute!(stdout(), EnableMouseCapture, EnableBracketedPaste)?;
    let services = TuiServices {
        runtime,
        tasks,
        manager,
        catalog,
        output,
    };
    run_loop(&mut terminal, &mut app, services, &mut receiver).await
}

struct TuiServices {
    runtime: Arc<AgentRuntime>,
    tasks: TaskManager,
    manager: Arc<ConfigManager>,
    catalog: Arc<ModelCatalog>,
    output: Arc<OutputStore>,
}

async fn run_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    services: TuiServices,
    receiver: &mut tokio::sync::broadcast::Receiver<SessionEvent>,
) -> Result<TuiExit> {
    let mut terminal_events = EventStream::new();
    let mut animation = time::interval(Duration::from_millis(90));
    loop {
        terminal.draw(|frame| render(frame, app))?;
        tokio::select! {
            _ = animation.tick() => app.frame = app.frame.wrapping_add(1),
            event = terminal_events.next() => {
                let Some(event) = event else { return Ok(TuiExit::Quit); };
                match event? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        match handle_key(app, key, &services.tasks, &services.catalog).await {
                            Action::Continue => {}
                            Action::Quit => return Ok(TuiExit::Quit),
                            Action::Sessions => return Ok(TuiExit::Sessions),
                            Action::Submit(prompt) => {
                                let runtime = services.runtime.clone();
                                tokio::spawn(async move {
                                    let _ = runtime.run_turn(prompt).await;
                                });
                            }
                            Action::OpenSettings => {
                                app.settings = Some(SettingsState::load(&services.manager, &services.catalog).await);
                                app.overlay = Some(Overlay::Settings);
                            }
                            Action::SaveSettings => {
                                save_settings(app, &services.runtime, &services.manager, &services.catalog, &services.output).await;
                            }
                            Action::RefreshCatalog => {
                                refresh_catalog(app, &services.runtime, &services.manager, &services.catalog, &services.output).await;
                            }
                            Action::ClearApiKey => {
                                clear_api_key(app, &services.runtime, &services.manager, &services.catalog, &services.output).await;
                            }
                            Action::OpenEditor { content, replace_input } => {
                                drop(terminal_events);
                                let result = open_editor(terminal, app.info.editor.as_deref(), &content, replace_input).await;
                                terminal_events = EventStream::new();
                                match result {
                                    Ok(Some(content)) => {
                                        app.input = TextArea::new(content.split('\n').map(str::to_owned).collect());
                                        style_input(&mut app.input, app.busy);
                                        app.mode = Mode::Insert;
                                    }
                                    Ok(None) => app.flash = Some("Editor closed".to_string()),
                                    Err(error) => app.flash = Some(format!("Editor failed: {error:#}")),
                                }
                            }
                        }
                    }
                    Event::Paste(text) => {
                        if let Some(settings) = app.settings.as_mut()
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
                                EditingSetting::ProviderSearch | EditingSetting::ModelSearch => {
                                    settings.search.push_str(text.trim());
                                }
                            }
                        } else if app.overlay == Some(Overlay::Command) {
                            app.command_line.push_str(text.trim());
                        } else if app.overlay.is_none() && app.mode == Mode::Insert {
                            app.input.insert_str(text);
                        }
                    }
                    Event::Mouse(mouse) => {
                        match handle_mouse(app, mouse, &services.tasks).await {
                            Action::Continue => {}
                            Action::Quit => return Ok(TuiExit::Quit),
                            Action::Sessions => return Ok(TuiExit::Sessions),
                            Action::Submit(prompt) => {
                                let runtime = services.runtime.clone();
                                tokio::spawn(async move {
                                    let _ = runtime.run_turn(prompt).await;
                                });
                            }
                            Action::OpenSettings => {
                                app.settings = Some(SettingsState::load(&services.manager, &services.catalog).await);
                                app.overlay = Some(Overlay::Settings);
                            }
                            Action::SaveSettings => {
                                save_settings(app, &services.runtime, &services.manager, &services.catalog, &services.output).await;
                            }
                            Action::RefreshCatalog => {
                                refresh_catalog(app, &services.runtime, &services.manager, &services.catalog, &services.output).await;
                            }
                            Action::ClearApiKey => {
                                clear_api_key(app, &services.runtime, &services.manager, &services.catalog, &services.output).await;
                            }
                            Action::OpenEditor { content, replace_input } => {
                                drop(terminal_events);
                                let result = open_editor(terminal, app.info.editor.as_deref(), &content, replace_input).await;
                                terminal_events = EventStream::new();
                                match result {
                                    Ok(Some(content)) => {
                                        app.input = TextArea::new(content.split('\n').map(str::to_owned).collect());
                                        style_input(&mut app.input, app.busy);
                                        app.mode = Mode::Insert;
                                    }
                                    Ok(None) => app.flash = Some("Editor closed".to_string()),
                                    Err(error) => app.flash = Some(format!("Editor failed: {error:#}")),
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
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(TuiExit::Quit),
            }
        }
    }
}

enum Action {
    Continue,
    Quit,
    Sessions,
    Submit(String),
    OpenSettings,
    SaveSettings,
    RefreshCatalog,
    ClearApiKey,
    OpenEditor {
        content: String,
        replace_input: bool,
    },
}

fn parse_ui_command(command: &str) -> Option<UiCommand> {
    match command
        .trim()
        .trim_start_matches(':')
        .split_whitespace()
        .next()?
    {
        "compose" | "insert" => Some(UiCommand::Compose),
        "detail" | "open" => Some(UiCommand::Detail),
        "edit" | "editor" => Some(UiCommand::Editor),
        "sessions" => Some(UiCommand::Sessions),
        "tasks" => Some(UiCommand::Tasks),
        "protocols" => Some(UiCommand::Protocols),
        "settings" | "model" | "login" => Some(UiCommand::Settings),
        "help" => Some(UiCommand::Help),
        "quit" | "q" => Some(UiCommand::Quit),
        _ => None,
    }
}

async fn dispatch_ui_command(app: &mut App, command: UiCommand, tasks: &TaskManager) -> Action {
    app.overlay = None;
    match command {
        UiCommand::Compose => {
            app.mode = Mode::Insert;
            app.flash = None;
            Action::Continue
        }
        UiCommand::Detail => {
            if app.selected_block().is_some() {
                app.overlay_scroll = 0;
                app.overlay = Some(Overlay::Detail);
            } else {
                app.flash = Some("No event is selected".to_string());
            }
            Action::Continue
        }
        UiCommand::Editor => {
            if let Some(block) = app.selected_block() {
                Action::OpenEditor {
                    content: block_document(block),
                    replace_input: false,
                }
            } else {
                app.flash = Some("No event is selected".to_string());
                Action::Continue
            }
        }
        UiCommand::Sessions => {
            if app.busy {
                app.flash = Some("Wait for the active turn before switching sessions".to_string());
                Action::Continue
            } else {
                Action::Sessions
            }
        }
        UiCommand::Tasks => {
            app.task_records = tasks.list().await;
            app.selected_task = 0;
            app.overlay_scroll = 0;
            app.overlay = Some(Overlay::Tasks);
            Action::Continue
        }
        UiCommand::Protocols => {
            app.overlay_scroll = 0;
            app.overlay = Some(Overlay::Protocols);
            Action::Continue
        }
        UiCommand::Settings => Action::OpenSettings,
        UiCommand::Help => {
            app.overlay_scroll = 0;
            app.overlay = Some(Overlay::Help);
            Action::Continue
        }
        UiCommand::Quit => Action::Quit,
    }
}

async fn handle_key(
    app: &mut App,
    key: KeyEvent,
    tasks: &TaskManager,
    catalog: &ModelCatalog,
) -> Action {
    let key_name = key_name(key);
    match app.keymap.action_chain(&[], &key_name).as_deref() {
        Some("quit") => return dispatch_ui_command(app, UiCommand::Quit, tasks).await,
        Some("help") => return dispatch_ui_command(app, UiCommand::Help, tasks).await,
        Some("settings") => return dispatch_ui_command(app, UiCommand::Settings, tasks).await,
        Some("protocols") => return dispatch_ui_command(app, UiCommand::Protocols, tasks).await,
        Some("tasks") => return dispatch_ui_command(app, UiCommand::Tasks, tasks).await,
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
                        .min(PALETTE_ITEMS.len().saturating_sub(1));
                }
                Some("confirm") => {
                    let command = PALETTE_ITEMS[app.palette_selected].command;
                    return dispatch_ui_command(app, command, tasks).await;
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
                    if let Some(command) = parse_ui_command(&entered) {
                        return dispatch_ui_command(app, command, tasks).await;
                    }
                    app.flash = Some(format!(
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
            Overlay::Help | Overlay::Protocols => {
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
                            settings.search.clear();
                        }
                        Some("confirm") => {
                            match editing {
                                EditingSetting::ProviderSearch => {
                                    if let Some(index) = settings.provider_search_match() {
                                        settings.provider_index = index;
                                        let provider = settings.provider().to_string();
                                        settings.models = catalog.models(&provider).await;
                                        settings.model_index =
                                            settings.models.len().saturating_sub(1);
                                    } else {
                                        app.flash =
                                            Some("No provider matches that search".to_string());
                                    }
                                }
                                EditingSetting::ModelSearch => {
                                    if let Some(index) = settings.model_search_match() {
                                        settings.model_index = index;
                                    } else {
                                        app.flash =
                                            Some("No model matches that search".to_string());
                                    }
                                }
                                EditingSetting::ApiKey
                                | EditingSetting::OutputLimit
                                | EditingSetting::Editor => {}
                            }
                            settings.editing = None;
                            settings.search.clear();
                        }
                        Some("backspace") => match editing {
                            EditingSetting::ProviderSearch | EditingSetting::ModelSearch => {
                                settings.search.pop();
                            }
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
                        },
                        _ => {
                            if let KeyCode::Char(character) = key.code
                                && !key.modifiers.intersects(
                                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                                )
                            {
                                match editing {
                                    EditingSetting::ProviderSearch
                                    | EditingSetting::ModelSearch => {
                                        settings.search.push(character);
                                    }
                                    EditingSetting::ApiKey => {
                                        settings.api_key.push(character);
                                        settings.api_key_changed = true;
                                    }
                                    EditingSetting::OutputLimit if character.is_ascii_digit() => {
                                        settings.output_limit.push(character);
                                    }
                                    EditingSetting::OutputLimit => {}
                                    EditingSetting::Editor => settings.editor.push(character),
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
                    Some("next") => settings.selected = (settings.selected + 1).min(4),
                    Some("left") if settings.selected == 0 => {
                        settings.cycle_provider(-1, catalog).await;
                    }
                    Some("right") if settings.selected == 0 => {
                        settings.cycle_provider(1, catalog).await;
                    }
                    Some("left") if settings.selected == 1 => settings.cycle_model(-1),
                    Some("right") if settings.selected == 1 => settings.cycle_model(1),
                    Some("edit") => {
                        settings.editing = Some(match settings.selected {
                            0 => EditingSetting::ProviderSearch,
                            1 => EditingSetting::ModelSearch,
                            2 => EditingSetting::ApiKey,
                            3 => EditingSetting::OutputLimit,
                            _ => EditingSetting::Editor,
                        });
                        match settings.selected {
                            0 | 1 => settings.search.clear(),
                            2 => settings.api_key.clear(),
                            3 => settings.output_limit.clear(),
                            _ => settings.editor.clear(),
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
            return dispatch_ui_command(app, UiCommand::Tasks, tasks).await;
        }
        action if app.mode == Mode::Insert => match action {
            Some("browse") => app.mode = Mode::Browse,
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
            Some("send") => {
                if let Some(prompt) = app.submit() {
                    if matches!(
                        prompt.split_whitespace().next(),
                        Some("/settings" | "/model" | "/login")
                    ) {
                        app.busy = false;
                        style_input(&mut app.input, false);
                        return Action::OpenSettings;
                    }
                    return Action::Submit(prompt);
                }
            }
            _ => {
                app.flash = None;
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
        Some("insert") => return dispatch_ui_command(app, UiCommand::Compose, tasks).await,
        Some("next") => app.move_selection(1),
        Some("previous") => app.move_selection(-1),
        Some("page_down") => app.move_selection(10),
        Some("page_up") => app.move_selection(-10),
        Some("first") => app.selected_block = 0,
        Some("last") => app.selected_block = app.blocks.len().saturating_sub(1),
        Some("detail") => return dispatch_ui_command(app, UiCommand::Detail, tasks).await,
        Some("editor") => return dispatch_ui_command(app, UiCommand::Editor, tasks).await,
        Some("sessions") => return dispatch_ui_command(app, UiCommand::Sessions, tasks).await,
        _ => {}
    }
    Action::Continue
}

async fn handle_mouse(app: &mut App, mouse: MouseEvent, tasks: &TaskManager) -> Action {
    match mouse.kind {
        MouseEventKind::ScrollUp => match app.overlay {
            Some(Overlay::Palette) => {
                app.palette_selected = app.palette_selected.saturating_sub(1);
            }
            Some(Overlay::Tasks) => {
                app.selected_task = app.selected_task.saturating_sub(1);
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
                    .min(PALETTE_ITEMS.len().saturating_sub(1));
            }
            Some(Overlay::Tasks) => {
                app.selected_task = app
                    .selected_task
                    .saturating_add(1)
                    .min(app.task_records.len().saturating_sub(1));
            }
            Some(Overlay::Settings) => {
                if let Some(settings) = app.settings.as_mut() {
                    settings.selected = settings.selected.saturating_add(1).min(4);
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
                        return dispatch_ui_command(app, UiCommand::Detail, tasks).await;
                    }
                }
                AppHit::Composer => {
                    return dispatch_ui_command(app, UiCommand::Compose, tasks).await;
                }
                AppHit::Palette(index) => {
                    app.palette_selected = index;
                    return dispatch_ui_command(app, PALETTE_ITEMS[index].command, tasks).await;
                }
                AppHit::Task(index) => app.selected_task = index,
                AppHit::Setting(index) => {
                    if let Some(settings) = app.settings.as_mut() {
                        settings.selected = index;
                    }
                }
            }
        }
        _ => {}
    }
    Action::Continue
}

fn shifted(index: usize, length: usize, direction: isize) -> usize {
    if length == 0 {
        return 0;
    }
    if direction < 0 {
        index.checked_sub(1).unwrap_or(length - 1)
    } else {
        (index + 1) % length
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
    let output_limit = match settings.output_limit.parse::<usize>() {
        Ok(limit) if limit >= 1024 => limit,
        Ok(_) => {
            app.flash = Some("Output limit must be at least 1024 bytes".to_string());
            return;
        }
        Err(error) => {
            app.flash = Some(format!("Output limit is invalid: {error}"));
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
        manager.set_editor(Some(editor)).await?;
        if let Some((provider, model)) = selection {
            manager.set_model(&provider, &model).await?;
        }
        let active = manager.current().await;
        apply_active(app, runtime, catalog, output, &active).await?;
        app.settings = Some(SettingsState::load(manager, catalog).await);
        Ok::<_, anyhow::Error>(())
    }
    .await;
    app.flash = Some(match result {
        Ok(()) => "Settings saved and applied".to_string(),
        Err(error) => format!("Settings were not fully applied: {error:#}"),
    });
}

async fn refresh_catalog(
    app: &mut App,
    runtime: &AgentRuntime,
    manager: &ConfigManager,
    catalog: &ModelCatalog,
    output: &OutputStore,
) {
    let result = async {
        catalog.refresh(true).await?;
        let active = manager.reload().await?;
        apply_active(app, runtime, catalog, output, &active).await?;
        app.settings = Some(SettingsState::load(manager, catalog).await);
        Ok::<_, anyhow::Error>(())
    }
    .await;
    app.flash = Some(match result {
        Ok(()) => "Pi model catalog refreshed".to_string(),
        Err(error) => format!("Catalog refresh failed: {error:#}"),
    });
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
    app.flash = Some(match result {
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
    runtime.set_backend(backend).await;
    runtime
        .session()
        .update_model(&active.provider, &active.model)
        .await?;
    output.set_limit(active.output_limit);
    app.info.provider.clone_from(&active.provider);
    app.info.model.clone_from(&active.model);
    app.info.editor.clone_from(&active.editor);
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
    let directory = env::temp_dir().join("uri-agent");
    fs::create_dir_all(&directory).await?;
    let path = directory.join(format!("view-{}.md", Uuid::now_v7().simple()));
    fs::write(&path, content).await?;
    arguments.push(path.to_string_lossy().into_owned());

    execute!(stdout(), DisableMouseCapture, DisableBracketedPaste)?;
    ratatui::try_restore()?;
    let editor_result = Command::new(&executable).args(arguments).status().await;
    *terminal = ratatui::try_init()?;
    execute!(stdout(), EnableMouseCapture, EnableBracketedPaste)?;
    terminal.clear()?;

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

fn render_session_picker(frame: &mut Frame<'_>, picker: &mut SessionPicker) {
    picker.hit_regions.clear();
    let area = frame.area();
    frame.render_widget(Block::new().style(Style::default().bg(BG)), area);
    let outer = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(8),
        Constraint::Length(2),
    ])
    .margin(1)
    .split(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " URI/AGENT ",
                Style::default()
                    .fg(BG)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("  SESSIONS", Style::default().fg(MUTED)),
        ])),
        outer[0],
    );
    let columns = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(outer[1]);
    let directory_mode = picker.directory.is_some();
    let (title, items, selected, item_count) = if let Some(directory) = &picker.directory {
        let items = directory
            .visible()
            .into_iter()
            .enumerate()
            .map(|(index, path)| {
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("?");
                ListItem::new(format!(
                    "{} {name}/",
                    if index == directory.selected {
                        "›"
                    } else {
                        " "
                    }
                ))
                .style(Style::default().fg(if index == directory.selected {
                    ACCENT
                } else {
                    TEXT
                }))
            })
            .collect::<Vec<_>>();
        (
            format!(
                " DIRECTORIES · {}/{} select · {} descend ",
                key_hint(&picker.keymap, "directory", "previous"),
                key_hint(&picker.keymap, "directory", "next"),
                key_hint(&picker.keymap, "directory", "open")
            ),
            if items.is_empty() {
                vec![ListItem::new("No matching child directories.")]
            } else {
                items
            },
            (!directory.visible().is_empty()).then_some(directory.selected),
            directory.visible_len(),
        )
    } else {
        let items = if picker.sessions.is_empty() {
            vec![ListItem::new(
                "No sessions yet. Press n to choose a project.",
            )]
        } else {
            picker
                .sessions
                .iter()
                .enumerate()
                .map(|(index, session)| {
                    let marker = if index == picker.selected { "›" } else { " " };
                    ListItem::new(vec![
                        Line::styled(
                            format!("{marker} {}", session.cwd.display()),
                            Style::default()
                                .fg(if index == picker.selected {
                                    ACCENT
                                } else {
                                    TEXT
                                })
                                .add_modifier(Modifier::BOLD),
                        ),
                        Line::styled(
                            format!(
                                "  {} · {}/{}",
                                session.updated_at.format("%Y-%m-%d %H:%M"),
                                session.provider,
                                session.model
                            ),
                            Style::default().fg(MUTED),
                        ),
                    ])
                })
                .collect()
        };
        (
            format!(
                " RECENT · {}/{} select · {} open ",
                key_hint(&picker.keymap, "sessions", "previous"),
                key_hint(&picker.keymap, "sessions", "next"),
                key_hint(&picker.keymap, "sessions", "open")
            ),
            items,
            (!picker.sessions.is_empty()).then_some(picker.selected),
            picker.sessions.len(),
        )
    };
    let list_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .title(title)
        .padding(Padding::horizontal(1));
    let list_inner = list_block.inner(columns[0]);
    let mut state = ListState::default().with_selected(selected);
    frame.render_stateful_widget(List::new(items).block(list_block), columns[0], &mut state);
    let item_height = if directory_mode { 1 } else { 2 };
    for index in state.offset()..item_count {
        let y = list_inner
            .y
            .saturating_add(((index - state.offset()) * item_height) as u16);
        if y.saturating_add(item_height as u16) > list_inner.y.saturating_add(list_inner.height) {
            break;
        }
        picker.hit_regions.push(HitRegion {
            area: Rect::new(list_inner.x, y, list_inner.width, item_height as u16),
            target: if directory_mode {
                PickerHit::Directory(index)
            } else {
                PickerHit::Session(index)
            },
        });
    }

    let detail = if let Some(directory) = &picker.directory {
        format!(
            "PROJECT DIRECTORY\n{}\n\nFILTER\n{}{}\n\n{} descend\n{} parent\n{} choose current\n{} filter this level\n{} recursive fzf search\n{} sessions",
            directory.current.display(),
            directory.query,
            if directory.searching { "█" } else { "" },
            key_hint(&picker.keymap, "directory", "open"),
            key_hint(&picker.keymap, "directory", "parent"),
            key_hint(&picker.keymap, "directory", "select"),
            key_hint(&picker.keymap, "directory", "search"),
            key_hint(&picker.keymap, "directory", "fzf"),
            key_hint(&picker.keymap, "directory", "cancel")
        )
    } else if let Some(session) = picker.sessions.get(picker.selected) {
        format!(
            "PROJECT\n{}\n\nSESSION\n{}\n\nCREATED\n{}\n\nMODEL\n{} / {}\n\n{} choose project · {} refresh · {} quit",
            session.cwd.display(),
            session.id,
            session.created_at.format("%Y-%m-%d %H:%M"),
            session.provider,
            session.model,
            key_hint(&picker.keymap, "sessions", "new"),
            key_hint(&picker.keymap, "sessions", "refresh"),
            key_hint(&picker.keymap, "sessions", "quit")
        )
    } else {
        format!(
            "NEW WORKSPACE\n\nPress {} to browse for a project directory.\n\nThe application may be launched from any directory, including {}.",
            key_hint(&picker.keymap, "sessions", "new"),
            picker.base.display()
        )
    };
    frame.render_widget(
        Paragraph::new(detail)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(MUTED))
                    .padding(Padding::uniform(1)),
            )
            .style(Style::default().fg(TEXT))
            .wrap(Wrap { trim: false }),
        columns[1],
    );
    if directory_mode {
        picker.hit_regions.push(HitRegion {
            area: Rect::new(
                columns[1].x.saturating_add(2),
                columns[1].y.saturating_add(10),
                columns[1].width.saturating_sub(4),
                1,
            ),
            target: PickerHit::ChooseDirectory,
        });
    }
    frame.render_widget(
        Paragraph::new(
            picker
                .flash
                .as_deref()
                .unwrap_or(if picker.directory.is_some() {
                    "DIRECTORY  ·  choose current  ·  filter  ·  recursive fzf  ·  cancel"
                } else {
                    "SESSIONS  ·  choose project  ·  open session  ·  quit"
                }),
        )
        .alignment(Alignment::Center)
        .style(Style::default().fg(if picker.flash.is_some() { ERROR } else { MUTED })),
        outer[2],
    );
}

fn render(frame: &mut Frame<'_>, app: &mut App) {
    app.hit_regions.clear();
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
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(input_height),
            Constraint::Length(1),
        ])
        .split(area);
    render_header(frame, app, areas[0]);
    render_transcript(frame, app, areas[1]);
    if app.mode == Mode::Insert {
        frame.render_widget(&app.input, areas[2]);
    } else {
        let insert = key_hint(&app.keymap, "browse", "insert");
        let detail = key_hint(&app.keymap, "browse", "detail");
        let editor = key_hint(&app.keymap, "browse", "editor");
        frame.render_widget(
            Paragraph::new(format!(
                " BROWSE  ·  {insert} compose  ·  {detail} detail  ·  {editor} editor  ·  Space commands  ·  : command"
            ))
            .style(Style::default().fg(MUTED).bg(SURFACE)),
            areas[2],
        );
        app.hit_regions.push(HitRegion {
            area: areas[2],
            target: AppHit::Composer,
        });
    }
    render_footer(frame, app, areas[3]);
    if let Some(overlay) = app.overlay {
        app.hit_regions.clear();
        render_overlay(frame, app, overlay);
    }
}

fn render_header(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(12), Constraint::Length(24)])
        .split(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " URI/AGENT ",
                Style::default()
                    .fg(BG)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {} / {}", app.info.provider, app.info.model),
                Style::default().fg(MUTED),
            ),
        ]))
        .style(Style::default().bg(BG)),
        columns[0],
    );
    let mode = if app.mode == Mode::Insert {
        "INSERT"
    } else {
        "BROWSE"
    };
    let state = if app.busy {
        format!("{mode}  {} working", dither(app.frame))
    } else {
        format!("{mode}  · ready")
    };
    frame.render_widget(
        Paragraph::new(state).alignment(Alignment::Right).style(
            Style::default()
                .fg(if app.busy { ACCENT } else { MUTED })
                .bg(BG),
        ),
        columns[1],
    );
}

fn render_transcript(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    if app.blocks.is_empty() {
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(
                    "A small surface for capable tools.",
                    Style::default().fg(TEXT),
                ),
                Line::styled(
                    "Press i to compose, Space for commands, or click this area. F1 shows all keys.",
                    Style::default().fg(MUTED),
                ),
            ])
            .block(Block::new().padding(Padding::uniform(2))),
            area,
        );
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
                BlockKind::Notice => MUTED,
                BlockKind::Error => ERROR,
            }
        };
        let status = if block.kind == BlockKind::Tool {
            if block.failed {
                "×"
            } else if block.text.contains("\n\nRESULT\n") {
                "✓"
            } else {
                "·"
            }
        } else {
            " "
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

fn render_footer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let fallback = if app.mode == Mode::Insert {
        format!(
            "INSERT  ·  {} send  ·  {} newline  ·  {} editor  ·  {} browse",
            key_hint(&app.keymap, "insert", "send"),
            key_hint(&app.keymap, "insert", "newline"),
            key_hint(&app.keymap, "insert", "editor"),
            key_hint(&app.keymap, "insert", "browse")
        )
    } else if area.width > 90 {
        format!(
            "BROWSE  ·  {}/{} select  ·  {} detail  ·  {} compose  ·  Space commands  ·  : command  ·  {} sessions",
            key_hint(&app.keymap, "browse", "previous"),
            key_hint(&app.keymap, "browse", "next"),
            key_hint(&app.keymap, "browse", "detail"),
            key_hint(&app.keymap, "browse", "insert"),
            key_hint(&app.keymap, "browse", "sessions")
        )
    } else {
        format!(
            "BROWSE  ·  {}/{} select  ·  {} detail  ·  {} compose",
            key_hint(&app.keymap, "browse", "previous"),
            key_hint(&app.keymap, "browse", "next"),
            key_hint(&app.keymap, "browse", "detail"),
            key_hint(&app.keymap, "browse", "insert")
        )
    };
    let text = app.flash.as_deref().unwrap_or(&fallback);
    frame.render_widget(
        Paragraph::new(text)
            .alignment(Alignment::Center)
            .style(Style::default().fg(MUTED).bg(BG)),
        area,
    );
}

fn key_hint(keymap: &Keymap, mode: &str, action: &str) -> String {
    keymap
        .key_for(mode, action)
        .map(|key| match key.as_str() {
            "up" => "↑".to_string(),
            "down" => "↓".to_string(),
            "left" => "←".to_string(),
            "right" => "→".to_string(),
            "pageup" => "PgUp".to_string(),
            "pagedown" => "PgDn".to_string(),
            _ => key,
        })
        .unwrap_or_else(|| format!("[{action}]"))
}

fn keymap_help(keymap: &Keymap) -> String {
    let mut output = String::new();
    for (title, mode) in [
        ("SESSIONS", "sessions"),
        ("DIRECTORY", "directory"),
        ("BROWSE", "browse"),
        ("INSERT", "insert"),
        ("DETAIL", "detail"),
        ("LIST PANELS", "list"),
        ("TASKS", "tasks"),
        ("SETTINGS", "settings"),
        ("COMMAND PANEL", "palette"),
        ("COMMAND LINE", "command"),
        ("TEXT FIELDS", "text"),
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

fn render_overlay(frame: &mut Frame<'_>, app: &mut App, overlay: Overlay) {
    let area = if overlay == Overlay::Command {
        Rect::new(
            1,
            frame.area().height.saturating_sub(6),
            frame.area().width.saturating_sub(2),
            5,
        )
    } else {
        centered(frame.area(), 78, 72)
    };
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
                "ACTIVE KEYMAP\n\n{}COMMANDS\n  :settings · :model · :login · :sessions · :tasks\n  :protocols · :compose · :detail · :editor · :help · :quit\n\nSlash commands remain available while composing.\n\nSESSION\n{}\n{}\n\nPROJECT\n{}",
                keymap_help(&app.keymap),
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
    }
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
        .unwrap_or_else(|| "(no runnable model in this provider)".to_string());
    let provider = if settings.editing == Some(EditingSetting::ProviderSearch) {
        let candidate = settings
            .provider_search_match()
            .and_then(|index| settings.providers.get(index))
            .map(String::as_str)
            .unwrap_or("no match");
        format!("search: {}█  →  {candidate}", settings.search)
    } else {
        format!("‹  {provider}  ›")
    };
    let model = if settings.editing == Some(EditingSetting::ModelSearch) {
        let candidate = settings
            .model_search_match()
            .and_then(|index| settings.models.get(index))
            .map(|model| model.id.as_str())
            .unwrap_or("no match");
        format!("search: {}█  →  {candidate}", settings.search)
    } else {
        format!("‹  {model}  ›")
    };
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
    let rows = [
        ("Provider", provider),
        ("Model", model),
        ("API key", key),
        ("Output limit", output_limit),
        ("Editor", editor),
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
                "provider {}  ·  model {}  ·  limit {}  ·  editor {}",
                settings.active.provider_source.label(),
                settings.active.model_source.label(),
                settings.active.output_limit_source.label(),
                settings.active.editor_source.label()
            ),
            Style::default().fg(MUTED),
        ),
        Line::styled(
            "Environment variables and command-line values override saved text files.",
            Style::default().fg(MUTED),
        ),
        Line::default(),
        Line::styled(
            "↑/↓ field  ·  ←/→ choose  ·  Enter edit  ·  x clear key  ·  s save  ·  r refresh",
            Style::default().fg(ACCENT),
        ),
    ]);
    if let Some(flash) = &app.flash {
        lines.extend([
            Line::default(),
            Line::styled(flash.clone(), Style::default().fg(WARM)),
        ]);
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(block.title(" SETTINGS · Esc close "))
            .wrap(Wrap { trim: false }),
        area,
    );
    for index in 0..5 {
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
    let items = PALETTE_ITEMS.iter().enumerate().map(|(index, item)| {
        let selected = index == app.palette_selected;
        ListItem::new(Line::from(vec![
            Span::styled(
                if selected { "› " } else { "  " },
                Style::default().fg(ACCENT),
            ),
            Span::styled(
                format!("{:<24}", item.name),
                Style::default()
                    .fg(if selected { ACCENT } else { TEXT })
                    .add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            Span::styled(item.description, Style::default().fg(MUTED)),
        ]))
        .style(Style::default().bg(if selected { BG } else { SURFACE }))
    });
    let mut state = ListState::default().with_selected(Some(app.palette_selected));
    frame.render_stateful_widget(List::new(items), inner, &mut state);
    for index in state.offset()..PALETTE_ITEMS.len() {
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
            .title(if busy { " working " } else { " message " })
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

fn dither(frame: usize) -> &'static str {
    const FRAMES: [&str; 12] = [
        "▪   ",
        " ▪  ",
        "  ▪ ",
        "   ▪",
        "  ▪▪",
        " ▪▪▪",
        "▪▪▪▪",
        "▪▪▪ ",
        "▪▪  ",
        "▪   ",
        "▪ ▪ ",
        " ▪ ▪",
    ];
    FRAMES[frame % FRAMES.len()]
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
            TuiInfo {
                cwd: PathBuf::from("/workspace"),
                provider: "test".to_string(),
                model: "model".to_string(),
                session_id: "session".to_string(),
                editor: None,
            },
            Keymap::default(),
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
            provider_source: ValueSource::Global,
            model_source: ValueSource::Global,
            api_key_source: ValueSource::Global,
            output_limit_source: ValueSource::Global,
            editor_source: ValueSource::Global,
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
            TuiInfo {
                cwd: PathBuf::from("/workspace"),
                provider: active.provider.clone(),
                model: active.model.clone(),
                session_id: "session".to_string(),
                editor: active.editor.clone(),
            },
            Keymap::default(),
        );
        app.overlay = Some(Overlay::Settings);
        app.settings = Some(SettingsState {
            active,
            providers: vec!["openai".to_string()],
            provider_index: 0,
            models: vec![model],
            model_index: 0,
            selected: 0,
            editing: None,
            api_key: String::new(),
            api_key_changed: false,
            output_limit: "32768".to_string(),
            editor: "nvim -f".to_string(),
            search: String::new(),
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
        assert_eq!(parse_ui_command(":settings"), Some(UiCommand::Settings));
        assert_eq!(parse_ui_command("model"), Some(UiCommand::Settings));
        assert_eq!(parse_ui_command("compose"), Some(UiCommand::Compose));
        assert_eq!(parse_ui_command("editor"), Some(UiCommand::Editor));
        assert_eq!(parse_ui_command("q"), Some(UiCommand::Quit));
        assert_eq!(parse_ui_command("unknown"), None);
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
        assert!(rendered.contains("Open in Helix"));
        assert_eq!(
            app.hit_regions
                .iter()
                .filter(|region| matches!(region.target, AppHit::Palette(_)))
                .count(),
            PALETTE_ITEMS.len()
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

    #[tokio::test]
    async fn directory_browser_filters_folders_without_path_entry() {
        let temporary = tempfile::tempdir().unwrap();
        tokio::fs::create_dir(temporary.path().join("alpha"))
            .await
            .unwrap();
        tokio::fs::create_dir(temporary.path().join("beta"))
            .await
            .unwrap();
        tokio::fs::write(temporary.path().join("not-a-directory"), b"file")
            .await
            .unwrap();
        let mut picker = DirectoryPicker::open(temporary.path().to_path_buf())
            .await
            .unwrap();

        assert_eq!(picker.visible_len(), 2);
        picker.query = "bet".to_string();
        assert_eq!(
            picker
                .selected_path()
                .unwrap()
                .file_name()
                .unwrap()
                .to_str(),
            Some("beta")
        );
    }
}
