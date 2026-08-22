use super::{ACCENT, MUTED, SURFACE, TEXT};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(super) fn render(source: &str, width: usize) -> Vec<Line<'static>> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_TASKLISTS);
    let mut writer = Writer::new(width.max(1));
    for event in Parser::new_ext(source, options) {
        writer.event(event);
    }
    writer.finish()
}

struct ListState {
    next: Option<u64>,
    marker_width: usize,
}

#[derive(Default)]
struct TableState {
    header: Vec<String>,
    rows: Vec<Vec<String>>,
    current_row: Vec<String>,
    current_cell: String,
    in_header: bool,
}

struct Writer {
    width: usize,
    lines: Vec<Line<'static>>,
    spans: Vec<Span<'static>>,
    line_open: bool,
    line_width: usize,
    prefix_width: usize,
    line_style: Style,
    styles: Vec<Style>,
    links: Vec<String>,
    blockquotes: usize,
    lists: Vec<ListState>,
    pending_marker: Option<String>,
    needs_blank: bool,
    in_code_block: bool,
    code_language: Option<String>,
    table: Option<TableState>,
}

impl Writer {
    fn new(width: usize) -> Self {
        Self {
            width,
            lines: Vec::new(),
            spans: Vec::new(),
            line_open: false,
            line_width: 0,
            prefix_width: 0,
            line_style: Style::default(),
            styles: vec![Style::default().fg(TEXT)],
            links: Vec::new(),
            blockquotes: 0,
            lists: Vec::new(),
            pending_marker: None,
            needs_blank: false,
            in_code_block: false,
            code_language: None,
            table: None,
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush_line();
        while self.lines.last().is_some_and(|line| line.width() == 0) {
            self.lines.pop();
        }
        if self.lines.is_empty() {
            self.lines.push(Line::default());
        }
        self.lines
    }

    fn event(&mut self, event: Event<'_>) {
        if self.table.is_some() && self.table_event(&event) {
            return;
        }
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => {
                if self.in_code_block {
                    self.code_text(&text);
                } else {
                    self.text(&text, self.style());
                }
            }
            Event::Code(code) => {
                let style = self.style().patch(Style::default().fg(ACCENT).bg(SURFACE));
                self.text(&code, style);
            }
            Event::SoftBreak => self.text(" ", self.style()),
            Event::HardBreak => self.flush_line(),
            Event::Rule => {
                self.start_block();
                self.text(&"─".repeat(self.width.min(24)), Style::default().fg(MUTED));
                self.flush_line();
                self.needs_blank = true;
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                self.text(&html, Style::default().fg(MUTED));
            }
            Event::FootnoteReference(label) => {
                self.text(&format!("[{label}]"), Style::default().fg(MUTED));
            }
            Event::TaskListMarker(done) => {
                let marker = if done { "☑ " } else { "☐ " };
                if !self.line_open && self.pending_marker.is_some() {
                    self.pending_marker = Some(marker.to_string());
                    if let Some(list) = self.lists.last_mut() {
                        list.marker_width = marker.width();
                    }
                } else {
                    self.text(marker, Style::default().fg(ACCENT));
                }
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.start_block(),
            Tag::Heading { level, .. } => {
                self.start_block();
                let style = heading_style(level);
                self.push_style(style);
                self.text(&format!("{} ", "#".repeat(level as usize)), style);
            }
            Tag::BlockQuote => {
                self.start_block();
                self.blockquotes += 1;
            }
            Tag::CodeBlock(kind) => {
                self.start_block();
                self.in_code_block = true;
                self.code_language = match kind {
                    CodeBlockKind::Fenced(language) if !language.is_empty() => {
                        Some(language.into_string())
                    }
                    _ => None,
                };
            }
            Tag::List(next) => {
                if !self.lists.is_empty() {
                    self.flush_line();
                }
                self.lists.push(ListState {
                    next,
                    marker_width: 0,
                });
            }
            Tag::Item => {
                let marker = if let Some(list) = self.lists.last_mut() {
                    match list.next.as_mut() {
                        Some(next) => {
                            let marker = format!("{next}. ");
                            *next += 1;
                            marker
                        }
                        None => "• ".to_string(),
                    }
                } else {
                    "• ".to_string()
                };
                if let Some(list) = self.lists.last_mut() {
                    list.marker_width = marker.width();
                }
                self.pending_marker = Some(marker);
            }
            Tag::Emphasis => self.push_style(Style::default().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.push_style(Style::default().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => {
                self.push_style(Style::default().add_modifier(Modifier::CROSSED_OUT));
            }
            Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                self.links.push(dest_url.into_string());
                self.push_style(
                    Style::default()
                        .fg(ACCENT)
                        .add_modifier(Modifier::UNDERLINED),
                );
            }
            Tag::Table(_) => {
                self.start_block();
                self.table = Some(TableState::default());
            }
            Tag::TableHead => {
                if let Some(table) = self.table.as_mut() {
                    table.in_header = true;
                }
            }
            Tag::TableRow | Tag::TableCell => {}
            Tag::FootnoteDefinition(_) | Tag::HtmlBlock | Tag::MetadataBlock(_) => {
                self.start_block();
            }
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush_line();
                self.needs_blank = true;
            }
            TagEnd::Heading(_) => {
                self.flush_line();
                self.pop_style();
                self.needs_blank = true;
            }
            TagEnd::BlockQuote => {
                self.flush_line();
                self.blockquotes = self.blockquotes.saturating_sub(1);
                self.needs_blank = true;
            }
            TagEnd::CodeBlock => {
                self.flush_line();
                self.in_code_block = false;
                self.code_language = None;
                self.line_style = Style::default();
                self.needs_blank = true;
            }
            TagEnd::List(_) => {
                self.flush_line();
                self.lists.pop();
                self.pending_marker = None;
                self.needs_blank = self.lists.is_empty();
            }
            TagEnd::Item => {
                self.flush_line();
                self.pending_marker = None;
                if let Some(list) = self.lists.last_mut() {
                    list.marker_width = 0;
                }
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => self.pop_style(),
            TagEnd::Link | TagEnd::Image => {
                self.pop_style();
                if let Some(destination) = self.links.pop()
                    && !destination.is_empty()
                {
                    self.text(&format!(" ({destination})"), Style::default().fg(MUTED));
                }
            }
            TagEnd::Table => self.render_table(),
            TagEnd::TableHead | TagEnd::TableRow | TagEnd::TableCell => {}
            TagEnd::FootnoteDefinition | TagEnd::HtmlBlock | TagEnd::MetadataBlock(_) => {
                self.flush_line();
                self.needs_blank = true;
            }
        }
    }

    fn table_event(&mut self, event: &Event<'_>) -> bool {
        match event {
            Event::Start(Tag::TableHead) => {
                self.table.as_mut().unwrap().in_header = true;
            }
            Event::Start(Tag::TableRow) => {
                self.table.as_mut().unwrap().current_row.clear();
            }
            Event::Start(Tag::TableCell) => {
                self.table.as_mut().unwrap().current_cell.clear();
            }
            Event::Text(text) | Event::Code(text) => {
                self.table.as_mut().unwrap().current_cell.push_str(text);
            }
            Event::SoftBreak | Event::HardBreak => {
                self.table.as_mut().unwrap().current_cell.push(' ');
            }
            Event::End(TagEnd::TableCell) => {
                let table = self.table.as_mut().unwrap();
                let cell = table
                    .current_cell
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                table.current_row.push(cell);
            }
            Event::End(TagEnd::TableHead) => {
                let table = self.table.as_mut().unwrap();
                table.header = std::mem::take(&mut table.current_row);
                table.in_header = false;
            }
            Event::End(TagEnd::TableRow) => {
                let table = self.table.as_mut().unwrap();
                if table.in_header {
                    table.header = std::mem::take(&mut table.current_row);
                } else if !table.current_row.is_empty() {
                    table.rows.push(std::mem::take(&mut table.current_row));
                }
            }
            Event::End(TagEnd::Table) => return false,
            _ => {}
        }
        true
    }

    fn render_table(&mut self) {
        let Some(table) = self.table.take() else {
            return;
        };
        let columns = table
            .rows
            .iter()
            .map(Vec::len)
            .chain(std::iter::once(table.header.len()))
            .max()
            .unwrap_or(0);
        if columns == 0 {
            return;
        }
        let mut widths = vec![0; columns];
        for row in std::iter::once(&table.header).chain(table.rows.iter()) {
            for (index, cell) in row.iter().enumerate() {
                widths[index] = widths[index].max(cell.width());
            }
        }
        let table_width = widths.iter().sum::<usize>() + columns.saturating_sub(1) * 3;
        if table_width <= self.width {
            if !table.header.is_empty() {
                self.table_row(&table.header, &widths, true);
                self.text(
                    &widths
                        .iter()
                        .map(|width| "─".repeat(*width))
                        .collect::<Vec<_>>()
                        .join("─┼─"),
                    Style::default().fg(MUTED),
                );
                self.flush_line();
            }
            for row in &table.rows {
                self.table_row(row, &widths, false);
            }
        } else {
            for (row_index, row) in table.rows.iter().enumerate() {
                if row_index > 0 {
                    self.blank_line();
                }
                for (index, value) in row.iter().enumerate() {
                    let label = table
                        .header
                        .get(index)
                        .filter(|label| !label.is_empty())
                        .map_or_else(|| format!("Column {}", index + 1), Clone::clone);
                    self.text(
                        &format!("{label}: "),
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                    );
                    self.text(value, Style::default().fg(TEXT));
                    self.flush_line();
                }
            }
        }
        self.needs_blank = true;
    }

    fn table_row(&mut self, row: &[String], widths: &[usize], header: bool) {
        self.start_line();
        for (index, width) in widths.iter().enumerate() {
            if index > 0 {
                self.push_span(" │ ", Style::default().fg(MUTED));
            }
            let value = row.get(index).map_or("", String::as_str);
            let style = if header {
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(TEXT)
            };
            self.push_span(value, style);
            self.push_span(
                &" ".repeat(width.saturating_sub(value.width())),
                Style::default(),
            );
        }
        self.flush_line();
    }

    fn start_block(&mut self) {
        if self.needs_blank && self.lists.is_empty() {
            self.blank_line();
        }
        self.needs_blank = false;
    }

    fn blank_line(&mut self) {
        self.flush_line();
        if self.lines.last().is_some_and(|line| line.width() > 0) {
            self.lines.push(Line::default());
        }
    }

    fn style(&self) -> Style {
        self.styles.last().copied().unwrap_or_default()
    }

    fn push_style(&mut self, style: Style) {
        self.styles.push(self.style().patch(style));
    }

    fn pop_style(&mut self) {
        if self.styles.len() > 1 {
            self.styles.pop();
        }
    }

    fn start_line(&mut self) {
        if self.line_open {
            return;
        }
        self.line_open = true;
        self.line_style = if self.in_code_block {
            Style::default().bg(SURFACE)
        } else {
            Style::default()
        };
        for _ in 0..self.blockquotes {
            self.push_span("│ ", Style::default().fg(MUTED));
        }
        let nested_indent = self.lists.len().saturating_sub(1) * 2;
        if nested_indent > 0 {
            self.push_span(&" ".repeat(nested_indent), Style::default());
        }
        if let Some(marker) = self.pending_marker.take() {
            self.push_span(&marker, Style::default().fg(ACCENT));
        } else if let Some(marker_width) = self
            .lists
            .last()
            .map(|list| list.marker_width)
            .filter(|width| *width > 0)
        {
            self.push_span(&" ".repeat(marker_width), Style::default());
        }
        if self.in_code_block {
            self.push_span("  ", Style::default().bg(SURFACE));
        }
        self.prefix_width = self.line_width;
    }

    fn flush_line(&mut self) {
        if !self.line_open {
            return;
        }
        self.lines
            .push(Line::from(std::mem::take(&mut self.spans)).style(self.line_style));
        self.line_open = false;
        self.line_width = 0;
        self.prefix_width = 0;
        self.line_style = Style::default();
    }

    fn text(&mut self, text: &str, style: Style) {
        for (line_index, logical) in text.split('\n').enumerate() {
            if line_index > 0 {
                self.flush_line();
            }
            let mut token = String::new();
            let mut whitespace = None;
            for character in logical.chars().chain(std::iter::once('\0')) {
                let kind = character.is_whitespace();
                if let Some(previous) = whitespace
                    && (kind != previous || character == '\0')
                {
                    self.push_token(&token, previous, style);
                    token.clear();
                }
                if character != '\0' {
                    token.push(character);
                    whitespace = Some(kind);
                }
            }
        }
    }

    fn push_token(&mut self, token: &str, whitespace: bool, style: Style) {
        if token.is_empty() {
            return;
        }
        self.start_line();
        if whitespace {
            if self.line_width > self.prefix_width && self.line_width < self.width {
                self.push_span(" ", style);
            }
            return;
        }
        let token_width = token.width();
        if self.line_width > self.prefix_width && self.line_width + token_width > self.width {
            self.flush_line();
            self.start_line();
        }
        for character in token.chars() {
            let character_width = character.width().unwrap_or(0);
            if self.line_width > self.prefix_width && self.line_width + character_width > self.width
            {
                self.flush_line();
                self.start_line();
            }
            self.push_span(&character.to_string(), style);
        }
    }

    fn push_span(&mut self, content: &str, style: Style) {
        if content.is_empty() {
            return;
        }
        self.line_width += content.width();
        if let Some(last) = self.spans.last_mut()
            && last.style == style
        {
            last.content.to_mut().push_str(content);
        } else {
            self.spans.push(Span::styled(content.to_string(), style));
        }
    }

    fn code_text(&mut self, text: &str) {
        if let Some(language) = self.code_language.take() {
            self.start_line();
            self.push_span(
                &language,
                Style::default()
                    .fg(MUTED)
                    .bg(SURFACE)
                    .add_modifier(Modifier::ITALIC),
            );
            self.flush_line();
        }
        for (index, line) in text.split_terminator('\n').enumerate() {
            if index > 0 {
                self.flush_line();
            }
            self.start_line();
            for character in line.chars() {
                let character_width = character.width().unwrap_or(0);
                if self.line_width > self.prefix_width
                    && self.line_width + character_width > self.width
                {
                    self.flush_line();
                    self.start_line();
                }
                self.push_span(
                    &character.to_string(),
                    Style::default().fg(TEXT).bg(SURFACE),
                );
            }
        }
    }
}

fn heading_style(level: HeadingLevel) -> Style {
    let color = if matches!(level, HeadingLevel::H1 | HeadingLevel::H2) {
        ACCENT
    } else {
        TEXT
    };
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_common_markdown_structure_and_styles() {
        let lines = render(
            "# Heading\n\nA **bold** and *soft* [link](https://example.com).\n\n- one\n- `two`\n\n> quoted\n\n```rust\nfn main() {}\n```",
            60,
        );
        let text = lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("# Heading"));
        assert!(text.contains("• one"));
        assert!(text.contains("link (https://example.com)"));
        assert!(text.contains("│ quoted"));
        assert!(text.contains("rust"));
        assert!(text.contains("fn main() {}"));
        assert!(lines.iter().flat_map(|line| &line.spans).any(|span| {
            span.content == "bold" && span.style.add_modifier.contains(Modifier::BOLD)
        }));
    }

    #[test]
    fn renders_tables_or_responsive_records() {
        let source = "| Name | Result |\n| --- | --- |\n| tests | passed |";
        let wide = render(source, 40)
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(wide.contains("Name"));
        assert!(wide.contains("tests │ passed"));

        let narrow = render(source, 10)
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(narrow.contains("Name:"));
        assert!(narrow.contains("tests"));
        assert!(narrow.contains("Result:"));
        assert!(narrow.contains("passed"));
    }

    #[test]
    fn wraps_unicode_without_exceeding_the_requested_width() {
        let lines = render("你好世界 **abcdef**", 8);
        assert!(lines.iter().all(|line| line.width() <= 8));
    }

    #[test]
    fn task_and_nested_list_markers_stay_compact() {
        let text = render("- [x] done\n- parent\n    1. child\n- next", 40)
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("☑ done"));
        assert!(!text.contains("• ☑"));
        assert!(text.contains("  1. child"), "{text}");
        assert!(text.contains("• next"));
    }
}
