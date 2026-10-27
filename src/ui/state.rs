//! What the interface remembers about connections and their catalogs.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::dialect::Family;

use violet_core::{
    ColumnInfo, ConnectionId, DbObject, ForeignKey, Language, ObjectKind, ServerInfo,
};

/// Something fetched from a database.
#[derive(Clone, Debug, Default)]
pub enum Loadable<T> {
    #[default]
    Idle,
    Loading,
    Ready(T),
    Failed(String),
}

impl<T> Loadable<T> {
    pub fn is_loading(&self) -> bool {
        matches!(self, Self::Loading)
    }

    pub fn from_result<U: Into<T>>(result: violet_core::Result<U>) -> Self {
        match result {
            Ok(value) => Self::Ready(value.into()),
            Err(error) => Self::Failed(error.to_string()),
        }
    }
}

/// How many times a load for completion is asked for before a failure
/// stands.
const LOAD_TRIES: u8 = 3;

/// The wait before a failed load is asked again; each retry waits twice as
/// long as the one before.
const RETRY_WAIT: Duration = Duration::from_millis(250);

/// A catalog load that is asked again when it fails.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum LoadKey {
    Objects(ConnectionId, String),
    Columns(ConnectionId, String, String),
    ForeignKeys(ConnectionId, String),
}

impl LoadKey {
    pub fn owner(&self) -> ConnectionId {
        match self {
            Self::Objects(id, _) | Self::Columns(id, ..) | Self::ForeignKeys(id, _) => *id,
        }
    }
}

/// Which loads failed, how many times, and when each is asked again.
#[derive(Default)]
pub struct Retries {
    tries: HashMap<LoadKey, u8>,
    due: HashMap<LoadKey, Instant>,
}

impl Retries {
    /// `key` failed at `now`: how long to wait before asking again, or `None`
    /// when it was asked [`LOAD_TRIES`] times and the failure stands.
    pub fn failed(&mut self, key: LoadKey, now: Instant) -> Option<Duration> {
        let tries = self.tries.entry(key.clone()).or_insert(1);
        if *tries >= LOAD_TRIES {
            self.tries.remove(&key);
            return None;
        }
        let wait = RETRY_WAIT * 2u32.pow(u32::from(*tries - 1));
        *tries += 1;
        self.due.insert(key, now + wait);
        Some(wait)
    }

    /// `key` loaded: its failures are forgotten.
    pub fn succeeded(&mut self, key: &LoadKey) {
        self.tries.remove(key);
        self.due.remove(key);
    }

    /// The loads whose wait is over at `now`, to be asked again.
    pub fn due(&mut self, now: Instant) -> Vec<LoadKey> {
        let keys: Vec<LoadKey> = self
            .due
            .iter()
            .filter(|(_, at)| **at <= now)
            .map(|(key, _)| key.clone())
            .collect();
        for key in &keys {
            self.due.remove(key);
        }
        keys
    }

    /// How long until the next load is due, if any waits.
    pub fn next_due(&self, now: Instant) -> Option<Duration> {
        self.due
            .values()
            .map(|at| at.saturating_duration_since(now))
            .min()
    }

    fn forget(&mut self, id: ConnectionId) {
        self.tries.retain(|key, _| key.owner() != id);
        self.due.retain(|key, _| key.owner() != id);
    }
}

/// A schema's objects, found by name without scanning them.
pub struct ObjectIndex {
    list: Arc<[DbObject]>,
    /// Positions in `list` by lower-cased name: a name may be shared by
    /// objects that differ in case or kind.
    names: HashMap<String, Vec<u32>>,
}

impl ObjectIndex {
    fn new(list: Arc<[DbObject]>) -> Self {
        let mut names: HashMap<String, Vec<u32>> = HashMap::with_capacity(list.len());
        for (at, object) in list.iter().enumerate() {
            let at = u32::try_from(at).unwrap_or(u32::MAX);
            names
                .entry(object.name.to_ascii_lowercase())
                .or_default()
                .push(at);
        }
        Self { list, names }
    }

    /// The first object called `name`, as completion matches names.
    fn find(&self, name: &str, quoted: bool, relation: bool) -> Option<&DbObject> {
        let lower = if name.bytes().any(|b| b.is_ascii_uppercase()) {
            std::borrow::Cow::Owned(name.to_ascii_lowercase())
        } else {
            std::borrow::Cow::Borrowed(name)
        };
        self.names
            .get(lower.as_ref())?
            .iter()
            .filter_map(|at| self.list.get(*at as usize))
            .find(|o| (!relation || o.kind.is_relation()) && (!quoted || o.name == name))
    }
}

/// A connection's state.
#[derive(Clone, Debug, Default)]
pub enum Link {
    #[default]
    Offline,
    Connecting,
    Online(ServerInfo),
    Failed(String),
}

impl Link {
    pub fn is_online(&self) -> bool {
        matches!(self, Self::Online(_))
    }
}

/// A node of the object tree, as expansion and selection remember it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum NodeKey {
    Connection(ConnectionId),
    /// A database of a connection that shows them all.
    Database(ConnectionId, String),
    /// A folder of saved connections, by its `/`-separated path.
    ConnectionFolder(String),
    /// The scripts saved for a connection.
    Scripts(ConnectionId),
    /// A folder of a connection's scripts: its database and path.
    ScriptFolder(ConnectionId, String, String),
    Schema(ConnectionId, String),
    Group(ConnectionId, String, ObjectKind),
    Object(ConnectionId, String, String),
    /// A folder of a session's database: its schemas, triggers, roles or
    /// extensions.
    Folder(ConnectionId, Folder),
}

/// The folders a database opens into, as in DBeaver.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Folder {
    Schemas,
    Triggers,
    Roles,
    Extensions,
}

impl Folder {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Schemas => "Schemas",
            Self::Triggers => "Triggers",
            Self::Roles => "Roles",
            Self::Extensions => "Extensions",
        }
    }
}

/// What a tree filter picks: a database's schemas, or a server's
/// databases.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeFilter {
    Schemas,
    Databases,
}

/// An entry of the triggers, roles or extensions folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderItem {
    pub name: String,
    /// Shown beside the name: a trigger's table, an extension's version.
    pub detail: Option<String>,
    /// A trigger's schema and table.
    pub schema: Option<String>,
    pub table: Option<String>,
}

/// A relation: connection, schema and name.
pub type RelationKey = (ConnectionId, String, String);

/// A key lookup: connection, database and the keys' prefix.
pub type PrefixKey = (ConnectionId, String, String);

/// A key-value database's keys, as far as they are listed: they come a
/// page at a time, matching a glob.
#[derive(Clone, Debug, Default)]
pub struct Keys {
    /// The glob as typed in the tree.
    pub pattern: String,
    /// The glob the keys shown were listed with.
    pub applied: String,
    pub list: Arc<[DbObject]>,
    /// Where listing goes on; `None` once every key was listed.
    pub next: Option<String>,
    pub loading: bool,
    pub error: Option<String>,
    /// Listed at least once.
    pub listed: bool,
}

impl Keys {
    /// The glob to list with: everything when the box is empty.
    pub fn glob(&self) -> String {
        let typed = self.pattern.trim();
        if typed.is_empty() {
            "*".to_owned()
        } else if typed.contains(['*', '?', '[']) {
            typed.to_owned()
        } else {
            // A plain word finds keys containing it, as the object filter
            // does.
            format!("*{typed}*")
        }
    }
}

/// Catalogs fetched so far, and the tree's expansion.
///
/// Lists are shared slices: the tree takes a handle each frame while it
/// mutates expansion, and cloning an `Arc` is a counter bump where cloning
/// a schema's thousand objects would be a thousand allocations.
#[derive(Default)]
pub struct Explorer {
    pub links: HashMap<ConnectionId, Link>,
    /// The databases on each connection's server, for connections that
    /// show them all.
    pub databases: HashMap<ConnectionId, Loadable<Arc<[String]>>>,
    /// Sessions opened on another database of a saved connection: their
    /// connection and database. They come and go with the app.
    pub opened: HashMap<ConnectionId, (ConnectionId, String)>,
    pub schemas: HashMap<ConnectionId, Loadable<Arc<[String]>>>,
    pub objects: HashMap<(ConnectionId, String), Loadable<Arc<[DbObject]>>>,
    /// Each loaded schema's objects by name, for completion's lookups; kept
    /// by [`Explorer::objects_loaded`].
    pub object_index: HashMap<ConnectionId, HashMap<String, ObjectIndex>>,
    pub columns: HashMap<RelationKey, Loadable<Arc<[ColumnInfo]>>>,
    /// Foreign keys from or to each schema's tables, for completion.
    pub foreign_keys: HashMap<(ConnectionId, String), Loadable<Arc<[ForeignKey]>>>,
    /// Failed loads waiting to be asked again.
    pub retries: Retries,
    /// The triggers, roles and extensions of each session's database.
    pub folders: HashMap<(ConnectionId, Folder), Loadable<Arc<[FolderItem]>>>,
    /// The keys of each key-value database.
    pub keys: HashMap<(ConnectionId, String), Keys>,
    /// Keys looked up by prefix for completion, by database and prefix.
    pub key_matches: HashMap<PrefixKey, Loadable<Arc<[DbObject]>>>,
    /// Each driver's language, by driver id, as installed.
    pub languages: HashMap<String, Language>,
    pub expanded: HashSet<NodeKey>,
    pub selected: Option<NodeKey>,
    /// The schemas shown per `connection/database` (see
    /// [`Explorer::schema_scope`]), as the settings keep them.
    pub schema_filters: BTreeMap<String, BTreeSet<String>>,
    /// The databases shown per saved connection (its id), as the settings
    /// keep them.
    pub database_filters: BTreeMap<String, BTreeSet<String>>,
    /// Each connection's transaction, as the backend last reported it.
    pub transactions: HashMap<ConnectionId, violet_app::Transaction>,
    /// A node was clicked since this was last cleared.
    pub picked: bool,
    /// The tree was clicked last, so F-key shortcuts act on its selection.
    pub focused: bool,
    pub filter: String,
}

impl Explorer {
    pub fn link(&self, id: ConnectionId) -> &Link {
        static OFFLINE: Link = Link::Offline;
        self.links.get(&id).unwrap_or(&OFFLINE)
    }

    /// `id`'s transaction: auto-commit, none open, until told otherwise.
    pub fn transaction(&self, id: ConnectionId) -> violet_app::Transaction {
        self.transactions.get(&id).copied().unwrap_or_default()
    }

    pub fn is_expanded(&self, key: &NodeKey) -> bool {
        self.expanded.contains(key)
    }

    /// Selects a node the user clicked.
    pub fn pick(&mut self, key: NodeKey) {
        self.selected = Some(key);
        self.picked = true;
        self.focused = true;
    }

    pub fn toggle(&mut self, key: NodeKey) -> bool {
        if self.expanded.remove(&key) {
            false
        } else {
            self.expanded.insert(key);
            true
        }
    }

    /// Stores a schema's foreign keys as loaded. A failure is asked again
    /// (see [`Retries`]), staying loading meanwhile; returns the wait.
    pub fn foreign_keys_loaded(
        &mut self,
        id: ConnectionId,
        schema: String,
        result: violet_core::Result<Arc<[ForeignKey]>>,
        now: Instant,
    ) -> Option<Duration> {
        let key = LoadKey::ForeignKeys(id, schema.clone());
        let wait = self.settled(key, result.is_err(), now);
        let loaded = if wait.is_some() {
            Loadable::Loading
        } else {
            Loadable::from_result(result)
        };
        self.foreign_keys.insert((id, schema), loaded);
        wait
    }

    /// Stores a relation's columns as loaded, a failure being asked again.
    pub fn columns_loaded(
        &mut self,
        id: ConnectionId,
        schema: String,
        relation: String,
        result: violet_core::Result<Arc<[ColumnInfo]>>,
        now: Instant,
    ) -> Option<Duration> {
        let key = LoadKey::Columns(id, schema.clone(), relation.clone());
        let wait = self.settled(key, result.is_err(), now);
        let loaded = if wait.is_some() {
            Loadable::Loading
        } else {
            Loadable::from_result(result)
        };
        self.columns.insert((id, schema, relation), loaded);
        wait
    }

    /// Whether a load that finished stands, or waits to be asked again.
    fn settled(&mut self, key: LoadKey, failed: bool, now: Instant) -> Option<Duration> {
        if failed {
            return self.retries.failed(key, now);
        }
        self.retries.succeeded(&key);
        None
    }

    /// Gives up on a load that was to be asked again: it counts as failed.
    pub fn abandon(&mut self, key: LoadKey, reason: &str) {
        match key {
            LoadKey::Objects(id, schema) => {
                self.drop_object_index(id, &schema);
                self.objects
                    .insert((id, schema), Loadable::Failed(reason.to_owned()));
            }
            LoadKey::Columns(id, schema, relation) => {
                self.columns
                    .insert((id, schema, relation), Loadable::Failed(reason.to_owned()));
            }
            LoadKey::ForeignKeys(id, schema) => {
                self.foreign_keys
                    .insert((id, schema), Loadable::Failed(reason.to_owned()));
            }
        }
    }

    /// Stores a schema's objects as loaded, and indexes them by name. A
    /// failure is asked again, staying loading meanwhile; returns the wait.
    pub fn objects_loaded(
        &mut self,
        id: ConnectionId,
        schema: String,
        result: violet_core::Result<Arc<[DbObject]>>,
        now: Instant,
    ) -> Option<Duration> {
        let key = LoadKey::Objects(id, schema.clone());
        let wait = self.settled(key, result.is_err(), now);
        let loaded = if wait.is_some() {
            Loadable::Loading
        } else {
            Loadable::from_result(result)
        };
        match &loaded {
            Loadable::Ready(list) => {
                self.object_index
                    .entry(id)
                    .or_default()
                    .insert(schema.clone(), ObjectIndex::new(Arc::clone(list)));
            }
            _ => self.drop_object_index(id, &schema),
        }
        self.objects.insert((id, schema), loaded);
        wait
    }

    /// Marks a schema's objects as loading, which drops their index.
    pub fn objects_loading(&mut self, id: ConnectionId, schema: String) {
        self.drop_object_index(id, &schema);
        self.objects.insert((id, schema), Loadable::Loading);
    }

    fn drop_object_index(&mut self, id: ConnectionId, schema: &str) {
        if let Some(schemas) = self.object_index.get_mut(&id) {
            schemas.remove(schema);
        }
    }

    /// Forgets everything loaded for a connection, keeping expansion so a
    /// reconnect reopens the same branches.
    pub fn forget(&mut self, id: ConnectionId) {
        self.databases.remove(&id);
        self.schemas.remove(&id);
        self.object_index.remove(&id);
        self.objects.retain(|(owner, _), _| *owner != id);
        self.columns.retain(|(owner, _, _), _| *owner != id);
        self.foreign_keys.retain(|(owner, _), _| *owner != id);
        self.retries.forget(id);
        self.folders.retain(|(owner, _), _| *owner != id);
        self.keys.retain(|(owner, _), _| *owner != id);
    }

    /// A driver's language; SQL for one not installed yet.
    pub fn language(&self, driver: &str) -> Language {
        self.languages.get(driver).copied().unwrap_or_default()
    }

    /// The key a saved connection's database choice is kept under.
    pub fn database_scope(id: ConnectionId) -> String {
        id.0.to_string()
    }

    /// The saved connection and database a session is on, as
    /// `connection/database`: the key its schema choice is kept under.
    /// `None` until the session says which database it is on.
    pub fn schema_scope(&self, id: ConnectionId) -> Option<String> {
        if let Some((parent, database)) = self.opened.get(&id) {
            return Some(format!("{}/{database}", parent.0));
        }
        match self.link(id) {
            Link::Online(server) => Some(format!("{}/{}", id.0, server.database)),
            _ => None,
        }
    }

    /// Schemas whose objects completion offers unqualified, as `family`
    /// looks names up.
    pub fn search_path(&self, id: ConnectionId, family: Family) -> Vec<String> {
        let Some(Loadable::Ready(schemas)) = self.schemas.get(&id) else {
            return Vec::new();
        };
        let (user, database) = match self.link(id) {
            Link::Online(server) => (server.user.as_str(), server.database.as_str()),
            _ => ("", ""),
        };
        family.search_path(schemas, user, database)
    }

    /// The session opened on `database` of connection `parent`.
    pub fn opened(&self, parent: ConnectionId, database: &str) -> Option<ConnectionId> {
        self.opened
            .iter()
            .find(|(_, (owner, name))| *owner == parent && name == database)
            .map(|(id, _)| *id)
    }

    /// The sessions opened on other databases of `parent`.
    pub fn opened_under(&self, parent: ConnectionId) -> Vec<ConnectionId> {
        self.opened
            .iter()
            .filter(|(_, (owner, _))| *owner == parent)
            .map(|(id, _)| *id)
            .collect()
    }

    pub fn remove(&mut self, id: ConnectionId) {
        self.forget(id);
        self.links.remove(&id);
        self.expanded.retain(|key| owner(key) != Some(id));
        if self
            .selected
            .as_ref()
            .is_some_and(|key| owner(key) == Some(id))
        {
            self.selected = None;
        }
    }
}

fn owner(key: &NodeKey) -> Option<ConnectionId> {
    match key {
        NodeKey::Connection(id)
        | NodeKey::Database(id, _)
        | NodeKey::Scripts(id)
        | NodeKey::ScriptFolder(id, ..)
        | NodeKey::Schema(id, _)
        | NodeKey::Group(id, _, _)
        | NodeKey::Object(id, _, _)
        | NodeKey::Folder(id, _) => Some(*id),
        NodeKey::ConnectionFolder(_) => None,
    }
}

/// One connection's catalog, as completion reads it. Failed loads count as
/// empty, so they are not asked for again and again.
pub struct ConnectionCatalog<'a> {
    pub explorer: &'a Explorer,
    pub id: Option<ConnectionId>,
}

fn ready<'a, T: ?Sized>(
    loadable: Option<&'a Loadable<Arc<T>>>,
    empty: &'static T,
) -> Option<&'a T> {
    match loadable? {
        Loadable::Ready(value) => Some(value),
        Loadable::Failed(_) => Some(empty),
        Loadable::Idle | Loadable::Loading => None,
    }
}

impl crate::completion::Catalog for ConnectionCatalog<'_> {
    fn schemas(&self) -> &[String] {
        let Some(id) = self.id else { return &[] };
        ready(self.explorer.schemas.get(&id), &[]).unwrap_or_default()
    }

    fn objects(&self, schema: &str) -> Option<&[DbObject]> {
        let id = self.id?;
        if let Some(index) = self
            .explorer
            .object_index
            .get(&id)
            .and_then(|m| m.get(schema))
        {
            return Some(&index.list);
        }
        ready(self.explorer.objects.get(&(id, schema.to_owned())), &[])
    }

    fn object(&self, schema: &str, name: &str, quoted: bool, relation: bool) -> Option<&DbObject> {
        let index = self
            .explorer
            .object_index
            .get(&self.id?)
            .and_then(|m| m.get(schema));
        match index {
            Some(index) => index.find(name, quoted, relation),
            None => self.objects(schema)?.iter().find(|o| {
                (!relation || o.kind.is_relation())
                    && crate::completion::same(&o.name, name, quoted)
            }),
        }
    }

    fn columns(&self, schema: &str, relation: &str) -> Option<&[ColumnInfo]> {
        let id = self.id?;
        let key = (id, schema.to_owned(), relation.to_owned());
        ready(self.explorer.columns.get(&key), &[])
    }

    fn foreign_keys(&self, schema: &str) -> Option<&[ForeignKey]> {
        let id = self.id?;
        ready(
            self.explorer.foreign_keys.get(&(id, schema.to_owned())),
            &[],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use violet_core::{DbError, ErrorKind};

    fn failure() -> violet_core::Result<Arc<[ForeignKey]>> {
        Err(DbError::new(ErrorKind::Connection, "dropped"))
    }

    fn object(name: &str, kind: violet_core::ObjectKind) -> DbObject {
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

    #[test]
    fn objects_are_found_by_name_through_the_index() {
        use crate::completion::Catalog;
        use violet_core::ObjectKind::{Function, Table};
        let mut explorer = Explorer::default();
        let id = ConnectionId::new();
        let list: Arc<[DbObject]> = vec![
            object("Orders", Function),
            object("orders", Table),
            object("Items", Table),
        ]
        .into();
        explorer.objects_loaded(id, "public".into(), Ok(list), Instant::now());
        let catalog = ConnectionCatalog {
            explorer: &explorer,
            id: Some(id),
        };
        assert_eq!(catalog.objects("public").map(<[_]>::len), Some(3));
        // Any case, the first one listed; or only what a query can read.
        assert_eq!(
            catalog
                .object("public", "ORDERS", false, false)
                .expect("found")
                .name,
            "Orders"
        );
        assert_eq!(
            catalog
                .object("public", "ORDERS", false, true)
                .expect("found")
                .name,
            "orders"
        );
        // Quoted names match exactly.
        assert_eq!(
            catalog
                .object("public", "orders", true, false)
                .expect("found")
                .name,
            "orders"
        );
        assert!(catalog.object("public", "ORDERS", true, false).is_none());
        assert!(catalog.object("public", "missing", false, false).is_none());
        assert!(catalog.object("sales", "orders", false, false).is_none());
        // Loading again leaves nothing to find until it answers.
        explorer.objects_loading(id, "public".into());
        let catalog = ConnectionCatalog {
            explorer: &explorer,
            id: Some(id),
        };
        assert!(catalog.object("public", "orders", false, false).is_none());
        assert!(catalog.objects("public").is_none());
    }

    fn objects_failure() -> violet_core::Result<Arc<[DbObject]>> {
        Err(DbError::new(ErrorKind::Connection, "dropped"))
    }

    #[test]
    fn failed_foreign_keys_are_asked_again_a_few_times() {
        let mut explorer = Explorer::default();
        let id = ConnectionId::new();
        let key = (id, "public".to_owned());
        let now = Instant::now();
        assert!(
            explorer
                .foreign_keys_loaded(id, "public".into(), failure(), now)
                .is_some()
        );
        assert!(explorer.foreign_keys[&key].is_loading());
        assert!(
            explorer
                .foreign_keys_loaded(id, "public".into(), failure(), now)
                .is_some()
        );
        // The third failure stands.
        assert!(
            explorer
                .foreign_keys_loaded(id, "public".into(), failure(), now)
                .is_none()
        );
        assert!(matches!(explorer.foreign_keys[&key], Loadable::Failed(_)));
    }

    #[test]
    fn foreign_keys_loaded_after_a_retry_are_kept() {
        let mut explorer = Explorer::default();
        let id = ConnectionId::new();
        let now = Instant::now();
        assert!(
            explorer
                .foreign_keys_loaded(id, "public".into(), failure(), now)
                .is_some()
        );
        let kept = Ok(Arc::from(Vec::new()));
        assert!(
            explorer
                .foreign_keys_loaded(id, "public".into(), kept, now)
                .is_none()
        );
        assert!(matches!(
            explorer.foreign_keys[&(id, "public".to_owned())],
            Loadable::Ready(_)
        ));
        assert!(explorer.retries.next_due(now).is_none());
    }

    #[test]
    fn failed_loads_wait_longer_each_time_and_are_due_after_the_wait() {
        let mut retries = Retries::default();
        let key = LoadKey::Objects(ConnectionId::new(), "public".into());
        let now = Instant::now();
        let first = retries.failed(key.clone(), now).expect("a retry");
        let second = retries.failed(key.clone(), now).expect("a retry");
        assert_eq!(second, first * 2);
        assert!(retries.failed(key.clone(), now).is_none());
        // A stood failure leaves nothing waiting, and the next load starts over.
        assert!(retries.next_due(now).is_some());
        assert!(retries.due(now).is_empty());
        assert_eq!(retries.due(now + second), std::slice::from_ref(&key));
        assert!(retries.due(now + second).is_empty());
        assert_eq!(retries.failed(key, now), Some(first));
    }

    #[test]
    fn failed_objects_and_columns_are_asked_again_then_stand() {
        let mut explorer = Explorer::default();
        let id = ConnectionId::new();
        let now = Instant::now();
        let objects = (id, "public".to_owned());
        for _ in 0..2 {
            let wait = explorer.objects_loaded(id, "public".into(), objects_failure(), now);
            assert!(wait.is_some());
            assert!(explorer.objects[&objects].is_loading());
        }
        let wait = explorer.objects_loaded(id, "public".into(), objects_failure(), now);
        assert!(wait.is_none());
        assert!(matches!(explorer.objects[&objects], Loadable::Failed(_)));

        let columns = (id, "public".to_owned(), "orders".to_owned());
        let failed = || Err(DbError::new(ErrorKind::Connection, "dropped"));
        let load = |explorer: &mut Explorer, result| {
            explorer.columns_loaded(id, "public".into(), "orders".into(), result, now)
        };
        assert!(load(&mut explorer, failed()).is_some());
        assert!(explorer.columns[&columns].is_loading());
        assert!(load(&mut explorer, failed()).is_some());
        assert!(load(&mut explorer, failed()).is_none());
        assert!(matches!(explorer.columns[&columns], Loadable::Failed(_)));
        // A load that works clears the count.
        assert!(load(&mut explorer, failed()).is_some());
        assert!(load(&mut explorer, Ok(Arc::from(Vec::new()))).is_none());
        assert!(load(&mut explorer, failed()).is_some());
        assert!(load(&mut explorer, failed()).is_some());
    }

    #[test]
    fn a_failed_need_counts_as_answered_once_it_stands() {
        use crate::completion::{Completion, Need};
        let mut explorer = Explorer::default();
        let id = ConnectionId::new();
        let now = Instant::now();
        let waiting = Completion {
            needs: vec![
                Need::Objects("public".into()),
                Need::Columns("public".into(), "orders".into()),
            ],
            ..Completion::default()
        };
        let answered = |explorer: &Explorer| {
            let catalog = ConnectionCatalog {
                explorer,
                id: Some(id),
            };
            waiting.needs_answered(&catalog, None)
        };
        assert!(!answered(&explorer));
        // Failed but to be asked again: still waiting.
        explorer.objects_loaded(id, "public".into(), objects_failure(), now);
        assert!(!answered(&explorer));
        explorer.objects_loaded(id, "public".into(), objects_failure(), now);
        explorer.objects_loaded(id, "public".into(), objects_failure(), now);
        assert!(answered(&explorer));
    }

    #[test]
    fn a_load_given_up_on_is_failed_and_forgetting_clears_the_waits() {
        let mut explorer = Explorer::default();
        let id = ConnectionId::new();
        let now = Instant::now();
        explorer.columns_loaded(
            id,
            "public".into(),
            "t".into(),
            Err(DbError::new(ErrorKind::Connection, "x")),
            now,
        );
        assert!(explorer.retries.next_due(now).is_some());
        explorer.abandon(
            LoadKey::Columns(id, "public".into(), "t".into()),
            "not connected",
        );
        let key = (id, "public".to_owned(), "t".to_owned());
        assert!(matches!(explorer.columns[&key], Loadable::Failed(_)));
        explorer.forget(id);
        assert!(explorer.retries.next_due(now).is_none());
    }
}
