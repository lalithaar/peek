//! The query line grammar.
//!
//! One input line, two modes:
//!
//! ```text
//! index            filter file *paths*
//! :f token         filter file *contents*
//! :l token         filter the lines of the *open file*
//! ```
//!
//! Same term shapes in both modes:
//!
//! ```text
//! bare             fuzzy on paths, substring on contents
//! "literal"        substring, exactly as typed
//! /regex/          regular expression
//! !term            exclude anything that matches
//! dir/             restrict to a directory (token ending in `/`)
//! ```
//!
//! Lowercase terms match case-insensitively; a term containing an uppercase
//! character becomes case-sensitive (smartcase), so you never toggle a flag.

use regex::RegexBuilder;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Match against file paths.
    Files,
    /// Match against line contents across the project (`:f`).
    Content,
    /// Match against line contents of the open file only (`:l`).
    Buffer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Fuzzy,
    Literal,
    Regex,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Term {
    pub kind: Kind,
    pub text: String,
    pub negated: bool,
    pub case_sensitive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub raw: String,
    pub mode: Mode,
    pub scope: Option<String>,
    pub terms: Vec<Term>,
    /// A regex that does not compile. Non-fatal: the caller keeps showing the
    /// previous results and displays this next to the query line.
    pub error: Option<String>,
}

impl Default for Query {
    fn default() -> Self {
        Query {
            raw: String::new(),
            mode: Mode::Files,
            scope: None,
            terms: Vec::new(),
            error: None,
        }
    }
}

impl Query {
    pub fn parse(raw: &str) -> Query {
        let mut q = Query {
            raw: raw.to_string(),
            ..Query::default()
        };

        let mut body = raw;
        for (prefix, mode) in [(":f", Mode::Content), (":l", Mode::Buffer)] {
            if let Some(after) = raw.strip_prefix(prefix)
                && (after.is_empty() || after.starts_with(char::is_whitespace))
            {
                q.mode = mode;
                body = after.trim_start();
                break;
            }
        }

        let chars: Vec<char> = body.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if chars[i].is_whitespace() {
                i += 1;
                continue;
            }

            let mut negated = false;
            if chars[i] == '!' && i + 1 < chars.len() && !chars[i + 1].is_whitespace() {
                negated = true;
                i += 1;
            }
            if i >= chars.len() {
                break;
            }

            match chars[i] {
                '"' => {
                    i += 1;
                    let mut text = String::new();
                    while i < chars.len() && chars[i] != '"' {
                        if chars[i] == '\\' && i + 1 < chars.len() {
                            i += 1;
                        }
                        text.push(chars[i]);
                        i += 1;
                    }
                    if i < chars.len() {
                        i += 1; // closing quote
                    }
                    // An unterminated quote is just you mid-keystroke: treat it
                    // as a literal so the list keeps filtering while typing.
                    push_term(&mut q, Kind::Literal, text, negated);
                }
                '/' => {
                    let start = i + 1;
                    let mut end = None;
                    let mut j = start;
                    while j < chars.len() {
                        if chars[j] == '\\' {
                            j += 2;
                            continue;
                        }
                        if chars[j] == '/' {
                            end = Some(j);
                            break;
                        }
                        j += 1;
                    }
                    match end {
                        Some(e) => {
                            let text: String = chars[start..e].iter().collect();
                            i = e + 1;
                            push_term(&mut q, Kind::Regex, text, negated);
                        }
                        None => {
                            // Still typing the regex: filter as a literal
                            // instead of blanking the list.
                            let text: String = chars[start..].iter().collect();
                            i = chars.len();
                            push_term(&mut q, Kind::Fuzzy, text, negated);
                        }
                    }
                }
                _ => {
                    let start = i;
                    while i < chars.len() && !chars[i].is_whitespace() {
                        i += 1;
                    }
                    let text: String = chars[start..i].iter().collect();
                    if !negated && text.len() > 1 && text.ends_with('/') {
                        let scope = text
                            .replace('\\', "/")
                            .trim_start_matches("./")
                            .trim_start_matches('/')
                            .to_string();
                        if q.scope.is_none() && !scope.is_empty() {
                            q.scope = Some(scope);
                        } else {
                            push_term(&mut q, Kind::Fuzzy, text, false);
                        }
                    } else {
                        push_term(&mut q, Kind::Fuzzy, text, negated);
                    }
                }
            }
        }

        q
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty() && self.scope.is_none()
    }
}

fn push_term(q: &mut Query, kind: Kind, text: String, negated: bool) {
    if text.is_empty() {
        return;
    }
    let case_sensitive = text.chars().any(|c| c.is_uppercase());
    if kind == Kind::Regex {
        match RegexBuilder::new(&text)
            .case_insensitive(!case_sensitive)
            .build()
        {
            Ok(_) => {}
            Err(err) => {
                if q.error.is_none() {
                    q.error = Some(format!("bad regex: {err}"));
                }
                return;
            }
        }
    }
    q.terms.push(Term {
        kind,
        text,
        negated,
        case_sensitive,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query() {
        let q = Query::parse("");
        assert!(q.is_empty());
        assert_eq!(q.mode, Mode::Files);
    }

    #[test]
    fn bare_term_is_fuzzy() {
        let q = Query::parse("index");
        assert_eq!(q.terms.len(), 1);
        assert_eq!(q.terms[0].kind, Kind::Fuzzy);
        assert_eq!(q.terms[0].text, "index");
        assert!(!q.terms[0].case_sensitive);
    }

    #[test]
    fn quotes_are_literal() {
        let q = Query::parse("\"index\"");
        assert_eq!(q.terms[0].kind, Kind::Literal);
        assert_eq!(q.terms[0].text, "index");
    }

    #[test]
    fn slash_is_regex() {
        let q = Query::parse("/idx.*\\.rs/");
        assert_eq!(q.terms[0].kind, Kind::Regex);
        assert_eq!(q.terms[0].text, "idx.*\\.rs");
        assert!(q.error.is_none());
    }

    #[test]
    fn broken_regex_is_reported_not_fatal() {
        let q = Query::parse("/[/");
        assert!(q.error.is_some());
        assert!(q.terms.is_empty());
    }

    #[test]
    fn unterminated_regex_still_filters_as_text() {
        let q = Query::parse("/ind");
        assert!(q.error.is_none());
        assert_eq!(q.terms[0].kind, Kind::Fuzzy);
        assert_eq!(q.terms[0].text, "ind");
    }

    #[test]
    fn unterminated_quote_still_filters_as_text() {
        let q = Query::parse("\"ind");
        assert!(q.error.is_none());
        assert_eq!(q.terms[0].kind, Kind::Literal);
        assert_eq!(q.terms[0].text, "ind");
    }

    #[test]
    fn scope_and_term() {
        let q = Query::parse("auth/ \"token\"");
        assert_eq!(q.scope.as_deref(), Some("auth/"));
        assert_eq!(q.terms.len(), 1);
        assert_eq!(q.terms[0].kind, Kind::Literal);
    }

    #[test]
    fn content_mode() {
        let q = Query::parse(":f token");
        assert_eq!(q.mode, Mode::Content);
        assert_eq!(q.terms[0].text, "token");
    }

    #[test]
    fn content_mode_with_scope_and_regex() {
        let q = Query::parse(":f auth/ /tok.en/");
        assert_eq!(q.mode, Mode::Content);
        assert_eq!(q.scope.as_deref(), Some("auth/"));
        assert_eq!(q.terms[0].kind, Kind::Regex);
    }

    #[test]
    fn buffer_mode() {
        let q = Query::parse(":l rehab");
        assert_eq!(q.mode, Mode::Buffer);
        assert_eq!(q.terms[0].text, "rehab");
        assert_eq!(q.terms[0].kind, Kind::Fuzzy);
    }

    #[test]
    fn buffer_mode_accepts_the_same_shapes() {
        let q = Query::parse(":l /rehab|physio/ !draft");
        assert_eq!(q.mode, Mode::Buffer);
        assert_eq!(q.terms[0].kind, Kind::Regex);
        assert!(q.terms[1].negated);
    }

    #[test]
    fn negation() {
        let q = Query::parse("!test");
        assert!(q.terms[0].negated);
    }

    #[test]
    fn smartcase() {
        assert!(!Query::parse("index").terms[0].case_sensitive);
        assert!(Query::parse("Index").terms[0].case_sensitive);
    }

    #[test]
    fn multiple_terms_and() {
        let q = Query::parse("src tok");
        assert_eq!(q.terms.len(), 2);
    }

    #[test]
    fn escaped_quote_inside_literal() {
        let q = Query::parse("\"a\\\"b\"");
        assert_eq!(q.terms[0].text, "a\"b");
    }
}
