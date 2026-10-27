//! Problems the editor marks while you type: brackets and quotes left open,
//! and tables and columns the loaded catalog says do not exist. Like
//! completion it reads tokens, not a grammar, so it only says what it is
//! sure of: a name it cannot place, or whose schema is not loaded, is left
//! alone.

use std::ops::Range;
use std::time::{Duration, Instant};

use crate::completion::{self, Catalog, Options};
use crate::dialect::Family;
use crate::sql::{Token, TokenKind};

/// How sure a problem is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// The text cannot run as written.
    Error,
    /// The catalog says so, and it may be out of date.
    Warning,
}

/// Something wrong at a byte range of the text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub range: Range<usize>,
    pub message: String,
    pub severity: Severity,
}

/// Problems found for the text an editor shows, found again once the text
/// has stood still and now and then after, as the catalog fills in.
#[derive(Default)]
pub struct Tracker {
    /// The text last shown, which `found` is about once `fresh`.
    seen: String,
    /// When `seen` last changed.
    since: Option<Instant>,
    found: Vec<Diagnostic>,
    fresh: bool,
    /// When `found` was made.
    computed: Option<Instant>,
}

impl Tracker {
    /// How long the text stands still before it is read: typing a name
    /// leaves it unfinished for a while.
    pub const SETTLE: Duration = Duration::from_millis(400);
    /// How long what was found is trusted before looking again, the
    /// catalog having loaded more since.
    pub const REFRESH: Duration = Duration::from_secs(2);

    /// Notes that `text` is shown at `now`; true when its problems are due
    /// to be found, which [`Self::store`] then keeps.
    pub fn due(&mut self, text: &str, now: Instant) -> bool {
        if self.since.is_none() || self.seen != text {
            self.seen.clear();
            self.seen.push_str(text);
            self.since = Some(now);
            self.found.clear();
            self.fresh = false;
        }
        let settled = self
            .since
            .is_some_and(|since| now.duration_since(since) >= Self::SETTLE);
        let stale = self
            .computed
            .is_none_or(|at| now.duration_since(at) >= Self::REFRESH);
        settled && (!self.fresh || stale)
    }

    /// Keeps the problems of the text last shown.
    pub fn store(&mut self, found: Vec<Diagnostic>, now: Instant) {
        self.found = found;
        self.fresh = true;
        self.computed = Some(now);
    }

    /// How long until the text counts as settled, while it does not yet.
    pub fn wait(&self, now: Instant) -> Option<Duration> {
        if self.fresh {
            return None;
        }
        let since = self.since?;
        Self::SETTLE.checked_sub(now.duration_since(since))
    }

    /// The problems of the text last shown; none while it changes.
    pub fn found(&self) -> &[Diagnostic] {
        &self.found
    }

    /// The problems to mark with the cursor at `cursor`, less the one it
    /// is in the middle of writing.
    pub fn marks(&self, cursor: usize) -> Vec<(Range<usize>, Severity)> {
        self.found
            .iter()
            .filter(|found| !(found.range.start <= cursor && cursor <= found.range.end))
            .take(200)
            .map(|found| (found.range.clone(), found.severity))
            .collect()
    }
}

/// Every problem in `text`: syntax first, then names.
#[must_use]
pub fn diagnose(text: &str, catalog: &dyn Catalog, options: &Options<'_>) -> Vec<Diagnostic> {
    let mut found = syntax(text, options.family);
    found.extend(completion::unknown_names(text, catalog, options));
    found.sort_by_key(|diagnostic| diagnostic.range.start);
    found
}

/// Whether the lexer left the literal `token` open: a string, a quoted name
/// or a comment that runs to the end of the text.
fn unclosed(text: &str, token: &Token) -> bool {
    let body = &text[token.range.clone()];
    let Some(first) = body.chars().next() else {
        return false;
    };
    match token.kind {
        TokenKind::Comment => body.starts_with("/*") && !(body.len() >= 4 && body.ends_with("*/")),
        TokenKind::String if first == '$' => {
            let tag_end = body[1..].find('$').map_or(body.len(), |at| at + 2);
            let tag = &body[..tag_end];
            !(tag.ends_with('$') && body.len() >= 2 * tag.len() && body.ends_with(tag))
        }
        TokenKind::String | TokenKind::QuotedIdentifier => {
            let close = if first == '[' { ']' } else { first };
            body.len() < 2 || !body.ends_with(close)
        }
        _ => false,
    }
}

/// Brackets, quotes and block comments left open or closed twice. Reads
/// only the tokens, so it needs no catalog.
#[must_use]
pub fn syntax(text: &str, family: Family) -> Vec<Diagnostic> {
    let mut found = Vec::new();
    // The `(` still open in the statement being read.
    let mut open: Vec<Range<usize>> = Vec::new();
    let flush = |open: &mut Vec<Range<usize>>, found: &mut Vec<Diagnostic>| {
        found.extend(open.drain(..).map(|range| Diagnostic {
            range,
            message: "Unclosed parenthesis".to_owned(),
            severity: Severity::Error,
        }));
    };
    let (tokens, separators) = completion::lex(text, family);
    for token in tokens {
        let body = &text[token.range.clone()];
        if separators.contains(&token.range) {
            flush(&mut open, &mut found);
            continue;
        }
        match token.kind {
            TokenKind::Punctuation | TokenKind::Operator if body == "(" => {
                open.push(token.range.clone());
            }
            TokenKind::Punctuation | TokenKind::Operator if body == ")" => {
                if open.pop().is_none() {
                    found.push(Diagnostic {
                        range: token.range.clone(),
                        message: "Unmatched closing parenthesis".to_owned(),
                        severity: Severity::Error,
                    });
                }
            }
            _ if unclosed(text, &token) => {
                // The mark is the opening quote, not the rest of the text.
                let opening = body.char_indices().nth(1).map_or(body.len(), |(at, _)| at);
                let (end, what) = match token.kind {
                    TokenKind::Comment => (token.range.start + 2, "comment"),
                    TokenKind::QuotedIdentifier => (token.range.start + opening, "quoted name"),
                    _ if body.starts_with('$') => (
                        token.range.start + body[1..].find('$').map_or(body.len(), |at| at + 2),
                        "dollar-quoted string",
                    ),
                    _ => (token.range.start + opening, "string"),
                };
                found.push(Diagnostic {
                    range: token.range.start..end.min(token.range.end),
                    message: format!("Unclosed {what}"),
                    severity: Severity::Error,
                });
            }
            _ => {}
        }
    }
    flush(&mut open, &mut found);
    found.sort_by_key(|diagnostic| diagnostic.range.start);
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marked(text: &str) -> Vec<(&str, String)> {
        syntax(text, Family::Postgres)
            .into_iter()
            .map(|d| (&text[d.range], d.message))
            .collect()
    }

    fn is(found: Vec<(&str, String)>, expected: &[(&str, &str)]) -> bool {
        let found: Vec<(&str, &str)> = found.iter().map(|(a, b)| (*a, b.as_str())).collect();
        found == expected
    }

    #[test]
    fn problems_wait_for_the_text_to_settle_and_are_dropped_when_it_changes() {
        let start = Instant::now();
        let later = |ms: u64| start + Duration::from_millis(ms);
        let problem = Diagnostic {
            range: 7..10,
            message: "x".into(),
            severity: Severity::Warning,
        };
        let mut tracker = Tracker::default();
        assert!(!tracker.due("SELECT ", start));
        assert_eq!(tracker.wait(later(100)), Some(Duration::from_millis(300)));
        assert!(tracker.due("SELECT ", later(400)));
        tracker.store(vec![problem.clone()], later(400));
        assert_eq!(tracker.found().len(), 1);
        assert_eq!(tracker.wait(later(450)), None);
        // Nothing to do again until the problems are old.
        assert!(!tracker.due("SELECT ", later(1000)));
        assert!(tracker.due("SELECT ", later(2400)));
        // Typing drops them at once and starts the wait over.
        assert!(!tracker.due("SELECT a", later(2500)));
        assert!(tracker.found().is_empty());
        assert!(!tracker.due("SELECT a", later(2800)));
        assert!(tracker.due("SELECT a", later(2900)));
        // The word being written is not marked.
        tracker.store(vec![problem], later(2900));
        assert_eq!(tracker.marks(9).len(), 0);
        assert_eq!(tracker.marks(11).len(), 1);
    }

    #[test]
    fn balanced_text_has_no_problems() {
        assert!(marked("SELECT count(*), (a + b) FROM t WHERE x IN (1, 2);").is_empty());
        assert!(marked("SELECT 'it''s', \"a\"\"b\", $$ ( $$, $t$ ' $t$ -- ( \n/* ) */").is_empty());
        assert!(marked("").is_empty());
    }

    #[test]
    fn brackets_left_open_or_closed_twice_are_marked() {
        assert!(is(
            marked("SELECT count(a FROM t"),
            &[("(", "Unclosed parenthesis")]
        ));
        assert!(is(
            marked("SELECT a) FROM t"),
            &[(")", "Unmatched closing parenthesis")]
        ));
        // A statement's end closes nothing for the next one.
        assert!(is(
            marked("SELECT (a; SELECT (b)"),
            &[("(", "Unclosed parenthesis")]
        ));
    }

    #[test]
    fn quotes_left_open_mark_only_the_opening() {
        assert!(is(
            marked("SELECT 'abc FROM t"),
            &[("'", "Unclosed string")]
        ));
        assert!(is(
            marked("SELECT \"abc FROM t"),
            &[("\"", "Unclosed quoted name")]
        ));
        assert!(is(
            marked("SELECT $a$ abc"),
            &[("$a$", "Unclosed dollar-quoted string")]
        ));
        assert!(is(marked("SELECT 1 /* abc"), &[("/*", "Unclosed comment")]));
        // A parenthesis inside an open string is text.
        assert!(is(marked("SELECT '( abc"), &[("'", "Unclosed string")]));
        // A line comment ends with its line, so it is never open.
        assert!(marked("SELECT 1 -- (").is_empty());
    }
}
