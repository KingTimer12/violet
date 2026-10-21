//! Keyboard shortcuts. `COMMAND` is ⌘ on macOS and Ctrl elsewhere.

use egui::{Key, KeyboardShortcut, Modifiers};

const COMMAND_SHIFT: Modifiers = Modifiers {
    alt: false,
    ctrl: false,
    shift: true,
    mac_cmd: false,
    command: true,
};

pub const RUN_STATEMENT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Enter);
pub const RUN_SCRIPT: KeyboardShortcut = KeyboardShortcut::new(COMMAND_SHIFT, Key::Enter);
pub const CANCEL: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Period);
pub const NEW_EDITOR: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::T);
pub const CLOSE_TAB: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::W);
pub const NEW_CONNECTION: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::N);
pub const TOGGLE_SIDEBAR: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::B);
pub const EXPLAIN: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::E);
pub const EXPLAIN_ANALYZE: KeyboardShortcut = KeyboardShortcut::new(COMMAND_SHIFT, Key::E);
pub const SAVE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::S);
pub const SAVE_AS: KeyboardShortcut = KeyboardShortcut::new(COMMAND_SHIFT, Key::S);
pub const OPEN: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::O);
pub const HISTORY: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Y);
pub const REFRESH: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::F5);
pub const RENAME: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::F2);
pub const EDIT_CONNECTION: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::F4);
