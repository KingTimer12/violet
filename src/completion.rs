//! Context-sensitive SQL completion: what fits where the cursor is, drawn
//! from the catalog, from the statement around the cursor (its tables,
//! aliases, subqueries and CTEs), from the foreign keys between tables, and
//! from tables and views the script itself creates.
//!
//! Like the lexer it reads tokens, not a grammar, so it never fails: text it
//! cannot place gets columns and keywords.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;

use violet_core::{ColumnInfo, DbObject, ForeignKey, ObjectKind};

use crate::dialect::Family;
use crate::settings::KeywordCase;
use crate::sql::{self, Token, TokenKind};

/// What completion reads from the database. `None` means not loaded yet;
/// the completion then lists what to load in [`Completion::needs`].
pub trait Catalog {
    fn schemas(&self) -> &[String];
    fn objects(&self, schema: &str) -> Option<&[DbObject]>;
    /// The object called `name` in `schema`: by exact name when `quoted`,
    /// else in any case; only tables and views and the like when `relation`.
    /// A catalog with many objects answers from an index.
    fn object(&self, schema: &str, name: &str, quoted: bool, relation: bool) -> Option<&DbObject> {
        self.objects(schema)?
            .iter()
            .find(|o| (!relation || o.kind.is_relation()) && same(&o.name, name, quoted))
    }
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
    /// A key-value database's keys starting with a prefix.
    KeysMatching(String, String),
}

impl Completion {
    /// Whether the catalog now answers some of what [`Self::needs`] lists, so
    /// the list is worth making again. Key lookups cannot be told from here:
    /// they always count.
    #[must_use]
    pub fn needs_answered(&self, catalog: &dyn Catalog) -> bool {
        self.needs.iter().any(|need| match need {
            Need::Objects(schema) => catalog.objects(schema).is_some(),
            Need::Columns(schema, relation) => catalog.columns(schema, relation).is_some(),
            Need::ForeignKeys(schema) => catalog.foreign_keys(schema).is_some(),
            Need::Keys(_) | Need::KeysMatching(..) => true,
        })
    }
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
    /// The database connected to, which may lead `database.schema.table`.
    pub database: Option<&'a str>,
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

/// A key-value database's keys, as far as they are known.
#[derive(Clone, Copy, Debug)]
pub struct Keyspace<'a> {
    pub schema: &'a str,
    /// The keys the tree listed; `None` while they have not been.
    pub listed: Option<&'a [DbObject]>,
    /// The tree listed every key, so no other key starts with a prefix.
    pub whole: bool,
    /// Keys looked up by prefix: the prefix, and its keys once found.
    pub found: &'a [(&'a str, Option<&'a [DbObject]>)],
}

/// How many keys a lookup by prefix asks for.
pub const KEY_LOOKUP: u32 = 200;

/// Whether `c` belongs to a Redis word: anything but blanks and quotes, so
/// `user:1:name`, `a-b` and `v1.2` are one word each.
pub fn redis_word(c: char) -> bool {
    !c.is_whitespace() && !matches!(c, '"' | '\'')
}

/// A `SCAN MATCH` glob for keys starting with `prefix`, its glob
/// characters escaped.
pub fn key_glob(prefix: &str) -> String {
    let mut glob = String::with_capacity(prefix.len() + 1);
    for c in prefix.chars() {
        if matches!(c, '*' | '?' | '[' | '\\') {
            glob.push('\\');
        }
        glob.push(c);
    }
    glob.push('*');
    glob
}

/// Whether `line` ends inside a quoted word; a backslash escapes the next
/// character, and the other kind of quote counts for nothing inside one.
fn in_quotes(line: &str) -> bool {
    let mut open = None;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match (open, c) {
            (Some(_), '\\') => {
                chars.next();
            }
            (Some(quote), c) if c == quote => open = None,
            (None, '"' | '\'') => open = Some(c),
            _ => {}
        }
    }
    open.is_some()
}

/// Whether the lines before `line_start` leave a `MULTI` open: one with no
/// `EXEC` or `DISCARD` after it.
fn in_multi(text: &str, line_start: usize) -> bool {
    let mut open = false;
    for line in text[..line_start].lines() {
        let first = line.split_ascii_whitespace().next().unwrap_or("");
        if first.eq_ignore_ascii_case("MULTI") {
            open = true;
        } else if first.eq_ignore_ascii_case("EXEC") || first.eq_ignore_ascii_case("DISCARD") {
            open = false;
        }
    }
    open
}

/// Whether argument `index` (0 for the first after the command's name) of
/// a command with `syntax` is a key: a `key`, `source` or `destination` in
/// the fixed arguments, or in a repeated group right after them
/// (`key [key ...]`, `key value [key value ...]`).
fn takes_key(syntax: &str, index: usize) -> bool {
    let is_key = |word: &str| matches!(word, "key" | "source" | "destination");
    let words: Vec<&str> = syntax.split_ascii_whitespace().collect();
    let fixed = words.iter().take_while(|w| !w.starts_with('[')).count();
    if index < fixed {
        return is_key(words[index]);
    }
    let rest = &words[fixed..];
    let Some(end) = rest.iter().position(|w| w.trim_end_matches(']') == "...") else {
        return false;
    };
    let group = &rest[..end];
    // Only one plain group: `[NX | XX] [GET]` or nested ones are not.
    if group.is_empty()
        || group.iter().skip(1).any(|w| w.starts_with('['))
        || group.iter().any(|w| w.ends_with(']') || *w == "|")
    {
        return false;
    }
    let word = group[(index - fixed) % group.len()].trim_start_matches('[');
    is_key(word)
}

/// Completes a Redis command line: command names (with their syntax) in
/// the first word, a container's subcommands in the second, and keys where
/// the command takes one. Keys the tree has not listed are looked up by
/// prefix through [`Need::KeysMatching`].
pub fn complete_redis(
    text: &str,
    cursor: usize,
    keys: Option<&Keyspace<'_>>,
    history: &History,
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
    if in_quotes(before) {
        return None;
    }
    let start = line_start
        + before
            .char_indices()
            .rev()
            .find(|(_, c)| !redis_word(*c))
            .map_or(0, |(at, c)| at + c.len_utf8());
    let end = cursor
        + text[cursor..]
            .find(|c: char| !redis_word(c))
            .unwrap_or(text.len() - cursor);
    let prefix = text[start..cursor].to_owned();
    let head = &text[line_start..start];
    let words: Vec<String> = match violet_lang::redis::argv(head) {
        Ok(args) => args
            .iter()
            .map(|arg| String::from_utf8_lossy(arg).into_owned())
            .collect(),
        Err(_) => head.split_ascii_whitespace().map(str::to_owned).collect(),
    };
    let multi = in_multi(text, line_start);
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
                // Inside MULTI another MULTI or a WATCH is an error.
                if multi && matches!(first, "MULTI" | "WATCH") {
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
                if let Some(sub) = c.name.strip_prefix(&head) {
                    items.push(command(sub, c.syntax, format!("{sub} ")));
                }
            }
        }
        [first, ..] => {
            let name = violet_lang::redis::command(&words);
            let skip = if is_container(&first.to_ascii_uppercase()) {
                2
            } else {
                1
            };
            let key_slot = name
                .is_some_and(|c| words.len() >= skip && takes_key(c.syntax, words.len() - skip));
            if key_slot && let Some(keys) = keys {
                key_items(keys, &prefix, &mut items, &mut needs);
            }
        }
    }
    let mut items = rank(items, &prefix, history, false);
    if multi {
        // What closes the block comes first.
        items.sort_by_key(|item| {
            !(item.kind == Kind::Keyword && matches!(item.label.as_str(), "EXEC" | "DISCARD"))
        });
    }
    Some(Completion {
        range: start..end,
        prefix,
        items,
        needs,
    })
}

/// The known keys for a key argument, and what to load for the rest.
fn key_items(keys: &Keyspace<'_>, prefix: &str, items: &mut Vec<Item>, needs: &mut Vec<Need>) {
    let found = keys.found.iter().filter(|(by, _)| prefix.starts_with(by));
    let covered = keys.whole || found.clone().any(|(_, keys)| keys.is_some());
    let mut seen = HashSet::new();
    let lists = keys
        .listed
        .into_iter()
        .chain(found.filter_map(|(_, keys)| *keys));
    for key in lists.flatten() {
        if !seen.insert(key.name.as_str()) {
            continue;
        }
        let insert = violet_lang::redis::quote(key.name.as_bytes());
        items.push(Item {
            label: key.name.clone(),
            caret: insert.len(),
            insert,
            kind: Kind::Key,
            detail: key.subtype.clone().unwrap_or_default(),
            key: key.name.clone(),
        });
    }
    if keys.listed.is_none() {
        needs.push(Need::Keys(keys.schema.to_owned()));
    }
    if !covered && !prefix.is_empty() {
        needs.push(Need::KeysMatching(
            keys.schema.to_owned(),
            prefix.to_owned(),
        ));
    }
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
        offer(
            Kind::Keyword,
            "show collections",
            "",
            "show collections".to_owned(),
        );
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
    let analysis = analyze(text, options.family);
    let (all, separators, significant) =
        (&analysis.all, &analysis.separators, &analysis.significant);
    if inside_literal(text, all, cursor) {
        return None;
    }
    let start = word_start(text, cursor);
    if text[start..].starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let end = word_end(text, cursor);
    let (qualifiers, qualified_start, quoted) = qualifiers(text, start);

    let after = separators
        .iter()
        .rfind(|separator| separator.end <= qualified_start)
        .map_or(0, |separator| separator.end);
    let until = separators
        .iter()
        .find(|separator| separator.start >= cursor)
        .map_or(text.len(), |separator| separator.start);
    let statement_start = significant.partition_point(|token| token.range.start < after);
    let statement_end = significant.partition_point(|token| token.range.end <= until);
    let statement = &significant[statement_start..statement_end.max(statement_start)];
    let before: Vec<&Token> = statement
        .iter()
        .filter(|token| token.range.end <= qualified_start)
        .collect();

    let script = &analysis.script;
    let mut scope = Scope::read(text, statement, qualified_start);
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
        script,
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

/// What reading a text yields: its tokens, where its statements end and what
/// its `CREATE`s make.
struct Analysis {
    text: String,
    family: Family,
    all: Vec<Token>,
    separators: Vec<Range<usize>>,
    /// The tokens that are neither blanks nor comments.
    significant: Vec<Token>,
    script: Script,
}

/// How many `CREATE` statements [`CREATES`] remembers.
const CREATE_MEMORY: usize = 2048;

thread_local! {
    /// The last text analysed: completion runs again for the same text when
    /// the cursor moves or the catalog answers, and reading it is the cost
    /// that grows with the script.
    static ANALYSIS: RefCell<Option<Rc<Analysis>>> = const { RefCell::new(None) };
    /// What each `CREATE` statement reads to, by its source: a script that
    /// changes after its `CREATE`s reads only the statement that changed.
    static CREATES: RefCell<HashMap<String, Rc<Script>>> = RefCell::new(HashMap::new());
}

/// Reads `text`, or hands back the reading of the same text.
fn analyze(text: &str, family: Family) -> Rc<Analysis> {
    if let Some(kept) = ANALYSIS.with(|kept| kept.borrow().clone())
        && kept.family == family
        && kept.text == text
    {
        return kept;
    }
    let (all, separators) = lex(text, family);
    let significant: Vec<Token> = all
        .iter()
        .filter(|token| !matches!(token.kind, TokenKind::Whitespace | TokenKind::Comment))
        .cloned()
        .collect();
    let script = Script::read(text, &significant);
    let analysis = Rc::new(Analysis {
        text: text.to_owned(),
        family,
        all,
        separators,
        significant,
        script,
    });
    ANALYSIS.with(|kept| *kept.borrow_mut() = Some(Rc::clone(&analysis)));
    analysis
}

/// The text's tokens and where its statements end. A `;` inside a string,
/// a comment or a `$$` body ends nothing; MySQL knows no `$$` bodies, and
/// its client ends statements with what `DELIMITER` sets.
fn lex(text: &str, family: Family) -> (Vec<Token>, Vec<Range<usize>>) {
    if family != Family::MySql {
        let tokens: Vec<Token> = sql::tokens(text).collect();
        let separators = tokens
            .iter()
            .filter(|token| is(text, token, ";"))
            .map(|token| token.range.clone())
            .collect();
        return (tokens, separators);
    }
    let separators = delimited(text);
    let mut tokens = Vec::new();
    let mut from = 0;
    for separator in separators
        .iter()
        .cloned()
        .chain(std::iter::once(text.len()..text.len()))
    {
        tokens.extend(
            sql::tokens(&text[from..separator.start]).map(|token| Token {
                kind: token.kind,
                range: token.range.start + from..token.range.end + from,
            }),
        );
        from = separator.end;
    }
    (tokens, separators)
}

/// Where MySQL's client ends statements: each delimiter outside strings and
/// comments, and each `DELIMITER` line, which sets the next one.
fn delimited(text: &str) -> Vec<Range<usize>> {
    let bytes = text.as_bytes();
    let mut separators = Vec::new();
    let mut delimiter = ";".to_owned();
    let mut at = 0;
    let mut line_start = true;
    while at < bytes.len() {
        if line_start {
            let line_end = text[at..].find('\n').map_or(text.len(), |end| at + end);
            let line = text[at..line_end].trim();
            let mut words = line.split_whitespace();
            if words
                .next()
                .is_some_and(|w| w.eq_ignore_ascii_case("DELIMITER"))
                && let Some(next) = words.next()
            {
                delimiter = next.to_owned();
                separators.push(at..line_end);
                at = line_end;
                continue;
            }
        }
        line_start = bytes[at] == b'\n';
        if text[at..].starts_with(delimiter.as_str()) {
            separators.push(at..at + delimiter.len());
            at += delimiter.len();
            continue;
        }
        at = match bytes[at] {
            quote @ (b'\'' | b'"' | b'`') => {
                let mut end = at + 1;
                while end < bytes.len() && bytes[end] != quote {
                    end += if bytes[end] == b'\\' { 2 } else { 1 };
                }
                end + 1
            }
            b'#' => text[at..].find('\n').map_or(text.len(), |end| at + end),
            b'-' if text[at..].starts_with("-- ") => {
                text[at..].find('\n').map_or(text.len(), |end| at + end)
            }
            b'/' if text[at..].starts_with("/*") => text[at + 2..]
                .find("*/")
                .map_or(text.len(), |end| at + end + 4),
            _ => at + text[at..].chars().next().map_or(1, char::len_utf8),
        }
        .min(bytes.len());
    }
    separators
}

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
            let closed = body.len() >= 2
                && matches!(body.as_bytes()[body.len() - 1], b'\'' | b'"' | b'`' | b'$');
            cursor < token.range.end || !closed
        }
        _ => false,
    }
}

/// The names before the word, as in `catalog.schema.table.` + word: at most
/// three, where the first starts, and whether the last is quoted.
fn qualifiers(text: &str, start: usize) -> (Vec<String>, usize, bool) {
    let mut names = Vec::new();
    let mut quoted = false;
    let mut at = start;
    while names.len() < 3 && text[..at].ends_with('.') {
        let dot = at - 1;
        let head = &text[..dot];
        let quote = head.chars().next_back().filter(|c| matches!(c, '"' | '`'));
        let (name, begin) = if let Some(quote) = quote {
            let inner = &head[..head.len() - 1];
            let Some(open) = inner.rfind(quote) else {
                break;
            };
            let doubled = quote.to_string().repeat(2);
            (
                inner[open + 1..].replace(&doubled, &quote.to_string()),
                open,
            )
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

/// The index after `IF [NOT] EXISTS` at `tokens[at]`, as in `CREATE TABLE`,
/// `DROP INDEX` or `ALTER TABLE ... ADD COLUMN`; `at` when it is not there.
fn if_exists(text: &str, tokens: &[Token], at: usize) -> usize {
    if !is_word(text, tokens.get(at), "IF") {
        return at;
    }
    let not = usize::from(is_word(text, tokens.get(at + 1), "NOT"));
    if is_word(text, tokens.get(at + 1 + not), "EXISTS") {
        at + 2 + not
    } else {
        at
    }
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
    /// Inside a subquery in `FROM`, its own tables; a `LATERAL` one also
    /// sees the tables left of it.
    fn read(text: &str, tokens: &[Token], cursor: usize) -> Self {
        let mut relations = relations(text, tokens);
        if let Some((open, close, lateral)) = derived(text, tokens, cursor) {
            let start = tokens[open].range.start;
            relations.retain(|r| lateral && r.at < start);
            relations.extend(self::relations(text, &tokens[open + 1..close]));
        }
        Self {
            relations,
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
            Some("ONLY" | "LATERAL" | "NATURAL") => {}
            // `DELETE FROM a USING b, c`; `JOIN t USING (` lists columns.
            Some("USING") if !tokens.get(index + 1).is_some_and(|t| is(text, t, "(")) => {
                expect = true;
                from = Some(depth);
            }
            Some(
                "WHERE" | "GROUP" | "ORDER" | "HAVING" | "LIMIT" | "SET" | "VALUES" | "ON"
                | "USING" | "SELECT" | "UNION" | "RETURNING" | "WINDOW",
            ) => {
                expect = false;
                if from == Some(depth) {
                    from = None;
                }
            }
            _ if expect && if_exists(text, tokens, index) > index => {
                index = if_exists(text, tokens, index);
                continue;
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

/// The innermost `(SELECT ...)` after `FROM`, `JOIN` or `LATERAL` holding
/// the cursor: its brackets' indices, and whether it is `LATERAL`.
fn derived(text: &str, tokens: &[Token], cursor: usize) -> Option<(usize, usize, bool)> {
    let mut found = None;
    for (open, token) in tokens.iter().enumerate() {
        if !is(text, token, "(") || token.range.end > cursor {
            continue;
        }
        let close = closing(text, tokens, open);
        if tokens.get(close).is_some_and(|t| t.range.start < cursor)
            || !is_word(text, tokens.get(open + 1), "SELECT")
        {
            continue;
        }
        let previous = open.checked_sub(1).and_then(|at| word(text, &tokens[at]));
        if matches!(previous.as_deref(), Some("FROM" | "JOIN" | "LATERAL")) {
            found = Some((open, close, previous.as_deref() == Some("LATERAL")));
        }
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
            // The statement, and the token after it, which a table's end
            // can look at.
            let end = tokens[create..]
                .iter()
                .position(|token| is(text, token, ";"))
                .map_or(tokens.len(), |at| create + at + 1);
            let last = tokens.get(end).unwrap_or(&tokens[end - 1]);
            let source = &text[tokens[create].range.start..last.range.end];
            let kept = CREATES.with(|memory| memory.borrow().get(source).cloned());
            let read = kept.unwrap_or_else(|| {
                let mut read = Self::default();
                read.read_create(text, tokens, create + 1);
                let read = Rc::new(read);
                CREATES.with(|memory| {
                    let mut memory = memory.borrow_mut();
                    if memory.len() >= CREATE_MEMORY {
                        memory.clear();
                    }
                    memory.insert(source.to_owned(), Rc::clone(&read));
                });
                read
            });
            script.created.extend(read.created.iter().cloned());
            script.keys.extend(read.keys.iter().cloned());
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
        let at = if_exists(text, tokens, at + 1);
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
    /// `JOIN t USING (`: the columns `t` shares with the tables before it,
    /// `USING` at this byte offset.
    Using(usize),
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
    // `DROP TABLE IF EXISTS` is still a table slot.
    if is_word(text, Some(last), "EXISTS") {
        let mut at = before.len() - 1;
        if at > 0 && is_word(text, Some(before[at - 1]), "NOT") {
            at -= 1;
        }
        if at > 0 && is_word(text, Some(before[at - 1]), "IF") {
            return context(text, &before[..at - 1], scope);
        }
    }
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
            // A natural join brings its own condition.
            let natural = before[..before.len() - 1]
                .iter()
                .rev()
                .filter_map(|t| word(text, t))
                .find(|w| !matches!(w.as_str(), "LEFT" | "RIGHT" | "FULL" | "INNER" | "OUTER"))
                .is_some_and(|w| w == "NATURAL");
            return Context::Relation {
                join: !natural,
                alias: true,
            };
        }
        Some("USING") if is_word(text, before.first().copied(), "DELETE") => {
            return Context::Relation {
                join: false,
                alias: true,
            };
        }
        Some("ON") => return Context::JoinOn,
        _ => {}
    }
    let (clause, bracket) = clause(text, before);
    if let Some(open) = bracket {
        if open > 0 && is_word(text, Some(before[open - 1]), "USING") {
            return Context::Using(before[open - 1].range.start);
        }
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
            Context::Using(at) => self.shared(*at),
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

    /// Finds a relation in the catalog, as its schema and name: in `schema`,
    /// else the search path, else any schema loaded.
    fn find(&mut self, schema: Option<&str>, name: &str, quoted: bool) -> Option<(String, String)> {
        let schemas: Vec<String> = match schema {
            Some(schema) => self.schema_named(schema).into_iter().collect(),
            None => self.options.search.to_vec(),
        };
        for schema in &schemas {
            if self.objects(schema).is_none() {
                continue;
            }
            if let Some(object) = self.catalog.object(schema, name, quoted, false) {
                return Some((schema.clone(), object.name.clone()));
            }
        }
        if schema.is_some() {
            return None;
        }
        self.catalog.schemas().iter().find_map(|schema| {
            let object = self.catalog.object(schema, name, quoted, true)?;
            Some((schema.clone(), object.name.clone()))
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
            .map(|(schema, name)| Source::Catalog(schema, name))
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
        // The catalog outlives the builder: its lists are read in place, and
        // names that cannot match what is typed are not built at all.
        let (catalog, options, prefix) = (self.catalog, self.options, self.prefix);
        let taken = self.taken();
        for schema in options.search {
            let objects = catalog.objects(schema);
            if objects.is_none() {
                self.needs.push(Need::Objects(schema.clone()));
            }
            for object in objects.unwrap_or_default() {
                if let Some(kind) = relation_kind(object.kind)
                    && score(&object.name, prefix).is_some()
                {
                    let insert = self.ident(&object.name);
                    let (label, insert) =
                        self.aliased_among(&taken, object.name.clone(), insert, &object.name);
                    self.push_keyed(kind, label, insert, schema.clone(), object.name.clone());
                }
            }
        }
        // Other schemas' tables, qualified, once something is typed.
        if !prefix.is_empty() {
            for schema in catalog.schemas() {
                if options.search.contains(schema) {
                    continue;
                }
                for object in catalog.objects(schema).unwrap_or_default() {
                    if let Some(kind) = relation_kind(object.kind)
                        && score(&object.name, prefix).is_some()
                    {
                        let insert = format!("{}.{}", self.ident(schema), self.ident(&object.name));
                        let label = format!("{schema}.{}", object.name);
                        let (label, insert) =
                            self.aliased_among(&taken, label, insert, &object.name);
                        self.push_keyed(kind, label, insert, schema.clone(), object.name.clone());
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
        self.aliased_among(&self.taken(), label, insert, table)
    }

    /// The handles the statement's tables already use, lower-cased.
    fn taken(&self) -> Vec<String> {
        if !self.alias_tables {
            return Vec::new();
        }
        self.scope
            .relations
            .iter()
            .map(|r| r.handle().to_lowercase())
            .collect()
    }

    /// [`Self::aliased`] with `taken` worked out by the caller, once for a
    /// whole catalog's worth of tables.
    fn aliased_among(
        &self,
        taken: &[String],
        label: String,
        insert: String,
        table: &str,
    ) -> (String, String) {
        if !self.alias_tables {
            return (label, insert);
        }
        let alias = short_alias(table, taken);
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
        let current = |name: &str| {
            self.options
                .database
                .is_some_and(|database| database.eq_ignore_ascii_case(name))
        };
        match names {
            // Another database's names are not loaded: nothing beats wrong.
            [catalog, rest @ ..]
                if names.len() == 3
                    || (current(catalog) && self.schema_named(catalog).is_none()) =>
            {
                if current(catalog) {
                    self.qualified(rest, quoted);
                }
            }
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

    /// A relation's column names, when they are known yet.
    fn column_names(&mut self, relation: &Relation) -> Option<Vec<String>> {
        match self.source(relation)? {
            Source::Known(columns) => Some(columns.to_vec()),
            Source::Script(created) => Some(created.columns.clone()),
            Source::Catalog(schema, name) => match self.catalog.columns(&schema, &name) {
                Some(columns) => Some(columns.iter().map(|c| c.name.clone()).collect()),
                None => {
                    self.needs.push(Need::Columns(schema, name));
                    None
                }
            },
        }
    }

    /// `JOIN t USING (`: the columns of `t`, the table before `USING` at
    /// `at`, that a table before it has too.
    fn shared(&mut self, at: usize) {
        let mut before: Vec<Relation> = self
            .scope
            .relations
            .iter()
            .filter(|r| r.at < at)
            .cloned()
            .collect();
        let Some(joined) = before.pop() else {
            return;
        };
        let Some(columns) = self.column_names(&joined) else {
            return;
        };
        let mut left = Vec::new();
        for relation in &before {
            left.extend(self.column_names(relation).unwrap_or_default());
        }
        for column in columns {
            if left.iter().any(|c| c.eq_ignore_ascii_case(&column)) {
                let insert = self.ident(&column);
                let detail = joined.handle().to_owned();
                self.push_keyed(Kind::Column, column.clone(), insert, detail, column);
            }
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
pub fn same(catalog: &str, typed: &str, quoted: bool) -> bool {
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
    // What the comparison reads is worked out once per item, not once per
    // comparison: a large catalog sorts thousands of them.
    let mut scored: Vec<(u8, u64, String, Item)> = items
        .into_iter()
        .filter_map(|item| {
            let score = score(&item.key, prefix)?;
            let recency = history.recency(&item);
            let label = if prefix.is_empty() {
                String::new()
            } else {
                item.label.to_lowercase()
            };
            Some((score, recency, label, item))
        })
        .collect();
    scored.sort_by(|(a, recent_x, label_x, x), (b, recent_y, label_y, y)| {
        let order = a
            .cmp(b)
            .then_with(|| recent_y.cmp(recent_x))
            .then(kind_order(x.kind, relation).cmp(&kind_order(y.kind, relation)));
        if prefix.is_empty() {
            return order;
        }
        order
            .then(x.key.len().cmp(&y.key.len()))
            .then_with(|| label_x.cmp(label_y))
    });
    let mut seen = HashSet::new();
    scored
        .into_iter()
        .map(|(.., item)| item)
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
                database: Some("shop"),
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
                    database: None,
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
                database: Some("shop"),
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

    fn redis_labels(text: &str, keys: Option<&Keyspace<'_>>) -> Vec<String> {
        let cursor = text.find('|').expect("cursor");
        let text = text.replace('|', "");
        complete_redis(&text, cursor, keys, &History::default())
            .expect("completion")
            .items
            .into_iter()
            .map(|i| i.label)
            .collect()
    }

    fn keyspace<'a>(listed: Option<&'a [DbObject]>) -> Keyspace<'a> {
        Keyspace {
            schema: "db0",
            listed,
            whole: true,
            found: &[],
        }
    }

    #[test]
    fn redis_lines_complete_commands_and_keys() {
        let got = redis_labels("hge|", None);
        assert!(
            got.contains(&"HGET".to_owned()) && got.contains(&"HGETALL".to_owned()),
            "{got:?}"
        );
        assert_eq!(redis_labels("CONFIG g|", None)[0], "GET");
        let keys = [
            object("user:1", ObjectKind::Key),
            object("order:9", ObjectKind::Key),
        ];
        let space = keyspace(Some(&keys));
        assert_eq!(redis_labels("GET us|", Some(&space)), ["user:1"]);
        let got = redis_labels("AUTH secret\nMULTI\nHGETALL us|", Some(&space));
        assert_eq!(got, ["user:1"]);
        let waiting = keyspace(None);
        let got = complete_redis("GET us", 6, Some(&waiting), &History::default());
        assert_eq!(got.expect("completion").needs[0], Need::Keys("db0".into()));
    }

    #[test]
    fn redis_results_are_ranked() {
        // Prefix before fuzzy, shorter first among prefixes.
        let got = redis_labels("hget|", None);
        assert_eq!(got[..2], ["HGET", "HGETALL"]);
        // Keys match in any case, and go in as the database has them.
        let keys = [
            object("session:9", ObjectKind::Key),
            object("User:1", ObjectKind::Key),
        ];
        let space = keyspace(Some(&keys));
        let completion =
            complete_redis("GET us", 6, Some(&space), &History::default()).expect("completion");
        assert_eq!(completion.items[0].insert, "User:1");
        // What was accepted lately comes first among equals.
        let mut history = History::default();
        let both = complete_redis("GET ", 4, Some(&space), &history).expect("completion");
        let last = both.items.last().expect("item").clone();
        history.accepted(&last);
        let again = complete_redis("GET ", 4, Some(&space), &history).expect("completion");
        assert_eq!(again.items[0], last);
    }

    #[test]
    fn redis_keys_only_where_a_command_takes_one() {
        let keys = [object("user:1", ObjectKind::Key)];
        let space = keyspace(Some(&keys));
        let has_key = |text: &str| redis_labels(text, Some(&space)).contains(&"user:1".to_owned());
        assert!(has_key("GET |"));
        assert!(!has_key("GET user:1 |"));
        assert!(has_key("SET |"));
        assert!(!has_key("SET user:1 |"));
        assert!(has_key("MSET a 1 |"));
        assert!(!has_key("MSET a 1 b |"));
        assert!(has_key("DEL a b |"));
        assert!(has_key("HGET |"));
        assert!(!has_key("HGET user:1 |"));
        assert!(has_key("OBJECT ENCODING |"));
        assert!(!has_key("SET k v |"));
        // A command Violet does not know takes nothing for granted.
        assert!(!has_key("FROBNICATE |"));
        // A quoted argument with a blank counts once.
        assert!(!has_key("SET \"a b\" |"));
    }

    #[test]
    fn redis_keys_not_listed_are_looked_up() {
        let keys = [object("user:1", ObjectKind::Key)];
        let mut space = keyspace(Some(&keys));
        space.whole = false;
        let needs = |space: &Keyspace<'_>| {
            complete_redis("GET or", 6, Some(space), &History::default())
                .expect("completion")
                .needs
        };
        assert_eq!(
            needs(&space),
            [Need::KeysMatching("db0".into(), "or".into())]
        );
        // Asked for and on its way: still waiting on it.
        let pending = [("or", None)];
        space.found = &pending;
        assert_eq!(needs(&space).len(), 1);
        // Found under a shorter prefix: done, and offered.
        let order = [object("order:9", ObjectKind::Key)];
        let done = [("o", Some(&order[..]))];
        space.found = &done;
        assert!(needs(&space).is_empty());
        let got = redis_labels("GET or|", Some(&space));
        assert_eq!(got, ["order:9"]);
        assert_eq!(key_glob("a*b?[c]\\"), "a\\*b\\?\\[c]\\\\*");
    }

    #[test]
    fn redis_quotes_respect_escapes() {
        assert!(in_quotes("SET k \"open"));
        assert!(!in_quotes("SET k \"a \\\" b\" "));
        assert!(in_quotes("SET k \"a \\\""));
        assert!(!in_quotes("SET k \"it's\" "));
        assert!(!in_quotes("SET k 'say \"hi\"' "));
        assert!(in_quotes("SET k 'it\\'s"));
        assert!(complete_redis("SET k \"open", 11, None, &History::default()).is_none());
        assert!(complete_redis("SET k \"it's\" ", 13, None, &History::default()).is_some());
    }

    #[test]
    fn redis_multi_blocks_offer_their_end_first() {
        let inside = redis_labels("MULTI\nSET a 1\n|", None);
        assert_eq!(inside[..2], ["DISCARD", "EXEC"]);
        assert!(!inside.contains(&"MULTI".to_owned()));
        let after = redis_labels("MULTI\nSET a 1\nEXEC\nmu|", None);
        assert_eq!(after[0], "MULTI");
    }

    #[test]
    fn redis_words_run_through_colons_dashes_and_dots() {
        let keys = [object("user:1:name", ObjectKind::Key)];
        let space = keyspace(Some(&keys));
        let completion = complete_redis(
            "GET user:1:na-x.y rest",
            12,
            Some(&space),
            &History::default(),
        )
        .expect("completion");
        assert_eq!(completion.range, 4..17);
        assert_eq!(completion.prefix, "user:1:n");
        assert!(redis_word(':') && redis_word('-') && redis_word('.'));
        assert!(!redis_word(' ') && !redis_word('"'));
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
        assert_eq!(
            keyword(KeywordCase::Typed, "|"),
            keyword(KeywordCase::Upper, "|")
        );
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

    #[test]
    fn using_natural_lateral_and_delete_using_read_their_tables() {
        let shop = shop();
        let using = at(&shop, "SELECT * FROM customers c JOIN orders o USING (|");
        assert_eq!(inserts(&using), ["id"]);
        let natural = at(&shop, "SELECT * FROM customers c NATURAL LEFT JOIN |");
        assert!(!natural.items.iter().any(|item| item.kind == Kind::Join));
        assert!(inserts(&natural).contains(&"orders o"));
        let lateral = at(
            &shop,
            "SELECT * FROM customers c, LATERAL (SELECT c.| FROM orders o) l",
        );
        assert_eq!(inserts(&lateral), ["id", "name", "country"]);
        let inner = at(
            &shop,
            "SELECT * FROM customers c JOIN LATERAL (SELECT tot| FROM orders o",
        );
        assert_eq!(inserts(&inner)[0], "total");
        let delete = at(&shop, "DELETE FROM orders o USING customers c WHERE c|");
        let found = inserts(&delete);
        assert!(
            found.contains(&"country") && found.contains(&"customer_id"),
            "{found:?}"
        );
        assert!(inserts(&at(&shop, "DELETE FROM orders USING cust|")).contains(&"customers c"));
    }

    #[test]
    fn three_part_names_start_with_the_database() {
        let shop = shop();
        let columns = at(&shop, "SELECT shop.public.customers.| FROM customers");
        assert_eq!(inserts(&columns), ["id", "name", "country"]);
        let quoted = at(&shop, "SELECT \"shop\".\"public\".\"orders\".tot|");
        assert_eq!(inserts(&quoted), ["total"]);
        let tables = at(&shop, "SELECT * FROM shop.sales.|");
        assert_eq!(inserts(&tables), ["targets t"]);
        // Another database: nothing rather than this one's names.
        assert!(
            at(&shop, "SELECT other.public.customers.|")
                .items
                .is_empty()
        );
        assert!(at(&shop, "SELECT * FROM other.public.|").items.is_empty());
    }

    #[test]
    fn if_exists_is_read_where_it_is_written() {
        let shop = shop();
        let script = "CREATE TABLE IF NOT EXISTS notes (id int, body text);\nSELECT bo| FROM notes";
        assert_eq!(inserts(&at(&shop, script))[0], "body");
        let plain = "CREATE TABLE notes (id int, body text);\nSELECT bo| FROM notes";
        assert_eq!(inserts(&at(&shop, plain))[0], "body");
        assert_eq!(
            inserts(&at(&shop, "DROP TABLE IF EXISTS ord|"))[0],
            "orders"
        );
        assert_eq!(
            inserts(&at(&shop, "ALTER TABLE IF EXISTS cust|"))[0],
            "customers"
        );
        let alter = at(
            &shop,
            "ALTER TABLE IF EXISTS orders ADD COLUMN IF NOT EXISTS note text; SELECT tot| FROM orders",
        );
        assert_eq!(inserts(&alter)[0], "total");
    }

    #[test]
    fn statements_end_where_the_database_ends_them() {
        let shop = shop();
        let with = |family: Family, text: &str| {
            let cursor = text.find('|').expect("a cursor");
            let text = text.replacen('|', "", 1);
            let search = vec!["public".to_owned()];
            let quote = |name: &str| format!("`{name}`");
            let history = History::default();
            let options = Options {
                search: &search,
                database: None,
                quote: &quote,
                family,
                case: KeywordCase::default(),
                history: &history,
            };
            let found = complete(&text, cursor, &shop, &options).expect("a completion");
            found
                .items
                .iter()
                .map(|item| item.insert.clone())
                .collect::<Vec<_>>()
        };
        // A `;` inside a function body ends nothing.
        let body = "SELECT tot| FROM orders WHERE id IN (SELECT f()) AND $$ ; $$ = '' AND $fn$ a; b $fn$ = ''";
        assert_eq!(with(Family::Postgres, body)[0], "total");
        let after = "CREATE FUNCTION f() RETURNS int AS $$ SELECT 1; SELECT 2 $$ LANGUAGE sql;\nSELECT na| FROM customers";
        assert_eq!(with(Family::Postgres, after)[0], "name");
        // MySQL ends statements with the delimiter, and `$$` can be one.
        let mysql = "DELIMITER $$\nCREATE PROCEDURE p() BEGIN SELECT 1; END $$\nDELIMITER ;\nSELECT na| FROM customers;";
        assert_eq!(with(Family::MySql, mysql)[0], "name");
        let block = "DELIMITER //\nCREATE PROCEDURE p() BEGIN\n  SELECT 1 FROM orders;\n  SELECT tot| FROM orders o;\nEND //\nDELIMITER ;";
        // The whole block is one statement: both tables are in scope.
        let found = with(Family::MySql, block);
        assert!(found.contains(&"o.total".to_owned()), "{found:?}");
        let next = "DELIMITER //\nSELECT 1 FROM orders //\nSELECT na| FROM customers //";
        assert_eq!(with(Family::MySql, next)[0], "name");
    }

    #[test]
    fn relations_are_looked_up_by_name_in_any_schema() {
        let shop = shop();
        // Unquoted names match in any case, quoted ones exactly.
        let plain = at(&shop, "SELECT tot| FROM ORDERS");
        assert_eq!(inserts(&plain)[0], "total");
        let quoted = at(&shop, "SELECT tot| FROM \"ORDERS\"");
        assert!(!inserts(&quoted).contains(&"total"));
        let exact = at(&shop, "SELECT tot| FROM \"orders\"");
        assert_eq!(inserts(&exact)[0], "total");
    }

    #[test]
    fn the_same_text_is_read_once() {
        let text = "CREATE TABLE notes (id int, body text);\nSELECT 1";
        let first = analyze(text, Family::Other);
        assert!(Rc::ptr_eq(&first, &analyze(text, Family::Other)));
        // Another dialect splits statements another way.
        assert!(!Rc::ptr_eq(&first, &analyze(text, Family::MySql)));
        assert!(!Rc::ptr_eq(&first, &analyze("SELECT 2", Family::Other)));
    }

    #[test]
    fn a_create_that_did_not_change_is_not_read_again() {
        let created = |script: &str| {
            let tokens: Vec<Token> = sql::tokens(script)
                .filter(|t| !matches!(t.kind, TokenKind::Whitespace | TokenKind::Comment))
                .collect();
            Script::read(script, &tokens)
        };
        let kept = || CREATES.with(|memory| memory.borrow().len());
        let one = "CREATE TABLE only_once_here (id int PRIMARY KEY, body text);\n";
        let first = created(&format!("{one}SELECT a"));
        let known = kept();
        // The text after it moved on; the statement's reading did not.
        let next = created(&format!("{one}SELECT abc FROM only"));
        assert_eq!(kept(), known);
        assert_eq!(first.created[0].columns, next.created[0].columns);
        assert_eq!(next.created[0].primary, ["id"]);
        // Editing the statement reads it again.
        let edited = created(&one.replace("body", "note"));
        assert_eq!(kept(), known + 1);
        assert_eq!(edited.created[0].columns, ["id", "note"]);
        // The memory gives the same script as reading fresh.
        let again = created(&format!("{one}{one}"));
        assert_eq!(again.created.len(), 2);
    }

    /// 20 schemas of 500 tables, and a script of 300 `CREATE TABLE`s.
    fn large() -> (Fake, String) {
        let mut fake = Fake::default();
        for s in 0..20 {
            let schema = if s == 0 {
                "public".to_owned()
            } else {
                format!("s{s}")
            };
            let tables = (0..500)
                .map(|t| object(&format!("table_{s}_{t}"), ObjectKind::Table))
                .collect();
            fake.objects.insert(schema.clone(), tables);
            fake.keys.insert(schema.clone(), Vec::new());
            fake.schemas.push(schema);
        }
        let script = (0..300)
            .map(|n| format!("CREATE TABLE made_{n} (id int PRIMARY KEY, name text);\n"))
            .collect::<String>();
        (fake, script)
    }

    #[test]
    fn large_catalogs_and_scripts_complete() {
        let (fake, script) = large();
        let found = at(&fake, &format!("{script}SELECT * FROM table_0_49|"));
        assert!(inserts(&found).iter().any(|i| i.starts_with("table_0_499")));
        let made = at(&fake, &format!("{script}SELECT * FROM made_29|"));
        assert!(inserts(&made)[0].starts_with("made_29"));
    }

    /// `cargo test --release large_catalog_timing -- --ignored --nocapture`
    #[test]
    #[ignore = "timing"]
    fn large_catalog_timing() {
        let (fake, script) = large();
        let runs = 50;
        let time = |name: &str, catalog: &Fake, text: String| {
            let start = std::time::Instant::now();
            for _ in 0..runs {
                std::hint::black_box(at(catalog, &text));
            }
            println!("{name}: {:?} per call", start.elapsed() / runs);
        };
        time("complete", &fake, format!("{script}SELECT * FROM tab|"));
        time(
            "script only",
            &shop(),
            format!("{script}SELECT * FROM tab|"),
        );
        time("catalog only", &fake, "SELECT * FROM tab|".to_owned());
    }
}
