//! What the interface remembers about connections and their catalogs.

use std::collections::{HashMap, HashSet};

use violet_core::{ColumnInfo, ConnectionId, DbObject, ObjectKind, ServerInfo};

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

    pub fn from_result(result: violet_core::Result<T>) -> Self {
        match result {
            Ok(value) => Self::Ready(value),
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
    Schema(ConnectionId, String),
    Group(ConnectionId, String, ObjectKind),
    Object(ConnectionId, String, String),
}

/// Catalogs fetched so far, and the tree's expansion.
#[derive(Default)]
pub struct Explorer {
    pub links: HashMap<ConnectionId, Link>,
    pub schemas: HashMap<ConnectionId, Loadable<Vec<String>>>,
    pub objects: HashMap<(ConnectionId, String), Loadable<Vec<DbObject>>>,
    pub columns: HashMap<(ConnectionId, String, String), Loadable<Vec<ColumnInfo>>>,
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
        self.schemas.remove(&id);
        self.objects.retain(|(owner, _), _| *owner != id);
        self.columns.retain(|(owner, _, _), _| *owner != id);
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
        | NodeKey::Schema(id, _)
        | NodeKey::Group(id, _, _)
        | NodeKey::Object(id, _, _) => *id,
    }
}
