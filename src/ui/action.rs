//! What views ask the app to do. Views only read state and return actions;
//! the app applies them in one place, after drawing.

use violet_core::{ConnectionId, ConnectionProfile, Secret};

use super::workspace::TabId;

/// Which part of an editor to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunScope {
    /// The selection, else the statement under the cursor.
    Statement,
    /// Every statement in the editor.
    Script,
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
    Refresh(ConnectionId),
    LoadObjects(ConnectionId, String),
    LoadColumns(ConnectionId, String, String),
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
    Cancel(TabId),
    CloseTab(TabId),
    SelectTab(TabId),
    SetTabConnection(TabId, Option<ConnectionId>),
    SetTheme(egui::ThemePreference),
    ToggleSidebar,
    Copy(String),
}
