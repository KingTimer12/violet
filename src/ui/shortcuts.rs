//! Keyboard shortcuts. `COMMAND` is ⌘ on macOS and Ctrl elsewhere.
//!
//! Each command has a default the user can change; the changes are kept in
//! the settings by command name, as text such as `Command+Shift+F`.

use std::collections::BTreeMap;
use std::sync::RwLock;

use egui::{InputState, Key, KeyboardShortcut, Modifiers};

const COMMAND_SHIFT: Modifiers = Modifiers {
    alt: false,
    ctrl: false,
    shift: true,
    mac_cmd: false,
    command: true,
};

/// What a shortcut does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    RunStatement,
    RunScript,
    Cancel,
    Explain,
    ExplainAnalyze,
    NewEditor,
    CloseTab,
    NewConnection,
    ToggleSidebar,
    Save,
    SaveAs,
    Open,
    History,
    Find,
    Replace,
    Format,
    Comment,
}

impl Command {
    /// Every command, in the order the shortcuts dialog lists them.
    pub const ALL: [Self; 17] = [
        Self::RunStatement,
        Self::RunScript,
        Self::Cancel,
        Self::Explain,
        Self::ExplainAnalyze,
        Self::NewEditor,
        Self::CloseTab,
        Self::NewConnection,
        Self::ToggleSidebar,
        Self::Save,
        Self::SaveAs,
        Self::Open,
        Self::History,
        Self::Find,
        Self::Replace,
        Self::Format,
        Self::Comment,
    ];

    /// The name its shortcut is kept under in the settings.
    pub const fn key(self) -> &'static str {
        match self {
            Self::RunStatement => "run-statement",
            Self::RunScript => "run-script",
            Self::Cancel => "cancel",
            Self::Explain => "explain",
            Self::ExplainAnalyze => "explain-analyze",
            Self::NewEditor => "new-editor",
            Self::CloseTab => "close-tab",
            Self::NewConnection => "new-connection",
            Self::ToggleSidebar => "toggle-sidebar",
            Self::Save => "save",
            Self::SaveAs => "save-as",
            Self::Open => "open",
            Self::History => "history",
            Self::Find => "find",
            Self::Replace => "replace",
            Self::Format => "format",
            Self::Comment => "comment",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::RunStatement => "Run statement under cursor",
            Self::RunScript => "Run whole script",
            Self::Cancel => "Cancel running query",
            Self::Explain => "Explain plan",
            Self::ExplainAnalyze => "Explain analyze",
            Self::NewEditor => "New SQL editor",
            Self::CloseTab => "Close tab",
            Self::NewConnection => "New connection",
            Self::ToggleSidebar => "Toggle sidebar",
            Self::Save => "Save script",
            Self::SaveAs => "Save script as",
            Self::Open => "Open script",
            Self::History => "Query history",
            Self::Find => "Find",
            Self::Replace => "Replace",
            Self::Format => "Format SQL",
            Self::Comment => "Comment lines out",
        }
    }

    pub const fn default_shortcut(self) -> KeyboardShortcut {
        let (modifiers, key) = match self {
            Self::RunStatement => (Modifiers::COMMAND, Key::Enter),
            Self::RunScript => (COMMAND_SHIFT, Key::Enter),
            Self::Cancel => (Modifiers::COMMAND, Key::Period),
            Self::Explain => (Modifiers::COMMAND, Key::E),
            Self::ExplainAnalyze => (COMMAND_SHIFT, Key::E),
            Self::NewEditor => (Modifiers::COMMAND, Key::T),
            Self::CloseTab => (Modifiers::COMMAND, Key::W),
            Self::NewConnection => (Modifiers::COMMAND, Key::N),
            Self::ToggleSidebar => (Modifiers::COMMAND, Key::B),
            Self::Save => (Modifiers::COMMAND, Key::S),
            Self::SaveAs => (COMMAND_SHIFT, Key::S),
            Self::Open => (Modifiers::COMMAND, Key::O),
            Self::History => (Modifiers::COMMAND, Key::Y),
            Self::Find => (Modifiers::COMMAND, Key::F),
            Self::Replace => (Modifiers::COMMAND, Key::H),
            Self::Format => (COMMAND_SHIFT, Key::F),
            Self::Comment => (Modifiers::COMMAND, Key::Slash),
        };
        KeyboardShortcut::new(modifiers, key)
    }
}

type Keymap = [KeyboardShortcut; Command::ALL.len()];

const DEFAULTS: Keymap = {
    let mut keymap = [KeyboardShortcut::new(Modifiers::NONE, Key::A); Command::ALL.len()];
    let mut index = 0;
    while index < keymap.len() {
        keymap[index] = Command::ALL[index].default_shortcut();
        index += 1;
    }
    keymap
};

/// The shortcuts in force. Global, as every view reads them, some inside
/// egui's input lock where the context cannot be read.
static KEYMAP: RwLock<Keymap> = RwLock::new(DEFAULTS);

/// `command`'s shortcut.
pub fn get(command: Command) -> KeyboardShortcut {
    KEYMAP.read().map_or_else(
        |_| command.default_shortcut(),
        |keymap| keymap[command as usize],
    )
}

/// Puts the user's shortcuts, by command name, over the defaults. Names
/// or shortcuts not understood are left out.
pub fn apply(changed: &BTreeMap<String, String>) {
    let mut keymap = DEFAULTS;
    for command in Command::ALL {
        if let Some(shortcut) = changed.get(command.key()).and_then(|text| parse(text)) {
            keymap[command as usize] = shortcut;
        }
    }
    if let Ok(mut current) = KEYMAP.write() {
        *current = keymap;
    }
}

/// Whether `command`'s shortcut was pressed, consuming it. Shift and Alt
/// must match too, so `Command+Shift+F` never runs what `Command+F` does.
pub fn consume(input: &mut InputState, command: Command) -> bool {
    let shortcut = get(command);
    let mut found = false;
    input.events.retain(|event| {
        let hit = matches!(
            event,
            egui::Event::Key { key, modifiers, pressed: true, .. }
                if *key == shortcut.logical_key && same(*modifiers, shortcut.modifiers)
        );
        found |= hit;
        !hit
    });
    // With Alt, or no Ctrl, the keys also type their letter: it is the
    // shortcut's, not the text's.
    if found {
        input
            .events
            .retain(|event| !matches!(event, egui::Event::Text(_)));
    }
    found
}

/// Whether two shortcuts' modifiers are the same, as [`consume`] tells.
pub fn same(a: Modifiers, b: Modifiers) -> bool {
    a.command == b.command && a.shift == b.shift && a.alt == b.alt
}

/// The command `shortcut` is taken by, if any.
pub fn taken_by(shortcut: KeyboardShortcut) -> Option<Command> {
    Command::ALL.into_iter().find(|command| {
        let current = get(*command);
        current.logical_key == shortcut.logical_key && same(current.modifiers, shortcut.modifiers)
    })
}

/// `shortcut` as the settings keep it: `Command+Shift+F`.
pub fn to_text(shortcut: KeyboardShortcut) -> String {
    let mut parts = Vec::new();
    let modifiers = shortcut.modifiers;
    if modifiers.command {
        parts.push("Command");
    }
    if modifiers.alt {
        parts.push("Alt");
    }
    if modifiers.shift {
        parts.push("Shift");
    }
    parts.push(shortcut.logical_key.name());
    parts.join("+")
}

/// Whether `key` is Shift, Ctrl, Alt or Super itself, which egui reports
/// as keys too: held with others, never a shortcut's key.
pub fn is_modifier(key: Key) -> bool {
    matches!(
        key,
        Key::ShiftLeft
            | Key::ShiftRight
            | Key::ControlLeft
            | Key::ControlRight
            | Key::AltLeft
            | Key::AltRight
            | Key::SuperLeft
            | Key::SuperRight
    )
}

/// Reads [`to_text`]'s form back; `Ctrl` and `Cmd` stand for `Command`.
pub fn parse(text: &str) -> Option<KeyboardShortcut> {
    let mut parts: Vec<&str> = text.split('+').map(str::trim).collect();
    let key = Key::from_name(parts.pop()?).filter(|key| !is_modifier(*key))?;
    let mut modifiers = Modifiers::NONE;
    for part in parts {
        match part.to_ascii_lowercase().as_str() {
            "command" | "ctrl" | "cmd" => modifiers.command = true,
            "alt" | "option" => modifiers.alt = true,
            "shift" => modifiers.shift = true,
            _ => return None,
        }
    }
    Some(KeyboardShortcut::new(modifiers, key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcuts_read_back_as_written() {
        for command in Command::ALL {
            let shortcut = command.default_shortcut();
            assert_eq!(parse(&to_text(shortcut)), Some(shortcut));
        }
        assert_eq!(
            parse("Ctrl+Alt+K"),
            Some(KeyboardShortcut::new(
                Modifiers {
                    alt: true,
                    ..Modifiers::COMMAND
                },
                Key::K
            ))
        );
        assert_eq!(parse("Hyper+K"), None);
        assert_eq!(parse("Alt+AltLeft"), None);
        assert_eq!(parse("Command+Nope"), None);
    }

    #[test]
    fn defaults_are_all_different() {
        for (index, a) in DEFAULTS.iter().enumerate() {
            for b in &DEFAULTS[index + 1..] {
                assert!(!(a.logical_key == b.logical_key && same(a.modifiers, b.modifiers)));
            }
        }
        assert!(
            Command::ALL
                .iter()
                .enumerate()
                .all(|(index, command)| *command as usize == index)
        );
    }
}
