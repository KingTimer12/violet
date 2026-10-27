//! Formatting SQL: clauses on lines of their own, select lists and
//! conditions one item per line, subqueries indented, reserved words in
//! upper case.
//!
//! Only whitespace changes, and the case of reserved words: strings,
//! quoted names and comments are kept as they are. The result is lexed
//! again and compared with the source, token by token; any other
//! difference refuses the formatting instead of changing the script.

use crate::dialect::Family;
use crate::sql::{self, TokenKind};

const INDENT: &str = "    ";

/// Words written in upper case. Reserved in MySQL too, so none of them can
/// name a table unquoted there, where table names are case-sensitive.
const UPPER: &[&str] = &[
    "ALL",
    "ALTER",
    "AND",
    "AS",
    "ASC",
    "BETWEEN",
    "BY",
    "CASE",
    "CHECK",
    "CONSTRAINT",
    "CREATE",
    "CROSS",
    "DEFAULT",
    "DELETE",
    "DESC",
    "DISTINCT",
    "DROP",
    "ELSE",
    "EXCEPT",
    "EXISTS",
    "FALSE",
    "FOREIGN",
    "FROM",
    "GROUP",
    "HAVING",
    "IN",
    "INDEX",
    "INNER",
    "INSERT",
    "INTERSECT",
    "INTO",
    "IS",
    "JOIN",
    "KEY",
    "LEFT",
    "LIKE",
    "LIMIT",
    "NATURAL",
    "NOT",
    "NULL",
    "ON",
    "OR",
    "ORDER",
    "OUTER",
    "OVER",
    "PARTITION",
    "PRIMARY",
    "RECURSIVE",
    "REFERENCES",
    "RIGHT",
    "SELECT",
    "SET",
    "TABLE",
    "THEN",
    "TRUE",
    "UNION",
    "UNIQUE",
    "UPDATE",
    "USING",
    "VALUES",
    "WHEN",
    "WHERE",
    "WINDOW",
    "WITH",
];

/// Why a script was not formatted.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// A MySQL string, single or double quoted, holds a backslash, which
    /// the lexer does not follow: what lies inside the string is not
    /// known.
    Backslash,
    /// Formatting would change more than whitespace.
    Changed,
}

/// `source` formatted as `family` reads it: MySQL has backticks and `#`
/// comments, SQLite backticks and `[names]`, and each adds reserved words.
pub fn format(source: &str, family: Family) -> Result<String, Refused> {
    let tokens = significant(source, family);
    // MySQL reads `"…"` as a string too, by default.
    if family.escapes_with_backslash()
        && tokens.iter().any(|token| {
            (token.kind == TokenKind::String || token.text.starts_with('"'))
                && token.text.contains('\\')
        })
    {
        return Err(Refused::Backslash);
    }
    let mut writer = Writer {
        family,
        ..Writer::default()
    };
    writer.write(&tokens);
    let mut out = writer.out.trim_end().to_owned();
    if source.ends_with('\n') {
        out.push('\n');
    }
    if same_tokens(source, &out, family) {
        Ok(out)
    } else {
        Err(Refused::Changed)
    }
}

/// A token that is not whitespace, and what stood before it.
#[derive(Clone, Copy)]
struct Token<'a> {
    kind: TokenKind,
    text: &'a str,
    /// Nothing stood between it and the token before.
    glued: bool,
    /// A line break stood before it.
    line: bool,
    /// An empty line stood before it.
    blank: bool,
}

impl Token<'_> {
    fn is(&self, word: &str) -> bool {
        matches!(self.kind, TokenKind::Keyword | TokenKind::Identifier)
            && self.text.eq_ignore_ascii_case(word)
    }

    fn is_line_comment(&self) -> bool {
        self.kind == TokenKind::Comment && !self.text.starts_with("/*")
    }
}

/// The tokens of `source` but whitespace. Where `family` has them, a
/// backticked or bracketed name is one quoted token and `#` starts a
/// comment.
fn significant(source: &str, family: Family) -> Vec<Token<'_>> {
    let mut out = Vec::new();
    let mut space = "";
    let mut skip_to = 0;
    for token in sql::tokens(source) {
        if token.range.start < skip_to {
            continue;
        }
        let text = &source[token.range.clone()];
        let (kind, end) = match (token.kind, text) {
            (TokenKind::Whitespace, _) => {
                space = text;
                continue;
            }
            (TokenKind::Operator, "`") if family.backtick_quotes() => (
                TokenKind::QuotedIdentifier,
                backticked_end(source, token.range.end),
            ),
            (TokenKind::Punctuation, "[") if family.bracket_quotes() => (
                TokenKind::QuotedIdentifier,
                bracketed_end(source, token.range.end),
            ),
            (TokenKind::Operator, "#") if family.hash_comments() => {
                let end = source[token.range.start..]
                    .find('\n')
                    .map_or(source.len(), |at| token.range.start + at);
                (TokenKind::Comment, end)
            }
            (kind, _) => (kind, token.range.end),
        };
        skip_to = end;
        out.push(Token {
            kind,
            text: &source[token.range.start..end],
            glued: space.is_empty() && !out.is_empty(),
            line: space.contains('\n'),
            blank: space.matches('\n').count() > 1,
        });
        space = "";
    }
    out
}

/// Where a backticked name that opens before `from` ends: past its
/// closing backtick, a doubled one standing for a backtick inside.
fn backticked_end(source: &str, mut from: usize) -> usize {
    while let Some(at) = source[from..].find('`') {
        let end = from + at + 1;
        if !source[end..].starts_with('`') {
            return end;
        }
        from = end + 1;
    }
    source.len()
}

/// Where a bracketed name that opens before `from` ends: past its closing
/// bracket, a doubled one standing for a bracket inside.
fn bracketed_end(source: &str, mut from: usize) -> usize {
    while let Some(at) = source[from..].find(']') {
        let end = from + at + 1;
        if !source[end..].starts_with(']') {
            return end;
        }
        from = end + 1;
    }
    source.len()
}

/// Whether `formatted` reads as `source`: the same tokens, reserved words
/// in any case. Quoted tokens next to each other keep whether they touch:
/// `'a''b'` is one string, `'a' 'b'` two.
fn same_tokens(source: &str, formatted: &str, family: Family) -> bool {
    let a = significant(source, family);
    let b = significant(formatted, family);
    let quoted =
        |token: &Token<'_>| matches!(token.kind, TokenKind::String | TokenKind::QuotedIdentifier);
    a.len() == b.len()
        && a.iter().zip(&b).enumerate().all(|(index, (x, y))| {
            let touching = index > 0 && quoted(x) && quoted(&a[index - 1]);
            x.kind == y.kind
                && (!touching || x.glued == y.glued)
                && match x.kind {
                    TokenKind::Keyword | TokenKind::Identifier => {
                        x.text.eq_ignore_ascii_case(y.text)
                    }
                    // A line comment loses the spaces that ended its line.
                    TokenKind::Comment => x.text.trim_end() == y.text.trim_end(),
                    _ => x.text == y.text,
                }
        })
}

/// The clause a statement is in, which decides where items break.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Clause {
    #[default]
    None,
    /// A select list or an `UPDATE`'s assignments: an item per line.
    List,
    /// `WHERE`, `HAVING`, `ON`: a condition per line.
    Conditions,
    Other,
}

/// An open parenthesis.
struct Paren {
    /// It holds a query, indented under it.
    query: bool,
    /// What was in force outside it.
    indent: usize,
    clause: Clause,
}

#[derive(Default)]
struct Writer {
    out: String,
    /// Whose reserved words are written in upper case.
    family: Family,
    /// The indentation of the statement's clauses.
    indent: usize,
    clause: Clause,
    parens: Vec<Paren>,
    /// Nothing written on the current line yet.
    fresh: bool,
    /// An `AND` that belongs to a `BETWEEN`.
    between: bool,
    /// A line comment ended the line: the next token starts a new one.
    broken: bool,
    /// The last token was a sign, glued to what follows.
    sign: bool,
}

/// Operators written with spaces around them; the others keep the
/// spacing they had.
fn spaced(operator: &str) -> bool {
    operator.len() == 1 && "=<>!+-*/%|&^~".contains(operator)
}

impl Writer {
    /// Starts a new line at `level`.
    fn newline(&mut self, level: usize) {
        let trimmed = self.out.trim_end_matches(' ').len();
        self.out.truncate(trimmed);
        if !self.out.is_empty() && !self.out.ends_with('\n') {
            self.out.push('\n');
        }
        if !self.out.is_empty() {
            self.out.push_str(&INDENT.repeat(level));
        }
        self.fresh = true;
    }

    /// An empty line, between statements.
    fn blank_line(&mut self) {
        let trimmed = self.out.trim_end().len();
        self.out.truncate(trimmed);
        if !self.out.is_empty() {
            self.out.push_str("\n\n");
        }
        self.fresh = true;
    }

    /// Whether clause words act here: outside any parenthesis but a query.
    fn at_clause_level(&self) -> bool {
        self.parens.last().is_none_or(|paren| paren.query)
    }

    fn write(&mut self, tokens: &[Token<'_>]) {
        self.fresh = true;
        for (index, token) in tokens.iter().enumerate() {
            let prev = index.checked_sub(1).map(|i| tokens[i]);
            let next = tokens.get(index + 1).copied();
            self.token(*token, prev, next);
        }
    }

    fn token(&mut self, token: Token<'_>, prev: Option<Token<'_>>, next: Option<Token<'_>>) {
        let top = self.parens.is_empty();
        let starts_statement = [
            "SELECT", "INSERT", "UPDATE", "DELETE", "WITH", "CREATE", "ALTER", "DROP",
        ]
        .iter()
        .any(|w| token.is(w));
        // Paragraphs between statements stay apart.
        if token.blank && top && (self.clause_at_rest(prev) || starts_statement) {
            self.blank_line();
            self.indent = 0;
            self.clause = Clause::None;
            self.between = false;
        } else if self.broken || (token.kind == TokenKind::Comment && token.line) {
            self.newline(self.indent + usize::from(self.clause != Clause::None));
        }
        self.broken = false;

        if token.kind == TokenKind::Comment {
            self.space(token, prev, next);
            if token.is_line_comment() {
                self.out.push_str(token.text.trim_end());
                self.broken = true;
            } else {
                self.out.push_str(token.text);
            }
            self.fresh = false;
            return;
        }

        let clause_level = self.at_clause_level();
        let word = |w: &str| token.is(w);
        let prev_is = |w: &str| prev.is_some_and(|p| p.is(w));

        // Clauses start lines of their own.
        if clause_level {
            if word("SELECT") {
                self.newline(self.indent);
                self.emit(token, prev, next);
                self.clause = Clause::List;
                self.newline(self.indent + 1);
                return;
            }
            let starts = [
                "FROM",
                "WHERE",
                "HAVING",
                "LIMIT",
                "VALUES",
                "RETURNING",
                "UNION",
                "INTERSECT",
                "EXCEPT",
                "WINDOW",
            ];
            let joins = ["LEFT", "RIGHT", "INNER", "FULL", "CROSS", "NATURAL"];
            let starts_clause = (starts.iter().any(|w| word(w))
                && !(word("FROM") && prev_is("DELETE")))
                || ((word("GROUP") || word("ORDER")) && next.is_some_and(|n| n.is("BY")))
                || (word("SET") && !prev.is_some_and(|p| p.text == ";") && prev.is_some())
                || joins.iter().any(|w| word(w))
                || (word("JOIN") && !joins.iter().any(|w| prev_is(w)) && !prev_is("OUTER"));
            if starts_clause {
                self.newline(self.indent);
                self.emit(token, prev, next);
                self.clause = if word("WHERE") || word("HAVING") {
                    Clause::Conditions
                } else if word("SET") {
                    Clause::List
                } else {
                    Clause::Other
                };
                return;
            }
            if word("ON") && self.clause == Clause::Other {
                self.emit(token, prev, next);
                self.clause = Clause::Conditions;
                return;
            }
            if word("BETWEEN") {
                self.between = true;
            } else if (word("AND") || word("OR")) && self.clause == Clause::Conditions {
                if word("AND") && self.between {
                    self.between = false;
                } else {
                    self.newline(self.indent + 1);
                    self.emit(token, prev, next);
                    return;
                }
            }
        }

        match token.text {
            "," if clause_level && self.clause == Clause::List => {
                self.out.push(',');
                self.newline(self.indent + 1);
            }
            ";" => {
                self.out.push(';');
                self.fresh = false;
                self.indent = 0;
                self.clause = Clause::None;
                self.parens.clear();
                self.between = false;
                if next.is_some() {
                    self.blank_line();
                }
            }
            "(" if token.kind == TokenKind::Punctuation => {
                self.emit(token, prev, next);
                let query = next.is_some_and(|n| n.is("SELECT") || n.is("WITH"));
                self.parens.push(Paren {
                    query,
                    indent: self.indent,
                    clause: self.clause,
                });
                if query {
                    self.indent += 1;
                    self.clause = Clause::None;
                }
            }
            ")" if token.kind == TokenKind::Punctuation => {
                if let Some(paren) = self.parens.pop() {
                    if paren.query {
                        self.newline(paren.indent);
                    }
                    self.indent = paren.indent;
                    self.clause = paren.clause;
                }
                self.emit(token, prev, next);
            }
            _ => self.emit(token, prev, next),
        }
    }

    /// Whether a statement has just ended, or not begun: an empty line
    /// before a token there is kept.
    fn clause_at_rest(&self, prev: Option<Token<'_>>) -> bool {
        prev.is_none_or(|p| p.text == ";" || p.kind == TokenKind::Comment)
            || self.clause == Clause::None
    }

    /// Writes `token` after the space it takes.
    fn emit(&mut self, token: Token<'_>, prev: Option<Token<'_>>, next: Option<Token<'_>>) {
        self.space(token, prev, next);
        let word = token.text.to_ascii_uppercase();
        let upper = matches!(token.kind, TokenKind::Keyword)
            && (UPPER.binary_search(&word.as_str()).is_ok()
                || self.family.reserved().contains(&word.as_str()));
        if upper {
            self.out.push_str(&word);
        } else {
            self.out.push_str(token.text);
        }
        // A sign after an operator, a parenthesis, a comma or a word is
        // glued to its number.
        self.sign = (token.text == "-" || token.text == "+")
            && prev.is_none_or(|p| {
                matches!(p.kind, TokenKind::Operator | TokenKind::Keyword)
                    || matches!(p.text, "(" | "," | "[")
            });
        self.fresh = false;
    }

    /// The space before `token`, if any.
    fn space(&mut self, token: Token<'_>, prev: Option<Token<'_>>, next: Option<Token<'_>>) {
        if self.fresh {
            return;
        }
        let Some(prev) = prev else {
            return;
        };
        let operator = |t: Token<'_>| t.kind == TokenKind::Operator;
        // `=-1`: a sign after a comparison, not one operator with it.
        let sign = matches!(token.text, "-" | "+")
            && matches!(prev.text, "=" | "<" | ">")
            && next.is_some_and(|n| n.kind == TokenKind::Number && n.glued);
        let none = matches!(token.text, "," | ";" | ")" | "]" | ".")
            && token.kind == TokenKind::Punctuation
            || matches!(prev.text, "(" | "[" | ".") && prev.kind == TokenKind::Punctuation
            // Calls and casts, `count(*)`, `varchar(20)`: a parenthesis
            // glued to a word stays so.
            || token.text == "(" && token.kind == TokenKind::Punctuation && token.glued
            // `tags[1]`: a subscript stays glued where brackets are not names.
            || token.text == "[" && token.kind == TokenKind::Punctuation && token.glued
            || self.sign
            // `::`, `>=`, `<>`: operators written together stay together.
            || operator(prev) && operator(token) && token.glued && !sign
            // `:name`, `@var`, `?`: others keep the spacing they had.
            || (operator(token) && !spaced(token.text) || operator(prev) && !spaced(prev.text))
                && token.glued;
        if !none {
            self.out.push(' ');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn formatted(source: &str) -> String {
        format(source, Family::Other).expect("formats")
    }

    #[test]
    fn upper_words_are_sorted() {
        assert!(UPPER.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn clauses_and_lists_break() {
        assert_eq!(
            formatted(
                "select id, nome from usuarios u join pedidos p on p.usuario_id = u.id where u.ativo = 1 and p.total between 1 and 10 order by nome"
            ),
            "SELECT\n    id,\n    nome\nFROM usuarios u\nJOIN pedidos p ON p.usuario_id = u.id\nWHERE u.ativo = 1\n    AND p.total BETWEEN 1 AND 10\nORDER BY nome"
        );
    }

    #[test]
    fn calls_casts_and_lists_stay_inline() {
        assert_eq!(
            formatted("SELECT count(*),x::int,coalesce(a,b) FROM t WHERE a IN (1,2) AND b>=-1"),
            "SELECT\n    count(*),\n    x::int,\n    coalesce(a, b)\nFROM t\nWHERE a IN (1, 2)\n    AND b >= -1"
        );
    }

    #[test]
    fn subqueries_indent() {
        assert_eq!(
            formatted("select * from (select id from t) s where exists (select 1 from u)"),
            "SELECT\n    *\nFROM (\n    SELECT\n        id\n    FROM t\n) s\nWHERE EXISTS (\n    SELECT\n        1\n    FROM u\n)"
        );
    }

    #[test]
    fn statements_and_comments_keep_their_lines() {
        assert_eq!(
            formatted("-- first\nselect 1; update t set a = 1, b = 'x  y' where id = 2;\n"),
            "-- first\nSELECT\n    1;\n\nUPDATE t\nSET a = 1,\n    b = 'x  y'\nWHERE id = 2;\n"
        );
    }

    #[test]
    fn mysql_quotes_and_comments_are_kept() {
        assert_eq!(
            format(
                "select `order`, @x from `my table` # note\nwhere a=1",
                Family::MySql
            ),
            Ok("SELECT\n    `order`,\n    @x\nFROM `my table` # note\nWHERE a = 1".to_owned())
        );
        assert_eq!(
            format("select 'it\\'s'", Family::MySql),
            Err(Refused::Backslash)
        );
        assert_eq!(
            format("select \"a\\\" , b\"", Family::MySql),
            Err(Refused::Backslash)
        );
        // A doubled backtick is one inside the name.
        assert_eq!(
            format("select `a``b` from t", Family::MySql),
            Ok("SELECT\n    `a``b`\nFROM t".to_owned())
        );
    }

    #[test]
    fn each_family_reads_its_own_quotes_and_comments() {
        // `#` is an operator in PostgreSQL: `#>` digs into JSON.
        assert_eq!(
            format("select a #> '{b}' from t", Family::Postgres),
            Ok("SELECT\n    a #> '{b}'\nFROM t".to_owned())
        );
        // Backslashes are text outside MySQL.
        assert_eq!(
            format("select 'a\\' from t", Family::Postgres),
            Ok("SELECT\n    'a\\'\nFROM t".to_owned())
        );
        assert_eq!(
            format("select [my col], `x` from [t t]", Family::Sqlite),
            Ok("SELECT\n    [my col],\n    `x`\nFROM [t t]".to_owned())
        );
        assert_eq!(
            format("select [a]]b] from t", Family::Sqlite),
            Ok("SELECT\n    [a]]b]\nFROM t".to_owned())
        );
        // Array subscripts are not names where brackets do not quote.
        assert_eq!(
            format("select a[1] from t", Family::Postgres),
            Ok("SELECT\n    a[1]\nFROM t".to_owned())
        );
    }

    #[test]
    fn reserved_words_follow_the_family() {
        let source = "select * from t where a ilike 'x' offset 5";
        assert_eq!(
            format(source, Family::Postgres),
            Ok("SELECT\n    *\nFROM t\nWHERE a ILIKE 'x' OFFSET 5".to_owned())
        );
        // In MySQL `ilike` and `offset` may name columns.
        assert_eq!(
            format(source, Family::MySql),
            Ok("SELECT\n    *\nFROM t\nWHERE a ilike 'x' offset 5".to_owned())
        );
    }

    #[test]
    fn statements_without_semicolons_stay_apart() {
        assert_eq!(
            formatted("select 1\n\nselect 2  -- two  \n"),
            "SELECT\n    1\n\nSELECT\n    2 -- two\n"
        );
        assert_eq!(
            formatted("insert into t (a, b) values (1, 2)"),
            "INSERT INTO t (a, b)\nVALUES (1, 2)"
        );
    }

    #[test]
    fn only_reserved_words_change_case() {
        // `type` and `date` name columns as often as anything.
        assert_eq!(
            formatted("select type, date from t"),
            "SELECT\n    type,\n    date\nFROM t"
        );
    }
}
