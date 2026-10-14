//! Backups and restores through a database's own client programs, such as
//! `pg_dump`. A driver describes its tasks and builds their command lines;
//! the application finds the program and runs it.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{DbError, ErrorKind, Result};

/// Programs a task may never name: shells, interpreters and launchers run
/// whatever their arguments say, so a provider could escape its sandbox
/// through them.
const DENIED_PROGRAMS: &[&str] = &[
    "sh",
    "bash",
    "zsh",
    "fish",
    "dash",
    "ksh",
    "csh",
    "tcsh",
    "nu",
    "cmd",
    "command",
    "powershell",
    "pwsh",
    "wsl",
    "python",
    "python3",
    "perl",
    "ruby",
    "node",
    "deno",
    "bun",
    "php",
    "lua",
    "tclsh",
    "osascript",
    "env",
    "sudo",
    "su",
    "doas",
    "open",
    "xdg-open",
    "start",
    "explorer",
    "rundll32",
    "mshta",
    "wscript",
    "cscript",
    "curl",
    "wget",
    "ssh",
    "scp",
    "nc",
    "ncat",
    "rm",
    "mv",
    "cp",
    "dd",
    "chmod",
    "chown",
    "launchctl",
    "crontab",
    "at",
    "xargs",
    "find",
    "awk",
    "sed",
    "git",
    "make",
    "docker",
];

/// Environment variables a command may never set: they change which code
/// a program loads or what a shell runs.
const DENIED_ENV: &[&str] = &[
    "PATH",
    "PATHEXT",
    "HOME",
    "USER",
    "SHELL",
    "IFS",
    "ENV",
    "BASH_ENV",
    "PROMPT_COMMAND",
    "TMPDIR",
    "TEMP",
    "TMP",
    "COMSPEC",
    "SYSTEMROOT",
    "NODE_OPTIONS",
    "PERL5OPT",
    "PERL5LIB",
    "PYTHONPATH",
    "PYTHONSTARTUP",
    "RUBYOPT",
    "EDITOR",
    "VISUAL",
    "PAGER",
    "PSQL_PAGER",
    "PSQLRC",
    "PSQL_EDITOR",
];

/// Prefixes of denied environment variables.
const DENIED_ENV_PREFIXES: &[&str] = &["LD_", "DYLD_", "MALLOC", "GCONV", "LOCPATH"];

/// How much of a file [`crate::Driver::inspect_input`] is shown.
pub const INSPECT_HEAD: usize = 4096;
/// How many of a folder's entries it is shown.
pub const INSPECT_ENTRIES: usize = 64;

const MAX_FIELDS: usize = 64;
const MAX_PROGRAMS: usize = 8;
const MAX_LOCATIONS: usize = 16;
const MAX_ARGS: usize = 512;
const MAX_ARG: usize = 32 << 10;
const MAX_ENV: usize = 32;

/// What a task does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskKind {
    Backup,
    Restore,
}

impl TaskKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Backup => "Backup",
            Self::Restore => "Restore",
        }
    }
}

impl fmt::Display for TaskKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// One of a choice field's values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    pub value: String,
    pub label: String,
}

/// What a setting holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FieldKind {
    /// `true` or `false`.
    Flag,
    Text,
    /// A whole number from `min` to `max`.
    Number {
        min: i64,
        max: i64,
    },
    Choice(Vec<Choice>),
}

/// Holds when `field` has one of `values`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Condition {
    pub field: String,
    pub values: Vec<String>,
}

impl Condition {
    #[must_use]
    pub fn holds(&self, settings: &TaskSettings) -> bool {
        settings
            .get(&self.field)
            .is_some_and(|value| self.values.iter().any(|v| v == value))
    }
}

/// A task setting, as the dialog shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub id: String,
    pub label: String,
    pub help: String,
    /// The group it is shown in.
    pub section: String,
    pub kind: FieldKind,
    pub default: String,
    /// Shown only while this holds.
    pub when: Option<Condition>,
}

impl Field {
    /// Whether the field applies with these settings.
    #[must_use]
    pub fn visible(&self, settings: &TaskSettings) -> bool {
        self.when.as_ref().is_none_or(|when| when.holds(settings))
    }

    /// `value` if the field can hold it.
    #[must_use]
    pub fn accepts(&self, value: &str) -> bool {
        match &self.kind {
            FieldKind::Flag => value == "true" || value == "false",
            FieldKind::Text => !value.contains('\0'),
            FieldKind::Number { min, max } => value
                .parse::<i64>()
                .is_ok_and(|n| (*min..=*max).contains(&n)),
            FieldKind::Choice(choices) => choices.iter().any(|c| c.value == value),
        }
    }
}

/// Field values by id.
pub type TaskSettings = BTreeMap<String, String>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PathKind {
    File,
    Directory,
}

/// What a backup writes or a restore reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub when: Option<Condition>,
    pub kind: PathKind,
    /// Without the dot; empty for any.
    pub extension: String,
}

/// A backup or restore a driver offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Task {
    pub kind: TaskKind,
    /// Programs its commands may run, by file name.
    pub programs: Vec<String>,
    /// Where the programs usually live; `*` stands for one folder name.
    pub locations: Vec<String>,
    /// Makes a program print its version.
    pub version_arg: Option<String>,
    pub fields: Vec<Field>,
    /// The user picks schemas and relations.
    pub objects: bool,
    pub targets: Vec<Target>,
}

impl Task {
    /// Checks a description that came from a provider: names, sizes, and
    /// that every default and condition makes sense.
    pub fn check(&self) -> Result<()> {
        let bad = |what: String| Err(invalid(format!("{} task: {what}", self.kind)));
        if self.programs.is_empty() || self.programs.len() > MAX_PROGRAMS {
            return bad(format!("needs 1 to {MAX_PROGRAMS} programs"));
        }
        for program in &self.programs {
            if !program_name_ok(program) {
                return bad(format!("{program:?} is not a program it may run"));
            }
        }
        if self.locations.len() > MAX_LOCATIONS {
            return bad("too many locations".into());
        }
        for location in &self.locations {
            if !location_ok(location) {
                return bad(format!("{location:?} is not an absolute folder"));
            }
        }
        if let Some(arg) = &self.version_arg
            && !(arg.starts_with('-') && arg.len() <= 32 && plain_text(arg))
        {
            return bad(format!("{arg:?} is not a version option"));
        }
        if self.fields.len() > MAX_FIELDS {
            return bad("too many settings".into());
        }
        let mut ids = std::collections::BTreeSet::new();
        for field in &self.fields {
            let id_ok = !field.id.is_empty()
                && field.id.len() <= 32
                && field
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
            if !id_ok || !ids.insert(field.id.as_str()) {
                return bad(format!("{:?} is not a usable setting id", field.id));
            }
            let short = |text: &str, max: usize| text.chars().count() <= max && plain_text(text);
            if !short(&field.label, 80) || !short(&field.help, 400) || !short(&field.section, 40) {
                return bad(format!("setting {:?} has an unusable label", field.id));
            }
            match &field.kind {
                FieldKind::Number { min, max } if min > max => {
                    return bad(format!("setting {:?} has an empty range", field.id));
                }
                FieldKind::Choice(choices)
                    if choices.is_empty()
                        || choices.len() > 32
                        || choices.iter().any(|c| {
                            c.value.len() > 64 || !short(&c.label, 80) || !plain_text(&c.value)
                        }) =>
                {
                    return bad(format!("setting {:?} has unusable choices", field.id));
                }
                _ => {}
            }
            if !field.accepts(&field.default) {
                return bad(format!("setting {:?} has an invalid default", field.id));
            }
        }
        let conditions = self
            .fields
            .iter()
            .filter_map(|field| field.when.as_ref())
            .chain(
                self.targets
                    .iter()
                    .filter_map(|target| target.when.as_ref()),
            );
        for condition in conditions {
            if !ids.contains(condition.field.as_str()) {
                return bad(format!("a condition names {:?}", condition.field));
            }
        }
        if self.targets.len() > 16
            || self.targets.iter().any(|target| {
                target.extension.len() > 16
                    || !target.extension.bytes().all(|b| b.is_ascii_alphanumeric())
            })
        {
            return bad("unusable targets".into());
        }
        Ok(())
    }

    /// Every field at its default.
    #[must_use]
    pub fn defaults(&self) -> TaskSettings {
        self.fields
            .iter()
            .map(|field| (field.id.clone(), field.default.clone()))
            .collect()
    }

    /// `settings` with every field present and valid: missing, unknown and
    /// unusable values give way to defaults.
    #[must_use]
    pub fn normalize(&self, settings: &TaskSettings) -> TaskSettings {
        self.fields
            .iter()
            .map(|field| {
                let value = settings
                    .get(&field.id)
                    .filter(|value| field.accepts(value))
                    .unwrap_or(&field.default);
                (field.id.clone(), value.clone())
            })
            .collect()
    }

    /// What the task writes or reads with these settings.
    #[must_use]
    pub fn target(&self, settings: &TaskSettings) -> Target {
        self.targets
            .iter()
            .find(|target| target.when.as_ref().is_none_or(|when| when.holds(settings)))
            .cloned()
            .unwrap_or(Target {
                when: None,
                kind: PathKind::File,
                extension: String::new(),
            })
    }
}

impl ToolCommand {
    /// Checks a command a provider built for `task`: a program the task
    /// declared, bounded arguments, and nothing in the environment that
    /// changes what runs.
    pub fn check(&self, task: &Task) -> Result<()> {
        if !task.programs.contains(&self.program) || !program_name_ok(&self.program) {
            return Err(invalid(format!(
                "the command runs {:?}, which the {} task does not declare",
                self.program, task.kind
            )));
        }
        if self.args.len() > MAX_ARGS
            || self
                .args
                .iter()
                .any(|arg| arg.len() > MAX_ARG || arg.contains('\0'))
        {
            return Err(invalid("the command's arguments are unusable".into()));
        }
        if self.env.len() > MAX_ENV {
            return Err(invalid("the command sets too many variables".into()));
        }
        for name in self
            .env
            .iter()
            .map(|(name, _)| name)
            .chain(self.password_env.as_ref())
        {
            if !env_name_ok(name) {
                return Err(invalid(format!("the command may not set {name:?}")));
            }
        }
        if self
            .env
            .iter()
            .any(|(_, value)| value.contains('\0') || value.len() > MAX_ARG)
        {
            return Err(invalid("the command's variables are unusable".into()));
        }
        if self.stdout_to_path && self.stdin_from_path {
            return Err(invalid("the command both reads and writes the path".into()));
        }
        Ok(())
    }
}

fn invalid(message: String) -> DbError {
    DbError::new(ErrorKind::Provider, format!("Unusable task: {message}"))
}

/// No control characters.
fn plain_text(text: &str) -> bool {
    !text.chars().any(char::is_control)
}

/// A bare file name a task may run.
#[must_use]
pub fn program_name_ok(name: &str) -> bool {
    let stem = name
        .strip_suffix(".exe")
        .unwrap_or(name)
        .to_ascii_lowercase();
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with(['.', '-'])
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'+'))
        && !DENIED_PROGRAMS.contains(&stem.as_str())
}

/// An absolute folder without `..`, with at most one `*`, standing for
/// part of one folder name.
fn location_ok(location: &str) -> bool {
    let unix = location.starts_with('/');
    let bytes = location.as_bytes();
    let windows = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/');
    (unix || windows)
        && location.len() <= 256
        && plain_text(location)
        && location.split(['/', '\\']).all(|part| part != "..")
        && location.matches('*').count() <= 1
}

/// An environment variable a command may set.
fn env_name_ok(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && !name.as_bytes()[0].is_ascii_digit()
        && !DENIED_ENV.contains(&upper.as_str())
        && !DENIED_ENV_PREFIXES
            .iter()
            .any(|prefix| upper.starts_with(prefix))
}

/// A schema, or a relation in one.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ObjectRef {
    pub schema: String,
    pub name: Option<String>,
}

/// A task to build a command for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskRequest {
    pub kind: TaskKind,
    pub settings: TaskSettings,
    /// What to back up; empty for everything.
    pub objects: Vec<ObjectRef>,
    /// The file or folder to write or read, absolute.
    pub path: String,
}

/// A program to run, as a driver asks for it. Untrusted until the
/// application checks it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCommand {
    /// A program of the task's, by name.
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// The variable that carries the password.
    pub password_env: Option<String>,
    /// Write standard output to the path.
    pub stdout_to_path: bool,
    /// Feed the path to standard input.
    pub stdin_from_path: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task() -> Task {
        Task {
            kind: TaskKind::Backup,
            programs: vec!["pg_dump".into()],
            locations: Vec::new(),
            version_arg: None,
            fields: vec![
                Field {
                    id: "format".into(),
                    label: "Format".into(),
                    help: String::new(),
                    section: "Output".into(),
                    kind: FieldKind::Choice(vec![
                        Choice {
                            value: "custom".into(),
                            label: "Custom".into(),
                        },
                        Choice {
                            value: "directory".into(),
                            label: "Directory".into(),
                        },
                    ]),
                    default: "custom".into(),
                    when: None,
                },
                Field {
                    id: "jobs".into(),
                    label: "Jobs".into(),
                    help: String::new(),
                    section: "Output".into(),
                    kind: FieldKind::Number { min: 1, max: 8 },
                    default: "1".into(),
                    when: Some(Condition {
                        field: "format".into(),
                        values: vec!["directory".into()],
                    }),
                },
            ],
            objects: true,
            targets: vec![
                Target {
                    when: Some(Condition {
                        field: "format".into(),
                        values: vec!["directory".into()],
                    }),
                    kind: PathKind::Directory,
                    extension: String::new(),
                },
                Target {
                    when: None,
                    kind: PathKind::File,
                    extension: "dump".into(),
                },
            ],
        }
    }

    #[test]
    fn settings_fall_back_to_defaults() {
        let task = task();
        let mut settings = TaskSettings::new();
        settings.insert("format".into(), "zip".into());
        settings.insert("jobs".into(), "99".into());
        settings.insert("other".into(), "x".into());
        let normalized = task.normalize(&settings);
        assert_eq!(normalized, task.defaults());
        assert!(!task.fields[1].visible(&normalized));
        assert_eq!(task.target(&normalized).kind, PathKind::File);

        settings.insert("format".into(), "directory".into());
        settings.insert("jobs".into(), "4".into());
        let normalized = task.normalize(&settings);
        assert_eq!(normalized["jobs"], "4");
        assert!(task.fields[1].visible(&normalized));
        assert_eq!(task.target(&normalized).kind, PathKind::Directory);
        assert!(task.check().is_ok());
    }

    #[test]
    fn tasks_cannot_name_shells_or_odd_places() {
        let mut bad = task();
        bad.programs = vec!["bash".into()];
        assert!(bad.check().is_err());
        bad.programs = vec!["../pg_dump".into()];
        assert!(bad.check().is_err());
        bad.programs = vec!["PowerShell.exe".into()];
        assert!(bad.check().is_err());

        let mut bad = task();
        bad.locations = vec!["relative/bin".into()];
        assert!(bad.check().is_err());
        bad.locations = vec!["/usr/lib/../../etc".into()];
        assert!(bad.check().is_err());
        bad.locations = vec![
            "/usr/lib/postgresql/*/bin".into(),
            r"C:\Program Files\PostgreSQL\*\bin".into(),
        ];
        assert!(bad.check().is_ok());
        bad.locations = vec!["/usr/*/postgresql@*/bin".into()];
        assert!(bad.check().is_err());

        let mut bad = task();
        bad.fields[1].default = "0".into();
        assert!(bad.check().is_err());
        let mut bad = task();
        bad.fields[1].when.as_mut().expect("when").field = "nope".into();
        assert!(bad.check().is_err());
    }

    #[test]
    fn commands_stay_within_their_task() {
        let task = task();
        let mut command = ToolCommand {
            program: "pg_dump".into(),
            args: vec!["--file".into(), "/tmp/x".into()],
            env: vec![("PGSSLMODE".into(), "require".into())],
            password_env: Some("PGPASSWORD".into()),
            stdout_to_path: false,
            stdin_from_path: false,
        };
        assert!(command.check(&task).is_ok());
        command.program = "psql".into();
        assert!(command.check(&task).is_err());
        command.program = "pg_dump".into();
        for name in [
            "PATH",
            "DYLD_INSERT_LIBRARIES",
            "ld_preload",
            "A=B",
            "PSQLRC",
        ] {
            command.env = vec![(name.into(), "x".into())];
            assert!(command.check(&task).is_err(), "{name}");
        }
        command.env.clear();
        command.password_env = Some("LD_PRELOAD".into());
        assert!(command.check(&task).is_err());
    }
}
