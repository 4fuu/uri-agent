use super::*;
use std::borrow::Cow;

pub(super) fn block_document(block: &DisplayBlock) -> String {
    block_document_with_level(block, 1)
}

pub(super) fn block_document_with_level(block: &DisplayBlock, level: usize) -> String {
    if let Some(tool) = &block.tool {
        return tool_document(block, tool, level);
    }
    let mut document = format!("{} {}\n", "#".repeat(level), block.title);
    document.push('\n');
    document.push_str(&block.text);
    if !document.ends_with('\n') {
        document.push('\n');
    }
    document
}

fn tool_document(block: &DisplayBlock, tool: &ToolDisplay, level: usize) -> String {
    let section_level = level.saturating_add(1);
    let mut document = format!("{} {}\n\n", "#".repeat(level), block.title);
    document.push_str(match (&tool.output, block.failed) {
        (None, _) => "**• Running**\n",
        (Some(_), true) => "**× Failed**\n",
        (Some(_), false) => "**✓ Succeeded**\n",
    });

    if let Some(target) = tool_target(tool) {
        document.push_str(&format!("\n**Target:** {}\n", inline_code(target)));
    }
    append_tool_input(&mut document, tool, section_level);

    if let Some(output) = &tool.output {
        let heading = if block.failed { "Error" } else { "Result" };
        document.push_str(&format!("\n{} {heading}\n\n", "#".repeat(section_level)));
        let output = if block.failed {
            output.strip_prefix("Error: ").unwrap_or(output)
        } else {
            output
        };
        if output.is_empty() {
            document.push_str("_(no output)_\n");
        } else {
            document.push_str(&fenced_block(output, "text"));
        }
    }
    document
}

fn tool_target(tool: &ToolDisplay) -> Option<&str> {
    tool.arguments
        .get("uri")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            (tool.name == "replace")
                .then(|| tool.arguments.get("path")?.as_str())
                .flatten()
        })
}

fn append_tool_input(document: &mut String, tool: &ToolDisplay, level: usize) {
    let heading = "#".repeat(level);
    if tool.name == "apply_patch"
        && let Some(patch) = tool
            .arguments
            .get("patch")
            .and_then(serde_json::Value::as_str)
    {
        document.push_str(&format!("\n{heading} Patch\n\n"));
        document.push_str(&fenced_block(patch, "diff"));
        return;
    }
    if tool.name == "replace" {
        for (key, label) in [("old_text", "Before"), ("new_text", "After")] {
            if let Some(value) = tool.arguments.get(key).and_then(serde_json::Value::as_str) {
                document.push_str(&format!("\n{heading} {label}\n\n"));
                document.push_str(&fenced_block(value, "text"));
            }
        }
        return;
    }
    if let Some(body) = tool_body_text(&tool.arguments)
        && !body.is_empty()
    {
        let protocol = tool_protocol(&tool.arguments);
        let (label, language) = if tool.name == "exec" {
            match protocol.as_deref() {
                Some("bash") => ("Command", "bash"),
                Some("pwsh") => ("Command", "powershell"),
                _ => ("Input", "text"),
            }
        } else {
            ("Input", "text")
        };
        let parsed = (label != "Command")
            .then(|| serde_json::from_str::<serde_json::Value>(body).ok())
            .flatten();
        let rendered = parsed.as_ref().and_then(|value| {
            serde_json::to_string_pretty(&redact_sensitive_arguments(value)).ok()
        });
        document.push_str(&format!("\n{heading} {label}\n\n"));
        document.push_str(&fenced_block(
            rendered.as_deref().unwrap_or(body),
            if rendered.is_some() { "json" } else { language },
        ));
    }

    let Some(arguments) = tool.arguments.as_object() else {
        return;
    };
    let remaining = arguments
        .iter()
        .filter(|(name, _)| !matches!(name.as_str(), "uri" | "body"))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect::<serde_json::Map<_, _>>();
    if remaining.is_empty() {
        return;
    }
    let remaining = redact_sensitive_arguments(&serde_json::Value::Object(remaining));
    let input = serde_json::to_string_pretty(&remaining).unwrap_or_else(|_| remaining.to_string());
    document.push_str(&format!("\n{heading} Input\n\n"));
    document.push_str(&fenced_block(&input, "json"));
}

pub(super) fn redact_sensitive_arguments(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(fields) => serde_json::Value::Object(
            fields
                .iter()
                .map(|(name, value)| {
                    let value = if sensitive_argument_name(name) {
                        serde_json::Value::String("[redacted]".to_string())
                    } else {
                        redact_sensitive_arguments(value)
                    };
                    (name.clone(), value)
                })
                .collect(),
        ),
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.iter().map(redact_sensitive_arguments).collect())
        }
        value => value.clone(),
    }
}

fn sensitive_argument_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase().replace('-', "_");
    matches!(
        name.as_str(),
        "api_key"
            | "apikey"
            | "access_token"
            | "accesstoken"
            | "refresh_token"
            | "refreshtoken"
            | "auth_token"
            | "authtoken"
            | "authorization"
            | "password"
            | "passwords"
            | "passphrase"
            | "secret"
            | "secrets"
            | "client_secret"
            | "clientsecret"
            | "credential"
            | "credentials"
            | "cookie"
            | "cookies"
            | "private_key"
            | "privatekey"
            | "environment"
            | "environment_variables"
            | "environmentvariables"
            | "env"
            | "env_vars"
            | "envvars"
            | "token"
    ) || ["_api_key", "_token", "_password", "_secret", "_credential"]
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

fn inline_code(value: &str) -> String {
    let fence = "`".repeat(longest_backtick_run(value).saturating_add(1).max(1));
    let padding = value.starts_with(['`', ' ']) || value.ends_with(['`', ' ']);
    if padding {
        format!("{fence} {value} {fence}")
    } else {
        format!("{fence}{value}{fence}")
    }
}

pub(super) fn fenced_block(value: &str, language: &str) -> String {
    let fence = "`".repeat(longest_backtick_run(value).saturating_add(1).max(3));
    format!(
        "{fence}{language}\n{value}{}{fence}\n",
        if value.ends_with('\n') { "" } else { "\n" }
    )
}

fn longest_backtick_run(value: &str) -> usize {
    value
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or_default()
}

pub(super) fn tool_protocol(arguments: &serde_json::Value) -> Option<String> {
    let uri = arguments.get("uri")?.as_str()?;
    let separator = uri.find("://").or_else(|| uri.find(':'))?;
    (separator > 0).then(|| uri[..separator].to_string())
}

fn tool_body_text(arguments: &serde_json::Value) -> Option<&str> {
    arguments.get("body")?.as_str()
}

fn tool_body(arguments: &serde_json::Value) -> Option<Cow<'_, serde_json::Value>> {
    let body = arguments.get("body")?;
    let Some(value) = body.as_str() else {
        return Some(Cow::Borrowed(body));
    };
    if value.is_empty() {
        return None;
    }
    serde_json::from_str(value)
        .ok()
        .map(Cow::Owned)
        .or(Some(Cow::Borrowed(body)))
}

pub(super) fn tool_title(name: &str, arguments: &serde_json::Value) -> String {
    if name == "apply_patch" {
        let files = arguments
            .get("patch")
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
    if name == "replace" {
        let path = arguments
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        return format!("Edited {}", single_line_preview(path, 72));
    }
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
        && let Some(command) = tool_body_text(arguments)
    {
        return format!(
            "$ {}",
            single_line_preview(command.lines().next().unwrap_or_default(), 76)
        );
    }
    if name == "read" && protocol == "file" {
        return format!("Read {}", single_line_preview(target, 76));
    }
    if target == "help" {
        return format!("Read {protocol} help");
    }
    format!("{action} {}", single_line_preview(uri, 76))
}

pub(super) fn patch_targets(patch: &str) -> Vec<String> {
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

pub(super) fn tool_detail_lines(
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

pub(super) fn tool_argument_details(
    arguments: &serde_json::Value,
    lines: &mut Vec<(String, Color)>,
) {
    if let Some(fields) = arguments.as_object() {
        for (key, value) in fields {
            if matches!(key.as_str(), "uri" | "body") {
                continue;
            }
            if key == "patch"
                && let Some(patch) = value.as_str()
            {
                lines.extend(
                    patch_targets(patch)
                        .into_iter()
                        .map(|file| (format!("  {file}"), MUTED)),
                );
                continue;
            }
            lines.push((format!("  {key}: {}", argument_summary(key, value)), MUTED));
        }
    }
    let Some(body) = tool_body(arguments) else {
        return;
    };
    match body.as_ref() {
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
                lines.push((format!("  {key}: {}", argument_summary(key, value)), MUTED));
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

fn argument_summary(name: &str, value: &serde_json::Value) -> String {
    if sensitive_argument_name(name) {
        "[redacted]".to_string()
    } else {
        json_value_summary(value)
    }
}

pub(super) fn json_value_summary(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(value) => single_line_preview(value, 72),
        serde_json::Value::Number(value) => value.to_string(),
        serde_json::Value::Bool(value) => value.to_string(),
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Array(values) => format!("{} items", values.len()),
        serde_json::Value::Object(values) => format!("{} fields", values.len()),
    }
}

pub(super) fn render(frame: &mut Frame<'_>, app: &mut App) {
    app.hit_regions.clear();
    app.overlay_bounds = None;
    app.overlay_viewport_rows = 0;
    app.transcript_scrollbar_area = None;
    app.selectable = None;
    app.composer_view = None;
    if app.overlay != Some(Overlay::Composer) {
        if app.composer_mouse_selecting {
            app.mouse_word_selecting = false;
        }
        app.composer_mouse_selecting = false;
    }
    let marquee_visible = matches!(
        app.overlay,
        Some(
            Overlay::Command
                | Overlay::Tasks
                | Overlay::Models
                | Overlay::Settings
                | Overlay::Selector
        )
    ) || (app.overlay == Some(Overlay::Composer)
        && app.completions.is_some());
    if !marquee_visible {
        app.marquee = None;
    }
    let area = frame.area();
    frame.render_widget(Block::new().style(Style::default().bg(BG)), area);
    if app.showing_splash() {
        render_brand(frame, app, area, true);
        return;
    }
    app.prune_flashes();
    let idle = app.blocks.is_empty();
    let notices = fixed_bottom_notices(app);
    let notice_lines = bottom_notice_lines(&notices, area.width);
    let has_notices = !notice_lines.is_empty();
    let notice_height = notice_lines.len().min(u16::MAX as usize) as u16;
    let live_activity = footer_activity(app);
    let footer_height = 1 + u16::from(live_activity.is_some());
    let constraints = match (idle, has_notices) {
        (true, false) => vec![Constraint::Min(3)],
        (true, true) => vec![Constraint::Min(3), Constraint::Length(notice_height)],
        (false, false) => vec![Constraint::Min(3), Constraint::Length(footer_height)],
        (false, true) => vec![
            Constraint::Min(3),
            Constraint::Length(notice_height),
            Constraint::Length(footer_height),
        ],
    };
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);
    let footer_area = if idle {
        None
    } else if has_notices {
        Some(areas[2])
    } else {
        Some(areas[1])
    };
    let notice_area = has_notices.then(|| areas[1]);
    let (content, transcript_row_separators) = if idle {
        render_brand(frame, app, areas[0], false);
        (areas[0], None)
    } else {
        let row_separators = render_transcript(frame, app, areas[0]);
        render_footer(
            frame,
            app,
            footer_area.expect("conversation footer area"),
            live_activity.as_deref(),
        );
        (areas[0], Some(row_separators))
    };
    if let Some(notice_area) = notice_area {
        frame.render_widget(
            Paragraph::new(notice_lines)
                .style(Style::default().bg(SURFACE))
                .block(Block::new().padding(Padding::horizontal(1))),
            notice_area,
        );
    }
    let flash_lines = bottom_notice_lines(&transient_notices(app), area.width);
    if !flash_lines.is_empty() {
        let bottom = notice_area
            .or(footer_area)
            .map_or(area.bottom(), |bottom_area| bottom_area.y);
        let height = flash_lines
            .len()
            .min(bottom.saturating_sub(area.y) as usize) as u16;
        let flash_area = Rect::new(area.x, bottom.saturating_sub(height), area.width, height);
        frame.render_widget(
            Paragraph::new(flash_lines)
                .style(Style::default().bg(SURFACE))
                .block(Block::new().padding(Padding::horizontal(1))),
            flash_area,
        );
    }
    app.transcript_scrollbar_area = if app.overlay.is_none() && !idle {
        transcript_scrollbar_area(app, content)
    } else {
        None
    };
    let selectable_area = if let Some(overlay) = app.overlay {
        app.hit_regions.clear();
        let area = overlay_area(frame.area(), app, overlay);
        app.overlay_bounds = Some(area);
        render_overlay(frame, app, overlay);
        Some(area.inner(Margin {
            horizontal: 2,
            vertical: 2,
        }))
    } else {
        Some(Rect {
            width: content
                .width
                .saturating_sub(u16::from(app.transcript_scrollbar_area.is_some())),
            ..content
        })
    };
    if let Some(selectable_area) = selectable_area.filter(|area| !area.is_empty()) {
        let row_separators = app
            .overlay
            .is_none()
            .then_some(transcript_row_separators)
            .flatten();
        let left_padding = usize::from(row_separators.is_some());
        capture_surface(frame, app, selectable_area, row_separators, left_padding);
        render_selection(frame, app);
    }
    if app.overlay.is_none() && !idle {
        render_transcript_scrollbar(frame, app);
    }
    if app.overlay.is_none()
        && let Some(footer_area) = footer_area.filter(|area| area.height == 1)
    {
        render_floating_tail_button(frame, app, footer_area);
    }
}

const WORDMARK_BOX_HEIGHT: u16 = 13;
const WORDMARK_BOX_WIDTH: u16 = 76;

pub(super) fn wordmark_box(area: Rect) -> Rect {
    let width = area.width.clamp(1, WORDMARK_BOX_WIDTH);
    let height = area.height.clamp(1, WORDMARK_BOX_HEIGHT);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

pub(super) fn render_brand(frame: &mut Frame<'_>, app: &mut App, area: Rect, splash: bool) {
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

pub(super) fn welcome_lines(app: &App, width: usize) -> Vec<Line<'static>> {
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
        Line::styled("No model configured. Run :login", Style::default().fg(WARM))
    };
    let hints = action_hints(
        &app.keymap,
        &[
            ("main", "compose", "compose"),
            ("main", "command", "commands"),
            ("main", "help", "help"),
        ],
    );
    vec![
        Line::styled(
            single_line_preview(&footer_cwd(&app.info.cwd), width.saturating_sub(1)),
            Style::default().fg(MUTED),
        ),
        model,
        Line::default(),
        Line::styled(
            single_line_preview(&hints, width.saturating_sub(1)),
            Style::default().fg(MUTED),
        ),
    ]
}

/// Minimal conversation footer. Live activity follows the model while project,
/// usage, and extension details stay in the bottom-anchored status panel.
pub(super) fn render_footer(
    frame: &mut Frame<'_>,
    app: &mut App,
    area: Rect,
    live_activity: Option<&str>,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let percent = context_percent(app);
    let available = area.width as usize;
    let usage = match app.info.context_accuracy {
        ContextAccuracy::Unknown => "?".to_string(),
        _ if show_context_estimate(app) => format!("≈{percent:.1}%"),
        _ => format!("{percent:.1}%"),
    };
    let progress_frame = if app.busy { app.frame } else { 0 };
    let progress = if app.info.context_accuracy == ContextAccuracy::Unknown {
        animation::progress(progress_frame, 8, 0.0)
    } else {
        animation::progress(progress_frame, 8, percent / 100.0)
    };
    let context = single_line_preview(
        &format!(
            "{progress} {usage}/{}",
            format_tokens(app.info.context_window as u64),
        ),
        available,
    );
    let context_width = context.width();
    let model_limit = available.saturating_sub(context_width + 2);
    let model = single_line_preview(&compact_model(app), model_limit);
    let model_width = model.width();
    let gap = available.saturating_sub(model_width + context_width);
    let mut base = Vec::new();
    if !model.is_empty() {
        base.push(Span::styled(
            model,
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ));
    }
    if gap > 0 {
        base.push(Span::raw(" ".repeat(gap)));
    }
    base.push(Span::styled(
        context,
        Style::default()
            .fg(context_color(percent))
            .add_modifier(Modifier::BOLD),
    ));
    let mut lines = Vec::new();
    if area.height > 1
        && let Some(activity) = live_activity
    {
        let show_tail_button = transcript_before_live_tail(app) && available > 0;
        let tail_button = if TAIL_BUTTON_LABEL.width() <= available {
            TAIL_BUTTON_LABEL
        } else {
            "↓"
        };
        let tail_button_width = usize::from(show_tail_button) * tail_button.width();
        let tail_button_inset = usize::from(show_tail_button)
            * TAIL_BUTTON_RIGHT_INSET.min(available.saturating_sub(tail_button_width));
        let tail_controls_width = tail_button_width + tail_button_inset;
        let activity_limit =
            available.saturating_sub(tail_controls_width + usize::from(show_tail_button) * 2);
        let activity = single_line_preview(activity, activity_limit);
        let activity_width = activity.width();
        let activity_gap = available.saturating_sub(activity_width + tail_controls_width);
        let mut activity_line = Vec::new();
        if !activity.is_empty() {
            activity_line.push(Span::styled(activity, Style::default().fg(ACCENT)));
        }
        if activity_gap > 0 {
            activity_line.push(Span::raw(" ".repeat(activity_gap)));
        }
        if show_tail_button {
            let button_x = area
                .right()
                .saturating_sub((tail_button_width + tail_button_inset) as u16);
            activity_line.push(Span::styled(
                tail_button,
                Style::default()
                    .fg(ACCENT)
                    .bg(ROW_ACTIVE)
                    .add_modifier(Modifier::BOLD),
            ));
            app.hit_regions.insert(
                0,
                HitRegion {
                    area: Rect::new(button_x, area.y, tail_button_width as u16, 1),
                    target: AppHit::TranscriptTail,
                },
            );
            if tail_button_inset > 0 {
                activity_line.push(Span::raw(" ".repeat(tail_button_inset)));
            }
        }
        lines.push(Line::from(activity_line));
    }
    lines.push(Line::from(base));
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(SURFACE)),
        area,
    );
    app.hit_regions.push(HitRegion {
        area,
        target: AppHit::Status,
    });
}

pub(super) fn render_floating_tail_button(frame: &mut Frame<'_>, app: &mut App, footer_area: Rect) {
    if !transcript_before_live_tail(app) || footer_area.width == 0 || footer_area.y == 0 {
        return;
    }
    let available = footer_area.width as usize;
    let label = if FLOATING_TAIL_BUTTON_LABEL.width() <= available {
        FLOATING_TAIL_BUTTON_LABEL
    } else {
        "↓"
    };
    let width = label.width();
    let inset = TAIL_BUTTON_RIGHT_INSET.min(available.saturating_sub(width));
    let area = Rect::new(
        footer_area.right().saturating_sub((width + inset) as u16),
        footer_area.y.saturating_sub(1),
        width as u16,
        1,
    );
    frame.render_widget(
        Paragraph::new(label).style(
            Style::default()
                .fg(ACCENT)
                .bg(ROW_ACTIVE)
                .add_modifier(Modifier::BOLD),
        ),
        area,
    );
    app.hit_regions.insert(
        0,
        HitRegion {
            area,
            target: AppHit::TranscriptTail,
        },
    );
}

pub(super) fn transcript_before_live_tail(app: &App) -> bool {
    app.transcript_offset < transcript_live_tail(app.transcript_rows, app.transcript_height)
}

pub(super) fn footer_activity(app: &mut App) -> Option<String> {
    if !app.busy {
        return None;
    }
    let activity = app
        .activity
        .as_ref()
        .map(Activity::label)
        .unwrap_or_else(|| "working".to_string());
    let elapsed = app
        .busy_since
        .map(|since| format!(" {:.1}s", since.elapsed().as_secs_f32()))
        .unwrap_or_default();
    let token_rate = app
        .token_rate
        .display_rate(Instant::now())
        .map(|rate| format!(" · {}", format_token_rate(rate)))
        .unwrap_or_default();
    Some(format!(
        "{} {activity}{elapsed}  {}{token_rate}",
        animation::spinner(app.frame),
        animation::activity(app.frame, 8)
    ))
}

pub(super) fn compact_model(app: &App) -> String {
    if !app.info.model_ready || app.info.model.is_empty() {
        return "no-model".to_string();
    }
    let model = if app.info.provider_count > 1 {
        format!("{}/{}", app.info.provider, app.info.model)
    } else {
        app.info.model.clone()
    };
    let token_rate = (!app.busy && app.activity.is_none())
        .then(|| app.token_rate.final_average())
        .flatten()
        .map(|rate| format!(" · {}", format_token_rate(rate)))
        .unwrap_or_default();
    format!("{model} · effort {}{token_rate}", app.info.thinking)
}

pub(super) fn context_percent(app: &App) -> f64 {
    if app.info.context_window > 0 {
        app.info.context_tokens as f64 / app.info.context_window as f64 * 100.0
    } else {
        0.0
    }
}

pub(super) fn show_context_estimate(app: &App) -> bool {
    !app.busy
        && matches!(
            app.info.context_accuracy,
            ContextAccuracy::Hybrid | ContextAccuracy::Estimated
        )
}

pub(super) fn context_status(app: &App, percent: f64) -> String {
    let compaction = if app.info.compaction_enabled {
        "automatic compaction"
    } else {
        "automatic compaction disabled"
    };
    match app.info.context_accuracy {
        ContextAccuracy::Hybrid | ContextAccuracy::Estimated if show_context_estimate(app) => {
            format!(
                "≈{} / {} · ≈{percent:.1}% · {compaction}",
                format_tokens(app.info.context_tokens as u64),
                format_tokens(app.info.context_window as u64),
            )
        }
        ContextAccuracy::Unknown => format!(
            "unknown / {} · {compaction}",
            format_tokens(app.info.context_window as u64),
        ),
        ContextAccuracy::Api | ContextAccuracy::Hybrid | ContextAccuracy::Estimated => format!(
            "{} / {} · {percent:.1}% · {compaction}",
            format_tokens(app.info.context_tokens as u64),
            format_tokens(app.info.context_window as u64),
        ),
    }
}

pub(super) fn context_color(percent: f64) -> Color {
    if percent > 90.0 {
        ERROR
    } else if percent > 70.0 {
        WARM
    } else {
        ACCENT
    }
}

pub(super) fn plugin_status_items(app: &App, expanded: bool) -> Vec<TuiStatusItem> {
    app.tui.status_items(&TuiStatusContext {
        cwd: app.info.cwd.clone(),
        session_id: app.info.session_id.clone(),
        expanded,
    })
}

pub(super) fn status_tone_style(tone: TuiStatusTone) -> Style {
    Style::default().fg(match tone {
        TuiStatusTone::Default => TEXT,
        TuiStatusTone::Accent => ACCENT,
        TuiStatusTone::Warning => WARM,
        TuiStatusTone::Error => ERROR,
    })
}

pub(super) fn render_transcript(
    frame: &mut Frame<'_>,
    app: &mut App,
    area: Rect,
) -> Vec<TextRowSeparator> {
    if app.blocks.is_empty() {
        let unconfigured = app.info.provider.trim().is_empty() || app.info.model.trim().is_empty();
        let lines = if unconfigured {
            vec![
                Line::styled(
                    "No model configured. Run :login",
                    Style::default().fg(WARM).add_modifier(Modifier::BOLD),
                ),
                Line::styled(
                    "space compose   : command   :model",
                    Style::default().fg(MUTED),
                ),
            ]
        } else {
            vec![
                Line::styled("No messages yet", Style::default().fg(MUTED)),
                Line::styled(
                    "space compose   : command   :login",
                    Style::default().fg(WARM),
                ),
            ]
        };
        frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), area);
        return vec![TextRowSeparator::Newline; area.height as usize];
    }
    let message_width = area.width.saturating_sub(2).max(1) as usize;
    let process_width = message_width.saturating_sub(2).max(1);
    app.transcript_body_width = process_width;
    let mut items = Vec::new();
    let mut row_separators = Vec::new();
    let mut block_for_row = Vec::new();
    let mut user_surface_for_row = Vec::new();
    let mut block_rows = vec![None; app.blocks.len()];
    let active_block = app.active_transcript_block();
    let collapsed_processes = app.collapsed_processes();
    let mut previous_visible = None;
    for (index, block) in app.blocks.iter().enumerate() {
        if block
            .parent_process
            .is_some_and(|process| collapsed_processes.contains(&process))
        {
            continue;
        }
        if previous_visible.is_some_and(|(previous, previous_turn_result)| {
            transcript_needs_gap(
                previous,
                previous_turn_result,
                block.kind,
                block.turn_result,
            )
        }) {
            items.push(ListItem::new(Line::default()).style(Style::default().bg(BG)));
            row_separators.push(TextRowSeparator::Newline);
            block_for_row.push(None);
            user_surface_for_row.push(false);
        }
        if block.kind == BlockKind::User {
            items.push(ListItem::new(Line::default()).style(Style::default().bg(USER_SURFACE)));
            row_separators.push(TextRowSeparator::Newline);
            block_for_row.push(None);
            user_surface_for_row.push(true);
        }
        let first = items.len();
        for (item, separator) in transcript_block_items(
            block,
            index == app.selected_block,
            Some(index) == active_block,
            message_width,
            process_width,
            app,
        ) {
            items.push(item);
            row_separators.push(separator);
            block_for_row.push(Some(index));
            user_surface_for_row.push(block.kind == BlockKind::User);
        }
        block_rows[index] = Some((first, items.len().saturating_sub(1)));
        if block.kind == BlockKind::User {
            items.push(ListItem::new(Line::default()).style(Style::default().bg(USER_SURFACE)));
            row_separators.push(TextRowSeparator::Newline);
            block_for_row.push(None);
            user_surface_for_row.push(true);
        }
        previous_visible = Some((block.kind, block.turn_result));
    }
    app.transcript_rows = items.len();
    app.transcript_height = area.height as usize;
    let live_tail = transcript_live_tail(app.transcript_rows, app.transcript_height);
    let reading_end = transcript_reading_end(app.transcript_rows, app.transcript_height);
    if app.transcript_follow_tail {
        app.transcript_offset = live_tail;
    } else if app.transcript_center_selected
        && let Some((first, _)) = block_rows.get(app.selected_block).copied().flatten()
        && (first < app.transcript_offset
            || first >= app.transcript_offset.saturating_add(app.transcript_height))
    {
        app.transcript_offset = first
            .saturating_sub(app.transcript_height / 2)
            .min(reading_end);
    } else {
        app.transcript_offset = app.transcript_offset.min(reading_end);
    }
    app.transcript_center_selected = false;
    let offset = app.transcript_offset;
    let visible = items
        .into_iter()
        .skip(offset)
        .take(app.transcript_height)
        .collect::<Vec<_>>();
    let mut visible_row_separators = row_separators
        .into_iter()
        .skip(offset)
        .take(app.transcript_height)
        .collect::<Vec<_>>();
    visible_row_separators.resize(app.transcript_height, TextRowSeparator::Newline);
    frame.render_widget(
        List::new(visible).block(Block::new().padding(Padding::horizontal(1))),
        area,
    );
    // Ratatui resets the hidden cells behind wide glyphs even though the terminal paints their
    // background. Restore the complete user row so later frame diffs can clear exposed tail cells.
    let content_area = area.inner(Margin {
        horizontal: 1,
        vertical: 0,
    });
    for (row, user_surface) in user_surface_for_row
        .into_iter()
        .skip(offset)
        .take(app.transcript_height)
        .enumerate()
    {
        if user_surface {
            frame.buffer_mut().set_style(
                Rect::new(content_area.x, area.y + row as u16, content_area.width, 1),
                Style::default().bg(USER_SURFACE),
            );
        }
    }
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
    visible_row_separators
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct TranscriptScrollbarMetrics {
    pub(super) reading_end: usize,
    pub(super) thumb_start: usize,
    pub(super) thumb_length: usize,
    pub(super) max_thumb_start: usize,
}

fn transcript_scrollbar_area(app: &App, area: Rect) -> Option<Rect> {
    (transcript_reading_end(app.transcript_rows, app.transcript_height) > 0 && !area.is_empty())
        .then(|| Rect::new(area.right().saturating_sub(1), area.y, 1, area.height))
}

pub(super) fn transcript_scrollbar_metrics(app: &App) -> Option<TranscriptScrollbarMetrics> {
    let area = app.transcript_scrollbar_area?;
    let track_length = area.height as usize;
    let reading_end = transcript_reading_end(app.transcript_rows, app.transcript_height);
    let content_span = reading_end.saturating_add(app.transcript_height);
    if track_length == 0 || content_span == 0 {
        return None;
    }
    let rounding_divide = |numerator: usize, denominator: usize| {
        numerator.saturating_add(denominator / 2) / denominator
    };
    let thumb_length = rounding_divide(
        app.transcript_height.saturating_mul(track_length),
        content_span,
    )
    .clamp(1, track_length);
    let max_thumb_start = track_length.saturating_sub(thumb_length);
    let thumb_start = rounding_divide(
        app.transcript_offset
            .min(reading_end)
            .saturating_mul(track_length),
        content_span,
    )
    .min(max_thumb_start);
    Some(TranscriptScrollbarMetrics {
        reading_end,
        thumb_start,
        thumb_length,
        max_thumb_start,
    })
}

pub(super) fn render_transcript_scrollbar(frame: &mut Frame<'_>, app: &App) {
    let Some(area) = app.transcript_scrollbar_area else {
        return;
    };
    let reading_end = transcript_reading_end(app.transcript_rows, app.transcript_height);
    let mut state = ScrollbarState::new(reading_end.saturating_add(1))
        .position(app.transcript_offset.min(reading_end))
        .viewport_content_length(app.transcript_height);
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None)
        .track_symbol(Some("│"))
        .track_style(Style::default().fg(MUTED))
        .thumb_symbol("┃")
        .thumb_style(Style::default().fg(SCROLLBAR));
    frame.render_stateful_widget(scrollbar, area, &mut state);
}

pub(super) fn transcript_needs_gap(
    previous: BlockKind,
    previous_turn_result: bool,
    current: BlockKind,
    current_turn_result: bool,
) -> bool {
    (previous_turn_result
        && matches!(previous, BlockKind::Assistant | BlockKind::Error)
        && current == BlockKind::User)
        || matches!((previous, current), (BlockKind::User, BlockKind::Process))
        || (current_turn_result && matches!(current, BlockKind::Assistant | BlockKind::Error))
}

pub(super) fn transcript_live_tail(rows: usize, height: usize) -> usize {
    rows.saturating_sub(height)
}

pub(super) fn transcript_reading_end(rows: usize, height: usize) -> usize {
    rows.saturating_add(height / 2).saturating_sub(height)
}

pub(super) fn transcript_block_items(
    block: &DisplayBlock,
    selected: bool,
    live: bool,
    mut message_width: usize,
    mut process_width: usize,
    app: &App,
) -> Vec<(ListItem<'static>, TextRowSeparator)> {
    if block.parent_process.is_some() {
        message_width = message_width.saturating_sub(2).max(1);
        process_width = process_width.saturating_sub(2).max(1);
    }
    let background = match block.kind {
        BlockKind::User => USER_SURFACE,
        BlockKind::Assistant => BG,
        _ if selected => ROW_ACTIVE,
        _ => BG,
    };
    let open_hint = app.keymap.key_hint("main", "open").map_or_else(
        || "right-click opens full".to_string(),
        |key| format!("{key} or right-click opens full"),
    );
    let expand_hint = app.keymap.key_hint("main", "toggle").map_or_else(
        || "select to expand".to_string(),
        |key| format!("{key} to expand"),
    );
    let collapsed_hint = format!("  ▸ {expand_hint}");
    let mut rows = Vec::new();
    let mut row_separators = Vec::new();

    match block.kind {
        BlockKind::User => {
            for line in wrapped_block_lines_with_separators(&block.text, message_width) {
                row_separators.push(line.separator);
                rows.push(transcript_block_item(
                    block,
                    vec![Span::styled(line.text, Style::default().fg(TEXT))],
                    background,
                ));
            }
        }
        BlockKind::Assistant => {
            for rendered in markdown::render(&block.text, message_width) {
                let mut line = rendered.line;
                if block.parent_process.is_some() {
                    line.spans.insert(0, Span::raw("  "));
                }
                rows.push(ListItem::new(line).style(Style::default().bg(background)));
                row_separators.push(rendered.separator);
            }
        }
        BlockKind::Process => {
            let steps = block.process.as_ref().map_or(0, |process| process.steps);
            rows.push(transcript_block_item(
                block,
                vec![
                    Span::styled("◇ ", Style::default().fg(MUTED)),
                    Span::styled(
                        format!(
                            "Process · {steps} step{}",
                            if steps == 1 { "" } else { "s" }
                        ),
                        Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        if block.expanded {
                            "  ▾".to_string()
                        } else {
                            collapsed_hint.clone()
                        },
                        Style::default().fg(MUTED),
                    ),
                ],
                background,
            ));
        }
        BlockKind::Reasoning => {
            rows.push(transcript_block_item(
                block,
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
                            "  ▾".to_string()
                        } else {
                            collapsed_hint.clone()
                        },
                        Style::default().fg(MUTED),
                    ),
                ],
                background,
            ));
            if block.expanded {
                let (lines, extra) =
                    visible_block_lines(&block.text, process_width, EXPANDED_PREVIEW_LINES, live);
                if live && extra > 0 {
                    rows.push(transcript_hint(
                        extra,
                        "earlier",
                        true,
                        &open_hint,
                        &expand_hint,
                        background,
                        block.parent_process.is_some(),
                    ));
                }
                for line in lines {
                    rows.push(transcript_block_item(
                        block,
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
                if !live && extra > 0 {
                    rows.push(transcript_hint(
                        extra,
                        "more",
                        true,
                        &open_hint,
                        &expand_hint,
                        background,
                        block.parent_process.is_some(),
                    ));
                }
            }
        }
        BlockKind::Tool => {
            let header_color = if block.protocol_help_required {
                PURPLE
            } else if block.failed {
                ERROR
            } else {
                WARM
            };
            let has_result = block.tool.as_ref().map_or_else(
                || block.text.contains("\n\nRESULT\n"),
                |tool| tool.output.is_some(),
            );
            let status = if live {
                animation::spinner(app.frame).to_string()
            } else if block.failed {
                "×".to_string()
            } else if has_result {
                "✓".to_string()
            } else {
                "·".to_string()
            };
            rows.push(transcript_block_item(
                block,
                vec![
                    Span::styled(format!("{status} "), Style::default().fg(header_color)),
                    Span::styled(
                        block.title.clone(),
                        Style::default()
                            .fg(header_color)
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
                    rows.push(transcript_block_item(
                        block,
                        vec![
                            Span::raw("  "),
                            Span::styled(line, Style::default().fg(color)),
                        ],
                        background,
                    ));
                }
                if extra > 0 {
                    rows.push(transcript_hint(
                        extra,
                        "more",
                        true,
                        &open_hint,
                        &expand_hint,
                        background,
                        block.parent_process.is_some(),
                    ));
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
            let (lines, extra) = visible_block_lines(&block.text, process_width, limit, false);
            for (index, line) in lines.into_iter().enumerate() {
                rows.push(transcript_block_item(
                    block,
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
                    "more",
                    block.expanded,
                    &open_hint,
                    &expand_hint,
                    background,
                    block.parent_process.is_some(),
                ));
            }
        }
    }

    row_separators.resize(rows.len(), TextRowSeparator::Newline);
    rows.into_iter().zip(row_separators).collect()
}

pub(super) fn transcript_item(spans: Vec<Span<'static>>, background: Color) -> ListItem<'static> {
    ListItem::new(Line::from(spans)).style(Style::default().bg(background))
}

pub(super) fn transcript_block_item(
    block: &DisplayBlock,
    mut spans: Vec<Span<'static>>,
    background: Color,
) -> ListItem<'static> {
    if block.parent_process.is_some() {
        spans.insert(0, Span::raw("  "));
    }
    transcript_item(spans, background)
}

pub(super) fn transcript_hint(
    extra: usize,
    position: &str,
    expanded: bool,
    open_hint: &str,
    expand_hint: &str,
    background: Color,
    nested: bool,
) -> ListItem<'static> {
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(
            format!(
                "… {extra} {position} lines · {}",
                if expanded { open_hint } else { expand_hint }
            ),
            Style::default().fg(MUTED),
        ),
    ];
    if nested {
        spans.insert(0, Span::raw("  "));
    }
    transcript_item(spans, background)
}

pub(super) fn visible_block_lines(
    text: &str,
    width: usize,
    limit: usize,
    from_tail: bool,
) -> (Vec<String>, usize) {
    let mut wrapped = wrapped_block_lines(text, width);
    let extra = wrapped.len().saturating_sub(limit);
    if from_tail {
        wrapped = wrapped.split_off(extra);
    } else {
        wrapped.truncate(limit);
    }
    (wrapped, extra)
}

struct WrappedBlockLine {
    text: String,
    separator: TextRowSeparator,
}

pub(super) fn wrapped_block_lines(text: &str, width: usize) -> Vec<String> {
    wrapped_block_lines_with_separators(text, width)
        .into_iter()
        .map(|line| line.text)
        .collect()
}

fn wrapped_block_lines_with_separators(text: &str, width: usize) -> Vec<WrappedBlockLine> {
    let mut wrapped = Vec::new();
    for logical in text.lines() {
        if logical.is_empty() {
            wrapped.push(WrappedBlockLine {
                text: String::new(),
                separator: TextRowSeparator::Newline,
            });
        } else {
            let lines = textwrap::wrap(logical, width.max(1));
            let mut search_from = 0;
            let mut ranges = Vec::with_capacity(lines.len());
            for line in &lines {
                let content = line.as_ref();
                let start = logical[search_from..]
                    .find(content)
                    .map_or(search_from, |offset| search_from + offset);
                let end = start.saturating_add(content.len()).min(logical.len());
                ranges.push((start, end));
                search_from = end;
            }
            for (index, line) in lines.into_iter().enumerate() {
                let separator = if let Some((next_start, _)) = ranges.get(index + 1) {
                    if logical[ranges[index].1..*next_start]
                        .chars()
                        .any(char::is_whitespace)
                    {
                        TextRowSeparator::Space
                    } else {
                        TextRowSeparator::None
                    }
                } else {
                    TextRowSeparator::Newline
                };
                wrapped.push(WrappedBlockLine {
                    text: line.into_owned(),
                    separator,
                });
            }
        }
    }
    if wrapped.is_empty() {
        wrapped.push(WrappedBlockLine {
            text: String::new(),
            separator: TextRowSeparator::Newline,
        });
    }
    wrapped
}

pub(super) fn transient_notices(app: &App) -> Vec<(String, Color)> {
    app.visible_flashes()
        .rev()
        .map(|flash| {
            (
                flash.to_string(),
                if flash_is_error(flash) { ERROR } else { WARM },
            )
        })
        .collect()
}

pub(super) fn fixed_bottom_notices(app: &App) -> Vec<(String, Color)> {
    let mut notices = Vec::new();
    if let Some((key, _)) = app
        .last_interrupt_press
        .as_ref()
        .filter(|(_, at)| app.busy && at.elapsed() < DOUBLE_CLICK_INTERVAL)
    {
        let key = app.keymap.display_key(key).unwrap_or_else(|| key.clone());
        notices.push((format!("press {key} again to interrupt"), WARM));
    }
    if !app.pending_messages.is_empty() {
        let restore = app.keymap.key_hint("composer", "restore_pending");
        let message = restore.map_or_else(
            || {
                format!(
                    " {} pending · open composer to review",
                    app.pending_messages.len()
                )
            },
            |restore| {
                format!(
                    " {} pending · open composer and restore with {restore}",
                    app.pending_messages.len(),
                )
            },
        );
        notices.push((message, WARM));
    }
    if app.jump != JumpKind::All {
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
        let hints = action_hints(
            &app.keymap,
            &[("main", "clear", "clear"), ("main", "toggle", "open")],
        );
        let message = if hints.is_empty() {
            format!("{label} {position}/{}", indices.len())
        } else {
            format!("{label} {position}/{}   {hints}", indices.len())
        };
        notices.push((message, WARM));
    }
    notices
}

pub(super) fn bottom_notice_lines(notices: &[(String, Color)], width: u16) -> Vec<Line<'static>> {
    notices
        .iter()
        .flat_map(|(message, color)| {
            wrapped_block_lines(message, width.saturating_sub(2).max(1) as usize)
                .into_iter()
                .map(move |line| Line::styled(line, Style::default().fg(*color)))
        })
        .collect()
}

pub(super) fn flash_duration(message: &str) -> Duration {
    let characters = message.graphemes(true).count() as u64;
    FLASH_MIN_DURATION
        .saturating_add(Duration::from_millis(
            characters.saturating_mul(FLASH_MILLIS_PER_CHARACTER),
        ))
        .min(FLASH_MAX_DURATION)
}

pub(super) fn flash_is_error(flash: &str) -> bool {
    let flash = flash.to_ascii_lowercase();
    flash.contains("failed")
        || flash.contains("error")
        || flash.contains("invalid")
        || flash.contains("could not")
        || flash.contains("unknown")
}

pub(super) fn keymap_help(keymap: &Keymap) -> String {
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
        ("DOCUMENT", "document"),
        ("SELECTION", "selection"),
        ("GLOBAL", "global"),
    ] {
        output.push_str(title);
        output.push('\n');
        for (key, action) in keymap.display_bindings_for(mode) {
            output.push_str(&format!("  {key:<16} {}\n", action.replace('_', " ")));
        }
        output.push('\n');
    }
    output
}

pub(super) fn key_alternatives(keymap: &Keymap, bindings: &[(&str, &str)]) -> Option<String> {
    let mut keys = Vec::new();
    for (mode, action) in bindings {
        if let Some(key) = keymap.key_hint(mode, action)
            && !keys.contains(&key)
        {
            keys.push(key);
        }
    }
    (!keys.is_empty()).then(|| keys.join("/"))
}

pub(super) fn action_hints(keymap: &Keymap, hints: &[(&str, &str, &str)]) -> String {
    hints
        .iter()
        .filter_map(|(mode, action, label)| {
            keymap
                .key_hint(mode, action)
                .map(|key| format!("{key} {label}"))
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

pub(super) fn panel_title(name: &str, hints: String) -> String {
    if hints.is_empty() {
        format!(" {name} ")
    } else {
        format!(" {name} · {hints} ")
    }
}

pub(super) fn fit_panel_title(title: &str, width: u16) -> String {
    let limit = width.saturating_sub(2) as usize;
    if title.width() <= limit {
        title.to_string()
    } else {
        single_line_preview(title, limit)
    }
}

pub(super) fn command_help(commands: &CommandRegistry) -> String {
    commands
        .list()
        .into_iter()
        .map(|command| format!("  :{:<14} {}\n", command.id, command.description))
        .collect()
}

pub(super) fn common_command_prefix(names: &[String]) -> String {
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

pub(super) fn matching_commands(commands: &CommandRegistry, query: &str) -> Vec<CommandMatch> {
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

pub(super) fn fuzzy_score(haystack: &str, query: &str) -> Option<usize> {
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

pub(super) fn overlay_area(frame: Rect, app: &App, overlay: Overlay) -> Rect {
    match overlay {
        Overlay::Command => centered(frame, 72, 62),
        Overlay::Status => bottom_float(frame, 14),
        Overlay::Composer => bottom_float(
            frame,
            8 + pending_preview_height(app) + completion_preview_height(app),
        ),
        Overlay::Delivery => bottom_float(frame, 9),
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

pub(super) fn bottom_float(frame: Rect, desired_height: u16) -> Rect {
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

pub(super) fn active_protocols(app: &App) -> Vec<ProtocolDescriptor> {
    app.protocol_source
        .as_ref()
        .map_or_else(|| app.protocols.clone(), |source| source.descriptors())
}

pub(super) fn render_overlay(frame: &mut Frame<'_>, app: &mut App, overlay: Overlay) {
    let area = overlay_area(frame.area(), app, overlay);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(SURFACE).fg(TEXT))
        .padding(Padding::uniform(1));
    app.overlay_viewport_rows = block.inner(area).height as usize;
    match overlay {
        Overlay::Composer => {
            let pending_height = pending_preview_height(app);
            let completion_height = completion_preview_height(app);
            let mut constraints = Vec::new();
            if pending_height > 0 {
                constraints.push(Constraint::Length(pending_height));
            }
            if completion_height > 0 {
                constraints.push(Constraint::Length(completion_height));
            }
            constraints.push(Constraint::Min(8));
            let sections = Layout::default()
                .direction(Direction::Vertical)
                .constraints(constraints)
                .split(area);
            let mut section = 0;
            if pending_height > 0 {
                render_pending_messages(frame, app, sections[section]);
                section += 1;
            }
            if completion_height > 0 {
                render_composer_completions(frame, app, sections[section]);
                section += 1;
            }
            let composer_area = sections[section];
            app.sync_composer_chrome();
            frame.render_widget(&app.input, composer_area);
            if let Some(position) = composer_cursor_position(frame, &app.input, composer_area) {
                frame.set_cursor_position(position);
                app.composer_view = composer_view(&app.input, composer_area, position);
            }
        }
        Overlay::Delivery => render_delivery(frame, app, area, block),
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
                    .block(block.title(" HELP "))
                    .wrap(Wrap { trim: false })
                    .scroll((app.overlay_scroll, 0)),
                area,
            );
        }
        Overlay::Protocols => {
            let mut lines = Vec::new();
            for protocol in active_protocols(app) {
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
                    Line::styled(protocol.description, Style::default().fg(TEXT)),
                    Line::default(),
                ]);
            }
            frame.render_widget(
                Paragraph::new(lines)
                    .block(block.title(fit_panel_title(
                        " PROTOCOLS · read <name>://help ",
                        area.width,
                    )))
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
                .map(|document| format!(" {} ", document.title))
                .unwrap_or_else(|| " PLUGIN PANEL ".to_string());
            let body = document
                .map(|document| document.body.as_str())
                .unwrap_or("Plugin panel did not return content.");
            frame.render_widget(
                Paragraph::new(body)
                    .block(block.title(fit_panel_title(&title, area.width)))
                    .wrap(Wrap { trim: false })
                    .scroll((app.overlay_scroll, 0)),
                area,
            );
        }
        Overlay::Document => {
            let hints = action_hints(&app.keymap, &[("document", "copy", "copy")]);
            let (name, body) = app
                .document
                .as_ref()
                .map(|(title, body)| (title.as_str(), body.as_str()))
                .unwrap_or(("DOCUMENT", "Nothing to show."));
            let title = panel_title(name, hints);
            let inner_width = block.inner(area).width as usize;
            let lines = markdown::render(body, inner_width)
                .into_iter()
                .map(|rendered| rendered.line)
                .collect::<Vec<_>>();
            frame.render_widget(
                Paragraph::new(lines)
                    .block(block.title(fit_panel_title(&title, area.width)))
                    .scroll((app.overlay_scroll, 0)),
                area,
            );
        }
        Overlay::Selector => render_selector(frame, app, area, block),
        Overlay::Text => {
            let Some(prompt) = app.text_prompt.as_ref() else {
                return;
            };
            let inner_width = block.inner(area).width as usize;
            let value = if prompt.secret {
                "•".repeat(prompt.value.chars().count().min(48))
            } else {
                prompt.value.clone()
            };
            let value = format!(
                "{}█",
                single_line_tail(&value, inner_width.saturating_sub(1))
            );
            let title = format!(" {} ", prompt.title);
            frame.render_widget(
                Paragraph::new(vec![
                    Line::styled(prompt.message.clone(), Style::default().fg(MUTED)),
                    Line::default(),
                    Line::styled(value, Style::default().fg(TEXT)),
                ])
                .block(block.title(fit_panel_title(&title, area.width)))
                .wrap(Wrap { trim: false }),
                area,
            );
        }
        Overlay::Terminal => render_pty(frame, app, area),
        Overlay::Oauth => {
            let Some(oauth) = app.oauth.as_ref() else {
                return;
            };
            let inner_width = block.inner(area).width as usize;
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
                    format!(
                        "paste {}█",
                        single_line_tail(&oauth.paste, inner_width.saturating_sub(7))
                    ),
                    Style::default().fg(TEXT),
                ));
            }
            let title = format!(" OAUTH · {} ", oauth.provider);
            frame.render_widget(
                Paragraph::new(lines)
                    .block(block.title(fit_panel_title(&title, area.width)))
                    .wrap(Wrap { trim: false }),
                area,
            );
        }
    }
}

pub(super) fn render_pty(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let mut hints = app
        .keymap
        .key_hint("terminal", "escape")
        .map(|key| format!("double {key} close"))
        .into_iter()
        .collect::<Vec<_>>();
    let shift = app
        .keymap
        .modifier_hint("shift")
        .unwrap_or_else(|| "Shift".to_string());
    hints.push(format!("{shift}-drag select"));
    let title = panel_title("TERMINAL", hints.join(" · "));
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
                    .title(fit_panel_title(&title, area.width)),
            ),
            area,
        );
        resize_error
    };
    if let Some(error) = resize_error {
        app.set_flash(format!("Embedded terminal resize failed: {error:#}"));
    }
}

pub(super) fn render_status(frame: &mut Frame<'_>, app: &mut App, area: Rect, block: Block<'_>) {
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
            "LOG",
            display_path(&app.info.diagnostics_path),
            Style::default().fg(MUTED),
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
            context_status(app, percent),
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
            format!("{} registered", active_protocols(app).len()),
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
            .block(block.title(" STATUS "))
            .wrap(Wrap { trim: false })
            .scroll((app.overlay_scroll, 0)),
        area,
    );
}

pub(super) fn status_row(
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

pub(super) fn render_command(frame: &mut Frame<'_>, app: &mut App, area: Rect, block: Block<'_>) {
    let inner = block.inner(area);
    let title = panel_title(
        "COMMAND",
        action_hints(&app.keymap, &[("command", "complete", "complete")]),
    );
    frame.render_widget(block.title(fit_panel_title(&title, area.width)), area);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(3)])
        .split(inner);
    app.overlay_viewport_rows = sections[1].height as usize;
    let query_width = sections[0].width.saturating_sub(3) as usize;
    frame.render_widget(
        Paragraph::new(format!(
            "⌕ {}█",
            single_line_tail(&app.command_query, query_width)
        ))
        .style(Style::default().fg(TEXT)),
        sections[0],
    );
    let commands = app.matching_commands();
    let marquee_elapsed = commands
        .get(app.command_selected)
        .map(|item| app.marquee_elapsed(format!("command:{}:{}", item.spec.id, item.name)))
        .unwrap_or_default();
    let row_width = sections[1].width as usize;
    let name_width = 16.min(row_width.saturating_sub(2));
    let description_width = row_width.saturating_sub(2 + name_width);
    let items = commands.iter().enumerate().map(|(index, item)| {
        let selected = index == app.command_selected;
        ListItem::new(Line::from(vec![
            Span::styled(
                if selected { "› " } else { "  " },
                Style::default().fg(ACCENT),
            ),
            Span::styled(
                list_cell(
                    &format!(":{}", item.name),
                    name_width,
                    selected,
                    marquee_elapsed,
                ),
                Style::default()
                    .fg(if selected { ACCENT } else { TEXT })
                    .add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            Span::styled(
                list_cell(
                    &item.spec.description,
                    description_width,
                    selected,
                    marquee_elapsed,
                ),
                Style::default().fg(MUTED),
            ),
        ]))
        .style(Style::default().bg(if selected { ROW_ACTIVE } else { SURFACE }))
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

const PENDING_PREVIEW_LIMIT: usize = 4;
const COMPLETION_PREVIEW_LIMIT: usize = 6;

pub(super) fn completion_preview_height(app: &App) -> u16 {
    app.completions.as_ref().map_or(0, |completions| {
        completions.result.items.len().min(COMPLETION_PREVIEW_LIMIT) as u16 + 2
    })
}

pub(super) fn render_composer_completions(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let selected_key = app.completions.as_ref().and_then(|completions| {
        completions
            .result
            .items
            .get(completions.selected)
            .map(|item| {
                format!(
                    "completion:{}:{}",
                    app.completion_generation, item.insert_text
                )
            })
    });
    let marquee_elapsed = selected_key
        .map(|key| app.marquee_elapsed(key))
        .unwrap_or_default();
    let Some(completions) = app.completions.as_ref() else {
        return;
    };
    let select = key_alternatives(
        &app.keymap,
        &[("composer", "cursor_up"), ("composer", "cursor_down")],
    );
    let insert = key_alternatives(
        &app.keymap,
        &[("composer", "submit"), ("composer", "complete")],
    );
    let hints = [
        select.map(|keys| format!("{keys} select")),
        insert.map(|keys| format!("{keys} insert")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ");
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(MUTED))
        .style(Style::default().bg(SURFACE).fg(TEXT))
        .title(fit_panel_title(
            &panel_title("REFERENCES", hints),
            area.width,
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let count = completions.result.items.len().min(inner.height as usize);
    let offset = completions
        .selected
        .saturating_sub(count.saturating_sub(1))
        .min(completions.result.items.len().saturating_sub(count));
    let row_width = inner.width as usize;
    let available = row_width.saturating_sub(2);
    let minimum_label_width = 8.min(available);
    let minimum_description_width = 12.min(available.saturating_sub(minimum_label_width));
    let desired_label_width = completions
        .result
        .items
        .iter()
        .map(|item| {
            normalized_single_line(&item.label)
                .width()
                .saturating_add(2)
        })
        .max()
        .unwrap_or_default()
        .min(36);
    let label_width = desired_label_width
        .max(minimum_label_width)
        .min(available.saturating_sub(minimum_description_width));
    let label_content_width = label_width.saturating_sub(2);
    let separator_width = label_width.saturating_sub(label_content_width);
    let description_width = available.saturating_sub(label_width);
    let lines = completions
        .result
        .items
        .iter()
        .enumerate()
        .skip(offset)
        .take(count)
        .map(|(index, item)| {
            let selected = index == completions.selected;
            Line::from(vec![
                Span::styled(
                    if selected { "› " } else { "  " },
                    Style::default().fg(ACCENT),
                ),
                Span::styled(
                    list_cell(&item.label, label_content_width, selected, marquee_elapsed),
                    Style::default()
                        .fg(if selected { ACCENT } else { TEXT })
                        .add_modifier(if selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
                Span::raw(" ".repeat(separator_width)),
                Span::styled(
                    list_cell(
                        &item.description,
                        description_width,
                        selected,
                        marquee_elapsed,
                    ),
                    Style::default().fg(MUTED),
                ),
            ])
            .style(Style::default().bg(if selected { ROW_ACTIVE } else { SURFACE }))
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), inner);
    for (row, index) in (offset..offset + count).enumerate() {
        app.hit_regions.push(HitRegion {
            area: Rect::new(inner.x, inner.y.saturating_add(row as u16), inner.width, 1),
            target: AppHit::Completion(index),
        });
    }
}

pub(super) fn pending_preview_height(app: &App) -> u16 {
    if app.pending_messages.is_empty() {
        0
    } else {
        1 + app.pending_messages.len().min(PENDING_PREVIEW_LIMIT) as u16
            + u16::from(app.pending_messages.len() > PENDING_PREVIEW_LIMIT)
    }
}

pub(super) fn render_pending_messages(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let hints = action_hints(
        &app.keymap,
        &[
            ("composer", "restore_pending", "restore latest"),
            ("composer", "upgrade_pending", "upgrade latest queue"),
        ],
    );
    let heading = if hints.is_empty() {
        "Pending".to_string()
    } else {
        format!("Pending · {hints}")
    };
    let mut lines = vec![Line::styled(
        single_line_preview(&heading, area.width as usize),
        Style::default().fg(MUTED).bg(SURFACE),
    )];
    let hidden = app
        .pending_messages
        .len()
        .saturating_sub(PENDING_PREVIEW_LIMIT);
    for message in app.pending_messages.iter().skip(hidden) {
        let label = match message.kind {
            PendingMessageKind::Queued => "QUEUE",
            PendingMessageKind::Guidance => "GUIDE",
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {label:<5} "),
                Style::default().fg(ACCENT).bg(SURFACE),
            ),
            Span::styled(
                single_line_preview(
                    &message.text.replace(['\r', '\n'], " ↵ "),
                    area.width.saturating_sub(8) as usize,
                ),
                Style::default().fg(TEXT).bg(SURFACE),
            ),
        ]));
    }
    if hidden > 0 {
        lines.push(Line::styled(
            format!(" … {hidden} earlier pending"),
            Style::default().fg(MUTED).bg(SURFACE),
        ));
    }
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(SURFACE)),
        area,
    );
}

pub(super) fn render_delivery(frame: &mut Frame<'_>, app: &mut App, area: Rect, block: Block<'_>) {
    let Some(delivery) = app.delivery.as_ref() else {
        return;
    };
    let inner = block.inner(area);
    let select = key_alternatives(&app.keymap, &[("list", "previous"), ("list", "next")]);
    let mut hints = select
        .map(|keys| format!("{keys} select"))
        .into_iter()
        .collect::<Vec<_>>();
    let actions = action_hints(
        &app.keymap,
        &[("list", "confirm", "choose"), ("list", "close", "back")],
    );
    if !actions.is_empty() {
        hints.push(actions);
    }
    frame.render_widget(
        block.title(fit_panel_title(
            &panel_title("SEND WHILE RUNNING", hints.join(" · ")),
            area.width,
        )),
        area,
    );
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(2)])
        .split(inner);
    frame.render_widget(
        Paragraph::new(single_line_preview(&app.draft_text(), inner.width as usize))
            .style(Style::default().fg(MUTED)),
        sections[0],
    );
    let choices = [
        ("Queue", "Run after the current turn finishes"),
        ("Guidance", "Add before the next model request"),
    ];
    let items = choices
        .iter()
        .enumerate()
        .map(|(index, (title, description))| {
            let selected = delivery.selected == index;
            ListItem::new(Line::from(vec![
                Span::styled(
                    if selected { "› " } else { "  " },
                    Style::default().fg(ACCENT),
                ),
                Span::styled(
                    format!("{title:<12}"),
                    Style::default().fg(if selected { ACCENT } else { TEXT }),
                ),
                Span::styled(*description, Style::default().fg(MUTED)),
            ]))
            .style(Style::default().bg(if selected { ROW_ACTIVE } else { SURFACE }))
        });
    let mut state = ListState::default().with_selected(Some(delivery.selected));
    frame.render_stateful_widget(List::new(items), sections[1], &mut state);
    for index in 0..choices.len() {
        let y = sections[1].y.saturating_add(index as u16);
        if y < sections[1].bottom() {
            app.hit_regions.push(HitRegion {
                area: Rect::new(sections[1].x, y, sections[1].width, 1),
                target: AppHit::Delivery(index),
            });
        }
    }
}

pub(super) fn render_selector(frame: &mut Frame<'_>, app: &mut App, area: Rect, block: Block<'_>) {
    let inner = block.inner(area);
    let selected_key = app.selector.as_ref().and_then(|selector| {
        selector
            .visible
            .get(selector.selected)
            .and_then(|index| selector.items.get(*index))
            .map(|item| format!("selector:{}:{}", selector.title, item.id))
    });
    let marquee_elapsed = selected_key
        .map(|key| app.marquee_elapsed(key))
        .unwrap_or_default();
    let Some(selector) = app.selector.as_ref() else {
        return;
    };
    frame.render_widget(
        block.title(fit_panel_title(
            &format!(" {} ", selector.title),
            area.width,
        )),
        area,
    );
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(3)])
        .split(inner);
    app.overlay_viewport_rows = sections[1].height as usize;
    let query_width = sections[0].width.saturating_sub(3) as usize;
    frame.render_widget(
        Paragraph::new(format!(
            "⌕ {}█",
            single_line_tail(&selector.query, query_width)
        ))
        .style(Style::default().fg(TEXT)),
        sections[0],
    );
    let row_width = sections[1].width as usize;
    let title_width = 22.min(row_width.saturating_sub(2));
    let description_width = row_width.saturating_sub(2 + title_width);
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
                        list_cell(&item.title, title_width, selected, marquee_elapsed),
                        Style::default().fg(if selected { ACCENT } else { TEXT }),
                    ),
                    Span::styled(
                        list_cell(
                            &item.description,
                            description_width,
                            selected,
                            marquee_elapsed,
                        ),
                        Style::default().fg(MUTED),
                    ),
                ]))
                .style(Style::default().bg(if selected {
                    ROW_ACTIVE
                } else {
                    SURFACE
                })),
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

pub(super) fn render_models(frame: &mut Frame<'_>, app: &mut App, area: Rect, block: Block<'_>) {
    let inner = block.inner(area);
    let name = match &app.model_selection_target {
        ModelSelectionTarget::Conversation => "MODELS".to_string(),
        ModelSelectionTarget::Role(role) => format!("MODELS · ROLE {role}"),
    };
    let title = panel_title(
        &name,
        action_hints(&app.keymap, &[("models", "refresh", "refresh")]),
    );
    frame.render_widget(block.title(fit_panel_title(&title, area.width)), area);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(4),
            Constraint::Length(3),
        ])
        .split(inner);
    app.overlay_viewport_rows = sections[1].height as usize;
    let selected_key = app.model_selector.as_ref().and_then(|selector| {
        selector
            .selected()
            .map(|model| format!("model:{}/{}", model.provider, model.id))
    });
    let marquee_elapsed = selected_key
        .map(|key| app.marquee_elapsed(key))
        .unwrap_or_default();
    let Some(selector) = app.model_selector.as_ref() else {
        frame.render_widget(
            Paragraph::new("Model catalog is not loaded.").style(Style::default().fg(MUTED)),
            inner,
        );
        return;
    };
    let summary = if app.catalog_refreshing {
        format!(
            "{} refreshing model catalogs",
            animation::spinner(app.frame)
        )
    } else {
        format!(
            "{} matches · {} models · {} providers",
            selector.visible_len(),
            selector.model_count(),
            selector.provider_count()
        )
    };
    let query_width = sections[0].width.saturating_sub(6) as usize;
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("⌕  ", Style::default().fg(ACCENT)),
            Span::styled(
                single_line_tail(selector.query(), query_width),
                Style::default().fg(TEXT),
            ),
            Span::styled("█", Style::default().fg(ACCENT)),
        ]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(MUTED))
                .title(fit_panel_title(
                    &format!(" SEARCH · {summary} "),
                    sections[0].width,
                )),
        ),
        sections[0],
    );
    let row_width = sections[1].width as usize;
    let desired_name_width: usize = if sections[1].width < 60 { 18 } else { 30 };
    let provider_width = 14.min(row_width.saturating_sub(4));
    let items = selector.visible().enumerate().map(|(position, model)| {
        let selected = position == selector.selected_position();
        let details = format!(
            "{}{}",
            context_label(model.context_window()),
            if reasoning(model) { " · think" } else { "" }
        );
        let available = row_width.saturating_sub(4 + provider_width);
        let minimum_name_width = 8.min(available);
        let reserved_details = details
            .width()
            .min(available.saturating_sub(minimum_name_width));
        let name_width = desired_name_width.min(available.saturating_sub(reserved_details));
        let details_width = available.saturating_sub(name_width);
        ListItem::new(Line::from(vec![
            Span::styled(
                if selected { "› " } else { "  " },
                Style::default().fg(ACCENT),
            ),
            Span::styled(
                if selector.is_current(model) {
                    "● "
                } else {
                    "  "
                },
                Style::default().fg(MUTED),
            ),
            Span::styled(
                list_cell(&model.provider, provider_width, selected, marquee_elapsed),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                list_cell(model_label(model), name_width, selected, marquee_elapsed),
                Style::default().fg(if selected { ACCENT } else { TEXT }),
            ),
            Span::styled(
                single_line_preview(&details, details_width),
                Style::default().fg(MUTED),
            ),
        ]))
        .style(Style::default().bg(if selected { ROW_ACTIVE } else { SURFACE }))
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
        Paragraph::new(footer)
            .style(Style::default().fg(MUTED))
            .wrap(Wrap { trim: false }),
        sections[2],
    );
}

pub(super) fn render_settings(frame: &mut Frame<'_>, app: &mut App, area: Rect, block: Block<'_>) {
    let inner = block.inner(area);
    let selected_key = app.settings.as_ref().map(|settings| {
        format!(
            "settings:{}:{}/{}",
            settings.selected, settings.active.provider, settings.active.model
        )
    });
    let marquee_elapsed = selected_key
        .map(|key| app.marquee_elapsed(key))
        .unwrap_or_default();
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
    let edit = app.keymap.key_hint("settings", "edit");
    let environment = edit.as_ref().map_or_else(
        || {
            format!(
                "{} variable{}",
                settings.environment_count,
                if settings.environment_count == 1 {
                    ""
                } else {
                    "s"
                }
            )
        },
        |key| {
            format!(
                "{} variable{} · {key} manages",
                settings.environment_count,
                if settings.environment_count == 1 {
                    ""
                } else {
                    "s"
                }
            )
        },
    );
    let rows = [
        ("Model", format!("{} / {model}", settings.provider())),
        ("Credential", credential),
        ("Thinking", settings.thinking.to_string()),
        ("Output limit", output_limit),
        ("Agent environment", environment),
    ];
    let edit_help = edit.map_or_else(
        || "Use :login / :logout for credentials.".to_string(),
        |key| format!("Use :login / :logout for credentials. {key} edits the selected field."),
    );
    let row_count = rows.len();
    let row_width = inner.width as usize;
    let label_width = 18.min(row_width.saturating_sub(2));
    let value_width = row_width.saturating_sub(2 + label_width);
    let mut lines = vec![
        Line::styled(
            single_line_preview(&edit_help, row_width),
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
                list_cell(label, label_width, selected, marquee_elapsed),
                Style::default().fg(if selected { ACCENT } else { MUTED }),
            ),
            Span::styled(
                list_cell(&value, value_width, selected, marquee_elapsed),
                Style::default().fg(TEXT),
            ),
        ]));
        lines.push(Line::default());
    }
    let title = panel_title(
        "SETTINGS",
        action_hints(&app.keymap, &[("settings", "save", "save")]),
    );
    frame.render_widget(
        Paragraph::new(lines).block(block.title(fit_panel_title(&title, area.width))),
        area,
    );
    for index in 0..row_count {
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

pub(super) fn render_tasks(frame: &mut Frame<'_>, app: &mut App, area: Rect, block: Block<'_>) {
    if app.task_records.is_empty() {
        frame.render_widget(
            Paragraph::new("No managed tasks in this session.")
                .block(block.title(" TASKS "))
                .style(Style::default().fg(MUTED)),
            area,
        );
        return;
    }
    let inner = block.inner(area);
    let title = panel_title(
        "TASKS",
        action_hints(&app.keymap, &[("tasks", "cancel", "cancel")]),
    );
    frame.render_widget(block.title(fit_panel_title(&title, area.width)), area);
    let selected_key = app
        .task_records
        .get(app.selected_task)
        .map(|task| format!("task:{}", task.id));
    let marquee_elapsed = selected_key
        .map(|key| app.marquee_elapsed(key))
        .unwrap_or_default();
    let label_width = (inner.width as usize).saturating_sub(12);
    let items = app.task_records.iter().enumerate().map(|(index, task)| {
        let selected = index == app.selected_task;
        ListItem::new(Line::from(vec![
            Span::raw(if selected { "› " } else { "  " }),
            Span::raw(format!("{:<10}", task.status.as_str())),
            Span::raw(list_cell(
                &task.label,
                label_width,
                selected,
                marquee_elapsed,
            )),
        ]))
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

pub(super) fn style_input(input: &mut TextArea<'static>, busy: bool, keymap: &Keymap) {
    let border = ACCENT;
    let hints = if busy {
        action_hints(
            keymap,
            &[
                ("composer", "submit", "choose delivery"),
                ("composer", "newline", "newline"),
                ("composer", "close", "keep draft"),
            ],
        )
    } else {
        action_hints(
            keymap,
            &[
                ("composer", "submit", "send"),
                ("composer", "newline", "newline"),
                ("composer", "paste_image", "image"),
            ],
        )
    };
    let footer = format!(" {hints} ");
    input.set_block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border))
            .title(Line::styled(
                " MESSAGE ",
                Style::default().fg(border).add_modifier(Modifier::BOLD),
            ))
            .title_bottom(Line::styled(footer, Style::default().fg(MUTED)).right_aligned())
            .style(Style::default().bg(SURFACE)),
    );
    input.set_placeholder_text(if busy {
        "Add guidance or queue a follow-up…"
    } else {
        "Ask URI Agent to build, explain, or fix…"
    });
    input.set_placeholder_style(Style::default().fg(MUTED).bg(SURFACE));
    input.set_style(Style::default().fg(TEXT).bg(SURFACE));
    input.set_cursor_line_style(Style::default().fg(TEXT).bg(SURFACE));
    input.set_cursor_style(Style::default().fg(SURFACE).bg(border));
    input.set_selection_style(Style::default().fg(TEXT).bg(ACCENT));
    input.set_wrap_mode(WrapMode::WordOrGlyph);
}

pub(super) fn composer_view(
    input: &TextArea<'_>,
    area: Rect,
    cursor_position: (u16, u16),
) -> Option<ComposerView> {
    let inner = input.block().map_or(area, |block| block.inner(area));
    if inner.is_empty() {
        return None;
    }
    let rows = composer_visual_rows(input.lines(), inner.width as usize, input.tab_length());
    let cursor = input.cursor();
    let cursor_visual_row = rows.iter().enumerate().find_map(|(index, wrapped)| {
        if wrapped.logical_row != cursor.0 {
            return None;
        }
        let last_in_line = rows
            .get(index + 1)
            .is_none_or(|next| next.logical_row != wrapped.logical_row);
        ((wrapped.start_col <= cursor.1)
            && (cursor.1 < wrapped.end_col || (last_in_line && cursor.1 == wrapped.end_col)))
            .then_some(index)
    })?;
    let cursor_screen_row = cursor_position.1.saturating_sub(inner.y) as usize;
    Some(ComposerView {
        inner,
        top: cursor_visual_row.saturating_sub(cursor_screen_row),
        rows,
    })
}

pub(super) fn composer_visual_rows(
    lines: &[String],
    width: usize,
    tab_length: u8,
) -> Vec<ComposerVisualRow> {
    let mut rows = Vec::new();
    for (logical_row, line) in lines.iter().enumerate() {
        let mut start_col = 0usize;
        for (start_byte, end_byte) in composer_line_ranges(line, width.max(1), tab_length) {
            let end_col = start_col + line[start_byte..end_byte].chars().count();
            rows.push(ComposerVisualRow {
                logical_row,
                start_col,
                end_col,
            });
            start_col = end_col;
        }
    }
    rows
}

pub(super) fn composer_line_ranges(
    line: &str,
    width: usize,
    tab_length: u8,
) -> Vec<(usize, usize)> {
    let chunks = UnicodeSegmentation::split_word_bound_indices(line)
        .map(|(start, text)| (start, start + text.len()))
        .collect::<Vec<_>>();
    if chunks.is_empty() {
        return vec![(0, 0)];
    }

    let mut ranges = Vec::new();
    let mut index = 0usize;
    let mut start = chunks[0].0;
    let mut end = start;
    let mut line_width = 0usize;
    while index < chunks.len() {
        let chunk = chunks[index];
        if end == start {
            start = chunk.0;
        }
        let next_width = display_width_str(&line[chunk.0..chunk.1], line_width, tab_length);
        if next_width <= width {
            end = chunk.1;
            line_width = next_width;
            index += 1;
        } else if end > start {
            ranges.push((start, end));
            start = end;
            line_width = 0;
        } else {
            split_composer_graphemes(line, chunk.0, chunk.1, width, tab_length, &mut ranges);
            index += 1;
            start = chunk.1;
            end = chunk.1;
            line_width = 0;
        }
    }
    if end > start {
        ranges.push((start, end));
    }
    ranges
}

pub(super) fn split_composer_graphemes(
    line: &str,
    start: usize,
    end: usize,
    width: usize,
    tab_length: u8,
    ranges: &mut Vec<(usize, usize)>,
) {
    let mut segment_start = start;
    while segment_start < end {
        let mut segment_end = segment_start;
        let mut segment_width = 0usize;
        for (offset, grapheme) in
            UnicodeSegmentation::grapheme_indices(&line[segment_start..end], true)
        {
            let grapheme_start = segment_start + offset;
            let grapheme_end = grapheme_start + grapheme.len();
            let next_width = display_width_str(grapheme, segment_width, tab_length);
            if segment_end != segment_start && next_width > width {
                break;
            }
            segment_end = grapheme_end;
            segment_width = next_width;
            if segment_width > width {
                break;
            }
        }
        if segment_end == segment_start {
            segment_end = line[segment_start..end]
                .chars()
                .next()
                .map_or(end, |character| segment_start + character.len_utf8());
        }
        ranges.push((segment_start, segment_end));
        segment_start = segment_end;
    }
}

pub(super) fn display_width_str(text: &str, mut width: usize, tab_length: u8) -> usize {
    for character in text.chars() {
        width = display_width_to(character, width, tab_length);
    }
    width
}

pub(super) fn display_width_to(character: char, width: usize, tab_length: u8) -> usize {
    if character == '\t' && tab_length > 0 {
        let tab_length = tab_length as usize;
        width + tab_length - width % tab_length
    } else {
        width + character.width().unwrap_or(0)
    }
}

pub(super) fn composer_cursor_position(
    frame: &mut Frame<'_>,
    input: &TextArea<'_>,
    area: Rect,
) -> Option<(u16, u16)> {
    let inner = input.block().map_or(area, |block| block.inner(area));
    if inner.is_empty() {
        return None;
    }

    let cursor_style = input.cursor_style();
    let foreground = cursor_style.fg?;
    let background = cursor_style.bg?;
    let buffer = frame.buffer_mut();
    for y in inner.y..inner.bottom() {
        for x in inner.x..inner.right() {
            let cell = buffer.cell((x, y))?;
            if cell.fg == foreground && cell.bg == background {
                return Some((x, y));
            }
        }
    }
    None
}

pub(super) fn centered(area: Rect, width_percent: u16, height_percent: u16) -> Rect {
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

pub(super) fn capture_surface(
    frame: &mut Frame<'_>,
    app: &mut App,
    area: Rect,
    row_separators: Option<Vec<TextRowSeparator>>,
    left_padding: usize,
) {
    let cells = (area.y..area.bottom())
        .map(|row| {
            let mut hidden_cells = 0;
            (area.x..area.right())
                .map(|column| {
                    let Some(cell) = frame.buffer_mut().cell((column, row)) else {
                        return String::new();
                    };
                    if hidden_cells > 0 {
                        hidden_cells -= 1;
                        return String::new();
                    }
                    hidden_cells = cell.cell_width().saturating_sub(1);
                    cell.symbol().to_string()
                })
                .collect()
        })
        .collect::<Vec<_>>();
    let mut row_separators =
        row_separators.unwrap_or_else(|| vec![TextRowSeparator::Newline; cells.len()]);
    row_separators.resize(cells.len(), TextRowSeparator::Newline);
    row_separators.truncate(cells.len());
    app.selectable = Some(SelectableSurface {
        area,
        cells,
        row_separators,
        left_padding,
    });
}

pub(super) fn render_selection(frame: &mut Frame<'_>, app: &App) {
    let (Some(surface), Some(selection)) = (&app.selectable, app.selection) else {
        return;
    };
    let clamp = |point: (u16, u16)| {
        (
            point
                .0
                .clamp(surface.area.x, surface.area.right().saturating_sub(1)),
            point
                .1
                .clamp(surface.area.y, surface.area.bottom().saturating_sub(1)),
        )
    };
    let first = clamp(selection.start);
    let second = clamp(selection.end);
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

pub(super) fn update_mouse_selection(
    app: &mut App,
    mouse: MouseEvent,
    require_shift: bool,
) -> bool {
    let Some(area) = app.selectable.as_ref().map(|surface| surface.area) else {
        return false;
    };
    let point = (
        mouse.column.clamp(area.x, area.right().saturating_sub(1)),
        mouse.row.clamp(area.y, area.bottom().saturating_sub(1)),
    );
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left)
            if (!require_shift || mouse.modifiers.contains(KeyModifiers::SHIFT))
                && area.contains(point.into()) =>
        {
            let double_click = is_double_click(
                &mut app.last_text_click,
                TextClickTarget::Surface(app.overlay, point),
            );
            if double_click
                && let Some(selection) = app
                    .selectable
                    .as_ref()
                    .and_then(|surface| surface_word_selection(surface, point))
            {
                app.selection = Some(selection);
                app.mouse_word_selecting = true;
                return true;
            }
            app.selection = Some(TextSelection {
                start: point,
                end: point,
            });
            app.mouse_word_selecting = false;
            true
        }
        MouseEventKind::Drag(MouseButton::Left) if app.selection.is_some() => {
            app.last_text_click = None;
            app.mouse_word_selecting = false;
            if let Some(selection) = app.selection.as_mut() {
                selection.end = point;
            }
            true
        }
        MouseEventKind::Up(MouseButton::Left) if app.selection.is_some() => {
            if app.mouse_word_selecting {
                app.mouse_word_selecting = false;
                return true;
            }
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

pub(super) fn surface_word_selection(
    surface: &SelectableSurface,
    point: (u16, u16),
) -> Option<TextSelection> {
    let row = point.1.saturating_sub(surface.area.y) as usize;
    let cells = surface.cells.get(row)?;
    let last_column = cells.len().checked_sub(1)?;
    let column = point.0.saturating_sub(surface.area.x) as usize;
    let clicked = (0..=column.min(last_column))
        .rev()
        .find(|index| !cells[*index].is_empty())?;
    let text = cells.concat();
    let clicked_character = cells[..clicked]
        .iter()
        .map(|cell| cell.chars().count())
        .sum();
    let (word_start, word_end) = word_bounds_at(&text, clicked_character)?;

    let mut offset = 0usize;
    let mut start = None;
    let mut end = None;
    for (index, cell) in cells.iter().enumerate() {
        let next = offset + cell.chars().count();
        if next > word_start && offset < word_end {
            start.get_or_insert(index);
            end = Some(index);
        }
        offset = next;
    }
    let start = start?;
    let mut end = end?;
    while end + 1 < cells.len() && cells[end + 1].is_empty() {
        end += 1;
    }
    Some(TextSelection {
        start: (surface.area.x + start as u16, point.1),
        end: (surface.area.x + end as u16, point.1),
    })
}

pub(super) fn copy_current_surface(app: &mut App) {
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
    copy_text_with_osc52(app, &text);
    app.selection = None;
}

pub(super) fn last_assistant_response(app: &App) -> Option<&str> {
    app.blocks
        .iter()
        .rev()
        .find(|block| block.kind == BlockKind::Assistant && !block.text.trim().is_empty())
        .map(|block| block.text.as_str())
}

pub(super) fn copy_last_assistant_response(app: &mut App) {
    let Some(text) = last_assistant_response(app).map(str::to_string) else {
        app.set_flash("No assistant response to copy yet");
        return;
    };
    copy_text_with_osc52(app, &text);
}

pub(super) fn copy_document(app: &mut App) {
    let Some(text) = app
        .document
        .as_ref()
        .map(|(_, body)| body.clone())
        .filter(|body| !body.trim().is_empty())
    else {
        app.set_flash("Nothing visible can be copied");
        return;
    };
    copy_text_with_osc52(app, &text);
}

pub(super) fn copy_composer_selection(app: &mut App) {
    let Some(text) = composer_selected_text(&app.input) else {
        return;
    };
    copy_text_with_osc52(app, &text);
}

pub(super) fn composer_has_selection(input: &TextArea<'_>) -> bool {
    input
        .selection_range()
        .is_some_and(|(start, end)| start != end)
}

pub(super) fn composer_selected_text(input: &TextArea<'_>) -> Option<String> {
    let (start, end) = input.selection_range()?;
    if start == end {
        return None;
    }
    if start.0 == end.0 {
        return Some(
            input.lines()[start.0]
                .chars()
                .skip(start.1)
                .take(end.1.saturating_sub(start.1))
                .collect(),
        );
    }
    let mut selected = input.lines()[start.0]
        .chars()
        .skip(start.1)
        .collect::<String>();
    for line in &input.lines()[start.0 + 1..end.0] {
        selected.push('\n');
        selected.push_str(line);
    }
    selected.push('\n');
    selected.extend(input.lines()[end.0].chars().take(end.1));
    Some(selected)
}

pub(super) fn copy_text_with_osc52(app: &mut App, text: &str) {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let result = write!(stdout(), "\x1b]52;c;{encoded}\x07").and_then(|()| stdout().flush());
    app.set_flash(if result.is_ok() {
        format!("Copied {} characters with OSC52", text.chars().count())
    } else {
        "Could not write OSC52 clipboard data".to_string()
    });
}

pub(super) fn selected_surface_text(
    surface: &SelectableSurface,
    selection: TextSelection,
) -> String {
    if surface.cells.is_empty() {
        return String::new();
    }
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
    let mut text = String::new();
    let last_row = end_y.min(surface.cells.len().saturating_sub(1));
    for row in start_y..=last_row {
        let cells = &surface.cells[row];
        let from = if row == start_y {
            start_x
        } else if surface.row_separators[row - 1] != TextRowSeparator::Newline {
            first_content_cell(cells)
        } else {
            surface.left_padding.min(cells.len())
        };
        let to = if row == end_y {
            end_x.saturating_add(1)
        } else {
            cells.len()
        };
        text.push_str(
            cells[from.min(cells.len())..to.min(cells.len())]
                .concat()
                .trim_end(),
        );
        if row < last_row {
            push_row_separator(&mut text, surface.row_separators[row]);
        }
    }
    text
}

pub(super) fn complete_surface_text(surface: &SelectableSurface) -> String {
    let mut text = String::new();
    for (row, cells) in surface.cells.iter().enumerate() {
        let from = if row > 0 && surface.row_separators[row - 1] != TextRowSeparator::Newline {
            first_content_cell(cells)
        } else {
            surface.left_padding.min(cells.len())
        };
        text.push_str(cells[from..].concat().trim_end());
        if row + 1 < surface.cells.len() {
            push_row_separator(&mut text, surface.row_separators[row]);
        }
    }
    text.trim().to_string()
}

fn first_content_cell(cells: &[String]) -> usize {
    cells
        .iter()
        .position(|cell| !cell.chars().all(char::is_whitespace))
        .unwrap_or(cells.len())
}

fn push_row_separator(text: &mut String, separator: TextRowSeparator) {
    match separator {
        TextRowSeparator::None => {}
        TextRowSeparator::Space => text.push(' '),
        TextRowSeparator::Newline => text.push('\n'),
    }
}

/// Pi's `formatCwdForFooter`: replace the home directory prefix with `~`.
pub(super) fn footer_cwd(path: &Path) -> String {
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
pub(super) fn format_tokens(count: u64) -> String {
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

pub(super) fn current_branch(app: &mut App) -> Option<String> {
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
pub(super) fn git_branch(cwd: &Path) -> Option<String> {
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

pub(super) fn head_branch(gitdir: &Path) -> Option<String> {
    let head = std::fs::read_to_string(gitdir.join("HEAD")).ok()?;
    let head = head.trim();
    Some(
        head.strip_prefix("ref: refs/heads/")
            .unwrap_or("detached")
            .to_string(),
    )
}

pub(super) fn search_line_preview(text: &str, query: &str, limit: usize) -> String {
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

pub(super) fn single_line_preview(text: &str, limit: usize) -> String {
    let normalized = normalized_single_line(text);
    if normalized.width() <= limit {
        normalized
    } else if limit == 0 {
        String::new()
    } else if limit == 1 {
        "…".to_string()
    } else {
        let mut width = 0;
        let preview = normalized
            .graphemes(true)
            .take_while(|grapheme| {
                let grapheme_width = grapheme.width();
                if width + grapheme_width > limit - 1 {
                    false
                } else {
                    width += grapheme_width;
                    true
                }
            })
            .collect::<String>();
        preview + "…"
    }
}

pub(super) fn single_line_tail(text: &str, limit: usize) -> String {
    let text = text.replace(['\r', '\n'], " ");
    if text.width() <= limit {
        return text;
    }
    if limit == 0 {
        return String::new();
    }
    if limit == 1 {
        return "…".to_string();
    }
    let graphemes = text.graphemes(true).collect::<Vec<_>>();
    let mut width = 0;
    let start = graphemes
        .iter()
        .enumerate()
        .rev()
        .take_while(|(_, grapheme)| {
            let grapheme_width = grapheme.width();
            if width + grapheme_width > limit - 1 {
                false
            } else {
                width += grapheme_width;
                true
            }
        })
        .last()
        .map_or(graphemes.len(), |(index, _)| index);
    format!("…{}", graphemes[start..].concat())
}

pub(super) const MARQUEE_HOLD_FRAMES: usize = 8;
pub(super) const MARQUEE_STEP_FRAMES: usize = 2;

pub(super) fn marquee_preview(text: &str, limit: usize, elapsed_frames: usize) -> String {
    let normalized = normalized_single_line(text);
    if normalized.width() <= limit {
        return normalized;
    }
    if limit <= 1 {
        return single_line_preview(&normalized, limit);
    }
    let graphemes = normalized.graphemes(true).collect::<Vec<_>>();
    let mut suffix_width = 0;
    let mut max_start = graphemes.len().saturating_sub(1);
    for (index, grapheme) in graphemes.iter().enumerate().rev() {
        suffix_width += grapheme.width();
        if suffix_width > limit - 1 {
            break;
        }
        max_start = index;
    }
    let travel_frames = max_start.saturating_mul(MARQUEE_STEP_FRAMES);
    let cycle_frames = MARQUEE_HOLD_FRAMES
        .saturating_mul(2)
        .saturating_add(travel_frames.saturating_mul(2))
        .max(1);
    let phase = elapsed_frames % cycle_frames;
    let start = if phase < MARQUEE_HOLD_FRAMES {
        0
    } else if phase < MARQUEE_HOLD_FRAMES + travel_frames {
        (phase - MARQUEE_HOLD_FRAMES) / MARQUEE_STEP_FRAMES
    } else if phase < MARQUEE_HOLD_FRAMES * 2 + travel_frames {
        max_start
    } else {
        max_start
            .saturating_sub((phase - MARQUEE_HOLD_FRAMES * 2 - travel_frames) / MARQUEE_STEP_FRAMES)
    };
    marquee_window(&graphemes, start, limit)
}

pub(super) fn list_cell(text: &str, width: usize, selected: bool, elapsed_frames: usize) -> String {
    let content = if selected {
        marquee_preview(text, width, elapsed_frames)
    } else {
        single_line_preview(text, width)
    };
    let padding = width.saturating_sub(content.width());
    format!("{content}{}", " ".repeat(padding))
}

fn normalized_single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn marquee_window(graphemes: &[&str], start: usize, limit: usize) -> String {
    let left_hidden = start > 0;
    let left_width = usize::from(left_hidden);
    let suffix_width = graphemes[start..].concat().width();
    let right_hidden = suffix_width > limit.saturating_sub(left_width);
    let content_width = limit
        .saturating_sub(left_width)
        .saturating_sub(usize::from(right_hidden));
    let mut width = 0;
    let content = graphemes[start..]
        .iter()
        .take_while(|grapheme| {
            let grapheme_width = grapheme.width();
            if width + grapheme_width > content_width {
                false
            } else {
                width += grapheme_width;
                true
            }
        })
        .copied()
        .collect::<String>();
    format!(
        "{}{}{}",
        if left_hidden { "…" } else { "" },
        content,
        if right_hidden { "…" } else { "" }
    )
}
