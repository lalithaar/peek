//! peek — a terminal editor where you type a pattern, not a path.
//!
//! Launch it with no arguments to index the current directory, or pass a path.

mod app;
mod editor;
mod git;
mod grep;
mod index;
mod query;
mod syntax;
mod ui;

use std::io::Write;
use std::path::PathBuf;

use ratatui::crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use ratatui::crossterm::execute;

fn main() -> std::io::Result<()> {
    let arg = std::env::args().nth(1);
    let root = match arg {
        Some(path) => PathBuf::from(path),
        None => std::env::current_dir()?,
    };
    let root = std::fs::canonicalize(&root).unwrap_or(root);

    let mut app = app::App::new(root);

    let mut stdout = std::io::stdout();
    let _ = execute!(stdout, EnableBracketedPaste);
    let mut terminal = ratatui::init();
    let result = app.run(&mut terminal);
    ratatui::restore();
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    stdout.flush().ok();

    result
}
