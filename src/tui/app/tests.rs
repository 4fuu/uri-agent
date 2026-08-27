use super::*;
use crate::config::ValueSource;
use crate::protocol::{Protocol, ProtocolContext, ProtocolRequest};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::collections::BTreeMap;

struct LiveProtocol;

#[async_trait::async_trait]
impl Protocol for LiveProtocol {
    fn descriptor(&self) -> ProtocolDescriptor {
        ProtocolDescriptor {
            name: "live".to_string(),
            description: "Live protocol".to_string(),
            can_read: true,
            can_exec: false,
        }
    }

    async fn read(
        &self,
        _request: ProtocolRequest<'_>,
        _context: ProtocolContext,
    ) -> Result<Vec<u8>> {
        Ok(Vec::new())
    }
}

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
            context_accuracy: ContextAccuracy::Api,
            compaction_enabled: true,
            diagnostics_path: PathBuf::from("/tmp/uri-agent/diagnostics.jsonl"),
            terminal: None,
            key_display: KeyDisplayStyle::Text,
        },
        Keymap::with_defaults().unwrap(),
        String::new(),
        show_splash,
    )
}

fn test_app() -> App {
    test_app_with_splash(true)
}

fn apply_event(app: &mut App, sequence: u64, kind: EventKind) {
    app.apply(SessionEvent {
        sequence,
        at: chrono::Utc::now(),
        kind,
    });
}

fn edit_composer_with_default_keymap(app: &mut App, key: KeyEvent) {
    let action = app.keymap.action("composer", &key_name(key));
    app.edit_composer(key, action.as_deref());
}

#[tokio::test]
async fn protocol_surfaces_prefer_the_live_registry() {
    let session_id = format!("tui-protocol-test-{}", uuid::Uuid::now_v7().simple());
    let output = Arc::new(OutputStore::new(&session_id, 1024).await.unwrap());
    let output_directory = output.directory().to_path_buf();
    let mut registry = ProtocolRegistry::new(output, TaskManager::new());
    registry.register(LiveProtocol).unwrap();
    let mut app = test_app();
    app.protocol_source = Some(Arc::new(registry));

    assert_eq!(
        active_protocols(&app)
            .into_iter()
            .map(|descriptor| descriptor.name)
            .collect::<Vec<_>>(),
        ["live"]
    );
    let _ = tokio::fs::remove_dir_all(output_directory).await;
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
    assert_eq!(app.blocks.len(), 1);
    assert_eq!(app.blocks[0].kind, BlockKind::Compaction);
    app.apply_transient(EventKind::AssistantText {
        text: "replacement draft".into(),
    });
    assert_eq!(app.blocks[1].text, "replacement draft");
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
fn model_retry_clears_failed_stream_and_shows_the_retry_state() {
    let mut app = test_app();
    app.apply_transient(EventKind::AssistantText {
        text: "partial failed answer".into(),
    });

    apply_event(
        &mut app,
        0,
        EventKind::ModelRetry {
            attempt: 2,
            max_retries: 5,
            delay_ms: 2_500,
            reason: "network error during stream".into(),
        },
    );

    assert_eq!(app.blocks.len(), 1);
    assert_eq!(app.blocks[0].kind, BlockKind::Notice);
    assert_eq!(
        app.blocks[0].text,
        "network error during stream; retry 2/5 in 2.5s"
    );
    assert_eq!(
        app.activity.as_ref().unwrap().label(),
        "retrying 2/5 in 2.5s"
    );

    app.apply_transient(EventKind::AssistantText {
        text: "new attempt".into(),
    });
    apply_event(
        &mut app,
        1,
        EventKind::AssistantText {
            text: "recovered answer".into(),
        },
    );
    assert_eq!(app.blocks.len(), 2);
    assert_eq!(app.blocks[0].kind, BlockKind::Notice);
    assert_eq!(app.blocks[1].text, "recovered answer");
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
fn completed_turn_folds_its_process_and_keeps_the_final_response_visible() {
    let mut app = test_app();
    apply_event(
        &mut app,
        0,
        EventKind::User {
            text: "Inspect the renderer".into(),
        },
    );
    apply_event(
        &mut app,
        1,
        EventKind::AssistantReasoning {
            text: "Find the transcript owner".into(),
        },
    );
    apply_event(
        &mut app,
        2,
        EventKind::ToolCall {
            call_id: "call-1".into(),
            name: "read".into(),
            arguments: serde_json::json!({"uri": "file://src/tui.rs"}),
        },
    );
    apply_event(
        &mut app,
        3,
        EventKind::ToolResult {
            call_id: "call-1".into(),
            name: "read".into(),
            output: "source".into(),
            failed: false,
            protocol_help_required: false,
        },
    );
    apply_event(
        &mut app,
        4,
        EventKind::AssistantText {
            text: "The final response stays visible.".into(),
        },
    );
    apply_event(
        &mut app,
        5,
        EventKind::AssistantReasoning {
            text: "Reasoning emitted after the response text".into(),
        },
    );
    apply_event(&mut app, 6, EventKind::TurnFinished);

    assert_eq!(
        app.blocks
            .iter()
            .map(|block| block.kind)
            .collect::<Vec<_>>(),
        vec![
            BlockKind::User,
            BlockKind::Process,
            BlockKind::Reasoning,
            BlockKind::Tool,
            BlockKind::Reasoning,
            BlockKind::Assistant,
        ]
    );
    assert!(!app.blocks[1].expanded);
    assert!(app.blocks[5].turn_result);
    assert_eq!(app.filtered_indices(), vec![0, 1, 5]);

    let collapsed = render_to_string(&mut app, 100, 24);
    assert!(collapsed.contains("Process · 3 steps  ▸ Enter to expand"));
    assert!(collapsed.contains("The final response stays visible."));
    assert!(!collapsed.contains("Thought"));
    assert!(!collapsed.contains("Read src/tui.rs"));

    app.selected_block = 1;
    app.toggle_selected();
    let expanded = render_to_string(&mut app, 100, 24);
    assert!(expanded.contains("Process · 3 steps  ▾"));
    assert!(expanded.contains("◇ Thought  ▸ Enter to expand"));
    assert!(expanded.contains("✓ Read src/tui.rs  ▸"));
    assert!(expanded.contains("The final response stays visible."));

    app.selected_block = 1;
    app.toggle_selected();
    app.jump_to(JumpKind::Tool);
    assert!(app.blocks[1].expanded);
    assert_eq!(app.selected_block, 3);
}

#[test]
fn completed_turns_have_independent_process_folds() {
    let mut app = test_app();
    for (sequence, kind) in [
        (0, EventKind::User { text: "one".into() }),
        (
            1,
            EventKind::AssistantReasoning {
                text: "first process".into(),
            },
        ),
        (
            2,
            EventKind::AssistantText {
                text: "first result".into(),
            },
        ),
        (3, EventKind::TurnFinished),
    ] {
        apply_event(&mut app, sequence, kind);
    }
    app.selected_block = 1;
    app.toggle_selected();

    for (sequence, kind) in [
        (4, EventKind::User { text: "two".into() }),
        (
            5,
            EventKind::AssistantReasoning {
                text: "second process".into(),
            },
        ),
        (
            6,
            EventKind::AssistantText {
                text: "second result".into(),
            },
        ),
        (7, EventKind::TurnFinished),
    ] {
        apply_event(&mut app, sequence, kind);
    }

    let folds = app
        .blocks
        .iter()
        .filter(|block| block.kind == BlockKind::Process)
        .map(|block| block.expanded)
        .collect::<Vec<_>>();
    assert_eq!(folds, vec![true, false]);

    app.finish_hydration();
    assert!(
        app.blocks
            .iter()
            .filter(|block| block.kind == BlockKind::Process)
            .all(|block| !block.expanded)
    );
}

#[test]
fn searching_hidden_process_content_expands_its_turn() {
    let mut app = test_app();
    for (sequence, kind) in [
        (
            0,
            EventKind::User {
                text: "inspect".into(),
            },
        ),
        (
            1,
            EventKind::AssistantReasoning {
                text: "unique hidden reasoning".into(),
            },
        ),
        (
            2,
            EventKind::AssistantText {
                text: "done".into(),
            },
        ),
        (3, EventKind::TurnFinished),
    ] {
        apply_event(&mut app, sequence, kind);
    }

    assert!(!app.blocks[1].expanded);
    assert!(app.select_search_result(2));
    assert!(app.blocks[1].expanded);
    assert!(app.blocks[2].expanded);
    assert_eq!(app.selected_block, 2);
    assert!(render_to_string(&mut app, 80, 16).contains("unique hidden reasoning"));
}

#[test]
fn turn_without_intermediate_steps_has_no_empty_process() {
    let mut app = test_app();
    apply_event(
        &mut app,
        0,
        EventKind::User {
            text: "answer directly".into(),
        },
    );
    apply_event(
        &mut app,
        1,
        EventKind::AssistantText {
            text: "direct answer".into(),
        },
    );
    apply_event(&mut app, 2, EventKind::TurnFinished);

    assert_eq!(app.blocks.len(), 2);
    assert!(app.blocks[1].turn_result);
    assert!(
        !app.blocks
            .iter()
            .any(|block| block.kind == BlockKind::Process)
    );
    let rendered = render_to_string(&mut app, 80, 12);
    assert!(rendered.contains("direct answer"));
    assert!(!rendered.contains("Process ·"));
}

#[test]
fn failed_turn_folds_its_process_and_keeps_the_error_visible() {
    let mut app = test_app();
    apply_event(
        &mut app,
        0,
        EventKind::User {
            text: "run the check".into(),
        },
    );
    apply_event(
        &mut app,
        1,
        EventKind::AssistantReasoning {
            text: "inspect failure".into(),
        },
    );
    apply_event(
        &mut app,
        2,
        EventKind::Error {
            text: "check failed".into(),
        },
    );
    apply_event(&mut app, 3, EventKind::TurnFinished);

    assert_eq!(app.blocks[1].kind, BlockKind::Process);
    assert!(!app.blocks[1].expanded);
    assert_eq!(app.blocks[3].kind, BlockKind::Error);
    assert!(app.blocks[3].turn_result);
    let rendered = render_to_string(&mut app, 80, 12);
    assert!(rendered.contains("Process · 1 step"));
    assert!(rendered.contains("check failed"));
    assert!(!rendered.contains("inspect failure"));
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
    assert!(compact.contains("Nomodelconfigured.Run:login"));
    assert!(!rendered.contains("gpt-5.2"));
    assert!(!rendered.contains("openai/"));
}

#[test]
fn welcome_keeps_its_layout_with_a_centered_local_key_hint() {
    let mut app = test_app();
    let rendered = render_to_string(&mut app, 100, 24);
    assert!(rendered.contains("/workspace"));
    assert!(rendered.contains("test / model · effort off"));
    assert!(rendered.contains("Space compose · : commands · ? help"));
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
    assert!(lines[hint_row].contains("Space compose · : commands · ? help"));
    assert_eq!(lines[hint_row].find("Space compose"), Some(33));
    assert_eq!(lines.len() - hint_row - 1, 7);
}

#[test]
fn transient_notifications_overlay_without_reflowing_base_surfaces() {
    let mut welcome = test_app();
    let baseline = render_to_string(&mut welcome, 100, 24);
    let baseline_project_row = baseline
        .lines()
        .position(|line| line.contains("/workspace"))
        .unwrap();
    welcome.set_flash("Older notification");
    welcome.set_flash("Newer notification");
    let notified = render_to_string(&mut welcome, 100, 24);
    let notified_project_row = notified
        .lines()
        .position(|line| line.contains("/workspace"))
        .unwrap();
    assert_eq!(notified_project_row, baseline_project_row);

    let mut conversation = test_app();
    conversation.push(
        BlockKind::Assistant,
        "AGENT",
        "answer".to_string(),
        None,
        false,
        false,
    );
    render_to_string(&mut conversation, 100, 24);
    let baseline_height = conversation.transcript_height;
    conversation.set_flash("Older notification");
    conversation.set_flash("Newer notification");
    let notified = render_to_string(&mut conversation, 100, 24);
    assert_eq!(conversation.transcript_height, baseline_height);
    assert!(
        notified
            .lines()
            .nth(21)
            .unwrap()
            .contains("Newer notification")
    );
    assert!(
        notified
            .lines()
            .nth(22)
            .unwrap()
            .contains("Older notification")
    );
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
    assert!(!rendered.contains("space compose"));
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
fn bottom_notifications_wrap_completely_in_narrow_windows() {
    let mut app = test_app();
    app.skip_splash();
    app.push(
        BlockKind::Assistant,
        "AGENT",
        "answer".to_string(),
        None,
        false,
        false,
    );
    let message = "Detailed notification text wraps completely inside a narrow terminal window";
    app.set_flash(message);

    let width = 24;
    let height = 12;
    let notice_height = bottom_notice_lines(&[(message.to_string(), WARM)], width).len();
    assert!(notice_height > 1);

    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let rows = terminal
        .backend()
        .buffer()
        .content()
        .chunks(width as usize)
        .collect::<Vec<_>>();
    let notice_start = height as usize - notice_height - 1;
    let notice_rows = &rows[notice_start..notice_start + notice_height];
    let rendered_notice = notice_rows
        .iter()
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join(" ");

    for word in message.split_whitespace() {
        assert!(rendered_notice.contains(word), "missing {word:?}");
    }
    assert!(
        notice_rows
            .iter()
            .flat_map(|row| row.iter())
            .all(|cell| cell.bg == SURFACE)
    );
}

#[test]
fn new_notifications_stack_upward_above_fixed_statuses() {
    let mut app = test_app();
    app.skip_splash();
    app.push(
        BlockKind::Assistant,
        "AGENT",
        "answer".to_string(),
        None,
        false,
        false,
    );
    app.busy = true;
    app.activity = Some(Activity::Thinking);
    app.pending_messages.push(PendingMessage {
        id: 1,
        text: "follow up".to_string(),
        kind: PendingMessageKind::Queued,
    });
    render_to_string(&mut app, 100, 24);
    let baseline_height = app.transcript_height;
    app.set_flash("Older notification");
    app.set_flash("Newer notification");

    let rendered = render_to_string(&mut app, 100, 24);
    assert_eq!(app.transcript_height, baseline_height);
    let rows = rendered.lines().collect::<Vec<_>>();
    let row = |text: &str| {
        rows.iter()
            .position(|line| line.contains(text))
            .unwrap_or_else(|| panic!("missing {text:?}"))
    };

    assert!(row("Newer notification") < row("Older notification"));
    assert!(row("Older notification") < row("1 pending"));
    let activity_row = rows.len() - 2;
    assert!(row("1 pending") < activity_row);
    assert!(rows[activity_row].contains("thinking"));
    let footer = rows.last().unwrap();
    assert!(footer.starts_with("model · effort off"));
    assert!(!footer.contains("thinking"));
    assert!(footer.trim_end().ends_with("········ 0.0%/128k"));
}

#[test]
fn flash_residence_time_scales_with_character_count() {
    assert_eq!(flash_duration(""), FLASH_MIN_DURATION);
    assert_eq!(flash_duration("é"), flash_duration("e\u{301}"));

    let short = "Saved";
    let long = "Long notification text ".repeat(8);
    assert!(flash_duration(&long) > flash_duration(short));
    assert_eq!(flash_duration(&"x".repeat(1_000)), FLASH_MAX_DURATION);

    let elapsed = flash_duration(short) + Duration::from_millis(10);
    let mut app = test_app();
    app.flashes.push(FlashNotice {
        message: short.to_string(),
        created: Instant::now() - elapsed,
    });
    assert!(app.visible_flashes().next().is_none());
    app.flashes.push(FlashNotice {
        message: long,
        created: Instant::now() - elapsed,
    });
    assert!(app.visible_flashes().next().is_some());
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
fn single_line_overflow_preserves_input_tail_and_scrolls_selected_text() {
    assert_eq!(single_line_tail("prefix  模型", 7), "…  模型");
    assert_eq!(single_line_tail("a  b", 4), "a  b");

    assert_eq!(marquee_preview("abcdef", 4, 0), "abc…");
    assert_eq!(
        marquee_preview("abcdef", 4, MARQUEE_HOLD_FRAMES + MARQUEE_STEP_FRAMES),
        "…bc…"
    );
    assert_eq!(
        marquee_preview("abcdef", 4, MARQUEE_HOLD_FRAMES + 3 * MARQUEE_STEP_FRAMES),
        "…def"
    );
    assert_eq!(list_cell("模型名称", 5, false, 0).width(), 5);
    assert_eq!(single_line_preview("e\u{301}clair", 2), "e\u{301}…");
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
            total: 2_600,
            context: true,
            provider: "test".to_string(),
            model: "model".to_string(),
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
    assert!(rendered.contains("STATUS"));
    assert!(!rendered.contains("toggle"));
    assert!(!rendered.contains("Esc close"));
    assert!(rendered.contains("test / model · effort off"));
    assert!(rendered.contains("/tmp/uri-agent/diagnostics.jsonl"));
    assert!(rendered.contains("26k / 262k · 10.0%"));
    assert!(rendered.contains("read 500 · write 0 · last hit 25.0%"));
    assert!(rendered.contains("$0.0123"));
    // Usage events remain available in status without adding transcript blocks.
    assert_eq!(app.blocks.len(), 1);
}

#[test]
fn context_meter_marks_only_idle_estimates_and_animates_only_while_busy() {
    let mut app = test_app();
    app.info.context_tokens = 64_000;
    app.info.context_accuracy = ContextAccuracy::Estimated;
    app.push(
        BlockKind::Assistant,
        "AGENT",
        "answer".to_string(),
        None,
        false,
        true,
    );
    let footer = |app: &mut App| {
        render_to_string(app, 100, 24)
            .lines()
            .last()
            .unwrap()
            .trim_end()
            .to_string()
    };

    app.frame = 0;
    let idle = footer(&mut app);
    assert!(idle.ends_with("≈50.0%/128k"));
    app.frame = 1;
    assert_eq!(footer(&mut app), idle);

    app.busy = true;
    app.activity = Some(Activity::Thinking);
    app.frame = 0;
    let active = footer(&mut app);
    assert!(active.ends_with("50.0%/128k"));
    assert!(!active.contains('≈'));
    app.frame = 1;
    assert_ne!(footer(&mut app), active);
}

#[test]
fn context_status_distinguishes_api_estimates_and_unknown_usage() {
    let mut app = test_app();
    app.info.context_tokens = 12_800;

    app.info.context_accuracy = ContextAccuracy::Api;
    assert!(context_status(&app, 10.0).starts_with("12k /"));
    app.info.context_accuracy = ContextAccuracy::Estimated;
    assert!(context_status(&app, 10.0).starts_with("≈12k /"));
    app.busy = true;
    assert!(context_status(&app, 10.0).starts_with("12k /"));
    assert!(!context_status(&app, 10.0).contains('≈'));
    app.info.context_accuracy = ContextAccuracy::Unknown;
    assert!(context_status(&app, 10.0).starts_with("unknown /"));
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
    for padding_row in [user_rows[0] - 1, user_rows[1] + 1] {
        assert_eq!(
            terminal.backend().buffer()[(10, padding_row)].bg,
            USER_SURFACE
        );
        assert!(!app.hit_regions.iter().any(|region| {
            region.area.y == padding_row && matches!(region.target, AppHit::Transcript(_))
        }));
    }
    let assistant_row = app
        .hit_regions
        .iter()
        .find_map(|region| (region.target == AppHit::Transcript(1)).then_some(region.area.y))
        .unwrap();
    assert_eq!(
        terminal.backend().buffer()[(10, assistant_row)].bg,
        Color::Reset
    );
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
    assert_eq!(
        terminal.backend().buffer()[(10, assistant_row)].bg,
        Color::Reset
    );
    app.selected_block = 2;
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    assert_eq!(
        terminal.backend().buffer()[(10, reasoning_row)].bg,
        ROW_ACTIVE
    );
}

#[test]
fn user_wide_character_trailing_cell_is_styled_for_scroll_cleanup() {
    let mut user_app = test_app();
    user_app.push(BlockKind::User, "YOU", "a你".into(), None, false, false);
    user_app.skip_splash();
    let backend = TestBackend::new(8, 5);
    let mut terminal = Terminal::new(backend).unwrap();
    let user_frame = terminal
        .draw(|frame| render(frame, &mut user_app))
        .unwrap()
        .buffer
        .clone();
    let user_row = user_app
        .hit_regions
        .iter()
        .find_map(|region| (region.target == AppHit::Transcript(0)).then_some(region.area.y))
        .unwrap();

    assert_eq!(user_frame[(2, user_row)].symbol(), "你");
    assert_eq!(user_frame[(2, user_row)].bg, USER_SURFACE);
    assert_eq!(user_frame[(3, user_row)].symbol(), " ");
    assert_eq!(user_frame[(3, user_row)].bg, USER_SURFACE);

    let mut replacement_app = test_app();
    replacement_app.push(
        BlockKind::Assistant,
        "AGENT",
        "filler".into(),
        None,
        false,
        false,
    );
    replacement_app.push(
        BlockKind::Assistant,
        "AGENT",
        "好".into(),
        None,
        false,
        false,
    );
    replacement_app.skip_splash();
    let backend = TestBackend::new(8, 5);
    let mut terminal = Terminal::new(backend).unwrap();
    let replacement_frame = terminal
        .draw(|frame| render(frame, &mut replacement_app))
        .unwrap()
        .buffer
        .clone();
    let replacement_row = replacement_app
        .hit_regions
        .iter()
        .find_map(|region| (region.target == AppHit::Transcript(1)).then_some(region.area.y))
        .unwrap();

    assert_eq!(replacement_row, user_row);
    assert_eq!(replacement_frame[(1, replacement_row)].symbol(), "好");
    assert!(
        user_frame
            .diff(&replacement_frame)
            .iter()
            .any(|(x, y, cell)| *x == 3 && *y == replacement_row && cell.bg == Color::Reset)
    );
}

#[test]
fn follow_up_user_prompt_has_an_extra_blank_row_after_the_previous_result() {
    let mut app = test_app();
    app.push(
        BlockKind::Assistant,
        "AGENT",
        "Previous answer.".into(),
        None,
        false,
        true,
    );
    app.blocks[0].turn_result = true;
    app.push(
        BlockKind::User,
        "YOU",
        "Follow-up question.".into(),
        None,
        false,
        false,
    );
    app.push(
        BlockKind::Reasoning,
        "THINKING",
        "Check the new turn.".into(),
        None,
        false,
        false,
    );
    app.push(
        BlockKind::Assistant,
        "AGENT",
        "Next answer.".into(),
        None,
        false,
        true,
    );
    app.apply(SessionEvent {
        sequence: 0,
        at: chrono::Utc::now(),
        kind: EventKind::TurnFinished,
    });
    app.skip_splash();
    let backend = TestBackend::new(80, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let previous_row = app
        .hit_regions
        .iter()
        .find_map(|region| (region.target == AppHit::Transcript(0)).then_some(region.area.y))
        .unwrap();
    let user_row = app
        .hit_regions
        .iter()
        .find_map(|region| (region.target == AppHit::Transcript(1)).then_some(region.area.y))
        .unwrap();
    let process_row = app
        .hit_regions
        .iter()
        .find_map(|region| (region.target == AppHit::Transcript(2)).then_some(region.area.y))
        .unwrap();
    let result_row = app
        .hit_regions
        .iter()
        .find_map(|region| (region.target == AppHit::Transcript(4)).then_some(region.area.y))
        .unwrap();

    assert_eq!(user_row, previous_row + 3);
    assert_eq!(process_row, user_row + 3);
    assert_eq!(result_row, process_row + 2);
    for row in [user_row - 1, user_row, user_row + 1] {
        assert_eq!(terminal.backend().buffer()[(1, row)].bg, USER_SURFACE);
        assert_eq!(terminal.backend().buffer()[(78, row)].bg, USER_SURFACE);
    }
    assert_eq!(
        terminal.backend().buffer()[(1, user_row + 2)].bg,
        Color::Reset
    );
    assert_eq!(
        terminal.backend().buffer()[(1, previous_row + 1)].bg,
        Color::Reset
    );
    for row in [previous_row + 1, user_row - 1, user_row + 1, user_row + 2] {
        assert!(!app.hit_regions.iter().any(|region| {
            region.area.y == row && matches!(region.target, AppHit::Transcript(_))
        }));
    }
    assert!(transcript_needs_gap(
        BlockKind::Assistant,
        true,
        BlockKind::User,
        false,
    ));
    assert!(transcript_needs_gap(
        BlockKind::User,
        false,
        BlockKind::Process,
        false
    ));
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
        let background = if symbol == "U" {
            USER_SURFACE
        } else {
            Color::Reset
        };
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
fn transcript_copy_omits_visual_soft_wraps() {
    for (kind, text, width, separator, expected) in [
        (
            BlockKind::Assistant,
            "abc 中文内容",
            10,
            TextRowSeparator::None,
            "abc 中文内容",
        ),
        (
            BlockKind::User,
            "abc defgh",
            8,
            TextRowSeparator::Space,
            "abc defgh",
        ),
        (
            BlockKind::Assistant,
            "abc  \ndef",
            10,
            TextRowSeparator::Newline,
            "abc\ndef",
        ),
    ] {
        let mut app = test_app();
        app.push(
            kind,
            if kind == BlockKind::User {
                "YOU"
            } else {
                "AGENT"
            },
            text.to_string(),
            None,
            false,
            kind == BlockKind::Assistant,
        );
        render_to_string(&mut app, width, 12);
        let mut rows = app
            .hit_regions
            .iter()
            .filter_map(|region| (region.target == AppHit::Transcript(0)).then_some(region.area.y));
        let first = rows.next().expect("first wrapped transcript row");
        let last = rows.next_back().unwrap_or(first);
        assert_eq!(last, first + 1);
        let surface = app.selectable.as_ref().unwrap();
        let selection = TextSelection {
            start: (surface.area.x + 1, first),
            end: (surface.area.right().saturating_sub(1), last),
        };

        assert_eq!(
            surface.row_separators[(first - surface.area.y) as usize],
            separator
        );
        assert_eq!(selected_surface_text(surface, selection), expected);
    }
}

#[test]
fn virtual_transcript_tail_supports_direct_reverse_drag_selection() {
    let mut app = test_app();
    for index in 0..30 {
        app.push(
            BlockKind::Assistant,
            "AGENT",
            format!("message {index}"),
            None,
            false,
            true,
        );
    }
    render_to_string(&mut app, 80, 12);
    app.scroll_transcript(isize::MAX);
    render_to_string(&mut app, 80, 12);
    let final_row = app
        .hit_regions
        .iter()
        .find_map(|region| (region.target == AppHit::Transcript(29)).then_some(region.area.y))
        .unwrap();
    let tail_row = app.transcript_height.saturating_sub(1) as u16;
    assert!(tail_row > final_row);
    assert!(!app.hit_regions.iter().any(|region| {
        region.area.y == tail_row && matches!(region.target, AppHit::Transcript(_))
    }));
    let down = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 20,
        row: tail_row,
        modifiers: KeyModifiers::NONE,
    };
    let drag = MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column: 1,
        row: final_row,
        modifiers: KeyModifiers::NONE,
    };

    assert!(begin_direct_transcript_selection(&mut app, down));
    assert!(update_mouse_selection(&mut app, drag, true));
    assert!(
        selected_surface_text(app.selectable.as_ref().unwrap(), app.selection.unwrap())
            .contains("message 29")
    );
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
    assert!(!transcript_needs_gap(
        BlockKind::User,
        false,
        BlockKind::Tool,
        false
    ));
    assert!(!transcript_needs_gap(
        BlockKind::Reasoning,
        false,
        BlockKind::Assistant,
        false
    ));
    assert!(!transcript_needs_gap(
        BlockKind::Reasoning,
        false,
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

    assert_eq!(app.blocks[0].kind, BlockKind::Process);
    assert!(!app.blocks[0].expanded);
    assert!(app.blocks[3].turn_result);
    let rendered = render_to_string(&mut app, 80, 12);
    let process_row = app
        .hit_regions
        .iter()
        .find_map(|region| (region.target == AppHit::Transcript(0)).then_some(region.area.y))
        .unwrap();
    let final_row = app
        .hit_regions
        .iter()
        .find_map(|region| (region.target == AppHit::Transcript(3)).then_some(region.area.y))
        .unwrap();

    assert_eq!(final_row, process_row + 2);
    assert!(
        rendered
            .lines()
            .nth((process_row + 1) as usize)
            .unwrap()
            .trim()
            .is_empty()
    );
    assert!(transcript_needs_gap(
        BlockKind::Reasoning,
        false,
        BlockKind::Assistant,
        true
    ));
}

#[test]
fn final_assistant_response_has_one_blank_row_after_the_user_when_there_is_no_process() {
    let mut app = test_app();
    apply_event(
        &mut app,
        0,
        EventKind::User {
            text: "hello".into(),
        },
    );
    apply_event(
        &mut app,
        1,
        EventKind::AssistantText {
            text: "Hello! How can I help?".into(),
        },
    );
    apply_event(&mut app, 2, EventKind::TurnFinished);

    assert_eq!(app.blocks.len(), 2);
    assert_eq!(app.blocks[0].kind, BlockKind::User);
    assert_eq!(app.blocks[1].kind, BlockKind::Assistant);
    assert!(app.blocks[1].turn_result);
    assert!(
        !app.blocks
            .iter()
            .any(|block| block.kind == BlockKind::Process)
    );

    app.skip_splash();
    let backend = TestBackend::new(80, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let user_row = app
        .hit_regions
        .iter()
        .find_map(|region| (region.target == AppHit::Transcript(0)).then_some(region.area.y))
        .unwrap();
    let result_row = app
        .hit_regions
        .iter()
        .find_map(|region| (region.target == AppHit::Transcript(1)).then_some(region.area.y))
        .unwrap();

    assert_eq!(result_row, user_row + 3);
    assert_eq!(
        terminal.backend().buffer()[(1, user_row + 1)].bg,
        USER_SURFACE
    );
    assert_eq!(
        terminal.backend().buffer()[(1, user_row + 2)].bg,
        Color::Reset
    );
    assert!(!app.hit_regions.iter().any(|region| {
        region.area.y == user_row + 2 && matches!(region.target, AppHit::Transcript(_))
    }));
    assert!(transcript_needs_gap(
        BlockKind::User,
        false,
        BlockKind::Assistant,
        true,
    ));
    assert!(transcript_needs_gap(
        BlockKind::User,
        false,
        BlockKind::Error,
        true,
    ));
}

#[test]
fn assistant_transcript_renders_markdown_instead_of_source_markers() {
    let mut app = test_app();
    app.push(
        BlockKind::Assistant,
        "AGENT",
        "# Result\n\n- **done** with `cargo test`\n\n[Details](https://example.com)".to_string(),
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
        overlay_area(Rect::new(0, 0, 100, 24), &app, Overlay::Status),
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
    assert!(rendered.contains("Space compose · : commands · ? help"));
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
    let (prompt, images) = app.submit().unwrap();
    assert_eq!(prompt, "first\nsecond");
    assert!(images.is_empty());
    assert!(app.draft_text().is_empty());
    assert!(app.overlay.is_none());
    assert!(!app.animations_paused());
}

#[test]
fn composer_applies_generic_completion_ranges_and_rejects_stale_results() {
    let mut app = test_app();
    app.overlay = Some(Overlay::Composer);
    app.input.insert_str("open @@old");
    app.completion_generation = 4;
    let result = TuiCompletions {
        replacement: TuiTextRange {
            start: TuiTextPosition { line: 0, column: 5 },
            end: TuiTextPosition {
                line: 0,
                column: 10,
            },
        },
        items: vec![crate::plugin::TuiCompletionItem {
            insert_text: "@@session-id ".to_string(),
            label: "Earlier session".to_string(),
            description: "session-id".to_string(),
        }],
    };

    app.finish_completion_query(3, Ok(Some(result.clone())));
    assert!(app.completions.is_none());
    app.finish_completion_query(4, Ok(Some(result)));
    assert!(app.accept_completion());
    assert_eq!(app.draft_text(), "open @@session-id ");
    assert!(app.completions.is_none());
}

#[test]
fn composer_completion_popup_is_visible_and_mouse_selectable() {
    let mut app = test_app();
    app.overlay = Some(Overlay::Composer);
    app.completions = Some(ComposerCompletions {
        result: TuiCompletions {
            replacement: TuiTextRange {
                start: TuiTextPosition { line: 0, column: 0 },
                end: TuiTextPosition { line: 0, column: 0 },
            },
            items: vec![crate::plugin::TuiCompletionItem {
                insert_text: "@src/main.rs ".to_string(),
                label: "src/main.rs".to_string(),
                description: "src".to_string(),
            }],
        },
        selected: 0,
    });
    assert!(!app.animations_paused());

    let rendered = render_to_string(&mut app, 80, 24);

    assert!(rendered.contains("REFERENCES · Up/Down select · Enter/Tab insert"));
    assert!(rendered.contains("src/main.rs"));
    assert!(
        app.hit_regions
            .iter()
            .any(|region| region.target == AppHit::Completion(0))
    );
    let region = app
        .hit_regions
        .iter()
        .find(|region| region.target == AppHit::Completion(0))
        .copied()
        .unwrap();
    assert!(handle_completion_mouse(
        &mut app,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: region.area.x,
            row: region.area.y,
            modifiers: KeyModifiers::NONE,
        }
    ));
    assert_eq!(app.draft_text(), "@src/main.rs ");
}

#[test]
fn smart_clipboard_result_inserts_text_or_an_image_into_the_composer() {
    let mut pasted_from_main = test_app();
    handle_paste(&mut pasted_from_main, "main paste".to_string());
    assert!(pasted_from_main.overlay == Some(Overlay::Composer));
    assert_eq!(pasted_from_main.draft_text(), "main paste");

    let mut app = test_app();
    app.overlay = Some(Overlay::Composer);
    app.clipboard_image_loading = true;
    assert!(app.finish_clipboard_read(Ok(clipboard::ClipboardContent::Text("pasted".to_string()))));
    assert_eq!(app.draft_text(), "pasted");

    app.clipboard_image_loading = true;
    assert!(!app.finish_clipboard_read(Ok(clipboard::ClipboardContent::Image(vec![1, 2]))));
    assert_eq!(app.draft_text(), "pasted[Image #1]");
    let (prompt, images) = app.composer_submission();
    assert_eq!(prompt, "pasted[Image #1]");
    assert_eq!(images, [ImageAttachment::png(vec![1, 2])]);
}

#[test]
fn multiline_paste_inserts_newlines_without_sending() {
    let mut app = test_app();
    app.overlay = Some(Overlay::Composer);
    handle_paste(&mut app, "first\r\nsecond\rthird".to_string());
    assert!(app.overlay == Some(Overlay::Composer));
    assert_eq!(app.draft_text(), "first\nsecond\nthird");
    assert!(!app.busy);

    let mut from_main = test_app();
    handle_paste(&mut from_main, "alpha\nbeta\n".to_string());
    assert!(from_main.overlay == Some(Overlay::Composer));
    assert_eq!(from_main.draft_text(), "alpha\nbeta\n");
    assert!(from_main.submit().is_some());

    let mut clipboard = test_app();
    clipboard.overlay = Some(Overlay::Composer);
    clipboard.clipboard_image_loading = true;
    assert!(
        clipboard.finish_clipboard_read(Ok(clipboard::ClipboardContent::Text(
            "one\r\ntwo".to_string()
        )))
    );
    assert_eq!(clipboard.draft_text(), "one\ntwo");
    assert!(clipboard.overlay == Some(Overlay::Composer));
}

#[test]
fn unbracketed_paste_keys_are_not_treated_as_submit() {
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    let typed = |character| KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE);
    assert_eq!(
        pasted_text_from_keys(&[typed('h'), typed('i'), enter]),
        None
    );
    assert_eq!(
        pasted_text_from_keys(&[typed('y'), typed('e'), typed('s'), enter]),
        None
    );
    assert_eq!(pasted_text_from_keys(&[enter]), None);
    assert_eq!(pasted_text_from_keys(&[enter, enter]), None);
    assert_eq!(
        pasted_text_from_keys(&[typed('h'), typed('e'), typed('l'), typed('l'), typed('o')]),
        None
    );
    assert_eq!(
        pasted_text_from_keys(&[
            typed('h'),
            typed('e'),
            typed('l'),
            typed('l'),
            typed('o'),
            enter,
            typed('w'),
            typed('o'),
            typed('r'),
            typed('l'),
            typed('d'),
        ]),
        Some("hello\nworld".to_string())
    );
    assert_eq!(
        pasted_text_from_keys(&[
            typed('l'),
            typed('i'),
            typed('n'),
            typed('e'),
            typed(' '),
            typed('o'),
            typed('n'),
            typed('e'),
            enter,
        ]),
        Some("line one\n".to_string())
    );
    assert_eq!(
        pasted_text_from_keys(&[
            typed('a'),
            KeyEvent::new(KeyCode::Char('\r'), KeyModifiers::NONE),
            typed('b')
        ]),
        Some("a\nb".to_string())
    );
}

#[test]
fn paste_burst_skips_windows_key_releases_and_keeps_following_events() {
    let press = |code| KeyEvent::new(code, KeyModifiers::NONE);
    let release = |code| KeyEvent::new_with_kind(code, KeyModifiers::NONE, KeyEventKind::Release);

    // Windows consoles deliver a release for every pasted key; the burst must
    // survive them so the multi-line text is still recognized as a paste.
    let (keys, rest) = split_paste_burst(
        press(KeyCode::Char('a')),
        vec![
            Event::Key(release(KeyCode::Char('a'))),
            Event::Key(press(KeyCode::Enter)),
            Event::Key(release(KeyCode::Enter)),
            Event::Key(press(KeyCode::Char('b'))),
        ],
    );
    assert_eq!(
        keys.iter().map(|key| key.code).collect::<Vec<_>>(),
        vec![KeyCode::Char('a'), KeyCode::Enter, KeyCode::Char('b')]
    );
    assert!(rest.is_empty());
    assert_eq!(pasted_text_from_keys(&keys), Some("a\nb".to_string()));

    // A lone key followed by its release stays ordinary typing, not a paste.
    let (keys, rest) = split_paste_burst(
        press(KeyCode::Char('a')),
        vec![Event::Key(release(KeyCode::Char('a')))],
    );
    assert_eq!(keys.len(), 1);
    assert!(rest.is_empty());
    assert_eq!(pasted_text_from_keys(&keys), None);

    // The first non-textual event ends the burst and keeps its order.
    let (keys, rest) = split_paste_burst(
        press(KeyCode::Char('a')),
        vec![
            Event::Key(press(KeyCode::Esc)),
            Event::Key(press(KeyCode::Char('b'))),
        ],
    );
    assert_eq!(keys.len(), 1);
    assert_eq!(
        rest.iter()
            .filter_map(|event| match event {
                Event::Key(key) => Some(key.code),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec![KeyCode::Esc, KeyCode::Char('b')]
    );
}

#[test]
fn clipboard_image_token_is_inserted_at_the_caret() {
    let mut app = test_app();
    app.overlay = Some(Overlay::Composer);
    app.insert_composer_text("beforeafter");
    app.input.move_cursor(CursorMove::Jump(0, 6));

    app.finish_clipboard_image_read(Ok(vec![1]));

    assert_eq!(app.draft_text(), "before[Image #1]after");
    assert_eq!(app.input.cursor(), (0, 16));
}

#[test]
fn image_tokens_are_crossed_and_deleted_atomically() {
    let mut app = test_app();
    app.overlay = Some(Overlay::Composer);
    app.insert_clipboard_image(vec![1]);

    app.input.move_cursor(CursorMove::Head);
    app.edit_composer(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), None);
    assert_eq!(app.input.cursor(), (0, "[Image #1]".chars().count()));
    app.edit_composer(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), None);
    assert_eq!(app.input.cursor(), (0, 0));

    app.edit_composer(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE), None);
    assert!(app.draft_text().is_empty());
    assert!(app.composer_submission().1.is_empty());

    app.input.undo();
    app.sync_composer_chrome();
    assert_eq!(app.draft_text(), "[Image #1]");
    app.input.move_cursor(CursorMove::End);
    app.edit_composer(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE), None);
    assert!(app.draft_text().is_empty());
    assert!(app.composer_submission().1.is_empty());
}

#[test]
fn deleting_a_selection_that_touches_a_token_removes_the_whole_image() {
    let mut app = test_app();
    app.overlay = Some(Overlay::Composer);
    app.insert_composer_text("before ");
    app.insert_clipboard_image(vec![1]);
    app.insert_composer_text(" after");
    app.input.move_cursor(CursorMove::Jump(0, 8));
    app.input.start_selection();
    app.input.move_cursor(CursorMove::Jump(0, 10));

    app.edit_composer(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE), None);

    assert_eq!(app.draft_text(), "before  after");
    assert!(app.composer_submission().1.is_empty());
}

#[test]
fn deleting_one_image_token_keeps_the_remaining_token_number_on_submit() {
    let mut app = test_app();
    app.overlay = Some(Overlay::Composer);
    app.insert_clipboard_image(vec![1]);
    app.insert_composer_text(" ");
    app.insert_clipboard_image(vec![2]);
    app.input.move_cursor(CursorMove::Jump(0, 4));
    app.edit_composer(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE), None);

    assert_eq!(app.draft_text(), " [Image #2]");
    let (prompt, images) = app.composer_submission();
    assert_eq!(prompt, " [Image #2]");
    assert_eq!(images, [ImageAttachment::png(vec![2])]);

    let (prompt, images) = app.submit().unwrap();
    assert_eq!(prompt, " [Image #2]");
    assert_eq!(images, [ImageAttachment::png(vec![2])]);
    app.busy = false;
    app.restore_to_draft(&prompt);
    assert_eq!(app.draft_text(), " [Image #2]");
    assert_eq!(app.composer_submission(), (prompt, images));
}

#[test]
fn submitted_image_cleanup_preserves_images_added_while_the_turn_starts() {
    let mut app = test_app();
    app.overlay = Some(Overlay::Composer);
    app.insert_composer_text("inspect ");
    app.insert_clipboard_image(vec![1]);
    let (prompt, _) = app.submit().unwrap();
    assert_eq!(prompt, "inspect [Image #1]");
    let submitted_image_ids = app.composer_images.keys().copied().collect::<Vec<_>>();

    app.insert_composer_text("next ");
    app.insert_clipboard_image(vec![2]);
    app.discard_submitted_images(&submitted_image_ids);

    assert_eq!(app.draft_text(), "next [Image #2]");
    assert_eq!(app.composer_submission().1, [ImageAttachment::png(vec![2])]);
    app.discard_submitted_images(&[2]);
    assert!(app.composer_images.is_empty());
    assert_eq!(app.next_composer_image_id, 1);
}

#[test]
fn clipboard_images_wait_for_the_next_submit() {
    let mut app = test_app();
    let image = ImageAttachment::png(vec![1]);
    app.overlay = Some(Overlay::Composer);
    app.insert_composer_text("inspect this ");
    app.insert_clipboard_image(vec![1]);
    app.clipboard_image_loading = true;
    assert!(app.submit().is_none());
    assert_eq!(app.draft_text(), "inspect this [Image #1]");
    assert_eq!(
        app.composer_submission().1.as_slice(),
        std::slice::from_ref(&image)
    );

    app.clipboard_image_loading = false;
    let (prompt, images) = app.submit().unwrap();
    assert_eq!(prompt, "inspect this [Image #1]");
    assert_eq!(images, [image]);
    assert!(app.draft_text().is_empty());

    app.busy = false;
    app.restore_to_draft(&prompt);
    assert_eq!(app.draft_text(), "inspect this [Image #1]");
    assert_eq!(app.composer_submission().0, prompt);
}

#[test]
fn clipboard_read_completion_inserts_a_token_without_a_success_notice() {
    let mut app = test_app();
    app.overlay = Some(Overlay::Composer);
    app.clipboard_image_loading = true;

    app.finish_clipboard_image_read(Ok(vec![1, 2, 3]));

    assert!(!app.clipboard_image_loading);
    assert_eq!(app.draft_text(), "[Image #1]");
    assert!(app.visible_flashes().next().is_none());

    app.clipboard_image_loading = true;
    app.finish_clipboard_image_read(Err(anyhow!("clipboard unavailable")));
    assert!(!app.clipboard_image_loading);
    assert_eq!(app.draft_text(), "[Image #1]");
    assert!(
        app.visible_flashes()
            .next_back()
            .unwrap()
            .contains("failed")
    );
}

#[test]
fn composer_shows_image_tokens_without_external_image_chrome() {
    let mut app = test_app();
    app.skip_splash();
    app.overlay = Some(Overlay::Composer);
    app.insert_clipboard_image(vec![1, 2, 3]);
    let backend = TestBackend::new(100, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let cells = terminal.backend().buffer().content();
    let symbols = cells.iter().map(|cell| cell.symbol()).collect::<Vec<_>>();
    let token = "[Image #1]"
        .chars()
        .map(|c| c.to_string())
        .collect::<Vec<_>>();
    let start = symbols
        .windows(token.len())
        .position(|window| {
            window
                .iter()
                .zip(&token)
                .all(|(cell, token)| *cell == token)
        })
        .expect("image token should be rendered");
    for cell in &cells[start..start + token.len()] {
        assert_eq!(cell.fg, WARM);
        assert_eq!(cell.bg, ROW_ACTIVE);
    }
    let rendered = cells
        .chunks(100)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");

    assert!(rendered.contains("[Image #1]"));
    assert!(!rendered.contains("MESSAGE · 1 image"));
    assert!(!rendered.contains("image ready"));
    assert!(!rendered.contains("Alt+Backspace"));
}

#[test]
fn mouse_clicks_snap_to_image_token_edges() {
    let mut app = test_app();
    app.skip_splash();
    app.overlay = Some(Overlay::Composer);
    app.insert_clipboard_image(vec![1]);
    render_to_string(&mut app, 80, 24);
    let inner = app.composer_view.as_ref().unwrap().inner;

    assert!(handle_composer_mouse(
        &mut app,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: inner.x + 1,
            row: inner.y,
            modifiers: KeyModifiers::NONE,
        }
    ));
    assert_eq!(app.input.cursor(), (0, 0));
    app.composer_mouse_selecting = false;
    assert!(handle_composer_mouse(
        &mut app,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: inner.x + 9,
            row: inner.y,
            modifiers: KeyModifiers::NONE,
        }
    ));
    assert_eq!(app.input.cursor(), (0, 10));
}

#[test]
fn restoring_pending_images_merges_and_renumbers_composer_tokens() {
    let mut app = test_app();
    app.insert_clipboard_image(vec![2]);

    app.restore_pending_to_draft("queued [Image #1]", vec![ImageAttachment::png(vec![1])]);

    assert_eq!(app.draft_text(), "queued [Image #1]\n\n[Image #2]");
    let (prompt, images) = app.composer_submission();
    assert_eq!(prompt, "queued [Image #1]\n\n[Image #2]");
    assert_eq!(
        images,
        [ImageAttachment::png(vec![1]), ImageAttachment::png(vec![2])]
    );
}

#[test]
fn restoring_pending_images_with_gapped_markers_rebases_the_draft() {
    let mut app = test_app();
    app.insert_clipboard_image(vec![9]);

    app.restore_pending_to_draft("queued [Image #3]", vec![ImageAttachment::png(vec![3])]);

    assert_eq!(app.draft_text(), "queued [Image #1]\n\n[Image #2]");
    let (prompt, images) = app.composer_submission();
    assert_eq!(prompt, "queued [Image #1]\n\n[Image #2]");
    assert_eq!(
        images,
        [ImageAttachment::png(vec![3]), ImageAttachment::png(vec![9])]
    );
}

#[test]
fn persisted_drafts_drop_image_tokens_without_binary_attachments() {
    assert_eq!(
        strip_image_references("before [Image #1]\nafter [Image #20]"),
        "before \nafter "
    );
    assert_eq!(
        strip_image_references("keep [Image #2 1280x720] text"),
        "keep  text"
    );
}

#[test]
fn clipboard_image_tokens_include_png_dimensions() {
    let mut app = test_app();
    app.overlay = Some(Overlay::Composer);
    let png = crate::clipboard::encode_png(12, 8, vec![0; 12 * 8 * 4]).unwrap();
    app.insert_clipboard_image(png.clone());

    assert_eq!(app.draft_text(), "[Image #1 12x8]");
    let (prompt, images) = app.composer_submission();
    assert_eq!(prompt, "[Image #1 12x8]");
    assert_eq!(images, [ImageAttachment::png(png.clone())]);

    let mut restored = test_app();
    restored.restore_pending_to_draft("queued [Image #3 12x8]", vec![ImageAttachment::png(png)]);
    assert_eq!(restored.draft_text(), "queued [Image #1 12x8]");

    app.input.move_cursor(CursorMove::End);
    app.edit_composer(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE), None);
    assert!(app.draft_text().is_empty());
    assert!(app.composer_submission().1.is_empty());
}

#[test]
fn image_token_spans_accept_optional_dimensions() {
    let spans = image_token_spans("see [Image #2 1920x1080] and [Image #3]");
    assert_eq!(spans.len(), 2);
    assert_eq!(spans[0].id, 2);
    assert_eq!(
        spans[0].end_col - spans[0].start_col,
        "[Image #2 1920x1080]".chars().count()
    );
    assert_eq!(spans[1].id, 3);
    assert!(image_token_spans("[Image #1 extra]").is_empty());
}

#[test]
fn running_composer_enter_opens_queue_or_guidance_delivery() {
    let mut app = test_app();
    app.skip_splash();
    app.busy = true;
    app.activity = Some(Activity::Thinking);
    app.overlay = Some(Overlay::Composer);
    app.input.insert_str("adjust the implementation");
    let image = ImageAttachment::png(vec![1, 2, 3]);
    app.insert_clipboard_image(vec![1, 2, 3]);

    assert!(app.submit().is_none());
    assert!(app.overlay == Some(Overlay::Delivery));
    assert!(app.animations_paused());
    assert!(matches!(
        confirm_delivery(&app),
        Action::Enqueue {
            prompt,
            images,
            kind: PendingMessageKind::Queued,
        } if prompt == "adjust the implementation[Image #1]"
            && images.as_slice() == std::slice::from_ref(&image)
    ));

    app.delivery.as_mut().unwrap().selected = 1;
    assert!(matches!(
        confirm_delivery(&app),
        Action::Enqueue {
            prompt,
            images,
            kind: PendingMessageKind::Guidance,
        } if prompt == "adjust the implementation[Image #1]"
            && images.as_slice() == std::slice::from_ref(&image)
    ));
    let rendered = render_to_string(&mut app, 100, 24);
    assert!(rendered.contains("SEND WHILE RUNNING"));
    assert!(rendered.contains("Queue"));
    assert!(rendered.contains("Guidance"));
    assert_eq!(
        app.hit_regions
            .iter()
            .filter(|region| matches!(region.target, AppHit::Delivery(_)))
            .count(),
        2
    );
}

#[test]
fn composer_previews_pending_messages_and_restores_them_in_order() {
    let mut app = test_app();
    app.skip_splash();
    app.busy = true;
    app.overlay = Some(Overlay::Composer);
    app.input.insert_str("current draft");
    app.pending_messages = vec![
        PendingMessage {
            id: 0,
            text: "queued follow-up".to_string(),
            kind: PendingMessageKind::Queued,
        },
        PendingMessage {
            id: 1,
            text: "guide now".to_string(),
            kind: PendingMessageKind::Guidance,
        },
    ];

    let rendered = render_to_string(&mut app, 100, 24);
    assert!(rendered.contains("QUEUE"));
    assert!(rendered.contains("queued follow-up"));
    assert!(rendered.contains("GUIDE"));
    assert!(rendered.contains("guide now"));
    assert_eq!(
        app.keymap.action("composer", "alt+up").as_deref(),
        Some("restore_pending")
    );
    assert_eq!(
        app.keymap.action("composer", "alt+enter").as_deref(),
        Some("upgrade_pending")
    );

    app.restore_to_draft("guide now");
    app.restore_to_draft("queued follow-up");
    assert_eq!(
        app.draft_text(),
        "queued follow-up\n\nguide now\n\ncurrent draft"
    );
}

#[test]
fn composer_boundary_arrows_reach_the_start_and_end_of_the_draft() {
    let mut app = test_app();
    app.input.insert_str("first");
    app.input.insert_newline();
    app.input.insert_str("second");

    app.input.move_cursor(CursorMove::Jump(0, 3));
    edit_composer_with_default_keymap(&mut app, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.input.cursor(), (0, 0));

    app.input.move_cursor(CursorMove::Jump(1, 2));
    edit_composer_with_default_keymap(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.input.cursor(), (1, 6));
}

#[test]
fn composer_common_navigation_and_editing_shortcuts_work() {
    let mut app = test_app();
    app.input.insert_str("alpha beta");
    app.input.insert_newline();
    app.input.insert_str("omega");

    edit_composer_with_default_keymap(
        &mut app,
        KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL),
    );
    assert_eq!(app.input.cursor(), (0, 0));
    edit_composer_with_default_keymap(&mut app, KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL));
    assert_eq!(app.input.cursor(), (1, 5));

    edit_composer_with_default_keymap(
        &mut app,
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL),
    );
    assert_eq!(app.draft_text(), "alpha beta\n");
    edit_composer_with_default_keymap(
        &mut app,
        KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL),
    );
    assert_eq!(app.draft_text(), "alpha beta\nomega");
    edit_composer_with_default_keymap(
        &mut app,
        KeyEvent::new(
            KeyCode::Char('Z'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ),
    );
    assert_eq!(app.draft_text(), "alpha beta\n");
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
    assert_eq!(
        app.keymap.action("composer", "ctrl+c").as_deref(),
        Some("copy")
    );
    assert_eq!(app.keymap.action("command", "ctrl+c"), None);
    assert_eq!(
        app.keymap.action("selection", "ctrl+c").as_deref(),
        Some("copy")
    );
}

#[test]
fn selection_releases_command_and_global_panel_shortcuts() {
    let mut app = test_app();
    let selection = TextSelection {
        start: (0, 0),
        end: (1, 0),
    };

    for (key, expected_action) in [
        (":", "command"),
        ("f1", "help"),
        ("f2", "settings"),
        ("f3", "model"),
        ("f4", "status"),
        ("ctrl+p", "protocols"),
        ("ctrl+t", "tasks"),
    ] {
        app.selection = Some(selection);

        assert!(!handle_selection_key(&mut app, key));
        assert!(app.selection.is_none());
        assert_eq!(
            app.keymap.action_chain(&["main"], key).as_deref(),
            Some(expected_action)
        );
    }
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
    let newline_hint = if cfg!(windows) {
        "Enter send · Ctrl+Enter newline"
    } else {
        "Enter send · Shift+Enter newline"
    };
    assert!(rows[23].contains(newline_hint));
    assert!(!rows[23].contains("Esc keep draft"));
    assert!(rows[23].ends_with("╯  "));
}

#[test]
fn composer_footer_does_not_show_image_count_or_removal_shortcut() {
    let mut app = test_app();
    app.overlay = Some(Overlay::Composer);
    app.insert_clipboard_image(vec![1, 2, 3]);

    let rendered = render_to_string(&mut app, 80, 24);

    assert!(rendered.contains("[Image #1]"));
    assert!(!rendered.contains("MESSAGE · 1 image"));
    assert!(!rendered.contains("Alt+Backspace"));
}

#[test]
fn composer_soft_wraps_long_input_and_tracks_the_visual_caret() {
    let mut app = test_app();
    app.skip_splash();
    app.overlay = Some(Overlay::Composer);
    app.input.insert_str("x".repeat(75));
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal.draw(|frame| render(frame, &mut app)).unwrap();

    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .chunks(80)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>();
    terminal.backend_mut().assert_cursor_position((4, 18));
    assert!(rendered[17].contains(&"x".repeat(74)));
    assert_eq!(
        terminal.backend().buffer().cell((3, 18)).unwrap().symbol(),
        "x"
    );
}

#[test]
fn composer_mouse_click_moves_the_caret_and_drag_selects_editable_text() {
    let mut app = test_app();
    app.overlay = Some(Overlay::Composer);
    app.input.insert_str("alpha beta");
    render_to_string(&mut app, 80, 24);
    let inner = app.composer_view.as_ref().unwrap().inner;
    let event = |kind, column| MouseEvent {
        kind,
        column: inner.x + column,
        row: inner.y,
        modifiers: KeyModifiers::NONE,
    };

    assert!(handle_composer_mouse(
        &mut app,
        event(MouseEventKind::Down(MouseButton::Left), 6)
    ));
    assert!(handle_composer_mouse(
        &mut app,
        event(MouseEventKind::Up(MouseButton::Left), 6)
    ));
    assert_eq!(app.input.cursor(), (0, 6));
    assert!(!app.input.is_selecting());

    assert!(handle_composer_mouse(
        &mut app,
        event(MouseEventKind::Down(MouseButton::Left), 0)
    ));
    assert!(handle_composer_mouse(
        &mut app,
        event(MouseEventKind::Drag(MouseButton::Left), 5)
    ));
    assert!(handle_composer_mouse(
        &mut app,
        event(MouseEventKind::Up(MouseButton::Left), 5)
    ));
    assert_eq!(composer_selected_text(&app.input).as_deref(), Some("alpha"));

    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    terminal
        .backend_mut()
        .assert_cursor_position((inner.x + 5, inner.y));
    assert_eq!(terminal.backend().buffer()[(inner.x, inner.y)].bg, ACCENT);

    edit_composer_with_default_keymap(
        &mut app,
        KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE),
    );
    assert_eq!(app.draft_text(), "X beta");
}

#[test]
fn composer_mouse_selection_follows_soft_wrapped_text() {
    let mut app = test_app();
    app.overlay = Some(Overlay::Composer);
    app.input.insert_str("x".repeat(75));
    render_to_string(&mut app, 80, 24);
    let inner = app.composer_view.as_ref().unwrap().inner;
    let event = |kind, column, row| MouseEvent {
        kind,
        column: inner.x + column,
        row: inner.y + row,
        modifiers: KeyModifiers::NONE,
    };

    assert!(handle_composer_mouse(
        &mut app,
        event(MouseEventKind::Down(MouseButton::Left), 0, 0)
    ));
    assert!(handle_composer_mouse(
        &mut app,
        event(MouseEventKind::Drag(MouseButton::Left), 1, 1)
    ));
    assert!(handle_composer_mouse(
        &mut app,
        event(MouseEventKind::Up(MouseButton::Left), 1, 1)
    ));
    assert_eq!(composer_selected_text(&app.input), Some("x".repeat(75)));
}

#[test]
fn composer_selection_extracts_multiline_unicode_text() {
    let mut input = TextArea::from(["你好吗", "second", "终"]);
    input.move_cursor(CursorMove::Jump(0, 1));
    input.start_selection();
    input.move_cursor(CursorMove::Jump(2, 1));

    assert_eq!(
        composer_selected_text(&input).as_deref(),
        Some("好吗\nsecond\n终")
    );
}

#[test]
fn live_reasoning_preview_follows_its_tail() {
    let mut app = test_app();
    app.apply(SessionEvent {
        sequence: 0,
        at: chrono::Utc::now(),
        kind: EventKind::User {
            text: "question".into(),
        },
    });
    let reasoning = (0..30)
        .map(|index| format!("thought-{index:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.apply_transient(EventKind::AssistantReasoning { text: reasoning });

    let rendered = render_to_string(&mut app, 100, 40);
    assert!(rendered.contains("Thinking…"));
    assert!(rendered.contains("… 6 earlier lines"));
    assert!(!rendered.contains("thought-05"));
    assert!(rendered.contains("thought-06"));
    assert!(rendered.contains("thought-29"));

    app.apply_transient(EventKind::AssistantReasoning {
        text: "\nthought-30".into(),
    });
    let rendered = render_to_string(&mut app, 100, 40);
    assert!(rendered.contains("… 7 earlier lines"));
    assert!(!rendered.contains("thought-06"));
    assert!(rendered.contains("thought-07"));
    assert!(rendered.contains("thought-30"));

    app.apply_transient(EventKind::AssistantText {
        text: "answer".into(),
    });
    app.selected_block = 1;
    app.toggle_selected();
    let rendered = render_to_string(&mut app, 100, 40);
    assert!(rendered.contains("Thought"));
    assert!(rendered.contains("thought-00"));
    assert!(rendered.contains("… 7 more lines"));
    assert!(!rendered.contains("thought-30"));
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
            assert!(rendered.contains("O or right-click opens full"));
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
fn document_overlay_copies_the_full_body_with_c() {
    let mut app = test_app();
    let thought = format!("{}more thought", "thinking line\n".repeat(40));
    app.push(
        BlockKind::Reasoning,
        "THINKING",
        thought,
        None,
        false,
        false,
    );
    app.open_selected_document();

    assert_eq!(app.keymap.action("document", "c").as_deref(), Some("copy"));
    let expected = block_document(&app.blocks[0]);
    assert_eq!(
        app.document.as_ref().map(|(_, body)| body.as_str()),
        Some(expected.as_str())
    );
    assert!(expected.contains("more thought"));
    assert!(expected.lines().count() > 20);

    let rendered = render_to_string(&mut app, 72, 14);
    assert!(rendered.contains("C copy"), "{rendered}");
    assert!(keymap_help(&app.keymap).contains("DOCUMENT"));
    assert!(keymap_help(&app.keymap).contains("copy"));
}

#[test]
fn clicking_outside_dismisses_cancelable_floats() {
    let mut app = test_app();
    let bounds = Rect::new(10, 5, 60, 16);
    let outside_click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: bounds.x.saturating_sub(1),
        row: bounds.y,
        modifiers: KeyModifiers::NONE,
    };

    for overlay in [
        Overlay::Status,
        Overlay::Help,
        Overlay::Protocols,
        Overlay::Tasks,
        Overlay::Models,
        Overlay::Plugin,
    ] {
        app.overlay = Some(overlay);
        app.overlay_bounds = Some(bounds);
        app.overlay_scroll = 6;
        app.selection = Some(TextSelection {
            start: (20, 8),
            end: (24, 8),
        });

        assert!(close_float_on_outside_click(&mut app, outside_click));
        assert!(app.overlay.is_none());
        assert_eq!(app.overlay_scroll, 0);
        assert!(app.selection.is_none());
    }
}

#[test]
fn clicking_outside_uses_each_float_cancel_semantics() {
    let mut app = test_app();
    let bounds = Rect::new(10, 5, 60, 16);
    let outside_click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: bounds.x.saturating_sub(1),
        row: bounds.y,
        modifiers: KeyModifiers::NONE,
    };

    app.input.insert_str("keep this draft");
    app.composer_mouse_selecting = true;
    app.overlay = Some(Overlay::Composer);
    app.overlay_bounds = Some(bounds);
    assert!(close_float_on_outside_click(&mut app, outside_click));
    assert!(app.overlay.is_none());
    assert_eq!(app.draft_text(), "keep this draft");
    assert!(!app.composer_mouse_selecting);

    app.delivery = Some(DeliveryState { selected: 1 });
    app.overlay = Some(Overlay::Delivery);
    app.overlay_bounds = Some(bounds);
    assert!(close_float_on_outside_click(&mut app, outside_click));
    assert!(app.overlay == Some(Overlay::Composer));
    assert!(app.delivery.is_none());

    app.command_query = "status".to_string();
    app.command_selected = 2;
    app.command_stem = Some("stat".to_string());
    app.overlay = Some(Overlay::Command);
    app.overlay_bounds = Some(bounds);
    assert!(close_float_on_outside_click(&mut app, outside_click));
    assert!(app.overlay.is_none());
    assert!(app.command_query.is_empty());
    assert_eq!(app.command_selected, 0);
    assert!(app.command_stem.is_none());

    app.document = Some(("DETAIL".to_string(), "body".to_string()));
    app.overlay = Some(Overlay::Document);
    app.overlay_bounds = Some(bounds);
    assert!(close_float_on_outside_click(&mut app, outside_click));
    assert!(app.overlay.is_none());
    assert!(app.document.is_none());

    app.selector = Some(SelectorState::new(
        SelectorKind::Search,
        "SEARCH",
        Vec::new(),
    ));
    app.overlay = Some(Overlay::Selector);
    app.overlay_bounds = Some(bounds);
    assert!(close_float_on_outside_click(&mut app, outside_click));
    assert!(app.overlay.is_none());
    assert!(app.selector.is_none());
}

#[test]
fn clicking_outside_does_not_dismiss_editing_or_active_work_floats() {
    let mut app = test_app();
    let bounds = Rect::new(10, 5, 60, 16);
    let outside_click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: bounds.x.saturating_sub(1),
        row: bounds.y,
        modifiers: KeyModifiers::NONE,
    };

    for overlay in [
        Overlay::Settings,
        Overlay::Text,
        Overlay::Oauth,
        Overlay::Terminal,
    ] {
        app.overlay = Some(overlay);
        app.overlay_bounds = Some(bounds);

        assert!(!close_float_on_outside_click(&mut app, outside_click));
        assert!(app.overlay == Some(overlay));
    }
}

#[test]
fn clicking_inside_or_using_another_mouse_button_does_not_dismiss_a_float() {
    let mut app = test_app();
    let bounds = Rect::new(10, 5, 60, 16);
    app.overlay = Some(Overlay::Status);
    app.overlay_bounds = Some(bounds);

    assert!(!close_float_on_outside_click(
        &mut app,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: bounds.x,
            row: bounds.y,
            modifiers: KeyModifiers::NONE,
        }
    ));
    assert!(!close_float_on_outside_click(
        &mut app,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: bounds.x.saturating_sub(1),
            row: bounds.y,
            modifiers: KeyModifiers::NONE,
        }
    ));
    assert!(app.overlay == Some(Overlay::Status));
}

#[test]
fn transcript_right_click_opens_during_streaming_without_changing_folded_state() {
    let mut app = test_app();
    app.push(
        BlockKind::Reasoning,
        "THINKING",
        "full thought".to_string(),
        None,
        false,
        false,
    );
    app.apply(SessionEvent {
        sequence: 0,
        at: chrono::Utc::now(),
        kind: EventKind::User {
            text: "keep working".into(),
        },
    });
    app.apply_transient(EventKind::AssistantText {
        text: "streaming now".into(),
    });
    assert!(app.busy);
    render_to_string(&mut app, 100, 24);
    let area = app
        .hit_regions
        .iter()
        .find_map(|region| (region.target == AppHit::Transcript(0)).then_some(region.area))
        .expect("history block mouse region");
    let right_click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Right),
        column: area.x,
        row: area.y,
        modifiers: KeyModifiers::NONE,
    };

    assert!(activate_transcript_mouse(&mut app, right_click));

    assert!(!app.blocks[0].expanded);
    assert!(app.overlay == Some(Overlay::Document));
    assert!(app.document.is_some());
    assert_eq!(app.selected_block, 0);
    assert!(!app.transcript_follow_tail);
}

#[test]
fn transcript_single_click_toggles_immediately_each_time() {
    let mut app = test_app();
    app.push(
        BlockKind::Reasoning,
        "THINKING",
        "full thought".to_string(),
        None,
        false,
        false,
    );
    render_to_string(&mut app, 100, 24);
    let area = app
        .hit_regions
        .iter()
        .find_map(|region| (region.target == AppHit::Transcript(0)).then_some(region.area))
        .expect("history block mouse region");
    let left_click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: area.x,
        row: area.y,
        modifiers: KeyModifiers::NONE,
    };

    assert!(activate_transcript_mouse(&mut app, left_click));
    assert!(app.blocks[0].expanded);

    assert!(activate_transcript_mouse(&mut app, left_click));
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
    assert_eq!(bounded_index(0, -3, 5), 0);
    assert_eq!(bounded_index(0, 3, 5), 3);
    assert_eq!(bounded_index(3, 3, 5), 4);
    assert_eq!(bounded_index(8, 1, 0), 0);

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
            BlockKind::Assistant,
            "AGENT",
            format!("message {index}"),
            None,
            false,
            true,
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
        BlockKind::Assistant,
        "AGENT",
        "message 30".to_string(),
        None,
        false,
        true,
    );
    render_to_string(&mut app, 80, 12);
    assert_eq!(app.transcript_offset, tail_offset - 3);
    assert_eq!(app.selected_block, selected);

    app.scroll_transcript(4);
    render_to_string(&mut app, 80, 12);
    assert_eq!(app.transcript_offset, tail_offset + 1);
    assert!(app.transcript_follow_tail);

    app.push(
        BlockKind::Assistant,
        "AGENT",
        "message 31".to_string(),
        None,
        false,
        true,
    );
    render_to_string(&mut app, 80, 12);
    assert_eq!(app.transcript_offset, tail_offset + 2);

    render_to_string(&mut app, 80, 40);
    assert_eq!(app.transcript_offset, 0);
}

#[test]
fn transcript_pages_by_the_rendered_viewport_without_moving_selection() {
    let mut app = test_app();
    for index in 0..50 {
        app.push(
            BlockKind::Assistant,
            "AGENT",
            format!("message {index}"),
            None,
            false,
            true,
        );
    }
    app.selected_block = 49;
    render_to_string(&mut app, 80, 12);
    let live_tail = app.transcript_offset;
    let page_rows = app.transcript_height;

    app.page_transcript(-1);
    assert_eq!(app.transcript_offset, live_tail - page_rows);
    assert_eq!(app.selected_block, 49);
    assert!(!app.transcript_follow_tail);

    app.page_transcript(1);
    assert_eq!(app.transcript_offset, live_tail);
    assert_eq!(app.selected_block, 49);
    assert!(app.transcript_follow_tail);

    render_to_string(&mut app, 80, 20);
    assert_eq!(app.transcript_height, 19);
    app.page_transcript(-1);
    assert_eq!(
        app.transcript_offset,
        transcript_live_tail(app.transcript_rows, app.transcript_height) - 19
    );
}

#[test]
fn overflowing_transcript_renders_an_inset_scrollbar_outside_copied_cells() {
    let mut app = test_app();
    for index in 0..50 {
        app.push(
            BlockKind::Assistant,
            "AGENT",
            format!("message {index}"),
            None,
            false,
            true,
        );
    }

    let rendered = render_to_string(&mut app, 40, 12);
    let content_rows = rendered
        .lines()
        .take(app.transcript_height)
        .collect::<Vec<_>>();
    assert!(content_rows.iter().any(|row| row.ends_with('│')));
    assert!(content_rows.iter().any(|row| row.ends_with('┃')));
    assert!(content_rows.last().unwrap().ends_with('│'));
    let scrollbar_area = app.transcript_scrollbar_area.unwrap();
    assert_eq!(
        app.selectable.as_ref().unwrap().area.right(),
        scrollbar_area.x
    );
    assert!(
        app.selectable
            .as_ref()
            .unwrap()
            .cells
            .iter()
            .all(|row| row.last().is_some_and(|cell| cell == " "))
    );

    app.scroll_transcript(isize::MAX);
    let rendered = render_to_string(&mut app, 40, 12);
    let content_rows = rendered
        .lines()
        .take(app.transcript_height)
        .collect::<Vec<_>>();
    assert_eq!(
        app.transcript_offset,
        transcript_reading_end(app.transcript_rows, app.transcript_height)
    );
    assert!(content_rows.last().unwrap().ends_with('┃'));

    app.page_transcript(-1);
    let rendered = render_to_string(&mut app, 40, 12);
    let content_rows = rendered
        .lines()
        .take(app.transcript_height)
        .collect::<Vec<_>>();
    assert!(!content_rows.last().unwrap().ends_with('┃'));

    let mut short = test_app();
    short.push(
        BlockKind::Assistant,
        "AGENT",
        "short".to_string(),
        None,
        false,
        true,
    );
    let rendered = render_to_string(&mut short, 40, 12);
    assert!(
        rendered
            .lines()
            .take(short.transcript_height)
            .all(|row| !row.ends_with(['│', '┃']))
    );
}

#[test]
fn transcript_scrollbar_handles_track_clicks_and_thumb_drags() {
    let mut app = test_app();
    for index in 0..50 {
        app.push(
            BlockKind::Assistant,
            "AGENT",
            format!("message {index}"),
            None,
            false,
            true,
        );
    }
    render_to_string(&mut app, 40, 12);
    let area = app.transcript_scrollbar_area.unwrap();
    let metrics = transcript_scrollbar_metrics(&app).unwrap();
    let live_tail = transcript_live_tail(app.transcript_rows, app.transcript_height);
    let reading_end = metrics.reading_end;
    assert!(reading_end > live_tail);
    assert_eq!(app.transcript_offset, live_tail);
    app.selection = Some(TextSelection {
        start: (1, area.y),
        end: (2, area.y),
    });

    let thumb_row = area.y + metrics.thumb_start as u16;
    assert!(handle_transcript_scrollbar_mouse(
        &mut app,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x,
            row: thumb_row,
            modifiers: KeyModifiers::NONE,
        }
    ));
    assert!(app.selection.is_none());
    assert!(app.transcript_scrollbar_drag.is_some());
    assert_eq!(app.transcript_offset, live_tail);

    assert!(handle_transcript_scrollbar_mouse(
        &mut app,
        MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: area.x.saturating_sub(5),
            row: area.y,
            modifiers: KeyModifiers::NONE,
        }
    ));
    assert_eq!(app.transcript_offset, 0);
    assert!(!app.transcript_follow_tail);
    assert!(handle_transcript_scrollbar_mouse(
        &mut app,
        MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        }
    ));
    assert!(app.transcript_scrollbar_drag.is_none());

    assert!(handle_transcript_scrollbar_mouse(
        &mut app,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x,
            row: area.bottom().saturating_sub(1),
            modifiers: KeyModifiers::NONE,
        }
    ));
    assert_eq!(app.transcript_offset, reading_end);
    assert!(!app.transcript_follow_tail);
    assert!(handle_transcript_scrollbar_mouse(
        &mut app,
        MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: area.x,
            row: area.bottom().saturating_sub(1),
            modifiers: KeyModifiers::NONE,
        }
    ));

    render_to_string(&mut app, 40, 12);
    let area = app.transcript_scrollbar_area.unwrap();
    let metrics = transcript_scrollbar_metrics(&app).unwrap();
    assert_eq!(metrics.thumb_start, metrics.max_thumb_start);
    assert!(handle_transcript_scrollbar_mouse(
        &mut app,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x,
            row: area.y + metrics.thumb_start as u16,
            modifiers: KeyModifiers::NONE,
        }
    ));
    assert_eq!(app.transcript_offset, reading_end);
}

#[test]
fn transcript_text_selection_stops_before_the_scrollbar_column() {
    let mut app = test_app();
    for index in 0..50 {
        app.push(
            BlockKind::Assistant,
            "AGENT",
            format!("message {index}"),
            None,
            false,
            true,
        );
    }
    render_to_string(&mut app, 40, 12);
    let area = app.transcript_scrollbar_area.unwrap();
    let selectable = app.selectable.as_ref().unwrap().area;
    let down = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: selectable.x + 1,
        row: selectable.y,
        modifiers: KeyModifiers::NONE,
    };
    let drag = MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column: area.x,
        row: selectable.bottom().saturating_sub(1),
        modifiers: KeyModifiers::NONE,
    };
    assert!(update_mouse_selection(&mut app, down, false));
    assert!(update_mouse_selection(&mut app, drag, false));
    assert_eq!(
        app.selection.unwrap().end.0,
        selectable.right().saturating_sub(1)
    );

    let backend = TestBackend::new(40, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let area = app.transcript_scrollbar_area.unwrap();
    assert!((area.y..area.bottom()).all(|row| {
        !terminal.backend().buffer()[(area.x, row)]
            .modifier
            .contains(Modifier::REVERSED)
    }));
}

#[test]
fn overlays_page_by_their_rendered_content_height() {
    let mut app = test_app();
    app.overlay = Some(Overlay::Document);
    app.document = Some((
        "LONG".to_string(),
        (0..100).map(|i| format!("line {i}\n")).collect(),
    ));
    render_to_string(&mut app, 100, 24);
    let page_rows = app.overlay_viewport_rows;
    assert!(page_rows > 8);

    app.page_overlay(1);
    assert_eq!(app.overlay_scroll, page_rows as u16);
    app.page_overlay(-1);
    assert_eq!(app.overlay_scroll, 0);
}

#[test]
fn scrolled_transcript_shows_a_mouse_button_that_returns_to_the_live_tail() {
    let mut app = test_app();
    for index in 0..30 {
        app.push(
            BlockKind::Assistant,
            "AGENT",
            format!("message {index}"),
            None,
            false,
            true,
        );
    }
    app.selected_block = 12;
    render_to_string(&mut app, 80, 12);
    app.scroll_transcript(-3);
    app.busy = true;
    app.activity = Some(Activity::Thinking);

    let rendered = render_to_string(&mut app, 80, 12);
    let rows = rendered.lines().collect::<Vec<_>>();
    let activity = rows[10];
    let footer = rows[11];
    assert_eq!(
        activity
            .chars()
            .position(|character| !character.is_whitespace()),
        Some(0)
    );
    assert!(activity.contains("thinking"));
    assert!(activity.contains(TAIL_BUTTON_LABEL));
    assert!(footer.starts_with("model · effort off"));
    assert!(footer.trim_end().ends_with("········ 0.0%/128k"));
    assert!(!footer.contains("↓ bottom"));
    let button = app
        .hit_regions
        .iter()
        .find(|region| region.target == AppHit::TranscriptTail)
        .copied()
        .expect("return-to-bottom mouse target");
    assert_eq!(button.area, Rect::new(68, 10, 10, 1));

    let narrow = render_to_string(&mut app, 12, 12);
    assert!(narrow.lines().nth(10).unwrap().contains("↓ bottom"));
    let narrow_button = app
        .hit_regions
        .iter()
        .find(|region| region.target == AppHit::TranscriptTail)
        .expect("narrow return-to-bottom mouse target");
    assert_eq!(narrow_button.area, Rect::new(0, 10, 10, 1));

    app.busy = false;
    app.activity = None;
    let rendered = render_to_string(&mut app, 80, 12);
    let rows = rendered.lines().collect::<Vec<_>>();
    assert_eq!(rows[10].chars().nth(76), Some('↓'));
    assert!(!rows[11].contains('↓'));
    let floating_button = app
        .hit_regions
        .iter()
        .find(|region| region.target == AppHit::TranscriptTail)
        .copied()
        .expect("floating return-to-bottom mouse target");
    assert_eq!(floating_button.area, Rect::new(75, 10, 3, 1));
    assert!(
        app.selectable.as_ref().unwrap().cells[10][75..78]
            .iter()
            .all(|cell| cell == " ")
    );

    let click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: floating_button.area.x,
        row: floating_button.area.y,
        modifiers: KeyModifiers::NONE,
    };
    assert!(activate_transcript_mouse(&mut app, click));
    assert_eq!(app.selected_block, 12);
    assert!(app.transcript_follow_tail);
    assert_eq!(
        app.transcript_offset,
        transcript_live_tail(app.transcript_rows, app.transcript_height)
    );

    let rendered = render_to_string(&mut app, 80, 12);
    assert!(rendered.contains("message 29"));
    assert!(!rendered.contains('↓'));
    assert!(
        app.hit_regions
            .iter()
            .all(|region| region.target != AppHit::TranscriptTail)
    );

    app.scroll_transcript(3);
    assert!(
        app.transcript_offset > transcript_live_tail(app.transcript_rows, app.transcript_height)
    );
    app.busy = true;
    app.activity = Some(Activity::Thinking);
    let rendered = render_to_string(&mut app, 80, 12);
    assert!(!rendered.contains("↓ bottom"));
    assert!(
        app.hit_regions
            .iter()
            .all(|region| region.target != AppHit::TranscriptTail)
    );

    app.busy = false;
    app.activity = None;
    let rendered = render_to_string(&mut app, 80, 12);
    assert!(!rendered.contains('↓'));
    assert!(
        app.hit_regions
            .iter()
            .all(|region| region.target != AppHit::TranscriptTail)
    );
}

#[test]
fn manual_scroll_can_lift_the_final_row_to_the_viewport_middle() {
    let mut app = test_app();
    for index in 0..30 {
        app.push(
            BlockKind::Assistant,
            "AGENT",
            format!("message {index}"),
            None,
            false,
            true,
        );
    }
    app.selected_block = 29;
    render_to_string(&mut app, 80, 12);
    let live_tail = app.transcript_offset;
    assert_eq!(
        live_tail,
        transcript_live_tail(app.transcript_rows, app.transcript_height)
    );

    app.scroll_transcript(3);
    assert_eq!(app.transcript_offset, live_tail + 3);
    assert!(!app.transcript_follow_tail);
    app.scroll_transcript(isize::MAX);
    render_to_string(&mut app, 80, 12);
    assert_eq!(
        app.transcript_offset,
        transcript_reading_end(app.transcript_rows, app.transcript_height)
    );
    let final_row = app
        .hit_regions
        .iter()
        .find_map(|region| (region.target == AppHit::Transcript(29)).then_some(region.area.y))
        .unwrap();
    assert_eq!(final_row, app.transcript_height as u16 / 2);

    app.push(
        BlockKind::Assistant,
        "AGENT",
        "message 30".into(),
        None,
        false,
        true,
    );
    render_to_string(&mut app, 80, 12);
    assert!(!app.transcript_follow_tail);
    assert_eq!(app.transcript_offset, live_tail + app.transcript_height / 2);

    app.transcript_follow_tail = true;
    render_to_string(&mut app, 80, 12);
    let final_row = app
        .hit_regions
        .iter()
        .find_map(|region| (region.target == AppHit::Transcript(30)).then_some(region.area.y))
        .unwrap();
    assert_eq!(final_row, app.transcript_height as u16 - 1);
}

#[test]
fn keyboard_navigation_centers_an_offscreen_transcript_block() {
    let mut app = test_app();
    for index in 0..30 {
        app.push(
            BlockKind::Assistant,
            "AGENT",
            format!("message {index}"),
            None,
            false,
            true,
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
        region.target == AppHit::Transcript(18) && region.area.y == app.transcript_height as u16 / 2
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
    assert!(rendered.contains("COMMAND · Tab complete"));
    assert!(!rendered.contains("type to filter"));
    assert!(!rendered.contains("Enter run"));
    assert!(!rendered.contains("Esc close"));
    assert!(rendered.contains("⌕ effort█"));
    assert!(rendered.contains(":effort"));
    assert!(!rendered.contains(":terminal"));
}

#[test]
fn command_and_selector_panels_page_by_their_visible_rows() {
    let mut app = test_app();
    app.overlay = Some(Overlay::Command);
    render_to_string(&mut app, 80, 24);
    let command_count = app.matching_commands().len();
    let command_page = app.overlay_viewport_rows;
    assert!(command_count > command_page);
    assert!(matches!(
        apply_command_key(
            &mut app,
            KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
            "pagedown"
        ),
        CommandKey::Continue
    ));
    assert_eq!(
        app.command_selected,
        bounded_index(0, command_page as isize, command_count)
    );

    let items = (0..30)
        .map(|index| SelectorItem {
            id: index.to_string(),
            title: format!("Item {index}"),
            description: String::new(),
            search_text: None,
        })
        .collect();
    app.selector = Some(SelectorState::new(SelectorKind::Logout, "SELECT", items));
    app.overlay = Some(Overlay::Selector);
    render_to_string(&mut app, 80, 24);
    let selector_page = app.overlay_viewport_rows;
    assert!(matches!(
        apply_selector_key(
            &mut app,
            KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
            "pagedown"
        ),
        SelectorKey::Continue
    ));
    assert_eq!(app.selector.as_ref().unwrap().selected, selector_page);
}

#[test]
fn selected_command_description_scrolls_inside_its_single_row() {
    let mut app = test_app();
    app.overlay = Some(Overlay::Command);
    app.command_query = "refresh-catalog".to_string();

    let initial = render_to_string(&mut app, 80, 24);
    let initial_row = initial
        .lines()
        .find(|line| line.contains(":refresh-catalog"))
        .unwrap()
        .to_string();
    assert!(initial_row.contains('…'));

    app.frame = MARQUEE_HOLD_FRAMES + 4 * MARQUEE_STEP_FRAMES;
    let advanced = render_to_string(&mut app, 80, 24);
    let advanced_row = advanced
        .lines()
        .find(|line| line.contains(":refresh-catalog"))
        .unwrap();
    assert_ne!(advanced_row, initial_row);
    assert!(advanced_row.matches('…').count() >= 1);
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
    assert_eq!(app.command_query, "set-env");
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
    assert!(rendered.contains("COMMAND · Tab complete"));
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
        compaction: crate::compaction::Settings::default(),
        provider_source: ValueSource::Global,
        model_source: ValueSource::Global,
        api_key_source: ValueSource::Default,
        output_limit_source: ValueSource::Global,
        thinking_source: ValueSource::Global,
        terminal: None,
        key_display: KeyDisplayStyle::Text,
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
    assert!(rendered.contains("SEARCH"));
    assert!(!rendered.contains("type to filter"));
    assert!(!rendered.contains("Enter jump"));
    assert!(!rendered.contains("Esc close"));
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
    assert_eq!(
        app.visible_flashes().next_back(),
        Some("No conversation text to search")
    );

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
        commands.resolve(":set-env").unwrap().spec.target,
        CommandTarget::Core(CoreCommand::SetEnvironment)
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
        compaction: crate::compaction::Settings::default(),
        provider_source: ValueSource::Global,
        model_source: ValueSource::Global,
        api_key_source: ValueSource::Global,
        output_limit_source: ValueSource::Global,
        thinking_source: ValueSource::Global,
        terminal: None,
        key_display: KeyDisplayStyle::Text,
        terminal_source: ValueSource::Global,
        credential_environment: BTreeMap::new(),
    };
    let mut app = test_app();
    app.overlay = Some(Overlay::Settings);
    app.settings = Some(SettingsState {
        active,
        model: None,
        environment_count: 2,
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
    assert!(rendered.contains("Agent environment"));
    assert!(rendered.contains("2 variables"));
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

    app.settings.as_mut().unwrap().selected = 4;
    assert!(matches!(
        handle_settings_key(&mut app, key, &key_name(key)),
        Action::OpenEnvironment {
            return_to_settings: true
        }
    ));
}

#[test]
fn environment_prompts_hide_values_and_return_to_the_manager() {
    let mut app = test_app();
    open_environment_name_prompt(&mut app, false);
    app.text_prompt.as_mut().unwrap().value = "NPM_TOKEN".to_string();
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::empty());
    assert!(matches!(
        handle_text_key(&mut app, enter, &key_name(enter)),
        Action::Continue
    ));
    let prompt = app.text_prompt.as_mut().unwrap();
    assert!(prompt.secret);
    prompt.value = "super-secret-value".to_string();
    let rendered = render_to_string(&mut app, 100, 24);
    assert!(rendered.contains("SET NPM_TOKEN"));
    assert!(rendered.contains("••••"));
    assert!(!rendered.contains("super-secret-value"));

    open_environment_value_prompt(&mut app, "NPM_TOKEN".to_string(), true);
    let narrow = render_to_string(&mut app, 28, 16);
    for word in "Value is stored privately and injected into future Agent shell commands."
        .split_whitespace()
    {
        assert!(
            narrow.contains(word),
            "missing wrapped prompt word {word:?}"
        );
    }
    let escape = KeyEvent::new(KeyCode::Esc, KeyModifiers::empty());
    assert!(matches!(
        handle_text_key(&mut app, escape, &key_name(escape)),
        Action::OpenEnvironment {
            return_to_settings: true
        }
    ));
    assert!(app.text_prompt.is_none());
}

#[tokio::test]
async fn environment_manager_lists_names_without_values_and_updates_storage() {
    let directory = tempfile::tempdir().unwrap();
    let environment = AgentEnvironment::load(directory.path()).await.unwrap();
    let mut app = test_app();

    store_environment(
        &mut app,
        &environment,
        "NPM_TOKEN".to_string(),
        "super-secret-value".to_string(),
        false,
    )
    .await;
    assert!(app.overlay.is_none());
    assert_eq!(
        environment.get("NPM_TOKEN").await.unwrap().as_deref(),
        Some("super-secret-value")
    );

    open_environment(&mut app, &environment, true).await;
    let rendered = render_to_string(&mut app, 100, 24);
    assert!(rendered.contains("NPM_TOKEN"));
    assert!(!rendered.contains("super-secret-value"));

    delete_environment(&mut app, &environment, "NPM_TOKEN", true).await;
    assert_eq!(environment.get("NPM_TOKEN").await.unwrap(), None);
    assert!(
        app.selector
            .as_ref()
            .is_some_and(|selector| selector.items.is_empty())
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
            protocol_help_required: false,
        },
    });
    assert_eq!(app.blocks.len(), 1);
    assert_eq!(app.blocks[0].title, "Read src/main.rs");
    assert!(app.blocks[0].text.is_empty());
    let document = block_document(&app.blocks[0]);
    assert!(document.contains("**✓ Succeeded**"));
    assert!(document.contains("**Target:** `file://src/main.rs`"));
    assert!(document.contains("## Result"));
    assert!(document.contains("complete tool output"));
    assert!(!document.contains("Call ID:"));
    assert!(!document.contains("\nCALL\n"));
    assert!(!document.contains("\nRESULT\n"));

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
fn full_tool_documents_render_status_input_and_output_as_markdown() {
    let mut app = test_app();
    apply_event(
        &mut app,
        1,
        EventKind::ToolCall {
            call_id: "shell-call".to_string(),
            name: "exec".to_string(),
            arguments: serde_json::json!({
                "uri": "bash://run",
                "body": "printf done"
            }),
        },
    );
    apply_event(
        &mut app,
        2,
        EventKind::ToolResult {
            call_id: "shell-call".to_string(),
            name: "exec".to_string(),
            output: "done".to_string(),
            failed: false,
            protocol_help_required: false,
        },
    );

    app.open_selected_document();
    let body = &app.document.as_ref().unwrap().1;
    assert!(body.contains("## Command"));
    assert!(body.contains("```bash\nprintf done\n```"));
    assert!(body.contains("## Result"));
    assert!(!body.contains("shell-call"));

    app.skip_splash();
    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let cells = terminal.backend().buffer().content();
    let symbols = cells.iter().map(|cell| cell.symbol()).collect::<Vec<_>>();
    let needle = "printf done"
        .chars()
        .map(|character| character.to_string())
        .collect::<Vec<_>>();
    let commands = symbols
        .windows(needle.len())
        .enumerate()
        .filter_map(|(index, window)| {
            window
                .iter()
                .zip(&needle)
                .all(|(cell, char)| *cell == char)
                .then_some(index)
        })
        .collect::<Vec<_>>();
    assert!(!commands.is_empty(), "rendered command should be visible");
    assert!(commands.iter().any(|start| {
        cells[*start..*start + needle.len()]
            .iter()
            .all(|cell| cell.bg == SURFACE)
    }));
    let language = ["b", "a", "s", "h"];
    assert!(
        symbols
            .windows(language.len())
            .enumerate()
            .any(|(start, window)| {
                window == language
                    && cells[start..start + language.len()]
                        .iter()
                        .all(|cell| cell.fg == MUTED && cell.modifier.contains(Modifier::ITALIC))
            })
    );
}

#[test]
fn tool_details_redact_sensitive_dynamic_arguments() {
    let mut app = test_app();
    apply_event(
        &mut app,
        1,
        EventKind::ToolCall {
            call_id: "custom-call".to_string(),
            name: "custom".to_string(),
            arguments: serde_json::json!({
                "api_key": "argument-secret",
                "body": r#"{"authorization":"body-secret","query":"visible body"}"#,
                "environment_variables": {"DATABASE_URL": "env-secret"},
                "message": "visible argument",
                "settings": {"password": "nested-secret"}
            }),
        },
    );
    apply_event(
        &mut app,
        2,
        EventKind::ToolResult {
            call_id: "custom-call".to_string(),
            name: "custom".to_string(),
            output: "done".to_string(),
            failed: false,
            protocol_help_required: false,
        },
    );

    let document = block_document(&app.blocks[0]);
    let (details, _) = tool_detail_lines(&app.blocks[0], 120, 20);
    let details = details
        .into_iter()
        .map(|(line, _)| line)
        .collect::<Vec<_>>()
        .join("\n");
    let search = block_search_text(&app.blocks[0]);

    for rendered in [&document, &details, &search] {
        assert!(rendered.contains("[redacted]"));
        assert!(rendered.contains("visible argument"));
        assert!(!rendered.contains("argument-secret"));
        assert!(!rendered.contains("body-secret"));
        assert!(!rendered.contains("env-secret"));
        assert!(!rendered.contains("nested-secret"));
    }
    assert!(document.contains("visible body"));
}

#[test]
fn tool_documents_distinguish_running_failed_and_empty_results() {
    let mut app = test_app();
    apply_event(
        &mut app,
        1,
        EventKind::ToolCall {
            call_id: "failed-call".to_string(),
            name: "read".to_string(),
            arguments: serde_json::json!({"uri": "file://missing", "body": ""}),
        },
    );
    assert!(block_document(&app.blocks[0]).contains("**• Running**"));

    apply_event(
        &mut app,
        2,
        EventKind::ToolResult {
            call_id: "failed-call".to_string(),
            name: "read".to_string(),
            output: "Error: file not found".to_string(),
            failed: true,
            protocol_help_required: false,
        },
    );
    let failed = block_document(&app.blocks[0]);
    assert!(failed.contains("**× Failed**"));
    assert!(failed.contains("## Error"));
    assert!(failed.contains("```text\nfile not found\n```"));
    assert!(!failed.contains("Error: file not found"));

    apply_event(
        &mut app,
        3,
        EventKind::ToolCall {
            call_id: "empty-call".to_string(),
            name: "read".to_string(),
            arguments: serde_json::json!({"uri": "file://empty", "body": ""}),
        },
    );
    apply_event(
        &mut app,
        4,
        EventKind::ToolResult {
            call_id: "empty-call".to_string(),
            name: "read".to_string(),
            output: String::new(),
            failed: false,
            protocol_help_required: false,
        },
    );
    let empty = block_document(&app.blocks[1]);
    assert!(empty.contains("**✓ Succeeded**"));
    assert!(empty.contains("## Result\n\n_(no output)_"));
}

#[test]
fn tool_document_fences_do_not_conflict_with_dynamic_backticks() {
    assert_eq!(
        fenced_block("before\n```\nafter", "text"),
        "````text\nbefore\n```\nafter\n````\n"
    );
}

#[test]
fn protocol_help_gate_colors_only_the_tool_header_purple() {
    let mut app = test_app();
    apply_event(
        &mut app,
        1,
        EventKind::ToolCall {
            call_id: "blocked".to_string(),
            name: "read".to_string(),
            arguments: serde_json::json!({
                "uri": "file://src/main.rs",
                "body": ""
            }),
        },
    );
    apply_event(
        &mut app,
        2,
        EventKind::ToolResult {
            call_id: "blocked".to_string(),
            name: "read".to_string(),
            output: "Read \"file://help\" with an empty body before using this protocol."
                .to_string(),
            failed: true,
            protocol_help_required: true,
        },
    );
    app.blocks[0].expanded = true;
    app.skip_splash();
    let backend = TestBackend::new(100, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let cells = terminal.backend().buffer().content();
    let symbols = cells.iter().map(|cell| cell.symbol()).collect::<Vec<_>>();
    let find = |needle: &str| {
        let needle = needle.chars().map(|ch| ch.to_string()).collect::<Vec<_>>();
        symbols
            .windows(needle.len())
            .position(|window| window.iter().zip(&needle).all(|(cell, ch)| *cell == ch))
            .expect("expected text should be rendered")
    };

    let header = "× Read src/main.rs";
    let header_start = find(header);
    for cell in &cells[header_start..header_start + header.chars().count()] {
        assert_eq!(cell.fg, PURPLE);
    }
    let detail = "Read \"file://help\" with an empty body before using this protocol.";
    let detail_start = find(detail);
    for cell in &cells[detail_start..detail_start + detail.chars().count()] {
        assert_eq!(cell.fg, ERROR);
    }
}

#[test]
fn tool_summaries_describe_shell_patch_and_unknown_arguments_without_json() {
    assert_eq!(
        tool_title(
            "exec",
            &serde_json::json!({
                "uri": "bash://run",
                "body": "cargo test\necho done"
            })
        ),
        "$ cargo test"
    );
    assert_eq!(
        tool_title(
            "exec",
            &serde_json::json!({"uri": "bash://run", "body": "test command"})
        ),
        "$ test command"
    );
    assert_eq!(
        tool_title(
            "apply_patch",
            &serde_json::json!({
                "patch": "*** Begin Patch\n*** Update File: src/tui.rs\n*** Update File: Cargo.toml\n*** End Patch"
            })
        ),
        "Patched src/tui.rs +1"
    );

    let mut lines = Vec::new();
    tool_argument_details(
        &serde_json::json!({
            "body": "{\"path\":\"src/main.rs\",\"limit\":20}"
        }),
        &mut lines,
    );
    let text = lines
        .into_iter()
        .map(|(line, _)| line)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("path: src/main.rs"));
    assert!(text.contains("limit: 20"));
    assert!(!text.contains("kind:"));
    assert!(!text.contains("value:"));
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
    let rendered = render_to_string(&mut app, 100, 24);
    assert!(rendered.lines().nth(22).unwrap().contains("running file"));
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
            protocol_help_required: false,
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
        key_name(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)),
        "space"
    );
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
        key_name(KeyEvent::new(KeyCode::Char('@'), KeyModifiers::SHIFT)),
        "@"
    );
    assert_eq!(
        key_name(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT)),
        "alt+backspace"
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
fn macos_key_display_formats_composer_panel_and_help_hints() {
    let mut app = test_app();
    app.keymap = Keymap::with_display_style(KeyDisplayStyle::Macos).unwrap();
    app.overlay = Some(Overlay::Composer);
    app.sync_composer_chrome();
    let rendered = render_to_string(&mut app, 100, 24);
    assert!(rendered.contains("↩ send · ⇧ ↩ newline · ⌥ V image"));

    app.overlay = Some(Overlay::Models);
    let rendered = render_to_string(&mut app, 100, 24);
    assert!(rendered.contains("MODELS · ⌃ R refresh"));

    let help = keymap_help(&app.keymap);
    assert!(help.contains("⌘ C"));
    assert!(help.contains("⌘ V"));
    assert!(help.contains("⌘ Z"));
}

#[test]
fn double_escape_gesture_requires_two_press_events_during_a_running_turn() {
    let mut app = test_app();
    let escape = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    let repeat = KeyEvent::new_with_kind(KeyCode::Esc, KeyModifiers::NONE, KeyEventKind::Repeat);

    assert!(keymap_help(&app.keymap).contains("interrupt on double press"));
    assert!(!app.interrupt_on_double_press(escape, "esc"));
    app.busy = true;
    app.set_flash("Existing notification");
    assert!(!app.interrupt_on_double_press(escape, "esc"));
    let rendered = render_to_string(&mut app, 80, 24);
    assert!(rendered.contains("press Esc again to interrupt"));
    assert!(
        rendered.find("Existing notification").unwrap()
            < rendered.find("press Esc again to interrupt").unwrap()
    );
    assert!(!app.interrupt_on_double_press(repeat, "esc"));
    assert!(app.interrupt_on_double_press(escape, "esc"));
    assert!(!render_to_string(&mut app, 80, 24).contains("press Esc again to interrupt"));
    assert!(!app.interrupt_on_double_press(escape, "esc"));

    let unrelated = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
    assert!(!app.interrupt_on_double_press(unrelated, "x"));
    assert!(!app.interrupt_on_double_press(escape, "esc"));
    app.last_interrupt_press = Some(("esc".to_string(), Instant::now() - DOUBLE_CLICK_INTERVAL));
    assert!(!app.interrupt_on_double_press(escape, "esc"));
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
        row_separators: vec![TextRowSeparator::Newline; 2],
        left_padding: 0,
    };
    let selection = TextSelection {
        start: (11, 5),
        end: (12, 6),
    };
    assert_eq!(selected_surface_text(&surface, selection), "bc\ndef");
    assert_eq!(complete_surface_text(&surface), "abc\ndef");
}

#[test]
fn cell_selection_omits_soft_wraps_but_preserves_source_separators() {
    let surface = SelectableSurface {
        area: Rect::new(0, 0, 5, 4),
        cells: ["abc", "def", "ghi", "j"]
            .into_iter()
            .map(|line| {
                line.chars()
                    .map(|character| character.to_string())
                    .chain(std::iter::repeat_n(" ".to_string(), 5 - line.len()))
                    .collect()
            })
            .collect(),
        row_separators: vec![
            TextRowSeparator::None,
            TextRowSeparator::Space,
            TextRowSeparator::Newline,
            TextRowSeparator::Newline,
        ],
        left_padding: 0,
    };
    let selection = TextSelection {
        start: (0, 0),
        end: (0, 3),
    };

    assert_eq!(selected_surface_text(&surface, selection), "abcdef ghi\nj");
    assert_eq!(complete_surface_text(&surface), "abcdef ghi\nj");
}

#[test]
fn captured_wide_characters_do_not_include_their_hidden_cells() {
    let mut app = test_app();
    let area = Rect::new(0, 0, 12, 1);
    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            frame.render_widget(Paragraph::new("复制内容"), area);
            capture_surface(frame, &mut app, area, None, 0);
        })
        .unwrap();

    let surface = app.selectable.as_ref().unwrap();
    assert_eq!(
        selected_surface_text(
            surface,
            TextSelection {
                start: (0, 0),
                end: (7, 0),
            },
        ),
        "复制内容"
    );
    assert_eq!(complete_surface_text(surface), "复制内容");
}

#[test]
fn web_search_providers_use_api_key_login_prompts() {
    let parallel = login_provider_item("parallel", "openai");
    assert_eq!(parallel.description, "Web search · API key");

    let mut app = test_app();
    open_api_key_prompt(&mut app, "parallel".to_string());
    let prompt = app.text_prompt.as_ref().unwrap();
    assert!(prompt.secret);
    assert!(prompt.message.contains("platform.parallel.ai"));

    open_api_key_prompt(&mut app, "exa".to_string());
    assert!(
        app.text_prompt
            .as_ref()
            .unwrap()
            .message
            .contains("dashboard.exa.ai")
    );
}
