//! What views ask the app to do. Views only read state and return actions;
//! the app applies them in one place, after drawing.

use std::path::PathBuf;

use violet_core::{ConnectionId, ConnectionProfile, DriverId, ObjectRef, Secret, TaskKind};

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
    EditConnection(ConnectionId),
    DuplicateConnection(ConnectionId),
    AskDeleteConnection(ConnectionId),
    DeleteConnection(ConnectionId),
    Connect(ConnectionId),
    Disconnect(ConnectionId),
    /// Opens a database of a connection that shows them all, in a session
    /// of its own.
    OpenDatabase(ConnectionId, String),
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
    },
    Run(TabId, RunScope),
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
    Sort(TabId, String),
    FetchMore(TabId),
    FetchAll(TabId),
    CountRows(TabId),
    Cancel(TabId),
    CloseTab(TabId),
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
}
