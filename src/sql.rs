//! A small SQL lexer: enough to colour the editor and to find the statement
//! under the cursor. It never fails; text it does not understand is plain.

use std::ops::Range;

/// What a run of text is, for colouring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    Keyword,
    Type,
    Identifier,
    QuotedIdentifier,
    String,
    Number,
    Comment,
    Parameter,
    Operator,
    Punctuation,
    Whitespace,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    /// Byte range in the source.
    pub range: Range<usize>,
}

const KEYWORDS: &[&str] = &[
    "ADD",
    "ALL",
    "ALTER",
    "ANALYZE",
    "AND",
    "ANY",
    "AS",
    "ASC",
    "BEGIN",
    "BETWEEN",
    "BY",
    "CASCADE",
    "CASE",
    "CAST",
    "CHECK",
    "COLLATE",
    "COLUMN",
    "COMMENT",
    "COMMIT",
    "CONFLICT",
    "CONSTRAINT",
    "COPY",
    "CREATE",
    "CROSS",
    "CURRENT_DATE",
    "CURRENT_TIMESTAMP",
    "CURRENT_USER",
    "DATABASE",
    "DEFAULT",
    "DELETE",
    "DESC",
    "DISTINCT",
    "DO",
    "DROP",
    "ELSE",
    "END",
    "EXCEPT",
    "EXISTS",
    "EXPLAIN",
    "EXTENSION",
    "FALSE",
    "FETCH",
    "FILTER",
    "FIRST",
    "FOR",
    "FOREIGN",
    "FROM",
    "FULL",
    "FUNCTION",
    "GRANT",
    "GROUP",
    "HAVING",
    "IF",
    "ILIKE",
    "IN",
    "INDEX",
    "INNER",
    "INSERT",
    "INTERSECT",
    "INTO",
    "IS",
    "JOIN",
    "KEY",
    "LANGUAGE",
    "LAST",
    "LATERAL",
    "LEFT",
    "LIKE",
    "LIMIT",
    "MATERIALIZED",
    "NATURAL",
    "NEXT",
    "NOT",
    "NOTHING",
    "NULL",
    "NULLS",
    "OF",
    "OFFSET",
    "ON",
    "ONLY",
    "OR",
    "ORDER",
    "OUTER",
    "OVER",
    "PARTITION",
    "PRIMARY",
    "PROCEDURE",
    "RECURSIVE",
    "REFERENCES",
    "REFRESH",
    "RENAME",
    "REPLACE",
    "RETURNING",
    "RETURNS",
    "REVOKE",
    "RIGHT",
    "ROLLBACK",
    "ROW",
    "ROWS",
    "SCHEMA",
    "SELECT",
    "SEQUENCE",
    "SET",
    "SHOW",
    "SOME",
    "TABLE",
    "THEN",
    "TO",
    "TRANSACTION",
    "TRIGGER",
    "TRUE",
    "TRUNCATE",
    "TYPE",
    "UNION",
    "UNIQUE",
    "UPDATE",
    "USING",
    "VACUUM",
    "VALUES",
    "VIEW",
    "WHEN",
    "WHERE",
    "WINDOW",
    "WITH",
];

const TYPES: &[&str] = &[
    "BIGINT",
    "BIGSERIAL",
    "BIT",
    "BOOL",
    "BOOLEAN",
    "BYTEA",
    "CHAR",
    "CHARACTER",
    "CIDR",
    "DATE",
    "DECIMAL",
    "DOUBLE",
    "FLOAT",
    "INET",
    "INT",
    "INT2",
    "INT4",
    "INT8",
    "INTEGER",
    "INTERVAL",
    "JSON",
    "JSONB",
    "MONEY",
    "NUMERIC",
    "PRECISION",
    "REAL",
    "SERIAL",
    "SMALLINT",
    "TEXT",
    "TIME",
    "TIMESTAMP",
    "TIMESTAMPTZ",
    "TIMETZ",
    "UUID",
    "VARCHAR",
    "VARYING",
    "XML",
];

/// The longest word in [`KEYWORDS`] and [`TYPES`] fits this buffer.
const LONGEST_WORD: usize = 20;

/// Keyword, type or plain identifier. Upper-cases into a stack buffer, so
/// the lexer allocates nothing per word.
fn word_kind(word: &str) -> TokenKind {
    let mut buffer = [0u8; LONGEST_WORD];
    let Some(upper) = buffer.get_mut(..word.len()) else {
        return TokenKind::Identifier;
    };
    upper.copy_from_slice(word.as_bytes());
    upper.make_ascii_uppercase();
    // The lists are ASCII, so a word that is not cannot be in them.
    let Ok(upper) = std::str::from_utf8(upper) else {
        return TokenKind::Identifier;
    };
    if KEYWORDS.binary_search(&upper).is_ok() {
        TokenKind::Keyword
    } else if TYPES.binary_search(&upper).is_ok() {
        TokenKind::Type
    } else {
        TokenKind::Identifier
    }
}

/// Lazily splits `source` into tokens that cover it end to end, without
/// collecting them.
#[must_use]
pub fn tokens(source: &str) -> Tokens<'_> {
    Tokens { source, at: 0 }
}

pub struct Tokens<'a> {
    source: &'a str,
    at: usize,
}

impl Iterator for Tokens<'_> {
    type Item = Token;

    fn next(&mut self) -> Option<Token> {
        let source = self.source;
        let bytes = source.as_bytes();
        let mut at = self.at;
        if at >= bytes.len() {
            return None;
        }
        let start = at;
        let byte = bytes[at];
        let next = bytes.get(at + 1).copied();
        let kind = match byte {
            b' ' | b'\t' | b'\r' | b'\n' => {
                at = skip_while(bytes, at, |b| b.is_ascii_whitespace());
                TokenKind::Whitespace
            }
            b'-' if next == Some(b'-') => {
                at = find_byte(bytes, at, b'\n');
                TokenKind::Comment
            }
            b'/' if next == Some(b'*') => {
                at = block_comment_end(bytes, at);
                TokenKind::Comment
            }
            b'\'' => {
                at = quoted_end(bytes, at, b'\'');
                TokenKind::String
            }
            b'"' => {
                at = quoted_end(bytes, at, b'"');
                TokenKind::QuotedIdentifier
            }
            b'$' => match dollar_tag(bytes, at) {
                Some(tag_end) => {
                    at = dollar_body_end(bytes, at, tag_end);
                    TokenKind::String
                }
                None => {
                    at = skip_while(bytes, at + 1, |b| b.is_ascii_digit());
                    TokenKind::Parameter
                }
            },
            b'0'..=b'9' => {
                at = number_end(bytes, at);
                TokenKind::Number
            }
            b'.' if next.is_some_and(|b| b.is_ascii_digit()) => {
                at = number_end(bytes, at);
                TokenKind::Number
            }
            b if b.is_ascii_alphabetic() || b == b'_' || b >= 0x80 => {
                at = skip_while(bytes, at, |b| {
                    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
                });
                // `E'...'` and friends are strings with a prefix. Only `E`
                // strings treat a backslash as an escape: `E'\''` is one
                // string, and reading it as `E'\'` then `'` would let a `;`
                // inside the literal split the statement differently from
                // the server.
                if at - start == 1 && bytes.get(at) == Some(&b'\'') {
                    at = if bytes[start].eq_ignore_ascii_case(&b'e') {
                        escaped_quoted_end(bytes, at)
                    } else {
                        quoted_end(bytes, at, b'\'')
                    };
                    TokenKind::String
                } else {
                    word_kind(&source[start..at])
                }
            }
            b'(' | b')' | b',' | b';' | b'.' | b'[' | b']' => {
                at += 1;
                TokenKind::Punctuation
            }
            _ => {
                at += utf8_len(byte);
                TokenKind::Operator
            }
        };
        let end = at.min(bytes.len()).max(start + 1);
        self.at = end;
        Some(Token {
            kind,
            range: start..end,
        })
    }
}

fn utf8_len(first: u8) -> usize {
    match first {
        0xF0..=0xFF => 4,
        0xE0..=0xEF => 3,
        0xC0..=0xDF => 2,
        _ => 1,
    }
}

fn skip_while(bytes: &[u8], mut at: usize, keep: impl Fn(u8) -> bool) -> usize {
    while at < bytes.len() && keep(bytes[at]) {
        at += 1;
    }
    at
}

fn find_byte(bytes: &[u8], at: usize, needle: u8) -> usize {
    bytes[at..]
        .iter()
        .position(|&b| b == needle)
        .map_or(bytes.len(), |offset| at + offset)
}

/// PostgreSQL block comments nest.
fn block_comment_end(bytes: &[u8], mut at: usize) -> usize {
    let mut depth = 0usize;
    while at < bytes.len() {
        if bytes[at] == b'/' && bytes.get(at + 1) == Some(&b'*') {
            depth += 1;
            at += 2;
        } else if bytes[at] == b'*' && bytes.get(at + 1) == Some(&b'/') {
            depth -= 1;
            at += 2;
            if depth == 0 {
                return at;
            }
        } else {
            at += 1;
        }
    }
    bytes.len()
}

/// The end of a quoted run, where a doubled quote is an escaped one.
fn quoted_end(bytes: &[u8], mut at: usize, quote: u8) -> usize {
    at += 1;
    while at < bytes.len() {
        if bytes[at] == quote {
            if bytes.get(at + 1) == Some(&quote) {
                at += 2;
                continue;
            }
            return at + 1;
        }
        at += 1;
    }
    bytes.len()
}

/// The end of an `E'...'` body opening at `at`, where a backslash escapes
/// the next character as well as a doubled quote ending nothing.
fn escaped_quoted_end(bytes: &[u8], mut at: usize) -> usize {
    at += 1;
    while at < bytes.len() {
        match bytes[at] {
            b'\\' => at += 2,
            b'\'' if bytes.get(at + 1) == Some(&b'\'') => at += 2,
            b'\'' => return at + 1,
            _ => at += 1,
        }
    }
    bytes.len()
}

/// The end of `$tag$` when `at` opens a dollar quote.
fn dollar_tag(bytes: &[u8], at: usize) -> Option<usize> {
    let mut end = at + 1;
    while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
        end += 1;
    }
    let tag = &bytes[at + 1..end];
    let valid = tag.first().is_none_or(|b| !b.is_ascii_digit());
    (valid && bytes.get(end) == Some(&b'$')).then_some(end + 1)
}

fn dollar_body_end(bytes: &[u8], start: usize, tag_end: usize) -> usize {
    let tag = &bytes[start..tag_end];
    bytes[tag_end..]
        .windows(tag.len())
        .position(|window| window == tag)
        .map_or(bytes.len(), |offset| tag_end + offset + tag.len())
}

fn number_end(bytes: &[u8], at: usize) -> usize {
    let mut end = skip_while(bytes, at, |b| b.is_ascii_digit() || b == b'.' || b == b'_');
    if matches!(bytes.get(end), Some(b'e' | b'E')) {
        let mut exponent = end + 1;
        if matches!(bytes.get(exponent), Some(b'+' | b'-')) {
            exponent += 1;
        }
        if bytes.get(exponent).is_some_and(u8::is_ascii_digit) {
            end = skip_while(bytes, exponent, |b| b.is_ascii_digit());
        }
    }
    end
}

/// Byte ranges of the statements in `source`, split on top-level `;`, with
/// surrounding whitespace trimmed. Empty statements are left out.
#[must_use]
pub fn statements(source: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut start = 0;
    // A run of only comments is no statement.
    let mut meaningful = false;
    for token in tokens(source) {
        let semicolon = token.kind == TokenKind::Punctuation && &source[token.range.clone()] == ";";
        if semicolon {
            if meaningful {
                push_trimmed(source, start..token.range.end, &mut ranges);
            }
            start = token.range.end;
            meaningful = false;
        } else if !matches!(token.kind, TokenKind::Whitespace | TokenKind::Comment) {
            meaningful = true;
        }
    }
    if meaningful {
        push_trimmed(source, start..source.len(), &mut ranges);
    }
    ranges
}

fn push_trimmed(source: &str, range: Range<usize>, ranges: &mut Vec<Range<usize>>) {
    let text = &source[range.clone()];
    let leading = text.len() - text.trim_start().len();
    let start = range.start + leading;
    ranges.push(start..start + text.trim().len());
}

/// The statement the cursor (a byte offset) is in or just after, as DBeaver
/// picks it for "execute statement".
#[must_use]
pub fn statement_at(source: &str, cursor: usize) -> Option<Range<usize>> {
    let all = statements(source);
    all.iter()
        .find(|range| range.start <= cursor && cursor <= range.end)
        .or_else(|| all.iter().rev().find(|range| range.end <= cursor))
        .or_else(|| all.first())
        .cloned()
}

/// Statements that only read, by their first word.
const READ_STARTS: &[&str] = &["EXPLAIN", "SELECT", "SHOW", "TABLE", "VALUES", "WITH"];

/// Words that make a reading statement write or lock: data-modifying CTEs,
/// `SELECT INTO`, `EXPLAIN ANALYZE` (which runs the statement), and
/// `FOR UPDATE` style locks.
const WRITING_WORDS: &[&str] = &["ANALYZE", "DELETE", "INSERT", "INTO", "MERGE", "UPDATE"];

/// Functions whose result changes without any data changing, or that have
/// side effects. A query calling them is never cached.
const VOLATILE_FUNCTIONS: &[&str] = &[
    "CLOCK_TIMESTAMP",
    "CURRVAL",
    "DBLINK",
    "DBLINK_EXEC",
    "GEN_RANDOM_UUID",
    "LASTVAL",
    "LO_EXPORT",
    "LO_IMPORT",
    "NEXTVAL",
    "NOW",
    "PG_ADVISORY_LOCK",
    "PG_ADVISORY_XACT_LOCK",
    "PG_BACKEND_PID",
    "PG_CANCEL_BACKEND",
    "PG_CURRENT_XACT_ID",
    "PG_NOTIFY",
    "PG_READ_FILE",
    "PG_SLEEP",
    "PG_TERMINATE_BACKEND",
    "PG_TRY_ADVISORY_LOCK",
    "RANDOM",
    "SETSEED",
    "SETVAL",
    "SET_CONFIG",
    "STATEMENT_TIMESTAMP",
    "TIMEOFDAY",
    "TRANSACTION_TIMESTAMP",
    "TXID_CURRENT",
    "UUID_GENERATE_V1",
    "UUID_GENERATE_V4",
];

/// Values and views that move on their own: the clock, and live server
/// state. `pg_stat*` views are matched by prefix.
const VOLATILE_NAMES: &[&str] = &[
    "CURRENT_DATE",
    "CURRENT_TIME",
    "CURRENT_TIMESTAMP",
    "LOCALTIME",
    "LOCALTIMESTAMP",
    "PG_CURSORS",
    "PG_LOCKS",
    "PG_PREPARED_XACTS",
    "PG_REPLICATION_SLOTS",
];

/// A statement's words, upper-cased, each with whether a `(` follows it.
fn words(text: &str) -> Vec<(String, bool)> {
    let mut words: Vec<(String, bool)> = Vec::new();
    let mut after_word = false;
    for token in tokens(text) {
        match token.kind {
            TokenKind::Whitespace | TokenKind::Comment => {}
            TokenKind::Keyword | TokenKind::Type | TokenKind::Identifier => {
                words.push((text[token.range].to_ascii_uppercase(), false));
                after_word = true;
            }
            _ => {
                if after_word
                    && &text[token.range] == "("
                    && let Some(last) = words.last_mut()
                {
                    last.1 = true;
                }
                after_word = false;
            }
        }
    }
    words
}

/// What the app may do with a piece of SQL besides running it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Every statement only reads, and gives the same rows for the same
    /// data: its result may be cached.
    pub cacheable: bool,
    /// A single query that can be read a page at a time: its text without
    /// the trailing `;`.
    pub pageable: Option<String>,
}

/// Decides whether `source` may be cached and paged. It errs on the side of
/// running: anything unclear is neither.
#[must_use]
pub fn plan(source: &str) -> Plan {
    let ranges = statements(source);
    if ranges.is_empty() {
        return Plan::default();
    }
    let mut cacheable = true;
    let mut pageable = ranges.len() == 1;
    for range in &ranges {
        let words = words(&source[range.clone()]);
        let first = words.first().map(|(word, _)| word.as_str());
        let reads = first.is_some_and(|word| READ_STARTS.contains(&word));
        pageable &= first.is_some_and(|word| !matches!(word, "EXPLAIN" | "SHOW"));
        let writes = words.iter().enumerate().any(|(index, (word, _))| {
            WRITING_WORDS.contains(&word.as_str())
                || (word == "FOR"
                    && words.get(index + 1).is_some_and(|(next, _)| {
                        matches!(next.as_str(), "UPDATE" | "SHARE" | "NO" | "KEY")
                    }))
        });
        let volatile = words.iter().any(|(word, called)| {
            word.starts_with("PG_STAT")
                || VOLATILE_NAMES.binary_search(&word.as_str()).is_ok()
                || (*called && VOLATILE_FUNCTIONS.binary_search(&word.as_str()).is_ok())
        });
        cacheable &= reads && !writes && !volatile;
        pageable &= reads && !writes;
    }
    let pageable = pageable.then(|| {
        let text = &source[ranges[0].clone()];
        text.trim_end().trim_end_matches(';').trim_end().to_owned()
    });
    Plan {
        cacheable,
        pageable,
    }
}

/// Whether `filter` is one expression, safe to place in `WHERE (...)`: no
/// statement separator outside strings and comments, and brackets that
/// balance so it cannot close the `WHERE` early.
#[must_use]
pub fn is_single_expression(filter: &str) -> bool {
    let mut depth = 0i32;
    for token in tokens(filter) {
        if token.kind != TokenKind::Punctuation {
            continue;
        }
        match &filter[token.range] {
            ";" => return false,
            "(" => depth += 1,
            ")" => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

/// 1-based line and column of a character offset (as servers report error
/// positions) within `source`.
#[must_use]
pub fn line_column(source: &str, char_offset: usize) -> (usize, usize) {
    let mut line = 1;
    let mut column = 1;
    for character in source.chars().take(char_offset.saturating_sub(1)) {
        if character == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    (line, column)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(source: &str) -> Vec<(TokenKind, &str)> {
        tokens(source)
            .filter(|t| t.kind != TokenKind::Whitespace)
            .map(|t| (t.kind, &source[t.range]))
            .collect()
    }

    #[test]
    fn keyword_lists_are_sorted_for_binary_search() {
        assert!(KEYWORDS.windows(2).all(|w| w[0] < w[1]));
        assert!(TYPES.windows(2).all(|w| w[0] < w[1]));
        assert!(
            KEYWORDS
                .iter()
                .chain(TYPES)
                .all(|w| w.len() <= LONGEST_WORD && w.is_ascii())
        );
        for list in [
            READ_STARTS,
            WRITING_WORDS,
            VOLATILE_FUNCTIONS,
            VOLATILE_NAMES,
        ] {
            assert!(list.windows(2).all(|w| w[0] < w[1]), "{list:?}");
        }
    }

    #[test]
    fn tokens_cover_the_source() {
        let source = "SELECT 'a''b', \"Q\"\"x\", 1.5e3, $1 -- hi\n/* a /* b */ c */ FROM t;é";
        let mut at = 0;
        for token in tokens(source) {
            assert_eq!(token.range.start, at);
            at = token.range.end;
        }
        assert_eq!(at, source.len());
    }

    #[test]
    fn recognises_the_main_kinds() {
        assert_eq!(
            kinds("select id::int FROM \"T\" where x = 'it''s' -- c"),
            vec![
                (TokenKind::Keyword, "select"),
                (TokenKind::Identifier, "id"),
                (TokenKind::Operator, ":"),
                (TokenKind::Operator, ":"),
                (TokenKind::Type, "int"),
                (TokenKind::Keyword, "FROM"),
                (TokenKind::QuotedIdentifier, "\"T\""),
                (TokenKind::Keyword, "where"),
                (TokenKind::Identifier, "x"),
                (TokenKind::Operator, "="),
                (TokenKind::String, "'it''s'"),
                (TokenKind::Comment, "-- c"),
            ]
        );
        // Longer than any keyword, or not ASCII: plain identifiers.
        assert_eq!(
            word_kind("a_very_long_identifier_name"),
            TokenKind::Identifier
        );
        assert_eq!(word_kind("sélect"), TokenKind::Identifier);
        assert_eq!(word_kind("TimestampTZ"), TokenKind::Type);
    }

    #[test]
    fn escape_strings_hide_semicolons() {
        // The server reads one literal; splitting at the `;` inside it would
        // run `DROP TABLE x` on its own from the statement under the cursor.
        let source = "SELECT E'\\'; DROP TABLE x; --'";
        assert_eq!(statements(source), vec![0..source.len()]);
        assert_eq!(
            kinds("e'a\\\\' x E'it''s\\n'"),
            vec![
                (TokenKind::String, "e'a\\\\'"),
                (TokenKind::Identifier, "x"),
                (TokenKind::String, "E'it''s\\n'"),
            ]
        );
        // Only `E` strings escape: in a standard string a backslash is text.
        let plain = "SELECT '\\'; SELECT 2";
        assert_eq!(statements(plain).len(), 2);
        // A trailing backslash cannot read past the end.
        assert_eq!(kinds("E'\\"), vec![(TokenKind::String, "E'\\")]);
    }

    #[test]
    fn dollar_quotes_hide_semicolons() {
        let source = "CREATE FUNCTION f() RETURNS int AS $$ SELECT 1; $$ LANGUAGE sql; SELECT 2";
        let ranges = statements(source);
        assert_eq!(ranges.len(), 2);
        assert_eq!(&source[ranges[1].clone()], "SELECT 2");
        assert_eq!(kinds("$1")[0].0, TokenKind::Parameter);
    }

    #[test]
    fn the_statement_under_the_cursor_is_chosen() {
        let source = "SELECT 1;\n\n  SELECT 2 ;\n-- only a comment\n";
        assert_eq!(
            &source[statement_at(source, 3).expect("first")],
            "SELECT 1;"
        );
        assert_eq!(
            &source[statement_at(source, 14).expect("second")],
            "SELECT 2 ;"
        );
        // After the last statement, it is still the one picked.
        assert_eq!(
            &source[statement_at(source, source.len()).expect("last")],
            "SELECT 2 ;"
        );
        assert_eq!(statements(source).len(), 2);
    }

    #[test]
    fn only_plain_reads_are_cached_and_paged() {
        let read = plan("select * from users where id > 1;  -- done");
        assert!(read.cacheable);
        assert_eq!(
            read.pageable.as_deref(),
            Some("select * from users where id > 1")
        );

        // Several reads: cached, not paged.
        let script = plan("SELECT 1; SELECT 2;");
        assert!(script.cacheable && script.pageable.is_none());

        for writes in [
            "UPDATE t SET a = 1",
            "WITH x AS (DELETE FROM t RETURNING *) SELECT * FROM x",
            "SELECT * INTO copy FROM t",
            "SELECT * FROM t FOR UPDATE",
            "EXPLAIN ANALYZE SELECT 1",
            "SELECT 1; DROP TABLE t",
            "SET search_path TO x",
        ] {
            let plan = plan(writes);
            assert!(!plan.cacheable, "{writes}");
            assert!(plan.pageable.is_none(), "{writes}");
        }

        // Same data, different answer: never cached, still paged.
        for volatile in [
            "SELECT now()",
            "SELECT random()",
            "SELECT nextval('s')",
            "SELECT current_timestamp",
            "SELECT * FROM pg_stat_activity",
            "SELECT * FROM pg_locks",
        ] {
            assert!(!plan(volatile).cacheable, "{volatile}");
        }
        // A column merely named like a function is fine.
        assert!(plan("SELECT now_playing, random_id FROM t").cacheable);

        // Read-only but not a row source to wrap.
        let explain = plan("EXPLAIN SELECT 1");
        assert!(explain.cacheable && explain.pageable.is_none());
        assert_eq!(plan("-- nothing"), Plan::default());
        // Semicolons inside strings do not split.
        assert!(plan("SELECT ';' AS semi").pageable.is_some());
    }

    #[test]
    fn filters_must_be_one_expression() {
        assert!(is_single_expression("id > 1 AND (name ILIKE 'a;b%')"));
        assert!(is_single_expression("note = ')' -- )"));
        assert!(!is_single_expression("1 = 1; DROP TABLE t"));
        assert!(!is_single_expression("1 = 1) OR (1 = 1"));
        assert!(!is_single_expression("(id = 1"));
    }

    #[test]
    fn error_positions_become_lines_and_columns() {
        assert_eq!(line_column("SELECT\n  oops", 10), (2, 3));
        assert_eq!(line_column("x", 1), (1, 1));
    }
}
