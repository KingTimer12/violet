//! What views ask the app to do. Views only read state and return actions;
//! the app applies them in one place, after drawing.

use std::path::PathBuf;

use violet_core::{
    ConnectionId, ConnectionProfile, DriverId, ObjectKind, ObjectRef, Secret, TaskKind,
};

use super::workspace::TabId;
use crate::completion::Need;

/// Which part of an editor to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunScope {
    /// The selection, else the statement under the cursor.
    Statement,
    /// Every statement in the editor.
    Script,
    /// A data tab's rows, read again from the first page.
    Data,
    /// How the server would run the statement under the cursor.
    Explain,
    /// The statement under the cursor run and timed by the server; reads
    /// only.
    ExplainAnalyze,
}

#[derive(Debug)]
pub enum Action {
    NewConnection,
    /// Opens the connection form filled from a connection string.
    NewConnectionFromUri(String),
    EditConnection(ConnectionId),
    DuplicateConnection(ConnectionId),
    AskDeleteConnection(ConnectionId),
    DeleteConnection(ConnectionId),
    /// Files a connection in a folder; empty for the top level.
    MoveToFolder(ConnectionId, String),
    /// Deletes a connection folder, moving what it holds up a level.
    DeleteFolder(String),
    Connect(ConnectionId),
    /// Disconnects, asking first about an open transaction.
    Disconnect(ConnectionId),
    /// Commits, rolls back, or switches auto-commit; leaving manual commit
    /// with a transaction open asks first.
    Transact(ConnectionId, violet_app::TxCommand),
    /// Opens a database of a connection that shows them all, in a session
    /// of its own.
    OpenDatabase(ConnectionId, String),
    /// Opens the backup or restore dialog for a database of a connection
    /// that shows them all, whether or not it is open.
    DatabaseTask(ConnectionId, String, TaskKind),
    Refresh(ConnectionId),
    LoadObjects(ConnectionId, String),
    LoadColumns(ConnectionId, String, String),
    /// Catalog data completion is waiting for; what is already loading or
    /// loaded is skipped.
    LoadForCompletion(ConnectionId, Vec<Need>),
    SaveProfile {
        profile: ConnectionProfile,
        password: Option<Secret>,
        connect: bool,
    },
    TestProfile {
        profile: ConnectionProfile,
        password: Option<Secret>,
    },
    ConnectWithPassword(ConnectionId, Secret),
    OpenEditor {
        connection: Option<ConnectionId>,
        sql: String,
    },
    OpenData {
        connection: ConnectionId,
        schema: String,
        relation: String,
        /// A key's type, for a key-value store.
        subtype: Option<String>,
    },
    /// Lists a key-value database's keys matching its glob: from the
    /// first page, or the next one.
    LoadKeys(ConnectionId, String, bool),
    /// Asks how to change a key: its TTL, its name, or to delete it.
    KeyDialog {
        connection: ConnectionId,
        schema: String,
        key: String,
        op: super::key_flow::KeyOp,
    },
    /// Runs commands in a key-value database for the tree, then lists its
    /// keys again; `done` is the notice shown when they worked.
    RunInSchema {
        connection: ConnectionId,
        schema: String,
        script: String,
        done: String,
    },
    Run(TabId, RunScope),
    /// Runs again, the user having accepted statements without `WHERE`.
    RunConfirmed(TabId, RunScope),
    /// Opens the editor for a cell of a tab's rows.
    EditCell(TabId, usize, usize),
    /// Changes a cell in the grid, not on the server yet; `None` is NULL.
    SetCell(TabId, usize, usize, Option<String>),
    RevertCell(TabId, usize, usize),
    DiscardEdits(TabId),
    /// Adds an empty row to a tab's grid, inserted when its changes are
    /// saved.
    AddRow(TabId),
    /// Marks rows of a tab's grid to delete; an added row just goes.
    DeleteRows(TabId, Vec<usize>),
    /// Keeps rows marked to delete after all.
    RestoreRows(TabId, Vec<usize>),
    /// Shows the statements that save a tab's changed cells.
    ReviewEdits(TabId),
    /// Writes them, the user having reviewed them.
    ApplyEdits(TabId),
    /// Opens the keyboard shortcuts dialog.
    OpenShortcuts,
    /// Changes a command's shortcut; `None` puts its default back.
    SetShortcut(super::shortcuts::Command, Option<egui::KeyboardShortcut>),
    /// Formats an editor's selection, or its whole script.
    FormatSql(TabId),
    /// Opens an editor's find bar; `true` with its replace field.
    Find(TabId, bool),
    /// Saves an editor: to its file, else as a script of its connection's
    /// database, else wherever the user picks.
    SaveScript(TabId),
    /// Saves an editor to a file the user picks.
    SaveScriptAs(TabId),
    /// Saves an editor as a script of its connection's database, named.
    SaveScriptNamed(TabId, String),
    /// Opens a `.sql` file the user picks.
    OpenScriptFile,
    /// Opens a script, on the database it was saved for.
    OpenScript(PathBuf),
    /// Reads the shown result again, skipping the cache.
    RefreshTab(TabId),
    /// Applies a data tab's filter box.
    ApplyFilter(TabId),
    ClearFilter(TabId),
    /// Sorts a data tab by a column, cycling the direction.
    /// Reads a tab's rows again sorted on the server by the column at this
    /// index, descending or not; `None` takes the sort off.
    SortBy(TabId, usize, Option<bool>),
    FetchMore(TabId),
    /// Sets how many rows a paged result reads at a time.
    SetFetchSize(u64),
    FetchAll(TabId),
    CountRows(TabId),
    Cancel(TabId),
    CloseTab(TabId),
    /// Reads a tab's rows again on an interval; `None` stops it.
    SetAutoRefresh(TabId, Option<std::time::Duration>),
    /// Asks for a new name for a tab or a relation.
    AskRename(super::rename_flow::RenameTarget),
    /// The name typed in the rename prompt.
    RenameTo(String),
    /// Asks to confirm deleting a database of a connection.
    AskDropDatabase(ConnectionId, String),
    DropDatabase,
    /// Closes several tabs at once, asking once about unsaved scripts.
    CloseTabs(Vec<TabId>),
    SelectTab(TabId),
    SetTabConnection(TabId, Option<ConnectionId>),
    SetTheme(egui::ThemePreference),
    OpenProviders,
    /// Fetches the provider catalogue again.
    RefreshProviders,
    /// Installs or updates a provider.
    InstallProvider(DriverId),
    RemoveProvider(DriverId),
    ToggleSidebar,
    Copy(String),
    /// Opens the source of a function, procedure or view in an editor.
    OpenDefinition {
        connection: ConnectionId,
        schema: String,
        name: String,
        kind: ObjectKind,
    },
    /// Opens a trigger's definition in an editor.
    OpenTrigger {
        connection: ConnectionId,
        schema: String,
        table: String,
        name: String,
    },
    /// Lists a folder (triggers, roles, extensions) of a session's
    /// database.
    LoadFolder(ConnectionId, super::state::Folder),
    /// Opens the dialog picking which schemas a session's database shows.
    ChooseSchemas(ConnectionId),
    /// Opens the dialog picking which databases a connection shows.
    ChooseDatabases(ConnectionId),
    /// Keeps the schemas picked for a `connection/database`, or the
    /// databases for a connection; `None` shows them all.
    SetTreeFilter(
        super::state::TreeFilter,
        String,
        Option<std::collections::BTreeSet<String>>,
    ),
    /// Opens advanced copy for a tab's picked cells.
    AdvancedCopy(TabId),
    /// The format and header choice advanced copy starts with next time.
    RememberCopy(crate::export::Format, bool),
    /// Writes the shown rows of a tab to a file in a format.
    Export(TabId, crate::export::Format),
    /// Opens, beside a tab's grid, the row a foreign key cell points at:
    /// the tab's row and column.
    FollowKey(TabId, usize, usize),
    /// Follows a foreign key from the rows shown beside a tab's grid: the
    /// step the cell is in, its row and its column.
    FollowNestedKey(TabId, usize, usize, usize),
    /// Goes back to a step of the references beside a tab's grid.
    BackToReference(TabId, usize),
    /// Closes the referenced rows beside the grid.
    CloseReference,
    OpenHistory,
    ClearHistory,
    /// Puts SQL at the active editor's cursor.
    InsertSql(String),
    /// Opens the backup or restore dialog, with these objects picked.
    OpenTask {
        connection: ConnectionId,
        kind: TaskKind,
        objects: Vec<ObjectRef>,
    },
    /// Lets a provider run its client programs.
    TrustTools(DriverId),
    /// The open task dialog's programs, command and input changed.
    TaskFindPrograms,
    TaskPreview,
    TaskInspect(PathBuf),
    TaskStart,
    TaskCancel,
    TaskAgain,
    TaskClose,
    /// Downloads a provider's client programs again.
    RetryTools(DriverId),
    /// Shows a file in the system's file manager.
    Reveal(PathBuf),
    /// Quits so the downloaded update installs, then starts it.
    RestartToUpdate,
    /// Checks for, and downloads, the update again after a failure.
    RetryUpdate,
    /// Opens a web page, such as a release's.
    OpenUrl(String),
}
