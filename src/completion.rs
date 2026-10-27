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

use crate::diagnostics::{Diagnostic, Severity};
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
    /// Whether the catalog, or a key-value database's `keys`, now answers
    /// some of what [`Self::needs`] lists, so the list is worth making again.
    #[must_use]
    pub fn needs_answered(&self, catalog: &dyn Catalog, keys: Option<&Keyspace<'_>>) -> bool {
        self.needs.iter().any(|need| match need {
            Need::Objects(schema) => catalog.objects(schema).is_some(),
            Need::Columns(schema, relation) => catalog.columns(schema, relation).is_some(),
            Need::ForeignKeys(schema) => catalog.foreign_keys(schema).is_some(),
            Need::Keys(_) => keys.is_some_and(|keys| keys.listed.is_some()),
            Need::KeysMatching(_, prefix) => keys.is_some_and(|keys| keys.covers(prefix)),
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
    /// How many bytes from the caret are selected, a placeholder to type
    /// over; zero for a bare caret.
    pub caret_len: usize,
    /// Where Tab moves on to after the caret, as byte ranges of `insert`:
    /// a placeholder to type over, or an empty range for a bare caret.
    pub stops: Vec<Range<usize>>,
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
    /// By kind, then lower-cased key, so a lookup allocates nothing.
    used: HashMap<Kind, HashMap<String, u64>>,
}

impl History {
    /// Remembers `item` as just accepted.
    pub fn accepted(&mut self, item: &Item) {
        self.clock += 1;
        self.used
            .entry(item.kind)
            .or_default()
            .insert(item.key.to_lowercase(), self.clock);
        if self.used.values().map(HashMap::len).sum::<usize>() > HISTORY_SIZE
            && let Some((kind, oldest)) = self
                .used
                .iter()
                .flat_map(|(kind, keys)| keys.iter().map(move |(key, at)| (*kind, key, *at)))
                .min_by_key(|(.., at)| *at)
                .map(|(kind, key, _)| (kind, key.clone()))
            && let Some(keys) = self.used.get_mut(&kind)
        {
            keys.remove(&oldest);
        }
    }

    /// When an item of `kind` called `key` was last accepted; 0 when never.
    fn recency(&self, kind: Kind, key: &str) -> u64 {
        if self.used.is_empty() {
            return 0;
        }
        let Some(keys) = self.used.get(&kind) else {
            return 0;
        };
        let at = if key.bytes().any(|b| b.is_ascii_uppercase()) || !key.is_ascii() {
            keys.get(&key.to_lowercase())
        } else {
            keys.get(key)
        };
        at.copied().unwrap_or(0)
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

impl Keyspace<'_> {
    /// Whether the keys starting with `prefix` are all known: the tree
    /// listed every key, or a lookup by a prefix of it answered.
    fn covers(&self, prefix: &str) -> bool {
        self.whole
            || self
                .found
                .iter()
                .any(|(by, keys)| prefix.starts_with(by) && keys.is_some())
    }
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
        caret_len: 0,
        stops: Vec::new(),
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
    let covered = keys.covers(prefix);
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
            caret_len: 0,
            stops: Vec::new(),
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

/// The arguments and what each shell method does, by method name.
const MONGO_SYNTAX: &[(&str, &str, &str)] = &[
    (
        "aggregate",
        "(pipeline, options)",
        "Runs an aggregation pipeline",
    ),
    (
        "batchSize",
        "(size)",
        "Sets how many documents each batch returns",
    ),
    (
        "bulkWrite",
        "(operations, options)",
        "Runs several writes in one request",
    ),
    (
        "collation",
        "(options)",
        "Sets the language rules for comparing strings",
    ),
    ("count", "()", "Counts the documents the cursor reaches"),
    (
        "countDocuments",
        "(filter, options)",
        "Counts the documents that match",
    ),
    (
        "createCollection",
        "(name, options)",
        "Creates a collection",
    ),
    ("createIndex", "(keys, options)", "Builds an index"),
    (
        "deleteMany",
        "(filter)",
        "Deletes every document that matches",
    ),
    (
        "deleteOne",
        "(filter)",
        "Deletes the first document that matches",
    ),
    (
        "distinct",
        "(field, filter)",
        "Lists the distinct values of a field",
    ),
    ("drop", "()", "Drops the collection"),
    ("dropDatabase", "()", "Drops the database"),
    ("dropIndex", "(index)", "Drops an index, by name or keys"),
    (
        "estimatedDocumentCount",
        "()",
        "Counts the documents from the collection's metadata",
    ),
    ("explain", "(verbosity)", "Shows how the query runs"),
    (
        "find",
        "(filter, projection)",
        "Returns a cursor over the documents that match",
    ),
    (
        "findOne",
        "(filter, projection)",
        "Returns the first document that matches",
    ),
    (
        "findOneAndDelete",
        "(filter, options)",
        "Deletes the first match and returns it",
    ),
    (
        "findOneAndReplace",
        "(filter, replacement, options)",
        "Replaces the first match and returns it",
    ),
    (
        "findOneAndUpdate",
        "(filter, update, options)",
        "Updates the first match and returns it",
    ),
    (
        "forEach",
        "(function)",
        "Calls a function for each document",
    ),
    ("getCollection", "(name)", "Gets a collection by name"),
    (
        "getCollectionNames",
        "()",
        "Lists the collections of the database",
    ),
    ("getIndexes", "()", "Lists the indexes of the collection"),
    ("hint", "(index)", "Forces the index the query uses"),
    (
        "insertMany",
        "(documents, options)",
        "Inserts several documents",
    ),
    ("insertOne", "(document, options)", "Inserts a document"),
    (
        "limit",
        "(count)",
        "Caps how many documents the cursor returns",
    ),
    ("map", "(function)", "Maps each document through a function"),
    ("pretty", "()", "Prints the results formatted"),
    (
        "project",
        "(projection)",
        "Picks the fields the documents keep",
    ),
    (
        "replaceOne",
        "(filter, replacement, options)",
        "Replaces the first document that matches",
    ),
    ("runCommand", "(command)", "Runs a database command"),
    ("skip", "(count)", "Skips that many documents"),
    (
        "sort",
        "(order)",
        "Orders the documents: 1 ascending, -1 descending",
    ),
    ("stats", "()", "Shows statistics of the database"),
    ("toArray", "()", "Reads every document into an array"),
    (
        "updateMany",
        "(filter, update, options)",
        "Updates every document that matches",
    ),
    (
        "updateOne",
        "(filter, update, options)",
        "Updates the first document that matches",
    ),
];

/// The syntax of the shell method named at byte `offset`: after a `.`,
/// on `db`, on a collection, or on a cursor.
pub fn mongo_hover(text: &str, offset: usize) -> Option<Hover> {
    if offset > text.len() || !text.is_char_boundary(offset) {
        return None;
    }
    let (start, end) = (word_start(text, offset), word_end(text, offset));
    let method = &text[start..end];
    let owner = text[..start].strip_suffix('.')?;
    let open = text[end..].trim_start_matches([' ', '\t']).starts_with('(');
    if method.is_empty() || !open {
        return None;
    }
    let name = &owner[word_start(owner, owner.len())..];
    let parent = owner[..owner.len() - name.len()].strip_suffix('.');
    // The lists the owner offers; a call's result is a cursor, or a
    // collection after `getCollection(...)`.
    let lists: &[&[&str]] = if owner.ends_with(')') {
        &[MONGO_CURSOR, MONGO_COLLECTION]
    } else if name == "db" && parent.is_none() {
        &[MONGO_DB]
    } else if parent.is_some_and(|parent| parent.ends_with("db")) {
        &[MONGO_COLLECTION]
    } else {
        return None;
    };
    let list = lists.iter().find(|list| list.contains(&method))?;
    let (_, arguments, detail) = MONGO_SYNTAX.iter().find(|(known, ..)| *known == method)?;
    let scope = if list.as_ptr() == MONGO_DB.as_ptr() {
        "db."
    } else if list.as_ptr() == MONGO_COLLECTION.as_ptr() {
        "db.collection."
    } else {
        "cursor."
    };
    Some(Hover {
        range: start..end,
        title: format!("{scope}{method}{arguments}"),
        detail: (*detail).to_owned(),
    })
}

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
            caret_len: 0,
            stops: Vec::new(),
            kind,
            detail: detail.to_owned(),
            key: name.to_owned(),
        });
    };
    let head = text[..start].trim_end_matches([' ', '\t']);
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

    let statement = statement_at(significant, separators, qualified_start, cursor);
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
        pending: Vec::new(),
        taken: Vec::new(),
        needs: Vec::new(),
        prefix: &text[start..cursor],
    };
    builder.fill(&context);
    let prefix = text[start..cursor].to_owned();
    let relation = matches!(context, Context::Relation { .. });
    let mut items = builder.ranked(relation);
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

/// The tokens of the statement that holds `from..until`.
fn statement_at<'t>(
    tokens: &'t [Token],
    separators: &[Range<usize>],
    from: usize,
    until: usize,
) -> &'t [Token] {
    let after = separators
        .iter()
        .rfind(|separator| separator.end <= from)
        .map_or(0, |separator| separator.end);
    let until = separators
        .iter()
        .find(|separator| separator.start >= until)
        .map_or(usize::MAX, |separator| separator.start);
    let first = tokens.partition_point(|token| token.range.start < after);
    let last = tokens.partition_point(|token| token.range.end <= until);
    &tokens[first..last.max(first)]
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
        // The list of columns is done: `VALUES` is next.
        Some(')') => matches!(
            context(text, &before, &Scope::default()),
            Context::InsertValues(_)
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
pub(crate) fn lex(text: &str, family: Family) -> (Vec<Token>, Vec<Range<usize>>) {
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
            // The comma itself is no table: the one after it is.
            expect = from == Some(depth);
            index += 1;
            continue;
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
    /// `INSERT INTO table `: the next clause, and a template to fill in.
    InsertTable(String),
    /// `INSERT INTO table (a, b) `: the next clause, and `VALUES` with a
    /// place for each column typed.
    InsertValues(usize),
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

/// How many columns the list `INSERT INTO [schema.]table (a, b)` ending
/// `before` names; `None` when the bracket is not right after the table.
fn typed_columns(text: &str, before: &[&Token]) -> Option<usize> {
    let mut depth = 0usize;
    let open = before.iter().rposition(|token| {
        if is(text, token, ")") {
            depth += 1;
        } else if is(text, token, "(") {
            depth -= 1;
        }
        depth == 0
    })?;
    // The table, maybe with its schema, then `INTO`.
    let mut at = open;
    while at > 0 && name(text, before[at - 1]).is_some() {
        at -= 1;
        if at > 0 && is(text, before[at - 1], ".") {
            at -= 1;
        } else {
            break;
        }
    }
    if at == open || at == 0 || !is_word(text, Some(before[at - 1]), "INTO") {
        return None;
    }
    let inner: Vec<Token> = before[open + 1..before.len() - 1]
        .iter()
        .map(|token| (*token).clone())
        .collect();
    let count = split_commas(text, &inner).len();
    (count > 0).then_some(count)
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
        if clause.as_deref() == Some("INTO")
            && is_word(text, before.first().copied(), "INSERT")
            && let Some(table) = name(text, last)
        {
            return Context::InsertTable(table);
        }
        if is(text, last, ")")
            && clause.as_deref() == Some("INTO")
            && is_word(text, before.first().copied(), "INSERT")
            && let Some(columns) = typed_columns(text, &before)
        {
            return Context::InsertValues(columns);
        }
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

/// What lies under the pointer, for a tooltip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hover {
    /// The word it is about.
    pub range: Range<usize>,
    pub title: String,
    pub detail: String,
}

/// What the SQL word at byte `offset` names, from what the catalog has
/// loaded: a table or view with its columns, a column with its type, a
/// function. Never asks the catalog for more.
pub fn hover(
    text: &str,
    offset: usize,
    catalog: &dyn Catalog,
    options: &Options<'_>,
) -> Option<Hover> {
    if offset > text.len() || !text.is_char_boundary(offset) {
        return None;
    }
    let analysis = analyze(text, options.family);
    if inside_literal(text, &analysis.all, offset) {
        return None;
    }
    let (start, end) = (word_start(text, offset), word_end(text, offset));
    let word = &text[start..end];
    if word.is_empty() || word.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let (qualifiers, qualified_start, quoted) = qualifiers(text, start);
    let statement = statement_at(
        &analysis.significant,
        &analysis.separators,
        qualified_start,
        end,
    );
    let script = &analysis.script;
    let mut scope = Scope::read(text, statement, qualified_start);
    // The word itself is no table of the statement it is typed in.
    scope.relations.retain(|r| r.at != start);
    let mut builder = Builder {
        catalog,
        options,
        script,
        scope: &scope,
        cursor: qualified_start,
        alias_tables: false,
        items: Vec::new(),
        pending: Vec::new(),
        taken: Vec::new(),
        needs: Vec::new(),
        prefix: "",
    };
    let (title, detail) = builder.describe(word, &qualifiers, quoted)?;
    Some(Hover {
        range: start..end,
        title,
        detail,
    })
}

impl Builder<'_> {
    /// The title and detail of what `word` names, after `qualifiers`.
    fn describe(
        &mut self,
        word: &str,
        qualifiers: &[String],
        quoted: bool,
    ) -> Option<(String, String)> {
        let scope = self.scope;
        let Some(owner) = qualifiers.last() else {
            if let Some(relation) = scope.by_handle(word) {
                let relation = relation.clone();
                return Some(self.relation_hover(&relation));
            }
            for relation in &scope.relations {
                if let Some(found) = self.column_hover(relation, word) {
                    return Some(found);
                }
            }
            if let Some((schema, name)) = self.find(None, word, quoted)
                && let Some(object) = self
                    .catalog
                    .objects(&schema)
                    .and_then(|objects| objects.iter().find(|o| o.name == name))
                    .cloned()
            {
                return Some(self.object_hover(&schema, &object));
            }
            let function = self
                .options
                .family
                .functions()
                .into_iter()
                .find(|f| f.eq_ignore_ascii_case(word))?;
            return Some((format!("{function}()"), "built-in function".to_owned()));
        };
        if let Some(relation) = scope.by_handle(owner) {
            return self.column_hover(relation, word);
        }
        let schema = self.schema_named(owner)?;
        let object = self
            .objects(&schema)?
            .iter()
            .find(|o| same(&o.name, word, quoted))?
            .clone();
        Some(self.object_hover(&schema, &object))
    }

    fn object_hover(&mut self, schema: &str, object: &DbObject) -> (String, String) {
        let title = format!("{schema}.{}", object.name);
        let mut detail = object_label(object.kind);
        if object.kind.is_relation()
            && let Some(columns) = self.catalog.columns(schema, &object.name)
        {
            detail.push_str(&columns_summary(columns));
        }
        (title, detail)
    }

    fn relation_hover(&mut self, relation: &Relation) -> (String, String) {
        if let Some((schema, name)) = self.resolved(relation)
            && let Some(object) = self
                .catalog
                .objects(&schema)
                .and_then(|objects| objects.iter().find(|o| o.name == name))
        {
            return self.object_hover(&schema, &object.clone());
        }
        let detail = if self.scope.cte(&relation.name).is_some() {
            "CTE"
        } else if relation.columns.is_some() {
            "subquery"
        } else if self
            .created(relation.schema.as_deref(), &relation.name)
            .is_some()
        {
            "defined in this script"
        } else {
            "table"
        };
        (relation.name.clone(), detail.to_owned())
    }

    /// `column` of `relation`, when it has one so named.
    fn column_hover(&mut self, relation: &Relation, column: &str) -> Option<(String, String)> {
        let title = format!("{}.{column}", relation.handle());
        if let Some((schema, name)) = self.resolved(relation) {
            let info = self
                .catalog
                .columns(&schema, &name)?
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(column))?;
            let mut detail = vec![info.data_type.clone()];
            detail.push(
                if info.nullable {
                    "nullable"
                } else {
                    "not null"
                }
                .to_owned(),
            );
            if info.primary_key {
                detail.push("primary key".to_owned());
            }
            if let Some(default) = &info.default {
                detail.push(format!("default {default}"));
            }
            if let Some(comment) = &info.comment {
                detail.push(comment.clone());
            }
            return Some((title, detail.join(" · ")));
        }
        let names = self.column_names(relation)?;
        names
            .iter()
            .any(|name| name.eq_ignore_ascii_case(column))
            .then(|| (title, format!("column of {}", relation.name)))
    }
}

/// ` · 3 columns: a, b, c`, the list cut short past a few.
fn columns_summary(columns: &[ColumnInfo]) -> String {
    const SHOWN: usize = 8;
    let names: Vec<&str> = columns
        .iter()
        .take(SHOWN)
        .map(|c| c.name.as_str())
        .collect();
    let more = columns.len().saturating_sub(SHOWN);
    format!(
        " · {} columns: {}{}",
        columns.len(),
        names.join(", "),
        if more > 0 { ", …" } else { "" }
    )
}

/// A Redis command's syntax with the argument being typed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    /// The command and its arguments, as the documentation writes them.
    pub label: String,
    /// The argument the cursor is in, as a byte range of `label`; none
    /// past the last fixed argument of a command that repeats none.
    pub active: Option<Range<usize>>,
}

/// The syntax of the Redis command the cursor's line holds, once its name
/// is complete (a space after it), and which argument the cursor is in.
pub fn redis_signature(text: &str, cursor: usize) -> Option<Signature> {
    use violet_lang::redis::{command, is_container};
    if cursor > text.len() || !text.is_char_boundary(cursor) {
        return None;
    }
    let line_start = text[..cursor].rfind('\n').map_or(0, |at| at + 1);
    let head = &text[line_start..cursor];
    let words: Vec<String> = match violet_lang::redis::argv(head) {
        Ok(args) => args
            .iter()
            .map(|arg| String::from_utf8_lossy(arg).into_owned())
            .collect(),
        Err(_) => head.split_ascii_whitespace().map(str::to_owned).collect(),
    };
    // The name is done once a blank follows it (both words, for a container).
    let typing = !head.ends_with(char::is_whitespace);
    let completed = words.len() - usize::from(typing && !words.is_empty());
    let skip = words.first().map_or(1, |w| {
        if is_container(&w.to_ascii_uppercase()) {
            2
        } else {
            1
        }
    });
    if completed < skip {
        return None;
    }
    let found = command(&words)?;
    let label = format!("{} {}", found.name, found.syntax)
        .trim_end()
        .to_owned();
    // The argument the cursor is in, or at the start of: 0 for the first
    // after the name.
    let index = completed - skip;
    let tokens: Vec<Range<usize>> = {
        let mut at = found.name.len();
        label[at..]
            .split(' ')
            .filter_map(|part| {
                let start = label[at..].find(part).map(|found| at + found);
                let start = start?;
                at = start + part.len();
                (!part.is_empty()).then_some(start..at)
            })
            .collect()
    };
    // Past the fixed arguments, the repeated group `[... ...]` is what is
    // being filled; keep it simple and mark the last token that is not `...`.
    let active = tokens.get(index).cloned().or_else(|| {
        tokens
            .iter()
            .rev()
            .find(|range| !label[range.start..range.end].contains("..."))
            .cloned()
    });
    Some(Signature { label, active })
}

/// The syntax of the built-in function whose parentheses hold `cursor`,
/// the argument being written marked. `None` outside a call, in a
/// string or a comment, and for functions with no known signature.
pub fn sql_signature(text: &str, cursor: usize, family: Family) -> Option<Signature> {
    if cursor > text.len() || !text.is_char_boundary(cursor) {
        return None;
    }
    let analysis = analyze(text, family);
    if inside_literal(text, &analysis.all, cursor) {
        return None;
    }
    let before = analysis
        .significant
        .partition_point(|token| token.range.end <= cursor);
    let (name, commas) = open_call(text, &analysis.significant[..before])?;
    let signature = family.signature(&name)?;
    let arguments: Vec<&str> = if signature.is_empty() {
        Vec::new()
    } else {
        signature.split(", ").collect()
    };
    let label = format!("{name}({signature})");
    // A call may pass more arguments than the list names: the last repeats.
    let active = arguments
        .get(commas.min(arguments.len().saturating_sub(1)))
        .map(|argument| {
            let start = name.len()
                + 1
                + arguments
                    .iter()
                    .take(commas.min(arguments.len() - 1))
                    .map(|argument| argument.len() + 2)
                    .sum::<usize>();
            start..start + argument.len()
        });
    Some(Signature { label, active })
}

/// The name of the call `tokens` ends inside, and how many arguments it
/// has had before the one being written.
fn open_call(text: &str, tokens: &[Token]) -> Option<(String, usize)> {
    // One entry per parenthesis left open: the word before it, and the
    // commas seen at its level.
    let mut open: Vec<(Option<&str>, usize)> = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        if is(text, token, ";") {
            open.clear();
        } else if is(text, token, "(") {
            let caller = index
                .checked_sub(1)
                .map(|before| &tokens[before])
                .filter(|before| matches!(before.kind, TokenKind::Keyword | TokenKind::Identifier))
                .map(|before| &text[before.range.clone()]);
            open.push((caller, 0));
        } else if is(text, token, ")") {
            open.pop();
        } else if is(text, token, ",")
            && let Some(top) = open.last_mut()
        {
            top.1 += 1;
        }
    }
    let (caller, commas) = open.pop()?;
    Some((caller?.to_owned(), commas))
}

/// The Redis command syntax for the command name at byte `offset`, when
/// that is the first word of its line (or a container's second).
pub fn redis_hover(text: &str, offset: usize) -> Option<Hover> {
    use violet_lang::redis::{command, is_container};
    if offset > text.len() || !text.is_char_boundary(offset) {
        return None;
    }
    let line_start = text[..offset].rfind('\n').map_or(0, |at| at + 1);
    let line_end = text[offset..]
        .find('\n')
        .map_or(text.len(), |at| offset + at);
    let line = &text[line_start..line_end];
    let blank = |c: char| c.is_ascii_whitespace();
    let words: Vec<(usize, &str)> = line
        .split(blank)
        .scan(0, |at, part| {
            let start = *at;
            *at += part.len() + 1;
            Some((start, part))
        })
        .filter(|(_, part)| !part.is_empty())
        .collect();
    let at = offset - line_start;
    let hit = words
        .iter()
        .position(|(start, part)| (*start..=*start + part.len()).contains(&at))?;
    let names: Vec<&str> = words.iter().take(2).map(|(_, part)| *part).collect();
    let container = is_container(&names.first()?.to_ascii_uppercase());
    // The command's own words: one, or two for a container.
    if hit > usize::from(container) {
        return None;
    }
    let found = command(&names)?;
    let (start, part) = words[hit];
    Some(Hover {
        range: line_start + start..line_start + start + part.len(),
        title: found.name.to_owned(),
        detail: found.syntax.to_owned(),
    })
}

// ---- names the catalog does not know ---------------------------------------

/// Names that tables and columns the statements read the loaded catalog
/// says are missing from it: a table or view that is in no schema it could
/// be in, and `alias.column` where the table's columns are loaded and lack
/// it. Never says so of what it cannot place: CTEs, subqueries, tables the
/// script creates, schemas or columns not loaded, functions in `FROM`.
pub fn unknown_names(text: &str, catalog: &dyn Catalog, options: &Options<'_>) -> Vec<Diagnostic> {
    /// Columns every row has without the catalog listing them.
    const SYSTEM_COLUMNS: &[&str] = &[
        "ctid", "oid", "tableoid", "xmin", "xmax", "cmin", "cmax", "rowid", "_rowid_",
    ];
    let analysis = analyze(text, options.family);
    let tokens = &analysis.significant;
    let script = &analysis.script;
    // `SET search_path` points at schemas the editor does not know.
    let moved = tokens
        .iter()
        .any(|token| is_word(text, Some(token), "SEARCH_PATH"));
    let mut found = Vec::new();
    let mut from = 0;
    let ends = analysis
        .separators
        .iter()
        .map(|separator| separator.start)
        .chain(std::iter::once(text.len()));
    for end in ends {
        let first = tokens.partition_point(|token| token.range.start < from);
        let last = tokens.partition_point(|token| token.range.end <= end);
        from = end;
        let statement = &tokens[first..last.max(first)];
        let reads = statement.first().and_then(|token| word(text, token));
        if !reads.is_some_and(|w| {
            matches!(
                w.as_str(),
                "SELECT" | "WITH" | "INSERT" | "UPDATE" | "DELETE" | "REPLACE"
            )
        }) {
            continue;
        }
        let inserting = statement
            .iter()
            .any(|token| matches!(word(text, token).as_deref(), Some("INSERT" | "REPLACE")));
        let scope = Scope {
            relations: relations(text, statement),
            ctes: ctes(text, statement),
        };
        // Where each table is named, so its parts are not read as columns.
        let mut named: Vec<Range<usize>> = Vec::new();
        for relation in scope.relations.iter().filter(|r| r.columns.is_none()) {
            let Some(index) = statement.iter().position(|t| t.range.start == relation.at) else {
                continue;
            };
            let Some((parts, next)) = dotted(text, statement, index) else {
                continue;
            };
            let span = relation.at..statement[next - 1].range.end;
            named.push(span.clone());
            let before = index
                .checked_sub(1)
                .and_then(|at| word(text, &statement[at]));
            let called = statement.get(next).is_some_and(|t| is(text, t, "("))
                && !matches!(before.as_deref(), Some("INTO" | "TABLE"));
            // `SELECT ... INTO t` makes the table.
            let makes = before.as_deref() == Some("INTO") && !inserting;
            let known = scope.cte(&relation.name).is_some() && relation.schema.is_none()
                || created_in(script, relation.schema.as_deref(), &relation.name).is_some();
            if called || makes || known || parts.len() > 2 || moved && relation.schema.is_none() {
                continue;
            }
            if table_missing(catalog, options, relation) {
                found.push(Diagnostic {
                    range: span,
                    message: format!("{} is not in the catalog", parts.join(".")),
                    severity: Severity::Warning,
                });
            }
        }

        // `handle.column` against the columns of the table the handle names.
        for (index, token) in statement.iter().enumerate() {
            let [dot, column, after] = [1, 2, 3].map(|at| statement.get(index + at));
            let (Some(dot), Some(column)) = (dot, column) else {
                continue;
            };
            let after_dot = index
                .checked_sub(1)
                .is_some_and(|at| is(text, &statement[at], "."));
            if !is(text, dot, ".")
                || after_dot
                || after.is_some_and(|t| is(text, t, ".") || is(text, t, "("))
                || named.iter().any(|span| span.contains(&token.range.start))
            {
                continue;
            }
            let (Some(handle), Some(typed)) = (alias(text, Some(token)), alias(text, Some(column)))
            else {
                continue;
            };
            let quoted = column.kind == TokenKind::QuotedIdentifier;
            let same_handle = |r: &&Relation| r.handle().eq_ignore_ascii_case(&handle);
            let mut matching = scope.relations.iter().filter(same_handle);
            let (Some(relation), None) = (matching.next(), matching.next()) else {
                continue;
            };
            if relation.columns.is_some()
                || scope.cte(&relation.name).is_some()
                || created_in(script, relation.schema.as_deref(), &relation.name).is_some()
                || SYSTEM_COLUMNS.contains(&typed.to_ascii_lowercase().as_str())
            {
                continue;
            }
            let Some((schema, table)) = table_found(catalog, options, relation) else {
                continue;
            };
            let Some(columns) = catalog.columns(&schema, &table) else {
                continue;
            };
            if !columns.iter().any(|c| same(&c.name, &typed, quoted)) {
                found.push(Diagnostic {
                    range: column.range.clone(),
                    message: format!("{table} has no column {typed}"),
                    severity: Severity::Warning,
                });
            }
        }
    }
    found
}

/// Where the catalog has `relation`, as its schema and name, when it is
/// loaded far enough to say.
fn table_found(
    catalog: &dyn Catalog,
    options: &Options<'_>,
    relation: &Relation,
) -> Option<(String, String)> {
    let schemas: Vec<&String> = match &relation.schema {
        Some(schema) => catalog
            .schemas()
            .iter()
            .filter(|s| s.eq_ignore_ascii_case(schema))
            .collect(),
        None => options.search.iter().chain(catalog.schemas()).collect(),
    };
    schemas.into_iter().find_map(|schema| {
        let object = catalog.object(schema, &relation.name, relation.quoted, true)?;
        Some((schema.clone(), object.name.clone()))
    })
}

/// Whether the catalog, loaded where `relation` could be, lacks it.
fn table_missing(catalog: &dyn Catalog, options: &Options<'_>, relation: &Relation) -> bool {
    let lower = relation.name.to_ascii_lowercase();
    // The server's own tables are in schemas the editor may not list.
    if lower.starts_with("pg_") || lower.starts_with("sqlite_") || lower == "dual" {
        return false;
    }
    let schemas: Vec<&String> = match &relation.schema {
        Some(schema) => {
            let Some(schema) = catalog
                .schemas()
                .iter()
                .find(|s| s.eq_ignore_ascii_case(schema))
            else {
                return false;
            };
            vec![schema]
        }
        None if options.search.is_empty() => return false,
        None => options.search.iter().collect(),
    };
    schemas
        .iter()
        .all(|schema| catalog.objects(schema).is_some())
        && table_found(catalog, options, relation).is_none()
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
    /// Catalog tables and views that match, built into items only if they
    /// rank among the first [`MAX_ITEMS`].
    pending: Vec<Pending<'a>>,
    /// The aliases [`Self::build`] avoids.
    taken: Vec<String>,
    needs: Vec<Need>,
    prefix: &'a str,
}

/// A catalog table or view that matches what was typed, not built yet.
struct Pending<'a> {
    kind: Kind,
    schema: &'a str,
    name: &'a str,
    /// Listed as `schema.name`.
    qualified: bool,
    score: u8,
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
            Context::InsertTable(table) => {
                self.keywords(&self.options.family.after_relation());
                self.insert_template(table);
            }
            Context::InsertValues(count) => {
                self.keywords(&self.options.family.after_relation());
                self.values_snippet(*count);
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
        self.items
            .push(Self::item(kind, label, insert, detail, key));
    }

    fn item(kind: Kind, label: String, insert: String, detail: String, key: String) -> Item {
        let caret = if insert.ends_with("()") {
            insert.len() - 1
        } else {
            insert.len()
        };
        Item {
            label,
            insert,
            caret,
            caret_len: 0,
            stops: Vec::new(),
            kind,
            detail,
            key,
        }
    }

    /// Builds a pending table or view into its item.
    fn build(&self, pending: &Pending<'_>) -> Item {
        let (label, insert) = if pending.qualified {
            (
                format!("{}.{}", pending.schema, pending.name),
                format!(
                    "{}.{}",
                    self.ident(pending.schema),
                    self.ident(pending.name)
                ),
            )
        } else {
            (pending.name.to_owned(), self.ident(pending.name))
        };
        let (label, insert) = self.aliased_among(&self.taken, label, insert, pending.name);
        Self::item(
            pending.kind,
            label,
            insert,
            pending.schema.to_owned(),
            pending.name.to_owned(),
        )
    }

    /// The items, best first: what was built, and the pending ones that rank
    /// high enough to be built at all.
    fn ranked(&mut self, relation: bool) -> Vec<Item> {
        let items = std::mem::take(&mut self.items);
        let pending = std::mem::take(&mut self.pending);
        rank_pending(
            items,
            pending,
            &|pending| self.build(pending),
            self.prefix,
            self.options.history,
            relation,
        )
    }

    /// `(columns) VALUES ()` for `table`, with a stop inside each value.
    fn insert_template(&mut self, table: &str) {
        let relation = self
            .scope
            .relations
            .iter()
            .find(|r| r.name.eq_ignore_ascii_case(table))
            .cloned()
            .unwrap_or_else(|| Relation {
                name: table.to_owned(),
                ..Relation::default()
            });
        let Some(columns) = self.column_names(&relation) else {
            return;
        };
        let (columns, stops, insert) = values_template(
            &columns.iter().map(|c| self.ident(c)).collect::<Vec<_>>(),
            &self.cased("VALUES"),
        );
        let Some(&caret) = stops.first() else {
            return;
        };
        let label = format!("({columns}) VALUES");
        self.push(Kind::Keyword, label, insert, "template".to_owned());
        if let Some(item) = self.items.last_mut() {
            item.caret = caret;
            item.stops = stops[1..].iter().map(|stop| *stop..*stop).collect();
            item.key = "VALUES".to_owned();
        }
    }

    /// `VALUES (, )` with a stop inside each of `count` values.
    fn values_snippet(&mut self, count: usize) {
        let (stops, insert) = values_tail(count, &self.cased("VALUES"));
        let Some(&caret) = stops.first() else {
            return;
        };
        self.push(
            Kind::Keyword,
            "VALUES".to_owned(),
            insert,
            "template".to_owned(),
        );
        if let Some(item) = self.items.last_mut() {
            item.caret = caret;
            item.stops = stops[1..].iter().map(|stop| *stop..*stop).collect();
            item.key = "VALUES".to_owned();
        }
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
        self.taken = self.taken();
        for schema in options.search {
            let objects = catalog.objects(schema);
            if objects.is_none() {
                self.needs.push(Need::Objects(schema.clone()));
            }
            for object in objects.unwrap_or_default() {
                if let Some(kind) = relation_kind(object.kind)
                    && let Some(score) = score(&object.name, prefix)
                {
                    self.pending.push(Pending {
                        kind,
                        schema,
                        name: &object.name,
                        qualified: false,
                        score,
                    });
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
                        && let Some(score) = score(&object.name, prefix)
                    {
                        self.pending.push(Pending {
                            kind,
                            schema,
                            name: &object.name,
                            qualified: true,
                            score,
                        });
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
            let call = call_template(&name, self.options.family.signature(function));
            self.push_keyed(
                Kind::Function,
                format!("{name}()"),
                call.insert,
                String::new(),
                name,
            );
            if let Some(item) = self.items.last_mut() {
                item.caret = call.caret;
                item.caret_len = call.caret_len;
                item.stops = call.stops;
            }
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

/// A function call to insert: its text and where the caret and the stops
/// of its arguments go.
struct Call {
    insert: String,
    caret: usize,
    caret_len: usize,
    stops: Vec<Range<usize>>,
}

/// `name(arg, arg)` for a `signature` of arguments, the first one selected
/// and Tab visiting the rest, then the end of the call. `name()` with the
/// caret inside when the arguments are unknown or there are none.
fn call_template(name: &str, signature: Option<&str>) -> Call {
    let arguments: Vec<&str> = signature
        .filter(|signature| !signature.is_empty())
        .map_or_else(Vec::new, |signature| signature.split(", ").collect());
    if arguments.is_empty() {
        let insert = format!("{name}()");
        return Call {
            caret: insert.len() - 1,
            caret_len: 0,
            stops: Vec::new(),
            insert,
        };
    }
    let mut insert = format!("{name}(");
    let mut places = Vec::new();
    for (index, argument) in arguments.iter().enumerate() {
        if index > 0 {
            insert.push_str(", ");
        }
        places.push(insert.len()..insert.len() + argument.len());
        insert.push_str(argument);
    }
    insert.push(')');
    let first = places.remove(0);
    places.push(insert.len()..insert.len());
    Call {
        caret: first.start,
        caret_len: first.len(),
        stops: places,
        insert,
    }
}

/// `(a, b) VALUES (, )`: the column list, the offsets between the commas of
/// the values, and the text.
fn values_template(columns: &[String], values: &str) -> (String, Vec<usize>, String) {
    let list = columns.join(", ");
    let head = format!("({list}) ");
    let (stops, tail) = values_tail(columns.len(), values);
    let stops = stops.into_iter().map(|stop| head.len() + stop).collect();
    (list, stops, head + &tail)
}

/// `VALUES (, )` for `count` values, and the offsets of each value's place.
fn values_tail(count: usize, values: &str) -> (Vec<usize>, String) {
    let mut insert = format!("{values} (");
    let mut stops = Vec::new();
    for index in 0..count {
        if index > 0 {
            insert.push_str(", ");
        }
        stops.push(insert.len());
    }
    insert.push(')');
    (stops, insert)
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
    if key.is_ascii() && prefix.is_ascii() {
        return score_ascii(key.as_bytes(), prefix.as_bytes());
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

/// [`score`] for ASCII, which allocates nothing: it runs for every name in a
/// large catalog.
fn score_ascii(key: &[u8], prefix: &[u8]) -> Option<u8> {
    let starts = |part: &[u8]| {
        part.len() >= prefix.len() && part[..prefix.len()].eq_ignore_ascii_case(prefix)
    };
    if starts(key) {
        return Some(0);
    }
    let mut initials = key
        .split(|b| *b == b'_')
        .filter_map(|part| part.first().copied());
    if key.split(|b| *b == b'_').skip(1).any(starts)
        || prefix.iter().all(|wanted| {
            initials
                .next()
                .is_some_and(|c| c.eq_ignore_ascii_case(wanted))
        })
    {
        return Some(1);
    }
    let mut letters = key.iter();
    prefix
        .iter()
        .all(|wanted| letters.any(|c| c.eq_ignore_ascii_case(wanted)))
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
    rank_pending(
        items,
        Vec::new(),
        &|_| unreachable!(),
        prefix,
        history,
        relation,
    )
}

/// One candidate of [`rank_pending`], with what the comparison reads worked
/// out once, not once per comparison: a large catalog sorts thousands.
struct Ranked<'a> {
    score: u8,
    recency: u64,
    order: (bool, Kind),
    key_len: usize,
    /// Where it came in, to keep equal candidates in that order.
    seq: usize,
    /// The label, lower-cased, of a built item.
    label: String,
    entry: Entry<'a>,
}

enum Entry<'a> {
    Built(Item),
    Pending(Pending<'a>),
    /// Taken already.
    Done,
}

impl Ranked<'_> {
    /// The label's characters, lower-cased, as [`Item::label`] would give.
    fn label_chars(&self) -> impl Iterator<Item = char> + '_ {
        let (schema, name) = match &self.entry {
            Entry::Pending(p) => (p.qualified.then_some(p.schema), p.name),
            _ => (None, ""),
        };
        schema
            .into_iter()
            .flat_map(|schema| {
                schema
                    .chars()
                    .flat_map(char::to_lowercase)
                    .chain(std::iter::once('.'))
            })
            .chain(name.chars().flat_map(char::to_lowercase))
            .chain(self.label.chars())
    }
}

/// [`rank`] for built items and for catalog tables not built yet: those are
/// ranked on references, and only the first [`MAX_ITEMS`] are built.
fn rank_pending<'a>(
    items: Vec<Item>,
    pending: Vec<Pending<'a>>,
    build: &dyn Fn(&Pending<'a>) -> Item,
    prefix: &str,
    history: &History,
    relation: bool,
) -> Vec<Item> {
    let mut ranked: Vec<Ranked<'a>> = Vec::with_capacity(items.len() + pending.len());
    for item in items {
        let Some(score) = score(&item.key, prefix) else {
            continue;
        };
        ranked.push(Ranked {
            score,
            recency: history.recency(item.kind, &item.key),
            order: kind_order(item.kind, relation),
            key_len: item.key.len(),
            seq: ranked.len(),
            label: if prefix.is_empty() {
                String::new()
            } else {
                item.label.to_lowercase()
            },
            entry: Entry::Built(item),
        });
    }
    for pending in pending {
        ranked.push(Ranked {
            score: pending.score,
            recency: history.recency(pending.kind, pending.name),
            order: kind_order(pending.kind, relation),
            key_len: pending.name.len(),
            seq: ranked.len(),
            label: String::new(),
            entry: Entry::Pending(pending),
        });
    }
    let compare = |x: &Ranked<'_>, y: &Ranked<'_>| {
        let order = x
            .score
            .cmp(&y.score)
            .then_with(|| y.recency.cmp(&x.recency))
            .then(x.order.cmp(&y.order));
        if prefix.is_empty() {
            return order.then(x.seq.cmp(&y.seq));
        }
        order
            .then(x.key_len.cmp(&y.key_len))
            .then_with(|| x.label_chars().cmp(y.label_chars()))
            .then(x.seq.cmp(&y.seq))
    };
    // Sort only as far as the list needs: the best few, and more if
    // duplicates leave it short.
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(MAX_ITEMS.min(ranked.len()));
    let (mut sorted, mut next) = (0, 0);
    while out.len() < MAX_ITEMS && next < ranked.len() {
        if next == sorted {
            let upto = (sorted + (MAX_ITEMS - out.len()) * 2).min(ranked.len());
            if upto < ranked.len() {
                ranked[sorted..].select_nth_unstable_by(upto - sorted - 1, compare);
            }
            ranked[sorted..upto].sort_by(compare);
            sorted = upto;
        }
        let entry = std::mem::replace(&mut ranked[next].entry, Entry::Done);
        next += 1;
        let item = match entry {
            Entry::Built(item) => item,
            Entry::Pending(pending) => build(&pending),
            Entry::Done => continue,
        };
        if seen.insert(item.insert.clone()) {
            out.push(item);
        }
    }
    out
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

    /// What hovering at `|` in `text` shows, as title and detail.
    fn hovering(catalog: &Fake, text: &str) -> Option<(String, String)> {
        let offset = text.find('|').expect("a pointer");
        let text = text.replacen('|', "", 1);
        let search = vec!["public".to_owned()];
        let quote = |name: &str| format!("\"{name}\"");
        let found = hover(
            &text,
            offset,
            catalog,
            &Options {
                search: &search,
                database: Some("shop"),
                quote: &quote,
                family: Family::Postgres,
                case: KeywordCase::default(),
                history: &History::default(),
            },
        )?;
        assert!(found.range.contains(&offset) || found.range.end == offset);
        Some((found.title, found.detail))
    }

    #[test]
    fn hover_names_tables_columns_and_functions() {
        let shop = shop();
        let (title, detail) = hovering(&shop, "SELECT 1 FROM cust|omers c").expect("table");
        assert_eq!(title, "public.customers");
        assert_eq!(detail, "table · 3 columns: id, name, country");
        // An alias is its table.
        let (title, _) =
            hovering(&shop, "SELECT c.name FROM customers c WHERE |c.id = 1").expect("alias");
        assert_eq!(title, "public.customers");
        let (title, detail) =
            hovering(&shop, "SELECT c.na|me FROM customers c").expect("qualified column");
        assert_eq!(title, "c.name");
        assert_eq!(detail, "integer · nullable");
        // The primary key says so; a bare column is found among the tables.
        let (_, detail) = hovering(&shop, "SELECT i|d FROM customers").expect("column");
        assert_eq!(detail, "integer · not null · primary key");
        let (title, detail) =
            hovering(&shop, "SELECT slug|ify(name) FROM customers").expect("function");
        assert_eq!(
            (title.as_str(), detail.as_str()),
            ("public.slugify", "function")
        );
        let (title, detail) = hovering(&shop, "SELECT coal|esce(a, b)").expect("built-in");
        assert_eq!(
            (title.as_str(), detail.as_str()),
            ("coalesce()", "built-in function")
        );
        // Strings, comments, numbers and unknown words say nothing.
        assert_eq!(hovering(&shop, "SELECT 'cust|omers'"), None);
        assert_eq!(hovering(&shop, "-- cust|omers"), None);
        assert_eq!(hovering(&shop, "SELECT 4|2"), None);
        assert_eq!(hovering(&shop, "SELECT xyz|zy FROM customers"), None);
    }

    #[test]
    fn hover_reads_columns_of_subqueries_and_the_catalog_not_loaded() {
        let shop = shop();
        let (title, detail) =
            hovering(&shop, "SELECT s.to|tal FROM (SELECT total FROM orders) s").expect("derived");
        assert_eq!(title, "s.total");
        assert_eq!(detail, "column of s");
        // Nothing loaded for `targets`: it is a table, nothing more.
        let (_, detail) = hovering(&shop, "SELECT 1 FROM sales.tar|gets").expect("schema table");
        assert_eq!(detail, "table");
    }

    #[test]
    fn redis_signature_marks_the_argument() {
        let marked = |text: &str| {
            let signature = redis_signature(text, text.len()).expect("a signature");
            let active = signature.active.expect("an argument");
            (signature.label.clone(), signature.label[active].to_owned())
        };
        let (label, active) = marked("SET ");
        assert!(label.starts_with("SET key value"));
        assert_eq!(active, "key");
        assert_eq!(marked("SET key ").1, "value");
        assert_eq!(marked("SET key va").1, "value");
        assert_eq!(marked("set \"a b\" ").1, "value");
        // A container's name has two words.
        assert!(redis_signature("CONFIG ", 7).is_none());
        assert_eq!(marked("CONFIG GET ").1, "parameter");
        // Still typing the name: the list is what helps.
        assert!(redis_signature("SE", 2).is_none());
        assert!(redis_signature("NOPE x ", 7).is_none());
        // Only the line under the cursor counts.
        assert!(redis_signature("SET a b\n", 8).is_none());
    }

    #[test]
    fn redis_hover_shows_the_command_syntax() {
        let hint = redis_hover("GET a\nconfig get x", 14).expect("container");
        assert_eq!(hint.title, "CONFIG GET");
        assert_eq!(&"GET a\nconfig get x"[hint.range], "get");
        assert!(redis_hover("GET a", 5).is_none());
        assert_eq!(redis_hover("GET a", 1).expect("command").title, "GET");
        assert!(redis_hover("GET a", 4).is_none());
        assert!(redis_hover("NOPE a", 1).is_none());
    }

    /// The names marked in `text`, as typed, with the catalog `shop`.
    fn unknown(shop: &Fake, text: &str) -> Vec<String> {
        let search = vec!["public".to_owned()];
        let quote = |name: &str| name.to_owned();
        let options = Options {
            search: &search,
            database: Some("shop"),
            quote: &quote,
            family: Family::Postgres,
            case: KeywordCase::default(),
            history: &History::default(),
        };
        unknown_names(text, shop, &options)
            .into_iter()
            .map(|d| text[d.range].to_owned())
            .collect()
    }

    #[test]
    fn missing_tables_and_columns_are_marked_only_when_sure() {
        let mut shop = shop();
        shop.schemas.push("archive".into());
        assert_eq!(unknown(&shop, "SELECT * FROM custmers"), ["custmers"]);
        assert_eq!(unknown(&shop, "SELECT * FROM public.nope"), ["public.nope"]);
        assert_eq!(
            unknown(&shop, "SELECT o.totl, o.total FROM orders o"),
            ["totl"]
        );
        assert_eq!(
            unknown(&shop, "SELECT c.zzz FROM orders o, customers c"),
            ["zzz"]
        );
        assert_eq!(
            unknown(&shop, "UPDATE orders SET total = 1 WHERE orders.x = 1"),
            ["x"]
        );
        assert_eq!(
            unknown(&shop, "INSERT INTO nope (a) VALUES (1); DELETE FROM orders"),
            ["nope"]
        );
        // What is known, or cannot be told, is left alone.
        for fine in [
            "SELECT * FROM customers c JOIN orders o ON o.customer_id = c.id",
            "SELECT * FROM \"orders\"",
            "WITH t AS (SELECT 1 AS a) SELECT t.a, t.zzz FROM t",
            "SELECT s.a FROM (SELECT 1 AS a) s",
            "SELECT * FROM generate_series(1, 3) g",
            "SELECT 1 INTO backup FROM orders",
            "CREATE TABLE scratch (a int); SELECT s.a, s.b FROM scratch s",
            "SELECT * FROM archive.old",
            "SELECT * FROM elsewhere.old",
            "SELECT * FROM pg_class",
            "SELECT o.ctid FROM orders o",
            "SELECT o.a.b, o.f(1) FROM orders o",
            "SELECT * FROM public.orders WHERE public.orders.id = 1",
            "SET search_path TO other; SELECT * FROM mine",
            "DROP TABLE IF EXISTS gone",
            "SELECT x.a FROM orders x, customers x",
        ] {
            assert!(
                unknown(&shop, fine).is_empty(),
                "{fine}: {:?}",
                unknown(&shop, fine)
            );
        }
        // A quoted name is exact.
        assert_eq!(unknown(&shop, "SELECT * FROM \"Orders\""), ["\"Orders\""]);
        // Columns not loaded say nothing; tables not loaded say nothing.
        shop.columns.clear();
        assert!(unknown(&shop, "SELECT o.zzz FROM orders o").is_empty());
        shop.objects.clear();
        assert!(unknown(&shop, "SELECT * FROM custmers").is_empty());
    }

    #[test]
    fn mongo_methods_have_their_syntax_on_hover() {
        let hover = |text: &str| {
            let at = text.find('|').expect("cursor");
            mongo_hover(&text.replacen('|', "", 1), at).map(|h| (h.title, h.detail))
        };
        assert_eq!(
            hover("db.orders.fi|nd({})").map(|h| h.0).as_deref(),
            Some("db.collection.find(filter, projection)")
        );
        assert_eq!(
            hover("db.getCollectionNames|()").map(|h| h.0).as_deref(),
            Some("db.getCollectionNames()")
        );
        assert_eq!(
            hover("db.orders.find({}).so|rt({ a: 1 })")
                .map(|h| h.0)
                .as_deref(),
            Some("cursor.sort(order)")
        );
        assert_eq!(
            hover("db.getCollection('x').update|One({})")
                .map(|h| h.0)
                .as_deref(),
            Some("db.collection.updateOne(filter, update, options)")
        );
        // Not a method of its owner, not a call, not after a dot.
        assert!(hover("db.orders.sor|t()").is_none());
        assert!(hover("db.orders.fi|nd").is_none());
        assert!(hover("fi|nd({})").is_none());
        assert!(hover("db.ord|ers.find({})").is_none());
        // Every method the list offers is described.
        for method in MONGO_DB.iter().chain(MONGO_COLLECTION).chain(MONGO_CURSOR) {
            assert!(
                MONGO_SYNTAX.iter().any(|(known, ..)| known == method),
                "{method}"
            );
        }
        assert!(MONGO_SYNTAX.windows(2).all(|pair| pair[0].0 < pair[1].0));
    }

    #[test]
    fn typed_columns_get_a_values_snippet() {
        let completion = at(&shop(), "INSERT INTO customers (id, name) |");
        let values = completion
            .items
            .iter()
            .find(|item| item.detail == "template")
            .expect("template");
        assert_eq!(values.insert, "VALUES (, )");
        let stops: Vec<usize> = std::iter::once(values.caret)
            .chain(values.stops.iter().map(|stop| stop.start))
            .collect();
        assert_eq!(stops, [8, 10]);
        // The keywords stay.
        assert!(
            completion
                .items
                .iter()
                .any(|item| item.insert == "RETURNING")
        );
        let lower = at(&shop(), "insert into public.orders (customer_id) |");
        let values = lower
            .items
            .iter()
            .find(|i| i.detail == "template")
            .expect("one");
        assert_eq!(values.insert, "VALUES ()");
        // Not after other brackets, nor with no columns.
        for text in [
            "INSERT INTO customers () |",
            "SELECT * FROM customers (id) |",
            "INSERT INTO customers (id, name) VALUES (1, 'a') |",
            "INSERT INTO customers |",
        ] {
            let completion = at(&shop(), text);
            let found = completion
                .items
                .iter()
                .any(|item| item.insert.starts_with("VALUES ("));
            assert!(!found, "{text}");
        }
        // Typing the bracket's end opens the list by itself.
        assert!(opens_by_itself("INSERT INTO customers (id)", 26));
        assert!(!opens_by_itself("SELECT (a)", 10));
    }

    #[test]
    fn a_call_shows_its_signature_with_the_argument_being_written() {
        let marked = |text: &str, family: Family| {
            let cursor = text.find('|').expect("cursor");
            let text = text.replacen('|', "", 1);
            sql_signature(&text, cursor, family).map(|sig| {
                let active = sig.active.map(|range| sig.label[range].to_owned());
                (sig.label, active)
            })
        };
        let round = |active: Option<&str>| {
            Some((
                "round(number, decimals)".to_owned(),
                active.map(str::to_owned),
            ))
        };
        assert_eq!(
            marked("SELECT round(|", Family::Postgres),
            round(Some("number"))
        );
        assert_eq!(
            marked("SELECT round(a, |", Family::Postgres),
            round(Some("decimals"))
        );
        // A nested call or parenthesis keeps its commas to itself.
        assert_eq!(
            marked("SELECT round(coalesce(a, b), |", Family::Postgres),
            round(Some("decimals"))
        );
        assert_eq!(
            marked("SELECT round(coalesce(a, |", Family::Postgres),
            Some((
                "coalesce(value, value)".to_owned(),
                Some("value".to_owned())
            ))
        );
        // Past the named arguments the last one repeats.
        assert!(marked("SELECT coalesce(a, b, c, |", Family::Postgres).is_some());
        assert_eq!(
            marked("SELECT now(|", Family::Postgres),
            Some(("now()".to_owned(), None))
        );
        // Not in a call, a string, a comment, a closed call, or one unknown.
        assert!(marked("SELECT |", Family::Postgres).is_none());
        assert!(marked("SELECT round(a, 'x|", Family::Postgres).is_none());
        assert!(marked("SELECT round(a -- x|", Family::Postgres).is_none());
        assert!(marked("SELECT round(a, b) |", Family::Postgres).is_none());
        assert!(marked("SELECT mine(|", Family::Postgres).is_none());
        assert!(marked("SELECT 1 WHERE a IN (|", Family::Postgres).is_none());
        // The statement before it does not leak into the next.
        assert!(marked("SELECT round(a; SELECT |", Family::Postgres).is_none());
        // A function this database lacks.
        assert!(marked("SELECT array_agg(|", Family::MySql).is_none());
    }

    #[test]
    fn a_function_inserts_its_arguments_as_placeholders() {
        let call = call_template("round", Some("number, decimals"));
        assert_eq!(call.insert, "round(number, decimals)");
        assert_eq!(
            &call.insert[call.caret..call.caret + call.caret_len],
            "number"
        );
        let names: Vec<&str> = call.stops.iter().map(|s| &call.insert[s.clone()]).collect();
        // The second argument, then the end of the call.
        assert_eq!(names, ["decimals", ""]);
        assert_eq!(call.stops[1].start, call.insert.len());
        // None known, or none taken: the caret goes inside the parentheses.
        for signature in [None, Some("")] {
            let call = call_template("f", signature);
            assert_eq!(
                (call.insert.as_str(), call.caret, call.caret_len),
                ("f()", 2, 0)
            );
            assert!(call.stops.is_empty());
        }
        let completion = at(&shop(), "SELECT rou|");
        let item = completion
            .items
            .iter()
            .find(|item| item.label == "round()")
            .expect("round");
        assert_eq!(item.insert, "round(number, decimals)");
        let custom = at(&shop(), "SELECT slug|");
        let item = custom
            .items
            .iter()
            .find(|item| item.label == "slugify()")
            .expect("slugify");
        assert_eq!((item.insert.as_str(), item.caret), ("slugify()", 8));
    }

    #[test]
    fn insert_offers_a_template_with_a_stop_per_value() {
        let completion = at(&shop(), "INSERT INTO customers |");
        let template = completion
            .items
            .iter()
            .find(|item| item.detail == "template")
            .expect("template");
        assert_eq!(template.insert, "(id, name, country) VALUES (, , )");
        // The caret and the stops sit inside each value's place.
        let places: Vec<usize> = std::iter::once(template.caret)
            .chain(template.stops.iter().map(|stop| stop.start))
            .collect();
        assert_eq!(places.len(), 3);
        for place in &places {
            assert!(template.insert[..*place].ends_with(['(', ' ']));
        }
        assert_eq!(&template.insert[places[2]..], ")");
        // Other statements get none.
        let select = at(&shop(), "SELECT * FROM customers |");
        assert!(select.items.iter().all(|item| item.detail != "template"));
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
        assert!(!inserts(&mysql).contains(&"array_agg(expression)"));
        let postgres = with(Family::Postgres, "SELECT ar| FROM orders");
        assert!(inserts(&postgres).contains(&"array_agg(expression)"));
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
    fn catalog_needs_are_answered_once_the_catalog_has_them() {
        let mut fake = Fake {
            schemas: vec!["public".into()],
            ..Fake::default()
        };
        let waiting = at(&fake, "SELECT * FROM ord|");
        assert_eq!(waiting.needs, [Need::Objects("public".into())]);
        assert!(!waiting.needs_answered(&fake, None));
        fake.objects
            .insert("public".into(), vec![object("orders", ObjectKind::Table)]);
        assert!(waiting.needs_answered(&fake, None));

        let columns = at(&fake, "SELECT | FROM orders");
        assert_eq!(
            columns.needs,
            [Need::Columns("public".into(), "orders".into())]
        );
        assert!(!columns.needs_answered(&fake, None));
        fake.columns
            .insert(("public".into(), "orders".into()), vec![column("id", true)]);
        assert!(columns.needs_answered(&fake, None));

        let joins = at(&fake, "SELECT * FROM orders o JOIN |");
        assert!(joins.needs.contains(&Need::ForeignKeys("public".into())));
        assert!(!joins.needs_answered(&fake, None));
        fake.keys.insert("public".into(), Vec::new());
        assert!(joins.needs_answered(&fake, None));
        // Nothing needed, nothing to answer.
        assert!(!Completion::default().needs_answered(&fake, None));
    }

    #[test]
    fn key_needs_are_answered_by_the_keys_not_always() {
        let shop = shop();
        let none = Completion {
            needs: vec![Need::Keys("db0".into())],
            ..Completion::default()
        };
        let keys = [object("user:1", ObjectKind::Key)];
        // Not listed yet: still waiting, whatever the catalog says.
        assert!(!none.needs_answered(&shop, None));
        assert!(!none.needs_answered(&shop, Some(&keyspace(None))));
        assert!(none.needs_answered(&shop, Some(&keyspace(Some(&keys)))));

        let lookup = Completion {
            needs: vec![Need::KeysMatching("db0".into(), "user:".into())],
            ..Completion::default()
        };
        let mut space = keyspace(Some(&keys));
        space.whole = false;
        assert!(!lookup.needs_answered(&shop, Some(&space)));
        // A lookup on its way is not an answer; one on a prefix of it is.
        let loading = [("user:", None)];
        space.found = &loading;
        assert!(!lookup.needs_answered(&shop, Some(&space)));
        let answered = [("us", Some(&keys[..]))];
        space.found = &answered;
        assert!(lookup.needs_answered(&shop, Some(&space)));
        let other = [("or", Some(&keys[..]))];
        space.found = &other;
        assert!(!lookup.needs_answered(&shop, Some(&space)));
        // The whole keyspace listed leaves nothing to look up.
        space.whole = true;
        assert!(lookup.needs_answered(&shop, Some(&space)));
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
    fn only_the_best_items_of_a_large_catalog_are_built() {
        let (fake, _) = large();
        let found = at(&fake, "SELECT * FROM tab|");
        assert_eq!(found.items.len(), MAX_ITEMS);
        // The order is the one of ranking everything: shorter names first,
        // then by label, which puts other schemas' qualified names first.
        let mut all: Vec<(usize, String, String)> = fake
            .objects
            .iter()
            .flat_map(|(schema, objects)| {
                objects.iter().map(move |o| {
                    let label = if schema == "public" {
                        o.name.clone()
                    } else {
                        format!("{schema}.{}", o.name)
                    };
                    (o.name.len(), label, o.name.clone())
                })
            })
            .collect();
        all.sort();
        let expected: Vec<&str> = all
            .iter()
            .take(MAX_ITEMS)
            .map(|(.., n)| n.as_str())
            .collect();
        let keys: Vec<&str> = found.items.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, expected);
        // What is built is complete: qualified, with its alias.
        let first = &found.items[0];
        assert_eq!(first.insert, "s1.table_1_0 t");
        assert_eq!(first.detail, "s1");
        // A typed word that matches little leaves a short list.
        let few = at(&fake, "SELECT * FROM table_0_49|");
        assert!(few.items.len() < MAX_ITEMS);
        assert_eq!(few.items[0].insert, "table_0_49 t");
    }

    #[test]
    fn duplicates_do_not_shorten_the_list() {
        let mut fake = shop();
        let many = (0..MAX_ITEMS + 50)
            .map(|n| object(&format!("order_{n:04}"), ObjectKind::Table))
            .collect();
        fake.objects.insert("public".into(), many);
        let found = at(&fake, "SELECT * FROM ord|");
        assert_eq!(found.items.len(), MAX_ITEMS);
        let mut seen = HashSet::new();
        assert!(found.items.iter().all(|i| seen.insert(&i.insert)));
    }

    #[test]
    fn scoring_ignores_case_without_changing_what_matches() {
        assert_eq!(score("Order_Items", "ord"), Some(0));
        assert_eq!(score("order_items", "ITEMS"), Some(1));
        assert_eq!(score("order_items", "oi"), Some(1));
        assert_eq!(score("order_items", "otm"), Some(2));
        assert_eq!(score("order_items", "xyz"), None);
        assert_eq!(score("Ação", "aç"), Some(0));
        assert_eq!(score("anything", ""), Some(0));
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
