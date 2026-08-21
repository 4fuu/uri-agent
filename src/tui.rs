use crate::catalog::{CatalogModel, ModelCatalog};
use crate::config::{ActiveSettings, ConfigManager};
use crate::model::configured_backend;
use crate::output::OutputStore;
use crate::protocol::ProtocolDescriptor;
use crate::runtime::AgentRuntime;
use crate::session::{EventKind, SessionEvent};
use crate::task::{TaskManager, TaskRecord, TaskStatus};
use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
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
use std::io::stdout;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::time;
use tui_textarea::TextArea;

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
}

#[derive(Clone, Copy)]
enum Overlay {
    Help,
    Protocols,
    Tasks,
    Settings,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum EditingSetting {
    ProviderSearch,
    ModelSearch,
    ApiKey,
    OutputLimit,
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
    overlay: Option<Overlay>,
    overlay_scroll: u16,
    busy: bool,
    scroll_from_bottom: usize,
    frame: usize,
    last_sequence: Option<u64>,
    info: TuiInfo,
    flash: Option<String>,
    settings: Option<SettingsState>,
}

impl App {
    fn new(protocols: Vec<ProtocolDescriptor>, info: TuiInfo) -> Self {
        let mut input = TextArea::default();
        style_input(&mut input, false);
        Self {
            input,
            blocks: Vec::new(),
            protocols,
            tasks: HashMap::new(),
            task_records: Vec::new(),
            selected_task: 0,
            overlay: None,
            overlay_scroll: 0,
            busy: false,
            scroll_from_bottom: 0,
            frame: 0,
            last_sequence: None,
            info,
            flash: None,
            settings: None,
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
        match event.kind {
            EventKind::SessionCreated { .. } | EventKind::ModelMessage { .. } => {}
            EventKind::User { text } => {
                self.busy = true;
                self.push(BlockKind::User, "YOU", text);
            }
            EventKind::AssistantText { text } => {
                self.append_or_push(BlockKind::Assistant, "AGENT", text);
            }
            EventKind::AssistantReasoning { text } => {
                self.append_or_push(BlockKind::Reasoning, "THINKING", text);
            }
            EventKind::ToolCall {
                name, arguments, ..
            } => {
                let text = serde_json::to_string_pretty(&arguments)
                    .unwrap_or_else(|_| arguments.to_string());
                self.push(BlockKind::Tool, &format!("{name} ↗"), text);
            }
            EventKind::ToolResult {
                name,
                output,
                failed,
                ..
            } => {
                self.push(
                    if failed {
                        BlockKind::Error
                    } else {
                        BlockKind::Tool
                    },
                    &format!("{name} ↙"),
                    output,
                );
            }
            EventKind::Task {
                id, label, status, ..
            } => {
                self.tasks.insert(id, (label, status));
            }
            EventKind::Notice { text } => self.push(BlockKind::Notice, "SYSTEM", text),
            EventKind::Error { text } => {
                self.busy = false;
                self.push(BlockKind::Error, "ERROR", text);
            }
            EventKind::TurnFinished => self.busy = false,
        }
        self.scroll_from_bottom = 0;
        style_input(&mut self.input, self.busy);
    }

    fn push(&mut self, kind: BlockKind, title: &str, text: String) {
        self.blocks.push(DisplayBlock {
            kind,
            title: title.to_string(),
            text,
        });
    }

    fn append_or_push(&mut self, kind: BlockKind, title: &str, text: String) {
        if let Some(block) = self.blocks.last_mut().filter(|block| block.kind == kind) {
            block.text.push_str(&text);
        } else {
            self.push(kind, title, text);
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
        Some(text)
    }

    fn transcript_lines(&self) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for block in &self.blocks {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            let (color, modifier) = match block.kind {
                BlockKind::User => (ACCENT, Modifier::BOLD),
                BlockKind::Assistant => (TEXT, Modifier::BOLD),
                BlockKind::Reasoning => (MUTED, Modifier::ITALIC),
                BlockKind::Tool => (WARM, Modifier::BOLD),
                BlockKind::Notice => (MUTED, Modifier::BOLD),
                BlockKind::Error => (ERROR, Modifier::BOLD),
            };
            lines.push(Line::from(Span::styled(
                block.title.clone(),
                Style::default().fg(color).add_modifier(modifier),
            )));
            let body_style = if block.kind == BlockKind::Reasoning {
                Style::default().fg(MUTED).add_modifier(Modifier::ITALIC)
            } else {
                Style::default().fg(TEXT)
            };
            if block.text.is_empty() {
                lines.push(Line::default());
            } else {
                lines.extend(
                    block
                        .text
                        .lines()
                        .map(|line| Line::styled(line.to_string(), body_style)),
                );
            }
        }
        if lines.is_empty() {
            lines.extend([
                Line::styled(
                    "A small surface for capable tools.",
                    Style::default().fg(TEXT),
                ),
                Line::styled(
                    "Describe the outcome you want. F1 shows controls.",
                    Style::default().fg(MUTED),
                ),
            ]);
        }
        lines
    }
}

pub async fn run(
    runtime: Arc<AgentRuntime>,
    protocols: Vec<ProtocolDescriptor>,
    tasks: TaskManager,
    manager: Arc<ConfigManager>,
    catalog: Arc<ModelCatalog>,
    output: Arc<OutputStore>,
    info: TuiInfo,
) -> Result<()> {
    let session = runtime.session().clone();
    let mut receiver = session.subscribe();
    let mut app = App::new(protocols, info);
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
) -> Result<()> {
    let mut terminal_events = EventStream::new();
    let mut animation = time::interval(Duration::from_millis(90));
    loop {
        terminal.draw(|frame| render(frame, app))?;
        tokio::select! {
            _ = animation.tick() => app.frame = app.frame.wrapping_add(1),
            event = terminal_events.next() => {
                let Some(event) = event else { return Ok(()); };
                match event? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        match handle_key(app, key, &services.tasks, &services.catalog).await {
                            Action::Continue => {}
                            Action::Quit => return Ok(()),
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
                                EditingSetting::ProviderSearch | EditingSetting::ModelSearch => {
                                    settings.search.push_str(text.trim());
                                }
                            }
                        } else if app.overlay.is_none() {
                            app.input.insert_str(text);
                        }
                    }
                    Event::Mouse(mouse) => match mouse.kind {
                        MouseEventKind::ScrollUp => {
                            if app.overlay.is_some() {
                                app.overlay_scroll = app.overlay_scroll.saturating_sub(3);
                            } else {
                                app.scroll_from_bottom = app.scroll_from_bottom.saturating_add(4);
                            }
                        }
                        MouseEventKind::ScrollDown => {
                            if app.overlay.is_some() {
                                app.overlay_scroll = app.overlay_scroll.saturating_add(3);
                            } else {
                                app.scroll_from_bottom = app.scroll_from_bottom.saturating_sub(4);
                            }
                        }
                        _ => {}
                    },
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
            }
        }
    }
}

enum Action {
    Continue,
    Quit,
    Submit(String),
    OpenSettings,
    SaveSettings,
    RefreshCatalog,
    ClearApiKey,
}

async fn handle_key(
    app: &mut App,
    key: KeyEvent,
    tasks: &TaskManager,
    catalog: &ModelCatalog,
) -> Action {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Action::Quit;
    }
    if key.code == KeyCode::Esc {
        if let Some(settings) = app.settings.as_mut()
            && settings.editing.take().is_some()
        {
            return Action::Continue;
        }
        app.overlay = None;
        return Action::Continue;
    }
    if let Some(overlay) = app.overlay {
        match overlay {
            Overlay::Tasks => match key.code {
                KeyCode::Up => app.selected_task = app.selected_task.saturating_sub(1),
                KeyCode::Down => {
                    app.selected_task = app
                        .selected_task
                        .saturating_add(1)
                        .min(app.task_records.len().saturating_sub(1));
                }
                KeyCode::Char('x') => {
                    if let Some(id) = app
                        .task_records
                        .get(app.selected_task)
                        .map(|task| task.id.clone())
                    {
                        let _ = tasks.cancel(&id).await;
                        app.task_records = tasks.list().await;
                    }
                }
                KeyCode::PageUp => app.overlay_scroll = app.overlay_scroll.saturating_sub(8),
                KeyCode::PageDown => app.overlay_scroll = app.overlay_scroll.saturating_add(8),
                _ => {}
            },
            Overlay::Help | Overlay::Protocols => match key.code {
                KeyCode::Up => app.overlay_scroll = app.overlay_scroll.saturating_sub(1),
                KeyCode::Down => app.overlay_scroll = app.overlay_scroll.saturating_add(1),
                KeyCode::PageUp => app.overlay_scroll = app.overlay_scroll.saturating_sub(8),
                KeyCode::PageDown => app.overlay_scroll = app.overlay_scroll.saturating_add(8),
                _ => {}
            },
            Overlay::Settings => {
                let Some(settings) = app.settings.as_mut() else {
                    app.overlay = None;
                    return Action::Continue;
                };
                if let Some(editing) = settings.editing {
                    match key.code {
                        KeyCode::Enter => {
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
                                EditingSetting::ApiKey | EditingSetting::OutputLimit => {}
                            }
                            settings.editing = None;
                            settings.search.clear();
                        }
                        KeyCode::Backspace => match editing {
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
                        },
                        KeyCode::Char(character)
                            if !key.modifiers.intersects(
                                KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                            ) =>
                        {
                            match editing {
                                EditingSetting::ProviderSearch | EditingSetting::ModelSearch => {
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
                            }
                        }
                        _ => {}
                    }
                    return Action::Continue;
                }
                match key.code {
                    KeyCode::Up => settings.selected = settings.selected.saturating_sub(1),
                    KeyCode::Down => settings.selected = (settings.selected + 1).min(3),
                    KeyCode::Left if settings.selected == 0 => {
                        settings.cycle_provider(-1, catalog).await;
                    }
                    KeyCode::Right if settings.selected == 0 => {
                        settings.cycle_provider(1, catalog).await;
                    }
                    KeyCode::Left if settings.selected == 1 => settings.cycle_model(-1),
                    KeyCode::Right if settings.selected == 1 => settings.cycle_model(1),
                    KeyCode::Enter if settings.selected == 0 => {
                        settings.editing = Some(EditingSetting::ProviderSearch);
                        settings.search.clear();
                    }
                    KeyCode::Enter if settings.selected == 1 => {
                        settings.editing = Some(EditingSetting::ModelSearch);
                        settings.search.clear();
                    }
                    KeyCode::Enter if settings.selected == 2 => {
                        settings.editing = Some(EditingSetting::ApiKey);
                        settings.api_key.clear();
                    }
                    KeyCode::Enter if settings.selected == 3 => {
                        settings.editing = Some(EditingSetting::OutputLimit);
                        settings.output_limit.clear();
                    }
                    KeyCode::Char('s') => return Action::SaveSettings,
                    KeyCode::Char('r') => return Action::RefreshCatalog,
                    KeyCode::Char('x') if settings.selected == 2 => {
                        return Action::ClearApiKey;
                    }
                    _ => {}
                }
            }
        }
        return Action::Continue;
    }
    match key.code {
        KeyCode::F(1) => {
            app.overlay_scroll = 0;
            app.overlay = Some(Overlay::Help);
        }
        KeyCode::F(2) => return Action::OpenSettings,
        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.overlay_scroll = 0;
            app.overlay = Some(Overlay::Protocols);
        }
        KeyCode::Char('t') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.task_records = tasks.list().await;
            app.selected_task = 0;
            app.overlay_scroll = 0;
            app.overlay = Some(Overlay::Tasks);
        }
        KeyCode::Char(',') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            return Action::OpenSettings;
        }
        KeyCode::PageUp => {
            app.scroll_from_bottom = app.scroll_from_bottom.saturating_add(12);
        }
        KeyCode::PageDown => {
            app.scroll_from_bottom = app.scroll_from_bottom.saturating_sub(12);
        }
        KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.scroll_from_bottom = 0;
        }
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
            app.input.insert_newline();
        }
        KeyCode::Enter => {
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
        KeyCode::Char('d')
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && app.input.lines().iter().all(|line| line.is_empty()) =>
        {
            return Action::Quit;
        }
        _ => {
            app.flash = None;
            app.input.input(key);
        }
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
    output.set_limit(active.output_limit);
    app.info.provider.clone_from(&active.provider);
    app.info.model.clone_from(&active.model);
    Ok(())
}

fn render(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    frame.render_widget(Block::new().style(Style::default().bg(BG)), area);
    let input_height = (app.input.lines().len() as u16).clamp(1, 6) + 2;
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
    frame.render_widget(&app.input, areas[2]);
    render_footer(frame, app, areas[3]);
    if let Some(overlay) = app.overlay {
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
    let state = if app.busy {
        format!("{}  working", dither(app.frame))
    } else {
        "·  ready".to_string()
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

fn render_transcript(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let lines = app.transcript_lines();
    let width = area.width.saturating_sub(4).max(1) as usize;
    let visual_lines = lines
        .iter()
        .map(|line| line.width().max(1).div_ceil(width))
        .sum::<usize>();
    let visible = area.height.saturating_sub(1) as usize;
    let max_scroll = visual_lines.saturating_sub(visible);
    let scroll = max_scroll.saturating_sub(app.scroll_from_bottom.min(max_scroll));
    let paragraph = Paragraph::new(lines)
        .block(
            Block::new()
                .padding(Padding::horizontal(2))
                .style(Style::default().bg(BG)),
        )
        .wrap(Wrap { trim: false })
        .scroll((scroll.min(u16::MAX as usize) as u16, 0));
    frame.render_widget(paragraph, area);
}

fn render_footer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let text = app.flash.as_deref().unwrap_or(if area.width > 80 {
        "Enter send  ·  Shift+Enter newline  ·  F2 settings  ·  F1 help  ·  Ctrl+P protocols"
    } else {
        "Enter send  ·  F2 settings  ·  F1 help"
    });
    frame.render_widget(
        Paragraph::new(text)
            .alignment(Alignment::Center)
            .style(Style::default().fg(MUTED).bg(BG)),
        area,
    );
}

fn render_overlay(frame: &mut Frame<'_>, app: &App, overlay: Overlay) {
    let area = centered(frame.area(), 78, 72);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(SURFACE).fg(TEXT))
        .padding(Padding::uniform(1));
    match overlay {
        Overlay::Help => {
            let text = format!(
                "KEYS\n\nEnter          send\nShift+Enter    newline\nF2 / Ctrl+,    settings and credentials\nPageUp/Down    scroll transcript\nCtrl+P         protocol index\nCtrl+T         managed tasks\nEsc            close this window\nCtrl+C         quit\n\nCOMMANDS\n\n/settings      open settings\n/model         choose provider and model\n/login         configure a credential\n\nSESSION\n\n{}\n{}\n\nWorking directory\n{}",
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
    }
}

fn render_settings(frame: &mut Frame<'_>, app: &App, area: Rect, block: Block<'_>) {
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
    let rows = [
        ("Provider", provider),
        ("Model", model),
        ("API key", key),
        ("Output limit", output_limit),
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
                "provider {}  ·  model {}  ·  limit {}",
                settings.active.provider_source.label(),
                settings.active.model_source.label(),
                settings.active.output_limit_source.label()
            ),
            Style::default().fg(MUTED),
        ),
        Line::styled(
            "Environment variables and command-line values override saved text files.",
            Style::default().fg(MUTED),
        ),
        Line::default(),
        Line::styled(
            "↑/↓ field  ·  ←/→ choose  ·  Enter search/edit  ·  x clear key  ·  s save  ·  r refresh",
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
}

fn render_tasks(frame: &mut Frame<'_>, app: &App, area: Rect, block: Block<'_>) {
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

    #[test]
    fn settings_panel_renders_sources_and_never_renders_the_api_key() {
        let active = ActiveSettings {
            provider: "openai".to_string(),
            model: "gpt-5.2".to_string(),
            api_key: Some("super-secret-value".to_string()),
            output_limit: 32 * 1024,
            provider_source: ValueSource::Global,
            model_source: ValueSource::Global,
            api_key_source: ValueSource::Global,
            output_limit_source: ValueSource::Global,
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
            },
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
}
