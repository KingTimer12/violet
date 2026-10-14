//! What the interface remembers about connections and their catalogs.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use violet_core::{ColumnInfo, ConnectionId, DbObject, ForeignKey, ObjectKind, ServerInfo};

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
    /// The scripts saved for a connection.
    Scripts(ConnectionId),
    Schema(ConnectionId, String),
    Group(ConnectionId, String, ObjectKind),
    Object(ConnectionId, String, String),
}

/// A relation: connection, schema and name.
pub type RelationKey = (ConnectionId, String, String);

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
    pub expanded: HashSet<NodeKey>,
    pub selected: Option<NodeKey>,
    pub filter: String,
}

impl Explorer {
    pub fn link(&self, id: ConnectionId) -> &Link {
        static OFFLINE: Link = Link::Offline;
        self.links.get(&id).unwrap_or(&OFFLINE)
    }

    pub fn is_expanded(&self, key: &NodeKey) -> bool {
        self.expanded.contains(key)
    }

    pub fn toggle(&mut self, key: NodeKey) -> bool {
        if self.expanded.remove(&key) {
            false
        } else {
            self.expanded.insert(key);
            true
        }
    }

    /// Forgets everything loaded for a connection, keeping expansion so a
    /// reconnect reopens the same branches.
    pub fn forget(&mut self, id: ConnectionId) {
        self.databases.remove(&id);
        self.schemas.remove(&id);
        self.objects.retain(|(owner, _), _| *owner != id);
        self.columns.retain(|(owner, _, _), _| *owner != id);
        self.foreign_keys.retain(|(owner, _), _| *owner != id);
    }

    /// Schemas whose objects completion offers unqualified: PostgreSQL's
    /// `public` and the user's own, or the database itself where schemas
    /// are databases; else the first one.
    pub fn search_path(&self, id: ConnectionId) -> Vec<String> {
        let Some(Loadable::Ready(schemas)) = self.schemas.get(&id) else {
            return Vec::new();
        };
        let mut path: Vec<String> = Vec::new();
        let mut add = |name: &str| {
            if schemas.iter().any(|schema| schema == name) && !path.iter().any(|p| p == name) {
                path.push(name.to_owned());
            }
        };
        if let Link::Online(server) = self.link(id) {
            add(&server.user);
        }
        add("public");
        if let Link::Online(server) = self.link(id) {
            add(&server.database);
        }
        if path.is_empty()
            && let Some(first) = schemas.first()
        {
            path.push(first.clone());
        }
        path
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
        self.expanded.retain(|key| owner(key) != id);
        if self.selected.as_ref().is_some_and(|key| owner(key) == id) {
            self.selected = None;
        }
    }
}

fn owner(key: &NodeKey) -> ConnectionId {
    match key {
        NodeKey::Connection(id)
        | NodeKey::Database(id, _)
        | NodeKey::Scripts(id)
        | NodeKey::Schema(id, _)
        | NodeKey::Group(id, _, _)
        | NodeKey::Object(id, _, _) => *id,
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
