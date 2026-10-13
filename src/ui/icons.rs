//! Every icon Violet draws. Lucide outlines (ISC), drawn in white and tinted
//! at paint time; the licence is in `assets/icons/LICENSE.txt`.

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
        Funnel => "funnel",
        ChevronsDown => "chevrons-down",
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
    }
}

impl Icon {
    pub fn for_kind(kind: violet_core::ObjectKind) -> Self {
        use violet_core::ObjectKind;
        match kind {
            ObjectKind::Table => Self::Table,
            ObjectKind::View => Self::View,
            ObjectKind::MaterializedView => Self::MaterializedView,
            ObjectKind::ForeignTable => Self::ForeignTable,
            ObjectKind::Function => Self::Function,
            ObjectKind::Sequence => Self::Sequence,
        }
    }
}
