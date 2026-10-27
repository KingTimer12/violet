//! What the interface remembers about connections and their catalogs.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

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

/// How many times a schema's foreign keys are asked for before a failure
/// stands.
const FOREIGN_KEY_TRIES: u8 = 3;

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
    pub columns: HashMap<RelationKey, Loadable<Arc<[ColumnInfo]>>>,
    /// Foreign keys from or to each schema's tables, for completion.
    pub foreign_keys: HashMap<(ConnectionId, String), Loadable<Arc<[ForeignKey]>>>,
    /// How many times each schema's foreign keys were asked for, while they
    /// are not loaded.
    pub foreign_key_tries: HashMap<(ConnectionId, String), u8>,
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

    /// Stores a schema's foreign keys as loaded. A failure is asked again, up
    /// to [`FOREIGN_KEY_TRIES`] times in all; returns whether to ask now.
    pub fn foreign_keys_loaded(
        &mut self,
        id: ConnectionId,
        schema: String,
        result: violet_core::Result<Arc<[ForeignKey]>>,
    ) -> bool {
        let key = (id, schema);
        if result.is_err() {
            let tries = self.foreign_key_tries.entry(key.clone()).or_insert(1);
            if *tries < FOREIGN_KEY_TRIES {
                *tries += 1;
                self.foreign_keys.insert(key, Loadable::Loading);
                return true;
            }
        } else {
            self.foreign_key_tries.remove(&key);
        }
        self.foreign_keys.insert(key, Loadable::from_result(result));
        false
    }

    /// Forgets everything loaded for a connection, keeping expansion so a
    /// reconnect reopens the same branches.
    pub fn forget(&mut self, id: ConnectionId) {
        self.databases.remove(&id);
        self.schemas.remove(&id);
        self.objects.retain(|(owner, _), _| *owner != id);
        self.columns.retain(|(owner, _, _), _| *owner != id);
        self.foreign_keys.retain(|(owner, _), _| *owner != id);
        self.foreign_key_tries.retain(|(owner, _), _| *owner != id);
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
        ready(self.explorer.objects.get(&(id, schema.to_owned())), &[])
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

    #[test]
    fn failed_foreign_keys_are_asked_again_a_few_times() {
        let mut explorer = Explorer::default();
        let id = ConnectionId::new();
        let key = (id, "public".to_owned());
        assert!(explorer.foreign_keys_loaded(id, "public".into(), failure()));
        assert!(explorer.foreign_keys[&key].is_loading());
        assert!(explorer.foreign_keys_loaded(id, "public".into(), failure()));
        // The third failure stands.
        assert!(!explorer.foreign_keys_loaded(id, "public".into(), failure()));
        assert!(matches!(explorer.foreign_keys[&key], Loadable::Failed(_)));
    }

    #[test]
    fn foreign_keys_loaded_after_a_retry_are_kept() {
        let mut explorer = Explorer::default();
        let id = ConnectionId::new();
        assert!(explorer.foreign_keys_loaded(id, "public".into(), failure()));
        assert!(!explorer.foreign_keys_loaded(id, "public".into(), Ok(Arc::from(Vec::new()))));
        assert!(matches!(
            explorer.foreign_keys[&(id, "public".to_owned())],
            Loadable::Ready(_)
        ));
        assert!(explorer.foreign_key_tries.is_empty());
    }
}
