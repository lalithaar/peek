//! The editing surface: a line buffer with undo, auto-indent, and an atomic
//! save that refuses to clobber a file someone else changed.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use crate::wrap::{self, TAB_WIDTH};

const UNDO_LIMIT: usize = 300;
const COALESCE: Duration = Duration::from_millis(400);

#[derive(Clone)]
struct Snapshot {
    lines: Vec<String>,
    row: usize,
    col: usize,
}

pub struct Buffer {
    pub path: Option<PathBuf>,
    pub lines: Vec<String>,
    pub row: usize,
    pub col: usize,
    /// Top of the view: which line, and which wrapped row of that line.
    pub scroll: usize,
    pub scroll_seg: usize,
    /// Cells available for text, set by the renderer each frame. Zero means the
    /// width is not known yet, so lines are not wrapped.
    pub wrap_width: usize,
    pub dirty: bool,
    /// Bumped on every change to the text, so the highlighter knows when its
    /// cache is stale without diffing lines.
    pub revision: u64,
    pub message: Option<String>,
    crlf: bool,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    last_edit: Option<Instant>,
    stamp: Option<(u64, u64)>,
    force_save: bool,
}

impl Default for Buffer {
    fn default() -> Self {
        Self::new()
    }
}

impl Buffer {
    pub fn new() -> Self {
        Buffer {
            path: None,
            lines: vec![String::new()],
            row: 0,
            col: 0,
            scroll: 0,
            scroll_seg: 0,
            wrap_width: 0,
            dirty: false,
            revision: 0,
            message: None,
            crlf: false,
            undo: Vec::new(),
            redo: Vec::new(),
            last_edit: None,
            stamp: None,
            force_save: false,
        }
    }

    pub fn open(&mut self, path: &Path) -> io::Result<()> {
        let bytes = fs::read(path)?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        self.crlf = text.contains("\r\n");
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
        if lines.is_empty() {
            lines.push(String::new());
        }
        self.lines = lines;
        self.path = Some(path.to_path_buf());
        self.row = 0;
        self.col = 0;
        self.scroll = 0;
        self.scroll_seg = 0;
        self.dirty = false;
        self.undo.clear();
        self.redo.clear();
        self.last_edit = None;
        self.force_save = false;
        self.stamp = fs::metadata(path).ok().map(|m| stamp_of(&m));
        self.message = None;
        self.revision += 1;
        Ok(())
    }

    pub fn label(&self) -> String {
        let Some(path) = &self.path else {
            return String::from("[no file]");
        };
        path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string())
    }

    /// Atomic save: write a sibling temp file, then rename over the original.
    /// Returns `true` only when bytes actually landed on disk.
    pub fn save(&mut self) -> io::Result<bool> {
        let Some(path) = self.path.clone() else {
            self.message = Some("no file to save".into());
            return Ok(false);
        };

        if let (Some(known), Ok(meta)) = (self.stamp, fs::metadata(&path))
            && stamp_of(&meta) != known
            && !self.force_save
        {
            self.force_save = true;
            self.message = Some("file changed on disk — ctrl+s again to overwrite".into());
            return Ok(false);
        }

        let body = self.lines.join(if self.crlf { "\r\n" } else { "\n" });
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| String::from("peek"));
        let tmp = path.with_file_name(format!(".{name}.peek-tmp"));
        {
            let mut file = fs::File::create(&tmp)?;
            file.write_all(body.as_bytes())?;
            file.sync_all()?;
        }
        fs::rename(&tmp, &path)?;

        self.dirty = false;
        self.force_save = false;
        self.stamp = fs::metadata(&path).ok().map(|m| stamp_of(&m));
        self.message = Some(format!("saved {}", name));
        Ok(true)
    }

    // ---- cursor -----------------------------------------------------------

    pub fn clamp(&mut self) {
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.row = self.row.min(self.lines.len() - 1);
        let len = self.lines[self.row].chars().count();
        self.col = self.col.min(len);
    }

    pub fn goto_line(&mut self, row: usize) {
        self.row = row.min(self.lines.len().saturating_sub(1));
        self.col = 0;
        self.clamp();
    }

    pub fn move_left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.lines[self.row].chars().count();
        }
    }

    pub fn move_right(&mut self) {
        let len = self.lines[self.row].chars().count();
        if self.col < len {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    /// Up and down move by what you can see: a wrapped line is several rows, so
    /// the caret walks them one at a time, keeping its distance from the left
    /// edge of the text.
    pub fn move_up(&mut self) {
        let Some((row, seg, offset)) = self.visual_cursor() else {
            if self.row > 0 {
                self.row -= 1;
                self.clamp();
            }
            return;
        };
        if seg > 0 {
            let rows = wrap::segments(&self.lines[row], self.wrap_width);
            let target = rows[seg - 1].clone();
            self.col = wrap::col_at_offset(&self.lines[row], target.start, offset).min(target.end);
        } else if row > 0 {
            self.row = row - 1;
            let rows = wrap::segments(&self.lines[self.row], self.wrap_width);
            let target = rows.last().cloned().unwrap_or(0..0);
            self.col =
                wrap::col_at_offset(&self.lines[self.row], target.start, offset).min(target.end);
        }
    }

    pub fn move_down(&mut self) {
        let Some((row, seg, offset)) = self.visual_cursor() else {
            if self.row + 1 < self.lines.len() {
                self.row += 1;
                self.clamp();
            }
            return;
        };
        let rows = wrap::segments(&self.lines[row], self.wrap_width);
        if seg + 1 < rows.len() {
            let target = rows[seg + 1].clone();
            self.col = wrap::col_at_offset(&self.lines[row], target.start, offset).min(target.end);
        } else if row + 1 < self.lines.len() {
            self.row = row + 1;
            self.col = wrap::col_at_offset(&self.lines[self.row], 0, offset);
        }
    }

    /// The caret as (line, wrapped row of that line, cells from the row's left
    /// edge), or `None` when lines are not wrapped.
    fn visual_cursor(&self) -> Option<(usize, usize, usize)> {
        if self.wrap_width == 0 {
            return None;
        }
        let row = self.row.min(self.lines.len().saturating_sub(1));
        let rows = wrap::segments(&self.lines[row], self.wrap_width);
        let seg = wrap::segment_of(&rows, self.col);
        let offset = wrap::col_in_row(&self.lines[row], rows[seg].start, self.col);
        Some((row, seg, offset))
    }

    pub fn home(&mut self) {
        self.col = 0;
    }

    pub fn end(&mut self) {
        self.col = self.lines[self.row].chars().count();
    }

    pub fn word_left(&mut self) {
        let chars: Vec<char> = self.lines[self.row].chars().collect();
        let mut i = self.col;
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        if i == self.col && i > 0 {
            i -= 1;
        }
        self.col = i;
    }

    pub fn word_right(&mut self) {
        let chars: Vec<char> = self.lines[self.row].chars().collect();
        let mut i = self.col;
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        self.col = i;
    }

    // ---- editing ----------------------------------------------------------

    fn begin_edit(&mut self, coalesce: bool) {
        let now = Instant::now();
        let merge = coalesce
            && self
                .last_edit
                .map(|prev| now.duration_since(prev) < COALESCE)
                .unwrap_or(false);
        if !merge {
            self.undo.push(Snapshot {
                lines: self.lines.clone(),
                row: self.row,
                col: self.col,
            });
            if self.undo.len() > UNDO_LIMIT {
                self.undo.remove(0);
            }
            self.redo.clear();
        }
        self.last_edit = Some(now);
        self.dirty = true;
        self.revision += 1;
        self.force_save = false;
    }

    pub fn insert_char(&mut self, c: char) {
        self.begin_edit(true);
        let (row, col) = (self.row, self.col);
        let byte = char_to_byte(&self.lines[row], col);
        self.lines[row].insert(byte, c);
        self.col = col + 1;
    }

    /// Raw insertion: pasted text lands exactly as it arrived. No re-indenting,
    /// no auto-closing, no surprises.
    pub fn insert_str(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.begin_edit(false);
        let (row, col) = (self.row, self.col);
        let byte = char_to_byte(&self.lines[row], col);
        if !text.contains('\n') {
            self.lines[row].insert_str(byte, text);
            self.col = col + text.chars().count();
        } else {
            let head: String = self.lines[row].chars().take(col).collect();
            let tail: String = self.lines[row].chars().skip(col).collect();
            let parts: Vec<&str> = text.split('\n').collect();
            let mut new_lines: Vec<String> = Vec::new();
            for (i, part) in parts.iter().enumerate() {
                if i == 0 {
                    new_lines.push(format!("{head}{part}"));
                } else if i == parts.len() - 1 {
                    new_lines.push(format!("{part}{tail}"));
                } else {
                    new_lines.push(part.to_string());
                }
            }
            let last_col = parts.last().map(|p| p.chars().count()).unwrap_or(0);
            self.lines.splice(row..=row, new_lines);
            self.row = row + parts.len() - 1;
            self.col = last_col;
        }
        self.clamp();
    }

    pub fn tab(&mut self) {
        let fill = TAB_WIDTH - (self.col % TAB_WIDTH);
        for _ in 0..fill {
            self.insert_char(' ');
        }
    }

    pub fn newline(&mut self) {
        self.begin_edit(false);
        let (row, col) = (self.row, self.col);
        let line = self.lines[row].clone();
        let head: String = line.chars().take(col).collect();
        let tail: String = line.chars().skip(col).collect();
        let indent: String = head
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        self.lines[row] = head;
        self.lines.insert(row + 1, format!("{indent}{tail}"));
        self.row = row + 1;
        self.col = indent.chars().count();
    }

    pub fn backspace(&mut self) {
        if self.col == 0 {
            if self.row == 0 {
                return;
            }
            self.begin_edit(false);
            let tail = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.lines[self.row].chars().count();
            self.lines[self.row].push_str(&tail);
            return;
        }
        self.begin_edit(true);
        let (row, col) = (self.row, self.col);
        let start = char_to_byte(&self.lines[row], col - 1);
        let end = char_to_byte(&self.lines[row], col);
        self.lines[row].replace_range(start..end, "");
        self.col = col - 1;
    }

    pub fn delete_forward(&mut self) {
        let len = self.lines[self.row].chars().count();
        if self.col >= len {
            if self.row + 1 >= self.lines.len() {
                return;
            }
            self.begin_edit(false);
            let tail = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&tail);
            return;
        }
        self.begin_edit(false);
        let (row, col) = (self.row, self.col);
        let start = char_to_byte(&self.lines[row], col);
        let end = char_to_byte(&self.lines[row], col + 1);
        self.lines[row].replace_range(start..end, "");
    }

    pub fn undo(&mut self) {
        let Some(snap) = self.undo.pop() else {
            self.message = Some("nothing to undo".into());
            return;
        };
        self.redo.push(Snapshot {
            lines: std::mem::take(&mut self.lines),
            row: self.row,
            col: self.col,
        });
        self.lines = snap.lines;
        self.row = snap.row;
        self.col = snap.col;
        self.clamp();
        self.dirty = true;
        self.revision += 1;
        self.last_edit = None;
        self.message = None;
    }

    pub fn redo(&mut self) {
        let Some(snap) = self.redo.pop() else {
            self.message = Some("nothing to redo".into());
            return;
        };
        self.undo.push(Snapshot {
            lines: std::mem::take(&mut self.lines),
            row: self.row,
            col: self.col,
        });
        self.lines = snap.lines;
        self.row = snap.row;
        self.col = snap.col;
        self.clamp();
        self.dirty = true;
        self.revision += 1;
        self.last_edit = None;
    }
}

fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(byte, _)| byte)
        .unwrap_or(s.len())
}

fn stamp_of(meta: &fs::Metadata) -> (u64, u64) {
    let secs = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    (meta.len(), secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("peek-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn typing_and_undo() {
        let mut b = Buffer::new();
        for c in "hello".chars() {
            b.insert_char(c);
        }
        assert_eq!(b.lines[0], "hello");
        b.newline();
        for c in "world".chars() {
            b.insert_char(c);
        }
        assert_eq!(b.lines, vec!["hello".to_string(), "world".to_string()]);
        b.undo();
        b.undo();
        b.undo();
        assert_eq!(b.lines, vec![String::new()]);
    }

    #[test]
    fn auto_indent_copies_leading_space() {
        let mut b = Buffer::new();
        b.insert_str("    fn main() {}");
        b.end();
        b.newline();
        assert_eq!(b.lines[1], "    ");
        assert_eq!(b.col, 4);
    }

    #[test]
    fn paste_is_raw() {
        let mut b = Buffer::new();
        b.insert_str("  one\n\ttwo\nthree");
        assert_eq!(
            b.lines,
            vec![
                "  one".to_string(),
                "\ttwo".to_string(),
                "three".to_string()
            ]
        );
        assert_eq!(b.row, 2);
        assert_eq!(b.col, 5);
    }

    #[test]
    fn backspace_joins_lines() {
        let mut b = Buffer::new();
        b.insert_str("ab\ncd");
        b.goto_line(1);
        b.backspace();
        assert_eq!(b.lines, vec!["abcd".to_string()]);
        assert_eq!(b.col, 2);
    }

    #[test]
    fn down_and_up_walk_wrapped_rows() {
        let mut b = Buffer::new();
        b.insert_str("aaaa bbbb cccc dddd");
        b.wrap_width = 10;
        b.home();
        b.move_down();
        assert_eq!(b.row, 0, "a wrapped row is still the same line");
        assert_eq!(b.col, 10);
        b.move_down();
        assert_eq!((b.row, b.col), (0, 10), "nothing below the last row");
        b.move_up();
        assert_eq!((b.row, b.col), (0, 0));
    }

    #[test]
    fn vertical_movement_keeps_the_screen_column() {
        let mut b = Buffer::new();
        b.insert_str("0123456789abcdef\nxyz");
        b.wrap_width = 8;
        b.row = 0;
        b.col = 3;

        b.move_down();
        assert_eq!((b.row, b.col), (0, 11));
        b.move_down();
        assert_eq!((b.row, b.col), (1, 3), "short line clamps to its end");
        b.move_up();
        assert_eq!((b.row, b.col), (0, 11));
    }

    #[test]
    fn unicode_columns_are_chars_not_bytes() {
        let mut b = Buffer::new();
        b.insert_str("héllo wörld");
        b.home();
        for _ in 0..6 {
            b.move_right();
        }
        assert_eq!(b.col, 6);
        b.insert_char('!');
        assert_eq!(b.lines[0], "héllo !wörld");
    }

    #[test]
    fn save_roundtrip_is_atomic_and_keeps_crlf() {
        let path = temp_file("crlf.txt");
        fs::write(&path, "a\r\nb\r\n").unwrap();
        let mut b = Buffer::new();
        b.open(&path).unwrap();
        assert!(b.crlf);
        assert_eq!(
            b.lines,
            vec!["a".to_string(), "b".to_string(), String::new()]
        );
        b.insert_str("x");
        assert!(b.save().unwrap());
        assert!(!b.dirty);
        assert_eq!(fs::read_to_string(&path).unwrap(), "xa\r\nb\r\n");
        assert!(!path.with_file_name(".crlf.txt.peek-tmp").exists());
        fs::remove_file(&path).ok();
    }

    #[test]
    fn save_warns_once_on_external_change() {
        let path = temp_file("external.txt");
        fs::write(&path, "original\n").unwrap();
        let mut b = Buffer::new();
        b.open(&path).unwrap();
        b.insert_str("mine ");
        std::thread::sleep(Duration::from_millis(1100));
        fs::write(&path, "theirs\n").unwrap();

        assert!(!b.save().unwrap());
        assert!(b.dirty);
        assert!(
            b.message
                .as_deref()
                .unwrap_or("")
                .contains("changed on disk")
        );

        assert!(b.save().unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), "mine original\n");
        fs::remove_file(&path).ok();
    }
}
