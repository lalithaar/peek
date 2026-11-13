//! Project file index: a background gitignore-aware walk, plus the path
//! matcher that turns a query into ranked file hits.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};

use ignore::WalkBuilder;
use nucleo_matcher::{Config, Matcher, Utf32Str};
use regex::{Regex, RegexBuilder};

use crate::query::{Kind, Query, Term};

pub enum WalkMsg {
    Batch(Vec<String>),
    Done,
}

/// Walks `root` in the background, streaming batches of relative paths so the
/// picker is usable on a small repo before the walk of a huge one finishes.
pub fn spawn_walk(root: PathBuf) -> Receiver<WalkMsg> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        // Honour .gitignore even outside a git checkout: people keep the same
        // ignore file in plain folders, and indexing `target/` or `node_modules/`
        // just because there is no `.git` is never what anyone wants.
        let walker = WalkBuilder::new(&root).require_git(false).build();
        let mut batch: Vec<String> = Vec::with_capacity(2048);
        for entry in walker.flatten() {
            let is_file = entry.file_type().map(|t| t.is_file()).unwrap_or(false);
            if !is_file {
                continue;
            }
            let Ok(rel) = entry.path().strip_prefix(&root) else {
                continue;
            };
            batch.push(rel.to_string_lossy().replace('\\', "/"));
            if batch.len() >= 2048 {
                if tx.send(WalkMsg::Batch(std::mem::take(&mut batch))).is_err() {
                    return;
                }
            }
        }
        if !batch.is_empty() {
            let _ = tx.send(WalkMsg::Batch(batch));
        }
        let _ = tx.send(WalkMsg::Done);
    });
    rx
}

#[derive(Debug, Clone)]
pub struct Hit {
    pub path: String,
    pub score: u32,
    /// Char offsets into `path`, for highlighting.
    pub indices: Vec<u32>,
}

pub struct FileFilter {
    ic: Matcher,
    cs: Matcher,
    cand: Vec<char>,
    needle: Vec<char>,
    regexes: HashMap<String, Regex>,
}

impl Default for FileFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl FileFilter {
    pub fn new() -> Self {
        let mut strict = Config::DEFAULT.match_paths();
        strict.ignore_case = false;
        FileFilter {
            ic: Matcher::new(Config::DEFAULT.match_paths()),
            cs: Matcher::new(strict),
            cand: Vec::new(),
            needle: Vec::new(),
            regexes: HashMap::new(),
        }
    }
}

/// Rank `files` against a query. Terms are ANDed; each positive term must
/// match and their scores add up.
pub fn filter(files: &[String], q: &Query, f: &mut FileFilter, limit: usize) -> Vec<Hit> {
    let mut out: Vec<Hit> = Vec::new();
    for path in files {
        if let Some(scope) = &q.scope
            && !in_scope(path, scope)
        {
            continue;
        }

        let mut total: u32 = 0;
        let mut indices: Vec<u32> = Vec::new();
        let mut keep = true;
        for term in &q.terms {
            match (term.negated, match_path(path, term, f, &mut indices)) {
                (true, Some(_)) => {
                    keep = false;
                    break;
                }
                (true, None) => {}
                (false, Some(score)) => total = total.saturating_add(score),
                (false, None) => {
                    keep = false;
                    break;
                }
            }
        }
        if keep {
            indices.sort_unstable();
            indices.dedup();
            out.push(Hit {
                path: path.clone(),
                score: total,
                indices,
            });
        }
    }

    out.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.path.len().cmp(&b.path.len()))
            .then_with(|| a.path.cmp(&b.path))
    });
    out.truncate(limit);
    out
}

fn match_path(path: &str, term: &Term, f: &mut FileFilter, indices: &mut Vec<u32>) -> Option<u32> {
    match term.kind {
        Kind::Fuzzy => {
            f.cand.clear();
            let hay = Utf32Str::new(path, &mut f.cand);
            f.needle.clear();
            let needle = Utf32Str::new(&term.text, &mut f.needle);
            let matcher = if term.case_sensitive {
                &mut f.cs
            } else {
                &mut f.ic
            };
            matcher
                .fuzzy_indices(hay, needle, indices)
                .map(|score| score as u32)
        }
        Kind::Literal => {
            let hay = if term.case_sensitive {
                path.to_string()
            } else {
                path.to_lowercase()
            };
            let needle = if term.case_sensitive {
                term.text.clone()
            } else {
                term.text.to_lowercase()
            };
            let pos = hay.find(&needle)?;
            let start = hay[..pos].chars().count() as u32;
            let len = needle.chars().count() as u32;
            indices.extend(start..start + len);

            let boundary = pos == 0 || hay[..pos].ends_with('/');
            let base = hay.rsplit('/').next().unwrap_or(&hay);
            let score = 800
                + if boundary { 400 } else { 0 }
                + if base.starts_with(&needle) {
                    200
                } else if base.contains(&needle) {
                    100
                } else {
                    0
                };
            Some(score as u32)
        }
        Kind::Regex => {
            let key = format!("{}|{}", u8::from(term.case_sensitive), term.text);
            if !f.regexes.contains_key(&key) {
                let re = RegexBuilder::new(&term.text)
                    .case_insensitive(!term.case_sensitive)
                    .build()
                    .ok()?;
                f.regexes.insert(key.clone(), re);
            }
            let re = f.regexes.get(&key)?;
            let m = re.find(path)?;
            let start = path[..m.start()].chars().count() as u32;
            let len = path[m.start()..m.end()].chars().count() as u32;
            indices.extend(start..start + len);
            let boundary = m.start() == 0 || path[..m.start()].ends_with('/');
            Some(600 + if boundary { 300 } else { 0 })
        }
    }
}

pub fn in_scope(path: &str, scope: &str) -> bool {
    if starts_with_ci(path, scope) {
        return true;
    }
    // A scope names a directory, not a necessarily root-level one: `auth/`
    // matches `src/auth/token.rs` as well as `auth/token.rs`.
    let mut from = 0;
    while let Some(pos) = path[from..].find('/') {
        let next = from + pos + 1;
        if starts_with_ci(&path[next..], scope) {
            return true;
        }
        from = next;
    }
    false
}

fn starts_with_ci(path: &str, prefix: &str) -> bool {
    match path.get(..prefix.len()) {
        Some(head) => head.eq_ignore_ascii_case(prefix),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files() -> Vec<String> {
        vec![
            "src/index.rs".into(),
            "src/main.rs".into(),
            "src/auth/token.rs".into(),
            "src/auth/session.rs".into(),
            "docs/index.md".into(),
            "test_index.go".into(),
        ]
    }

    fn run(q: &str) -> Vec<String> {
        let parsed = Query::parse(q);
        let mut f = FileFilter::new();
        filter(&files(), &parsed, &mut f, 50)
            .into_iter()
            .map(|h| h.path)
            .collect()
    }

    #[test]
    fn fuzzy_prefers_basename_start() {
        let hits = run("index");
        assert_eq!(hits.first().map(String::as_str), Some("src/index.rs"));
        assert!(hits.contains(&"docs/index.md".to_string()));
        assert!(hits.contains(&"test_index.go".to_string()));
    }

    #[test]
    fn literal_is_exact_substring() {
        let hits = run("\"index\"");
        assert!(hits.iter().all(|h| h.contains("index")));
        assert!(!hits.iter().any(|h| h.contains("main")));
    }

    #[test]
    fn regex_matches() {
        let hits = run("/^src\\/auth\\//");
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.starts_with("src/auth/")));
    }

    #[test]
    fn scope_restricts() {
        let hits = run("auth/ /./");
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.starts_with("src/auth/")));
    }

    #[test]
    fn scope_can_be_nested_or_rooted() {
        assert_eq!(run("src/auth/ /./").len(), 2);
        assert_eq!(run("src/ index"), vec!["src/index.rs".to_string()]);
        assert!(run("docs/ /./").iter().all(|h| h.starts_with("docs/")));
    }

    #[test]
    fn negation_excludes() {
        let hits = run("index !test");
        assert!(!hits.iter().any(|h| h.contains("test_index")));
        assert!(!hits.is_empty());
    }

    #[test]
    fn terms_are_anded() {
        let hits = run("src tok");
        assert_eq!(hits, vec!["src/auth/token.rs".to_string()]);
    }

    #[test]
    fn smartcase_is_respected() {
        let mut f = FileFilter::new();
        let lower = filter(&files(), &Query::parse("\"INDEX\""), &mut f, 50);
        assert!(lower.is_empty());
        let upper = filter(&files(), &Query::parse("\"index\""), &mut f, 50);
        assert!(!upper.is_empty());
    }

    #[test]
    fn indices_point_into_the_path() {
        let mut f = FileFilter::new();
        let hits = filter(&files(), &Query::parse("\"index\""), &mut f, 50);
        let hit = &hits[0];
        let chars: Vec<char> = hit.path.chars().collect();
        let highlighted: String = hit.indices.iter().map(|i| chars[*i as usize]).collect();
        assert_eq!(highlighted, "index");
    }
}
