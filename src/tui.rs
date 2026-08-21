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
    run_loop(&mut terminal, &mut app, runtime, tasks, &mut receiver).await
}

async fn run_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    runtime: Arc<AgentRuntime>,
    tasks: TaskManager,
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
                        match handle_key(app, key, &tasks).await {
                            Action::Continue => {}
                            Action::Quit => return Ok(()),
                            Action::Submit(prompt) => {
                                let runtime = runtime.clone();
                                tokio::spawn(async move {
                                    let _ = runtime.run_turn(prompt).await;
                                });
                            }
                        }
                    }
                    Event::Paste(text) if app.overlay.is_none() => {
                        app.input.insert_str(text);
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
                    Event::FocusGained | Event::FocusLost | Event::Resize(_, _) | Event::Key(_) | Event::Paste(_) => {}
                }
            }
            event = receiver.recv() => match event {
                Ok(event) => app.apply(event),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    for event in runtime.session().snapshot().await {
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
}

async fn handle_key(app: &mut App, key: KeyEvent, tasks: &TaskManager) -> Action {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Action::Quit;
    }
    if key.code == KeyCode::Esc {
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
        }
        return Action::Continue;
    }
    match key.code {
        KeyCode::F(1) => {
            app.overlay_scroll = 0;
            app.overlay = Some(Overlay::Help);
        }
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
        "Enter send  ·  Shift+Enter newline  ·  F1 help  ·  Ctrl+P protocols  ·  Ctrl+T tasks"
    } else {
        "Enter send  ·  F1 help  ·  Ctrl+P protocols"
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
                "KEYS\n\nEnter          send\nShift+Enter    newline\nPageUp/Down    scroll transcript\nCtrl+P         protocol index\nCtrl+T         managed tasks\nEsc            close this window\nCtrl+C         quit\n\nSESSION\n\n{}\n{}\n\nWorking directory\n{}",
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
    }
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
