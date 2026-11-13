//! `:f` — search inside file contents.
//!
//! Runs on a background thread and is aborted the moment the query changes, so
//! typing never waits for a scan. In this mode a bare term means substring
//! (that is what people expect from grep), `/.../` means regex, and `"..."` is
//! the same substring but keeps the list from re-ranking weirdly.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;

use ignore::WalkBuilder;
use regex::{Regex, RegexBuilder};

use crate::query::{Kind, Term};

const MAX_HITS: usize = 400;
const MAX_FILE_BYTES: u64 = 1_048_576;
const MAX_LINE_CHARS: usize = 300;

#[derive(Debug, Clone)]
pub struct ContentHit {
    pub path: String,
    pub line_no: usize,
    pub snippet: String,
    /// Char ranges inside `snippet`.
    pub ranges: Vec<(u32, u32)>,
}

pub struct ContentResult {
    pub generation: u64,
    pub hits: Vec<ContentHit>,
    pub truncated: bool,
}

pub struct Spec {
    pub root: PathBuf,
    pub scope: Option<String>,
    pub terms: Vec<Term>,
}

enum LineMatcher {
    Sub { needle: String, cs: bool },
    Re(Regex),
}

impl LineMatcher {
    fn matches(&self, line: &str) -> Option<(u32, u32)> {
        match self {
            LineMatcher::Sub { needle, cs } => {
                if *cs {
                    let pos = line.find(needle)?;
                    Some((
                        line[..pos].chars().count() as u32,
                        needle.chars().count() as u32,
                    ))
                } else {
                    let hay = line.to_lowercase();
                    let pos = hay.find(needle)?;
                    Some((
                        hay[..pos].chars().count() as u32,
                        needle.chars().count() as u32,
                    ))
                }
            }
            LineMatcher::Re(re) => {
                let m = re.find(line)?;
                Some((
                    line[..m.start()].chars().count() as u32,
                    line[m.start()..m.end()].chars().count() as u32,
                ))
            }
        }
    }
}

/// A compiled set of terms, reusable for matching single lines. `:f` runs it
/// over the whole project on a worker thread; `:l` runs it over the open
/// buffer on the spot, which is why both share this type.
pub struct LineQuery {
    positives: Vec<LineMatcher>,
    negatives: Vec<LineMatcher>,
}

impl LineQuery {
    pub fn new(terms: &[Term]) -> Self {
        LineQuery {
            positives: terms
                .iter()
                .filter(|term| !term.negated)
                .filter_map(build)
                .collect(),
            negatives: terms
                .iter()
                .filter(|term| term.negated)
                .filter_map(build)
                .collect(),
        }
    }

    /// Ranges of every positive term on this line, or `None` when the line
    /// does not satisfy the whole query.
    pub fn matches(&self, line: &str) -> Option<Vec<(u32, u32)>> {
        if self.positives.is_empty() {
            return None;
        }
        let ranges: Vec<(u32, u32)> = self
            .positives
            .iter()
            .filter_map(|matcher| matcher.matches(line))
            .collect();
        if ranges.len() != self.positives.len() {
            return None;
        }
        if self
            .negatives
            .iter()
            .any(|matcher| matcher.matches(line).is_some())
        {
            return None;
        }
        Some(ranges)
    }
}

pub fn spawn(spec: Spec, generation: u64, gens: Arc<AtomicU64>, tx: Sender<ContentResult>) {
    std::thread::spawn(move || {
        let result = run(spec, generation, &gens);
        let _ = tx.send(result);
    });
}

fn run(spec: Spec, generation: u64, gens: &AtomicU64) -> ContentResult {
    let query = LineQuery::new(&spec.terms);

    let mut hits: Vec<ContentHit> = Vec::new();
    let mut truncated = false;

    for entry in WalkBuilder::new(&spec.root)
        .require_git(false)
        .build()
        .flatten()
    {
        if gens.load(Ordering::Relaxed) != generation {
            break;
        }
        let is_file = entry.file_type().map(|t| t.is_file()).unwrap_or(false);
        if !is_file {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(&spec.root) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if let Some(scope) = &spec.scope
            && !crate::index::in_scope(&rel, scope)
        {
            continue;
        }
        let too_big = entry
            .metadata()
            .map(|m| m.len() > MAX_FILE_BYTES)
            .unwrap_or(true);
        if too_big {
            continue;
        }
        let Ok(bytes) = std::fs::read(entry.path()) else {
            continue;
        };
        if bytes.iter().take(4096).any(|b| *b == 0) {
            continue; // binary
        }
        let text = String::from_utf8_lossy(&bytes);

        for (i, line) in text.split('\n').enumerate() {
            let line = line.strip_suffix('\r').unwrap_or(line);
            let Some(ranges) = query.matches(line) else {
                continue;
            };
            let snippet: String = line.chars().take(MAX_LINE_CHARS).collect();
            hits.push(ContentHit {
                path: rel.clone(),
                line_no: i + 1,
                snippet,
                ranges,
            });
            if hits.len() >= MAX_HITS {
                truncated = true;
                break;
            }
        }
        if truncated {
            break;
        }
    }

    ContentResult {
        generation,
        hits,
        truncated,
    }
}

fn build(term: &Term) -> Option<LineMatcher> {
    match term.kind {
        Kind::Regex => RegexBuilder::new(&term.text)
            .case_insensitive(!term.case_sensitive)
            .build()
            .ok()
            .map(LineMatcher::Re),
        Kind::Fuzzy | Kind::Literal => Some(LineMatcher::Sub {
            needle: if term.case_sensitive {
                term.text.clone()
            } else {
                term.text.to_lowercase()
            },
            cs: term.case_sensitive,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::Query;

    fn matchers(q: &str) -> (Vec<LineMatcher>, Vec<LineMatcher>) {
        let parsed = Query::parse(q);
        let pos = parsed
            .terms
            .iter()
            .filter(|t| !t.negated)
            .filter_map(build)
            .collect();
        let neg = parsed
            .terms
            .iter()
            .filter(|t| t.negated)
            .filter_map(build)
            .collect();
        (pos, neg)
    }

    #[test]
    fn bare_term_is_substring_in_content_mode() {
        let (pos, _) = matchers(":f token");
        assert!(pos[0].matches("let token = 1;").is_some());
        assert!(pos[0].matches("tok en").is_none());
    }

    #[test]
    fn case_insensitive_by_default() {
        let (pos, _) = matchers(":f TOKEN");
        assert!(pos.is_empty() || pos[0].matches("TOKEN").is_some());
        let (pos, _) = matchers(":f token");
        assert!(pos[0].matches("TOKEN here").is_some());
    }

    #[test]
    fn uppercase_makes_it_sensitive() {
        let (pos, _) = matchers(":f Token");
        assert!(pos[0].matches("Token here").is_some());
        assert!(pos[0].matches("token here").is_none());
    }

    #[test]
    fn regex_term() {
        let (pos, _) = matchers(":f /t.k.n/");
        let m = pos[0].matches("xx token xx").unwrap();
        assert_eq!(m, (3, 5));
    }

    #[test]
    fn buffer_search_finds_the_line() {
        let query = LineQuery::new(&Query::parse(":l rehab").terms);
        let hit = query.matches("notes from the rehab session").unwrap();
        assert_eq!(hit, vec![(15, 5)]);
        assert!(query.matches("nothing here").is_none());
    }

    #[test]
    fn buffer_search_is_case_insensitive_and_regex_capable() {
        let lower = LineQuery::new(&Query::parse(":l /rehab/").terms);
        assert!(lower.matches("REHAB starts monday").is_some());
        let upper = LineQuery::new(&Query::parse(":l /REHAB/").terms);
        assert!(upper.matches("REHAB starts monday").is_some());
        assert!(upper.matches("rehab starts monday").is_none());
        let plain = LineQuery::new(&Query::parse(":l rehab").terms);
        assert!(plain.matches("REHAB starts monday").is_some());
    }

    #[test]
    fn negation_builds_negative_matcher() {
        let (pos, neg) = matchers(":f token !test");
        assert_eq!(pos.len(), 1);
        assert_eq!(neg.len(), 1);
    }
}
