//! Every icon Violet draws. Lucide outlines (ISC) and database logos from
//! Simple Icons (CC0), drawn in white and tinted at paint time; the
//! licences are in `assets/icons/LICENSE.txt`.

fastframe_icons::icons! {
    /// The interface's icons.
    pub enum Icon {
        prefix: "violet-icon-",
        directory: "../../assets/icons/",
        Logo => "logo",
        Database => "database",
        Table => "table",
        Columns => "columns",
        Key => "key",
        Schema => "layers",
        Folder => "folder",
        Plug => "plug",
        Power => "power",
        Terminal => "terminal",
        Stop => "stop",
        FileCode => "file-code",
        Function => "braces",
        Server => "server",
        Sequence => "hash",
        MaterializedView => "box",
        ForeignTable => "globe",
        Zap => "zap",
        Loader => "loader",
        Rows => "rows",
        ArrowUp => "arrow-up",
        ArrowDown => "arrow-down",
        ArrowUpRight => "arrow-up-right",
        Funnel => "funnel",
        ChevronsDown => "chevrons-down",
        Package => "package",
        Download => "download",
        View => lucide "eye",
        Check => lucide "check",
        CircleAlert => lucide "circle-alert",
        CircleCheck => lucide "circle-check",
        Info => lucide "info",
        Close => lucide "x",
        Plus => lucide "plus",
        Play => lucide "play",
        Search => lucide "search",
        Settings => lucide "settings",
        Sun => lucide "sun",
        Moon => lucide "moon",
        Monitor => lucide "monitor",
        Copy => lucide "copy",
        Trash => lucide "trash-2",
        Pencil => lucide "pencil",
        Refresh => lucide "refresh-cw",
        ChevronRight => lucide "chevron-right",
        ChevronDown => lucide "chevron-down",
        Ellipsis => lucide "ellipsis",
        Eye => lucide "eye",
        EyeOff => lucide "eye-off",
        Lock => lucide "lock",
        Clock => lucide "clock",
        PanelLeft => lucide "panel-left",
        Archive => "archive",
        ArchiveRestore => "archive-restore",
        FolderOpen => "folder-open",
        Save => "save",
        ShieldAlert => "shield-alert",
        Text => "type",
        List => "list",
        ListOrdered => "list-ordered",
        Shapes => "shapes",
        Activity => "activity",
        Code => "code",
        Link => "link",
        Keyboard => "keyboard",
        PostgreSql => "postgresql",
        MySql => "mysql",
        MariaDb => "mariadb",
        Sqlite => "sqlite",
    }
}

impl Icon {
    /// A key-value store's key, by its type.
    pub fn for_subtype(subtype: Option<&str>) -> Self {
        match subtype {
            Some("string") => Self::Text,
            Some("hash") => Self::Function,
            Some("list") => Self::List,
            Some("set") => Self::Shapes,
            Some("zset") => Self::ListOrdered,
            Some("stream") => Self::Activity,
            Some("json") => Self::Code,
            _ => Self::Key,
        }
    }

    /// The logo of the database a driver serves; a plain database for
    /// those without one.
    pub fn for_driver(id: &str) -> Self {
        let id = id.to_ascii_lowercase();
        if id.contains("postgres") {
            Self::PostgreSql
        } else if id.contains("maria") {
            Self::MariaDb
        } else if id.contains("mysql") {
            Self::MySql
        } else if id.contains("sqlite") {
            Self::Sqlite
        } else {
            Self::Database
        }
    }

    pub fn for_kind(kind: violet_core::ObjectKind) -> Self {
        use violet_core::ObjectKind;
        match kind {
            ObjectKind::Table => Self::Table,
            ObjectKind::View => Self::View,
            ObjectKind::MaterializedView => Self::MaterializedView,
            ObjectKind::ForeignTable => Self::ForeignTable,
            ObjectKind::Function => Self::Function,
            ObjectKind::Sequence => Self::Sequence,
            ObjectKind::Key => Self::Key,
        }
    }
}
