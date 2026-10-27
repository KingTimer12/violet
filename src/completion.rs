//! Context-sensitive SQL completion: what fits where the cursor is, drawn
//! from the catalog, from the statement around the cursor (its tables,
//! aliases, subqueries and CTEs), from the foreign keys between tables, and
//! from tables and views the script itself creates.
//!
//! Like the lexer it reads tokens, not a grammar, so it never fails: text it
//! cannot place gets columns and keywords.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use violet_core::{ColumnInfo, DbObject, ForeignKey, ObjectKind};

use crate::dialect::Family;
use crate::settings::KeywordCase;
use crate::sql::{self, Token, TokenKind};

/// What completion reads from the database. `None` means not loaded yet;
/// the completion then lists what to load in [`Completion::needs`].
pub trait Catalog {
    fn schemas(&self) -> &[String];
    fn objects(&self, schema: &str) -> Option<&[DbObject]>;
    fn columns(&self, schema: &str, relation: &str) -> Option<&[ColumnInfo]>;
    fn foreign_keys(&self, schema: &str) -> Option<&[ForeignKey]>;
}

/// Catalog data a completion would use but is not loaded.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Need {
    Objects(String),
    Columns(String, String),
    ForeignKeys(String),
    /// A key-value database's keys.
    Keys(String),
}

/// What a suggestion is, in the order they are listed when equally good.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    /// A join along a foreign key.
    Join,
    Column,
    /// A table alias, a CTE or a subquery.
    Alias,
    Table,
    View,
    Schema,
    Function,
    Keyword,
    Type,
    /// A key of a key-value store.
    Key,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub label: String,
    pub insert: String,
    /// Where the cursor goes, as a byte offset into `insert`.
    pub caret: usize,
    pub kind: Kind,
    /// Shown after the label: a type, a table, a schema.
    pub detail: String,
    /// The part of the label the typed prefix is matched against.
    key: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Completion {
    /// The bytes an accepted item replaces: the word at the cursor.
    pub range: Range<usize>,
    /// What was typed of the word, up to the cursor.
    pub prefix: String,
    pub items: Vec<Item>,
    pub needs: Vec<Need>,
}

pub struct Options<'a> {
    /// Schemas whose objects need no qualifier, first match wins.
    pub search: &'a [String],
    /// Quotes an identifier in the connection's dialect.
    pub quote: &'a dyn Fn(&str) -> String,
    /// Which statements, keywords, functions and types to offer.
    pub family: Family,
    /// The case keywords are offered in.
    pub case: KeywordCase,
    /// What was accepted before on this connection.
    pub history: &'a History,
}

const MAX_ITEMS: usize = 200;
/// How many accepted items [`History`] keeps.
const HISTORY_SIZE: usize = 200;

/// The items accepted lately, by kind and name: the latest come first among
/// equally good matches.
#[derive(Clone, Debug, Default)]
pub struct History {
    /// Counts acceptances; the higher, the more recent.
    clock: u64,
    used: HashMap<(Kind, String), u64>,
}

impl History {
    /// Remembers `item` as just accepted.
    pub fn accepted(&mut self, item: &Item) {
        self.clock += 1;
        self.used
            .insert((item.kind, item.key.to_lowercase()), self.clock);
        if self.used.len() > HISTORY_SIZE
            && let Some(oldest) = self
                .used
                .iter()
                .min_by_key(|(_, at)| **at)
                .map(|(key, _)| key.clone())
        {
            self.used.remove(&oldest);
        }
    }

    /// When `item` was last accepted; 0 when never.
    fn recency(&self, item: &Item) -> u64 {
        self.used
            .get(&(item.kind, item.key.to_lowercase()))
            .copied()
            .unwrap_or(0)
    }
}

/// Completes a Redis command line: command names (with their syntax) in
/// the first word, a container's subcommands in the second, and the keys
/// listed in the tree for `schema` where the command takes keys. `keys` is
/// `None` while they have not been listed.
pub fn complete_redis(
    text: &str,
    cursor: usize,
    schema: Option<&str>,
    keys: Option<&[DbObject]>,
) -> Option<Completion> {
    use violet_lang::redis::{COMMANDS, is_container};
    if cursor > text.len() || !text.is_char_boundary(cursor) {
        return None;
    }
    let line_start = text[..cursor].rfind('\n').map_or(0, |at| at + 1);
    let before = &text[line_start..cursor];
    if before.trim_start().starts_with('#') || before.trim_start().starts_with("//") {
        return None;
    }
    // Inside quotes nothing is offered.
    if before.chars().filter(|c| *c == '"').count() % 2 == 1
        || before.chars().filter(|c| *c == '\'').count() % 2 == 1
    {
        return None;
    }
    let start = line_start
        + before
            .rfind(|c: char| c.is_ascii_whitespace())
            .map_or(0, |at| at + 1);
    let end = cursor
        + text[cursor..]
            .find(|c: char| c.is_ascii_whitespace())
            .unwrap_or(text.len() - cursor);
    let prefix = text[start..cursor].to_owned();
    let words: Vec<&str> = text[line_start..start].split_ascii_whitespace().collect();
    let upper = prefix.to_ascii_uppercase();
    let mut items = Vec::new();
    let mut needs = Vec::new();
    let command = |name: &str, syntax: &str, insert: String| Item {
        label: name.to_owned(),
        caret: insert.len(),
        insert,
        kind: Kind::Keyword,
        detail: syntax.to_owned(),
        key: name.to_owned(),
    };
    match words.as_slice() {
        [] => {
            let mut containers = Vec::new();
            for c in COMMANDS {
                let first = c.name.split(' ').next().unwrap_or(c.name);
                if !first.starts_with(&upper) {
                    continue;
                }
                if c.name.contains(' ') {
                    // `CONFIG GET` and its siblings, under their container.
                    if !containers.contains(&first) {
                        containers.push(first);
                        items.push(command(first, "subcommand ...", format!("{first} ")));
                    }
                } else {
                    items.push(command(c.name, c.syntax, format!("{} ", c.name)));
                }
            }
        }
        [first] if is_container(&first.to_ascii_uppercase()) => {
            let head = format!("{} ", first.to_ascii_uppercase());
            for c in COMMANDS {
                if let Some(sub) = c.name.strip_prefix(&head)
                    && sub.starts_with(&upper)
                {
                    items.push(command(sub, c.syntax, format!("{sub} ")));
                }
            }
        }
        [first, ..] => {
            let name = violet_lang::redis::command(&words);
            let takes_keys = name.is_some_and(|c| {
                c.syntax.contains("key")
                    || c.syntax.contains("source")
                    || c.syntax.contains("destination")
            }) || name.is_none() && !is_container(&first.to_ascii_uppercase());
            if takes_keys {
                match keys {
                    Some(keys) => {
                        for key in keys.iter().filter(|k| k.name.starts_with(&prefix)) {
                            let insert = violet_lang::redis::quote(key.name.as_bytes());
                            items.push(Item {
                                label: key.name.clone(),
                                caret: insert.len(),
                                insert,
                                kind: Kind::Key,
                                detail: key.subtype.clone().unwrap_or_default(),
                                key: key.name.clone(),
                            });
                            if items.len() >= MAX_ITEMS {
                                break;
                            }
                        }
                    }
                    None => {
                        if let Some(schema) = schema {
                            needs.push(Need::Keys(schema.to_owned()));
                        }
                    }
                }
            }
        }
    }
    items.truncate(MAX_ITEMS);
    Some(Completion {
        range: start..end,
        prefix,
        items,
        needs,
    })
}

/// What the shell offers on `db.`.
const MONGO_DB: &[&str] = &[
    "aggregate",
    "createCollection",
    "dropDatabase",
    "getCollection",
    "getCollectionNames",
    "runCommand",
    "stats",
];

/// What the shell offers on `db.collection.`.
const MONGO_COLLECTION: &[&str] = &[
    "aggregate",
    "bulkWrite",
    "countDocuments",
    "createIndex",
    "deleteMany",
    "deleteOne",
    "distinct",
    "drop",
    "dropIndex",
    "estimatedDocumentCount",
    "find",
    "findOne",
    "findOneAndDelete",
    "findOneAndReplace",
    "findOneAndUpdate",
    "getIndexes",
    "insertMany",
    "insertOne",
    "replaceOne",
    "updateMany",
    "updateOne",
];

/// What a cursor, such as `find(...)`'s, offers after `).`.
const MONGO_CURSOR: &[&str] = &[
    "batchSize",
    "collation",
    "count",
    "explain",
    "forEach",
    "hint",
    "limit",
    "map",
    "pretty",
    "project",
    "skip",
    "sort",
    "toArray",
];

/// Query, update and aggregation operators.
const MONGO_OPERATORS: &[&str] = &[
    "$addFields",
    "$addToSet",
    "$all",
    "$and",
    "$avg",
    "$count",
    "$elemMatch",
    "$eq",
    "$exists",
    "$expr",
    "$facet",
    "$group",
    "$gt",
    "$gte",
    "$in",
    "$inc",
    "$limit",
    "$lookup",
    "$lt",
    "$lte",
    "$match",
    "$max",
    "$min",
    "$ne",
    "$nin",
    "$nor",
    "$not",
    "$or",
    "$project",
    "$pull",
    "$push",
    "$regex",
    "$set",
    "$size",
    "$skip",
    "$sort",
    "$sum",
    "$type",
    "$unset",
    "$unwind",
];

/// Completes a MongoDB shell statement: `db`, `show` and `use` to start
/// one, the collections listed in the tree for `schema` and the database's
/// methods after `db.`, a collection's methods after `db.name.`, a
/// cursor's after `).`, and operators on `$`. `collections` is `None`
/// while they have not been listed.
pub fn complete_mongo(
    text: &str,
    cursor: usize,
    schema: Option<&str>,
    collections: Option<&[DbObject]>,
) -> Option<Completion> {
    if cursor > text.len() || !text.is_char_boundary(cursor) {
        return None;
    }
    let line_start = text[..cursor].rfind('\n').map_or(0, |at| at + 1);
    let line = &text[line_start..cursor];
    if line.trim_start().starts_with("//")
        || line.chars().filter(|c| *c == '"').count() % 2 == 1
        || line.chars().filter(|c| *c == '\'').count() % 2 == 1
    {
        return None;
    }
    let start = word_start(text, cursor);
    let end = word_end(text, cursor);
    let prefix = text[start..cursor].to_owned();
    let mut items = Vec::new();
    let mut needs = Vec::new();
    let mut offer = |kind: Kind, name: &str, detail: &str, insert: String| {
        items.push(Item {
            label: name.to_owned(),
            caret: if insert.ends_with("()") {
                insert.len() - 1
            } else {
                insert.len()
            },
            insert,
            kind,
            detail: detail.to_owned(),
            key: name.to_owned(),
        });
    };
    let head = text[..start].trim_end_matches(|c: char| c == ' ' || c == '\t');
    if prefix.starts_with('$') {
        for operator in MONGO_OPERATORS {
            offer(Kind::Keyword, operator, "", (*operator).to_owned());
        }
    } else if let Some(owner) = head.strip_suffix('.') {
        let methods = |list: &[&str], offer: &mut dyn FnMut(Kind, &str, &str, String)| {
            for method in list {
                offer(Kind::Function, method, "", format!("{method}()"));
            }
        };
        let owner_start = word_start(owner, owner.len());
        let name = &owner[owner_start..];
        let parent = owner[..owner_start].strip_suffix('.');
        if owner.ends_with(')') {
            methods(MONGO_CURSOR, &mut offer);
        } else if name == "db" && parent.is_none() {
            methods(MONGO_DB, &mut offer);
            match collections {
                Some(collections) => {
                    for collection in collections {
                        let plain = collection
                            .name
                            .chars()
                            .all(|c| c.is_alphanumeric() || c == '_')
                            && !collection.name.starts_with(|c: char| c.is_ascii_digit());
                        let insert = if plain {
                            collection.name.clone()
                        } else {
                            format!("getCollection({:?})", collection.name)
                        };
                        offer(Kind::Table, &collection.name, "collection", insert);
                    }
                }
                None => {
                    if let Some(schema) = schema {
                        needs.push(Need::Objects(schema.to_owned()));
                    }
                }
            }
        } else if parent.is_some_and(|parent| parent.ends_with("db")) {
            methods(MONGO_COLLECTION, &mut offer);
        }
    } else if head.is_empty() || head.ends_with(';') || head.ends_with('\n') {
        offer(Kind::Keyword, "db", "", "db".to_owned());
        offer(Kind::Keyword, "show dbs", "", "show dbs".to_owned());
        offer(Kind::Keyword, "show collections", "", "show collections".to_owned());
        offer(Kind::Keyword, "use", "database", "use ".to_owned());
    }
    let mut items = rank(items, &prefix, &History::default(), false);
    items.truncate(MAX_ITEMS);
    Some(Completion {
        range: start..end,
        prefix,
        items,
        needs,
    })
}

/// Phrases offered with the keywords in expressions.
const PHRASES: &[&str] = &[
    "GROUP BY",
    "ORDER BY",
    "IS NULL",
    "IS NOT NULL",
    "NOT NULL",
    "LEFT JOIN",
    "INNER JOIN",
    "PARTITION BY",
    "UNION ALL",
];

/// Keywords that can never name a table here: they start or end a clause.
const RESERVED: &[&str] = &[
    "AND",
    "AS",
    "BY",
    "CROSS",
    "EXCEPT",
    "FROM",
    "FULL",
    "GROUP",
    "HAVING",
    "INNER",
    "INTERSECT",
    "JOIN",
    "LATERAL",
    "LEFT",
    "LIMIT",
    "NATURAL",
    "NOT",
    "OFFSET",
    "ON",
    "ONLY",
    "OR",
    "ORDER",
    "OUTER",
    "RETURNING",
    "RIGHT",
    "SELECT",
    "SET",
    "UNION",
    "USING",
    "VALUES",
    "WHERE",
    "WINDOW",
    "WITH",
];

/// Clause keywords, for finding which clause the cursor is in.
const CLAUSES: &[&str] = &[
    "BY",
    "FROM",
    "HAVING",
    "INTO",
    "JOIN",
    "LIMIT",
    "OFFSET",
    "ON",
    "RETURNING",
    "SELECT",
    "SET",
    "TABLE",
    "UPDATE",
    "USING",
    "VALUES",
    "WHERE",
    "WITH",
];

/// Suggestions for the cursor at byte `cursor` of `text`, or `None` inside a
/// string, a comment, a quoted name or a number.
#[must_use]
#[hotpath::measure]
pub fn complete(
    text: &str,
    cursor: usize,
    catalog: &dyn Catalog,
    options: &Options<'_>,
) -> Option<Completion> {
    if cursor > text.len() || !text.is_char_boundary(cursor) {
        return None;
    }
    let all: Vec<Token> = sql::tokens(text).collect();
    if inside_literal(text, &all, cursor) {
        return None;
    }
    let start = word_start(text, cursor);
    if text[start..].starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let end = word_end(text, cursor);
    let (qualifiers, qualified_start, quoted) = qualifiers(text, start);

    let significant: Vec<Token> = all
        .into_iter()
        .filter(|token| !matches!(token.kind, TokenKind::Whitespace | TokenKind::Comment))
        .collect();
    let semicolon = |token: &Token| is(text, token, ";");
    let statement_start = significant
        .iter()
        .rposition(|token| semicolon(token) && token.range.end <= qualified_start)
        .map_or(0, |index| index + 1);
    let statement_end = significant[statement_start..]
        .iter()
        .position(|token| semicolon(token) && token.range.start >= cursor)
        .map_or(significant.len(), |index| statement_start + index);
    let statement = &significant[statement_start..statement_end];
    let before: Vec<&Token> = statement
        .iter()
        .filter(|token| token.range.end <= qualified_start)
        .collect();

    let script = Script::read(text, &significant);
    let mut scope = Scope::read(text, statement);
    // The word being typed is no table yet.
    scope.relations.retain(|r| r.at != qualified_start);
    // Where the whole name goes: `FROM schema.ta|` is still a table slot.
    let slot = context(text, &before, &scope);
    let alias_tables = matches!(slot, Context::Relation { alias: true, .. });
    let context = if qualifiers.is_empty() {
        slot
    } else {
        Context::Qualified(qualifiers, quoted)
    };

    let mut builder = Builder {
        catalog,
        options,
        script: &script,
        scope: &scope,
        cursor: qualified_start,
        alias_tables,
        items: Vec::new(),
        needs: Vec::new(),
        prefix: &text[start..cursor],
    };
    builder.fill(&context);
    let prefix = text[start..cursor].to_owned();
    let relation = matches!(context, Context::Relation { .. });
    let mut items = rank(builder.items, &prefix, options.history, relation);
    if items.len() == 1 && items[0].insert.eq_ignore_ascii_case(&prefix) {
        items.clear();
    }
    let mut needs = Vec::new();
    for need in builder.needs {
        if !needs.contains(&need) {
            needs.push(need);
        }
    }
    Some(Completion {
        range: start..end,
        prefix,
        items,
        needs,
    })
}

/// Whether the list opens by itself with the cursor at `cursor`, right
/// after typing: after `FROM `, `JOIN `, `ON ` and `INTO `, the `(` of
/// `INSERT INTO table (` and a name's `.`; never in a string or a comment.
pub fn opens_by_itself(text: &str, cursor: usize) -> bool {
    if cursor == 0 || cursor > text.len() || !text.is_char_boundary(cursor) {
        return false;
    }
    let all: Vec<Token> = sql::tokens(text).collect();
    if inside_literal(text, &all, cursor) {
        return false;
    }
    let mut typed = text[..cursor].chars().rev();
    let before: Vec<&Token> = all
        .iter()
        .filter(|token| !matches!(token.kind, TokenKind::Whitespace | TokenKind::Comment))
        .filter(|token| token.range.end <= cursor)
        .collect();
    let Some(last) = before.last() else {
        return false;
    };
    match typed.next() {
        Some('.') => typed
            .next()
            .is_some_and(|c| is_word_char(c) || matches!(c, '"' | '`')),
        Some(' ') => {
            last.range.end + 1 == cursor
                && matches!(
                    word(text, last).as_deref(),
                    Some("FROM" | "JOIN" | "ON" | "INTO")
                )
        }
        Some('(') => matches!(
            context(text, &before, &Scope::default()),
            Context::InsertColumns(_)
        ),
        _ => false,
    }
}

// ---- reading the text -----------------------------------------------------

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

fn word_start(text: &str, cursor: usize) -> usize {
    text[..cursor]
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_word_char(*c))
        .last()
        .map_or(cursor, |(at, _)| at)
}

fn word_end(text: &str, cursor: usize) -> usize {
    text[cursor..]
        .char_indices()
        .find(|(_, c)| !is_word_char(*c))
        .map_or(text.len(), |(at, _)| cursor + at)
}

/// Whether the cursor sits inside a string, a comment or a quoted name.
fn inside_literal(text: &str, tokens: &[Token], cursor: usize) -> bool {
    let Some(token) = tokens
        .iter()
        .find(|token| token.range.start < cursor && cursor <= token.range.end)
    else {
        return false;
    };
    let body = &text[token.range.clone()];
    match token.kind {
        TokenKind::Comment => {
            cursor < token.range.end
                || body.starts_with("--")
                || !(body.len() >= 4 && body.ends_with("*/"))
        }
        TokenKind::String | TokenKind::QuotedIdentifier => {
            let closed =
                body.len() >= 2 && matches!(body.as_bytes()[body.len() - 1], b'\'' | b'"' | b'`' | b'$');
            cursor < token.range.end || !closed
        }
        _ => false,
    }
}

/// The names before the word, as in `schema.table.` + word: at most two,
/// where the first starts, and whether the last is quoted.
fn qualifiers(text: &str, start: usize) -> (Vec<String>, usize, bool) {
    let mut names = Vec::new();
    let mut quoted = false;
    let mut at = start;
    while names.len() < 2 && text[..at].ends_with('.') {
        let dot = at - 1;
        let head = &text[..dot];
        let quote = head.chars().next_back().filter(|c| matches!(c, '"' | '`'));
        let (name, begin) = if let Some(quote) = quote {
            let inner = &head[..head.len() - 1];
            let Some(open) = inner.rfind(quote) else { break };
            let doubled = quote.to_string().repeat(2);
            (inner[open + 1..].replace(&doubled, &quote.to_string()), open)
        } else {
            let begin = word_start(text, dot);
            (text[begin..dot].to_owned(), begin)
        };
        if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
            break;
        }
        if names.is_empty() {
            quoted = quote.is_some();
        }
        names.insert(0, name);
        at = begin;
    }
    let begin = if names.is_empty() { start } else { at };
    (names, begin, quoted)
}

fn is(text: &str, token: &Token, punctuation: &str) -> bool {
    matches!(token.kind, TokenKind::Punctuation | TokenKind::Operator)
        && &text[token.range.clone()] == punctuation
}

/// A keyword, type or plain word, upper-cased.
fn word(text: &str, token: &Token) -> Option<String> {
    matches!(
        token.kind,
        TokenKind::Keyword | TokenKind::Type | TokenKind::Identifier
    )
    .then(|| text[token.range.clone()].to_ascii_uppercase())
}

fn is_word(text: &str, token: Option<&Token>, expected: &str) -> bool {
    token.and_then(|token| word(text, token)).as_deref() == Some(expected)
}

/// The name a token gives, unquoted; keywords that end clauses are no
/// names.
fn name(text: &str, token: &Token) -> Option<String> {
    let body = &text[token.range.clone()];
    match token.kind {
        TokenKind::Identifier | TokenKind::Type => Some(body.to_owned()),
        TokenKind::Keyword => {
            let upper = body.to_ascii_uppercase();
            (!RESERVED.contains(&upper.as_str())).then(|| body.to_owned())
        }
        TokenKind::QuotedIdentifier => {
            let quote = body.chars().next().filter(|c| matches!(c, '"' | '`'))?;
            let inner = body.strip_prefix(quote)?.strip_suffix(quote)?;
            Some(inner.replace(&quote.to_string().repeat(2), &quote.to_string()))
        }
        _ => None,
    }
}

/// A name that can be an alias: not a keyword at all.
fn alias(text: &str, token: Option<&Token>) -> Option<String> {
    let token = token?;
    matches!(
        token.kind,
        TokenKind::Identifier | TokenKind::QuotedIdentifier
    )
    .then(|| name(text, token))
    .flatten()
}

/// `a`, `a.b` or `a.b.c` from `tokens[at]`, and the index after it.
fn dotted(text: &str, tokens: &[Token], at: usize) -> Option<(Vec<String>, usize)> {
    let mut parts = vec![name(text, tokens.get(at)?)?];
    let mut next = at + 1;
    while let (Some(dot), Some(part)) = (tokens.get(next), tokens.get(next + 1)) {
        if !is(text, dot, ".") {
            break;
        }
        let Some(part) = name(text, part) else { break };
        parts.push(part);
        next += 2;
    }
    Some((parts, next))
}

/// The index of the `)` closing the `(` at `open`, or the end.
fn closing(text: &str, tokens: &[Token], open: usize) -> usize {
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate().skip(open) {
        if is(text, token, "(") {
            depth += 1;
        } else if is(text, token, ")") {
            depth -= 1;
            if depth == 0 {
                return index;
            }
        }
    }
    tokens.len()
}

/// Splits `tokens` on commas outside brackets.
fn split_commas<'t>(text: &str, tokens: &'t [Token]) -> Vec<&'t [Token]> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (index, token) in tokens.iter().enumerate() {
        if is(text, token, "(") {
            depth += 1;
        } else if is(text, token, ")") {
            depth -= 1;
        } else if depth == 0 && is(text, token, ",") {
            parts.push(&tokens[start..index]);
            start = index + 1;
        }
    }
    parts.push(&tokens[start..]);
    parts.retain(|part| !part.is_empty());
    parts
}

/// The names of the columns a query returns, as far as they can be told
/// from its select list.
fn output_names(text: &str, tokens: &[Token]) -> Vec<String> {
    let mut depth = 0i32;
    let Some(select) = tokens.iter().position(|token| {
        if is(text, token, "(") {
            depth += 1;
        } else if is(text, token, ")") {
            depth -= 1;
        }
        depth == 0 && is_word(text, Some(token), "SELECT")
    }) else {
        return Vec::new();
    };
    let mut start = select + 1;
    if is_word(text, tokens.get(start), "DISTINCT") || is_word(text, tokens.get(start), "ALL") {
        start += 1;
        if is_word(text, tokens.get(start), "ON") && tokens.get(start + 1).is_some() {
            start = closing(text, tokens, start + 1) + 1;
        }
    }
    let mut depth = 0i32;
    let end = tokens[start.min(tokens.len())..]
        .iter()
        .position(|token| {
            if is(text, token, "(") {
                depth += 1;
            } else if is(text, token, ")") {
                depth -= 1;
            }
            depth < 0
                || (depth == 0
                    && word(text, token).is_some_and(|w| {
                        matches!(
                            w.as_str(),
                            "FROM" | "INTO" | "WHERE" | "GROUP" | "ORDER" | "LIMIT" | "UNION"
                        )
                    }))
        })
        .map_or(tokens.len(), |at| start + at);
    if start >= end {
        return Vec::new();
    }
    split_commas(text, &tokens[start..end])
        .into_iter()
        .filter_map(|item| {
            let last = item.last()?;
            let before = item.len().checked_sub(2).map(|at| &item[at]);
            if item.len() == 1 {
                return name(text, last);
            }
            if before.is_some_and(|b| is_word(text, Some(b), "AS") || is(text, b, ".")) {
                return name(text, last);
            }
            if is(text, last, ")") && is(text, &item[1], "(") {
                // `count(*)` is called `count`.
                return name(text, &item[0]).map(|n| n.to_lowercase());
            }
            alias(text, Some(last))
        })
        .collect()
}

// ---- the statement's scope ------------------------------------------------

/// A table, view, CTE or subquery the statement reads.
#[derive(Clone, Debug, Default)]
struct Relation {
    schema: Option<String>,
    name: String,
    alias: Option<String>,
    /// Known without the catalog: a subquery's or CTE's select list.
    columns: Option<Vec<String>>,
    /// Where it is named, as a byte offset.
    at: usize,
    /// Named in quotes: matched case-sensitively.
    quoted: bool,
}

impl Relation {
    /// What the statement calls it.
    fn handle(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.name)
    }
}

#[derive(Debug, Default)]
struct Scope {
    relations: Vec<Relation>,
    ctes: Vec<Relation>,
}

impl Scope {
    fn read(text: &str, tokens: &[Token]) -> Self {
        Self {
            relations: relations(text, tokens),
            ctes: ctes(text, tokens),
        }
    }

    fn by_handle(&self, handle: &str) -> Option<&Relation> {
        self.relations
            .iter()
            .find(|r| {
                r.alias
                    .as_deref()
                    .is_some_and(|a| a.eq_ignore_ascii_case(handle))
            })
            .or_else(|| {
                self.relations
                    .iter()
                    .find(|r| r.alias.is_none() && r.name.eq_ignore_ascii_case(handle))
            })
            .or_else(|| {
                self.ctes
                    .iter()
                    .find(|r| r.name.eq_ignore_ascii_case(handle))
            })
    }

    fn cte(&self, name: &str) -> Option<&Relation> {
        self.ctes.iter().find(|r| r.name.eq_ignore_ascii_case(name))
    }
}

/// The tables after `FROM`, `JOIN`, `UPDATE`, `INTO` and `TABLE`, with their
/// aliases, and subqueries with theirs.
fn relations(text: &str, tokens: &[Token]) -> Vec<Relation> {
    let mut found = Vec::new();
    let mut expect = false;
    let mut depth = 0usize;
    // The bracket depth of the `FROM` whose list a comma continues.
    let mut from: Option<usize> = None;
    let mut index = 0;
    while index < tokens.len() {
        let token = &tokens[index];
        if is(text, token, "(") {
            if expect {
                let close = closing(text, tokens, index);
                let columns = output_names(text, &tokens[index + 1..close.min(tokens.len())]);
                let mut next = close + 1;
                if is_word(text, tokens.get(next), "AS") {
                    next += 1;
                }
                if let Some(alias) = alias(text, tokens.get(next)) {
                    found.push(Relation {
                        name: alias.clone(),
                        alias: Some(alias),
                        columns: Some(columns),
                        at: token.range.start,
                        ..Relation::default()
                    });
                    next += 1;
                }
                expect = false;
                index = next;
                continue;
            }
            depth += 1;
        } else if is(text, token, ")") {
            depth = depth.saturating_sub(1);
            if from.is_some_and(|level| depth < level) {
                from = None;
            }
        } else if is(text, token, ",") {
            expect = from == Some(depth);
        }
        match word(text, token).as_deref() {
            Some("FROM") => {
                expect = true;
                from = Some(depth);
            }
            Some("JOIN" | "UPDATE" | "INTO" | "TABLE" | "TRUNCATE") => {
                expect = true;
                from = None;
            }
            Some("ONLY" | "LATERAL") => {}
            Some(
                "WHERE" | "GROUP" | "ORDER" | "HAVING" | "LIMIT" | "SET" | "VALUES" | "ON"
                | "USING" | "SELECT" | "UNION" | "RETURNING" | "WINDOW",
            ) => {
                expect = false;
                if from == Some(depth) {
                    from = None;
                }
            }
            _ if expect => {
                if let Some((mut parts, mut next)) = dotted(text, tokens, index) {
                    let quoted = tokens[next - 1].kind == TokenKind::QuotedIdentifier;
                    let table = parts.pop().unwrap_or_default();
                    let schema = parts.pop();
                    if is_word(text, tokens.get(next), "AS") {
                        next += 1;
                    }
                    let alias = alias(text, tokens.get(next));
                    if alias.is_some() {
                        next += 1;
                    }
                    found.push(Relation {
                        schema,
                        name: table,
                        alias,
                        columns: None,
                        at: token.range.start,
                        quoted,
                    });
                    expect = false;
                    index = next;
                    continue;
                }
                expect = false;
            }
            _ => {}
        }
        index += 1;
    }
    found
}

/// `WITH name [(columns)] AS (query), ...`, anywhere in the statement.
fn ctes(text: &str, tokens: &[Token]) -> Vec<Relation> {
    let mut found = Vec::new();
    for (with, _) in tokens
        .iter()
        .enumerate()
        .filter(|(_, token)| is_word(text, Some(token), "WITH"))
    {
        let mut at = with + 1;
        if is_word(text, tokens.get(at), "RECURSIVE") {
            at += 1;
        }
        while let Some(cte) = tokens.get(at).and_then(|token| name(text, token)) {
            let position = tokens[at].range.start;
            at += 1;
            let mut columns = None;
            if tokens.get(at).is_some_and(|token| is(text, token, "(")) {
                let close = closing(text, tokens, at);
                columns = Some(
                    split_commas(text, &tokens[at + 1..close.min(tokens.len())])
                        .iter()
                        .filter_map(|part| name(text, &part[0]))
                        .collect(),
                );
                at = close + 1;
            }
            if !is_word(text, tokens.get(at), "AS") {
                break;
            }
            at += 1;
            while is_word(text, tokens.get(at), "NOT")
                || is_word(text, tokens.get(at), "MATERIALIZED")
            {
                at += 1;
            }
            if !tokens.get(at).is_some_and(|token| is(text, token, "(")) {
                break;
            }
            let close = closing(text, tokens, at);
            let columns = columns
                .unwrap_or_else(|| output_names(text, &tokens[at + 1..close.min(tokens.len())]));
            found.push(Relation {
                name: cte,
                columns: Some(columns),
                at: position,
                ..Relation::default()
            });
            at = close + 1;
            if !tokens.get(at).is_some_and(|token| is(text, token, ",")) {
                break;
            }
            at += 1;
        }
    }
    found
}

// ---- what the script creates ----------------------------------------------

/// A table or view a `CREATE` in the editor makes.
#[derive(Clone, Debug)]
struct Created {
    schema: Option<String>,
    name: String,
    view: bool,
    columns: Vec<String>,
    primary: Vec<String>,
}

#[derive(Debug, Default)]
struct Script {
    created: Vec<Created>,
    /// Foreign keys the script declares; no referenced columns means the
    /// referenced table's primary key.
    keys: Vec<ForeignKey>,
}

impl Script {
    fn read(text: &str, tokens: &[Token]) -> Self {
        let mut script = Self::default();
        for (create, _) in tokens
            .iter()
            .enumerate()
            .filter(|(_, token)| is_word(text, Some(token), "CREATE"))
        {
            script.read_create(text, tokens, create + 1);
        }
        script
    }

    fn read_create(&mut self, text: &str, tokens: &[Token], mut at: usize) {
        while tokens.get(at).and_then(|t| word(text, t)).is_some_and(|w| {
            matches!(
                w.as_str(),
                "OR" | "REPLACE"
                    | "GLOBAL"
                    | "LOCAL"
                    | "TEMP"
                    | "TEMPORARY"
                    | "UNLOGGED"
                    | "MATERIALIZED"
                    | "RECURSIVE"
            )
        }) {
            at += 1;
        }
        let view = match tokens.get(at).and_then(|t| word(text, t)).as_deref() {
            Some("TABLE") => false,
            Some("VIEW") => true,
            _ => return,
        };
        at += 1;
        if is_word(text, tokens.get(at), "IF") {
            at += 3;
        }
        let Some((mut parts, mut next)) = dotted(text, tokens, at) else {
            return;
        };
        let name = parts.pop().unwrap_or_default();
        let schema = parts.pop();
        let end = tokens[next.min(tokens.len())..]
            .iter()
            .position(|token| is(text, token, ";"))
            .map_or(tokens.len(), |at| next + at);
        let mut created = Created {
            schema: schema.clone(),
            name: name.clone(),
            view,
            columns: Vec::new(),
            primary: Vec::new(),
        };
        if tokens.get(next).is_some_and(|token| is(text, token, "(")) {
            let close = closing(text, tokens, next).min(end);
            for part in split_commas(text, &tokens[next + 1..close]) {
                if view {
                    created.columns.extend(name_of(text, part));
                } else {
                    self.read_definition(text, part, &mut created);
                }
            }
            next = close + 1;
        }
        if created.columns.is_empty() && is_word(text, tokens.get(next), "AS") {
            created.columns = output_names(text, &tokens[next + 1..end.max(next + 1)]);
        }
        for key in &mut self.keys {
            if key.table.is_empty() {
                key.schema = schema.clone().unwrap_or_default();
                key.table.clone_from(&name);
            }
        }
        self.created.push(created);
    }

    /// One item of `CREATE TABLE (...)`: a column, or a constraint.
    fn read_definition(&mut self, text: &str, part: &[Token], created: &mut Created) {
        let first = word(text, &part[0]).unwrap_or_default();
        let mut constraint = part;
        if first == "CONSTRAINT" {
            constraint = part.get(2..).unwrap_or_default();
        }
        let head = constraint
            .first()
            .and_then(|t| word(text, t))
            .unwrap_or_default();
        match head.as_str() {
            "PRIMARY" => created.primary = bracketed_names(text, constraint),
            "FOREIGN" => {
                let columns = bracketed_names(text, constraint);
                if let Some(at) = constraint
                    .iter()
                    .position(|t| is_word(text, Some(t), "REFERENCES"))
                {
                    self.reference(text, &constraint[at + 1..], columns);
                }
            }
            "UNIQUE" | "CHECK" | "EXCLUDE" | "LIKE" => {}
            _ if first != "CONSTRAINT" => {
                let Some(column) = name(text, &part[0]) else {
                    return;
                };
                let words: Vec<String> = part.iter().filter_map(|t| word(text, t)).collect();
                if words.windows(2).any(|w| w[0] == "PRIMARY" && w[1] == "KEY") {
                    created.primary = vec![column.clone()];
                }
                if let Some(at) = part
                    .iter()
                    .position(|t| is_word(text, Some(t), "REFERENCES"))
                {
                    self.reference(text, &part[at + 1..], vec![column.clone()]);
                }
                created.columns.push(column);
            }
            _ => {}
        }
    }

    /// `REFERENCES table [(columns)]`, for a key on the table being created.
    fn reference(&mut self, text: &str, tokens: &[Token], columns: Vec<String>) {
        let Some((mut parts, next)) = dotted(text, tokens, 0) else {
            return;
        };
        let ref_table = parts.pop().unwrap_or_default();
        let ref_columns = if tokens.get(next).is_some_and(|t| is(text, t, "(")) {
            bracketed_names(text, &tokens[next..])
        } else {
            Vec::new()
        };
        self.keys.push(ForeignKey {
            name: String::new(),
            // Filled in once the table's name is known.
            schema: String::new(),
            table: String::new(),
            columns,
            ref_schema: parts.pop().unwrap_or_default(),
            ref_table,
            ref_columns,
        });
    }
}

/// A column definition's name: its first token.
fn name_of(text: &str, part: &[Token]) -> Option<String> {
    name(text, part.first()?)
}

/// The names in the first `( ... )` of `tokens`.
fn bracketed_names(text: &str, tokens: &[Token]) -> Vec<String> {
    let Some(open) = tokens.iter().position(|t| is(text, t, "(")) else {
        return Vec::new();
    };
    let close = closing(text, tokens, open);
    split_commas(text, &tokens[open + 1..close.min(tokens.len())])
        .iter()
        .filter_map(|part| name_of(text, part))
        .collect()
}

// ---- where the cursor is --------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
enum Context {
    /// The start of a statement.
    Start,
    /// A table goes here; after `JOIN`, joins along foreign keys too.
    /// In `FROM` and `JOIN` (`alias`) the table comes with an alias, as in
    /// DBeaver.
    Relation {
        join: bool,
        alias: bool,
    },
    /// Right after a table: an alias or the next clause.
    AfterRelation,
    /// `INSERT INTO table (` columns.
    InsertColumns(String),
    /// After `JOIN ... ON`: conditions along foreign keys, then columns.
    JoinOn,
    /// After `::`.
    Type,
    Expression,
    /// After `name.` or `schema.table.`; the last name in quotes.
    Qualified(Vec<String>, bool),
}

/// The innermost clause keyword before the cursor at its bracket depth, or
/// the bracket the cursor is in, as `(` with the tokens before it.
fn clause(text: &str, before: &[&Token]) -> (Option<String>, Option<usize>) {
    let mut depth = 0i32;
    for (index, token) in before.iter().enumerate().rev() {
        if is(text, token, ")") {
            depth += 1;
        } else if is(text, token, "(") {
            depth -= 1;
            if depth < 0 {
                return (None, Some(index));
            }
        } else if depth == 0
            && let Some(word) = word(text, token)
            && CLAUSES.contains(&word.as_str())
        {
            return (Some(word), None);
        }
    }
    (None, None)
}

fn context(text: &str, before: &[&Token], scope: &Scope) -> Context {
    let Some(last) = before.last() else {
        return Context::Start;
    };
    let previous = before.len().checked_sub(2).map(|at| before[at]);
    if is(text, last, ":") && previous.is_some_and(|p| is(text, p, ":")) {
        return Context::Type;
    }
    match word(text, last).as_deref() {
        Some("FROM") => {
            return Context::Relation {
                join: false,
                alias: true,
            };
        }
        Some("UPDATE" | "INTO" | "TABLE" | "TRUNCATE" | "ONLY") => {
            return Context::Relation {
                join: false,
                alias: false,
            };
        }
        Some("JOIN") => {
            return Context::Relation {
                join: true,
                alias: true,
            };
        }
        Some("ON") => return Context::JoinOn,
        _ => {}
    }
    let (clause, bracket) = clause(text, before);
    if let Some(open) = bracket {
        // `INSERT INTO [schema.]table (` lists the table's columns.
        let mut at = open;
        let mut names = Vec::new();
        while at > 0
            && let Some(part) = name(text, before[at - 1])
        {
            names.push(part);
            at -= 1;
            if at > 0 && is(text, before[at - 1], ".") {
                at -= 1;
            } else {
                break;
            }
        }
        if at > 0
            && is_word(text, Some(before[at - 1]), "INTO")
            && let Some(table) = names.first()
        {
            return Context::InsertColumns(table.clone());
        }
        return Context::Expression;
    }
    let after_relation = matches!(
        clause.as_deref(),
        Some("FROM" | "JOIN" | "UPDATE" | "INTO" | "TABLE")
    );
    if is(text, last, ",") && clause.as_deref() == Some("FROM") {
        return Context::Relation {
            join: false,
            alias: true,
        };
    }
    if after_relation && (name(text, last).is_some() || is(text, last, ")")) {
        // `FROM t a` then a new word is the next clause; so is a word after
        // a subquery's closing bracket.
        let named = scope.relations.iter().any(|r| r.at <= last.range.start);
        if named || is(text, last, ")") {
            return Context::AfterRelation;
        }
    }
    if clause.as_deref() == Some("ON") && matches!(word(text, last).as_deref(), Some("AND" | "OR"))
    {
        return Context::JoinOn;
    }
    Context::Expression
}

// ---- building the list ----------------------------------------------------

/// Where a relation's columns come from.
enum Source<'a> {
    Known(&'a [String]),
    Catalog(String, String),
    Script(&'a Created),
}

struct Builder<'a> {
    catalog: &'a dyn Catalog,
    options: &'a Options<'a>,
    script: &'a Script,
    scope: &'a Scope,
    /// Where the qualified word starts.
    cursor: usize,
    /// Tables are inserted with an alias: `usuarios u`.
    alias_tables: bool,
    items: Vec<Item>,
    needs: Vec<Need>,
    prefix: &'a str,
}

impl<'a> Builder<'a> {
    fn fill(&mut self, context: &Context) {
        match context {
            Context::Start => self.keywords(&self.options.family.starts()),
            Context::Relation { join, .. } => {
                if *join {
                    self.joins();
                }
                self.relations();
            }
            Context::AfterRelation => {
                self.keywords(&self.options.family.after_relation());
            }
            Context::InsertColumns(table) => {
                let relation = self
                    .scope
                    .relations
                    .iter()
                    .find(|r| r.name.eq_ignore_ascii_case(table))
                    .cloned()
                    .unwrap_or_else(|| Relation {
                        name: table.clone(),
                        ..Relation::default()
                    });
                self.columns_of(&relation, None);
            }
            Context::JoinOn => {
                self.conditions();
                self.expression();
            }
            Context::Type => {
                for name in self.options.family.types() {
                    let insert = self.cased(name);
                    self.push(Kind::Type, insert.clone(), insert, String::new());
                }
            }
            Context::Expression => self.expression(),
            Context::Qualified(names, quoted) => self.qualified(names, *quoted),
        }
    }

    fn push(&mut self, kind: Kind, label: String, insert: String, detail: String) {
        let key = label.clone();
        self.push_keyed(kind, label, insert, detail, key);
    }

    fn push_keyed(
        &mut self,
        kind: Kind,
        label: String,
        insert: String,
        detail: String,
        key: String,
    ) {
        let caret = if insert.ends_with("()") {
            insert.len() - 1
        } else {
            insert.len()
        };
        self.items.push(Item {
            label,
            insert,
            caret,
            kind,
            detail,
            key,
        });
    }

    /// A keyword in the case the settings ask for: by default the case the
    /// user types in.
    fn cased(&self, keyword: &str) -> String {
        let lower = match self.options.case {
            KeywordCase::Upper => false,
            KeywordCase::Lower => true,
            KeywordCase::Typed => {
                self.prefix.chars().any(|c| c.is_ascii_lowercase())
                    && !self.prefix.chars().any(|c| c.is_ascii_uppercase())
            }
        };
        if lower {
            keyword.to_lowercase()
        } else {
            keyword.to_uppercase()
        }
    }

    fn keywords(&mut self, list: &[&str]) {
        for keyword in list {
            let insert = self.cased(keyword);
            self.push(Kind::Keyword, insert.clone(), insert, String::new());
        }
    }

    /// A name as SQL: quoted when it would not survive bare.
    fn ident(&self, name: &str) -> String {
        let plain = name.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '$');
        if plain && !sql::is_keyword(name) {
            name.to_owned()
        } else {
            (self.options.quote)(name)
        }
    }

    fn objects(&mut self, schema: &str) -> Option<&[DbObject]> {
        let objects = self.catalog.objects(schema);
        if objects.is_none() {
            self.needs.push(Need::Objects(schema.to_owned()));
        }
        objects
    }

    fn schema_named(&self, name: &str) -> Option<String> {
        self.catalog
            .schemas()
            .iter()
            .find(|schema| schema.eq_ignore_ascii_case(name))
            .cloned()
    }

    /// Finds a relation in the catalog: in `schema`, else the search path,
    /// else any schema loaded.
    fn find(
        &mut self,
        schema: Option<&str>,
        name: &str,
        quoted: bool,
    ) -> Option<(String, DbObject)> {
        let schemas: Vec<String> = match schema {
            Some(schema) => self.schema_named(schema).into_iter().collect(),
            None => self.options.search.to_vec(),
        };
        for schema in &schemas {
            if let Some(object) = self
                .objects(schema)
                .and_then(|objects| objects.iter().find(|o| same(&o.name, name, quoted)))
            {
                return Some((schema.clone(), object.clone()));
            }
        }
        if schema.is_some() {
            return None;
        }
        self.catalog.schemas().iter().find_map(|schema| {
            self.catalog
                .objects(schema)?
                .iter()
                .find(|o| o.kind.is_relation() && same(&o.name, name, quoted))
                .map(|object| (schema.clone(), object.clone()))
        })
    }

    fn created(&self, schema: Option<&str>, name: &str) -> Option<&'a Created> {
        created_in(self.script, schema, name)
    }

    fn source<'s>(&mut self, relation: &'s Relation) -> Option<Source<'s>>
    where
        'a: 's,
    {
        if let Some(columns) = &relation.columns {
            return Some(Source::Known(columns));
        }
        let (scope, script): (&'a Scope, &'a Script) = (self.scope, self.script);
        if relation.schema.is_none()
            && let Some(cte) = scope.cte(&relation.name)
        {
            return cte.columns.as_deref().map(Source::Known);
        }
        if let Some(created) = created_in(script, relation.schema.as_deref(), &relation.name) {
            return Some(Source::Script(created));
        }
        self.find(relation.schema.as_deref(), &relation.name, relation.quoted)
            .map(|(schema, object)| Source::Catalog(schema, object.name))
    }

    /// Lists a relation's columns; `handle` qualifies them.
    fn columns_of(&mut self, relation: &Relation, handle: Option<&str>) {
        let qualify = |column: &str, this: &Self| match handle {
            Some(handle) => format!("{}.{}", this.ident(handle), this.ident(column)),
            None => this.ident(column),
        };
        let table = relation.handle().to_owned();
        match self.source(relation) {
            Some(Source::Known(columns)) => {
                for column in columns {
                    let insert = qualify(column, self);
                    self.push_keyed(
                        Kind::Column,
                        column.clone(),
                        insert,
                        table.clone(),
                        column.clone(),
                    );
                }
            }
            Some(Source::Script(created)) => {
                for column in created.columns.clone() {
                    let insert = qualify(&column, self);
                    let detail = format!("{table} · in script");
                    self.push_keyed(Kind::Column, column.clone(), insert, detail, column);
                }
            }
            Some(Source::Catalog(schema, name)) => match self.catalog.columns(&schema, &name) {
                Some(columns) => {
                    for column in columns {
                        let insert = qualify(&column.name, self);
                        let key = if column.primary_key { " · key" } else { "" };
                        let detail = format!("{}{key} · {table}", column.data_type);
                        self.push_keyed(
                            Kind::Column,
                            column.name.clone(),
                            insert,
                            detail,
                            column.name.clone(),
                        );
                    }
                }
                None => self.needs.push(Need::Columns(schema, name)),
            },
            None => {}
        }
    }

    /// Tables, views, CTEs, the script's own tables and schemas.
    fn relations(&mut self) {
        for cte in self.scope.ctes.clone() {
            let insert = self.ident(&cte.name);
            self.push(Kind::Alias, cte.name.clone(), insert, "CTE".to_owned());
        }
        for created in self.script.created.clone() {
            let kind = if created.view {
                Kind::View
            } else {
                Kind::Table
            };
            let insert = match &created.schema {
                Some(schema) => format!("{}.{}", self.ident(schema), self.ident(&created.name)),
                None => self.ident(&created.name),
            };
            let (label, insert) = self.aliased(created.name.clone(), insert, &created.name);
            self.push_keyed(
                kind,
                label,
                insert,
                "in script".to_owned(),
                created.name.clone(),
            );
        }
        let search = self.options.search.to_vec();
        for schema in &search {
            for object in self.objects(schema).unwrap_or_default().to_vec() {
                if let Some(kind) = relation_kind(object.kind) {
                    let insert = self.ident(&object.name);
                    let (label, insert) = self.aliased(object.name.clone(), insert, &object.name);
                    self.push_keyed(kind, label, insert, schema.clone(), object.name);
                }
            }
        }
        // Other schemas' tables, qualified, once something is typed.
        if !self.prefix.is_empty() {
            for schema in self.catalog.schemas().to_vec() {
                if search.contains(&schema) {
                    continue;
                }
                for object in self.catalog.objects(&schema).unwrap_or_default().to_vec() {
                    if let Some(kind) = relation_kind(object.kind) {
                        let insert =
                            format!("{}.{}", self.ident(&schema), self.ident(&object.name));
                        let label = format!("{schema}.{}", object.name);
                        let (label, insert) = self.aliased(label, insert, &object.name);
                        self.push_keyed(kind, label, insert, schema.clone(), object.name);
                    }
                }
            }
        }
        for schema in self.catalog.schemas().to_vec() {
            let insert = self.ident(&schema);
            self.push(Kind::Schema, schema, insert, "schema".to_owned());
        }
    }

    /// A table's label and text with an alias after it, when the slot
    /// takes one: `usuarios` as `usuarios u`, avoiding the statement's other
    /// aliases.
    fn aliased(&self, label: String, insert: String, table: &str) -> (String, String) {
        if !self.alias_tables {
            return (label, insert);
        }
        let taken: Vec<String> = self
            .scope
            .relations
            .iter()
            .map(|r| r.handle().to_lowercase())
            .collect();
        let alias = short_alias(table, &taken);
        (format!("{label} {alias}"), format!("{insert} {alias}"))
    }

    fn expression(&mut self) {
        let relations = self.scope.relations.clone();
        let several = relations.len() > 1;
        let before = self.items.len();
        for relation in &relations {
            let handle = several.then(|| relation.handle().to_owned());
            self.columns_of(relation, handle.as_deref());
        }
        // A column only one table has needs no qualifier.
        if several {
            let added = &self.items[before..];
            let unique: Vec<bool> = added
                .iter()
                .map(|item| {
                    added
                        .iter()
                        .filter(|other| other.key.eq_ignore_ascii_case(&item.key))
                        .count()
                        == 1
                })
                .collect();
            for (item, unique) in self.items[before..].iter_mut().zip(unique) {
                if unique && item.kind == Kind::Column {
                    item.insert = item
                        .insert
                        .rsplit_once('.')
                        .map_or_else(|| item.insert.clone(), |(_, column)| column.to_owned());
                    item.caret = item.insert.len();
                }
            }
        }
        for relation in &relations {
            let handle = relation.handle().to_owned();
            let insert = self.ident(&handle);
            let detail = if relation.alias.is_some() && relation.columns.is_none() {
                relation.name.clone()
            } else {
                "alias".to_owned()
            };
            self.push(Kind::Alias, handle, insert, detail);
        }
        if self.prefix.is_empty() {
            return;
        }
        for schema in self.options.search {
            for object in self.catalog.objects(schema).unwrap_or_default().to_vec() {
                if object.kind == ObjectKind::Function {
                    let insert = format!("{}()", self.ident(&object.name));
                    self.push_keyed(
                        Kind::Function,
                        format!("{}()", object.name),
                        insert,
                        schema.clone(),
                        object.name,
                    );
                }
            }
        }
        for function in self.options.family.functions() {
            let name = self.cased(function);
            self.push_keyed(
                Kind::Function,
                format!("{name}()"),
                format!("{name}()"),
                String::new(),
                name,
            );
        }
        self.keywords(PHRASES);
        self.keywords(&self.options.family.keywords());
    }

    fn qualified(&mut self, names: &[String], quoted: bool) {
        match names {
            [handle] => {
                if let Some(relation) = self.scope.by_handle(handle).cloned() {
                    self.columns_of(&relation, None);
                } else if let Some(schema) = self.schema_named(handle) {
                    for object in self.objects(&schema).unwrap_or_default().to_vec() {
                        // Sequences are read through functions, not by name.
                        if object.kind == ObjectKind::Sequence {
                            continue;
                        }
                        let kind = relation_kind(object.kind).unwrap_or(Kind::Function);
                        let (label, insert) = if kind == Kind::Function {
                            (
                                format!("{}()", object.name),
                                format!("{}()", self.ident(&object.name)),
                            )
                        } else {
                            let insert = self.ident(&object.name);
                            self.aliased(object.name.clone(), insert, &object.name)
                        };
                        self.push_keyed(
                            kind,
                            label,
                            insert,
                            object_label(object.kind),
                            object.name,
                        );
                    }
                } else {
                    // A table named in full before its alias exists, or one
                    // the statement does not read yet.
                    let relation = Relation {
                        name: handle.clone(),
                        quoted,
                        ..Relation::default()
                    };
                    self.columns_of(&relation, None);
                }
            }
            [schema, table] => {
                let relation = Relation {
                    schema: Some(schema.clone()),
                    name: table.clone(),
                    quoted,
                    ..Relation::default()
                };
                self.columns_of(&relation, None);
            }
            _ => {}
        }
    }

    /// The catalog tables a relation of the statement is, with its schema.
    fn resolved(&mut self, relation: &Relation) -> Option<(String, String)> {
        if relation.columns.is_some() || self.scope.cte(&relation.name).is_some() {
            return None;
        }
        if let Some(created) = self.created(relation.schema.as_deref(), &relation.name) {
            let schema = created
                .schema
                .clone()
                .or_else(|| self.options.search.first().cloned())
                .unwrap_or_default();
            return Some((schema, created.name.clone()));
        }
        self.find(relation.schema.as_deref(), &relation.name, relation.quoted)
            .map(|(schema, object)| (schema, object.name))
    }

    /// Foreign keys touching `schema`, from the catalog and the script.
    fn keys(&mut self, schemas: &[String]) -> Vec<ForeignKey> {
        let mut keys = Vec::new();
        for schema in schemas {
            match self.catalog.foreign_keys(schema) {
                Some(found) => keys.extend(found.iter().cloned()),
                None => self.needs.push(Need::ForeignKeys(schema.clone())),
            }
        }
        let default = self.options.search.first().cloned().unwrap_or_default();
        for key in &self.script.keys {
            let mut key = key.clone();
            if key.schema.is_empty() {
                key.schema.clone_from(&default);
            }
            if key.ref_schema.is_empty() {
                key.ref_schema.clone_from(&default);
            }
            if key.ref_columns.is_empty() {
                key.ref_columns = self.primary_key(&key.ref_schema, &key.ref_table);
            }
            if key.ref_columns.len() == key.columns.len() {
                keys.push(key);
            }
        }
        let mut seen = HashSet::new();
        keys.retain(|key| {
            seen.insert((
                key.schema.to_lowercase(),
                key.table.to_lowercase(),
                key.columns.join(","),
                key.ref_table.to_lowercase(),
            ))
        });
        keys
    }

    fn primary_key(&mut self, schema: &str, table: &str) -> Vec<String> {
        if let Some(created) = self.created(Some(schema), table) {
            return created.primary.clone();
        }
        match self.catalog.columns(schema, table) {
            Some(columns) => columns
                .iter()
                .filter(|c| c.primary_key)
                .map(|c| c.name.clone())
                .collect(),
            None => {
                self.needs
                    .push(Need::Columns(schema.to_owned(), table.to_owned()));
                Vec::new()
            }
        }
    }

    /// The statement's tables before the cursor, resolved, with handles.
    fn tables_before(&mut self) -> Vec<(String, String, String)> {
        let relations: Vec<Relation> = self
            .scope
            .relations
            .iter()
            .filter(|r| r.at < self.cursor)
            .cloned()
            .collect();
        relations
            .iter()
            .filter_map(|relation| {
                let (schema, table) = self.resolved(relation)?;
                Some((schema, table, relation.handle().to_owned()))
            })
            .collect()
    }

    /// After `JOIN`: tables a foreign key links to one already read, with
    /// the condition.
    fn joins(&mut self) {
        let tables = self.tables_before();
        if tables.is_empty() {
            return;
        }
        let mut schemas: Vec<String> = tables.iter().map(|(s, ..)| s.clone()).collect();
        schemas.extend(self.options.search.iter().cloned());
        schemas.sort();
        schemas.dedup();
        let keys = self.keys(&schemas);
        let mut taken: Vec<String> = self
            .scope
            .relations
            .iter()
            .map(|r| r.handle().to_lowercase())
            .collect();
        for (schema, table, handle) in &tables {
            for key in &keys {
                let from_here = key.schema.eq_ignore_ascii_case(schema)
                    && key.table.eq_ignore_ascii_case(table);
                let to_here = key.ref_schema.eq_ignore_ascii_case(schema)
                    && key.ref_table.eq_ignore_ascii_case(table);
                let (other_schema, other, ours, theirs) = if from_here {
                    (
                        &key.ref_schema,
                        &key.ref_table,
                        &key.columns,
                        &key.ref_columns,
                    )
                } else if to_here {
                    (&key.schema, &key.table, &key.ref_columns, &key.columns)
                } else {
                    continue;
                };
                let alias = short_alias(other, &taken);
                let named = if self.options.search.iter().any(|s| s == other_schema) {
                    self.ident(other)
                } else {
                    format!("{}.{}", self.ident(other_schema), self.ident(other))
                };
                let condition = ours
                    .iter()
                    .zip(theirs)
                    .map(|(own, their)| {
                        format!(
                            "{alias}.{} = {}.{}",
                            self.ident(their),
                            self.ident(handle),
                            self.ident(own)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" AND ");
                let on = self.cased("ON");
                let insert = format!("{named} {alias} {on} {condition}");
                self.push_keyed(
                    Kind::Join,
                    insert.clone(),
                    insert,
                    String::new(),
                    other.clone(),
                );
                taken.push(alias);
            }
        }
    }

    /// After `ON`: the conditions linking the table just joined to the ones
    /// before it.
    fn conditions(&mut self) {
        let mut tables = self.tables_before();
        let Some((schema, table, handle)) = tables.pop() else {
            return;
        };
        let mut schemas: Vec<String> = tables.iter().map(|(s, ..)| s.clone()).collect();
        schemas.push(schema.clone());
        schemas.sort();
        schemas.dedup();
        let keys = self.keys(&schemas);
        for (other_schema, other, other_handle) in &tables {
            for key in &keys {
                let pairs: Vec<(String, String)> = if key.schema.eq_ignore_ascii_case(&schema)
                    && key.table.eq_ignore_ascii_case(&table)
                    && key.ref_schema.eq_ignore_ascii_case(other_schema)
                    && key.ref_table.eq_ignore_ascii_case(other)
                {
                    key.columns
                        .iter()
                        .zip(&key.ref_columns)
                        .map(|(a, b)| (a.clone(), b.clone()))
                        .collect()
                } else if key.ref_schema.eq_ignore_ascii_case(&schema)
                    && key.ref_table.eq_ignore_ascii_case(&table)
                    && key.schema.eq_ignore_ascii_case(other_schema)
                    && key.table.eq_ignore_ascii_case(other)
                {
                    key.ref_columns
                        .iter()
                        .zip(&key.columns)
                        .map(|(a, b)| (a.clone(), b.clone()))
                        .collect()
                } else {
                    continue;
                };
                let condition = pairs
                    .iter()
                    .map(|(own, their)| {
                        format!(
                            "{}.{} = {}.{}",
                            self.ident(&handle),
                            self.ident(own),
                            self.ident(other_handle),
                            self.ident(their)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" AND ");
                self.push_keyed(
                    Kind::Join,
                    condition.clone(),
                    condition,
                    "foreign key".to_owned(),
                    handle.clone(),
                );
            }
        }
    }
}

/// The last table or view the script creates under that name.
fn created_in<'s>(script: &'s Script, schema: Option<&str>, name: &str) -> Option<&'s Created> {
    script.created.iter().rev().find(|created| {
        created.name.eq_ignore_ascii_case(name)
            && schema.is_none_or(|s| {
                created
                    .schema
                    .as_deref()
                    .is_none_or(|own| own.eq_ignore_ascii_case(s))
            })
    })
}

fn relation_kind(kind: ObjectKind) -> Option<Kind> {
    match kind {
        ObjectKind::Table | ObjectKind::ForeignTable => Some(Kind::Table),
        ObjectKind::View | ObjectKind::MaterializedView => Some(Kind::View),
        ObjectKind::Function | ObjectKind::Sequence | ObjectKind::Key => None,
    }
}

fn object_label(kind: ObjectKind) -> String {
    match kind {
        ObjectKind::Table => "table",
        ObjectKind::View => "view",
        ObjectKind::MaterializedView => "materialized view",
        ObjectKind::ForeignTable => "foreign table",
        ObjectKind::Function => "function",
        ObjectKind::Sequence => "sequence",
        ObjectKind::Key => "key",
    }
    .to_owned()
}

/// `order_items` as `oi`, unless taken: then `oi2`, `oi3`...
fn short_alias(table: &str, taken: &[String]) -> String {
    let mut base: String = table
        .split(['_', ' '])
        .filter_map(|part| part.chars().next())
        .filter(char::is_ascii_alphabetic)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if base.is_empty() || sql::is_keyword(&base) {
        base.insert(0, 't');
    }
    if !taken.contains(&base) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}{n}"))
        .find(|alias| !taken.contains(alias))
        .unwrap_or(base)
}

// ---- matching -------------------------------------------------------------

/// How well `key` matches what was typed: 0 by prefix, 1 by the start of a
/// word inside it (`items` in `order_items`) or its initials, 2 by letters
/// in order. `None` when it does not match.
fn score(key: &str, prefix: &str) -> Option<u8> {
    if prefix.is_empty() {
        return Some(0);
    }
    let key = key.to_lowercase();
    let prefix = prefix.to_lowercase();
    if key.starts_with(&prefix) {
        return Some(0);
    }
    let initials: String = key
        .split('_')
        .filter_map(|part| part.chars().next())
        .collect();
    if key.split('_').skip(1).any(|part| part.starts_with(&prefix)) || initials.starts_with(&prefix)
    {
        return Some(1);
    }
    let mut letters = key.chars();
    prefix
        .chars()
        .all(|wanted| letters.any(|c| c == wanted))
        .then_some(2)
}

/// Whether a catalog name is the one typed: exactly when it was quoted,
/// else in any case.
fn same(catalog: &str, typed: &str, quoted: bool) -> bool {
    if quoted {
        catalog == typed
    } else {
        catalog.eq_ignore_ascii_case(typed)
    }
}

/// Where a kind sorts among equally good items: where a table goes
/// (`relation`), tables and views lead.
fn kind_order(kind: Kind, relation: bool) -> (bool, Kind) {
    let table = matches!(kind, Kind::Table | Kind::View);
    (!(relation && table), kind)
}

/// Best matches first, the latest accepted first among them; with nothing
/// typed, each kind keeps its own order (columns as the table has them).
fn rank(items: Vec<Item>, prefix: &str, history: &History, relation: bool) -> Vec<Item> {
    let mut scored: Vec<(u8, Item)> = items
        .into_iter()
        .filter_map(|item| Some((score(&item.key, prefix)?, item)))
        .collect();
    scored.sort_by(|(a, x), (b, y)| {
        let order = a
            .cmp(b)
            .then_with(|| history.recency(y).cmp(&history.recency(x)))
            .then(kind_order(x.kind, relation).cmp(&kind_order(y.kind, relation)));
        if prefix.is_empty() {
            return order;
        }
        order
            .then(x.key.len().cmp(&y.key.len()))
            .then_with(|| x.label.to_lowercase().cmp(&y.label.to_lowercase()))
    });
    let mut seen = HashSet::new();
    scored
        .into_iter()
        .map(|(_, item)| item)
        .filter(|item| seen.insert(item.insert.clone()))
        .take(MAX_ITEMS)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[derive(Default)]
    struct Fake {
        schemas: Vec<String>,
        objects: HashMap<String, Vec<DbObject>>,
        columns: HashMap<(String, String), Vec<ColumnInfo>>,
        keys: HashMap<String, Vec<ForeignKey>>,
    }

    impl Catalog for Fake {
        fn schemas(&self) -> &[String] {
            &self.schemas
        }
        fn objects(&self, schema: &str) -> Option<&[DbObject]> {
            self.objects.get(schema).map(Vec::as_slice)
        }
        fn columns(&self, schema: &str, relation: &str) -> Option<&[ColumnInfo]> {
            self.columns
                .get(&(schema.to_owned(), relation.to_owned()))
                .map(Vec::as_slice)
        }
        fn foreign_keys(&self, schema: &str) -> Option<&[ForeignKey]> {
            self.keys.get(schema).map(Vec::as_slice)
        }
    }

    fn object(name: &str, kind: ObjectKind) -> DbObject {
        DbObject {
            schema: "public".into(),
            name: name.into(),
            kind,
            comment: None,
            estimated_rows: None,
            subtype: None,
            detail: None,
        }
    }

    fn column(name: &str, primary_key: bool) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            data_type: "integer".into(),
            nullable: !primary_key,
            default: None,
            primary_key,
            comment: None,
        }
    }

    /// A shop: customers, their orders and the orders' lines.
    fn shop() -> Fake {
        let mut fake = Fake {
            schemas: vec!["public".into(), "sales".into()],
            ..Fake::default()
        };
        fake.objects.insert(
            "public".into(),
            vec![
                object("customers", ObjectKind::Table),
                object("orders", ObjectKind::Table),
                object("order_items", ObjectKind::Table),
                object("revenue", ObjectKind::View),
                object("slugify", ObjectKind::Function),
            ],
        );
        fake.objects
            .insert("sales".into(), vec![object("targets", ObjectKind::Table)]);
        let mut columns = |table: &str, names: &[(&str, bool)]| {
            fake.columns.insert(
                ("public".into(), table.into()),
                names.iter().map(|(n, pk)| column(n, *pk)).collect(),
            );
        };
        columns(
            "customers",
            &[("id", true), ("name", false), ("country", false)],
        );
        columns(
            "orders",
            &[("id", true), ("customer_id", false), ("total", false)],
        );
        columns("order_items", &[("order_id", false), ("sku", false)]);
        let key = |table: &str, column: &str, target: &str| ForeignKey {
            name: format!("{table}_{column}_fkey"),
            schema: "public".into(),
            table: table.into(),
            columns: vec![column.into()],
            ref_schema: "public".into(),
            ref_table: target.into(),
            ref_columns: vec!["id".into()],
        };
        fake.keys.insert(
            "public".into(),
            vec![
                key("orders", "customer_id", "customers"),
                key("order_items", "order_id", "orders"),
            ],
        );
        fake.keys.insert("sales".into(), Vec::new());
        fake
    }

    /// Completes at `|` in `text`.
    fn at(catalog: &Fake, text: &str) -> Completion {
        at_with(catalog, text, KeywordCase::default(), &History::default())
    }

    /// Completes at `|` in `text` with keywords in `case`, after `history`.
    fn at_with(catalog: &Fake, text: &str, case: KeywordCase, history: &History) -> Completion {
        let cursor = text.find('|').expect("a cursor");
        let text = text.replacen('|', "", 1);
        let search = vec!["public".to_owned()];
        let quote = |name: &str| format!("\"{}\"", name.replace('"', "\"\""));
        complete(
            &text,
            cursor,
            catalog,
            &Options {
                search: &search,
                quote: &quote,
                family: Family::Other,
                case,
                history,
            },
        )
        .expect("a completion")
    }

    fn inserts(completion: &Completion) -> Vec<&str> {
        completion.items.iter().map(|i| i.insert.as_str()).collect()
    }

    #[test]
    fn tables_follow_from_and_columns_follow_select() {
        let shop = shop();
        let tables = at(&shop, "SELECT * FROM ord|");
        assert_eq!(inserts(&tables)[..2], ["orders o", "order_items oi"]);
        // No alias where SQL takes none.
        assert_eq!(inserts(&at(&shop, "INSERT INTO ord|"))[0], "orders");
        assert_eq!(tables.prefix, "ord");

        let columns = at(&shop, "SELECT na| FROM customers");
        assert_eq!(inserts(&columns)[0], "name");
        assert!(columns.items[0].detail.starts_with("integer"));

        // Several tables: shared names are qualified by alias.
        let joined = at(
            &shop,
            "SELECT i| FROM customers c JOIN orders o ON o.customer_id = c.id",
        );
        let found = inserts(&joined);
        assert!(
            found.contains(&"c.id") && found.contains(&"o.id"),
            "{found:?}"
        );
        let total = at(&shop, "SELECT tot| FROM customers c JOIN orders o ON true");
        assert_eq!(inserts(&total)[0], "total");
    }

    #[test]
    fn aliases_and_schemas_qualify() {
        let shop = shop();
        let alias = at(&shop, "SELECT o.| FROM orders o");
        assert_eq!(inserts(&alias), ["id", "customer_id", "total"]);
        let schema = at(&shop, "SELECT * FROM sales.|");
        assert_eq!(inserts(&schema), ["targets t"]);
        // As in DBeaver: the alias comes with the table, and works at once.
        let joined = at(
            &shop,
            "SELECT * FROM \"public\".\"orders\" t JOIN sales.tar|",
        );
        assert_eq!(inserts(&joined), ["targets t2"]);
        let alias = at(
            &shop,
            "SELECT * FROM \"public\".\"customers\" c JOIN sales.targets t ON t.customer_id = c.id\nWHERE c.|",
        );
        assert_eq!(inserts(&alias), ["id", "name", "country"]);
        let full = at(&shop, "SELECT public.customers.na| FROM public.customers");
        assert_eq!(inserts(&full), ["name"]);
        // Other schemas' tables come qualified.
        assert!(inserts(&at(&shop, "SELECT * FROM targ|")).contains(&"sales.targets t"));
    }

    #[test]
    fn joins_follow_foreign_keys() {
        let shop = shop();
        let join = at(&shop, "SELECT * FROM customers c JOIN |");
        // Tables lead; the join along the key comes first after them.
        let first_join = join.items.iter().find(|item| item.kind == Kind::Join);
        assert_eq!(
            first_join.map(|item| item.insert.as_str()),
            Some("orders o ON o.customer_id = c.id"),
            "{:?}",
            inserts(&join)
        );
        let lines = at(&shop, "SELECT * FROM orders o JOIN |");
        let found = inserts(&lines);
        assert!(found.contains(&"customers c ON c.id = o.customer_id"));
        assert!(found.contains(&"order_items oi ON oi.order_id = o.id"));

        let on = at(&shop, "SELECT * FROM customers c JOIN orders o ON |");
        assert_eq!(on.items[0].insert, "o.customer_id = c.id");
        assert_eq!(on.items[0].kind, Kind::Join);
    }

    #[test]
    fn ctes_subqueries_and_the_scripts_own_tables_count() {
        let shop = shop();
        let cte = at(
            &shop,
            "WITH big AS (SELECT id, total AS amount FROM orders) SELECT b.| FROM big b",
        );
        assert_eq!(inserts(&cte), ["id", "amount"]);
        let sub = at(
            &shop,
            "SELECT x.| FROM (SELECT count(*), name n FROM customers) x",
        );
        assert_eq!(inserts(&sub), ["count", "n"]);

        let script = "CREATE TABLE notes (id serial PRIMARY KEY, body text,\n  customer_id int REFERENCES customers);\nSELECT * FROM notes n JOIN |";
        let joined = at(&shop, script);
        assert_eq!(
            joined
                .items
                .iter()
                .find(|item| item.kind == Kind::Join)
                .map(|item| item.insert.as_str()),
            Some("customers c ON c.id = n.customer_id"),
            "{:?}",
            inserts(&joined)
        );
        let columns = at(
            &shop,
            "CREATE TABLE notes (id int, body text);\nSELECT bo| FROM notes",
        );
        assert_eq!(inserts(&columns)[0], "body");
        let tables = at(
            &shop,
            "CREATE VIEW recent AS SELECT id FROM orders;\nSELECT * FROM rec|",
        );
        assert_eq!(inserts(&tables)[0], "recent r");
    }

    #[test]
    fn keywords_fit_the_place_and_the_case() {
        let shop = shop();
        assert!(inserts(&at(&shop, "sel|")).contains(&"select"));
        assert!(inserts(&at(&shop, "SEL|")).contains(&"SELECT"));
        let after = at(&shop, "SELECT * FROM orders o wh|");
        assert_eq!(inserts(&after), ["where"]);
        let types = at(&shop, "SELECT id::tex|");
        assert_eq!(inserts(&types), ["text"]);
        let insert = at(&shop, "INSERT INTO orders (|");
        assert_eq!(inserts(&insert), ["id", "customer_id", "total"]);
    }

    #[test]
    fn nothing_inside_strings_and_comments() {
        let shop = shop();
        let none = |text: &str| {
            let cursor = text.find('|').expect("cursor");
            let text = text.replacen('|', "", 1);
            let quote = |n: &str| n.to_owned();
            complete(
                &text,
                cursor,
                &shop,
                &Options {
                    search: &[],
                    quote: &quote,
                    family: Family::Other,
                    case: KeywordCase::default(),
                    history: &History::default(),
                },
            )
        };
        assert!(none("SELECT 'or|").is_none());
        assert!(none("SELECT 1 -- or|").is_none());
        assert!(none("/* or| */").is_none());
        assert!(none("SELECT 12|").is_none());
        assert!(none("SELECT 'a' |").is_some());
    }

    #[test]
    fn missing_catalog_data_is_asked_for() {
        let mut shop = shop();
        shop.columns.clear();
        shop.objects.remove("sales");
        let columns = at(&shop, "SELECT | FROM orders");
        assert!(
            columns
                .needs
                .contains(&Need::Columns("public".into(), "orders".into()))
        );
        let objects = at(&shop, "SELECT * FROM sales.|");
        assert_eq!(objects.needs, [Need::Objects("sales".into())]);
    }

    #[test]
    fn names_are_quoted_when_needed() {
        let mut shop = shop();
        shop.objects
            .get_mut("public")
            .expect("public")
            .push(object("Order Log", ObjectKind::Table));
        let found = at(&shop, "SELECT * FROM log|");
        assert!(
            inserts(&found).contains(&"\"Order Log\" ol"),
            "{:?}",
            inserts(&found)
        );
        assert_eq!(short_alias("order_items", &["oi".into()]), "oi2");
    }

    #[test]
    fn mongo_completes_collections_methods_and_operators() {
        let orders = [object("orders", ObjectKind::Table)];
        let mongo = |text: &str, collections: Option<&[DbObject]>| {
            let cursor = text.find('|').expect("a cursor");
            let text = text.replacen('|', "", 1);
            complete_mongo(&text, cursor, Some("shop"), collections).expect("a completion")
        };
        let labels = |completion: &Completion| -> Vec<String> {
            completion.items.iter().map(|i| i.label.clone()).collect()
        };
        assert!(labels(&mongo("d|", None)).contains(&"db".to_owned()));
        let collections = mongo("db.o|", Some(&orders));
        assert_eq!(inserts(&collections)[0], "orders");
        assert_eq!(
            mongo("db.|", None).needs,
            vec![Need::Objects("shop".into())]
        );
        assert!(inserts(&mongo("db.orders.fi|", None)).contains(&"find()"));
        assert!(inserts(&mongo("db.orders.find({}).so|", None)).contains(&"sort()"));
        assert_eq!(
            inserts(&mongo("db.orders.find({ total: { $g|", None))[..3],
            ["$gt", "$gte", "$group"]
        );
    }

    #[test]
    fn each_database_family_gets_its_own_words() {
        let shop = shop();
        let search = vec!["public".to_owned()];
        let quote = |name: &str| format!("`{}`", name.replace('`', "``"));
        let with = |family: Family, text: &str| {
            let cursor = text.find('|').expect("a cursor");
            let text = text.replacen('|', "", 1);
            let options = Options {
                search: &search,
                quote: &quote,
                family,
                case: KeywordCase::default(),
                history: &History::default(),
            };
            complete(&text, cursor, &shop, &options).expect("a completion")
        };
        let mysql = with(Family::MySql, "SELECT ar| FROM orders");
        assert!(!inserts(&mysql).contains(&"array_agg()"));
        let postgres = with(Family::Postgres, "SELECT ar| FROM orders");
        assert!(inserts(&postgres).contains(&"array_agg()"));
        assert!(inserts(&with(Family::Sqlite, "PR|")).contains(&"PRAGMA"));
        assert!(inserts(&with(Family::MySql, "DESC|")).contains(&"DESCRIBE"));
        assert!(!inserts(&with(Family::Sqlite, "EXPLAIN A|")).contains(&"EXPLAIN ANALYZE"));
    }

    #[test]
    fn backticks_quote_names() {
        let shop = shop();
        let columns = at(&shop, "SELECT o.| FROM `orders` o");
        assert_eq!(inserts(&columns), ["id", "customer_id", "total"]);
        let qualified = at(&shop, "SELECT `o`.| FROM orders o");
        assert_eq!(inserts(&qualified), ["id", "customer_id", "total"]);
        let schema = at(&shop, "SELECT * FROM `public`.ord|");
        assert_eq!(inserts(&schema), ["orders o", "order_items oi"]);
    }

    #[test]
    fn redis_lines_complete_commands_and_keys() {
        let labels = |c: Option<Completion>| -> Vec<String> {
            c.expect("completion")
                .items
                .into_iter()
                .map(|i| i.label)
                .collect()
        };
        let got = labels(complete_redis("hge", 3, None, None));
        assert!(
            got.contains(&"HGET".to_owned()) && got.contains(&"HGETALL".to_owned()),
            "{got:?}"
        );
        let got = labels(complete_redis("CONFIG g", 8, None, None));
        assert_eq!(got, ["GET"]);
        let keys = [
            object("user:1", ObjectKind::Key),
            object("order:9", ObjectKind::Key),
        ];
        let got = labels(complete_redis("GET us", 6, Some("db0"), Some(&keys)));
        assert_eq!(got, ["user:1"]);
        let text = "AUTH secret
MULTI
HGETALL us";
        let got = labels(complete_redis(text, text.len(), Some("db0"), Some(&keys)));
        assert_eq!(got, ["user:1"]);
        let waiting = complete_redis("GET us", 6, Some("db0"), None).expect("completion");
        assert_eq!(waiting.needs, [Need::Keys("db0".into())]);
        assert!(complete_redis("SET k \"open", 11, None, None).is_none());
    }

    #[test]
    fn accepted_items_rise_on_their_connection() {
        let shop = shop();
        let fresh = at(&shop, "SELECT * FROM orders WHERE |");
        let first = fresh.items[0].clone();
        let last = fresh
            .items
            .iter()
            .rfind(|item| item.kind == Kind::Column)
            .expect("a column")
            .clone();
        assert_ne!(first, last);
        let mut history = History::default();
        history.accepted(&last);
        let used = at_with(
            &shop,
            "SELECT * FROM orders WHERE |",
            KeywordCase::default(),
            &history,
        );
        assert_eq!(used.items[0].insert, last.insert);
        // A worse match stays below a better one, used or not.
        let typed = at_with(
            &shop,
            "SELECT * FROM orders WHERE cu|",
            KeywordCase::default(),
            &history,
        );
        assert_eq!(typed.items[0].insert, "customer_id");
    }

    #[test]
    fn tables_lead_an_empty_relation_slot() {
        let shop = shop();
        let join = at(&shop, "SELECT * FROM orders o JOIN |");
        assert!(join.items.iter().any(|item| item.kind == Kind::Join));
        assert_eq!(join.items[0].kind, Kind::Table);
        let mut with_cte = at(&shop, "WITH recent AS (SELECT 1 AS id) SELECT * FROM |");
        assert_eq!(with_cte.items.remove(0).kind, Kind::Table);
        // Elsewhere the kinds keep their order.
        let on = at(&shop, "SELECT * FROM orders o JOIN customers c ON |");
        assert_ne!(on.items[0].kind, Kind::Table);
    }

    #[test]
    fn quoted_names_match_their_case() {
        let mut shop = shop();
        shop.objects
            .get_mut("public")
            .expect("public")
            .push(object("Users", ObjectKind::Table));
        shop.columns.insert(
            ("public".into(), "Users".into()),
            vec![column("id", true), column("email", false)],
        );
        assert!(inserts(&at(&shop, "SELECT \"Users\".| FROM x")).contains(&"email"));
        assert!(inserts(&at(&shop, "SELECT users.| FROM x")).contains(&"email"));
        let wrong = at(&shop, "SELECT \"users\".| FROM x");
        assert!(!inserts(&wrong).contains(&"email"));
        assert!(inserts(&at(&shop, "SELECT u.| FROM \"Users\" u")).contains(&"email"));
        assert!(!inserts(&at(&shop, "SELECT u.| FROM \"USERS\" u")).contains(&"email"));
    }

    #[test]
    fn keyword_case_follows_the_setting() {
        let shop = shop();
        let history = History::default();
        let keyword = |case, text| {
            at_with(&shop, text, case, &history)
                .items
                .into_iter()
                .find(|item| item.kind == Kind::Keyword)
                .expect("a keyword")
                .insert
        };
        assert_eq!(keyword(KeywordCase::Typed, "sel|"), "select");
        assert_eq!(keyword(KeywordCase::Typed, "|"), keyword(KeywordCase::Upper, "|"));
        assert_eq!(keyword(KeywordCase::Upper, "sel|"), "SELECT");
        assert_eq!(keyword(KeywordCase::Lower, "SEL|"), "select");
    }

    #[test]
    fn list_opens_by_itself_where_a_name_must_follow() {
        let opens = |text: &str| opens_by_itself(text, text.len());
        assert!(opens("SELECT * FROM "));
        assert!(opens("select * from orders o join "));
        assert!(opens("SELECT * FROM a JOIN b ON "));
        assert!(opens("INSERT INTO "));
        assert!(opens("INSERT INTO public.orders ("));
        assert!(opens("SELECT o."));
        assert!(opens("SELECT \"Users\"."));
        assert!(!opens("SELECT count("));
        assert!(!opens("SELECT a, "));
        assert!(!opens("SELECT * FROM  "));
        assert!(!opens("SELECT 'FROM "));
        assert!(!opens("-- FROM "));
        assert!(!opens("SELECT 1 /* o."));
    }
}
