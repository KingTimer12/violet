//! Things in a query that tend to be slow, found from its text and what
//! the catalog says of its tables: filters without an index, `SELECT *` and
//! a missing `LIMIT` on big tables, functions on indexed columns. The
//! editor marks them as it marks errors.

use std::ops::Range;

use violet_core::ResultSet;

use crate::completion::{TableUse, tables_used};
use crate::sql::{self, Token, TokenKind};

/// A table with this many rows or more is big for the hints.
pub const LARGE_ROWS: i64 = 100_000;

/// Below this many rows an index hardly matters.
pub const SMALL_ROWS: i64 = 1_000;

/// An index: its name and the columns it orders by, first column first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Index {
    pub name: String,
    pub columns: Vec<String>,
}

/// Reads the rows of [`crate::dialect::Family::index_query`]: index name,
/// then its columns separated by commas.
pub fn parse_indexes(set: &ResultSet) -> Vec<Index> {
    set.rows
        .iter()
        .filter_map(|row| {
            let name = row.get(0)??.to_owned();
            let columns = row
                .get(1)??
                .split(',')
                .map(|column| column.trim().trim_matches('"').to_owned())
                .collect();
            Some(Index { name, columns })
        })
        .collect()
}

/// What kind of trouble a hint points at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A filter or join column no index starts with.
    Unindexed,
    /// A function or arithmetic around an indexed column, so the index
    /// cannot be used.
    BlocksIndex,
    /// `SELECT *` on a big table.
    SelectStar,
    /// No `LIMIT` on a big table.
    NoLimit,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Unindexed => "No index",
            Self::BlocksIndex => "Index not used",
            Self::SelectStar => "SELECT *",
            Self::NoLimit => "No LIMIT",
        }
    }
}

/// A hint on a part of the text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hint {
    pub kind: Kind,
    /// Byte range in the text analyzed.
    pub range: Range<usize>,
    pub message: String,
}

/// What the catalog says of one table.
#[derive(Clone, Debug, Default)]
pub struct TableFacts {
    pub rows: Option<i64>,
    /// `None` while the columns are not loaded.
    pub columns: Option<Vec<String>>,
    /// The first column of each index, the primary key's included; `None`
    /// while the indexes are not loaded.
    pub leading: Option<Vec<String>>,
}

impl TableFacts {
    fn big(&self) -> bool {
        self.rows.is_some_and(|rows| rows >= LARGE_ROWS)
    }

    fn worth_an_index(&self) -> bool {
        self.rows.is_none_or(|rows| rows >= SMALL_ROWS)
    }
}

/// Where facts about tables come from.
pub trait Facts {
    fn table(&self, table: &TableUse) -> Option<TableFacts>;
}

/// What analyzing found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Analysis {
    pub hints: Vec<Hint>,
    /// Tables whose indexes would tell more, not loaded yet.
    pub missing: Vec<TableUse>,
}

/// One significant token with its text.
struct Word<'a> {
    token: Token,
    text: &'a str,
}

impl Word<'_> {
    fn upper(&self) -> String {
        self.text.to_ascii_uppercase()
    }

    fn is(&self, word: &str) -> bool {
        self.text.eq_ignore_ascii_case(word)
    }

    fn punct(&self, symbol: &str) -> bool {
        matches!(
            self.token.kind,
            TokenKind::Punctuation | TokenKind::Operator
        ) && self.text == symbol
    }

    fn name(&self) -> Option<String> {
        match self.token.kind {
            TokenKind::Identifier | TokenKind::Type => Some(self.text.to_owned()),
            TokenKind::QuotedIdentifier => {
                let inner = self.text.get(1..self.text.len().checked_sub(1)?)?;
                Some(inner.to_owned())
            }
            _ => None,
        }
    }

    fn comparison(&self) -> bool {
        match self.token.kind {
            TokenKind::Operator => {
                !self.text.is_empty()
                    && self
                        .text
                        .chars()
                        .all(|c| matches!(c, '=' | '<' | '>' | '!'))
            }
            TokenKind::Keyword => matches!(
                self.upper().as_str(),
                "LIKE" | "ILIKE" | "IN" | "BETWEEN" | "IS" | "NOT"
            ),
            _ => false,
        }
    }

    fn arithmetic(&self) -> bool {
        self.token.kind == TokenKind::Operator
            && matches!(self.text, "+" | "-" | "*" | "/" | "%" | "||")
    }
}

/// Words that end a `WHERE` or `ON` condition.
const ENDS: &[&str] = &[
    "GROUP",
    "ORDER",
    "HAVING",
    "LIMIT",
    "UNION",
    "EXCEPT",
    "INTERSECT",
    "WINDOW",
    "RETURNING",
    "JOIN",
    "INNER",
    "LEFT",
    "RIGHT",
    "FULL",
    "CROSS",
    "NATURAL",
    "WHERE",
    "SELECT",
    "OFFSET",
    "FETCH",
    "FOR",
];

/// Functions that fold rows into one: a query using them returns little.
const AGGREGATES: &[&str] = &["COUNT", "SUM", "AVG", "MIN", "MAX", "BOOL_AND", "BOOL_OR"];

/// Hints for every statement of `text`.
pub fn analyze(text: &str, facts: &dyn Facts) -> Analysis {
    let mut found = Analysis::default();
    for range in sql::statements(text) {
        statement(text, range, facts, &mut found);
    }
    found.hints.sort_by_key(|hint| hint.range.start);
    found.hints.dedup();
    found.missing.dedup();
    found
}

fn statement(text: &str, range: Range<usize>, facts: &dyn Facts, found: &mut Analysis) {
    let body = &text[range.clone()];
    let words: Vec<Word<'_>> = sql::tokens(body)
        .filter(|token| !matches!(token.kind, TokenKind::Whitespace | TokenKind::Comment))
        .map(|token| Word {
            text: &body[token.range.clone()],
            token,
        })
        .collect();
    let Some(first) = words.first().map(Word::upper) else {
        return;
    };
    if !matches!(first.as_str(), "SELECT" | "WITH" | "UPDATE" | "DELETE") {
        return;
    }
    let all: Vec<Token> = words.iter().map(|word| word.token.clone()).collect();
    let tables = tables_used(body, &all);
    let known: Vec<(TableUse, TableFacts)> = tables
        .iter()
        .filter_map(|table| Some((table.clone(), facts.table(table)?)))
        .collect();
    let at =
        |word: &Word<'_>| range.start + word.token.range.start..range.start + word.token.range.end;

    // Big tables: `SELECT *`, and no `LIMIT`.
    if let Some((table, big)) = known.iter().find(|(_, facts)| facts.big()) {
        let rows = big.rows.unwrap_or(0);
        for (index, word) in words.iter().enumerate() {
            let after = index.checked_sub(1).and_then(|before| words.get(before));
            let star = word.punct("*")
                && after.is_some_and(|before| {
                    before.is("SELECT") || before.punct(",") || before.punct(".")
                })
                && words
                    .get(index + 1)
                    .is_some_and(|next| next.punct(",") || next.is("FROM"));
            if star {
                found.hints.push(Hint {
                    kind: Kind::SelectStar,
                    range: at(word),
                    message: format!(
                        "SELECT * reads every column of {} (about {} rows). List the columns you need.",
                        table.name,
                        thousands(rows)
                    ),
                });
            }
        }
        let limited = words
            .iter()
            .any(|word| matches!(word.upper().as_str(), "LIMIT" | "FETCH" | "TOP"));
        let folds = words.iter().enumerate().any(|(index, word)| {
            (word.is("GROUP") && words.get(index + 1).is_some_and(|next| next.is("BY")))
                || (AGGREGATES.contains(&word.upper().as_str())
                    && words.get(index + 1).is_some_and(|next| next.punct("(")))
        });
        if first == "SELECT" && !limited && !folds {
            found.hints.push(Hint {
                kind: Kind::NoLimit,
                range: at(&words[0]),
                message: format!(
                    "No LIMIT: this can return all of {} (about {} rows).",
                    table.name,
                    thousands(rows)
                ),
            });
        }
    }

    // Filters and joins.
    let mut clause: Option<usize> = None;
    let mut depth = 0usize;
    let mut index = 0;
    while index < words.len() {
        let word = &words[index];
        if word.punct("(") {
            depth += 1;
        } else if word.punct(")") {
            depth = depth.saturating_sub(1);
            if clause.is_some_and(|start| depth < start) {
                clause = None;
            }
        } else if word.is("WHERE") || word.is("ON") {
            clause = Some(depth);
            index += 1;
            predicate(&words, index, &known, &tables, range.start, found);
            continue;
        } else if clause == Some(depth)
            && (ENDS.contains(&word.upper().as_str()) || word.punct(";"))
        {
            clause = None;
        } else if clause == Some(depth) && (word.is("AND") || word.is("OR")) {
            predicate(&words, index + 1, &known, &tables, range.start, found);
        }
        index += 1;
    }
}

/// One condition starting at `words[start]`.
fn predicate(
    words: &[Word<'_>],
    mut start: usize,
    known: &[(TableUse, TableFacts)],
    tables: &[TableUse],
    base: usize,
    found: &mut Analysis,
) {
    while words
        .get(start)
        .is_some_and(|word| word.is("NOT") || word.punct("("))
    {
        start += 1;
    }
    let Some(head) = words.get(start) else {
        return;
    };
    let span =
        |from: &Word<'_>, to: &Word<'_>| base + from.token.range.start..base + to.token.range.end;
    // A function around the column: `lower(email) = ...`.
    if head.name().is_some()
        && words.get(start + 1).is_some_and(|next| next.punct("("))
        && !matches!(
            head.upper().as_str(),
            "EXISTS" | "ANY" | "ALL" | "SOME" | "IN"
        )
    {
        if let Some((column, end)) = column_ref(words, start + 2)
            && let Some(close) = closing(words, start + 1)
            && words.get(close + 1).is_some_and(Word::comparison)
        {
            blocks(&column, span(head, &words[end]), known, tables, found);
        }
        return;
    }
    let Some((column, end)) = column_ref(words, start) else {
        return;
    };
    let Some(next) = words.get(end + 1) else {
        return;
    };
    if next.arithmetic() {
        blocks(&column, span(head, &words[end]), known, tables, found);
        return;
    }
    if !next.comparison() {
        return;
    }
    check(&column, span(head, &words[end]), known, tables, found);
    // A join compares two columns: the other side is checked too.
    if let Some(op) = words[end + 1..]
        .iter()
        .position(|word| word.token.kind == TokenKind::Operator && word.comparison())
        && let Some((other, last)) = column_ref(words, end + 2 + op)
        && other.qualifier.is_some()
    {
        check(
            &other,
            span(&words[end + 2 + op], &words[last]),
            known,
            tables,
            found,
        );
    }
}

/// The index of the `)` closing the `(` at `open`.
fn closing(words: &[Word<'_>], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (at, word) in words.iter().enumerate().skip(open) {
        if word.punct("(") {
            depth += 1;
        } else if word.punct(")") {
            depth -= 1;
            if depth == 0 {
                return Some(at);
            }
        }
    }
    None
}

/// A column as the statement writes it.
struct ColumnRef {
    qualifier: Option<String>,
    name: String,
}

/// `name` or `qualifier.name` at `words[start]`, and the index of its last
/// token.
fn column_ref(words: &[Word<'_>], start: usize) -> Option<(ColumnRef, usize)> {
    let first = words.get(start)?.name()?;
    if words.get(start + 1).is_some_and(|dot| dot.punct("."))
        && let Some(name) = words.get(start + 2).and_then(Word::name)
    {
        return Some((
            ColumnRef {
                qualifier: Some(first),
                name,
            },
            start + 2,
        ));
    }
    // A name followed by `(` is a function, not a column.
    (!words.get(start + 1).is_some_and(|next| next.punct("("))).then_some((
        ColumnRef {
            qualifier: None,
            name: first,
        },
        start,
    ))
}

/// The table `column` belongs to, with its facts.
fn owner<'a>(
    column: &ColumnRef,
    known: &'a [(TableUse, TableFacts)],
    tables: &[TableUse],
) -> Option<&'a (TableUse, TableFacts)> {
    if let Some(qualifier) = &column.qualifier {
        return known
            .iter()
            .find(|(table, _)| table.handle().eq_ignore_ascii_case(qualifier));
    }
    let mut candidates = known.iter().filter(|(_, facts)| {
        facts.columns.as_ref().is_some_and(|columns| {
            columns
                .iter()
                .any(|name| name.eq_ignore_ascii_case(&column.name))
        })
    });
    let first = candidates.next();
    if candidates.next().is_some() {
        return None;
    }
    // Columns not loaded: a statement on one table has no other owner.
    first.or_else(|| (tables.len() == 1).then(|| known.first()).flatten())
}

fn indexed(facts: &TableFacts, column: &str) -> Option<bool> {
    let leading = facts.leading.as_ref()?;
    Some(leading.iter().any(|name| name.eq_ignore_ascii_case(column)))
}

fn check(
    column: &ColumnRef,
    range: Range<usize>,
    known: &[(TableUse, TableFacts)],
    tables: &[TableUse],
    found: &mut Analysis,
) {
    let Some((table, facts)) = owner(column, known, tables) else {
        return;
    };
    match indexed(facts, &column.name) {
        None => found.missing.push(table.clone()),
        Some(false) if facts.worth_an_index() => found.hints.push(Hint {
            kind: Kind::Unindexed,
            range,
            message: format!(
                "No index on {}({}) starts with this column: the filter reads the whole table.",
                table.name, column.name
            ),
        }),
        Some(_) => {}
    }
}

fn blocks(
    column: &ColumnRef,
    range: Range<usize>,
    known: &[(TableUse, TableFacts)],
    tables: &[TableUse],
    found: &mut Analysis,
) {
    let Some((table, facts)) = owner(column, known, tables) else {
        return;
    };
    match indexed(facts, &column.name) {
        None => found.missing.push(table.clone()),
        Some(true) if facts.worth_an_index() => found.hints.push(Hint {
            kind: Kind::BlocksIndex,
            range,
            message: format!(
                "{} has an index, but a function or arithmetic on it cannot use it. Compare the bare column, or index the expression.",
                column.name
            ),
        }),
        Some(_) => {}
    }
}

/// `12345` as `12,345`.
fn thousands(n: i64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (at, ch) in digits.chars().enumerate() {
        if at > 0 && (digits.len() - at).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Stub(Vec<(&'static str, TableFacts)>);

    impl Facts for Stub {
        fn table(&self, table: &TableUse) -> Option<TableFacts> {
            self.0
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(&table.name))
                .map(|(_, facts)| facts.clone())
        }
    }

    fn orders(indexes: Option<&[&str]>) -> Stub {
        Stub(vec![
            (
                "orders",
                TableFacts {
                    rows: Some(2_000_000),
                    columns: Some(vec![
                        "id".into(),
                        "email".into(),
                        "status".into(),
                        "customer_id".into(),
                    ]),
                    leading: indexes.map(|names| names.iter().map(ToString::to_string).collect()),
                },
            ),
            (
                "customers",
                TableFacts {
                    rows: Some(500),
                    columns: Some(vec!["id".into(), "name".into()]),
                    leading: Some(vec!["id".into()]),
                },
            ),
        ])
    }

    fn kinds(text: &str, facts: &Stub) -> Vec<(Kind, String)> {
        analyze(text, facts)
            .hints
            .into_iter()
            .map(|hint| (hint.kind, text[hint.range].to_owned()))
            .collect()
    }

    #[test]
    fn flags_star_and_a_missing_limit_on_big_tables() {
        let found = kinds("SELECT * FROM orders", &orders(Some(&["id"])));
        assert_eq!(
            found,
            [
                (Kind::NoLimit, "SELECT".into()),
                (Kind::SelectStar, "*".into())
            ]
        );
        assert!(kinds("SELECT id FROM orders LIMIT 10", &orders(Some(&["id"]))).is_empty());
        assert!(kinds("SELECT count(*) FROM orders", &orders(Some(&["id"]))).is_empty());
        // A small table needs neither.
        assert!(kinds("SELECT * FROM customers", &orders(Some(&["id"]))).is_empty());
    }

    #[test]
    fn flags_filters_no_index_starts_with() {
        let facts = orders(Some(&["id"]));
        let found = kinds(
            "SELECT id FROM orders WHERE status = 'paid' LIMIT 5",
            &facts,
        );
        assert_eq!(found, [(Kind::Unindexed, "status".into())]);
        // An index on the column quiets it, as does a qualifier and alias.
        assert!(kinds("SELECT id FROM orders WHERE id = 1 LIMIT 5", &facts).is_empty());
        let found = kinds(
            "SELECT o.id FROM orders o WHERE o.email LIKE 'a%' AND o.id = 2 LIMIT 5",
            &facts,
        );
        assert_eq!(found, [(Kind::Unindexed, "o.email".into())]);
    }

    #[test]
    fn flags_join_columns() {
        let facts = orders(Some(&["id"]));
        let found = kinds(
            "SELECT o.id FROM orders o JOIN customers c ON o.customer_id = c.id LIMIT 5",
            &facts,
        );
        assert_eq!(found, [(Kind::Unindexed, "o.customer_id".into())]);
    }

    #[test]
    fn flags_functions_on_indexed_columns() {
        let facts = orders(Some(&["id", "email"]));
        let found = kinds(
            "SELECT id FROM orders WHERE lower(email) = 'a' LIMIT 5",
            &facts,
        );
        assert_eq!(found, [(Kind::BlocksIndex, "lower(email".into())]);
        let found = kinds("SELECT id FROM orders WHERE id + 1 = 5 LIMIT 5", &facts);
        assert_eq!(found, [(Kind::BlocksIndex, "id".into())]);
        // No index on it: nothing to block.
        let found = kinds(
            "SELECT id FROM orders WHERE lower(status) = 'a' LIMIT 5",
            &facts,
        );
        assert!(found.is_empty());
    }

    #[test]
    fn asks_for_indexes_not_loaded_yet() {
        let analysis = analyze(
            "SELECT id FROM orders WHERE status = 'x' LIMIT 5",
            &orders(None),
        );
        assert!(analysis.hints.is_empty());
        assert_eq!(analysis.missing.len(), 1);
        assert_eq!(analysis.missing[0].name, "orders");
    }

    #[test]
    fn leaves_other_statements_and_subqueries_be() {
        let facts = orders(Some(&["id"]));
        assert!(kinds("INSERT INTO orders VALUES (1)", &facts).is_empty());
        assert!(kinds("SELECT 1", &facts).is_empty());
        // The second statement is judged on its own.
        let found = kinds(
            "SELECT 1; SELECT id FROM orders WHERE status = 'x' LIMIT 1",
            &facts,
        );
        assert_eq!(found, [(Kind::Unindexed, "status".into())]);
    }

    #[test]
    fn reads_index_rows() {
        let mut rows = violet_core::Rows::new(2);
        rows.push([Some("orders_pkey"), Some("id")]);
        rows.push([Some("by_customer"), Some("customer_id, created_at")]);
        let set = ResultSet {
            columns: Vec::new(),
            rows,
            ..ResultSet::default()
        };
        let indexes = parse_indexes(&set);
        assert_eq!(indexes.len(), 2);
        assert_eq!(indexes[1].columns, ["customer_id", "created_at"]);
    }
}
