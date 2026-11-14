//! Application state and the event loop.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::editor::Buffer;
use crate::git::{self, Op as GitOp};
use crate::grep::{self, ContentHit, ContentResult, LineQuery, Spec};
use crate::index::{self, FileFilter, Hit, WalkMsg};
use crate::query::{Mode, Query};
use crate::syntax::{Highlighter, Segment};

#[derive(Debug, Clone)]
pub struct BufferHit {
    pub line_no: usize,
    pub snippet: String,
    pub ranges: Vec<(u32, u32)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Picker,
    Editor,
}

const RESULT_LIMIT: usize = 200;

pub struct App {
    pub root: PathBuf,
    pub files: Vec<String>,
    walk_rx: Receiver<WalkMsg>,
    walk_done: bool,

    pub query_input: String,
    pub query: Query,
    filter: FileFilter,
    pub results: Vec<Hit>,
    pub selection: usize,
    selected: Option<String>,

    pub content: Vec<ContentHit>,
    pub content_msg: Option<String>,
    pub line_hits: Vec<BufferHit>,
    pub line_msg: Option<String>,
    content_gen: Arc<AtomicU64>,
    content_tx: Sender<ContentResult>,
    content_rx: Receiver<ContentResult>,

    pub focus: Focus,
    pub buffer: Buffer,
    pub highlight: Vec<Vec<Segment>>,
    highlighter: Highlighter,
    highlighted_rev: u64,

    /// Files edited during this session, so "commit all" means what you
    /// actually touched here.
    pub edited: Vec<PathBuf>,
    git_tx: Sender<git::Outcome>,
    git_rx: Receiver<git::Outcome>,
    pub git_busy: Option<GitOp>,
    pub mru: Vec<String>,
    history: Vec<PathBuf>,
    hist_pos: usize,

    pub quit: bool,
    quit_armed: bool,
    /// Set when a switch would throw away unsaved edits; a second pick of the
    /// same file within a few seconds confirms the discard.
    pending_open: Option<(PathBuf, Instant)>,
    pub status: String,
}

impl App {
    pub fn new(root: PathBuf) -> Self {
        let walk_rx = index::spawn_walk(root.clone());
        let (content_tx, content_rx) = channel();
        let (git_tx, git_rx) = channel();
        let mut app = App {
            root,
            files: Vec::new(),
            walk_rx,
            walk_done: false,
            query_input: String::new(),
            query: Query::parse(""),
            filter: FileFilter::new(),
            results: Vec::new(),
            selection: 0,
            selected: None,
            content: Vec::new(),
            content_msg: None,
            line_hits: Vec::new(),
            line_msg: None,
            content_gen: Arc::new(AtomicU64::new(0)),
            content_tx,
            content_rx,
            focus: Focus::Picker,
            buffer: Buffer::new(),
            highlight: Vec::new(),
            highlighter: Highlighter::new(),
            highlighted_rev: u64::MAX,
            edited: Vec::new(),
            git_tx,
            git_rx,
            git_busy: None,
            mru: Vec::new(),
            history: Vec::new(),
            hist_pos: 0,
            quit: false,
            quit_armed: false,
            pending_open: None,
            status: String::new(),
        };
        app.recompute();
        app
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> std::io::Result<()> {
        loop {
            self.pump();
            terminal.draw(|frame| crate::ui::draw(frame, self))?;
            if event::poll(Duration::from_millis(30))? {
                match event::read()? {
                    Event::Key(key) => self.on_key(key),
                    Event::Paste(text) => self.on_paste(text),
                    _ => {}
                }
            }
            if self.quit {
                return Ok(());
            }
        }
    }

    // ---- background work --------------------------------------------------

    pub(crate) fn pump(&mut self) {
        if self.highlighted_rev != self.buffer.revision {
            self.highlighted_rev = self.buffer.revision;
            self.highlight = self
                .highlighter
                .highlight(self.buffer.path.as_deref(), &self.buffer.lines);
            // Remember what this session actually touched.
            if self.buffer.dirty
                && let Some(path) = self.buffer.path.clone()
                && !self.edited.contains(&path)
            {
                self.edited.push(path);
            }
        }

        for outcome in self.git_rx.try_iter() {
            self.git_busy = None;
            self.status = outcome.message;
            if outcome.ok {
                match outcome.op {
                    GitOp::CommitCurrent => {
                        if let Some(path) = &self.buffer.path {
                            self.edited.retain(|edited| edited != path);
                        }
                    }
                    GitOp::CommitAll => self.edited.clear(),
                    GitOp::Push => {}
                }
            }
        }

        let mut changed = false;
        for msg in self.walk_rx.try_iter() {
            match msg {
                WalkMsg::Batch(batch) => {
                    self.files.extend(batch);
                    changed = true;
                }
                WalkMsg::Done => {
                    self.files.sort();
                    self.files.dedup();
                    self.walk_done = true;
                    changed = true;
                }
            }
        }
        if changed {
            self.recompute();
        }

        let current = self.content_gen.load(Ordering::SeqCst);
        let incoming: Vec<ContentResult> = self
            .content_rx
            .try_iter()
            .filter(|result| result.generation == current)
            .collect();
        if let Some(result) = incoming.into_iter().last() {
            let count = result.hits.len();
            self.content = result.hits;
            self.content_msg = Some(if result.truncated {
                format!("{count}+ matches, refine to see more")
            } else {
                format!("{count} matches")
            });
            self.clamp_selection();
        }
        let _ = self.walk_done;
    }

    // ---- query ------------------------------------------------------------

    fn recompute(&mut self) {
        self.query = Query::parse(&self.query_input);

        match self.query.mode {
            Mode::Files => {
                self.content.clear();
                self.content_msg = None;
                self.clear_line_hits();
                if self.query.is_empty() {
                    self.results = self.empty_query_hits();
                } else if self.query.error.is_none() {
                    self.results =
                        index::filter(&self.files, &self.query, &mut self.filter, RESULT_LIMIT);
                }
                // A bad regex keeps the previous list on screen rather than
                // blanking it under the cursor.
                self.restore_selection();
            }
            Mode::Content => {
                self.results.clear();
                self.clear_line_hits();
                if self.query.error.is_some() {
                    return;
                }
                if self.query.terms.is_empty() {
                    self.content.clear();
                    self.content_msg = Some("type a term after :f".into());
                    return;
                }
                self.start_content_search();
            }
            Mode::Buffer => {
                self.results.clear();
                self.content.clear();
                self.content_msg = None;
                self.search_buffer();
            }
        }
    }

    fn clear_line_hits(&mut self) {
        self.line_hits.clear();
        self.line_msg = None;
    }

    /// `:l` — the same term grammar applied to the open buffer. Small enough to
    /// run inline on every keystroke, so the list never lags behind typing.
    fn search_buffer(&mut self) {
        self.line_hits.clear();
        if self.query.error.is_some() {
            return;
        }
        if self.buffer.path.is_none() {
            self.line_msg = Some("no file open — :f searches the project".into());
            return;
        }
        if self.query.terms.is_empty() {
            self.line_msg = Some("type a term after :l".into());
            return;
        }

        let matcher = LineQuery::new(&self.query.terms);
        let lines = &self.buffer.lines;
        for (index, line) in lines.iter().enumerate() {
            if let Some(ranges) = matcher.matches(line) {
                self.line_hits.push(BufferHit {
                    line_no: index + 1,
                    snippet: line.chars().take(300).collect(),
                    ranges,
                });
                if self.line_hits.len() >= RESULT_LIMIT {
                    break;
                }
            }
        }

        let count = self.line_hits.len();
        self.line_msg = Some(if count >= RESULT_LIMIT {
            format!("{count}+ matches in this file")
        } else {
            format!("{count} matches in this file")
        });
        self.selection = self.selection.min(count.saturating_sub(1));
        self.selected = None;
    }

    fn start_content_search(&mut self) {
        let generation = self.content_gen.fetch_add(1, Ordering::SeqCst) + 1;
        self.content.clear();
        self.content_msg = Some("searching…".into());
        self.selection = 0;
        self.selected = None;
        grep::spawn(
            Spec {
                root: self.root.clone(),
                scope: self.query.scope.clone(),
                terms: self.query.terms.clone(),
            },
            generation,
            self.content_gen.clone(),
            self.content_tx.clone(),
        );
        let _ = generation;
    }

    /// Empty query is never an empty box: siblings of the open file first,
    /// then most-recently-used files.
    fn empty_query_hits(&self) -> Vec<Hit> {
        let mut out: Vec<Hit> = Vec::new();
        if let Some(path) = &self.buffer.path
            && let Ok(rel) = path.strip_prefix(&self.root)
        {
            let rel = rel.to_string_lossy().replace('\\', "/");
            if let Some((dir, _)) = rel.rsplit_once('/') {
                let prefix = format!("{dir}/");
                for file in &self.files {
                    if let Some(rest) = file.strip_prefix(&prefix)
                        && !rest.contains('/')
                        && rest != rel.rsplit('/').next().unwrap_or_default()
                    {
                        out.push(Hit {
                            path: file.clone(),
                            score: 0,
                            indices: Vec::new(),
                        });
                    }
                }
            }
        }
        for path in &self.mru {
            if !out.iter().any(|hit| &hit.path == path) {
                out.push(Hit {
                    path: path.clone(),
                    score: 0,
                    indices: Vec::new(),
                });
            }
        }
        out.truncate(50);
        out
    }

    fn restore_selection(&mut self) {
        if let Some(previous) = &self.selected
            && let Some(pos) = self.results.iter().position(|hit| &hit.path == previous)
        {
            self.selection = pos;
            return;
        }
        self.clamp_selection();
    }

    fn clamp_selection(&mut self) {
        let len = self.visible_len();
        if len == 0 {
            self.selection = 0;
        } else {
            self.selection = self.selection.min(len - 1);
        }
        self.selected = self.current_path();
    }

    pub fn visible_len(&self) -> usize {
        match self.query.mode {
            Mode::Files => self.results.len(),
            Mode::Content => self.content.len(),
            Mode::Buffer => self.line_hits.len(),
        }
    }

    fn current_path(&self) -> Option<String> {
        match self.query.mode {
            Mode::Files => self.results.get(self.selection).map(|h| h.path.clone()),
            Mode::Content => self.content.get(self.selection).map(|h| h.path.clone()),
            Mode::Buffer => None,
        }
    }

    // ---- opening files ----------------------------------------------------

    fn load(&mut self, path: &Path, line: Option<usize>) -> bool {
        if !self.confirm_switch(path) {
            return false;
        }
        match self.buffer.open(path) {
            Ok(()) => {
                self.pending_open = None;
                if let Some(line) = line {
                    self.buffer.goto_line(line.saturating_sub(1));
                }
                true
            }
            Err(err) => {
                self.status = format!("cannot open {}: {err}", path.display());
                false
            }
        }
    }

    /// Opening a file replaces the buffer, so a dirty one gets one warning
    /// first. Picking the same file again inside a few seconds means yes,
    /// discard; ctrl+s instead saves and then the switch goes through.
    fn confirm_switch(&mut self, path: &Path) -> bool {
        if !self.buffer.dirty {
            return true;
        }
        let confirming = matches!(
            &self.pending_open,
            Some((pending, at)) if pending == path && at.elapsed() < Duration::from_secs(5)
        );
        if confirming {
            self.pending_open = None;
            return true;
        }

        let name = self.buffer.label();
        self.pending_open = Some((path.to_path_buf(), Instant::now()));
        self.status = format!(
            "{name} has unsaved changes — pick it again to discard them, or ctrl+s to save"
        );
        false
    }

    fn open_selected(&mut self) {
        // `:l` never leaves the buffer: it only moves the cursor.
        if self.query.mode == Mode::Buffer {
            if let Some(hit) = self.line_hits.get(self.selection).cloned() {
                self.buffer.goto_line(hit.line_no.saturating_sub(1));
                self.focus = Focus::Editor;
                self.query_input.clear();
                self.query = Query::parse("");
                self.clear_line_hits();
                self.selection = 0;
            }
            return;
        }

        let picked = match self.query.mode {
            Mode::Files => self
                .results
                .get(self.selection)
                .map(|hit| (hit.path.clone(), None)),
            Mode::Content => self
                .content
                .get(self.selection)
                .map(|hit| (hit.path.clone(), Some(hit.line_no))),
            Mode::Buffer => None,
        };
        let Some((rel, line)) = picked else {
            return;
        };
        let abs = self.root.join(&rel);
        if !self.load(&abs, line) {
            return;
        }
        self.mru.retain(|entry| entry != &rel);
        self.mru.insert(0, rel);
        self.mru.truncate(50);

        if self.history.get(self.hist_pos) != Some(&abs) {
            self.history.truncate(self.hist_pos.saturating_add(1));
            self.history.push(abs);
            self.hist_pos = self.history.len() - 1;
        }

        self.query_input.clear();
        self.query = Query::parse("");
        self.content.clear();
        self.content_msg = None;
        self.results.clear();
        self.selection = 0;
        self.selected = None;
        self.focus = Focus::Editor;
    }

    fn go_history(&mut self, delta: isize) {
        if self.history.is_empty() {
            return;
        }
        let next = self.hist_pos as isize + delta;
        if next < 0 || next >= self.history.len() as isize {
            return;
        }
        let path = self.history[next as usize].clone();
        // Only move the history cursor once the file actually opened, so a
        // refused switch (unsaved changes) leaves the position alone.
        if self.load(&path, None) {
            self.hist_pos = next as usize;
            self.focus = Focus::Editor;
        }
    }

    // ---- git --------------------------------------------------------------

    /// `ctrl+g` commits the open file; `ctrl+shift+g` commits everything edited
    /// in this session. Either way the buffer is written to disk first, because
    /// git sees files, not buffers.
    fn git_commit(&mut self, all: bool) {
        let op = if all {
            GitOp::CommitAll
        } else {
            GitOp::CommitCurrent
        };
        if self.git_busy.is_some() {
            self.status = "git: still working on the last one…".into();
            return;
        }

        if self.buffer.dirty {
            match self.buffer.save() {
                Ok(true) => {}
                Ok(false) => {
                    self.status = "git: save the file first (it changed on disk)".into();
                    return;
                }
                Err(err) => {
                    self.status = format!("git: save failed: {err}");
                    return;
                }
            }
        }

        let paths: Vec<PathBuf> = if all {
            self.edited
                .iter()
                .filter(|path| path.exists())
                .cloned()
                .collect()
        } else {
            match &self.buffer.path {
                Some(path) if path.exists() => vec![path.clone()],
                _ => Vec::new(),
            }
        };

        if paths.is_empty() {
            self.status = if all {
                "git: nothing edited in this session yet".into()
            } else {
                "git: no file open".into()
            };
            return;
        }

        self.buffer.message = None;
        self.status = if all {
            format!(
                "git: committing {} file(s) edited this session…",
                paths.len()
            )
        } else {
            "git: committing this file…".into()
        };
        self.git_busy = Some(op);
        git::spawn(self.root.clone(), op, paths, self.git_tx.clone());
    }

    fn git_push(&mut self) {
        if self.git_busy.is_some() {
            self.status = "git: still working on the last one…".into();
            return;
        }
        self.buffer.message = None;
        self.status = "git: pushing…".into();
        self.git_busy = Some(GitOp::Push);
        git::spawn(
            self.root.clone(),
            GitOp::Push,
            Vec::new(),
            self.git_tx.clone(),
        );
    }

    fn focus_picker(&mut self) {
        self.query_input.clear();
        self.query = Query::parse("");
        self.selection = 0;
        self.selected = None;
        self.focus = Focus::Picker;
        self.recompute();
    }

    // ---- input ------------------------------------------------------------

    fn on_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        if ctrl && matches!(key.code, KeyCode::Char('q')) {
            self.try_quit();
            return;
        }
        if ctrl && matches!(key.code, KeyCode::Char('s')) {
            if let Err(err) = self.buffer.save() {
                self.buffer.message = Some(format!("save failed: {err}"));
            }
            self.quit_armed = false;
            return;
        }
        if ctrl && matches!(key.code, KeyCode::Char(c) if c.eq_ignore_ascii_case(&'g')) {
            // Some terminals cannot distinguish ctrl+shift+g from ctrl+g and
            // deliver an uppercase G instead; accept that as "all" too.
            let all = key.modifiers.contains(KeyModifiers::SHIFT)
                || matches!(key.code, KeyCode::Char('G'));
            self.git_commit(all);
            return;
        }
        if ctrl && matches!(key.code, KeyCode::Char('u')) {
            self.git_push();
            return;
        }
        self.quit_armed = false;

        match self.focus {
            Focus::Picker => self.picker_key(key, ctrl),
            Focus::Editor => self.editor_key(key, ctrl),
        }
    }

    fn try_quit(&mut self) {
        if self.buffer.dirty && !self.quit_armed {
            self.quit_armed = true;
            self.buffer.message = Some("unsaved changes — ctrl+q again to quit".into());
            return;
        }
        self.quit = true;
    }

    fn picker_key(&mut self, key: KeyEvent, ctrl: bool) {
        match key.code {
            KeyCode::Char(c) if !ctrl => {
                self.query_input.push(c);
                self.recompute();
                self.clamp_selection();
            }
            KeyCode::Backspace => {
                self.query_input.pop();
                self.recompute();
                self.clamp_selection();
            }
            KeyCode::Up => {
                if self.selection > 0 {
                    self.selection -= 1;
                }
                self.selected = self.current_path();
            }
            KeyCode::Down => {
                if self.selection + 1 < self.visible_len() {
                    self.selection += 1;
                }
                self.selected = self.current_path();
            }
            KeyCode::PageUp => {
                self.selection = self.selection.saturating_sub(10);
                self.selected = self.current_path();
            }
            KeyCode::PageDown => {
                self.selection = (self.selection + 10).min(self.visible_len().saturating_sub(1));
                self.selected = self.current_path();
            }
            KeyCode::Enter => self.open_selected(),
            KeyCode::Esc => {
                if !self.query_input.is_empty() {
                    self.query_input.clear();
                    self.recompute();
                    self.clamp_selection();
                } else if self.buffer.path.is_some() {
                    self.focus = Focus::Editor;
                }
            }
            _ => {}
        }
    }

    fn editor_key(&mut self, key: KeyEvent, ctrl: bool) {
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char(c) if ctrl && matches!(c, 'z' | 'Z') => self.buffer.undo(),
            KeyCode::Char(c) if ctrl && matches!(c, 'y' | 'Y') => self.buffer.redo(),
            KeyCode::Char(c) if ctrl && matches!(c, 'o' | 'O' | 'p' | 'P') => self.focus_picker(),
            KeyCode::Char(c) if !ctrl && !alt => {
                self.buffer.insert_char(c);
                self.buffer.message = None;
            }
            KeyCode::Enter => self.buffer.newline(),
            KeyCode::Tab => self.buffer.tab(),
            KeyCode::Backspace if ctrl => self.buffer.word_left(), // ctrl+backspace: jump
            KeyCode::Backspace => self.buffer.backspace(),
            KeyCode::Delete => self.buffer.delete_forward(),
            KeyCode::Left if alt => self.go_history(-1),
            KeyCode::Right if alt => self.go_history(1),
            KeyCode::Left if ctrl => self.buffer.word_left(),
            KeyCode::Right if ctrl => self.buffer.word_right(),
            KeyCode::Left => self.buffer.move_left(),
            KeyCode::Right => self.buffer.move_right(),
            KeyCode::Up => self.buffer.move_up(),
            KeyCode::Down => self.buffer.move_down(),
            KeyCode::Home => self.buffer.home(),
            KeyCode::End => self.buffer.end(),
            KeyCode::PageUp => {
                for _ in 0..15 {
                    self.buffer.move_up();
                }
            }
            KeyCode::PageDown => {
                for _ in 0..15 {
                    self.buffer.move_down();
                }
            }
            KeyCode::Esc => self.focus_picker(),
            _ => {}
        }
    }

    pub fn on_paste(&mut self, text: String) {
        match self.focus {
            Focus::Picker => {
                let flat: String = text.chars().filter(|c| *c != '\n' && *c != '\r').collect();
                self.query_input.push_str(&flat);
                self.recompute();
                self.clamp_selection();
            }
            Focus::Editor => {
                self.buffer.insert_str(&text);
                self.buffer.message = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn wait_for_index(app: &mut App) {
        for _ in 0..300 {
            app.pump();
            if !app.files.is_empty() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("index never populated");
    }

    #[test]
    fn typing_a_pattern_then_enter_opens_that_file() {
        let dir = std::env::temp_dir().join(format!("peek-app-{}", std::process::id()));
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/index.rs"), "fn main() {}\n").unwrap();
        fs::write(dir.join("src/other.rs"), "x\n").unwrap();
        fs::write(dir.join("README.md"), "hi\n").unwrap();

        let mut app = App::new(dir.clone());
        wait_for_index(&mut app);
        app.pump();

        app.query_input = "index".into();
        app.recompute();
        assert_eq!(
            app.results.first().map(|hit| hit.path.as_str()),
            Some("src/index.rs")
        );

        app.open_selected();
        assert_eq!(app.focus, Focus::Editor);
        assert_eq!(
            app.buffer.path.as_deref(),
            Some(dir.join("src/index.rs").as_path())
        );
        assert!(app.query_input.is_empty());
        assert_eq!(app.buffer.lines[0], "fn main() {}");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn switching_files_warns_before_discarding_edits() {
        let dir = std::env::temp_dir().join(format!("peek-app-switch-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        fs::write(&a, "a\n").unwrap();
        fs::write(&b, "b\n").unwrap();

        let mut app = App::new(dir.clone());
        wait_for_index(&mut app);
        app.pump();

        assert!(app.load(&a, None));
        app.buffer.insert_str("precious ");
        assert!(app.buffer.dirty);

        assert!(!app.load(&b, None), "the first switch must refuse");
        assert_eq!(app.buffer.path.as_deref(), Some(a.as_path()));
        assert!(app.buffer.lines[0].starts_with("precious "));
        assert!(
            app.status.contains("unsaved changes"),
            "status was {:?}",
            app.status
        );

        assert!(app.load(&b, None), "picking it again confirms");
        assert_eq!(app.buffer.path.as_deref(), Some(b.as_path()));
        assert!(!app.buffer.dirty);
        assert_eq!(app.buffer.lines[0], "b");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn saving_first_lets_the_switch_through() {
        let dir = std::env::temp_dir().join(format!("peek-app-save-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        fs::write(&a, "a\n").unwrap();
        fs::write(&b, "b\n").unwrap();

        let mut app = App::new(dir.clone());
        wait_for_index(&mut app);
        app.pump();

        assert!(app.load(&a, None));
        app.buffer.insert_str("saved ");
        assert!(app.buffer.save().unwrap());

        assert!(app.load(&b, None), "a clean buffer switches freely");
        assert_eq!(app.buffer.path.as_deref(), Some(b.as_path()));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_refused_switch_does_not_move_the_history_cursor() {
        let dir = std::env::temp_dir().join(format!("peek-app-hist-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        fs::write(&a, "a\n").unwrap();
        fs::write(&b, "b\n").unwrap();

        let mut app = App::new(dir.clone());
        wait_for_index(&mut app);
        app.pump();

        assert!(app.load(&a, None));
        app.history.push(a.clone());
        assert!(app.load(&b, None));
        app.history.push(b.clone());
        app.hist_pos = 1;

        app.buffer.insert_str("edited ");
        app.go_history(-1);

        assert_eq!(app.buffer.path.as_deref(), Some(b.as_path()));
        assert_eq!(app.hist_pos, 1, "a refused step must not move the cursor");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tracks_every_file_edited_in_the_session() {
        let dir = std::env::temp_dir().join(format!("peek-app-edited-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        fs::write(&a, "a\n").unwrap();
        fs::write(&b, "b\n").unwrap();

        let mut app = App::new(dir.clone());
        wait_for_index(&mut app);
        app.pump();
        assert!(app.edited.is_empty(), "opening a file is not editing it");

        app.load(&a, None);
        app.buffer.insert_str("edited ");
        app.pump();
        assert_eq!(app.edited, vec![a.clone()]);

        // Switching away from a dirty buffer needs the second pick to confirm.
        assert!(!app.load(&b, None));
        assert!(app.load(&b, None));
        app.buffer.insert_str("also edited ");
        app.pump();
        assert_eq!(app.edited, vec![a.clone(), b.clone()]);

        // Editing the same file again must not duplicate it.
        assert!(!app.load(&a, None));
        assert!(app.load(&a, None));
        app.buffer.insert_str("more ");
        app.pump();
        assert_eq!(app.edited.len(), 2);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn buffer_search_jumps_to_the_matching_line() {
        let dir = std::env::temp_dir().join(format!("peek-app-line-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("notes.md"),
            "# notes\n\nsession: rehab appointment at 5\n\nend\n",
        )
        .unwrap();

        let mut app = App::new(dir.clone());
        wait_for_index(&mut app);
        app.pump();

        app.query_input = "notes".into();
        app.recompute();
        app.open_selected();
        assert_eq!(app.focus, Focus::Editor);

        // Edit something near the top, the way you would before remembering
        // the line you actually wanted.
        app.buffer.insert_str("edited ");
        assert!(app.buffer.lines[0].starts_with("edited "));

        app.focus_picker();
        app.query_input = ":l rehab".into();
        app.recompute();
        assert_eq!(app.line_hits.len(), 1);
        assert_eq!(app.line_hits[0].line_no, 3);

        app.open_selected();
        assert_eq!(app.focus, Focus::Editor);
        assert_eq!(app.buffer.row, 2);
        assert_eq!(app.buffer.col, 0);
        assert_eq!(
            app.buffer.path.as_deref(),
            Some(dir.join("notes.md").as_path())
        );
        assert!(app.buffer.lines[0].starts_with("edited "), "edit survived");
        assert!(app.query_input.is_empty());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn buffer_search_without_a_file_explains_itself() {
        let dir = std::env::temp_dir().join(format!("peek-app-nofile-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mut app = App::new(dir.clone());
        app.query_input = ":l rehab".into();
        app.recompute();
        assert!(app.line_hits.is_empty());
        assert!(
            app.line_msg
                .as_deref()
                .unwrap_or("")
                .contains("no file open")
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_query_lists_siblings_then_recents() {
        let dir = std::env::temp_dir().join(format!("peek-app-empty-{}", std::process::id()));
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/a.rs"), "a\n").unwrap();
        fs::write(dir.join("src/b.rs"), "b\n").unwrap();
        fs::write(dir.join("top.md"), "t\n").unwrap();

        let mut app = App::new(dir.clone());
        wait_for_index(&mut app);
        app.pump();

        app.query_input = "a.rs".into();
        app.recompute();
        app.open_selected();
        app.focus_picker();

        let listed: Vec<&str> = app.results.iter().map(|hit| hit.path.as_str()).collect();
        assert!(listed.contains(&"src/b.rs"), "siblings listed: {listed:?}");
        assert!(listed.contains(&"src/a.rs"), "recents listed: {listed:?}");

        fs::remove_dir_all(&dir).ok();
    }
}
