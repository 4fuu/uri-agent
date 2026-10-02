use super::*;
use std::borrow::Cow;

// Render caches are an optimization, not transcript state. Keep a small
// neighborhood around the viewport so a long session does not retain a
// second rendered copy of every historical block.
const TRANSCRIPT_CACHE_CONTEXT_BLOCKS: usize = 64;

// An expanded tool row previews arguments and output with separate budgets,
// so a long result is never squeezed out by argument detail.
const TOOL_PREVIEW_ARGUMENT_LINES: usize = 8;
const TOOL_PREVIEW_OUTPUT_LINES: usize = 8;
// Continuation lines shown after a multi-line shell script's titled first
// line, and diff lines per side shown for a `replace` call.
const TOOL_PREVIEW_SCRIPT_LINES: usize = 3;
const REPLACE_PREVIEW_DIFF_LINES: usize = 4;

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
        document.push_str(&format!("\n**Target:** {}\n", inline_code(target.as_ref())));
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

fn tool_target(tool: &ToolDisplay) -> Option<Cow<'_, str>> {
    let steps = parse_protocol_steps(&tool.arguments);
    if steps.len() > 1 {
        // A multi-step call lists each step under its own heading instead.
        return None;
    }
    if let Some(parsed) = steps.first() {
        return Some(display_tool_uri(parsed.address));
    }
    if tool.name == "replace" {
        let path = tool.arguments.get("path")?.as_str()?;
        return Some(Cow::Owned(display_path(Path::new(path))));
    }
    None
}

fn display_tool_uri(uri: &str) -> Cow<'_, str> {
    let Some(target) = uri.strip_prefix("file://") else {
        return Cow::Borrowed(uri);
    };
    let query_search_start = if target.starts_with(r"\\?\") { 4 } else { 0 };
    let path_end = target[query_search_start..]
        .find('?')
        .map(|index| query_search_start + index)
        .unwrap_or(target.len());
    let path = &target[..path_end];
    let displayed = display_path(Path::new(path));
    if displayed == path {
        return Cow::Borrowed(uri);
    }
    Cow::Owned(format!("file://{displayed}{}", &target[path_end..]))
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
    let protocol_steps = parse_protocol_steps(&tool.arguments);
    let protocol_steps_consumed = !protocol_steps.is_empty();
    if protocol_steps.len() > 1 {
        let step_heading = "#".repeat(level + 1);
        document.push_str(&format!("\n{heading} Steps\n"));
        for (index, step) in protocol_steps.iter().enumerate() {
            let action = if step.operation == "exec" {
                "Exec"
            } else {
                "Read"
            };
            document.push_str(&format!(
                "\n{step_heading} Step {} · {} {}\n",
                index + 1,
                action,
                inline_code(&display_tool_uri(step.address))
            ));
            append_step_input(document, step, level + 2);
        }
    } else if let Some(step) = protocol_steps.first() {
        append_step_input(document, step, level);
    }

    let Some(arguments) = tool.arguments.as_object() else {
        return;
    };
    let remaining = arguments
        .iter()
        .filter(|(name, _)| !(protocol_steps_consumed && name.as_str() == "steps"))
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

/// Appends one protocol step's `input` as a Command or Input section;
/// steps without input render nothing.
fn append_step_input(document: &mut String, step: &ProtocolStep<'_>, level: usize) {
    let Some(input) = step.input else {
        return;
    };
    if input.as_object().is_some_and(serde_json::Map::is_empty) {
        return;
    }
    let heading = "#".repeat(level);
    let protocol = step.address.split("://").next().unwrap_or_default();
    if step.operation == "exec"
        && matches!(protocol, "bash" | "pwsh")
        && let Some(script) = input.get("script").and_then(serde_json::Value::as_str)
    {
        let language = if protocol == "pwsh" {
            "powershell"
        } else {
            "bash"
        };
        document.push_str(&format!("\n{heading} Command\n\n"));
        document.push_str(&fenced_block(script, language));
        if let Some(fields) = input.as_object() {
            let remaining = fields
                .iter()
                .filter(|(name, _)| name.as_str() != "script")
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect::<serde_json::Map<_, _>>();
            if !remaining.is_empty() {
                let value = serde_json::Value::Object(remaining);
                let rendered = serde_json::to_string_pretty(&redact_sensitive_arguments(&value))
                    .unwrap_or_else(|_| value.to_string());
                document.push_str(&format!("\n{heading} Input\n\n"));
                document.push_str(&fenced_block(&rendered, "json"));
            }
        }
        return;
    }
    let rendered = serde_json::to_string_pretty(&redact_sensitive_arguments(input))
        .unwrap_or_else(|_| input.to_string());
    document.push_str(&format!("\n{heading} Input\n\n"));
    document.push_str(&fenced_block(&rendered, "json"));
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

/// One parsed `protocol` step for display only: the `read`/`exec` verb, the
/// address, and the step's `input` object. Every step of a call is kept so
/// previews can show the whole call.
struct ProtocolStep<'a> {
    operation: &'a str,
    address: &'a str,
    input: Option<&'a serde_json::Value>,
}

fn parse_protocol_steps(arguments: &serde_json::Value) -> Vec<ProtocolStep<'_>> {
    arguments
        .get("steps")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|step| {
            let object = step.as_object()?;
            let (operation, address) = match (
                object.get("read").and_then(serde_json::Value::as_str),
                object.get("exec").and_then(serde_json::Value::as_str),
            ) {
                (Some(address), None) => ("read", address),
                (None, Some(address)) => ("exec", address),
                _ => return None,
            };
            Some(ProtocolStep {
                operation,
                address,
                input: object.get("input"),
            })
        })
        .collect()
}

pub(super) fn tool_protocol(arguments: &serde_json::Value) -> Option<String> {
    let address = parse_protocol_steps(arguments).first()?.address;
    let separator = address.find("://").or_else(|| address.find(':'))?;
    (separator > 0).then(|| address[..separator].to_string())
}

/// The `input` of a single-step protocol call, shown as detail lines.
fn tool_body(arguments: &serde_json::Value) -> Option<&serde_json::Value> {
    let steps = parse_protocol_steps(arguments);
    if steps.len() > 1 {
        return None;
    }
    steps
        .first()
        .and_then(|step| step.input)
        .filter(|input| !input.as_object().is_some_and(serde_json::Map::is_empty))
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
        return format!(
            "Edited {}",
            single_line_preview(&display_path(Path::new(path)), 72)
        );
    }
    if name == "help" {
        let names = arguments
            .get("protocols")
            .and_then(serde_json::Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|value| value.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        if names.is_empty() {
            return name.to_string();
        }
        return format!("Loaded help: {}", single_line_preview(&names, 64));
    }
    let steps = parse_protocol_steps(arguments);
    let Some(parsed) = steps.first() else {
        return name.to_string();
    };
    // A batch titles with its first command so an exec is never hidden
    // behind a read's "+N"; read-only batches keep their first read.
    let significant = steps
        .iter()
        .find(|step| step.operation == "exec")
        .unwrap_or(parsed);
    let title = step_title(significant);
    if steps.len() > 1 {
        return format!("{title} +{}", steps.len() - 1);
    }
    title
}

fn step_title(step: &ProtocolStep<'_>) -> String {
    let action = if step.operation == "exec" {
        "Ran"
    } else {
        "Read"
    };
    protocol_step_title(action, step.address, step_script(step))
}

fn step_script<'a>(step: &ProtocolStep<'a>) -> &'a str {
    step.input
        .and_then(|input| input.get("script"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
}

fn step_protocol<'a>(step: &ProtocolStep<'a>) -> &'a str {
    step.address.split("://").next().unwrap_or_default()
}

fn step_is_shell_exec(step: &ProtocolStep<'_>) -> bool {
    step.operation == "exec" && matches!(step_protocol(step), "bash" | "pwsh")
}

/// True when a step's title already displays its address, so a `↳` detail
/// line would only repeat it. A shell command title shows the script's
/// first line instead of the address.
fn step_title_shows_address(step: &ProtocolStep<'_>) -> bool {
    !(step_is_shell_exec(step) && !step_script(step).is_empty())
}

fn protocol_step_title(action: &str, address: &str, script: &str) -> String {
    let address = display_tool_uri(address);
    let address = address.as_ref();
    let (protocol, target) = address.split_once("://").unwrap_or((address, ""));
    if action == "Ran" && matches!(protocol, "bash" | "pwsh") && !script.is_empty() {
        return format!(
            "$ {}",
            single_line_preview(script.lines().next().unwrap_or_default(), 76)
        );
    }
    if action == "Read" && protocol == "file" {
        return format!("Read {}", single_line_preview(target, 76));
    }
    if target == "help" {
        return format!("Read {protocol} help");
    }
    format!("{action} {}", single_line_preview(address, 76))
}

pub(super) fn patch_targets(patch: &str) -> Vec<String> {
    let mut targets = Vec::new();
    for line in patch.lines() {
        let path = ["*** Add File: ", "*** Update File: ", "*** Delete File: "]
            .iter()
            .find_map(|prefix| line.strip_prefix(prefix));
        if let Some(path) = path {
            let path = display_path(Path::new(path));
            if !targets.contains(&path) {
                targets.push(path);
            }
        }
    }
    targets
}

/// One step's outcome parsed from a batch result envelope.
#[derive(Clone, Copy)]
enum StepOutcome {
    Ok,
    Error,
    Skipped,
}

impl StepOutcome {
    fn parse(status: &str) -> Option<Self> {
        match status.trim() {
            "ok" => Some(Self::Ok),
            "error" => Some(Self::Error),
            "skipped" => Some(Self::Skipped),
            _ => None,
        }
    }

    fn marker(self) -> &'static str {
        match self {
            Self::Ok => "✓",
            Self::Error => "×",
            Self::Skipped => "·",
        }
    }

    fn color(self) -> Color {
        match self {
            Self::Error => ERROR,
            Self::Ok | Self::Skipped => MUTED,
        }
    }

    /// The worse outcome wins, so a `for` step with one failed element
    /// reports the step as failed.
    fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Error, _) | (_, Self::Error) => Self::Error,
            (Self::Skipped, _) | (_, Self::Skipped) => Self::Skipped,
            (Self::Ok, _) => Self::Ok,
        }
    }
}

/// A batch result's `*** Result <n> of <m>: <status>` envelope line. The
/// sections themselves belong to the full document; the preview only keeps
/// the statuses.
fn is_result_envelope_line(line: &str) -> bool {
    line.starts_with("*** Result ")
}

/// Per-step outcomes of a batch call, parsed from its result envelope. Loop
/// sections share a step number (`<n>.<k>`); the worst outcome wins.
fn tool_step_statuses(output: &Option<String>, step_count: usize) -> Vec<Option<StepOutcome>> {
    let mut statuses = vec![None; step_count];
    let Some(output) = output else {
        return statuses;
    };
    for line in output.lines() {
        let Some((label, status)) = line
            .strip_prefix("*** Result ")
            .and_then(|rest| rest.split_once(": "))
        else {
            continue;
        };
        let Some((label, _)) = label.split_once(" of ") else {
            continue;
        };
        let Some(outcome) = StepOutcome::parse(status) else {
            continue;
        };
        let Some(index) = label
            .split('.')
            .next()
            .and_then(|number| number.parse::<usize>().ok())
            .filter(|index| (1..=step_count).contains(index))
        else {
            continue;
        };
        statuses[index - 1] = Some(match statuses[index - 1] {
            Some(current) => current.merge(outcome),
            None => outcome,
        });
    }
    statuses
}

pub(super) fn tool_detail_lines(
    block: &DisplayBlock,
    width: usize,
    argument_limit: usize,
    output_limit: usize,
) -> (Vec<(String, Color)>, usize) {
    let mut arguments = Vec::new();
    let mut output = Vec::new();
    if let Some(tool) = &block.tool {
        let steps = parse_protocol_steps(&tool.arguments);
        if steps.len() > 1 {
            let statuses = tool_step_statuses(&tool.output, steps.len());
            for (index, step) in steps.iter().enumerate() {
                let (marker, color) = match statuses.get(index).copied().flatten() {
                    Some(outcome) => (outcome.marker(), outcome.color()),
                    None => ("·", if block.failed { ERROR } else { MUTED }),
                };
                arguments.push((format!("{marker} {}", step_title(step)), color));
            }
        } else if let Some(step) = steps.first().filter(|step| !step_title_shows_address(step)) {
            arguments.push((format!("↳ {}", display_tool_uri(step.address)), MUTED));
        } else if steps.is_empty() && block.title != tool.name {
            arguments.push((format!("↳ {}", tool.name), MUTED));
        }
        tool_argument_details(&tool.arguments, &mut arguments);
        if let Some(tool_output) = &tool.output {
            push_output_preview(tool_output, block.failed, &mut output);
        }
    } else if let Some((_, result)) = block
        .text
        .split_once("\n\nRESULT\n")
        .or_else(|| block.text.split_once("\n\nERROR\n"))
    {
        push_output_preview(result, block.failed, &mut output);
    }

    let mut wrapped = Vec::new();
    let mut extra = wrap_preview_lines(&arguments, width, argument_limit, &mut wrapped);
    extra += wrap_preview_lines(&output, width, output_limit, &mut wrapped);
    if wrapped.is_empty() {
        wrapped.push(("Waiting for result…".to_string(), MUTED));
    }
    (wrapped, extra)
}

fn push_output_preview(output: &str, failed: bool, lines: &mut Vec<(String, Color)>) {
    for (index, line) in output
        .lines()
        .filter(|line| !is_result_envelope_line(line))
        .enumerate()
    {
        lines.push((
            format!("{} {line}", if index == 0 { "└" } else { " " }),
            if failed { ERROR } else { MUTED },
        ));
    }
}

fn wrap_preview_lines(
    logical: &[(String, Color)],
    width: usize,
    limit: usize,
    wrapped: &mut Vec<(String, Color)>,
) -> usize {
    let mut section = Vec::new();
    for (line, color) in logical {
        let lines = wrapped_block_lines(line, width.max(1));
        section.extend(lines.into_iter().map(|line| (line, *color)));
    }
    let extra = section.len().saturating_sub(limit);
    wrapped.extend(section.into_iter().take(limit));
    extra
}

pub(super) fn tool_argument_details(
    arguments: &serde_json::Value,
    lines: &mut Vec<(String, Color)>,
) {
    let steps = parse_protocol_steps(arguments);
    let steps_consumed = !steps.is_empty();
    if let Some(fields) = arguments.as_object() {
        for (key, value) in fields {
            if steps_consumed && key == "steps" {
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
            if replace_edits(arguments).is_some()
                && matches!(key.as_str(), "path" | "old_text" | "new_text")
            {
                // The title names the path; the edited pair is a diff below.
                continue;
            }
            lines.push((format!("  {key}: {}", argument_summary(key, value)), MUTED));
        }
    }
    if let Some((old_text, new_text)) = replace_edits(arguments) {
        lines.extend(replace_diff_lines(old_text, new_text));
        return;
    }
    let Some(body) = tool_body(arguments) else {
        return;
    };
    if let Some(fields) = body.as_object() {
        // A shell script's first line is already the row title; continue it
        // as body text instead of flattening the whole script.
        let titled_script = steps
            .first()
            .is_some_and(|step| step_is_shell_exec(step) && !step_script(step).is_empty());
        if titled_script
            && let Some(script) = fields.get("script").and_then(serde_json::Value::as_str)
        {
            lines.extend(
                script
                    .lines()
                    .skip(1)
                    .take(TOOL_PREVIEW_SCRIPT_LINES)
                    .map(|line| (format!("  {line}"), MUTED)),
            );
        }
        for (key, value) in fields {
            if titled_script && key == "script" {
                continue;
            }
            lines.push((format!("  {key}: {}", argument_summary(key, value)), MUTED));
        }
    }
}

/// The edited text pair of a `replace` call, if these are its arguments.
fn replace_edits(arguments: &serde_json::Value) -> Option<(&str, Option<&str>)> {
    let old_text = arguments.get("old_text")?.as_str()?;
    let new_text = arguments
        .get("new_text")
        .and_then(serde_json::Value::as_str);
    Some((old_text, new_text))
}

fn replace_diff_lines(old_text: &str, new_text: Option<&str>) -> Vec<(String, Color)> {
    let mut lines = Vec::new();
    for (marker, color, text) in [
        ("-", ERROR, old_text),
        ("+", ACCENT, new_text.unwrap_or_default()),
    ] {
        lines.extend(
            text.lines()
                .take(REPLACE_PREVIEW_DIFF_LINES)
                .map(|line| (format!("  {marker} {line}"), color)),
        );
    }
    lines
}

/// The settled process row's fallback label when a turn had no tool steps.
fn process_step_label(steps: usize) -> String {
    format!(
        "Process · {steps} step{}",
        if steps == 1 { "" } else { "s" }
    )
}

fn plural_suffix(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// One activity line of a live process card's tail.
struct ProcessActivityRow {
    marker: String,
    text: String,
    color: Color,
}

/// Summarizes a turn's tool activity: every protocol step counts once and
/// classifies by what it did. The failed-call count is returned apart so the
/// heading can keep it visible when the activity text must be shortened.
/// Reasoning-only turns have no summary and fall back to the step-count label.
fn process_summary(children: &[&DisplayBlock]) -> Option<(String, usize)> {
    let mut commands = 0;
    let mut file_reads = 0;
    let mut edits = 0;
    let mut searches = 0;
    let mut others = 0;
    let mut failures = 0;
    for child in children {
        let Some(tool) = &child.tool else {
            continue;
        };
        let steps = parse_protocol_steps(&tool.arguments);
        if steps.is_empty() {
            match tool.name.as_str() {
                "replace" => edits += 1,
                "apply_patch" => {
                    edits += tool
                        .arguments
                        .get("patch")
                        .and_then(serde_json::Value::as_str)
                        .map(|patch| patch_targets(patch).len())
                        .filter(|files| *files > 0)
                        .unwrap_or(1);
                }
                _ => others += 1,
            }
        } else {
            for step in &steps {
                let protocol = step_protocol(step);
                if step_is_shell_exec(step) {
                    commands += 1;
                } else if step.operation == "read" && protocol == "file" {
                    file_reads += 1;
                } else if matches!(protocol, "search" | "finder") {
                    searches += 1;
                } else {
                    others += 1;
                }
            }
        }
        failures += usize::from(child.failed);
    }
    let mut parts = Vec::new();
    if commands > 0 {
        parts.push(format!("Ran {commands} command{}", plural_suffix(commands)));
    }
    if file_reads > 0 {
        parts.push(format!(
            "read {file_reads} file{}",
            plural_suffix(file_reads)
        ));
    }
    if edits > 0 {
        parts.push(format!("edited {edits} file{}", plural_suffix(edits)));
    }
    if searches > 0 {
        parts.push(format!(
            "searched {searches} time{}",
            plural_suffix(searches)
        ));
    }
    if others > 0 {
        parts.push(format!("{others} other"));
    }
    let mut summary = parts.join(" · ");
    if summary.is_empty() {
        return None;
    }
    if let Some(first) = summary.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    Some((summary, failures))
}

/// One row per step of the turn's activity, newest last: reasoning blocks
/// and direct tools get a row each, protocol calls get one row per step
/// once their results carry per-step statuses.
/// Marks a live card row or tool call that is still in progress.
const IN_PROGRESS_MARKER: &str = "›";

/// Intermediate text as at most `limit` wrapped rows under a `❝ ` lead,
/// blank lines dropped; a cut ends its last row with `…`.
fn narration_lines(text: &str, width: usize, limit: usize) -> Vec<String> {
    let body_width = width.saturating_sub(2).max(1);
    let mut wrapped = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .flat_map(|line| wrapped_block_lines(line.trim(), body_width))
        .collect::<Vec<_>>();
    if wrapped.len() > limit {
        wrapped.truncate(limit);
        if let Some(last) = wrapped.last_mut() {
            let kept = single_line_preview(last, body_width.saturating_sub(1).max(1));
            *last = format!("{}…", kept.trim_end_matches('…').trim_end());
        }
    }
    wrapped
        .into_iter()
        .enumerate()
        .map(|(index, line)| format!("{}{line}", if index == 0 { "❝ " } else { "  " }))
        .collect()
}

/// The footer owns the turn's only spinner; a row still in progress is
/// marked with a static `›` instead. Assistant text is the card's
/// narration, not an activity row.
fn process_activity_rows(children: &[&DisplayBlock]) -> Vec<ProcessActivityRow> {
    let mut rows = Vec::new();
    for child in children {
        match child.kind {
            BlockKind::Assistant => {}
            BlockKind::Reasoning => {
                if child.transient {
                    rows.push(ProcessActivityRow {
                        marker: IN_PROGRESS_MARKER.to_string(),
                        text: "Thinking…".to_string(),
                        color: ACCENT,
                    });
                } else {
                    rows.push(ProcessActivityRow {
                        marker: "◇".to_string(),
                        text: "Thought".to_string(),
                        color: MUTED,
                    });
                }
            }
            BlockKind::Tool => {
                let Some(tool) = &child.tool else {
                    continue;
                };
                let steps = parse_protocol_steps(&tool.arguments);
                if steps.len() > 1 && tool.output.is_some() {
                    let statuses = tool_step_statuses(&tool.output, steps.len());
                    for (index, step) in steps.iter().enumerate() {
                        let outcome = statuses.get(index).copied().flatten();
                        let (marker, color) = match outcome {
                            Some(outcome) => (outcome.marker().to_string(), outcome.color()),
                            None if child.failed => ("×".to_string(), ERROR),
                            None => ("·".to_string(), MUTED),
                        };
                        rows.push(ProcessActivityRow {
                            marker,
                            text: step_title(step),
                            color,
                        });
                    }
                } else {
                    let (marker, color) = if tool.output.is_none() {
                        (IN_PROGRESS_MARKER.to_string(), ACCENT)
                    } else if child.failed {
                        ("×".to_string(), ERROR)
                    } else {
                        ("✓".to_string(), MUTED)
                    };
                    rows.push(ProcessActivityRow {
                        marker,
                        text: child.title.clone(),
                        color,
                    });
                }
            }
            _ => rows.push(ProcessActivityRow {
                marker: "·".to_string(),
                text: child.title.clone(),
                color: MUTED,
            }),
        }
    }
    rows
}

fn argument_summary(name: &str, value: &serde_json::Value) -> String {
    if sensitive_argument_name(name) {
        "[redacted]".to_string()
    } else if name == "path"
        && let Some(path) = value.as_str()
    {
        single_line_preview(&display_path(Path::new(path)), 72)
    } else if name == "protocols"
        && let Some(list) = value.as_array()
    {
        let names = list
            .iter()
            .filter_map(|item| item.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        single_line_preview(&names, 72)
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
    app.resolve_layout(area.width);
    frame.render_widget(Block::new().style(Style::default().bg(BG)), area);
    if app.showing_splash() {
        app.selectable = None;
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
    let mut constraints = match (idle, has_notices) {
        (true, false) => vec![Constraint::Min(3)],
        (true, true) => vec![Constraint::Min(3), Constraint::Length(notice_height)],
        (false, false) => vec![Constraint::Min(3), Constraint::Length(footer_height)],
        (false, true) => vec![
            Constraint::Min(3),
            Constraint::Length(notice_height),
            Constraint::Length(footer_height),
        ],
    };
    if app.compact {
        constraints.push(Constraint::Length(ACTION_BAR_HEIGHT));
    }
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);
    let action_bar_area = app.compact.then(|| areas[areas.len() - 1]);
    if let Some(action_bar_area) = action_bar_area {
        render_action_bar(frame, app, action_bar_area);
    }
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
            .or(action_bar_area)
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
    // The compact layout has no scrollbar: on a phone its column sits under
    // the client's own text selection and swipes already scroll.
    app.transcript_scrollbar_area = if app.overlay.is_none() && !idle && !app.compact {
        transcript_scrollbar_area(app, content)
    } else {
        None
    };
    let selectable_area = if let Some(overlay) = app.overlay {
        app.hit_regions.clear();
        let area = overlay_area(frame.area(), app, overlay);
        app.overlay_bounds = Some(area);
        render_overlay(frame, app, overlay);
        Some(if app.compact {
            // Top border and one padding row; one padding column per side.
            Rect::new(
                area.x.saturating_add(1),
                area.y.saturating_add(2),
                area.width.saturating_sub(2),
                area.height.saturating_sub(2),
            )
        } else {
            area.inner(Margin {
                horizontal: 2,
                vertical: 2,
            })
        })
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
        let left_padding = usize::from(row_separators.is_some() && !app.compact);
        capture_surface(frame, app, selectable_area, row_separators, left_padding);
        render_selection(frame, app);
    } else {
        app.selectable = None;
    }
    if app.overlay.is_none() && !idle {
        render_transcript_scrollbar(frame, app);
    }
    if app.overlay.is_none()
        && !app.compact
        && let Some(footer_area) = footer_area.filter(|area| area.height == 1)
    {
        render_floating_tail_button(frame, app, footer_area);
    }
}

pub(super) const ACTION_BAR_HEIGHT: u16 = 2;

/// Compact-layout touch bar: two-row buttons, icon over label, sharing the
/// width evenly so each stays comfortably wide on a phone.
pub(super) fn render_action_bar(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    if area.is_empty() {
        return;
    }
    let last = if app.busy {
        (ActionButton::Stop, "■", "stop")
    } else {
        (ActionButton::Status, "≡", "status")
    };
    let buttons = [
        (ActionButton::Compose, "✎", "write"),
        (ActionButton::Command, ":", "commands"),
        (ActionButton::Latest, "↓", "latest"),
        last,
    ];
    frame.render_widget(Block::new().style(Style::default().bg(SURFACE)), area);
    let count = buttons.len() as u16;
    let gaps = count.saturating_sub(1);
    let width = area.width.saturating_sub(gaps) / count;
    if width == 0 {
        return;
    }
    let mut x = area.x;
    for (index, (button, icon, label)) in buttons.into_iter().enumerate() {
        // The last button absorbs the division remainder.
        let button_width = if index + 1 == buttons.len() {
            area.right().saturating_sub(x)
        } else {
            width
        };
        let button_area = Rect::new(x, area.y, button_width, area.height);
        let color = if button == ActionButton::Stop {
            ERROR
        } else {
            ACCENT
        };
        let label = single_line_preview(label, button_width as usize);
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(
                    icon,
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Line::styled(label, Style::default().fg(TEXT)),
            ])
            .alignment(Alignment::Center)
            .style(Style::default().bg(ROW_ACTIVE)),
            button_area,
        );
        app.hit_regions.push(HitRegion {
            area: button_area,
            target: AppHit::ActionBar(button),
        });
        x = x.saturating_add(button_width).saturating_add(1);
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
    let width = brand_area.width as usize;
    let progress = (app.started.elapsed().as_secs_f32() / SPLASH_DURATION.as_secs_f32()) * 1.25;
    let mut lines = if splash && progress < 1.0 {
        animation::wordmark_reveal(app.animation_phase, progress, width)
    } else {
        animation::wordmark(app.animation_phase, width)
    }
    .into_iter()
    .map(|line| {
        // Centering floors, leaving an odd spare column on the right, and the
        // mark already reads left-heavy: its left edge is the full-height U
        // while the I reaches its right edge only on the top and bottom rows.
        // Give the spare column to the left instead.
        let line = if (width.saturating_sub(line.width())) % 2 == 1 {
            format!(" {line}")
        } else {
            line
        };
        Line::styled(line, Style::default().fg(ACCENT))
    })
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
        let full = format!(
            "{} / {} · effort {}",
            app.info.provider, app.info.model, app.info.thinking
        );
        // Split before truncating so a narrow window keeps the effort.
        let rows = if full.width() < width {
            vec![full]
        } else {
            vec![
                format!("{} / {}", app.info.provider, app.info.model),
                format!("effort {}", app.info.thinking),
            ]
        };
        rows.into_iter()
            .map(|row| {
                Line::styled(
                    single_line_preview(&row, width.saturating_sub(1)),
                    Style::default().fg(TEXT),
                )
            })
            .collect()
    } else {
        vec![Line::styled(
            "No model configured. Run :login",
            Style::default().fg(WARM),
        )]
    };
    let hints = action_hints(
        &app.keymap,
        &[
            ("main", "compose", "compose"),
            ("main", "command", "commands"),
            ("main", "help", "help"),
        ],
    );
    let mut lines = vec![Line::styled(
        single_line_preview(&footer_cwd(&app.info.cwd), width.saturating_sub(1)),
        Style::default().fg(MUTED),
    )];
    lines.extend(model);
    lines.push(Line::default());
    if let Some(hints) = fitted_hints(&hints, width.saturating_sub(1)) {
        lines.push(Line::styled(hints, Style::default().fg(MUTED)));
    }
    if let Some(version) = &app.available_update {
        lines.push(Line::styled(
            single_line_preview(
                &format!("Update available: {version} · github.com/4fuu/uri-agent/releases/latest"),
                width.saturating_sub(1),
            ),
            Style::default().fg(WARM),
        ));
    }
    lines
}

/// Welcome hints degrade like the footer's task indicator: keep the whole line
/// while it fits the brand box, drop trailing hints as the box narrows, and
/// omit the line instead of truncating a hint mid-label.
fn fitted_hints(hints: &str, width: usize) -> Option<String> {
    const SEPARATOR: &str = " · ";
    let items = hints.split(SEPARATOR).collect::<Vec<_>>();
    (1..=items.len())
        .rev()
        .map(|kept| items[..kept].join(SEPARATOR))
        .find(|line| !line.is_empty() && line.width() <= width)
}

/// The footer's task badge: running count plus the newest running task's
/// label and elapsed time. It degrades from labeled to count-only to compact
/// so both layouts keep the model and context columns intact.
pub(super) fn footer_task_badge(
    task: Option<&FooterTask>,
    count: usize,
    context_width: usize,
    available: usize,
) -> Option<String> {
    if count == 0 {
        return None;
    }
    let noun = if count == 1 { "task" } else { "tasks" };
    let count_full = format!("● {count} {noun}");
    let count_compact = format!("●{count}");
    let detail = task.map(|task| {
        let elapsed = format_elapsed(
            chrono::Utc::now()
                .signed_duration_since(task.started_at)
                .to_std()
                .unwrap_or_default(),
        );
        format!("{} {elapsed}", single_line_preview(&task.label, 20))
    });
    let minimum_model_width = 12;
    let fits_full = |text: &str| {
        available
            >= context_width
                .saturating_add(text.width())
                .saturating_add(minimum_model_width)
                .saturating_add(4)
    };
    let fits_compact =
        |text: &str| available >= context_width.saturating_add(text.width()).saturating_add(3);
    let labeled_full = detail
        .as_ref()
        .map(|detail| format!("{count_full} · {detail}"));
    let labeled_compact = detail.map(|detail| format!("{count_compact} {detail}"));
    if let Some(full) = labeled_full.as_ref()
        && fits_full(full)
    {
        return Some(full.clone());
    }
    if fits_full(&count_full) {
        return Some(count_full);
    }
    if let Some(compact) = labeled_compact.as_ref()
        && fits_compact(compact)
    {
        return Some(compact.clone());
    }
    fits_compact(&count_compact).then_some(count_compact)
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
    let progress_phase = if app.busy { app.animation_phase } else { 0.0 };
    let progress = if app.info.context_accuracy == ContextAccuracy::Unknown {
        animation::progress(progress_phase, 8, 0.0)
    } else {
        animation::progress(progress_phase, 8, percent / 100.0)
    };
    // Compact footers keep only the percentage so the model name survives.
    let context = if app.compact {
        single_line_preview(&usage, available)
    } else {
        single_line_preview(
            &format!(
                "{progress} {usage}/{}",
                format_tokens(app.info.context_window as u64),
            ),
            available,
        )
    };
    let context_width = context.width();
    let task_count = app.active_task_count;
    let task = footer_task_badge(
        app.footer_task.as_ref(),
        task_count,
        context_width,
        available,
    );
    let task_width = task.as_deref().map_or(0, UnicodeWidthStr::width);
    let task_context_gap = usize::from(task.is_some()) * 2;
    let model_limit = available.saturating_sub(
        context_width
            .saturating_add(task_width)
            .saturating_add(task_context_gap)
            .saturating_add(2),
    );
    let model = single_line_preview(
        &if app.compact {
            short_model(app)
        } else {
            compact_model(app)
        },
        model_limit,
    );
    let model_width = model.width();
    let gap = available.saturating_sub(
        model_width
            .saturating_add(task_width)
            .saturating_add(task_context_gap)
            .saturating_add(context_width),
    );
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
    if let Some(task) = task {
        let task_x = area
            .x
            .saturating_add(u16::try_from(model_width.saturating_add(gap)).unwrap_or(u16::MAX));
        base.push(Span::styled(
            task,
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
        base.push(Span::raw(" ".repeat(task_context_gap)));
        app.hit_regions.push(HitRegion {
            area: Rect::new(
                task_x,
                area.bottom().saturating_sub(1),
                u16::try_from(task_width).unwrap_or(u16::MAX),
                1,
            ),
            target: AppHit::TaskStatus,
        });
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
        .map(|since| format!(" {}", format_elapsed(since.elapsed())))
        .unwrap_or_default();
    let token_rate = app
        .token_rate
        .display_rate(Instant::now())
        .map(|rate| format!(" · {}", format_token_rate(rate)))
        .unwrap_or_default();
    Some(format!(
        "{} {activity}{elapsed}  {}{token_rate}",
        animation::spinner(app.animation_phase),
        animation::activity(app.animation_phase, 8)
    ))
}

pub(super) fn format_elapsed(duration: Duration) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    let seconds = duration.as_secs();
    if seconds < MINUTE {
        format!("{:.1}s", duration.as_secs_f32())
    } else if seconds < HOUR {
        format!("{}m {}s", seconds / MINUTE, seconds % MINUTE)
    } else if seconds < DAY {
        format!("{}h {}m", seconds / HOUR, seconds % HOUR / MINUTE)
    } else {
        format!("{}d {}h", seconds / DAY, seconds % DAY / HOUR)
    }
}

/// Glyph and colour for one task status. Running borrows the live spinner
/// frame so the row animates with the rest of the interface.
pub(super) fn task_status_glyph(status: TaskStatus, animation_phase: f64) -> (char, Color) {
    match status {
        TaskStatus::Pending => ('…', WARM),
        TaskStatus::Running => (animation::spinner(animation_phase), ACCENT),
        TaskStatus::Completed => ('✓', ACCENT),
        TaskStatus::Failed => ('×', ERROR),
        TaskStatus::Cancelled => ('⊘', MUTED),
    }
}

/// Live elapsed time while a task runs, total duration once it settled.
pub(super) fn task_elapsed_text(record: &TaskRecord, now: chrono::DateTime<chrono::Utc>) -> String {
    let end = record.finished_at.unwrap_or(now);
    format_elapsed(
        end.signed_duration_since(record.started_at)
            .to_std()
            .unwrap_or_default(),
    )
}

pub(super) fn format_task_time(time: chrono::DateTime<chrono::Utc>) -> String {
    time.with_timezone(&chrono::Local)
        .format("%H:%M:%S")
        .to_string()
}

/// Bounded, control-safe lines for on-screen output tails. Terminal escapes
/// and control characters never reach the cell buffer, and only the last
/// `max_lines` lines survive; the count of dropped lines is returned so the
/// caller can mark the cut.
pub(super) fn sanitize_output_tail(output: &[u8], max_lines: usize) -> (usize, Vec<String>) {
    let text = String::from_utf8_lossy(output);
    let mut lines = text
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .split('\n')
        .map(sanitize_output_line)
        .collect::<Vec<_>>();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    let dropped = lines.len().saturating_sub(max_lines);
    (dropped, lines.split_off(dropped))
}

/// Strips ANSI escape sequences and replaces remaining control characters so
/// process output cannot smuggle terminal control into the interface.
fn sanitize_output_line(line: &str) -> String {
    let mut clean = String::with_capacity(line.len());
    let mut characters = line.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\x1b' => match characters.peek().copied() {
                // CSI: ESC [ parameters-and-intermediates final-byte.
                Some('[') => {
                    characters.next();
                    for next in characters.by_ref() {
                        if ('\x40'..='\x7e').contains(&next) {
                            break;
                        }
                    }
                }
                // OSC: ESC ] ... terminated by BEL or ST.
                Some(']') => {
                    characters.next();
                    while let Some(next) = characters.next() {
                        if next == '\x07' {
                            break;
                        }
                        if next == '\x1b' && characters.peek() == Some(&'\\') {
                            characters.next();
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\t' => clean.push_str("    "),
            control if control.is_control() => clean.push('·'),
            other => clean.push(other),
        }
    }
    clean
}

/// Complete-output document for the shared full-document viewer.
pub(super) fn task_document(record: &TaskRecord) -> (String, String) {
    let title = format!(
        "Task {} — {}",
        record.id,
        single_line_preview(&record.label, 40)
    );
    let status = match record.status {
        TaskStatus::Pending | TaskStatus::Running => "• Running",
        TaskStatus::Completed => "✓ Completed",
        TaskStatus::Failed => "× Failed",
        TaskStatus::Cancelled => "⊘ Cancelled",
    };
    let mut times = format!("started {}", format_task_time(record.started_at));
    if let Some(finished) = record.finished_at {
        times.push_str(&format!(" · finished {}", format_task_time(finished)));
    }
    times.push_str(&format!(
        " · elapsed {}",
        task_elapsed_text(record, chrono::Utc::now())
    ));
    let protocol = record.protocol.as_str();
    let mut body = format!("**{status}** · `{protocol}://`\n\n{times}\n\n## Output\n\n");
    let output = String::from_utf8_lossy(&record.content);
    if output.is_empty() {
        body.push_str("_(no output yet)_\n");
    } else {
        body.push_str(&fenced_block(&output, "text"));
    }
    (title, body)
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

/// Narrow footer label: the model without provider or a trailing
/// `-YYYYMMDD` snapshot date, then the effort.
pub(super) fn short_model(app: &App) -> String {
    if !app.info.model_ready || app.info.model.is_empty() {
        return "no-model".to_string();
    }
    let model = app.info.model.as_str();
    let model = model
        .rsplit_once('-')
        .filter(|(_, date)| date.len() == 8 && date.bytes().all(|byte| byte.is_ascii_digit()))
        .map_or(model, |(name, _)| name);
    format!("{model} · {}", app.info.thinking)
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
        match app.info.context_strategy {
            crate::compaction::Strategy::Rollover => "automatic rollover",
            crate::compaction::Strategy::Summary => "automatic summary",
        }
    } else {
        "automatic context checkpoints disabled"
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
    // The right padding column doubles as the scrollbar track. The compact
    // layout has no scrollbar and spends both padding columns on text.
    let padding = u16::from(!app.compact);
    let message_width = area.width.saturating_sub(padding * 2).max(1) as usize;
    let process_width = message_width.saturating_sub(2).max(1);
    app.transcript_body_width = process_width;
    let active_block = app.active_transcript_block();
    rebuild_transcript_layout(app, message_width, process_width, active_block);
    app.transcript_rows = app.transcript_layout.rows;
    app.transcript_height = area.height as usize;
    let live_tail = transcript_live_tail(app.transcript_rows, app.transcript_height);
    let reading_end = transcript_reading_end(app.transcript_rows, app.transcript_height);
    if app.transcript_follow_tail {
        app.transcript_offset = live_tail;
    } else if app.transcript_center_selected
        && let Some(first) = app
            .transcript_layout
            .blocks
            .iter()
            .find(|block| block.index == app.selected_block)
            .map(|block| block.block_start)
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
    trim_transcript_render_caches(app);
    let offset = app.transcript_offset;
    let (visible, mut visible_row_separators, block_for_row, user_surface_for_row) =
        materialize_transcript_viewport(
            app,
            offset,
            app.transcript_height,
            message_width,
            process_width,
            active_block,
        );
    visible_row_separators.resize(app.transcript_height, TextRowSeparator::Newline);
    frame.render_widget(
        List::new(visible).block(Block::new().padding(Padding::horizontal(padding))),
        area,
    );
    // Ratatui resets the hidden cells behind wide glyphs even though the terminal paints their
    // background. Restore the complete user row so later frame diffs can clear exposed tail cells.
    let content_area = area.inner(Margin {
        horizontal: padding,
        vertical: 0,
    });
    for (row, user_surface) in user_surface_for_row.into_iter().enumerate() {
        if user_surface {
            frame.buffer_mut().set_style(
                Rect::new(content_area.x, area.y + row as u16, content_area.width, 1),
                Style::default().bg(USER_SURFACE),
            );
        }
    }
    for (row, index) in block_for_row.into_iter().enumerate() {
        let y = area.y.saturating_add(row as u16);
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

fn trim_transcript_render_caches(app: &mut App) {
    if app.transcript_layout.blocks.is_empty() || app.transcript_height == 0 {
        return;
    }
    let offset = app.transcript_offset;
    let end = offset.saturating_add(app.transcript_height);
    let entries = &app.transcript_layout.blocks;
    let first = entries.partition_point(|entry| {
        entry.block_start + entry.block_rows + usize::from(entry.user_padding) <= offset
    });
    let last = entries
        .partition_point(|entry| entry.start < end)
        .max(first);
    let keep_start = entries
        .get(first.saturating_sub(TRANSCRIPT_CACHE_CONTEXT_BLOCKS))
        .map_or(0, |entry| entry.index);
    let keep_end = entries
        .get(
            (last + TRANSCRIPT_CACHE_CONTEXT_BLOCKS)
                .min(entries.len())
                .saturating_sub(1),
        )
        .map_or(app.blocks.len(), |entry| entry.index + 1);
    for (index, block) in app.blocks.iter_mut().enumerate() {
        if index < keep_start || index >= keep_end {
            *block.render_cache.borrow_mut() = None;
        }
    }
}

pub(super) fn rebuild_transcript_layout(
    app: &mut App,
    message_width: usize,
    process_width: usize,
    active_block: Option<usize>,
) {
    #[cfg(test)]
    {
        app.transcript_render_stats = TranscriptRenderStats::default();
    }
    if app.transcript_layout.message_width != message_width
        || app.transcript_layout.process_width != process_width
    {
        app.transcript_layout.message_width = message_width;
        app.transcript_layout.process_width = process_width;
        app.transcript_layout.blocks.clear();
        app.transcript_layout.rows = 0;
        app.transcript_layout.dirty_from = Some(0);
    }
    let Some(dirty_from) = app.transcript_layout.dirty_from.take() else {
        return;
    };

    let keep = app
        .transcript_layout
        .blocks
        .partition_point(|block| block.index < dirty_from);
    app.transcript_layout.blocks.truncate(keep);
    let mut row = app.transcript_layout.blocks.last().map_or(0, |block| {
        block.block_start + block.block_rows + usize::from(block.user_padding)
    });
    let mut previous_visible = app.transcript_layout.blocks.last().map(|entry| {
        let block = &app.blocks[entry.index];
        (block.kind, block.turn_result)
    });
    let collapsed_processes = app.collapsed_processes();
    for index in dirty_from..app.blocks.len() {
        let block = &app.blocks[index];
        if block
            .parent_process
            .is_some_and(|process| collapsed_processes.contains(&process))
        {
            continue;
        }
        let start = row;
        if previous_visible.is_some_and(|(previous, previous_turn_result)| {
            transcript_needs_gap(
                previous,
                previous_turn_result,
                block.kind,
                block.turn_result,
            )
        }) {
            row += 1;
        }
        let user_padding = block.kind == BlockKind::User;
        row += usize::from(user_padding);
        let block_start = row;
        let (block_rows, rendered) = cached_transcript_block_row_count(
            block,
            index == app.selected_block,
            Some(index) == active_block,
            message_width,
            process_width,
            app,
        );
        #[cfg(test)]
        if rendered {
            app.transcript_render_stats.rendered_blocks += 1;
        }
        #[cfg(not(test))]
        let _ = rendered;
        row += block_rows + usize::from(user_padding);
        // A full rebuild computes row counts for every block. Release older
        // render rows as we go so the first frame does not briefly retain a
        // cache for the entire transcript.
        if dirty_from == 0 && index >= TRANSCRIPT_CACHE_CONTEXT_BLOCKS {
            let expired = index - TRANSCRIPT_CACHE_CONTEXT_BLOCKS;
            *app.blocks[expired].render_cache.borrow_mut() = None;
        }
        app.transcript_layout.blocks.push(TranscriptLayoutBlock {
            index,
            start,
            block_start,
            block_rows,
            user_padding,
        });
        previous_visible = Some((block.kind, block.turn_result));
    }
    app.transcript_layout.rows = row;
}

type MaterializedTranscript = (
    Vec<ListItem<'static>>,
    Vec<TextRowSeparator>,
    Vec<Option<usize>>,
    Vec<bool>,
);

/// One rendered transcript row: its line content plus the metadata the
/// viewport needs for styling, hit regions, and copy separators.
struct MaterializedTranscriptRow {
    line: Line<'static>,
    separator: TextRowSeparator,
    block: Option<usize>,
    user_surface: bool,
}

fn materialize_transcript_rows(
    app: &mut App,
    offset: usize,
    height: usize,
    message_width: usize,
    process_width: usize,
    active_block: Option<usize>,
) -> Vec<MaterializedTranscriptRow> {
    let end = offset.saturating_add(height);
    let mut rows = Vec::with_capacity(height);
    let first = app.transcript_layout.blocks.partition_point(|entry| {
        entry.block_start + entry.block_rows + usize::from(entry.user_padding) <= offset
    });
    let entries = app.transcript_layout.blocks[first..]
        .iter()
        .take_while(|entry| entry.start < end)
        .copied()
        .collect::<Vec<_>>();
    for entry in entries {
        let entry_end = entry.block_start + entry.block_rows + usize::from(entry.user_padding);
        let top_padding_start = entry
            .block_start
            .saturating_sub(usize::from(entry.user_padding));
        append_blank_transcript_rows(
            entry.start,
            top_padding_start,
            offset,
            end,
            false,
            &mut rows,
        );
        append_blank_transcript_rows(
            top_padding_start,
            entry.block_start,
            offset,
            end,
            true,
            &mut rows,
        );
        let visible_start = offset
            .saturating_sub(entry.block_start)
            .min(entry.block_rows);
        let visible_end = end.saturating_sub(entry.block_start).min(entry.block_rows);
        if visible_start < visible_end {
            let (block_rows, rendered) = {
                let block = &app.blocks[entry.index];
                cached_transcript_block_rows(
                    block,
                    entry.index == app.selected_block,
                    Some(entry.index) == active_block,
                    message_width,
                    process_width,
                    app,
                    visible_start..visible_end,
                )
            };
            #[cfg(test)]
            {
                app.transcript_render_stats.rendered_blocks += usize::from(rendered);
            }
            #[cfg(not(test))]
            let _ = rendered;
            for (line, separator) in block_rows {
                rows.push(MaterializedTranscriptRow {
                    line,
                    separator,
                    block: Some(entry.index),
                    user_surface: entry.user_padding,
                });
            }
        }
        append_blank_transcript_rows(
            entry.block_start + entry.block_rows,
            entry_end,
            offset,
            end,
            true,
            &mut rows,
        );
    }
    #[cfg(test)]
    {
        app.transcript_render_stats.materialized_rows = rows.len();
    }
    rows
}

fn materialize_transcript_viewport(
    app: &mut App,
    offset: usize,
    height: usize,
    message_width: usize,
    process_width: usize,
    active_block: Option<usize>,
) -> MaterializedTranscript {
    let rows = materialize_transcript_rows(
        app,
        offset,
        height,
        message_width,
        process_width,
        active_block,
    );
    let mut items = Vec::with_capacity(rows.len());
    let mut separators = Vec::with_capacity(rows.len());
    let mut block_for_row = Vec::with_capacity(rows.len());
    let mut user_surface_for_row = Vec::with_capacity(rows.len());
    for row in rows {
        let background = match row.block {
            Some(index) => match app.blocks[index].kind {
                BlockKind::User => USER_SURFACE,
                BlockKind::Assistant => BG,
                _ if index == app.selected_block => ROW_ACTIVE,
                _ => BG,
            },
            None if row.user_surface => USER_SURFACE,
            None => BG,
        };
        items.push(ListItem::new(row.line).style(Style::default().bg(background)));
        separators.push(row.separator);
        block_for_row.push(row.block);
        user_surface_for_row.push(row.user_surface);
    }
    (items, separators, block_for_row, user_surface_for_row)
}

fn append_blank_transcript_rows(
    start: usize,
    end: usize,
    viewport_start: usize,
    viewport_end: usize,
    user_surface: bool,
    rows: &mut Vec<MaterializedTranscriptRow>,
) {
    let count = end
        .min(viewport_end)
        .saturating_sub(start.max(viewport_start));
    for _ in 0..count {
        rows.push(MaterializedTranscriptRow {
            line: Line::default(),
            separator: TextRowSeparator::Newline,
            block: None,
            user_surface,
        });
    }
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

fn transcript_block_render_key(
    block: &DisplayBlock,
    selected: bool,
    live: bool,
    message_width: usize,
    process_width: usize,
    app: &App,
) -> TranscriptBlockRenderKey {
    let is_message = matches!(block.kind, BlockKind::User | BlockKind::Assistant);
    // A live process card renders from its child blocks; child changes bump
    // its revision, and its liveness joins the key like a live tool's.
    let process_live = block.kind == BlockKind::Process
        && block
            .process
            .as_ref()
            .is_some_and(|process| app.busy && app.live_process == Some(process.id));
    let live =
        process_live || (live && matches!(block.kind, BlockKind::Reasoning | BlockKind::Tool));
    TranscriptBlockRenderKey {
        revision: block.render_revision,
        message_width,
        process_width: if block.kind == BlockKind::Assistant {
            0
        } else {
            process_width
        },
        expanded: block.expanded,
        nested: block.parent_process.is_some(),
        selected: !is_message && selected,
        live,
        open_hint: if is_message {
            String::new()
        } else {
            app.keymap.key_hint("main", "open").map_or_else(
                || "right-click opens full".to_string(),
                |key| format!("{key} or right-click opens full"),
            )
        },
        expand_hint: if is_message {
            String::new()
        } else {
            app.keymap.key_hint("main", "toggle").map_or_else(
                || "select to expand".to_string(),
                |key| format!("{key} to expand"),
            )
        },
    }
}

fn ensure_transcript_block_cache(
    block: &DisplayBlock,
    selected: bool,
    live: bool,
    message_width: usize,
    process_width: usize,
    app: &App,
) -> bool {
    let key = transcript_block_render_key(block, selected, live, message_width, process_width, app);
    if block
        .render_cache
        .borrow()
        .as_ref()
        .is_some_and(|cache| cache.key == key)
    {
        return false;
    }
    let rows = transcript_block_items(block, selected, live, message_width, process_width, app);
    *block.render_cache.borrow_mut() = Some(TranscriptBlockRenderCache { key, rows });
    true
}

fn cached_transcript_block_row_count(
    block: &DisplayBlock,
    selected: bool,
    live: bool,
    message_width: usize,
    process_width: usize,
    app: &App,
) -> (usize, bool) {
    let rendered =
        ensure_transcript_block_cache(block, selected, live, message_width, process_width, app);
    let rows = block
        .render_cache
        .borrow()
        .as_ref()
        .map_or(0, |cache| cache.rows.len());
    (rows, rendered)
}

#[allow(clippy::too_many_arguments)]
fn cached_transcript_block_rows(
    block: &DisplayBlock,
    selected: bool,
    live: bool,
    message_width: usize,
    process_width: usize,
    app: &App,
    range: std::ops::Range<usize>,
) -> (Vec<(Line<'static>, TextRowSeparator)>, bool) {
    let rendered =
        ensure_transcript_block_cache(block, selected, live, message_width, process_width, app);
    let rows = block
        .render_cache
        .borrow()
        .as_ref()
        .map(|cache| cache.rows[range].to_vec())
        .unwrap_or_default();
    (rows, rendered)
}

pub(super) fn transcript_block_items(
    block: &DisplayBlock,
    selected: bool,
    live: bool,
    mut message_width: usize,
    mut process_width: usize,
    app: &App,
) -> Vec<(Line<'static>, TextRowSeparator)> {
    if block.parent_process.is_some() {
        message_width = message_width.saturating_sub(2).max(1);
        process_width = process_width.saturating_sub(2).max(1);
    }
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
                ));
            }
        }
        BlockKind::Assistant => {
            for rendered in markdown::render(&block.text, message_width) {
                let mut line = rendered.line;
                if block.parent_process.is_some() {
                    line.spans.insert(0, Span::raw("  "));
                }
                rows.push(line);
                row_separators.push(rendered.separator);
            }
        }
        BlockKind::Process => {
            let steps = block.process.as_ref().map_or(0, |process| process.steps);
            let process_id = block.process.as_ref().map(|process| process.id);
            let live = process_id.is_some_and(|id| app.busy && app.live_process == Some(id));
            let children = app
                .blocks
                .iter()
                .filter(|child| child.parent_process == process_id)
                .collect::<Vec<_>>();
            let (heading, failures) =
                process_summary(&children).unwrap_or_else(|| (process_step_label(steps), 0));
            let failures = (failures > 0).then(|| format!(" · {failures} failed"));
            let mut marker = if block.expanded {
                "  ▾".to_string()
            } else if live {
                // The card's own rows already show activity, so the summary
                // row keeps only the fold marker.
                "  ▸".to_string()
            } else {
                collapsed_hint.clone()
            };
            // Narrow rows shorten the expand hint, then the activity text;
            // the failure count always stays on screen.
            let indent = if block.parent_process.is_some() { 4 } else { 2 };
            let failure_width = failures.as_deref().map_or(0, UnicodeWidthStr::width);
            let fixed = indent + failure_width;
            if fixed + heading.width() + marker.width() > process_width {
                marker = if block.expanded { "  ▾" } else { "  ▸" }.to_string();
            }
            let heading_width = process_width.saturating_sub(fixed + marker.width()).max(1);
            let mut spans = vec![
                Span::styled(
                    if live { "◆ " } else { "◇ " },
                    Style::default().fg(if live { ACCENT } else { MUTED }),
                ),
                Span::styled(
                    single_line_preview(&heading, heading_width),
                    Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
                ),
            ];
            if let Some(failures) = failures {
                spans.push(Span::styled(
                    failures,
                    Style::default().fg(ERROR).add_modifier(Modifier::BOLD),
                ));
            }
            spans.push(Span::styled(marker, Style::default().fg(MUTED)));
            rows.push(transcript_block_item(block, spans));
            // While the turn runs, the collapsed process is a bounded card:
            // the summary line above and the latest activity rows below.
            if live && !block.expanded {
                let tail_rows = if app.compact {
                    PROCESS_CARD_TAIL_ROWS_COMPACT
                } else {
                    PROCESS_CARD_TAIL_ROWS
                };
                let activity = process_activity_rows(&children);
                let earlier = activity.len().saturating_sub(tail_rows);
                if earlier > 0 {
                    rows.push(transcript_block_item(
                        block,
                        vec![Span::styled(
                            format!(
                                "  … {earlier} earlier step{}",
                                if earlier == 1 { "" } else { "s" }
                            ),
                            Style::default().fg(MUTED),
                        )],
                    ));
                }
                for row in activity.into_iter().skip(earlier) {
                    rows.push(transcript_block_item(
                        block,
                        vec![
                            Span::raw("  "),
                            Span::styled(
                                format!("{} ", row.marker),
                                Style::default().fg(row.color),
                            ),
                            Span::styled(row.text, Style::default().fg(MUTED)),
                        ],
                    ));
                }
                // The latest intermediate text stays on the card's last rows,
                // growing the card until newer text or later responses
                // replace it.
                let narration = app.live_narration.and_then(|narration| {
                    children.iter().find(|child| {
                        child.kind == BlockKind::Assistant && child.id == narration.block_id
                    })
                });
                if let Some(narration) = narration {
                    let limit = if app.compact {
                        PROCESS_NARRATION_ROWS_COMPACT
                    } else {
                        PROCESS_NARRATION_ROWS
                    };
                    for line in
                        narration_lines(&narration.text, process_width.saturating_sub(4), limit)
                    {
                        rows.push(transcript_block_item(
                            block,
                            vec![
                                Span::raw("  "),
                                Span::styled(line, Style::default().fg(TEXT)),
                            ],
                        ));
                    }
                }
            }
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
                    ));
                }
                if !live && extra > 0 {
                    rows.push(transcript_hint(
                        extra,
                        "more",
                        true,
                        &open_hint,
                        &expand_hint,
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
                IN_PROGRESS_MARKER.to_string()
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
            ));
            if block.expanded {
                let (lines, extra) = tool_detail_lines(
                    block,
                    process_width,
                    TOOL_PREVIEW_ARGUMENT_LINES,
                    TOOL_PREVIEW_OUTPUT_LINES,
                );
                for (line, color) in lines {
                    rows.push(transcript_block_item(
                        block,
                        vec![
                            Span::raw("  "),
                            Span::styled(line, Style::default().fg(color)),
                        ],
                    ));
                }
                if extra > 0 {
                    rows.push(transcript_hint(
                        extra,
                        "more",
                        true,
                        &open_hint,
                        &expand_hint,
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
                ));
            }
            if extra > 0 {
                rows.push(transcript_hint(
                    extra,
                    "more",
                    block.expanded,
                    &open_hint,
                    &expand_hint,
                    block.parent_process.is_some(),
                ));
            }
        }
    }

    row_separators.resize(rows.len(), TextRowSeparator::Newline);
    rows.into_iter().zip(row_separators).collect()
}

pub(super) fn transcript_item(spans: Vec<Span<'static>>) -> Line<'static> {
    Line::from(spans)
}

pub(super) fn transcript_block_item(
    block: &DisplayBlock,
    mut spans: Vec<Span<'static>>,
) -> Line<'static> {
    if block.parent_process.is_some() {
        spans.insert(0, Span::raw("  "));
    }
    transcript_item(spans)
}

pub(super) fn transcript_hint(
    extra: usize,
    position: &str,
    expanded: bool,
    open_hint: &str,
    expand_hint: &str,
    nested: bool,
) -> Line<'static> {
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
    transcript_item(spans)
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
        ("TASKS", "tasks"),
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

/// Palette order by expected use: session lifecycle, per-turn model
/// choices, in-conversation tools, occasional configuration, then rare setup.
/// `insert` is last because `Space` and the compact action bar cover it.
/// Commands not listed here, such as extension commands, follow
/// alphabetically. Search scores still rank first; this order breaks ties.
pub(super) const COMMAND_ORDER: &[&str] = &[
    "new",
    "resume",
    "quit",
    "model",
    "effort",
    "search",
    "compact",
    "status",
    "tasks",
    "terminal",
    "copy",
    "layout",
    "settings",
    "help",
    "context-strategy",
    "login",
    "model-roles",
    "mcp",
    "logout",
    "protocols",
    "refresh-catalog",
    "set-env",
    "set-terminal",
    "insert",
];

fn command_rank(id: &str) -> usize {
    COMMAND_ORDER
        .iter()
        .position(|ordered| *ordered == id)
        .unwrap_or(COMMAND_ORDER.len())
}

pub(super) fn matching_commands(commands: &CommandRegistry, query: &str) -> Vec<CommandMatch> {
    let query = query.trim().trim_start_matches([':', '：']).to_lowercase();
    if query.is_empty() {
        let mut specs = commands.list();
        specs.sort_by_key(|spec| command_rank(&spec.id));
        return specs
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
            .then_with(|| command_rank(&left.2).cmp(&command_rank(&right.2)))
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

/// A float never renders narrower than this while the terminal has more room:
/// when its laid-out width would fall below this value, the float spans the
/// full terminal width without side borders or inner horizontal padding.
pub(super) const FLOAT_MIN_WIDTH: u16 = 60;

pub(super) fn overlay_area(frame: Rect, app: &App, overlay: Overlay) -> Rect {
    // Compact panels fill the screen; only input floats stay at the bottom
    // above the phone keyboard.
    if app.compact
        && !matches!(
            overlay,
            Overlay::Composer | Overlay::Delivery | Overlay::Text | Overlay::Oauth
        )
    {
        return frame;
    }
    match overlay {
        Overlay::Command => centered(frame, 72, 62),
        Overlay::Status => bottom_float(frame, 16),
        Overlay::Composer => bottom_float(
            frame,
            8 + pending_preview_height(app) + completion_preview_height(app),
        ),
        Overlay::Delivery => bottom_float(frame, 9),
        Overlay::Text | Overlay::Oauth => {
            let horizontal_margin = float_horizontal_margin(frame.width);
            Rect::new(
                horizontal_margin,
                frame.height.saturating_sub(12).max(2),
                frame.width.saturating_sub(horizontal_margin * 2),
                10,
            )
        }
        Overlay::Terminal => centered(frame, 92, 88),
        Overlay::Models | Overlay::Settings | Overlay::Selector => centered(frame, 82, 78),
        _ => centered(frame, 78, 72),
    }
}

pub(super) fn bottom_float(frame: Rect, desired_height: u16) -> Rect {
    let horizontal_margin = float_horizontal_margin(frame.width);
    let width = frame.width.saturating_sub(horizontal_margin * 2);
    let height = desired_height.min(frame.height).max(1);
    Rect::new(
        frame.x.saturating_add(horizontal_margin),
        frame.bottom().saturating_sub(height),
        width,
        height,
    )
}

/// Bottom floats keep a two-column margin only when doing so still leaves at
/// least `FLOAT_MIN_WIDTH` columns for the float itself.
fn float_horizontal_margin(frame_width: u16) -> u16 {
    const MARGIN: u16 = 2;
    u16::from(frame_width.saturating_sub(MARGIN * 2) >= FLOAT_MIN_WIDTH) * MARGIN
}

pub(super) fn active_protocols(app: &App) -> Vec<ProtocolDescriptor> {
    app.protocol_source
        .as_ref()
        .map_or_else(|| app.protocols.clone(), |source| source.descriptors())
}

pub(super) fn render_overlay(frame: &mut Frame<'_>, app: &mut App, overlay: Overlay) {
    let area = overlay_area(frame.area(), app, overlay);
    frame.render_widget(Clear, area);
    let edge_to_edge = area.width == frame.area().width;
    let (borders, padding) = if app.compact {
        (Borders::TOP, Padding::new(1, 1, 1, 0))
    } else if edge_to_edge {
        (Borders::TOP | Borders::BOTTOM, Padding::vertical(1))
    } else {
        (Borders::ALL, Padding::uniform(1))
    };
    let block = Block::default()
        .borders(borders)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(SURFACE).fg(TEXT))
        .padding(padding);
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
            app.composer_hint_width = composer_area.width.saturating_sub(4) as usize;
            app.sync_composer_chrome();
            if edge_to_edge && let Some(block) = app.input.block().cloned() {
                app.input
                    .set_block(block.borders(Borders::TOP | Borders::BOTTOM));
            }
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
                    .block(block.title(overlay_title(
                        app.compact,
                        "PROTOCOLS",
                        "help([name])".to_string(),
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
        Overlay::Plugin => render_plugin_panel(frame, app, area, block),
        Overlay::Document => {
            let hints = action_hints(&app.keymap, &[("document", "copy", "copy")]);
            let (name, body) = app
                .document
                .as_ref()
                .map(|(title, body)| (title.as_str(), body.as_str()))
                .unwrap_or(("DOCUMENT", "Nothing to show."));
            let title = overlay_title(app.compact, name, hints, area.width);
            let inner_width = block.inner(area).width as usize;
            let lines = markdown::render(body, inner_width)
                .into_iter()
                .map(|rendered| rendered.line)
                .collect::<Vec<_>>();
            frame.render_widget(
                Paragraph::new(lines)
                    .block(block.title(title))
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
            let title = overlay_title(app.compact, &prompt.title, String::new(), area.width);
            frame.render_widget(
                Paragraph::new(vec![
                    Line::styled(prompt.message.clone(), Style::default().fg(MUTED)),
                    Line::default(),
                    Line::styled(value, Style::default().fg(TEXT)),
                ])
                .block(block.title(title))
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
            let title = overlay_title(
                app.compact,
                &format!("OAUTH · {}", oauth.provider),
                String::new(),
                area.width,
            );
            frame.render_widget(
                Paragraph::new(lines)
                    .block(block.title(title))
                    .wrap(Wrap { trim: false }),
                area,
            );
        }
    }
    if app.compact && overlay != Overlay::Composer {
        render_close_button(frame, app, area, overlay != Overlay::Terminal);
    }
}

const CLOSE_BUTTON_WIDTH: u16 = 5;

/// Compact panels have no side borders to click outside of, so the header
/// carries a close target spanning the title row and, where the panel has
/// one, the padding row below it. It acts like the panel's `Esc`.
fn render_close_button(frame: &mut Frame<'_>, app: &mut App, area: Rect, two_rows: bool) {
    let width = CLOSE_BUTTON_WIDTH.min(area.width);
    let height = (1 + u16::from(two_rows)).min(area.height);
    let button = Rect::new(area.right().saturating_sub(width), area.y, width, height);
    if button.is_empty() {
        return;
    }
    frame.render_widget(Clear, button);
    frame.render_widget(
        Paragraph::new("✕").alignment(Alignment::Center).style(
            Style::default()
                .fg(ACCENT)
                .bg(ROW_ACTIVE)
                .add_modifier(Modifier::BOLD),
        ),
        button,
    );
    app.hit_regions.insert(
        0,
        HitRegion {
            area: button,
            target: AppHit::CloseOverlay,
        },
    );
}

/// Panel title for the current layout. Compact headers drop key hints, which
/// a touch user cannot press, and leave room for the close button.
pub(super) fn overlay_title(compact: bool, name: &str, hints: String, width: u16) -> String {
    if compact {
        fit_panel_title(
            &panel_title(name, String::new()),
            width.saturating_sub(CLOSE_BUTTON_WIDTH),
        )
    } else {
        fit_panel_title(&panel_title(name, hints), width)
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
    let title = overlay_title(app.compact, "TERMINAL", hints.join(" · "), area.width);
    let resize_error = {
        let Some(pty) = app.pty.as_mut() else {
            return;
        };
        pty.area = area;
        let edge_to_edge = area.width == frame.area().width;
        let inner = area.inner(Margin {
            horizontal: u16::from(!edge_to_edge),
            vertical: 1,
        });
        let resize_error = pty.terminal.resize(inner.height, inner.width).err();
        let parser = pty.terminal.screen();
        frame.render_widget(
            PseudoTerminal::new(parser.screen()).block(
                Block::default()
                    .borders(if edge_to_edge {
                        Borders::TOP | Borders::BOTTOM
                    } else {
                        Borders::ALL
                    })
                    .border_style(Style::default().fg(ACCENT))
                    .style(Style::default().bg(SURFACE))
                    .title(title),
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
    let model_time = model_time_status(app);
    let average_rate = average_rate_status(app);
    let compact = app.compact;
    let mut lines: Vec<Line<'static>> = [
        status_lines(compact, "PROJECT", project, Style::default().fg(ACCENT)),
        status_lines(
            compact,
            "SESSION",
            app.info.session_id.clone(),
            Style::default().fg(TEXT),
        ),
        status_lines(
            compact,
            "LOG",
            display_path(&app.info.diagnostics_path),
            Style::default().fg(MUTED),
        ),
        status_lines(
            compact,
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
        status_lines(compact, "STATE", state, Style::default().fg(ACCENT)),
        status_lines(
            compact,
            "CONTEXT",
            context_status(app, percent),
            Style::default()
                .fg(context_color(percent))
                .add_modifier(Modifier::BOLD),
        ),
        status_lines(
            compact,
            "TOKENS",
            format!(
                "input {} · output {} · total {}",
                format_tokens(app.usage.input),
                format_tokens(app.usage.output),
                format_tokens(app.usage.input.saturating_add(app.usage.output)),
            ),
            Style::default().fg(TEXT),
        ),
        status_lines(
            compact,
            "CACHE",
            format!(
                "read {} · write {} · last hit {cache_hit}",
                format_tokens(app.usage.cache_read),
                format_tokens(app.usage.cache_write),
            ),
            Style::default().fg(TEXT),
        ),
        status_lines(
            compact,
            "COST",
            format!(
                "${:.4}{}",
                app.usage.cost,
                if subscription { " · subscription" } else { "" }
            ),
            Style::default().fg(if subscription { ACCENT } else { TEXT }),
        ),
        status_lines(compact, "MODEL TIME", model_time, Style::default().fg(TEXT)),
        status_lines(compact, "AVG RATE", average_rate, Style::default().fg(TEXT)),
        status_lines(
            compact,
            "PROTOCOLS",
            format!("{} registered", active_protocols(app).len()),
            Style::default().fg(TEXT),
        ),
    ]
    .into_iter()
    .flatten()
    .collect();
    let plugin_items = plugin_status_items(app, true);
    if !plugin_items.is_empty() {
        lines.push(Line::default());
        lines.push(Line::styled(
            "EXTENSIONS",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
        lines.extend(plugin_items.into_iter().flat_map(|item| {
            status_lines(
                compact,
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

/// Cumulative measured model generation time for the session, including any
/// in-flight response, plus the most recent complete turn when available.
pub(super) fn model_time_status(app: &App) -> String {
    let total = app.token_rate.model_time(Instant::now());
    let last_turn = app.token_rate.last_turn_generation_time();
    if total.is_zero() && last_turn.is_none() {
        return "—".to_string();
    }
    let mut status = format_elapsed(total);
    if let Some(last_turn) = last_turn {
        status.push_str(&format!(" · last turn {}", format_elapsed(last_turn)));
    }
    status
}

/// Session average output rate over measured model time, plus the most recent
/// complete turn's average when available.
pub(super) fn average_rate_status(app: &App) -> String {
    match (
        app.token_rate.average_rate(),
        app.token_rate.final_average(),
    ) {
        (Some(average), Some(last_turn)) => format!(
            "{} · last turn {}",
            format_token_rate(average),
            format_token_rate(last_turn)
        ),
        (Some(average), None) => format_token_rate(average),
        (None, Some(last_turn)) => format!("last turn {}", format_token_rate(last_turn)),
        (None, None) => "—".to_string(),
    }
}

/// Status rows align values in an 11-column label gutter. Compact panels
/// are too narrow for wrapped values to align, so the label gets its own line.
pub(super) fn status_lines(
    compact: bool,
    label: impl Into<String>,
    value: impl Into<String>,
    value_style: Style,
) -> Vec<Line<'static>> {
    if !compact {
        return vec![status_row(label, value, value_style)];
    }
    vec![
        Line::styled(
            label.into(),
            Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
        ),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(value.into(), value_style),
        ]),
    ]
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
    let title = overlay_title(
        app.compact,
        "COMMAND",
        action_hints(&app.keymap, &[("command", "complete", "complete")]),
        area.width,
    );
    frame.render_widget(block.title(title), area);
    let compact = app.compact;
    let row_height = list_row_height(compact);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);
    app.overlay_viewport_rows = (sections[1].height / row_height) as usize;
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
        let name_style = Style::default()
            .fg(if selected { ACCENT } else { TEXT })
            .add_modifier(if selected {
                Modifier::BOLD
            } else {
                Modifier::empty()
            });
        if compact {
            let width = row_width.saturating_sub(2);
            return compact_list_item(
                selected,
                vec![Span::styled(
                    list_cell(&format!(":{}", item.name), width, selected, marquee_elapsed),
                    name_style,
                )],
                vec![Span::styled(
                    list_cell(&item.spec.description, width, selected, marquee_elapsed),
                    Style::default().fg(MUTED),
                )],
            );
        }
        ListItem::new(Line::from(vec![
            selection_marker(selected),
            Span::styled(
                list_cell(
                    &format!(":{}", item.name),
                    name_width,
                    selected,
                    marquee_elapsed,
                ),
                name_style,
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
        .style(selected_row_style(selected))
    });
    render_selection_list(
        frame,
        &mut app.hit_regions,
        items,
        sections[1],
        Some(app.command_selected),
        commands.len(),
        row_height,
        |index| Some(AppHit::Palette(index)),
    );
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
                selection_marker(selected),
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
            .style(selected_row_style(selected))
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
            PendingMessageKind::Steer => "STEER",
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
        block.title(overlay_title(
            app.compact,
            "SEND WHILE RUNNING",
            hints.join(" · "),
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
        ("Steer", "Add before the next model request"),
    ];
    let compact = app.compact;
    let row_height = list_row_height(compact);
    let items = choices
        .iter()
        .enumerate()
        .map(|(index, (title, description))| {
            let selected = delivery.selected == index;
            if compact {
                return compact_list_item(
                    selected,
                    vec![Span::styled(
                        *title,
                        Style::default().fg(if selected { ACCENT } else { TEXT }),
                    )],
                    vec![Span::styled(*description, Style::default().fg(MUTED))],
                );
            }
            ListItem::new(Line::from(vec![
                selection_marker(selected),
                Span::styled(
                    format!("{title:<12}"),
                    Style::default().fg(if selected { ACCENT } else { TEXT }),
                ),
                Span::styled(*description, Style::default().fg(MUTED)),
            ]))
            .style(selected_row_style(selected))
        });
    render_selection_list(
        frame,
        &mut app.hit_regions,
        items,
        sections[1],
        Some(delivery.selected),
        choices.len(),
        row_height,
        |index| Some(AppHit::Delivery(index)),
    );
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
        block.title(overlay_title(
            app.compact,
            &selector.title,
            String::new(),
            area.width,
        )),
        area,
    );
    let compact = app.compact;
    let row_height = list_row_height(compact);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);
    app.overlay_viewport_rows = (sections[1].height / row_height) as usize;
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
            if compact {
                let width = row_width.saturating_sub(2);
                return Some(compact_list_item(
                    selected,
                    vec![Span::styled(
                        list_cell(&item.title, width, selected, marquee_elapsed),
                        Style::default().fg(if selected { ACCENT } else { TEXT }),
                    )],
                    vec![Span::styled(
                        list_cell(&item.description, width, selected, marquee_elapsed),
                        Style::default().fg(MUTED),
                    )],
                ));
            }
            Some(
                ListItem::new(Line::from(vec![
                    selection_marker(selected),
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
                .style(selected_row_style(selected)),
            )
        });
    render_selection_list(
        frame,
        &mut app.hit_regions,
        items,
        sections[1],
        Some(selector.selected),
        selector.visible.len(),
        row_height,
        |position| Some(AppHit::Selector(position)),
    );
}

pub(super) fn render_models(frame: &mut Frame<'_>, app: &mut App, area: Rect, block: Block<'_>) {
    let inner = block.inner(area);
    let title = overlay_title(
        app.compact,
        "MODEL HUB",
        action_hints(&app.keymap, &[("models", "refresh", "refresh")]),
        area.width,
    );
    frame.render_widget(block.title(title), area);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(4),
            Constraint::Length(1),
        ])
        .split(inner);
    let Some(hub) = app.model_hub.as_ref() else {
        frame.render_widget(
            Paragraph::new("Model workspace is not loaded.").style(Style::default().fg(MUTED)),
            sections[2],
        );
        return;
    };
    let active_tab = hub.tab;
    let flow = hub.role_flow.clone();
    let hub_tabs = ModelHubTab::ALL.map(|tab| tab.label());
    let active_hub_tab = ModelHubTab::ALL
        .iter()
        .position(|tab| *tab == active_tab)
        .unwrap_or_default();
    render_tab_strip(
        frame,
        app,
        sections[0],
        active_hub_tab,
        &hub_tabs,
        flow.is_none(),
        AppHit::ModelHubTab,
    );
    frame.render_widget(
        Paragraph::new("─".repeat(sections[1].width as usize)).style(Style::default().fg(MUTED)),
        sections[1],
    );
    match &flow {
        Some(ModelRoleFlow::PickingModel { role }) => {
            render_model_browser(frame, app, sections[2], Some(role));
        }
        Some(ModelRoleFlow::PickingEffort {
            role,
            model,
            options,
            selected,
        }) => render_model_role_effort(frame, app, sections[2], role, model, options, *selected),
        Some(ModelRoleFlow::ConfirmRemove { .. }) => {
            render_model_roles(frame, app, sections[2], false);
        }
        None if active_tab == ModelHubTab::Roles => {
            render_model_roles(frame, app, sections[2], true);
        }
        None => render_model_browser(frame, app, sections[2], None),
    }
    render_model_hub_footer(frame, app, sections[3], flow.as_ref());
}

fn render_model_browser(frame: &mut Frame<'_>, app: &mut App, area: Rect, role: Option<&str>) {
    let compact = app.compact;
    let row_height = list_row_height(compact);
    // Compact search drops the framed heading box to keep rows for results.
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(if compact { 1 } else { 3 }),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);
    app.overlay_viewport_rows = (sections[1].height / row_height) as usize;
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
            area,
        );
        return;
    };
    let summary = if app.catalog_refreshing {
        format!(
            "{} refreshing model catalogs",
            animation::spinner(app.animation_phase)
        )
    } else {
        format!(
            "{} matches · {} models · {} providers",
            selector.visible_len(),
            selector.model_count(),
            selector.provider_count()
        )
    };
    let heading = role.map_or_else(
        || format!(" SEARCH · {summary} "),
        |role| format!(" ASSIGN {role} · STEP 1 OF 2 · {summary} "),
    );
    let query_width = sections[0].width.saturating_sub(6) as usize;
    let query = Paragraph::new(Line::from(vec![
        Span::styled("⌕  ", Style::default().fg(ACCENT)),
        Span::styled(
            single_line_tail(selector.query(), query_width),
            Style::default().fg(TEXT),
        ),
        Span::styled("█", Style::default().fg(ACCENT)),
    ]));
    frame.render_widget(
        if compact {
            query
        } else {
            query.block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(MUTED))
                    .title(fit_panel_title(&heading, sections[0].width)),
            )
        },
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
        let current = if selector.is_current(model) {
            "● "
        } else {
            "  "
        };
        if compact {
            let width = row_width.saturating_sub(4);
            return compact_list_item(
                selected,
                vec![
                    Span::styled(current, Style::default().fg(MUTED)),
                    Span::styled(
                        list_cell(model_label(model), width, selected, marquee_elapsed),
                        Style::default().fg(if selected { ACCENT } else { TEXT }),
                    ),
                ],
                vec![
                    Span::raw("  "),
                    Span::styled(
                        list_cell(
                            &format!("{} · {details}", model.provider_label()),
                            width,
                            selected,
                            marquee_elapsed,
                        ),
                        Style::default().fg(MUTED),
                    ),
                ],
            );
        }
        ListItem::new(Line::from(vec![
            selection_marker(selected),
            Span::styled(current, Style::default().fg(MUTED)),
            Span::styled(
                list_cell(
                    model.provider_label(),
                    provider_width,
                    selected,
                    marquee_elapsed,
                ),
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
        .style(selected_row_style(selected))
    });
    render_selection_list(
        frame,
        &mut app.hit_regions,
        items,
        sections[1],
        Some(selector.selected_position()),
        selector.visible_len(),
        row_height,
        |position| Some(AppHit::Model(position)),
    );
    let footer = if let Some(model) = selector.selected() {
        format!("{}/{} · {}", model.provider, model.id, model.api)
    } else {
        "No models match this search".to_string()
    };
    frame.render_widget(
        Paragraph::new(single_line_preview(&footer, sections[2].width as usize))
            .style(Style::default().fg(MUTED)),
        sections[2],
    );
}

fn role_source_label(role: &ModelRoleInfo) -> String {
    match (&role.source, role.overrides_global) {
        (Some(ValueSource::Project), true) => "PROJECT ← GLOBAL".to_string(),
        (Some(ValueSource::Project), false) => "PROJECT".to_string(),
        (Some(ValueSource::Global), _) => "GLOBAL".to_string(),
        (Some(source), _) => value_source_label(source),
        // Only plugin-declared roles are listed while unassigned.
        (None, _) => "PLUGIN".to_string(),
    }
}

fn render_model_roles(frame: &mut Frame<'_>, app: &mut App, area: Rect, interactive: bool) {
    let selected_key = app.model_hub.as_ref().and_then(|hub| {
        hub.selected_role()
            .map(|role| format!("model-role:{}", role.name))
    });
    let marquee_elapsed = selected_key
        .map(|key| app.marquee_elapsed(key))
        .unwrap_or_default();
    let Some(hub) = app.model_hub.as_ref() else {
        return;
    };
    if hub.roles.is_empty() {
        frame.render_widget(
            Paragraph::new("No model roles are available.").style(Style::default().fg(MUTED)),
            area,
        );
        return;
    }
    let compact = app.compact;
    let row_height = list_row_height(compact);
    app.overlay_viewport_rows = (area.height / row_height) as usize;
    let row_width = area.width as usize;
    let name_width = 18.min(row_width.saturating_sub(2));
    let source_width = 18.min(row_width.saturating_sub(2 + name_width));
    let effort_width = 10.min(
        row_width
            .saturating_sub(2 + name_width)
            .saturating_sub(source_width),
    );
    let assignment_width = row_width.saturating_sub(2 + name_width + effort_width + source_width);
    let items = hub.roles.iter().enumerate().map(|(index, role)| {
        let selected = hub.selected_role == index;
        let (assignment, effort) = if let Some(error) = &role.error {
            (error.clone(), "INVALID".to_string())
        } else if let Some(assignment) = &role.role {
            (
                format!("{}/{}", assignment.provider, assignment.model),
                assignment.thinking.to_string(),
            )
        } else {
            ("— no model assigned".to_string(), "—".to_string())
        };
        if compact {
            let width = row_width.saturating_sub(2);
            return compact_list_item(
                selected,
                vec![Span::styled(
                    list_cell(
                        &format!("{} · {}", role.name, role_source_label(role)),
                        width,
                        selected,
                        marquee_elapsed,
                    ),
                    Style::default().fg(if selected { ACCENT } else { TEXT }),
                )],
                vec![Span::styled(
                    list_cell(
                        &format!("{assignment} · {effort}"),
                        width,
                        selected,
                        marquee_elapsed,
                    ),
                    Style::default().fg(if role.error.is_some() { ERROR } else { MUTED }),
                )],
            );
        }
        ListItem::new(Line::from(vec![
            selection_marker(selected),
            Span::styled(
                list_cell(&role.name, name_width, selected, marquee_elapsed),
                Style::default().fg(if selected { ACCENT } else { TEXT }),
            ),
            Span::styled(
                list_cell(&assignment, assignment_width, selected, marquee_elapsed),
                Style::default().fg(if role.error.is_some() { ERROR } else { TEXT }),
            ),
            Span::styled(
                list_cell(&effort, effort_width, selected, marquee_elapsed),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                list_cell(
                    &role_source_label(role),
                    source_width,
                    selected,
                    marquee_elapsed,
                ),
                Style::default().fg(MUTED),
            ),
        ]))
        .style(selected_row_style(selected))
    });
    render_selection_list(
        frame,
        &mut app.hit_regions,
        items,
        area,
        Some(hub.selected_role),
        hub.roles.len(),
        row_height,
        |index| interactive.then_some(AppHit::ModelRole(index)),
    );
}

fn render_model_role_effort(
    frame: &mut Frame<'_>,
    app: &mut App,
    area: Rect,
    role: &str,
    model: &CatalogModel,
    options: &[ThinkingLevel],
    selected: usize,
) {
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(2)])
        .split(area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(
                format!("ASSIGN {role} · STEP 2 OF 2"),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Line::styled(
                format!("{}/{} · choose thinking effort", model.provider, model.id),
                Style::default().fg(MUTED),
            ),
        ]),
        sections[0],
    );
    app.overlay_viewport_rows = sections[1].height as usize;
    let items = options.iter().enumerate().map(|(index, level)| {
        let active = index == selected;
        ListItem::new(Line::from(vec![
            selection_marker(active),
            Span::styled(
                level.to_string(),
                Style::default().fg(if active { ACCENT } else { TEXT }),
            ),
        ]))
        .style(selected_row_style(active))
    });
    render_selection_list(
        frame,
        &mut app.hit_regions,
        items,
        sections[1],
        Some(selected),
        options.len(),
        1,
        |index| Some(AppHit::ModelRoleEffort(index)),
    );
}

fn value_source_label(source: &ValueSource) -> String {
    match source {
        ValueSource::Global => "GLOBAL".to_string(),
        ValueSource::Project => "PROJECT".to_string(),
        ValueSource::Default => "DEFAULT".to_string(),
        ValueSource::Session => "SESSION".to_string(),
        source => source.label().to_uppercase(),
    }
}

fn render_model_hub_footer(
    frame: &mut Frame<'_>,
    app: &App,
    area: Rect,
    flow: Option<&ModelRoleFlow>,
) {
    let Some(hub) = app.model_hub.as_ref() else {
        return;
    };
    let (message, style) = match flow {
        Some(ModelRoleFlow::ConfirmRemove {
            role,
            source,
            reveals_global,
        }) => {
            let hints = action_hints(
                &app.keymap,
                &[
                    ("models", "confirm", "confirm"),
                    ("models", "close", "keep"),
                ],
            );
            let scope = match source {
                ValueSource::Project => "PROJECT",
                ValueSource::Global => "GLOBAL",
                _ => "SAVED",
            };
            let action = if *reveals_global {
                format!("{scope} {role} → GLOBAL fallback")
            } else {
                format!("REMOVE {scope} ASSIGNMENT FOR {role}")
            };
            (format!("{hints} · {action}"), Style::default().fg(ERROR))
        }
        Some(ModelRoleFlow::PickingModel { .. }) => {
            let hints = action_hints(
                &app.keymap,
                &[
                    ("models", "confirm", "choose model"),
                    ("models", "close", "back to roles"),
                    ("models", "refresh", "refresh"),
                ],
            );
            (
                format!("{hints} · type to search"),
                Style::default().fg(MUTED),
            )
        }
        Some(ModelRoleFlow::PickingEffort { .. }) => (
            action_hints(
                &app.keymap,
                &[
                    ("models", "confirm", "save assignment"),
                    ("models", "previous", "choose effort"),
                    ("models", "close", "back to model"),
                ],
            ),
            Style::default().fg(MUTED),
        ),
        None if hub.tab == ModelHubTab::Roles => (
            action_hints(
                &app.keymap,
                &[
                    ("models", "confirm", "assign"),
                    ("model_roles", "remove", "remove"),
                    ("models", "next_tab", "models"),
                    ("models", "close", "close"),
                ],
            ),
            Style::default().fg(MUTED),
        ),
        None => {
            let hints = action_hints(
                &app.keymap,
                &[
                    ("models", "confirm", "select"),
                    ("models", "next_tab", "roles"),
                    ("models", "refresh", "refresh"),
                    ("models", "close", "close"),
                ],
            );
            (
                format!("{hints} · type to search"),
                Style::default().fg(MUTED),
            )
        }
    };
    frame.render_widget(
        Paragraph::new(Line::styled(
            single_line_preview(&message, area.width as usize),
            style,
        )),
        area,
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
    let tab = settings.tab;
    let title = overlay_title(
        app.compact,
        "SETTINGS",
        action_hints(&app.keymap, &[("settings", "save", "save")]),
        area.width,
    );
    frame.render_widget(block.title(title), area);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(6),
            Constraint::Length(1),
        ])
        .split(inner);
    let settings_tabs = SettingsTab::ALL.map(|tab| tab.label());
    let active_settings_tab = SettingsTab::ALL
        .iter()
        .position(|candidate| *candidate == tab)
        .unwrap_or_default();
    render_tab_strip(
        frame,
        app,
        sections[0],
        active_settings_tab,
        &settings_tabs,
        true,
        AppHit::SettingsTab,
    );
    frame.render_widget(
        Paragraph::new("─".repeat(sections[1].width as usize)).style(Style::default().fg(MUTED)),
        sections[1],
    );
    render_settings_body(frame, app, sections[2], marquee_elapsed);
    let hints = action_hints(
        &app.keymap,
        &[
            ("settings", "edit", "edit"),
            ("settings", "next_tab", "sections"),
            ("settings", "save", "save"),
            ("settings", "close", "close"),
        ],
    );
    frame.render_widget(
        Paragraph::new(Line::styled(hints, Style::default().fg(MUTED))),
        sections[3],
    );
}

fn render_settings_body(frame: &mut Frame<'_>, app: &mut App, area: Rect, marquee_elapsed: usize) {
    let Some(settings) = app.settings.as_ref() else {
        return;
    };
    let model = settings
        .model()
        .map(|model| format!("{} / {}", model.provider, model_label(model)))
        .unwrap_or_else(|| format!("{} / {}", settings.active.provider, settings.active.model));
    let credential = match settings.active.auth_kind {
        AuthKind::Oauth => "OAuth".to_string(),
        AuthKind::ApiKey => "API key".to_string(),
        AuthKind::None => "not configured · use :login".to_string(),
    };
    let credential_source = if settings.active.auth_kind == AuthKind::None {
        "—".to_string()
    } else {
        value_source_label(&settings.active.api_key_source)
    };
    let output_limit = if settings.editing == Some(EditingSetting::OutputLimit) {
        format!("{}█", settings.output_limit)
    } else {
        format!("{} bytes", settings.output_limit)
    };
    let environment = format!(
        "{} variable{}",
        settings.environment_count,
        if settings.environment_count == 1 {
            ""
        } else {
            "s"
        }
    );
    let model_pending = settings.model().is_some_and(|selected| {
        selected.provider != settings.active.provider || selected.id != settings.active.model
    });
    let rows = match settings.tab {
        SettingsTab::Model => vec![
            (
                SettingsItem::Model,
                "Model",
                model,
                if model_pending {
                    "PENDING".to_string()
                } else {
                    value_source_label(&settings.active.model_source)
                },
            ),
            (
                SettingsItem::Credential,
                "Credential",
                format!("{} · {credential}", settings.active.provider),
                credential_source,
            ),
            (
                SettingsItem::Thinking,
                "Thinking",
                settings.thinking.to_string(),
                if settings.thinking != settings.active.thinking {
                    "PENDING".to_string()
                } else {
                    value_source_label(&settings.active.thinking_source)
                },
            ),
        ],
        SettingsTab::Agent => vec![
            (
                SettingsItem::OutputLimit,
                "Output limit",
                output_limit,
                if settings.output_limit != settings.active.output_limit.to_string() {
                    "PENDING".to_string()
                } else {
                    value_source_label(&settings.active.output_limit_source)
                },
            ),
            (
                SettingsItem::Environment,
                "Agent environment",
                environment,
                "PRIVATE GLOBAL".to_string(),
            ),
        ],
    };
    let row_count = rows.len();
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(2), Constraint::Length(4)])
        .split(area);
    let compact = app.compact;
    let row_height = list_row_height(compact);
    app.overlay_viewport_rows = (sections[0].height / row_height) as usize;
    let row_width = sections[0].width as usize;
    let label_width = 18.min(row_width.saturating_sub(2));
    let source_width = 22.min(row_width.saturating_sub(2 + label_width));
    let value_width = row_width.saturating_sub(2 + label_width + source_width);
    let items = rows
        .iter()
        .enumerate()
        .map(|(index, (_, label, value, source))| {
            let selected = settings.selected == index;
            if compact {
                let width = row_width.saturating_sub(2);
                return compact_list_item(
                    selected,
                    vec![Span::styled(
                        list_cell(
                            &format!("{label} · {source}"),
                            width,
                            selected,
                            marquee_elapsed,
                        ),
                        Style::default().fg(if selected { ACCENT } else { MUTED }),
                    )],
                    vec![Span::styled(
                        list_cell(value, width, selected, marquee_elapsed),
                        Style::default().fg(TEXT),
                    )],
                );
            }
            ListItem::new(Line::from(vec![
                selection_marker(selected),
                Span::styled(
                    list_cell(label, label_width, selected, marquee_elapsed),
                    Style::default().fg(if selected { ACCENT } else { MUTED }),
                ),
                Span::styled(
                    list_cell(value, value_width, selected, marquee_elapsed),
                    Style::default().fg(TEXT),
                ),
                Span::styled(
                    list_cell(source, source_width, selected, marquee_elapsed),
                    Style::default().fg(MUTED),
                ),
            ]))
            .style(selected_row_style(selected))
        });
    render_selection_list(
        frame,
        &mut app.hit_regions,
        items,
        sections[0],
        Some(settings.selected),
        row_count,
        row_height,
        |index| Some(AppHit::Setting(index)),
    );
    let detail = match settings.selected_item() {
        SettingsItem::Model => {
            "Conversation model · opens Model Hub · selection remains pending until Settings is saved"
        }
        SettingsItem::Credential => {
            "Credential for the active provider · manage stored credentials with :login / :logout"
        }
        SettingsItem::Thinking => {
            "Thinking effort supported by the selected model · editing cycles available levels"
        }
        SettingsItem::OutputLimit => "Maximum inline tool-result bytes · minimum 1024",
        SettingsItem::Environment => {
            "Private variables injected into future Agent shell commands · values remain hidden"
        }
    };
    let mut detail_lines = vec![Line::styled(
        "DETAIL",
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )];
    detail_lines.extend(
        textwrap::wrap(detail, sections[1].width as usize)
            .into_iter()
            .take(3)
            .map(|line| Line::styled(Cow::into_owned(line), Style::default().fg(MUTED))),
    );
    frame.render_widget(Paragraph::new(detail_lines), sections[1]);
}

/// Task rows: running first (newest first), then finished (newest first).
pub(super) fn order_task_records(records: &mut [TaskRecord]) {
    records.sort_by_key(|record| {
        (
            record.status.terminal(),
            std::cmp::Reverse(record.started_at),
        )
    });
}

const TASK_DETAIL_MAX_HEIGHT: u16 = 10;

/// Detail height below the list: at most a third of the panel, and never
/// below the three list rows a usable panel needs.
fn task_detail_height(inner_height: u16) -> u16 {
    TASK_DETAIL_MAX_HEIGHT
        .min(inner_height / 2)
        .min(inner_height.saturating_sub(3))
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
    let interruptible = app
        .task_records
        .get(app.selected_task)
        .is_some_and(TaskRecord::interruptible);
    let mut hints = vec![("tasks", "open", "open"), ("tasks", "copy", "copy")];
    if interruptible {
        hints.push(("tasks", "interrupt", "interrupt"));
    }
    hints.push(("tasks", "cancel", "cancel"));
    let inner = block.inner(area);
    let title = overlay_title(
        app.compact,
        "TASKS",
        action_hints(&app.keymap, &hints),
        area.width,
    );
    frame.render_widget(block.title(title), area);
    let selected_key = app
        .task_records
        .get(app.selected_task)
        .map(|task| format!("task:{}", task.id));
    let marquee_elapsed = selected_key
        .map(|key| app.marquee_elapsed(key))
        .unwrap_or_default();
    let compact = app.compact;
    let row_height = list_row_height(compact);
    let now = chrono::Utc::now();
    let detail_height = task_detail_height(inner.height);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(detail_height)])
        .split(inner);
    let (list_area, detail_area) = (sections[0], sections[1]);
    app.overlay_viewport_rows = (list_area.height / row_height) as usize;
    let row_width = list_area.width as usize;
    let elapsed_width = 7;
    let protocol_width = 8;
    let label_width = row_width.saturating_sub(2 + 2 + protocol_width + elapsed_width);
    let items = app.task_records.iter().enumerate().map(|(index, task)| {
        let selected = index == app.selected_task;
        let (glyph, glyph_color) = task_status_glyph(task.status, app.animation_phase);
        let elapsed = task_elapsed_text(task, now);
        if compact {
            let width = row_width.saturating_sub(4);
            return compact_list_item(
                selected,
                vec![
                    Span::styled(glyph.to_string(), Style::default().fg(glyph_color)),
                    Span::raw(" "),
                    Span::styled(
                        list_cell(&task.label, width, selected, marquee_elapsed),
                        Style::default().fg(if selected { ACCENT } else { TEXT }),
                    ),
                ],
                vec![Span::styled(
                    format!("{} · {} · {}", task.protocol, task.status.as_str(), elapsed),
                    Style::default().fg(MUTED),
                )],
            );
        }
        ListItem::new(Line::from(vec![
            selection_marker(selected),
            Span::styled(glyph.to_string(), Style::default().fg(glyph_color)),
            Span::raw(" "),
            Span::styled(
                list_cell(task.protocol.as_str(), protocol_width, false, 0),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                list_cell(&task.label, label_width, selected, marquee_elapsed),
                Style::default().fg(if selected { ACCENT } else { TEXT }),
            ),
            Span::styled(
                format!("{elapsed:>elapsed_width$}"),
                Style::default().fg(MUTED),
            ),
        ]))
    });
    render_selection_list(
        frame,
        &mut app.hit_regions,
        items,
        list_area,
        Some(app.selected_task),
        app.task_records.len(),
        row_height,
        |index| Some(AppHit::Task(index)),
    );
    if let Some(record) = app.task_records.get(app.selected_task) {
        render_task_detail(frame, record, detail_area, now, app.animation_phase);
    }
}

/// Selected-task detail: identity, timestamps, and a live, sanitised tail of
/// the newest output. The tail follows the record's bounded latest output;
/// the complete output stays one `open` press away.
fn render_task_detail(
    frame: &mut Frame<'_>,
    record: &TaskRecord,
    area: Rect,
    now: chrono::DateTime<chrono::Utc>,
    animation_phase: f64,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let width = area.width as usize;
    let (glyph, glyph_color) = task_status_glyph(record.status, animation_phase);
    let elapsed = task_elapsed_text(record, now);
    let mut lines = vec![Line::styled("─".repeat(width), Style::default().fg(MUTED))];
    lines.push(Line::from(vec![
        Span::styled(glyph.to_string(), Style::default().fg(glyph_color)),
        Span::raw(" "),
        Span::styled(
            format!("{} · {}", record.id, record.protocol),
            Style::default().fg(TEXT),
        ),
        Span::styled(
            format!(" · {} · {elapsed}", record.status.as_str()),
            Style::default().fg(glyph_color),
        ),
    ]));
    let mut times = format!("started {}", format_task_time(record.started_at));
    if let Some(finished) = record.finished_at {
        times.push_str(&format!(" · finished {}", format_task_time(finished)));
    }
    lines.push(Line::styled(times, Style::default().fg(MUTED)));
    let tail_rows = (area.height as usize).saturating_sub(3);
    if record.latest_output.is_empty() {
        lines.push(Line::styled("no output yet", Style::default().fg(MUTED)));
    } else {
        // Reserve one row for the cut marker when earlier lines are dropped.
        let (dropped, tail) =
            sanitize_output_tail(&record.latest_output, tail_rows.saturating_sub(1));
        if dropped > 0 {
            lines.push(Line::styled(
                format!(
                    "… {dropped} earlier line{}",
                    if dropped == 1 { "" } else { "s" }
                ),
                Style::default().fg(MUTED),
            ));
        }
        lines.extend(tail.into_iter().map(|line| {
            Line::styled(single_line_preview(&line, width), Style::default().fg(TEXT))
        }));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

pub(super) fn panel_tone_style(tone: TuiPanelTone) -> Style {
    Style::default().fg(match tone {
        TuiPanelTone::Default => TEXT,
        TuiPanelTone::Accent => ACCENT,
        TuiPanelTone::Muted => MUTED,
        TuiPanelTone::Warning => WARM,
        TuiPanelTone::Error => ERROR,
    })
}

pub(super) fn render_plugin_panel(
    frame: &mut Frame<'_>,
    app: &mut App,
    area: Rect,
    block: Block<'_>,
) {
    let Some(panel) = app.tui_panel.as_mut() else {
        frame.render_widget(
            Paragraph::new("Plugin panel is unavailable.")
                .block(block.title(" PLUGIN PANEL "))
                .style(Style::default().fg(MUTED)),
            area,
        );
        return;
    };
    let view = panel.view();
    let hints = view
        .hints
        .iter()
        .map(|hint| format!("{} {}", hint.key, hint.label))
        .collect::<Vec<_>>()
        .join(" · ");
    let title = overlay_title(app.compact, &view.title, String::new(), area.width);
    let inner = block.inner(area);
    frame.render_widget(block.title(title), area);
    let message_height = u16::from(view.message.is_some());
    let hints_height = u16::from(!hints.is_empty());
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(message_height),
            Constraint::Length(hints_height),
        ])
        .split(inner);
    let compact = app.compact;
    let row_height = list_row_height(compact);
    app.overlay_viewport_rows = (sections[0].height / row_height) as usize;
    let row_width = sections[0].width as usize;
    let label_width = 20.min(row_width.saturating_sub(2));
    let value_width = row_width.saturating_sub(label_width + 2);
    let items = view.rows.iter().enumerate().map(|(index, row)| {
        let selected = view.selected == Some(index);
        let mut value = row.value.clone();
        if let Some(cursor) = row.cursor {
            let mut characters = value.chars().collect::<Vec<_>>();
            characters.insert(cursor.min(characters.len()), '█');
            value = characters.into_iter().collect();
        }
        if compact {
            let width = row_width.saturating_sub(2);
            let mut secondary = vec![Span::styled(
                single_line_preview(&value, width),
                panel_tone_style(row.tone),
            )];
            let description_width = width.saturating_sub(value.width());
            if !row.description.is_empty() && description_width > 3 {
                let separator = if value.is_empty() { "" } else { " · " };
                secondary.push(Span::styled(
                    format!(
                        "{separator}{}",
                        single_line_preview(
                            &row.description,
                            description_width.saturating_sub(separator.width())
                        )
                    ),
                    Style::default().fg(MUTED),
                ));
            }
            return compact_list_item(
                selected,
                vec![Span::styled(
                    single_line_preview(&row.label, width),
                    panel_tone_style(row.tone).add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
                )],
                secondary,
            );
        }
        let value_limit = if row.description.is_empty() {
            value_width
        } else {
            (value_width * 3 / 5).max(8).min(value_width)
        };
        let value_preview = single_line_preview(&value, value_limit);
        let description_width = value_width.saturating_sub(value_preview.chars().count());
        let mut spans = vec![
            selection_marker(selected),
            Span::styled(
                format!(
                    "{:<width$}",
                    single_line_preview(&row.label, label_width),
                    width = label_width
                ),
                panel_tone_style(row.tone).add_modifier(if selected {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
            ),
            Span::styled(value_preview, panel_tone_style(row.tone)),
        ];
        if !row.description.is_empty() && description_width > 3 {
            spans.push(Span::styled(
                format!(
                    " · {}",
                    single_line_preview(&row.description, description_width.saturating_sub(3))
                ),
                Style::default().fg(MUTED),
            ));
        }
        ListItem::new(Line::from(spans)).style(selected_row_style(selected))
    });
    let selected = view.selected.filter(|index| *index < view.rows.len());
    render_selection_list(
        frame,
        &mut app.hit_regions,
        items,
        sections[0],
        selected,
        view.rows.len(),
        row_height,
        |index| {
            view.rows[index]
                .selectable
                .then_some(AppHit::PluginRow(index))
        },
    );
    if let Some((message, tone)) = view.message {
        frame.render_widget(
            Paragraph::new(single_line_preview(&message, sections[1].width as usize))
                .style(panel_tone_style(tone)),
            sections[1],
        );
    }
    if !hints.is_empty() {
        frame.render_widget(
            Paragraph::new(single_line_preview(&hints, sections[2].width as usize))
                .style(Style::default().fg(MUTED)),
            sections[2],
        );
        let mut x = sections[2].x;
        for (index, hint) in view.hints.iter().enumerate() {
            if x >= sections[2].right() {
                break;
            }
            let text = format!("{} {}", hint.key, hint.label);
            let width = text
                .width()
                .min(sections[2].right().saturating_sub(x) as usize) as u16;
            if hint.action.is_some() && width > 0 {
                app.hit_regions.push(HitRegion {
                    area: Rect::new(x, sections[2].y, width, 1),
                    target: AppHit::PluginHint(index),
                });
            }
            x = x.saturating_add(width).saturating_add(3);
        }
    }
}

pub(super) fn style_input(
    input: &mut TextArea<'static>,
    busy: bool,
    keymap: &Keymap,
    hint_width: usize,
) {
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
    // Drop trailing hints rather than letting the right-aligned border title
    // clip the leading ones.
    let footer = fitted_hints(&hints, hint_width)
        .map(|hints| format!(" {hints} "))
        .unwrap_or_default();
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
        "Add Steer or queue a follow-up…"
    } else {
        "Ask URI Agent to build, explain, or fix…"
    });
    input.set_placeholder_style(Style::default().fg(MUTED).bg(SURFACE));
    input.set_style(Style::default().fg(TEXT).bg(SURFACE));
    input.set_cursor_line_style(Style::default().fg(TEXT).bg(SURFACE));
    input.set_cursor_style(Style::default().fg(TEXT).bg(ACCENT));
    input.set_selection_style(Style::default().fg(SURFACE).bg(ACCENT));
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
    let band = vertical[1];
    if band.width <= FLOAT_MIN_WIDTH {
        return band;
    }
    let share = (u32::from(band.width) * u32::from(width_percent) / 100) as u16;
    // Keep only half of the side margins the percentage split would leave so
    // wide terminals do not waste columns on empty padding.
    let margin = (band.width - share) / 4;
    let width = (band.width - margin * 2).clamp(FLOAT_MIN_WIDTH, band.width);
    let margin = (band.width - width) / 2;
    Rect::new(band.x + margin, band.y, width, band.height)
}

pub(super) fn capture_surface(
    frame: &mut Frame<'_>,
    app: &mut App,
    area: Rect,
    row_separators: Option<Vec<TextRowSeparator>>,
    left_padding: usize,
) {
    let scroll_origin = if app.overlay.is_some() {
        usize::from(app.overlay_scroll)
    } else {
        app.transcript_offset
    };
    revalidate_selection(app, area, scroll_origin);
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
        scroll_origin,
        overlay: app.overlay,
    });
}

// A transcript selection anchors to its blocks rather than the viewport, so
// scrolling, streaming appends, and history prepends leave it attached to the
// same content; re-resolve the anchored rows against the latest layout before
// each capture, and clear the selection once its content is gone (rewrap,
// collapse, or a session change). Selections on other surfaces keep the
// visible-window rule: they are cleared as soon as either end scrolls out of
// view or the surface width changes.
fn revalidate_selection(app: &mut App, area: Rect, scroll_origin: usize) {
    let Some(mut selection) = app.selection else {
        return;
    };
    if selection.overlay != app.overlay {
        app.selection = None;
        return;
    }
    match selection.anchors {
        Some(anchors) => {
            if anchors.message_width != app.transcript_layout.message_width
                || anchors.process_width != app.transcript_layout.process_width
            {
                app.selection = None;
                return;
            }
            let Some(start_row) = resolve_selection_row_anchor(app, anchors.start) else {
                app.selection = None;
                return;
            };
            let Some(end_row) = resolve_selection_row_anchor(app, anchors.end) else {
                app.selection = None;
                return;
            };
            selection.start.1 = start_row;
            selection.end.1 = end_row;
            app.selection = Some(selection);
        }
        None => {
            let bottom = scroll_origin.saturating_add(usize::from(area.height));
            if selection.surface_width != area.width
                || [selection.start.1, selection.end.1]
                    .into_iter()
                    .any(|row| row < scroll_origin || row >= bottom)
            {
                app.selection = None;
            }
        }
    }
}

// Anchor a transcript content row to the block that contains or follows it, so
// later layout shifts can resolve the row back to the same content.
fn selection_row_anchor(app: &App, row: usize) -> Option<SelectionRowAnchor> {
    if app.overlay.is_some() || app.blocks.is_empty() {
        return None;
    }
    let entries = &app.transcript_layout.blocks;
    let index = entries.partition_point(|entry| entry.start <= row);
    let entry = entries.get(index.saturating_sub(1))?;
    let block = app.blocks.get(entry.index)?;
    Some(SelectionRowAnchor {
        block_id: block.id,
        block_index: entry.index,
        offset_from_block: row as isize - entry.block_start as isize,
        in_content: row >= entry.block_start && row < entry.block_start + entry.block_rows,
    })
}

fn resolve_selection_row_anchor(app: &App, anchor: SelectionRowAnchor) -> Option<usize> {
    let layout_entry = |entry: &&TranscriptLayoutBlock| {
        app.blocks
            .get(entry.index)
            .is_some_and(|block| block.id == anchor.block_id)
    };
    // Prefer the creation-time index; fall back to an id scan for blocks that
    // moved (history prepends shift every index).
    let entry = app
        .transcript_layout
        .blocks
        .iter()
        .find(|entry| entry.index == anchor.block_index && layout_entry(entry))
        .or_else(|| {
            app.transcript_layout
                .blocks
                .iter()
                .find(|entry| entry.index != anchor.block_index && layout_entry(entry))
        })?;
    if anchor.in_content && anchor.offset_from_block >= entry.block_rows as isize {
        // The anchored content rows are gone (for example after a collapse).
        return None;
    }
    let top = entry.start as isize;
    let bottom = (entry.block_start + entry.block_rows + usize::from(entry.user_padding)) as isize;
    let row = entry.block_start as isize + anchor.offset_from_block;
    Some(row.clamp(top, bottom.saturating_sub(1).max(top)).max(0) as usize)
}

fn new_surface_selection(app: &App, start: (u16, usize), end: (u16, usize)) -> TextSelection {
    let surface_width = app
        .selectable
        .as_ref()
        .map_or(0, |surface| surface.area.width);
    let anchors = match (
        selection_row_anchor(app, start.1),
        selection_row_anchor(app, end.1),
    ) {
        (Some(start), Some(end)) => Some(SelectionRowAnchors {
            start,
            end,
            message_width: app.transcript_layout.message_width,
            process_width: app.transcript_layout.process_width,
        }),
        _ => None,
    };
    TextSelection {
        start,
        end,
        overlay: app.overlay,
        surface_width,
        anchors,
    }
}

fn ordered_selection_points(selection: TextSelection) -> ((u16, usize), (u16, usize)) {
    if (selection.start.1, selection.start.0) <= (selection.end.1, selection.end.0) {
        (selection.start, selection.end)
    } else {
        (selection.end, selection.start)
    }
}

pub(super) fn render_selection(frame: &mut Frame<'_>, app: &App) {
    let (Some(surface), Some(selection)) = (&app.selectable, app.selection) else {
        return;
    };
    if surface.cells.is_empty() {
        return;
    }
    let (start, end) = ordered_selection_points(selection);
    let left = surface.area.x;
    let right = surface.area.right().saturating_sub(1);
    // Map content rows back to screen rows; rows scrolled out of the viewport
    // stay selected and copyable but are not highlighted.
    let first_visible = start.1.max(surface.scroll_origin);
    let last_visible = end.1.min(surface.scroll_origin + surface.cells.len() - 1);
    if first_visible > last_visible {
        return;
    }
    for content_row in first_visible..=last_visible {
        let screen_row = surface.area.y + (content_row - surface.scroll_origin) as u16;
        let from = if content_row == start.1 {
            start.0.clamp(left, right)
        } else {
            left
        };
        let to = if content_row == end.1 {
            end.0.clamp(left, right)
        } else {
            right
        };
        if from > to {
            continue;
        }
        for column in from..=to {
            if let Some(cell) = frame.buffer_mut().cell_mut((column, screen_row)) {
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
    let Some((area, scroll_origin)) = app
        .selectable
        .as_ref()
        .map(|surface| (surface.area, surface.scroll_origin))
    else {
        return false;
    };
    let screen = (
        mouse.column.clamp(area.x, area.right().saturating_sub(1)),
        mouse.row.clamp(area.y, area.bottom().saturating_sub(1)),
    );
    let point = (screen.0, scroll_origin + usize::from(screen.1 - area.y));
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left)
            if (!require_shift || mouse.modifiers.contains(KeyModifiers::SHIFT))
                && area.contains(screen.into()) =>
        {
            let double_click = is_double_click(
                &mut app.last_text_click,
                TextClickTarget::Surface(app.overlay, screen),
            );
            app.text_selection_dragged = false;
            if double_click
                && let Some(selection) = app
                    .selectable
                    .as_ref()
                    .and_then(|surface| surface_word_selection(app, surface, screen))
            {
                app.selection = Some(selection);
                app.mouse_word_selecting = true;
                return true;
            }
            app.selection = Some(new_surface_selection(app, point, point));
            app.mouse_word_selecting = false;
            true
        }
        MouseEventKind::Drag(MouseButton::Left) if app.selection.is_some() => {
            app.last_text_click = None;
            app.mouse_word_selecting = false;
            app.text_selection_dragged = true;
            let end_anchor = selection_row_anchor(app, point.1);
            if let Some(selection) = app.selection.as_mut() {
                selection.end = point;
                if let (Some(anchors), Some(end)) = (selection.anchors.as_mut(), end_anchor) {
                    anchors.end = end;
                }
            }
            true
        }
        MouseEventKind::Up(MouseButton::Left) if app.selection.is_some() => {
            if app.mouse_word_selecting {
                app.mouse_word_selecting = false;
                return true;
            }
            let dragged = app.text_selection_dragged;
            let end_anchor = selection_row_anchor(app, point.1);
            let empty = if let Some(selection) = app.selection.as_mut() {
                selection.end = point;
                if let (Some(anchors), Some(end)) = (selection.anchors.as_mut(), end_anchor) {
                    anchors.end = end;
                }
                selection.start == selection.end
            } else {
                false
            };
            // A click without a drag never selects: during auto-scroll the
            // content row under a stationary cursor moves between press and
            // release, which would otherwise invent a spanned selection.
            if empty || !dragged {
                app.selection = None;
            }
            true
        }
        _ => false,
    }
}

pub(super) fn surface_word_selection(
    app: &App,
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
    let row = point.1.saturating_sub(surface.area.y) as usize + surface.scroll_origin;
    Some(new_surface_selection(
        app,
        (surface.area.x + start as u16, row),
        (surface.area.x + end as u16, row),
    ))
}

pub(super) fn copy_current_surface(app: &mut App) {
    let Some(surface_overlay) = app.selectable.as_ref().map(|surface| surface.overlay) else {
        app.set_flash("Nothing visible can be copied");
        return;
    };
    let text = if let Some(selection) = app.selection {
        if surface_overlay.is_none() && selection.anchors.is_some() {
            transcript_selection_text(app, selection)
        } else {
            app.selectable.as_ref().map_or_else(String::new, |surface| {
                selected_surface_text(surface, selection)
            })
        }
    } else {
        app.selectable
            .as_ref()
            .map_or_else(String::new, complete_surface_text)
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
    let relative = |point: (u16, usize)| {
        (
            point.0.saturating_sub(surface.area.x) as usize,
            point.1.saturating_sub(surface.scroll_origin),
        )
    };
    let (start, end) = ordered_selection_points(selection);
    let (start_x, start_y) = relative(start);
    let (end_x, end_y) = relative(end);
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

/// Extract the selected transcript text by materializing rows straight from
/// the layout cache, so the selection may extend far beyond the visible
/// viewport. Row slicing mirrors `selected_surface_text` on the rendered
/// surface: column 0 is the list padding, soft-wrapped continuation rows skip
/// leading whitespace, and row separators follow the wrapped layout.
pub(super) fn transcript_selection_text(app: &mut App, selection: TextSelection) -> String {
    const CHUNK_ROWS: usize = 8192;
    let Some(surface_x) = app.selectable.as_ref().map(|surface| surface.area.x) else {
        return String::new();
    };
    let (start, end) = ordered_selection_points(selection);
    let message_width = app.transcript_layout.message_width;
    let process_width = app.transcript_layout.process_width;
    if message_width == 0 || process_width == 0 || end.1 < start.1 {
        return String::new();
    }
    let start_column = start.0.saturating_sub(surface_x) as usize;
    let end_column = end.0.saturating_sub(surface_x) as usize;
    let active_block = app.active_transcript_block();
    let mut text = String::new();
    let mut previous_separator = TextRowSeparator::Newline;
    let mut row = start.1;
    while row <= end.1 {
        let chunk_end = end.1.min(row.saturating_add(CHUNK_ROWS - 1));
        let rows = materialize_transcript_rows(
            app,
            row,
            chunk_end - row + 1,
            message_width,
            process_width,
            active_block,
        );
        if rows.is_empty() {
            break;
        }
        for (offset, materialized) in rows.into_iter().enumerate() {
            let row_index = row + offset;
            let line_text: String = materialized
                .line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            let from = if row_index == start.1 {
                start_column
            } else if previous_separator != TextRowSeparator::Newline {
                first_content_column(&line_text) + 1
            } else {
                1
            };
            let to = if row_index == end.1 {
                end_column + 1
            } else {
                usize::MAX
            };
            // The list's horizontal padding shifts line text one column right.
            text.push_str(
                slice_display_columns(&line_text, from.saturating_sub(1), to.saturating_sub(1))
                    .trim_end(),
            );
            if row_index < end.1 {
                push_row_separator(&mut text, materialized.separator);
            }
            previous_separator = materialized.separator;
        }
        row = chunk_end + 1;
    }
    text
}

/// Slice `text` to the display column range `[from, to)`, matching the buffer
/// cell model: a grapheme is included when the column of its first cell falls
/// inside the range.
fn slice_display_columns(text: &str, from: usize, to: usize) -> &str {
    let mut column = 0usize;
    let mut slice_start = None;
    let mut slice_end = 0usize;
    for (byte, grapheme) in text.grapheme_indices(true) {
        if from <= column && column < to {
            slice_start.get_or_insert(byte);
            slice_end = byte + grapheme.len();
        }
        column += UnicodeWidthStr::width(grapheme);
    }
    slice_start.map_or("", |start| &text[start..slice_end.max(start)])
}

fn first_content_column(text: &str) -> usize {
    let mut column = 0;
    for grapheme in text.graphemes(true) {
        if !grapheme.chars().all(char::is_whitespace) {
            return column;
        }
        column += UnicodeWidthStr::width(grapheme);
    }
    column
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

/// Compact list rows put secondary columns on an indented second line rather
/// than truncating every column into one.
pub(super) fn list_row_height(compact: bool) -> u16 {
    if compact { 2 } else { 1 }
}

/// The marker span opening every selectable list row.
fn selection_marker(selected: bool) -> Span<'static> {
    Span::styled(
        if selected { "› " } else { "  " },
        Style::default().fg(ACCENT),
    )
}

/// Row background: the selected row highlights.
fn selected_row_style(selected: bool) -> Style {
    Style::default().bg(if selected { ROW_ACTIVE } else { SURFACE })
}

pub(super) fn compact_list_item(
    selected: bool,
    primary: Vec<Span<'static>>,
    secondary: Vec<Span<'static>>,
) -> ListItem<'static> {
    let mut first = vec![selection_marker(selected)];
    first.extend(primary);
    let mut second = vec![Span::raw("  ")];
    second.extend(secondary);
    ListItem::new(vec![Line::from(first), Line::from(second)]).style(selected_row_style(selected))
}

/// Renders a stateful selection list and registers one hit region per
/// visible row. Takes only the hit-region vector because the item iterators
/// borrow the rest of `app`.
#[allow(clippy::too_many_arguments)]
fn render_selection_list(
    frame: &mut Frame<'_>,
    hit_regions: &mut Vec<HitRegion<AppHit>>,
    items: impl IntoIterator<Item = ListItem<'static>>,
    area: Rect,
    selected: Option<usize>,
    count: usize,
    row_height: u16,
    target: impl FnMut(usize) -> Option<AppHit>,
) {
    let mut state = ListState::default().with_selected(selected);
    frame.render_stateful_widget(List::new(items), area, &mut state);
    push_list_hits(hit_regions, area, state.offset(), count, row_height, target);
}

/// Shared tab strip for tabbed panels: the active tab highlighted, one hit
/// region per tab while `interactive`.
fn render_tab_strip(
    frame: &mut Frame<'_>,
    app: &mut App,
    area: Rect,
    active: usize,
    labels: &[&'static str],
    interactive: bool,
    target: fn(usize) -> AppHit,
) {
    let mut spans = Vec::new();
    let mut x = area.x;
    for (index, label) in labels.iter().enumerate() {
        let label = format!(" {label} ");
        let width = label.width() as u16;
        let active_tab = index == active;
        spans.push(Span::styled(
            label,
            Style::default()
                .fg(if active_tab { ACCENT } else { MUTED })
                .bg(if active_tab { ROW_ACTIVE } else { SURFACE })
                .add_modifier(if active_tab {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ));
        if interactive {
            app.hit_regions.push(HitRegion {
                area: Rect::new(x, area.y, width, 1),
                target: target(index),
            });
        }
        x = x.saturating_add(width);
        spans.push(Span::raw("  "));
        x = x.saturating_add(2);
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Register one click target per visible list row, `row_height` cells tall.
fn push_list_hits(
    hit_regions: &mut Vec<HitRegion<AppHit>>,
    area: Rect,
    offset: usize,
    count: usize,
    row_height: u16,
    mut target: impl FnMut(usize) -> Option<AppHit>,
) {
    for index in offset..count {
        let y = area
            .y
            .saturating_add(((index - offset) as u16).saturating_mul(row_height));
        if y >= area.bottom() {
            break;
        }
        if let Some(target) = target(index) {
            hit_regions.push(HitRegion {
                area: Rect::new(area.x, y, area.width, row_height.min(area.bottom() - y)),
                target,
            });
        }
    }
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
