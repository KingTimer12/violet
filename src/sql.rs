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

fn is_word(word: &str, list: &[&str]) -> bool {
    word.len() <= 20 && {
        let upper = word.to_ascii_uppercase();
        list.binary_search(&upper.as_str()).is_ok()
    }
}

/// Splits `source` into tokens that cover it end to end.
#[must_use]
pub fn tokenize(source: &str) -> Vec<Token> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
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
                // `E'...'` and friends are strings with a prefix.
                if at - start == 1 && bytes.get(at) == Some(&b'\'') {
                    at = quoted_end(bytes, at, b'\'');
                    TokenKind::String
                } else {
                    let word = &source[start..at];
                    if is_word(word, KEYWORDS) {
                        TokenKind::Keyword
                    } else if is_word(word, TYPES) {
                        TokenKind::Type
                    } else {
                        TokenKind::Identifier
                    }
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
        at = end;
        tokens.push(Token {
            kind,
            range: start..end,
        });
    }
    tokens
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
    for token in tokenize(source) {
        if token.kind == TokenKind::Punctuation && &source[token.range.clone()] == ";" {
            push_trimmed(source, start..token.range.end, &mut ranges);
            start = token.range.end;
        }
    }
    push_trimmed(source, start..source.len(), &mut ranges);
    ranges
}

fn push_trimmed(source: &str, range: Range<usize>, ranges: &mut Vec<Range<usize>>) {
    let text = &source[range.clone()];
    let leading = text.len() - text.trim_start().len();
    let trimmed = text.trim();
    // A run of only comments is no statement.
    let meaningful = tokenize(trimmed).iter().any(|t| {
        !matches!(t.kind, TokenKind::Whitespace | TokenKind::Comment)
            && &trimmed[t.range.clone()] != ";"
    });
    if meaningful {
        let start = range.start + leading;
        ranges.push(start..start + trimmed.len());
    }
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
        tokenize(source)
            .into_iter()
            .filter(|t| t.kind != TokenKind::Whitespace)
            .map(|t| (t.kind, &source[t.range]))
            .collect()
    }

    #[test]
    fn keyword_lists_are_sorted_for_binary_search() {
        assert!(KEYWORDS.windows(2).all(|w| w[0] < w[1]));
        assert!(TYPES.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn tokens_cover_the_source() {
        let source = "SELECT 'a''b', \"Q\"\"x\", 1.5e3, $1 -- hi\n/* a /* b */ c */ FROM t;é";
        let tokens = tokenize(source);
        let mut at = 0;
        for token in &tokens {
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
    fn error_positions_become_lines_and_columns() {
        assert_eq!(line_column("SELECT\n  oops", 10), (2, 3));
        assert_eq!(line_column("x", 1), (1, 1));
    }
}
