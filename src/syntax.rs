//! Syntax highlighting.
//!
//! Grammars come from syntect (Sublime Text definitions), but colours are
//! mapped onto the terminal's own 16-colour palette instead of hardcoded RGB.
//! A theme baked for a dark background looks wrong on a light terminal, and
//! hardcoding one is the classic way a TUI ends up unreadable for half its
//! users. Mapping by hue and lightness keeps comments/strings/keywords
//! distinguishable while still respecting whatever palette is configured.

use std::path::Path;

use ratatui::style::Color;
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Style as SynStyle, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

/// Above this the file is treated as a log rather than source: highlighting
/// would cost more than it is worth, so it stays plain.
const MAX_LINES: usize = 10_000;

/// One run of text that shares a style, inside a single line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub text: String,
    pub color: Color,
    pub bold: bool,
    pub italic: bool,
}

pub struct Highlighter {
    syntaxes: SyntaxSet,
    theme: Theme,
}

impl Default for Highlighter {
    fn default() -> Self {
        Self::new()
    }
}

impl Highlighter {
    pub fn new() -> Self {
        Highlighter {
            syntaxes: SyntaxSet::load_defaults_newlines(),
            theme: ThemeSet::load_defaults().themes["base16-ocean.dark"].clone(),
        }
    }

    /// Style every line. Returns an empty vector for things we cannot or should
    /// not highlight (no path, unknown extension, very large files), in which
    /// case the caller renders plain text.
    pub fn highlight(&self, path: Option<&Path>, lines: &[String]) -> Vec<Vec<Segment>> {
        let Some(path) = path else {
            return Vec::new();
        };
        if lines.len() > MAX_LINES {
            return Vec::new();
        }
        let Ok(Some(syntax)) = self.syntaxes.find_syntax_for_file(path) else {
            return Vec::new();
        };

        let mut state = HighlightLines::new(syntax, &self.theme);
        let mut out: Vec<Vec<Segment>> = Vec::with_capacity(lines.len());

        for line in lines {
            let mut text = String::with_capacity(line.len() + 1);
            text.push_str(line);
            text.push('\n');

            let Ok(ranges) = state.highlight_line(&text, &self.syntaxes) else {
                out.push(vec![Segment {
                    text: line.clone(),
                    color: Color::Reset,
                    bold: false,
                    italic: false,
                }]);
                continue;
            };

            let mut segments: Vec<Segment> = Vec::new();
            for (style, piece) in ranges {
                let piece = piece.trim_end_matches(|c| c == '\n' || c == '\r');
                if piece.is_empty() {
                    continue;
                }
                segments.push(Segment {
                    text: piece.to_string(),
                    color: ansi_for(style),
                    bold: style.font_style.contains(FontStyle::BOLD),
                    italic: style.font_style.contains(FontStyle::ITALIC),
                });
            }
            out.push(segments);
        }

        out
    }
}

fn ansi_for(style: SynStyle) -> Color {
    let c = style.foreground;
    let (hue, saturation, lightness) = hsl(c.r, c.g, c.b);

    if saturation < 0.16 {
        return if lightness > 0.66 {
            Color::White
        } else if lightness > 0.42 {
            Color::Gray
        } else {
            Color::DarkGray
        };
    }

    let bright = lightness > 0.70;
    match hue {
        h if !(20.0..340.0).contains(&h) => {
            if bright { Color::LightRed } else { Color::Red }
        }
        h if h < 45.0 => {
            if bright { Color::LightYellow } else { Color::Red }
        }
        h if h < 70.0 => {
            if bright { Color::LightYellow } else { Color::Yellow }
        }
        h if h < 165.0 => {
            if bright { Color::LightGreen } else { Color::Green }
        }
        h if h < 195.0 => {
            if bright { Color::LightCyan } else { Color::Cyan }
        }
        h if h < 262.0 => {
            if bright { Color::LightBlue } else { Color::Blue }
        }
        h if h < 320.0 => {
            if bright { Color::LightMagenta } else { Color::Magenta }
        }
        _ => {
            if bright { Color::LightRed } else { Color::Red }
        }
    }
}

/// Hue in degrees, saturation and lightness in 0..=1.
fn hsl(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    let (r, g, b) = (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let lightness = (max + min) / 2.0;
    let delta = max - min;
    if delta <= f32::EPSILON {
        return (0.0, 0.0, lightness);
    }

    let saturation = if lightness > 0.5 {
        delta / (2.0 - max - min)
    } else {
        delta / (max + min)
    };

    let hue = if max == r {
        60.0 * (((g - b) / delta) % 6.0)
    } else if max == g {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };

    (if hue < 0.0 { hue + 360.0 } else { hue }, saturation, lightness)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn plain(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|l| l.to_string()).collect()
    }

    #[test]
    fn segments_rebuild_the_line_exactly() {
        let highlighter = Highlighter::new();
        let lines = plain(&[
            "fn main() {",
            "    let greeting = \"hello\"; // say hi",
            "",
            "    println!(\"{greeting}\");",
            "}",
        ]);
        let styled = highlighter.highlight(Some(&PathBuf::from("demo.rs")), &lines);
        assert_eq!(styled.len(), lines.len());
        for (line, segments) in lines.iter().zip(&styled) {
            let rebuilt: String = segments.iter().map(|s| s.text.as_str()).collect();
            assert_eq!(&rebuilt, line, "segments must cover the line exactly");
        }
    }

    #[test]
    fn keywords_strings_and_comments_differ() {
        let highlighter = Highlighter::new();
        let lines = plain(&["fn go() { let x = \"text\"; } // note"]);
        let styled = highlighter.highlight(Some(&PathBuf::from("demo.rs")), &lines);

        let color_of = |needle: &str| {
            styled[0]
                .iter()
                .find(|segment| segment.text.contains(needle))
                .map(|segment| segment.color)
        };
        let keyword = color_of("fn");
        let string = color_of("text");
        let comment = color_of("note");
        assert!(keyword.is_some() && string.is_some() && comment.is_some());
        assert_ne!(keyword, string);
        assert_ne!(string, comment);
    }

    #[test]
    fn unknown_files_stay_plain() {
        let highlighter = Highlighter::new();
        let styled = highlighter.highlight(Some(&PathBuf::from("notes.zzz")), &plain(&["x"]));
        assert!(styled.is_empty());
        assert!(highlighter.highlight(None, &plain(&["x"])).is_empty());
    }

    #[test]
    fn oversized_files_are_not_highlighted() {
        let highlighter = Highlighter::new();
        let lines = vec!["x".to_string(); MAX_LINES + 1];
        assert!(highlighter.highlight(Some(&PathBuf::from("big.rs")), &lines).is_empty());
    }

    #[test]
    fn palette_mapping_is_sane() {
        assert_eq!(ansi_for(syn(0x00, 0x00, 0x00)), Color::DarkGray);
        assert_eq!(ansi_for(syn(0xff, 0xff, 0xff)), Color::White);
        assert_eq!(ansi_for(syn(0xb0, 0xb0, 0xb0)), Color::White);
        assert_eq!(ansi_for(syn(0x90, 0x90, 0x90)), Color::Gray);
        assert_eq!(ansi_for(syn(0x60, 0x60, 0x60)), Color::DarkGray);
        assert_eq!(ansi_for(syn(0x00, 0x00, 0xff)), Color::Blue);
        assert_eq!(ansi_for(syn(0x00, 0xff, 0x00)), Color::Green);
        assert_eq!(ansi_for(syn(0xff, 0x00, 0x00)), Color::Red);
    }

    fn syn(r: u8, g: u8, b: u8) -> SynStyle {
        SynStyle {
            foreground: syntect::highlighting::Color { r, g, b, a: 0xff },
            background: syntect::highlighting::Color {
                r: 0,
                g: 0,
                b: 0,
                a: 0xff,
            },
            font_style: FontStyle::empty(),
        }
    }
}
