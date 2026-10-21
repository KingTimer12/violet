//! What a database contains, as the object tree shows it.

/// The kinds of object the tree groups under a schema.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ObjectKind {
    Table,
    View,
    MaterializedView,
    ForeignTable,
    Function,
    Sequence,
    /// A key of a key-value store, such as Redis.
    Key,
}

impl ObjectKind {
    /// Kinds in the order the tree lists their groups.
    pub const ALL: [Self; 7] = [
        Self::Table,
        Self::View,
        Self::MaterializedView,
        Self::ForeignTable,
        Self::Function,
        Self::Sequence,
        Self::Key,
    ];

    #[must_use]
    pub const fn plural(self) -> &'static str {
        match self {
            Self::Table => "Tables",
            Self::View => "Views",
            Self::MaterializedView => "Materialized views",
            Self::ForeignTable => "Foreign tables",
            Self::Function => "Functions",
            Self::Sequence => "Sequences",
            Self::Key => "Keys",
        }
    }

    /// Whether rows can be read from it with a `SELECT`.
    #[must_use]
    pub const fn is_relation(self) -> bool {
        matches!(
            self,
            Self::Table | Self::View | Self::MaterializedView | Self::ForeignTable
        )
    }
}

/// A named object inside a schema.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DbObject {
    pub schema: String,
    pub name: String,
    pub kind: ObjectKind,
    pub comment: Option<String>,
    /// Estimated rows for relations, when the server keeps statistics; for
    /// a key, its elements.
    pub estimated_rows: Option<i64>,
    /// A key's type, such as `hash`: its icon, and how a data view reads it.
    pub subtype: Option<String>,
    /// Shown on hover, such as `hash · 120 fields · TTL 3600 s`.
    pub detail: Option<String>,
}

/// A page of a schema's objects, as drivers that are not SQL list them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EntryPage {
    pub objects: Vec<DbObject>,
    /// Passed back to read on; `None` at the end.
    pub next: Option<String>,
}

/// A column of a relation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub default: Option<String>,
    pub primary_key: bool,
    pub comment: Option<String>,
}

/// A foreign key: `columns` of `schema.table` point at `ref_columns` of
/// `ref_schema.ref_table`, pair by pair.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForeignKey {
    pub name: String,
    pub schema: String,
    pub table: String,
    pub columns: Vec<String>,
    pub ref_schema: String,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
}

/// What the server says about itself after connecting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerInfo {
    /// Product and version, such as `PostgreSQL 17.2`.
    pub version: String,
    /// The database the session is connected to.
    pub database: String,
    pub user: String,
}
