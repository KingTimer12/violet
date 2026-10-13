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
}

impl ObjectKind {
    /// Kinds in the order the tree lists their groups.
    pub const ALL: [Self; 6] = [
        Self::Table,
        Self::View,
        Self::MaterializedView,
        Self::ForeignTable,
        Self::Function,
        Self::Sequence,
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
    /// Estimated rows for relations, when the server keeps statistics.
    pub estimated_rows: Option<i64>,
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

/// What the server says about itself after connecting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerInfo {
    /// Product and version, such as `PostgreSQL 17.2`.
    pub version: String,
    /// The database the session is connected to.
    pub database: String,
    pub user: String,
}
