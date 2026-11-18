//! Rendering. Two surfaces only: the buffer, and the query line with the
//! dropup that grows out of it.

use std::collections::HashSet;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use crate::app::{App, Focus};
use crate::query::Mode;
use crate::syntax::Segment;
use crate::wrap::{self, TAB_WIDTH};

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    if area.width == 0 || area.height < 3 {
        return;
    }

    let picking = app.focus == Focus::Picker;
    let items = if picking { app.visible_len() } else { 0 };
    let max_drop = area.height.saturating_sub(4).clamp(1, 14);
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
    // Digits, a space, then the rule that separates them from the text.
    let gutter = (num_w + 2) as u16;
    let text_w = area.width.saturating_sub(gutter) as usize;
    let height = area.height as usize;
    if text_w == 0 || height == 0 {
        return None;
    }
    // Lines wrap to the width we actually have; movement needs the same number.
    app.buffer.wrap_width = text_w;

    let cursor_row = app.buffer.row.min(total - 1);
    let cursor_rows = wrap::segments(&app.buffer.lines[cursor_row], text_w);
    let cursor_seg = wrap::segment_of(&cursor_rows, app.buffer.col);
    let cursor_offset = wrap::col_in_row(
        &app.buffer.lines[cursor_row],
        cursor_rows[cursor_seg].start,
        app.buffer.col,
    );

    // Keep the caret inside the view. The view is measured in rows, and a
    // wrapped line is several of them, so count the rows that are actually on
    // screen instead of assuming one per line.
    if app.buffer.scroll >= total {
        app.buffer.scroll = total - 1;
        app.buffer.scroll_seg = 0;
    }
    if cursor_row < app.buffer.scroll {
        app.buffer.scroll = cursor_row;
        app.buffer.scroll_seg = 0;
    }
    if cursor_row == app.buffer.scroll && cursor_seg < app.buffer.scroll_seg {
        app.buffer.scroll_seg = cursor_seg;
    }
    // Counting stops as soon as the viewport is overfilled: every line is at
    // least one row, so this visits at most `height` lines even when the caret
    // has jumped halfway down the file.
    let mut rows = 0usize;
    for line in app.buffer.scroll..=cursor_row {
        let here = wrap::segments(&app.buffer.lines[line], text_w).len();
        let first = if line == app.buffer.scroll {
            app.buffer.scroll_seg.min(here - 1)
        } else {
            0
        };
        rows += if line == cursor_row {
            (cursor_seg + 1).saturating_sub(first)
        } else {
            here - first
        };
        if rows > height {
            break;
        }
    }

    if rows > height {
        // Too tall: let the caret sit on the bottom row, and start the view
        // whatever number of rows above it fits.
        let mut line = cursor_row;
        let mut seg = cursor_seg;
        for _ in 1..height {
            if seg > 0 {
                seg -= 1;
            } else if line > 0 {
                line -= 1;
                seg = wrap::segments(&app.buffer.lines[line], text_w).len() - 1;
            } else {
                break;
            }
        }
        app.buffer.scroll = line;
        app.buffer.scroll_seg = seg;
    }

    let mut out: Vec<Line> = Vec::new();
    let mut caret: Option<Position> = None;
    let mut idx = app.buffer.scroll;

    while idx < total && out.len() < height {
        let rows = wrap::segments(&app.buffer.lines[idx], text_w);
        // The first visible row of the line may not be its first row.
        let first = if idx == app.buffer.scroll {
            app.buffer.scroll_seg.min(rows.len() - 1)
        } else {
            0
        };
        let current = idx == cursor_row;

        for (seg, row) in rows.iter().enumerate().skip(first) {
            if out.len() >= height {
                break;
            }
            let mut spans = Vec::new();
            if seg == 0 {
                let current_style = if current {
                    Style::default().fg(Color::Gray)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                spans.push(Span::styled(format!("{:>num_w$} ", idx + 1), current_style));
            } else {
                // Continuation rows keep the text column, so wrapped code lines
                // up instead of sliding under the line numbers.
                spans.push(Span::raw(" ".repeat(num_w + 1)));
            }
            // The rule runs down every row, wrapped ones included, so the text
            // has one clear edge.
            spans.push(Span::styled(
                "\u{2502}",
                Style::default().fg(Color::DarkGray),
            ));

            match app.highlight.get(idx) {
                Some(segments) if !segments.is_empty() => {
                    spans.extend(segment_spans(segments, row.start, row.end));
                }
                _ => spans.push(Span::raw(wrap::expand_range(&app.buffer.lines[idx], row))),
            }

            out.push(Line::from(spans));
            if current && seg == cursor_seg {
                caret = Some(Position::new(
                    area.x + gutter + cursor_offset as u16,
                    area.y + out.len() as u16 - 1,
                ));
            }
        }
        idx += 1;
    }

    frame.render_widget(Paragraph::new(out), area);

    if app.focus == Focus::Editor {
        return caret;
    }
    None
}

fn draw_splash(frame: &mut Frame, area: Rect) {
    let dim = Style::default().fg(Color::DarkGray);
    let label = Style::default().fg(Color::Gray);
    let accent = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);

    // One fixed-width first column, so the descriptions line up as a table
    // instead of every row being centred on its own.
    let key_col = 12;
    let row = |keys: &str, description: &str| {
        Line::from(vec![
            Span::styled(format!("  {keys:<key_col$} "), label),
            Span::styled(description.to_string(), dim),
        ])
    };

    let lines = vec![
        // Two leading spaces keep the title flush with the table's first
        // column instead of hanging off its left edge.
        Line::from(Span::styled("  peek", accent)),
        Line::from(Span::styled("  type to find a file", dim)),
        Line::from(""),
        row("index", "fuzzy path match"),
        row("\"literal\"", "exact substring"),
        row("/regex/", "pattern"),
        row("auth/ tok", "scoped to a directory"),
        row(":l rehab", "find a line in this file"),
        row(":f token", "search inside files"),
        row("!test", "exclude"),
        Line::from(""),
        row("ctrl+g", "commit this file"),
        row("ctrl+shift+g", "commit every file edited here"),
        row("ctrl+u", "push"),
        Line::from(""),
        row("ctrl+s", "save"),
        row("esc", "back to the query line"),
        row("ctrl+q", "quit"),
    ];

    // Centre the block, not the lines: every row keeps a common left edge.
    let block_width = lines.iter().map(|line| line.width()).max().unwrap_or(0) as u16;
    let width = block_width.min(area.width);
    let height = (area.height as usize).min(lines.len()) as u16;
    let rect = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Paragraph::new(lines), rect);
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
        Focus::Picker => "enter open · ↑↓ select · esc clear · ctrl+g commit · ctrl+u push",
        Focus::Editor => "esc find · ctrl+s save · ctrl+g commit · ctrl+shift+g all · ctrl+u push",
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
        |i| {
            ranges
                .iter()
                .any(|(start, len)| i >= *start && i < *start + *len)
        },
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

/// Render the chars `[start, end)` of one line's styled segments. Tabs expand
/// from the row's left edge, matching the caret arithmetic in `wrap`.
fn segment_spans(segments: &[Segment], start: usize, end: usize) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut index = 0usize;
    let mut column = 0usize;

    for segment in segments {
        let base = index;
        let count = segment.text.chars().count();
        index += count;
        let mut buffer = String::new();

        for (offset, ch) in segment.text.chars().enumerate() {
            let here = base + offset;
            if here < start {
                continue;
            }
            if here >= end {
                break;
            }
            if ch == '\t' {
                let step = TAB_WIDTH - (column % TAB_WIDTH);
                buffer.push_str(&" ".repeat(step));
                column += step;
            } else {
                buffer.push(ch);
                column += 1;
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
    }

    spans
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
    fn splash_rows_share_one_left_edge() {
        let dir = std::env::temp_dir().join(format!("peek-ui-splash-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mut app = App::new(dir.clone());
        let mut terminal = Terminal::new(TestBackend::new(90, 30)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let width = 90;
        let row = |y: u16| -> String {
            (0..width)
                .map(|x| symbol_at(&terminal, x, y))
                .collect::<String>()
        };
        let column_of = |needle: &str| -> u16 {
            (0..30u16)
                .find_map(|y| row(y).find(needle).map(|col| col as u16))
                .unwrap_or_else(|| panic!("no row containing {needle:?}"))
        };

        let index = column_of("index");
        assert_eq!(column_of(":f token"), index, "query rows must align");
        assert_eq!(column_of("ctrl+shift+g"), index, "key rows must align");
        assert_eq!(column_of("ctrl+u"), index);

        // The block is centred as a whole, so its left edge is not column 0.
        assert!(index > 0, "block should sit away from the left edge");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_view_follows_the_caret_past_the_bottom_row() {
        let body: String = (1..=20).map(|n| format!("line {n}\n")).collect();
        let (mut app, dir) = app_with_file("scrolldown", &body);
        // Ten rows of terminal: eight for the buffer, then the query and status
        // lines.
        let mut terminal = Terminal::new(TestBackend::new(24, 10)).unwrap();

        for row in 0..20 {
            app.buffer.row = row;
            app.buffer.col = 0;
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();

            let screen: Vec<String> = (0..8)
                .map(|y| {
                    (0..16)
                        .map(|x| symbol_at(&terminal, x, y))
                        .collect::<String>()
                })
                .collect();
            assert!(
                screen
                    .iter()
                    .any(|line| line.contains(&format!("line {}", row + 1))),
                "line {} should be on screen, got {screen:?}",
                row + 1
            );
            // The view stays put until the caret reaches the last row, then it
            // scrolls one row at a time.
            assert_eq!(
                app.buffer.scroll,
                row.saturating_sub(7),
                "view top with the caret on line {}",
                row + 1
            );
        }

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn long_lines_wrap_and_the_caret_follows() {
        let line = "the quick brown fox jumps over the lazy dog";
        let (mut app, dir) = app_with_file("wrap", &format!("{line}\n"));
        let mut terminal = Terminal::new(TestBackend::new(20, 6)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let row = |y: u16| -> String {
            (0..20)
                .map(|x| symbol_at(&terminal, x, y))
                .collect::<String>()
        };
        assert!(row(0).contains("the quick"), "row 0 was {:?}", row(0));
        assert!(
            row(1).contains("brown fox"),
            "the rest of the line should be below it: {:?}",
            row(1)
        );
        // Continuation rows keep the text column and carry no line number, but
        // the gutter rule still runs down them.
        assert_eq!(symbol_at(&terminal, 0, 1), " ");
        assert_eq!(symbol_at(&terminal, 3, 1), " ");
        assert_eq!(symbol_at(&terminal, 4, 0), "\u{2502}");
        assert_eq!(symbol_at(&terminal, 4, 1), "\u{2502}");

        // Column 10 is the 'b' of "brown", the first character on row 1.
        app.buffer.col = 10;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let caret = terminal.get_cursor_position().unwrap();
        assert_eq!(caret.y, 1, "caret should be on the wrapped row");
        assert_eq!(symbol_at(&terminal, caret.x, caret.y), "b");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn line_numbers_are_right_aligned_and_text_starts_after_them() {
        let (mut app, dir) = app_with_file("numbers", "one\ntwo\n");
        let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let numbers: String = (0..4).map(|x| symbol_at(&terminal, x, 0)).collect();
        assert_eq!(numbers, "  1 ");
        assert_eq!(symbol_at(&terminal, 4, 0), "\u{2502}", "gutter rule");
        assert_eq!(symbol_at(&terminal, 5, 0), "o");

        fs::remove_dir_all(&dir).ok();
    }
}
