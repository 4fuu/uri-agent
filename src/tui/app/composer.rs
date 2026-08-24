use super::*;

pub(super) fn image_token_label(id: u64) -> String {
    format!("{IMAGE_TOKEN_PREFIX}{id}]")
}

pub(super) fn numbered_image_spans(
    line: &str,
    prefix: &'static str,
    closing: Option<u8>,
) -> Vec<ImageTokenSpan> {
    line.match_indices(prefix)
        .filter_map(|(start_byte, _)| {
            let digits_start = start_byte + prefix.len();
            let digits = line[digits_start..]
                .bytes()
                .take_while(u8::is_ascii_digit)
                .count();
            if digits == 0 {
                return None;
            }
            let mut end_byte = digits_start + digits;
            if let Some(closing) = closing {
                if line.as_bytes().get(end_byte) != Some(&closing) {
                    return None;
                }
                end_byte += 1;
            }
            let id = line[digits_start..digits_start + digits].parse().ok()?;
            Some(ImageTokenSpan {
                id,
                start_byte,
                end_byte,
                start_col: line[..start_byte].chars().count(),
                end_col: line[..end_byte].chars().count(),
            })
        })
        .collect()
}

pub(super) fn image_token_spans(line: &str) -> Vec<ImageTokenSpan> {
    numbered_image_spans(line, IMAGE_TOKEN_PREFIX, Some(b']'))
}

pub(super) fn image_marker_spans(line: &str) -> Vec<ImageTokenSpan> {
    numbered_image_spans(line, IMAGE_MARKER_PREFIX, Some(b']'))
}

pub(super) fn rewrite_image_references(
    text: &str,
    ids: &BTreeMap<u64, u64>,
    spans: fn(&str) -> Vec<ImageTokenSpan>,
    marker: bool,
) -> String {
    text.split('\n')
        .map(|line| {
            let mut rewritten = String::with_capacity(line.len());
            let mut cursor = 0;
            for token in spans(line) {
                let Some(id) = ids.get(&token.id) else {
                    continue;
                };
                rewritten.push_str(&line[cursor..token.start_byte]);
                if marker {
                    rewritten.push_str(&format!("[Image #{id}]"));
                } else {
                    rewritten.push_str(&image_token_label(*id));
                }
                cursor = token.end_byte;
            }
            rewritten.push_str(&line[cursor..]);
            rewritten
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn rewrite_image_token_ids(text: &str, ids: &BTreeMap<u64, u64>) -> String {
    rewrite_image_references(text, ids, image_token_spans, false)
}

pub(super) fn strip_image_references_with(
    text: &str,
    spans: fn(&str) -> Vec<ImageTokenSpan>,
) -> String {
    text.split('\n')
        .map(|line| {
            let mut stripped = String::with_capacity(line.len());
            let mut cursor = 0;
            for token in spans(line) {
                stripped.push_str(&line[cursor..token.start_byte]);
                cursor = token.end_byte;
            }
            stripped.push_str(&line[cursor..]);
            stripped
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn legacy_image_token_spans(line: &str) -> Vec<ImageTokenSpan> {
    numbered_image_spans(line, LEGACY_IMAGE_TOKEN_PREFIX, None)
}

pub(super) fn strip_image_references(text: &str) -> String {
    strip_image_references_with(
        &strip_image_references_with(
            &strip_image_references_with(text, image_token_spans),
            image_marker_spans,
        ),
        legacy_image_token_spans,
    )
}

pub(super) fn collapse_image_markers(text: &str, image_count: usize) -> String {
    let ids = (1..=image_count as u64)
        .map(|id| (id, id))
        .collect::<BTreeMap<_, _>>();
    rewrite_image_references(text, &ids, image_marker_spans, false)
}

pub(super) fn ensure_image_markers(text: &str, image_count: usize) -> String {
    let present = text
        .split('\n')
        .flat_map(image_marker_spans)
        .filter_map(|token| usize::try_from(token.id).ok())
        .filter(|id| *id <= image_count)
        .collect::<HashSet<_>>();
    let missing = (1..=image_count)
        .filter(|id| !present.contains(id))
        .map(|id| format!("[Image #{id}]"))
        .collect::<Vec<_>>()
        .join(" ");
    match (text.trim().is_empty(), missing.is_empty()) {
        (_, true) => text.to_string(),
        (true, false) => missing,
        (false, false) => format!("{text} {missing}"),
    }
}

pub(super) fn prepare_image_references(
    text: &str,
    image_store: &BTreeMap<u64, ImageAttachment>,
) -> (BTreeMap<u64, u64>, Vec<ImageAttachment>) {
    let mut ids = BTreeMap::new();
    let mut images = Vec::new();
    for line in text.split('\n') {
        for token in image_token_spans(line) {
            if ids.contains_key(&token.id) {
                continue;
            }
            let Some(image) = image_store.get(&token.id) else {
                continue;
            };
            let id = images.len() as u64 + 1;
            ids.insert(token.id, id);
            images.push(image.clone());
        }
    }
    (ids, images)
}

pub(super) fn prepare_composer_images(
    text: &str,
    image_store: &BTreeMap<u64, ImageAttachment>,
) -> (String, Vec<ImageAttachment>) {
    let (ids, images) = prepare_image_references(text, image_store);
    (rewrite_image_token_ids(text, &ids), images)
}

pub(super) fn prepare_image_submission(
    text: &str,
    image_store: &BTreeMap<u64, ImageAttachment>,
) -> (String, Vec<ImageAttachment>) {
    let (ids, images) = prepare_image_references(text, image_store);
    (
        rewrite_image_references(text, &ids, image_token_spans, true),
        images,
    )
}

pub(super) fn composer_key_edits(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Backspace | KeyCode::Delete)
        || matches!(key.code, KeyCode::Char(_))
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
}

pub(super) fn select_composer_range(
    input: &mut TextArea<'static>,
    start: (usize, usize),
    end: (usize, usize),
) {
    input.cancel_selection();
    input.move_cursor(CursorMove::Jump(
        end.0.min(u16::MAX as usize) as u16,
        end.1.min(u16::MAX as usize) as u16,
    ));
    input.start_selection();
    input.move_cursor(CursorMove::Jump(
        start.0.min(u16::MAX as usize) as u16,
        start.1.min(u16::MAX as usize) as u16,
    ));
}

pub(super) fn expand_composer_selection_to_image_tokens(
    input: &mut TextArea<'static>,
    image_store: &BTreeMap<u64, ImageAttachment>,
) -> bool {
    let Some((mut start, mut end)) = input.selection_range().filter(|(start, end)| start != end)
    else {
        return false;
    };
    let original = (start, end);
    for (row, line) in input.lines().iter().enumerate() {
        for token in image_token_spans(line)
            .into_iter()
            .filter(|token| image_store.contains_key(&token.id))
        {
            let token_start = (row, token.start_col);
            let token_end = (row, token.end_col);
            if start < token_end && token_start < end {
                start = start.min(token_start);
                end = end.max(token_end);
            }
        }
    }
    if (start, end) != original {
        select_composer_range(input, start, end);
    }
    true
}

pub(super) fn delete_adjacent_image_token(
    input: &mut TextArea<'static>,
    image_store: &BTreeMap<u64, ImageAttachment>,
    forward: bool,
) -> bool {
    let (row, column) = input.cursor();
    let Some(token) = image_token_spans(&input.lines()[row])
        .into_iter()
        .filter(|token| image_store.contains_key(&token.id))
        .find(|token| {
            if forward {
                token.start_col == column || token.start_col < column && column < token.end_col
            } else {
                token.end_col == column || token.start_col < column && column < token.end_col
            }
        })
    else {
        return false;
    };
    select_composer_range(input, (row, token.start_col), (row, token.end_col));
    input.insert_str("")
}

pub(super) fn snap_composer_cursor(
    input: &mut TextArea<'static>,
    image_store: &BTreeMap<u64, ImageAttachment>,
    direction: CursorSnap,
) {
    let (row, column) = input.cursor();
    let Some(token) = image_token_spans(&input.lines()[row])
        .into_iter()
        .filter(|token| image_store.contains_key(&token.id))
        .find(|token| token.start_col < column && column < token.end_col)
    else {
        return;
    };
    let column = match direction {
        CursorSnap::Backward => token.start_col,
        CursorSnap::Forward => token.end_col,
        CursorSnap::Nearest => {
            if column - token.start_col < token.end_col - column {
                token.start_col
            } else {
                token.end_col
            }
        }
    };
    input.move_cursor(CursorMove::Jump(
        row.min(u16::MAX as usize) as u16,
        column.min(u16::MAX as usize) as u16,
    ));
}
