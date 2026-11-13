//! Rendering. Two surfaces only: the buffer, and the query line with the
//! dropup that grows out of it.

use std::collections::HashSet;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use crate::app::{App, Focus};
use crate::query::Mode;
use crate::syntax::Segment;

const TAB_WIDTH: usize = 4;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    if area.width == 0 || area.height < 3 {
        return;
    }

    let picking = app.focus == Focus::Picker;
    let items = if picking { app.visible_len() } else { 0 };
    let max_drop = area.height.saturating_sub(4).min(14).max(1);
    // One row stays reserved for feedback ("searching…", "no files match") as
    // soon as there is something to report; an untouched empty query keeps the
    // screen to the splash and the prompt.
    let wants_feedback = !app.query.is_empty() || app.query.mode != Mode::Files;
    let drop_h = if !picking {
        0
    } else if wants_feedback {
        (items as u16).min(max_drop).max(1)
    } else {
        (items as u16).min(max_drop)
    };

    let chunks = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(drop_h),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(area);

    let mut cursor: Option<Position> = draw_buffer(frame, app, chunks[0]);

    if drop_h > 0 {
        draw_results(frame, app, chunks[1]);
    }

    let query_x = draw_query(frame, app, chunks[2]);
    if picking {
        cursor = Some(Position::new(query_x, chunks[2].y));
    }

    draw_status(frame, app, chunks[3]);

    if let Some(pos) = cursor {
        let max_y = area.y + area.height - 1;
        frame.set_cursor_position(Position::new(
            pos.x.min(area.x + area.width - 1),
            pos.y.min(max_y),
        ));
    }
}

fn draw_buffer(frame: &mut Frame, app: &mut App, area: Rect) -> Option<Position> {
    if app.buffer.path.is_none() {
        draw_splash(frame, area);
        return None;
    }

    let total = app.buffer.lines.len();
    let num_w = total.to_string().len().max(3);
    // Exactly as wide as `format!("{:>num_w$} ")` below: digits plus one space.
    // The cursor is placed at `area.x + gutter + column`, so any slack here
    // shows up as the caret sitting one cell off the text.
    let gutter = (num_w + 1) as u16;
    let text_w = area.width.saturating_sub(gutter) as usize;
    let height = area.height as usize;
    if text_w == 0 || height == 0 {
        return None;
    }

    if app.buffer.row < app.buffer.scroll {
        app.buffer.scroll = app.buffer.row;
    }
    if app.buffer.row >= app.buffer.scroll + height {
        app.buffer.scroll = app.buffer.row + 1 - height;
    }

    let cursor_col = display_col(&app.buffer.lines[app.buffer.row], app.buffer.col);
    if cursor_col < app.buffer.hscroll {
        app.buffer.hscroll = cursor_col;
    }
    if cursor_col >= app.buffer.hscroll + text_w {
        app.buffer.hscroll = cursor_col + 1 - text_w;
    }

    let mut out: Vec<Line> = Vec::new();
    for offset in 0..height {
        let idx = app.buffer.scroll + offset;
        if idx >= total {
            break;
        }
        let current = idx == app.buffer.row;
        let number_style = if current {
            Style::default().fg(Color::Gray)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let number = format!("{:>num_w$} ", idx + 1);
        let mut spans = vec![Span::styled(number, number_style)];
        match app.highlight.get(idx) {
            Some(segments) if !segments.is_empty() => {
                spans.extend(segment_spans(segments, app.buffer.hscroll, text_w));
            }
            _ => {
                let visible: String = expand_tabs(&app.buffer.lines[idx])
                    .chars()
                    .skip(app.buffer.hscroll)
                    .take(text_w)
                    .collect();
                spans.push(Span::raw(visible));
            }
        }
        out.push(Line::from(spans));
    }

    frame.render_widget(Paragraph::new(out), area);

    if app.focus == Focus::Editor {
        let x = area.x + gutter + (cursor_col.saturating_sub(app.buffer.hscroll)) as u16;
        let y = area.y + (app.buffer.row - app.buffer.scroll) as u16;
        return Some(Position::new(x, y));
    }
    None
}

fn draw_splash(frame: &mut Frame, area: Rect) {
    let dim = Style::default().fg(Color::DarkGray);
    let accent = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);

    let lines = vec![
        Line::from(""),
        Line::from(Span::styled("peek", accent)),
        Line::from(""),
        Line::from(Span::styled("type to find a file", dim)),
        Line::from(""),
        Line::from(vec![
            Span::styled("  index        ", Style::default().fg(Color::Gray)),
            Span::styled("fuzzy path match", dim),
        ]),
        Line::from(vec![
            Span::styled("  \"literal\"    ", Style::default().fg(Color::Gray)),
            Span::styled("exact substring", dim),
        ]),
        Line::from(vec![
            Span::styled("  /regex/      ", Style::default().fg(Color::Gray)),
            Span::styled("pattern", dim),
        ]),
        Line::from(vec![
            Span::styled("  auth/ tok    ", Style::default().fg(Color::Gray)),
            Span::styled("scoped to a directory", dim),
        ]),
        Line::from(vec![
            Span::styled("  :l rehab     ", Style::default().fg(Color::Gray)),
            Span::styled("find a line in this file", dim),
        ]),
        Line::from(vec![
            Span::styled("  :f token     ", Style::default().fg(Color::Gray)),
            Span::styled("search inside files", dim),
        ]),
        Line::from(vec![
            Span::styled("  !test        ", Style::default().fg(Color::Gray)),
            Span::styled("exclude", dim),
        ]),
    ];

    let height = (area.height as usize).min(lines.len()) as u16;
    let rect = Rect {
        y: area.y + area.height.saturating_sub(height) / 2,
        height,
        ..area
    };
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), rect);
}

fn draw_results(frame: &mut Frame, app: &App, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let total = app.visible_len();
    let height = area.height as usize;

    if total == 0 {
        let message = if let Some(error) = &app.query.error {
            error.clone()
        } else {
            match app.query.mode {
                Mode::Content => app
                    .content_msg
                    .clone()
                    .unwrap_or_else(|| "no matches".to_string()),
                Mode::Buffer => app
                    .line_msg
                    .clone()
                    .unwrap_or_else(|| "no matches in this file".to_string()),
                Mode::Files => {
                    if app.query.is_empty() {
                        "type a filename to open".to_string()
                    } else if app.files.is_empty() {
                        "indexing…".to_string()
                    } else {
                        "no files match".to_string()
                    }
                }
            }
        };
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("  {message}"),
                Style::default().fg(Color::DarkGray),
            ))),
            area,
        );
        return;
    }

    let start = app.selection.saturating_sub(height.saturating_sub(1));
    let width = area.width as usize;

    let mut lines: Vec<Line> = Vec::new();
    for idx in start..(start + height).min(total) {
        let selected = idx == app.selection;
        let mut line = match app.query.mode {
            Mode::Files => match app.results.get(idx) {
                Some(hit) => {
                    let marked: HashSet<u32> = hit.indices.iter().copied().collect();
                    Line::from(highlighted(
                        &hit.path,
                        |i| marked.contains(&i),
                        Style::default().fg(Color::Gray),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                        width,
                    ))
                }
                None => continue,
            },
            Mode::Content => match app.content.get(idx) {
                Some(hit) => snippet_row(
                    format!("{}:{}  ", hit.path, hit.line_no),
                    &hit.snippet,
                    &hit.ranges,
                    width,
                ),
                None => continue,
            },
            Mode::Buffer => match app.line_hits.get(idx) {
                Some(hit) => snippet_row(
                    format!("{:>5}  ", hit.line_no),
                    &hit.snippet,
                    &hit.ranges,
                    width,
                ),
                None => continue,
            },
        };

        if selected {
            for span in &mut line.spans {
                span.style = span.style.add_modifier(Modifier::REVERSED);
            }
        }
        lines.push(line);
    }

    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_query(frame: &mut Frame, app: &App, area: Rect) -> u16 {
    let accent = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);

    let spans = if app.query_input.is_empty() {
        vec![
            Span::styled("› ", accent),
            Span::styled(
                if app.buffer.path.is_some() {
                    "find a file…"
                } else {
                    "type a filename to open…   (:f inside files · :l this file)"
                },
                Style::default().fg(Color::DarkGray),
            ),
        ]
    } else {
        let mut spans = vec![Span::raw(app.query_input.clone())];
        if let Some(error) = &app.query.error {
            spans.push(Span::styled(
                format!("   {error}"),
                Style::default().fg(Color::Red),
            ));
        }
        spans
    };

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    // Cursor sits after the typed text; the prompt is two cells when empty.
    let used = if app.query_input.is_empty() {
        2
    } else {
        app.query_input.chars().count() as u16
    };
    area.x + used.min(area.width.saturating_sub(1))
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let hints = match app.focus {
        Focus::Picker => "enter open · up/down select · esc clear · ctrl+q quit",
        Focus::Editor => "esc find file · ctrl+s save · ctrl+z undo · alt+← back",
    };

    let right = if let Some(message) = &app.buffer.message {
        message.clone()
    } else if !app.status.is_empty() {
        app.status.clone()
    } else {
        match app.focus {
            Focus::Picker => {
                if app.files.is_empty() {
                    "indexing…".to_string()
                } else {
                    format!("{} files", app.files.len())
                }
            }
            Focus::Editor => format!(
                "{}{}  {}:{}",
                app.buffer.label(),
                if app.buffer.dirty { " ●" } else { "" },
                app.buffer.row + 1,
                app.buffer.col + 1
            ),
        }
    };

    let width = area.width as usize;
    let left_len = hints.chars().count();
    let right_len = right.chars().count();
    let pad = width.saturating_sub(left_len + right_len + 1);

    let right_style = if app.buffer.message.is_some() || !app.status.is_empty() {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(hints, Style::default().fg(Color::DarkGray)),
            Span::raw(" ".repeat(pad)),
            Span::styled(right, right_style),
        ])),
        area,
    );
}

fn snippet_row(head: String, snippet: &str, ranges: &[(u32, u32)], width: usize) -> Line<'static> {
    let mut spans = vec![Span::styled(
        head.clone(),
        Style::default().fg(Color::DarkGray),
    )];
    let room = width.saturating_sub(head.chars().count());
    spans.extend(highlighted(
        snippet,
        |i| ranges.iter().any(|(start, len)| i >= *start && i < *start + *len),
        Style::default(),
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        room,
    ));
    Line::from(spans)
}

fn highlighted(
    text: &str,
    marked: impl Fn(u32) -> bool,
    base: Style,
    hot: Style,
    width: usize,
) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buffer = String::new();
    let mut current = false;

    for (idx, ch) in text.chars().enumerate() {
        if idx >= width {
            break;
        }
        let want = marked(idx as u32);
        if want != current && !buffer.is_empty() {
            spans.push(Span::styled(
                std::mem::take(&mut buffer),
                if current { hot } else { base },
            ));
        }
        current = want;
        buffer.push(ch);
    }
    if !buffer.is_empty() {
        spans.push(Span::styled(buffer, if current { hot } else { base }));
    }
    spans
}

/// Render styled segments through the same column arithmetic as the caret:
/// tabs expand to the next multiple of `TAB_WIDTH`, and only the visible
/// window is emitted.
fn segment_spans(segments: &[Segment], hscroll: usize, width: usize) -> Vec<Span<'static>> {
    let end = hscroll.saturating_add(width);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut column = 0usize;

    for segment in segments {
        let mut buffer = String::new();
        for ch in segment.text.chars() {
            let step = if ch == '\t' {
                TAB_WIDTH - (column % TAB_WIDTH)
            } else {
                1
            };
            if column >= hscroll && column < end {
                if ch == '\t' {
                    buffer.push_str(&" ".repeat(step));
                } else {
                    buffer.push(ch);
                }
            }
            column += step;
            if column >= end {
                break;
            }
        }

        if !buffer.is_empty() {
            let mut style = Style::default().fg(segment.color);
            if segment.bold {
                style = style.add_modifier(Modifier::BOLD);
            }
            if segment.italic {
                style = style.add_modifier(Modifier::ITALIC);
            }
            spans.push(Span::styled(buffer, style));
        }
        if column >= end {
            break;
        }
    }

    spans
}

fn expand_tabs(line: &str) -> String {
    if !line.contains('\t') {
        return line.to_string();
    }
    let mut out = String::with_capacity(line.len());
    let mut col = 0;
    for ch in line.chars() {
        if ch == '\t' {
            let fill = TAB_WIDTH - (col % TAB_WIDTH);
            out.push_str(&" ".repeat(fill));
            col += fill;
        } else {
            out.push(ch);
            col += 1;
        }
    }
    out
}

fn display_col(line: &str, char_col: usize) -> usize {
    let mut col = 0;
    for (idx, ch) in line.chars().enumerate() {
        if idx >= char_col {
            break;
        }
        if ch == '\t' {
            col += TAB_WIDTH - (col % TAB_WIDTH);
        } else {
            col += 1;
        }
    }
    col
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn app_with_file(name: &str, text: &str) -> (App, std::path::PathBuf) {
        app_with_source(name, "a.txt", text)
    }

    fn app_with_source(name: &str, file: &str, text: &str) -> (App, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("peek-ui-{}-{name}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(file);
        fs::write(&path, text).unwrap();
        let mut app = App::new(dir.clone());
        app.buffer.open(&path).unwrap();
        app.pump();
        app.focus = Focus::Editor;
        (app, dir)
    }

    fn symbol_at(terminal: &Terminal<TestBackend>, x: u16, y: u16) -> String {
        terminal
            .backend()
            .buffer()
            .cell((x, y))
            .map(|cell| cell.symbol().to_string())
            .unwrap_or_default()
    }

    #[test]
    fn caret_sits_on_the_character_it_would_type_over() {
        let (mut app, dir) = app_with_file("caret", "hello world\n");
        let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let caret = terminal.get_cursor_position().unwrap();
        let row: String = (0..14).map(|x| symbol_at(&terminal, x, caret.y)).collect();
        assert_eq!(
            symbol_at(&terminal, caret.x, caret.y),
            "h",
            "caret at {caret:?} in row {row:?}"
        );

        // Same guarantee further into the line, i.e. the column arithmetic and
        // the rendered text agree everywhere, not just at column 0.
        app.buffer.col = 6;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let caret = terminal.get_cursor_position().unwrap();
        assert_eq!(symbol_at(&terminal, caret.x, caret.y), "w");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn caret_follows_the_query_text() {
        let (mut app, dir) = app_with_file("query", "hello\n");
        app.focus = Focus::Picker;
        app.query_input = "abc".into();
        app.query = crate::query::Query::parse("abc");

        let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let caret = terminal.get_cursor_position().unwrap();
        assert_eq!(symbol_at(&terminal, 0, caret.y), "a");
        assert_eq!(caret.x, 3, "caret should sit right after the typed text");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn highlighted_source_keeps_the_caret_aligned() {
        let (mut app, dir) = app_with_source(
            "highlight",
            "a.rs",
            "fn main() {\n    let answer = \"forty two\"; // note\n}\n",
        );
        assert!(
            !app.highlight.is_empty(),
            "a .rs file should be highlighted"
        );

        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        app.buffer.col = 3; // the 'm' of main
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let caret = terminal.get_cursor_position().unwrap();
        assert_eq!(symbol_at(&terminal, caret.x, caret.y), "m");

        // The styling has to reach the cells, not just the internal buffer.
        let colours: std::collections::HashSet<String> = (4..30)
            .filter_map(|x| {
                terminal
                    .backend()
                    .buffer()
                    .cell((x, 0))
                    .map(|cell| format!("{:?}", cell.fg))
            })
            .collect();
        assert!(
            colours.len() >= 2,
            "expected several syntax colours on the line, got {colours:?}"
        );

        // Second line is indented, which is where segment splicing is easy to
        // get off by one.
        app.buffer.row = 1;
        app.buffer.col = 8; // the 'a' of answer
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let caret = terminal.get_cursor_position().unwrap();
        assert_eq!(symbol_at(&terminal, caret.x, caret.y), "a");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn line_numbers_are_right_aligned_and_text_starts_after_them() {
        let (mut app, dir) = app_with_file("numbers", "one\ntwo\n");
        let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let numbers: String = (0..4).map(|x| symbol_at(&terminal, x, 0)).collect();
        assert_eq!(numbers, "  1 ");
        assert_eq!(symbol_at(&terminal, 4, 0), "o");

        fs::remove_dir_all(&dir).ok();
    }
}
