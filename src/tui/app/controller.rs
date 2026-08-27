use super::*;
use futures_util::{Stream, StreamExt};
use std::pin::Pin;
use std::task::Poll;

pub struct TuiServices {
    pub runtime: Arc<AgentRuntime>,
    pub protocols: Arc<ProtocolRegistry>,
    pub commands: Arc<CommandRegistry>,
    pub tui: Arc<TuiRegistry>,
    pub tasks: TaskManager,
    pub manager: Arc<ConfigManager>,
    pub environment: Arc<AgentEnvironment>,
    pub catalog: Arc<ModelCatalog>,
    pub output: Arc<OutputStore>,
    pub info: TuiInfo,
    pub draft: String,
}

pub struct TuiTerminal {
    terminal: DefaultTerminal,
    first_session: bool,
    keyboard_enhancement: bool,
}

impl TuiTerminal {
    pub fn new() -> Result<Self> {
        let terminal = ratatui::try_init()?;
        // A capability query blocks for two seconds when a terminal does not
        // answer device-status requests. Unsupported Unix terminals safely
        // ignore the keyboard enhancement push/pop control sequences.
        let keyboard_enhancement = cfg!(unix);
        let setup = if keyboard_enhancement {
            execute!(
                stdout(),
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
                EnableMouseCapture,
                EnableBracketedPaste
            )
        } else {
            execute!(stdout(), EnableMouseCapture, EnableBracketedPaste)
        };
        if let Err(error) = setup {
            if keyboard_enhancement {
                let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
            }
            ratatui::restore();
            return Err(error.into());
        }
        Ok(Self {
            terminal,
            first_session: true,
            keyboard_enhancement,
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
            environment,
            catalog,
            output,
            mut info,
            draft,
        } = services;
        info.thinking =
            effective_thinking(&catalog, &info.provider, &info.model, info.thinking).await;
        let session = runtime.session().clone();
        let mut receiver = session.subscribe();
        let mut pending_receiver = runtime.subscribe_pending_messages();
        let keymap = Keymap::load(Some(&info.cwd), info.key_display).await?;
        let show_splash = std::mem::take(&mut self.first_session);
        let refresh_catalog_on_start = show_splash && catalog.networking_enabled();
        let mut app = App::new(
            protocols.descriptors(),
            commands,
            tui,
            info,
            keymap,
            draft,
            show_splash,
        );
        app.pending_messages = pending_receiver.borrow().clone();
        app.protocol_source = Some(protocols);
        for event in session.snapshot().await {
            app.apply(event);
        }
        app.finish_hydration();
        if runtime.turn_running().await {
            app.busy = true;
            app.busy_since = Some(Instant::now());
            app.activity = Some(Activity::Thinking);
            app.sync_composer_chrome();
        }

        let services = LoopServices {
            runtime,
            tasks,
            manager,
            environment,
            catalog,
            output,
        };
        run_loop(
            &mut self.terminal,
            &mut app,
            services,
            &mut receiver,
            &mut pending_receiver,
            refresh_catalog_on_start,
        )
        .await
    }
}

impl Drop for TuiTerminal {
    fn drop(&mut self) {
        let _ = execute!(stdout(), DisableMouseCapture, DisableBracketedPaste);
        if self.keyboard_enhancement {
            let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
        }
        ratatui::restore();
    }
}

pub(super) struct LoopServices {
    runtime: Arc<AgentRuntime>,
    tasks: TaskManager,
    manager: Arc<ConfigManager>,
    environment: Arc<AgentEnvironment>,
    catalog: Arc<ModelCatalog>,
    output: Arc<OutputStore>,
}

pub(super) async fn run_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    services: LoopServices,
    receiver: &mut tokio::sync::broadcast::Receiver<SessionUpdate>,
    pending_receiver: &mut watch::Receiver<Vec<PendingMessage>>,
    refresh_catalog_on_start: bool,
) -> Result<TuiOutcome> {
    let mut terminal_events = EventStream::new();
    let mut animation = time::interval(Duration::from_millis(90));
    animation.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut smooth_scroll = time::interval(SMOOTH_SCROLL_FRAME_DURATION);
    smooth_scroll.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let (background_tx, mut background_rx) = mpsc::unbounded_channel();
    if refresh_catalog_on_start {
        start_background_catalog_refresh(app, &services, background_tx.clone());
    }
    loop {
        let context = services.runtime.context_usage();
        app.info.context_tokens = context.tokens;
        app.info.context_accuracy = context.accuracy;
        terminal.draw(|frame| render(frame, app))?;
        tokio::select! {
            _ = animation.tick(), if !app.animations_paused() => {
                app.frame = app.frame.wrapping_add(1);
            },
            _ = smooth_scroll.tick(), if app.mouse_scroll_animating() => {
                app.advance_mouse_scroll_animation();
            },
            event = terminal_events.next() => {
                let Some(event) = event else { return persist_and_exit(app, &services, TuiOutcome::Quit).await; };
                let (events, paste) =
                    collect_possible_paste(&mut terminal_events, event?, app.pty.is_none())
                        .await?;
                if let Some(text) = paste {
                    apply_surface_paste(app, text, &background_tx);
                }
                for event in events {
                match event {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        let selection_active = app.selection.is_some()
                            || (app.overlay == Some(Overlay::Composer)
                                && composer_has_selection(&app.input));
                        if app.pty.is_none()
                            && is_ignored_tui_key(key, selection_active)
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
                            apply_surface_paste(app, text, &background_tx);
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
            changed = pending_receiver.changed() => {
                if changed.is_ok() {
                    app.pending_messages = pending_receiver.borrow().clone();
                }
            },
            Some(event) = background_rx.recv() => {
                finish_background(app, &services, background_tx.clone(), event).await;
            },
        }
        if pty_finished(app)? {
            close_pty(app, "Terminal exited");
        }
    }
}

pub(super) async fn persist_and_exit(
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

pub(super) enum BackgroundEvent {
    CatalogRefreshed {
        result: Box<Result<CatalogRefreshReport>>,
        announced: bool,
    },
    TurnStarted {
        prompt: String,
        submitted_image_ids: Vec<u64>,
        result: Result<()>,
    },
    OauthFinished(Result<OauthToken>),
    ClipboardImageRead(Result<Vec<u8>>),
    ClipboardRead(Result<clipboard::ClipboardContent>),
    Completions {
        generation: u64,
        result: Result<Option<TuiCompletions>>,
    },
}

pub(super) enum Action {
    Continue,
    Quit,
    Submit {
        prompt: String,
        images: Vec<ImageAttachment>,
    },
    Enqueue {
        prompt: String,
        images: Vec<ImageAttachment>,
        kind: PendingMessageKind,
    },
    RestorePending,
    UpgradePending,
    ReadClipboardImage,
    ReadClipboard,
    RefreshCompletions,
    InterruptTurn,
    Compact,
    OpenModels(String),
    SelectModel,
    OpenSettings,
    SaveSettings,
    OpenEnvironment {
        return_to_settings: bool,
    },
    StoreEnvironment {
        name: String,
        value: String,
        return_to_settings: bool,
    },
    DeleteEnvironment {
        name: String,
        return_to_settings: bool,
    },
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

pub(super) async fn apply_action(
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
        Action::Submit { prompt, images } => {
            // Deferred startup work (frozen context, backend preparation) can
            // block on the first submit; run it off the event loop so the
            // interface and its animation keep rendering meanwhile.
            let submitted_image_ids = app.composer_images.keys().copied().collect();
            let runtime = services.runtime.clone();
            tokio::spawn(async move {
                let result = runtime.start_turn_with_images(prompt.clone(), images).await;
                let _ = background_tx.send(BackgroundEvent::TurnStarted {
                    prompt,
                    submitted_image_ids,
                    result,
                });
            });
            Ok(None)
        }
        Action::Enqueue {
            prompt,
            images,
            kind,
        } => {
            match services
                .runtime
                .enqueue_message_with_images(prompt, images, kind)
                .await
            {
                Ok(_) => {
                    app.clear_accepted_draft();
                    app.pending_messages = services.runtime.pending_messages().await;
                    let _ = services.runtime.session().save_draft("").await;
                }
                Err(error) => {
                    app.delivery = None;
                    app.overlay = Some(Overlay::Composer);
                    app.set_flash(format!("Cannot add pending message: {error:#}"));
                }
            }
            Ok(None)
        }
        Action::RestorePending => {
            if let Some((message, images)) = services.runtime.cancel_latest_pending().await {
                app.restore_pending_to_draft(&message.text, images);
                app.pending_messages = services.runtime.pending_messages().await;
                app.set_flash("Pending message restored to draft");
            } else {
                app.set_flash("No pending message to restore");
            }
            Ok(None)
        }
        Action::UpgradePending => {
            if services.runtime.upgrade_latest_queued().await.is_some() {
                app.pending_messages = services.runtime.pending_messages().await;
                app.set_flash("Queued message upgraded to guidance");
            } else {
                app.set_flash("No queued message to upgrade");
            }
            Ok(None)
        }
        Action::ReadClipboardImage => {
            start_clipboard_image_read(app, background_tx);
            Ok(None)
        }
        Action::ReadClipboard => {
            start_clipboard_read(app, background_tx);
            Ok(None)
        }
        Action::RefreshCompletions => {
            start_completion_query(app, background_tx);
            Ok(None)
        }
        Action::InterruptTurn => {
            if services.runtime.interrupt_turn().await {
                app.activity = Some(Activity::Interrupting);
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
            app.settings =
                Some(SettingsState::load(active, &services.catalog, &services.environment).await);
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
                &services.environment,
            )
            .await;
            Ok(None)
        }
        Action::OpenEnvironment { return_to_settings } => {
            open_environment(app, &services.environment, return_to_settings).await;
            Ok(None)
        }
        Action::StoreEnvironment {
            name,
            value,
            return_to_settings,
        } => {
            store_environment(app, &services.environment, name, value, return_to_settings).await;
            Ok(None)
        }
        Action::DeleteEnvironment {
            name,
            return_to_settings,
        } => {
            delete_environment(app, &services.environment, &name, return_to_settings).await;
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

fn apply_surface_paste(
    app: &mut App,
    text: String,
    background_tx: &mpsc::UnboundedSender<BackgroundEvent>,
) {
    app.skip_splash();
    handle_paste(app, text);
    if app.overlay == Some(Overlay::Composer) {
        start_completion_query(app, background_tx.clone());
    }
}

async fn collect_possible_paste(
    stream: &mut EventStream,
    first: Event,
    allow_burst: bool,
) -> Result<(Vec<Event>, Option<String>)> {
    let Event::Key(key) = first else {
        return Ok((vec![first], None));
    };
    if !allow_burst || key.kind == KeyEventKind::Release || !is_textual_paste_key(key) {
        return Ok((vec![Event::Key(key)], None));
    }
    let (keys, rest) = split_paste_burst(key, take_ready_events(stream).await?);
    let paste = pasted_text_from_keys(&keys);
    let mut events = if paste.is_some() {
        Vec::new()
    } else {
        keys.into_iter().map(Event::Key).collect()
    };
    events.extend(rest);
    Ok((events, paste))
}

/// Collects the events the terminal has already delivered without waiting for
/// more to arrive.
///
/// The poll runs under the caller's waker. `EventStream` parks a single waker
/// in its background reader thread, so polling it with a detached waker (such
/// as `now_or_never`) leaves that thread waking a waker that can never resume
/// this loop; the next key press then freezes the interface whenever no other
/// select branch fires.
async fn take_ready_events(stream: &mut EventStream) -> Result<Vec<Event>> {
    let mut events = Vec::new();
    // Poll once and surface `Pending` as a value instead of awaiting it: the
    // stream sees the loop's real waker, but an empty queue stops the drain
    // immediately instead of suspending until the next key arrives.
    while let Poll::Ready(event) =
        std::future::poll_fn(|context| Poll::Ready(Pin::new(&mut *stream).poll_next(context))).await
    {
        match event {
            Some(Ok(event)) => events.push(event),
            Some(Err(error)) => return Err(error.into()),
            None => break,
        }
    }
    Ok(events)
}

/// Splits already-delivered events into the burst of textual keys following
/// `first` and the events that no longer belong to the burst. Windows reports
/// a release for every pasted key, so textual releases are skipped without
/// ending the burst.
pub(super) fn split_paste_burst(
    first: KeyEvent,
    events: Vec<Event>,
) -> (Vec<KeyEvent>, Vec<Event>) {
    let mut keys = vec![first];
    let mut rest = Vec::new();
    for event in events {
        if rest.is_empty()
            && let Event::Key(key) = event
            && is_textual_paste_key(key)
        {
            if key.kind != KeyEventKind::Release {
                keys.push(key);
            }
            continue;
        }
        rest.push(event);
    }
    (keys, rest)
}

fn is_textual_paste_key(key: KeyEvent) -> bool {
    !key.modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
        && matches!(key.code, KeyCode::Char(_) | KeyCode::Enter | KeyCode::Tab)
}

pub(super) fn pasted_text_from_keys(keys: &[KeyEvent]) -> Option<String> {
    if keys.len() < 2 {
        return None;
    }
    let mut text = String::new();
    let mut newlines = 0usize;
    let mut has_non_enter = false;
    for key in keys {
        match key.code {
            KeyCode::Char('\n' | '\r') | KeyCode::Enter => {
                text.push('\n');
                newlines += 1;
            }
            KeyCode::Char(character) => {
                text.push(character);
                has_non_enter = true;
            }
            KeyCode::Tab => {
                text.push('\t');
                has_non_enter = true;
            }
            _ => return None,
        }
    }
    if newlines == 0 || !has_non_enter {
        return None;
    }
    // A short typed reply plus Enter can sit in the same input burst while a
    // frame is drawn. Keep that as a send; longer or mid-text newlines are paste.
    if newlines == 1 && text.ends_with('\n') && text.chars().count() <= 4 {
        return None;
    }
    Some(text)
}

pub(super) fn handle_paste(app: &mut App, text: String) {
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
            app.insert_composer_text(text);
        }
        None => {
            app.overlay = Some(Overlay::Composer);
            app.insert_composer_text(text);
        }
        _ => {}
    }
}

pub(super) fn replace_composer_range(
    input: &mut TextArea<'static>,
    range: TuiTextRange,
    replacement: &str,
) -> bool {
    if (range.start.line, range.start.column) > (range.end.line, range.end.column) {
        return false;
    }
    let lines = input.lines();
    let valid_position = |position: TuiTextPosition| {
        lines
            .get(position.line)
            .is_some_and(|line| position.column <= line.chars().count())
    };
    if !valid_position(range.start) || !valid_position(range.end) {
        return false;
    }
    let Ok(start_line) = u16::try_from(range.start.line) else {
        return false;
    };
    let Ok(start_column) = u16::try_from(range.start.column) else {
        return false;
    };
    let Ok(end_line) = u16::try_from(range.end.line) else {
        return false;
    };
    let Ok(end_column) = u16::try_from(range.end.column) else {
        return false;
    };
    input.cancel_selection();
    input.move_cursor(CursorMove::Jump(end_line, end_column));
    input.start_selection();
    input.move_cursor(CursorMove::Jump(start_line, start_column));
    input.insert_str(replacement)
}

pub(super) async fn dispatch_ui_command(
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
            app.dismiss_completions();
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
        CoreCommand::RefreshCatalog => Action::RefreshCatalog,
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
        CoreCommand::SetEnvironment => {
            open_environment_name_prompt(app, false);
            Action::Continue
        }
        CoreCommand::SetTerminal => {
            open_set_terminal_prompt(app);
            Action::Continue
        }
        CoreCommand::Terminal => Action::OpenTerminal,
    }
}

pub(super) fn start_compaction(app: &mut App, runtime: Arc<AgentRuntime>) {
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

pub(super) async fn handle_key(app: &mut App, key: KeyEvent, services: &LoopServices) -> Action {
    app.cancel_mouse_scroll_animation();
    let key_name = key_name(key);
    if app.interrupt_on_double_press(key, &key_name) {
        return Action::InterruptTurn;
    }
    if handle_selection_key(app, &key_name) {
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
            if app.overlay == Some(Overlay::Composer) && composer_has_selection(&app.input) {
                copy_composer_selection(app);
            } else {
                copy_current_surface(app);
            }
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
            app.dismiss_completions();
            app.overlay = Some(Overlay::Composer);
            Action::Continue
        }
        Some("reference") => {
            app.overlay = Some(Overlay::Composer);
            app.insert_composer_text("@");
            Action::RefreshCompletions
        }
        Some("paste") => {
            app.overlay = Some(Overlay::Composer);
            Action::ReadClipboard
        }
        Some("paste_image") => {
            app.overlay = Some(Overlay::Composer);
            Action::ReadClipboardImage
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
            app.page_transcript(1);
            Action::Continue
        }
        Some("page_up") => {
            app.page_transcript(-1);
            Action::Continue
        }
        Some("scroll_down") => {
            app.scroll_transcript(3);
            Action::Continue
        }
        Some("scroll_up") => {
            app.scroll_transcript(-3);
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

pub(super) fn handle_selection_key(app: &mut App, key_name: &str) -> bool {
    if app.selection.is_none() {
        return false;
    }
    match app.keymap.action("selection", key_name).as_deref() {
        Some("copy") => copy_current_surface(app),
        Some("close") => app.selection = None,
        _ => {
            app.selection = None;
            return false;
        }
    }
    true
}

pub(super) async fn handle_overlay_key(
    app: &mut App,
    key: KeyEvent,
    key_name: &str,
    overlay: Overlay,
    services: &LoopServices,
) -> Action {
    match overlay {
        Overlay::Composer => {
            let composer_action = app.keymap.action("composer", key_name);
            if app.completions.is_some() {
                match (key_name, composer_action.as_deref()) {
                    ("tab", _) | (_, Some("submit")) => {
                        app.accept_completion();
                        return Action::RefreshCompletions;
                    }
                    ("backtab" | "shift+tab" | "shift+backtab", _) => {
                        app.move_completion(-1);
                        return Action::Continue;
                    }
                    (_, Some("cursor_up")) => {
                        app.move_completion(-1);
                        return Action::Continue;
                    }
                    (_, Some("cursor_down")) => {
                        app.move_completion(1);
                        return Action::Continue;
                    }
                    (_, Some("close")) => {
                        app.dismiss_completions();
                        return Action::Continue;
                    }
                    _ => {}
                }
            }
            match composer_action.as_deref() {
                Some("submit") => {
                    app.submit()
                        .map_or(Action::Continue, |(prompt, images)| Action::Submit {
                            prompt,
                            images,
                        })
                }
                Some("newline") => {
                    app.insert_composer_newline();
                    Action::RefreshCompletions
                }
                Some("paste") => Action::ReadClipboard,
                Some("paste_image") => Action::ReadClipboardImage,
                Some("complete") => {
                    if app.accept_completion() {
                        Action::RefreshCompletions
                    } else {
                        Action::Continue
                    }
                }
                Some("copy") => {
                    copy_composer_selection(app);
                    Action::Continue
                }
                Some("restore_pending") => Action::RestorePending,
                Some("upgrade_pending") => Action::UpgradePending,
                Some("close") => {
                    app.composer_mouse_selecting = false;
                    app.dismiss_completions();
                    app.overlay = None;
                    Action::Continue
                }
                Some("quit") => Action::Quit,
                action => {
                    app.edit_composer(key, action);
                    Action::RefreshCompletions
                }
            }
        }
        Overlay::Delivery => match app.keymap.action("list", key_name).as_deref() {
            Some("previous") => {
                if let Some(delivery) = app.delivery.as_mut() {
                    delivery.selected = wrapped_index(delivery.selected, -1, 2);
                }
                Action::Continue
            }
            Some("next") => {
                if let Some(delivery) = app.delivery.as_mut() {
                    delivery.selected = wrapped_index(delivery.selected, 1, 2);
                }
                Action::Continue
            }
            Some("confirm") => confirm_delivery(app),
            Some("close") => {
                app.delivery = None;
                app.overlay = Some(Overlay::Composer);
                Action::Continue
            }
            Some("quit") => Action::Quit,
            _ => Action::Continue,
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
                app.selected_task = bounded_index(
                    app.selected_task,
                    app.overlay_page_distance(-1),
                    app.task_records.len(),
                );
                Action::Continue
            }
            Some("page_down") => {
                app.selected_task = bounded_index(
                    app.selected_task,
                    app.overlay_page_distance(1),
                    app.task_records.len(),
                );
                Action::Continue
            }
            _ => Action::Continue,
        },
        Overlay::Models => {
            let page_up = app.overlay_page_distance(-1);
            let page_down = app.overlay_page_distance(1);
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
                    selector.page_selection(page_up);
                    Action::Continue
                }
                Some("page_down") => {
                    selector.page_selection(page_down);
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
            Some("copy") => {
                copy_document(app);
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
                app.page_overlay(-1);
                Action::Continue
            }
            Some("page_down") => {
                app.page_overlay(1);
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
                    app.page_overlay(-1);
                    Action::Continue
                }
                Some("page_down") => {
                    app.page_overlay(1);
                    Action::Continue
                }
                _ => Action::Continue,
            }
        }
    }
}

pub(super) fn confirm_delivery(app: &App) -> Action {
    let Some(delivery) = app.delivery.as_ref() else {
        return Action::Continue;
    };
    let (prompt, images) = app.composer_submission();
    if prompt.trim().is_empty() {
        return Action::Continue;
    }
    Action::Enqueue {
        prompt,
        images,
        kind: if delivery.selected == 0 {
            PendingMessageKind::Queued
        } else {
            PendingMessageKind::Guidance
        },
    }
}

pub(super) async fn confirm_command(app: &mut App, services: &LoopServices) -> Action {
    let command = app.matching_commands().get(app.command_selected).cloned();
    app.overlay = None;
    app.reset_command_search();
    if let Some(command) = command {
        return dispatch_ui_command(app, command.spec.target, services).await;
    }
    Action::Continue
}

pub(super) enum CommandKey {
    Continue,
    Confirm,
    Quit,
}

pub(super) fn apply_command_key(app: &mut App, key: KeyEvent, key_name: &str) -> CommandKey {
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
        Some("page_up") => {
            app.page_command_selection(app.overlay_page_distance(-1));
            CommandKey::Continue
        }
        Some("page_down") => {
            app.page_command_selection(app.overlay_page_distance(1));
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

pub(super) enum SelectorKey {
    Continue,
    Confirm,
    Quit,
}

pub(super) async fn handle_selector_key(
    app: &mut App,
    key: KeyEvent,
    key_name: &str,
    services: &LoopServices,
) -> Action {
    let environment_return = app.selector.as_ref().and_then(|selector| {
        if let SelectorKind::Environment { return_to_settings } = &selector.kind {
            Some(*return_to_settings)
        } else {
            None
        }
    });
    if let Some(return_to_settings) = environment_return {
        match app
            .keymap
            .action_chain(&["environment", "selector"], key_name)
            .as_deref()
        {
            Some("add") => {
                app.selector = None;
                open_environment_name_prompt(app, return_to_settings);
                return Action::Continue;
            }
            Some("remove") => {
                return app
                    .selector
                    .as_ref()
                    .and_then(SelectorState::selected_item)
                    .map_or(Action::Continue, |item| Action::DeleteEnvironment {
                        name: item.id.clone(),
                        return_to_settings,
                    });
            }
            _ => {}
        }
    }
    match apply_selector_key(app, key, key_name) {
        SelectorKey::Quit => Action::Quit,
        SelectorKey::Confirm => confirm_selector(app, services).await,
        SelectorKey::Continue => Action::Continue,
    }
}

pub(super) fn apply_selector_key(app: &mut App, key: KeyEvent, key_name: &str) -> SelectorKey {
    let page_up = app.overlay_page_distance(-1);
    let page_down = app.overlay_page_distance(1);
    let Some(selector) = app.selector.as_mut() else {
        app.overlay = None;
        return SelectorKey::Continue;
    };
    match app.keymap.action("selector", key_name).as_deref() {
        Some("quit") => SelectorKey::Quit,
        Some("close") => {
            let return_to_settings = matches!(
                &selector.kind,
                SelectorKind::Environment {
                    return_to_settings: true
                }
            );
            app.selector = None;
            app.overlay = return_to_settings.then_some(Overlay::Settings);
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
        Some("page_up") => {
            selector.page_selection(page_up);
            SelectorKey::Continue
        }
        Some("page_down") => {
            selector.page_selection(page_down);
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

pub(super) async fn confirm_selector(app: &mut App, services: &LoopServices) -> Action {
    let Some(selector) = app.selector.take() else {
        app.overlay = None;
        return Action::Continue;
    };
    let Some(item) = selector.selected_item().cloned() else {
        app.set_flash("Nothing is selected");
        if matches!(&selector.kind, SelectorKind::Environment { .. }) {
            app.selector = Some(selector);
            app.overlay = Some(Overlay::Selector);
        } else {
            app.overlay = None;
        }
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
        SelectorKind::Environment { return_to_settings } => {
            open_environment_value_prompt(app, item.id, return_to_settings);
            Action::Continue
        }
    }
}

pub(super) fn handle_text_key(app: &mut App, key: KeyEvent, key_name: &str) -> Action {
    let Some(prompt) = app.text_prompt.as_mut() else {
        app.overlay = None;
        return Action::Continue;
    };
    match app.keymap.action("text", key_name).as_deref() {
        Some("quit") => Action::Quit,
        Some("cancel") => {
            let return_to_environment = app.text_prompt.take().is_some_and(|prompt| {
                matches!(
                    prompt.purpose,
                    TextPurpose::EnvironmentName {
                        return_to_settings: true
                    } | TextPurpose::EnvironmentValue {
                        return_to_settings: true,
                        ..
                    }
                )
            });
            app.overlay = None;
            if return_to_environment {
                Action::OpenEnvironment {
                    return_to_settings: true,
                }
            } else {
                Action::Continue
            }
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
                TextPurpose::EnvironmentName { return_to_settings } => {
                    let name = prompt.value.trim().to_string();
                    if let Err(error) = validate_environment_name(&name) {
                        open_environment_name_prompt(app, return_to_settings);
                        app.set_flash(format!("Invalid environment variable name: {error}"));
                    } else {
                        open_environment_value_prompt(app, name, return_to_settings);
                    }
                    Action::Continue
                }
                TextPurpose::EnvironmentValue {
                    name,
                    return_to_settings,
                } => Action::StoreEnvironment {
                    name,
                    value: prompt.value,
                    return_to_settings,
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

pub(super) fn handle_oauth_key(app: &mut App, key: KeyEvent, key_name: &str) -> Action {
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

pub(super) fn handle_settings_key(app: &mut App, key: KeyEvent, key_name: &str) -> Action {
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
            settings.selected = wrapped_index(settings.selected, -1, 5);
            Action::Continue
        }
        Some("next") => {
            settings.selected = wrapped_index(settings.selected, 1, 5);
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
            4 => Action::OpenEnvironment {
                return_to_settings: true,
            },
            _ => Action::Continue,
        },
        Some("save") => Action::SaveSettings,
        Some("refresh") => Action::RefreshCatalog,
        _ => Action::Continue,
    }
}

pub(super) async fn handle_mouse(
    app: &mut App,
    mouse: MouseEvent,
    services: &LoopServices,
) -> Action {
    if !matches!(
        mouse.kind,
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
    ) {
        app.cancel_mouse_scroll_animation();
    }
    if consume_copy_click_release(app, mouse) {
        return Action::Continue;
    }
    if handle_composer_mouse(app, mouse) {
        return Action::Continue;
    }
    if handle_completion_mouse(app, mouse) {
        return Action::RefreshCompletions;
    }
    if is_selection_copy_click(app, mouse) {
        copy_current_surface(app);
        app.copy_click_release_pending = true;
        return Action::Continue;
    }
    if close_float_on_outside_click(app, mouse) {
        return Action::Continue;
    }
    if handle_transcript_scrollbar_mouse(app, mouse) {
        return Action::Continue;
    }
    if begin_direct_transcript_selection(app, mouse) {
        return Action::Continue;
    }
    if update_mouse_selection(app, mouse, app.overlay.is_none()) {
        return Action::Continue;
    }
    if activate_transcript_mouse(app, mouse) {
        return Action::Continue;
    }
    match mouse.kind {
        MouseEventKind::ScrollUp => handle_mouse_scroll(app, -1),
        MouseEventKind::ScrollDown => handle_mouse_scroll(app, 1),
        MouseEventKind::Down(MouseButton::Left) => {
            let Some(target) = hit_target(&app.hit_regions, mouse) else {
                return Action::Continue;
            };
            if let AppHit::Transcript(index) = target {
                app.click_transcript_block(index, false);
                return Action::Continue;
            }
            let activate = is_double_click(&mut app.last_click, target);
            match target {
                AppHit::Transcript(_) => unreachable!(),
                AppHit::TranscriptTail => unreachable!(),
                AppHit::Completion(index) => {
                    app.select_completion(index);
                    return Action::RefreshCompletions;
                }
                AppHit::Delivery(index) => {
                    if let Some(delivery) = app.delivery.as_mut() {
                        delivery.selected = index;
                        if activate {
                            return confirm_delivery(app);
                        }
                    }
                }
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
                                4 => {
                                    return Action::OpenEnvironment {
                                        return_to_settings: true,
                                    };
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

pub(super) fn handle_mouse_scroll(app: &mut App, direction: isize) {
    match app.overlay {
        Some(Overlay::Command) => {
            app.cancel_mouse_scroll_animation();
            app.move_command_selection(direction);
        }
        Some(Overlay::Delivery) => {
            app.cancel_mouse_scroll_animation();
            if let Some(delivery) = app.delivery.as_mut() {
                delivery.selected = wrapped_index(delivery.selected, direction, 2);
            }
        }
        Some(Overlay::Selector) => {
            app.cancel_mouse_scroll_animation();
            if let Some(selector) = app.selector.as_mut() {
                selector.move_selection(direction);
            }
        }
        Some(Overlay::Tasks) => {
            app.cancel_mouse_scroll_animation();
            app.selected_task = wrapped_index(app.selected_task, direction, app.task_records.len());
        }
        Some(Overlay::Models) => {
            app.cancel_mouse_scroll_animation();
            if let Some(selector) = app.model_selector.as_mut() {
                selector.move_selection(direction * MOUSE_SCROLL_ROWS);
            }
        }
        Some(Overlay::Settings) => {
            app.cancel_mouse_scroll_animation();
            if let Some(settings) = app.settings.as_mut() {
                settings.selected = wrapped_index(settings.selected, direction, 5);
            }
        }
        Some(Overlay::Composer) => {
            app.cancel_mouse_scroll_animation();
            app.move_completion(direction);
        }
        Some(Overlay::Text | Overlay::Oauth | Overlay::Terminal) => {
            app.cancel_mouse_scroll_animation();
        }
        Some(_) => app.smooth_scroll_overlay(direction),
        None => app.smooth_scroll_transcript(direction),
    }
}

pub(super) fn handle_composer_mouse(app: &mut App, mouse: MouseEvent) -> bool {
    if app.overlay != Some(Overlay::Composer) {
        return false;
    }
    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Right))
        && composer_has_selection(&app.input)
    {
        copy_composer_selection(app);
        app.copy_click_release_pending = true;
        return true;
    }

    let Some(view) = app.composer_view.as_ref() else {
        return false;
    };
    let starting = matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left));
    let continuing = app.composer_mouse_selecting
        && matches!(
            mouse.kind,
            MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
        );
    if (!starting && !continuing)
        || (starting && !view.inner.contains((mouse.column, mouse.row).into()))
    {
        return false;
    }

    let mut cursor = composer_cursor_at(view, &app.input, mouse.column, mouse.row);
    if let Some(token) = image_token_spans(&app.input.lines()[cursor.0])
        .into_iter()
        .filter(|token| app.composer_images.contains_key(&token.id))
        .find(|token| token.start_col < cursor.1 && cursor.1 < token.end_col)
    {
        cursor.1 = if cursor.1 - token.start_col < token.end_col - cursor.1 {
            token.start_col
        } else {
            token.end_col
        };
    }
    if starting {
        app.input.cancel_selection();
        app.input
            .move_cursor(CursorMove::Jump(cursor.0 as u16, cursor.1 as u16));
        app.input.start_selection();
        app.composer_mouse_selecting = true;
    } else {
        app.input
            .move_cursor(CursorMove::Jump(cursor.0 as u16, cursor.1 as u16));
        if matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left)) {
            if app
                .input
                .selection_range()
                .is_some_and(|(start, end)| start == end)
            {
                app.input.cancel_selection();
            }
            app.composer_mouse_selecting = false;
        }
    }
    true
}

pub(super) fn composer_cursor_at(
    view: &ComposerView,
    input: &TextArea<'_>,
    column: u16,
    row: u16,
) -> (usize, usize) {
    let visual_row = view.top
        + row
            .clamp(view.inner.y, view.inner.bottom().saturating_sub(1))
            .saturating_sub(view.inner.y) as usize;
    let wrapped = view.rows[visual_row.min(view.rows.len().saturating_sub(1))];
    let target = column
        .clamp(view.inner.x, view.inner.right().saturating_sub(1))
        .saturating_sub(view.inner.x) as usize;
    let line = &input.lines()[wrapped.logical_row];
    let mut width = 0usize;
    let mut best = (wrapped.start_col, target);
    for (offset, character) in line
        .chars()
        .skip(wrapped.start_col)
        .take(wrapped.end_col.saturating_sub(wrapped.start_col))
        .enumerate()
    {
        let distance = target.abs_diff(width);
        if distance <= best.1 {
            best = (wrapped.start_col + offset, distance);
        }
        width = display_width_to(character, width, input.tab_length());
    }
    let distance = target.abs_diff(width);
    if distance <= best.1 {
        (wrapped.logical_row, wrapped.end_col)
    } else {
        (wrapped.logical_row, best.0)
    }
}

pub(super) fn activate_transcript_mouse(app: &mut App, mouse: MouseEvent) -> bool {
    let open_document = match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => false,
        MouseEventKind::Down(MouseButton::Right) => true,
        _ => return false,
    };
    let index = match hit_target(&app.hit_regions, mouse) {
        Some(AppHit::TranscriptTail) if !open_document => {
            app.last_click = None;
            app.transcript_offset =
                transcript_live_tail(app.transcript_rows, app.transcript_height);
            app.transcript_follow_tail = true;
            app.transcript_center_selected = false;
            return true;
        }
        Some(AppHit::Transcript(index)) => index,
        _ => return false,
    };
    if !app
        .blocks
        .get(index)
        .is_some_and(|block| !matches!(block.kind, BlockKind::User | BlockKind::Assistant))
    {
        return false;
    }
    app.last_click = None;
    app.click_transcript_block(index, open_document);
    true
}

pub(super) fn handle_transcript_scrollbar_mouse(app: &mut App, mouse: MouseEvent) -> bool {
    let starting = matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left));
    let continuing = app.transcript_scrollbar_drag.is_some()
        && matches!(
            mouse.kind,
            MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
        );
    if !starting && !continuing {
        return false;
    }
    if starting {
        let Some(area) = app.transcript_scrollbar_area.filter(|area| {
            app.overlay.is_none() && area.contains((mouse.column, mouse.row).into())
        }) else {
            app.transcript_scrollbar_drag = None;
            return false;
        };
        let Some(metrics) = transcript_scrollbar_metrics(app) else {
            app.transcript_scrollbar_drag = None;
            return false;
        };
        let row = mouse.row.clamp(area.y, area.bottom().saturating_sub(1));
        let relative_row = row.saturating_sub(area.y) as usize;
        let thumb_end = metrics.thumb_start.saturating_add(metrics.thumb_length);
        if !(metrics.thumb_start..thumb_end).contains(&relative_row) {
            let target_thumb_start = relative_row
                .saturating_sub(metrics.thumb_length / 2)
                .min(metrics.max_thumb_start);
            let offset = proportional_scrollbar_offset(
                target_thumb_start,
                metrics.max_thumb_start,
                metrics.reading_end,
            );
            app.set_transcript_scrollbar_offset(offset);
        }
        app.selection = None;
        app.last_click = None;
        app.transcript_scrollbar_drag = Some(TranscriptScrollbarDrag {
            row,
            offset: app.transcript_offset,
        });
        return true;
    }

    let drag = app
        .transcript_scrollbar_drag
        .expect("continuing scrollbar drag has state");
    if let (Some(area), Some(metrics)) = (
        app.transcript_scrollbar_area,
        transcript_scrollbar_metrics(app),
    ) {
        let row = mouse.row.clamp(area.y, area.bottom().saturating_sub(1));
        let distance = row.abs_diff(drag.row) as usize;
        let offset_distance =
            proportional_scrollbar_offset(distance, metrics.max_thumb_start, metrics.reading_end);
        let offset = if row < drag.row {
            drag.offset.saturating_sub(offset_distance)
        } else {
            drag.offset.saturating_add(offset_distance)
        };
        app.set_transcript_scrollbar_offset(offset);
    }
    if matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left)) {
        app.transcript_scrollbar_drag = None;
    }
    true
}

fn proportional_scrollbar_offset(position: usize, maximum: usize, live_tail: usize) -> usize {
    if maximum == 0 {
        return 0;
    }
    position
        .saturating_mul(live_tail)
        .saturating_add(maximum / 2)
        / maximum
}

pub(super) fn is_selection_copy_click(app: &App, mouse: MouseEvent) -> bool {
    app.selection.is_some() && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Right))
}

pub(super) fn consume_copy_click_release(app: &mut App, mouse: MouseEvent) -> bool {
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

pub(super) fn close_float_on_outside_click(app: &mut App, mouse: MouseEvent) -> bool {
    if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
        || !app
            .overlay_bounds
            .is_some_and(|area| !area.contains((mouse.column, mouse.row).into()))
    {
        return false;
    }

    match app.overlay {
        Some(Overlay::Composer) => {
            app.composer_mouse_selecting = false;
            app.dismiss_completions();
            app.overlay = None;
        }
        Some(Overlay::Delivery) => {
            app.delivery = None;
            app.overlay = Some(Overlay::Composer);
        }
        Some(Overlay::Command) => {
            app.reset_command_search();
            app.overlay = None;
        }
        Some(
            Overlay::Status
            | Overlay::Help
            | Overlay::Protocols
            | Overlay::Tasks
            | Overlay::Models
            | Overlay::Plugin,
        ) => app.overlay = None,
        Some(Overlay::Document) => {
            app.document = None;
            app.overlay = None;
        }
        Some(Overlay::Selector) => {
            let return_to_settings = matches!(
                app.selector.as_ref().map(|selector| &selector.kind),
                Some(SelectorKind::Environment {
                    return_to_settings: true
                })
            );
            app.selector = None;
            app.overlay = return_to_settings.then_some(Overlay::Settings);
        }
        Some(Overlay::Settings | Overlay::Text | Overlay::Oauth | Overlay::Terminal) | None => {
            return false;
        }
    }
    app.overlay_scroll = 0;
    app.selection = None;
    true
}

pub(super) fn handle_completion_mouse(app: &mut App, mouse: MouseEvent) -> bool {
    if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
        return false;
    }
    let Some(AppHit::Completion(index)) = hit_target(&app.hit_regions, mouse) else {
        return false;
    };
    app.select_completion(index)
}

pub(super) fn begin_direct_transcript_selection(app: &mut App, mouse: MouseEvent) -> bool {
    if app.overlay.is_some() || !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
        return false;
    }
    let index = match hit_target(&app.hit_regions, mouse) {
        Some(AppHit::Transcript(index)) => Some(index),
        None if !app.blocks.is_empty()
            && app
                .selectable
                .as_ref()
                .is_some_and(|surface| surface.area.contains((mouse.column, mouse.row).into())) =>
        {
            None
        }
        _ => return false,
    };
    if let Some(index) = index {
        if !app
            .blocks
            .get(index)
            .is_some_and(|block| matches!(block.kind, BlockKind::User | BlockKind::Assistant))
        {
            return false;
        }
        app.selected_block = index;
    }
    app.transcript_follow_tail = false;
    update_mouse_selection(app, mouse, false)
}

pub(super) fn open_status(app: &mut App) {
    app.overlay_scroll = 0;
    app.overlay = Some(Overlay::Status);
}

pub(super) async fn open_login(app: &mut App, catalog: &ModelCatalog) -> Action {
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
    for provider in WEB_SEARCH_LOGIN_PROVIDERS {
        if seen.insert((*provider).to_string()) {
            items.push(login_provider_item(provider, &current));
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

pub(super) fn login_provider_item(provider: &str, current: &str) -> SelectorItem {
    let description = if WEB_SEARCH_LOGIN_PROVIDERS.contains(&provider) {
        "Web search · API key".to_string()
    } else {
        match OauthProvider::from_id(provider) {
            Some(kind) if kind.offers_api_key() => format!("OAuth or API key · {}", kind.name()),
            Some(kind) => format!("OAuth · {}", kind.name()),
            None => "API key".to_string(),
        }
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

pub(super) fn open_login_method(app: &mut App, provider: String) -> Action {
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

pub(super) fn open_api_key_prompt(app: &mut App, provider: String) {
    let instructions = match provider.as_str() {
        "parallel" => concat!(
            "Create or copy a key at https://platform.parallel.ai/settings?tab=api-keys, ",
            "then paste it here."
        )
        .to_string(),
        "exa" => "Create or copy a key at https://dashboard.exa.ai/api-keys, then paste it here."
            .to_string(),
        _ => format!("Paste the API key for {provider}."),
    };
    let controls = action_hints(
        &app.keymap,
        &[("text", "confirm", "saves"), ("text", "cancel", "cancels")],
    );
    let message = if controls.is_empty() {
        instructions
    } else {
        format!("{instructions} {controls}.")
    };
    app.text_prompt = Some(TextPrompt {
        title: format!("API KEY · {provider}"),
        message,
        value: String::new(),
        secret: true,
        purpose: TextPurpose::ApiKey { provider },
    });
    app.overlay = Some(Overlay::Text);
}

pub(super) fn open_copilot_domain_prompt(app: &mut App) {
    app.text_prompt = Some(TextPrompt {
        title: "GITHUB COPILOT".to_string(),
        message: "GitHub Enterprise URL/domain (blank for github.com)".to_string(),
        value: String::new(),
        secret: false,
        purpose: TextPurpose::CopilotDomain,
    });
    app.overlay = Some(Overlay::Text);
}

pub(super) async fn open_environment(
    app: &mut App,
    environment: &AgentEnvironment,
    return_to_settings: bool,
) {
    let replace = app.keymap.key_hint("selector", "confirm").map_or_else(
        || "configured".to_string(),
        |key| format!("configured · {key} replaces value"),
    );
    let items = environment
        .names()
        .await
        .into_iter()
        .map(|name| SelectorItem {
            id: name.clone(),
            title: name,
            description: replace.clone(),
            search_text: None,
        })
        .collect();
    let hints = action_hints(
        &app.keymap,
        &[
            ("environment", "add", "add"),
            ("environment", "remove", "remove"),
        ],
    );
    app.selector = Some(SelectorState::new(
        SelectorKind::Environment { return_to_settings },
        panel_title("AGENT ENVIRONMENT", hints).trim().to_string(),
        items,
    ));
    app.overlay = Some(Overlay::Selector);
}

pub(super) async fn store_environment(
    app: &mut App,
    environment: &AgentEnvironment,
    name: String,
    value: String,
    return_to_settings: bool,
) {
    match environment.set(&name, value).await {
        Ok(()) => {
            if let Some(settings) = app.settings.as_mut() {
                settings.environment_count = environment.names().await.len();
            }
            app.set_flash(format!("Agent environment variable {name} saved"));
            if return_to_settings {
                open_environment(app, environment, true).await;
            } else {
                app.selector = None;
                app.overlay = None;
            }
        }
        Err(error) => {
            app.set_flash(format!("Could not save {name}: {error:#}"));
            open_environment_value_prompt(app, name, return_to_settings);
        }
    }
}

pub(super) async fn delete_environment(
    app: &mut App,
    environment: &AgentEnvironment,
    name: &str,
    return_to_settings: bool,
) {
    match environment.remove(name).await {
        Ok(true) => app.set_flash(format!("Agent environment variable {name} removed")),
        Ok(false) => app.set_flash(format!(
            "Agent environment variable {name} was not configured"
        )),
        Err(error) => app.set_flash(format!("Could not remove {name}: {error:#}")),
    }
    if let Some(settings) = app.settings.as_mut() {
        settings.environment_count = environment.names().await.len();
    }
    open_environment(app, environment, return_to_settings).await;
}

pub(super) fn open_environment_name_prompt(app: &mut App, return_to_settings: bool) {
    app.text_prompt = Some(TextPrompt {
        title: "ADD AGENT ENVIRONMENT".to_string(),
        message: "Variable name, for example NPM_TOKEN".to_string(),
        value: String::new(),
        secret: false,
        purpose: TextPurpose::EnvironmentName { return_to_settings },
    });
    app.overlay = Some(Overlay::Text);
}

pub(super) fn open_environment_value_prompt(app: &mut App, name: String, return_to_settings: bool) {
    app.text_prompt = Some(TextPrompt {
        title: format!("SET {name}"),
        message: "Value is stored privately and injected into future Agent shell commands."
            .to_string(),
        value: String::new(),
        secret: true,
        purpose: TextPurpose::EnvironmentValue {
            name,
            return_to_settings,
        },
    });
    app.overlay = Some(Overlay::Text);
}

pub(super) fn open_set_terminal_prompt(app: &mut App) {
    let controls = action_hints(
        &app.keymap,
        &[("text", "confirm", "saves"), ("text", "cancel", "cancels")],
    );
    let message = if controls.is_empty() {
        "Command used by :terminal, for example pwsh or bash.".to_string()
    } else {
        format!("Command used by :terminal, for example pwsh or bash. {controls}.")
    };
    app.text_prompt = Some(TextPrompt {
        title: "SET TERMINAL".to_string(),
        message,
        value: app.info.terminal.clone().unwrap_or_default(),
        secret: false,
        purpose: TextPurpose::SetTerminal,
    });
    app.overlay = Some(Overlay::Text);
}

pub(super) async fn save_terminal(app: &mut App, services: &LoopServices, command: String) {
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

pub(super) fn open_pty(app: &mut App) {
    let command = app.info.terminal.clone().unwrap_or_default();
    if command.trim().is_empty() {
        app.set_flash("No terminal configured. Run :set-terminal");
        return;
    }
    app.close_floats();
    let size = crossterm::terminal::size().unwrap_or((80, 24));
    let frame = Rect::new(0, 0, size.0, size.1);
    let area = overlay_area(frame, app, Overlay::Terminal);
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

pub(super) fn start_embedded_terminal(
    command: &str,
    cwd: &Path,
    inner: Rect,
) -> Result<EmbeddedTerminal> {
    let builder = terminal_command(command)?;
    EmbeddedTerminal::start(builder, cwd, inner.height, inner.width)
        .with_context(|| format!("cannot start `{command}`"))
}

pub(super) fn terminal_command(command: &str) -> Result<CommandBuilder> {
    let mut arguments = shell_words::split(command).context("cannot parse terminal command")?;
    if arguments.is_empty() {
        bail!("terminal command is empty");
    }
    let executable = arguments.remove(0);
    let mut builder = CommandBuilder::new(executable);
    builder.args(arguments);
    Ok(builder)
}

pub(super) fn handle_terminal_key(app: &mut App, key: KeyEvent) -> Result<bool> {
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

pub(super) fn handle_terminal_mouse(app: &mut App, mouse: MouseEvent) -> Result<()> {
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

pub(super) fn pty_finished(app: &mut App) -> Result<bool> {
    let Some(pty) = app.pty.as_mut() else {
        return Ok(false);
    };
    Ok(pty.terminal.try_wait()?.is_some())
}

pub(super) fn close_pty(app: &mut App, message: &str) {
    app.pty = None;
    if app.overlay == Some(Overlay::Terminal) {
        app.overlay = None;
    }
    app.selection = None;
    app.set_flash(message);
}

pub(super) async fn open_logout(app: &mut App, manager: &ConfigManager) -> Action {
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

pub(super) fn open_search(app: &mut App) {
    let items = app
        .blocks
        .iter()
        .enumerate()
        .filter(|(_, block)| block.kind != BlockKind::Process)
        .map(|(index, block)| {
            let search_text = block_search_text(block);
            SelectorItem {
                id: index.to_string(),
                title: block.title.clone(),
                description: search_line_preview(&search_text, "", 180),
                search_text: Some(search_text),
            }
        })
        .collect::<Vec<_>>();
    if items.is_empty() {
        app.set_flash("No conversation text to search");
        return;
    }
    app.selector = Some(SelectorState::new(SelectorKind::Search, "SEARCH", items));
    app.overlay = Some(Overlay::Selector);
}

pub(super) async fn open_resume(app: &mut App, services: &LoopServices) -> Action {
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

pub(super) fn resume_item(
    current: &str,
    session: SessionSummary,
    thinking: ThinkingLevel,
) -> SelectorItem {
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

pub(super) fn start_oauth(
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

pub(super) async fn store_api_key(
    app: &mut App,
    services: &LoopServices,
    provider: &str,
    key: String,
) {
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
            &services.manager,
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

pub(super) async fn logout_provider(app: &mut App, services: &LoopServices, provider: &str) {
    let current = services.runtime.session().model_settings().await;
    let result = async {
        services.manager.clear_api_key(provider).await?;
        let affects_current = current.provider == provider;
        if affects_current {
            // Do not leave a backend holding the removed stored credential if
            // any later settings or session write fails.
            services.runtime.set_backend(None, None).await;
            app.info.model_ready = false;
        }
        let credential_remains = services
            .manager
            .model_providers_with_credentials(&current.provider)
            .await
            .contains(provider);
        let clear_current = affects_current && !credential_remains;
        let defaults_warning = if clear_current {
            services
                .manager
                .clear_model_selection_for_provider(provider)
                .await
                .err()
                .map(|error| format!("{error:#}"))
        } else {
            None
        };
        let active = if clear_current {
            services
                .manager
                .for_session("", "", ThinkingLevel::Off)
                .await?
        } else {
            active_for_runtime(&services.manager, &services.runtime).await?
        };
        apply_active(
            app,
            &services.runtime,
            &services.manager,
            &services.catalog,
            &services.output,
            &active,
        )
        .await?;
        Ok::<_, anyhow::Error>((credential_remains, clear_current, defaults_warning))
    }
    .await;
    app.set_flash(match result {
        Ok((_, true, Some(warning))) => format!(
            "Removed stored credential and cleared the current model; saved defaults could not be cleared: {warning}"
        ),
        Ok((_, true, None)) => {
            format!("Removed stored credential for {provider} and cleared the current model")
        }
        Ok((true, false, _)) => format!(
            "Removed stored credential for {provider}; another credential source remains active"
        ),
        Ok((false, false, _)) => format!("Removed stored credential for {provider}"),
        Err(error) => format!("Could not log out {provider}: {error:#}"),
    });
}

pub(super) async fn open_models(
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
    let providers = manager
        .model_providers_with_credentials(&active.provider)
        .await;
    let selector = ModelSelector::load(catalog, &active, &providers, query).await;
    if selector.model_count() == 0 {
        app.set_flash("No authenticated model providers · use :login");
    }
    app.model_selector = Some(selector);
    app.overlay = Some(Overlay::Models);
}

pub(super) async fn select_model(
    app: &mut App,
    runtime: &AgentRuntime,
    manager: &Arc<ConfigManager>,
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
        apply_active(app, runtime, manager, catalog, output, &active).await?;
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

pub(super) async fn open_effort(app: &mut App, services: &LoopServices) {
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

pub(super) fn effort_selector(active: &ActiveSettings, model: &CatalogModel) -> SelectorState {
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

pub(super) async fn set_effort(
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
                &services.manager,
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

pub(super) async fn effective_thinking(
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

pub(super) fn is_ignored_tui_key(key: KeyEvent, selection_active: bool) -> bool {
    key_name(key) == "ctrl+c" && !selection_active
}

pub(super) fn key_name(key: KeyEvent) -> String {
    KeyStroke::from_event(key).map_or_else(String::new, |key| key.canonical())
}

pub(super) async fn save_settings(
    app: &mut App,
    runtime: &AgentRuntime,
    manager: &Arc<ConfigManager>,
    catalog: &ModelCatalog,
    output: &OutputStore,
    environment: &AgentEnvironment,
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
        apply_active(app, runtime, manager, catalog, output, &active).await?;
        app.settings = Some(SettingsState::load(active, catalog, environment).await);
        Ok::<_, anyhow::Error>(())
    }
    .await;
    app.set_flash(match result {
        Ok(()) => "Settings saved and applied".to_string(),
        Err(error) => format!("Settings were not fully applied: {error:#}"),
    });
}

pub(super) fn start_clipboard_image_read(
    app: &mut App,
    sender: mpsc::UnboundedSender<BackgroundEvent>,
) {
    if app.clipboard_image_loading {
        return;
    }
    app.clipboard_image_loading = true;
    tokio::spawn(async move {
        let result = tokio::task::spawn_blocking(clipboard::read_image_png)
            .await
            .context("clipboard image reader stopped unexpectedly")
            .and_then(|result| result);
        let _ = sender.send(BackgroundEvent::ClipboardImageRead(result));
    });
}

pub(super) fn start_clipboard_read(app: &mut App, sender: mpsc::UnboundedSender<BackgroundEvent>) {
    if app.clipboard_image_loading {
        return;
    }
    app.clipboard_image_loading = true;
    tokio::spawn(async move {
        let result = tokio::task::spawn_blocking(clipboard::read_preferred)
            .await
            .context("clipboard reader stopped unexpectedly")
            .and_then(|result| result);
        let _ = sender.send(BackgroundEvent::ClipboardRead(result));
    });
}

pub(super) fn start_completion_query(
    app: &mut App,
    sender: mpsc::UnboundedSender<BackgroundEvent>,
) {
    if app.overlay != Some(Overlay::Composer) {
        app.dismiss_completions();
        return;
    }
    let (generation, context) = app.begin_completion_query();
    let tui = app.tui.clone();
    app.completion_task = Some(tokio::spawn(async move {
        time::sleep(COMPLETION_DEBOUNCE).await;
        let result = tui.completions(&context).await;
        let _ = sender.send(BackgroundEvent::Completions { generation, result });
    }));
}

pub(super) fn start_catalog_refresh(
    app: &mut App,
    services: &LoopServices,
    sender: mpsc::UnboundedSender<BackgroundEvent>,
) {
    if app.catalog_refreshing {
        app.set_flash("The model catalogs are already refreshing");
        return;
    }
    app.catalog_refreshing = true;
    app.set_flash("Refreshing model catalogs…");
    spawn_catalog_refresh(services, sender, true, true);
}

fn start_background_catalog_refresh(
    app: &mut App,
    services: &LoopServices,
    sender: mpsc::UnboundedSender<BackgroundEvent>,
) {
    if app.catalog_refreshing {
        return;
    }
    app.catalog_refreshing = true;
    spawn_catalog_refresh(services, sender, false, false);
}

fn spawn_catalog_refresh(
    services: &LoopServices,
    sender: mpsc::UnboundedSender<BackgroundEvent>,
    force: bool,
    announced: bool,
) {
    let manager = services.manager.clone();
    tokio::spawn(async move {
        let result = manager.refresh_catalog(force).await;
        let _ = sender.send(BackgroundEvent::CatalogRefreshed {
            result: Box::new(result),
            announced,
        });
    });
}

pub(super) async fn finish_background(
    app: &mut App,
    services: &LoopServices,
    background_tx: mpsc::UnboundedSender<BackgroundEvent>,
    event: BackgroundEvent,
) {
    match event {
        BackgroundEvent::TurnStarted {
            prompt,
            submitted_image_ids,
            result,
        } => match result {
            Ok(()) => {
                app.discard_submitted_images(&submitted_image_ids);
                let _ = services.runtime.session().save_draft("").await;
            }
            Err(error) => {
                app.busy = false;
                app.busy_since = None;
                app.activity = None;
                app.restore_to_draft(&prompt);
                app.set_flash(format!("Cannot start turn: {error:#}"));
            }
        },
        BackgroundEvent::CatalogRefreshed { result, announced } => {
            app.catalog_refreshing = false;
            let result = async {
                let report = (*result)?;
                let active = active_for_runtime(&services.manager, &services.runtime).await?;
                apply_active(
                    app,
                    &services.runtime,
                    &services.manager,
                    &services.catalog,
                    &services.output,
                    &active,
                )
                .await?;
                if app.settings.is_some() {
                    app.settings = Some(
                        SettingsState::load(
                            active.clone(),
                            &services.catalog,
                            &services.environment,
                        )
                        .await,
                    );
                }
                if let Some(query) = app
                    .model_selector
                    .as_ref()
                    .map(|selector| selector.query().to_string())
                {
                    let providers = services
                        .manager
                        .model_providers_with_credentials(&active.provider)
                        .await;
                    app.model_selector = Some(
                        ModelSelector::load(&services.catalog, &active, &providers, query).await,
                    );
                }
                Ok::<_, anyhow::Error>(report)
            }
            .await;
            match result {
                Ok(report) if announced => {
                    if report.discovered_models > 0 {
                        app.set_flash(format!(
                            "Model catalogs refreshed; {} provider model(s) discovered",
                            report.discovered_models
                        ));
                    } else {
                        app.set_flash("Model catalogs refreshed");
                    }
                }
                Ok(_) => {}
                Err(error) => app.set_flash(format!("Catalog refresh failed: {error:#}")),
            }
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
                            &services.manager,
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
        BackgroundEvent::ClipboardImageRead(result) => {
            app.finish_clipboard_image_read(result);
        }
        BackgroundEvent::ClipboardRead(result) => {
            if app.finish_clipboard_read(result) {
                start_completion_query(app, background_tx);
            }
        }
        BackgroundEvent::Completions { generation, result } => {
            app.finish_completion_query(generation, result);
        }
    }
}

pub(super) async fn apply_active(
    app: &mut App,
    runtime: &AgentRuntime,
    manager: &Arc<ConfigManager>,
    catalog: &ModelCatalog,
    output: &OutputStore,
    active: &ActiveSettings,
) -> Result<()> {
    let configured = configured_backend(
        active,
        catalog,
        Some(runtime.session().id()),
        manager.clone(),
    )
    .await?;
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
    runtime.set_compaction_settings(active.compaction).await;
    runtime.refresh_context_estimate().await;
    output.set_limit(active.output_limit);
    app.info.provider.clone_from(&active.provider);
    app.info.model.clone_from(&active.model);
    app.info.thinking =
        effective_thinking(catalog, &active.provider, &active.model, active.thinking).await;
    app.info.context_window = context_window;
    app.info.model_ready = model_ready;
    app.info.provider_count = catalog.providers().await.len();
    app.info.compaction_enabled = active.compaction.enabled;
    app.info.terminal.clone_from(&active.terminal);
    Ok(())
}

pub(super) async fn active_for_runtime(
    manager: &ConfigManager,
    runtime: &AgentRuntime,
) -> Result<ActiveSettings> {
    let settings = runtime.session().model_settings().await;
    manager
        .for_session(&settings.provider, &settings.model, settings.thinking)
        .await
}
