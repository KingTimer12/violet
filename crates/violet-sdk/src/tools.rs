//! Backup and restore through the database's own client programs.
//!
//! A provider cannot start programs. It describes its [`Task`]s, and
//! [`crate::Provider::command`] turns the settings the user chose into a
//! [`ToolCommand`]; Violet finds the program, shows the command and runs it,
//! passing the password in [`ToolCommand::password_env`].
//!
//! ```ignore
//! fn tasks() -> Vec<Task> {
//!     vec![Task::new(TaskKind::Backup, &["litedump"])
//!         .locations(&["/opt/lite/*/bin"])
//!         .field(Field::choice("format", "Format", "Output", "sql", &[
//!             ("sql", "SQL script"),
//!             ("bin", "Binary"),
//!         ]))
//!         .field(Field::flag("data", "Include data", "Content", true))
//!         .field(Field::number("level", "Compression", "Output", 0..=9, 6)
//!             .when("format", &["bin"]))
//!         .target(Target::file("sql").when("format", &["sql"]))
//!         .target(Target::file("lite"))]
//! }
//! ```

pub use crate::bindings::exports::violet::provider::tools::{
    Choice, Condition, Field, FieldKind, NumberRange, ObjectRef, PathKind, Request, Target, Task,
    TaskKind, ToolCommand,
};

impl Task {
    /// A task running `programs`, with no settings yet.
    #[must_use]
    pub fn new(kind: TaskKind, programs: &[&str]) -> Self {
        Self {
            kind,
            programs: programs.iter().map(|p| (*p).to_owned()).collect(),
            locations: Vec::new(),
            version_arg: Some("--version".to_owned()),
            fields: Vec::new(),
            objects: false,
            targets: Vec::new(),
        }
    }

    /// Folders the programs usually live in; `*` stands for one folder
    /// name, such as a version.
    #[must_use]
    pub fn locations(mut self, locations: &[&str]) -> Self {
        self.locations
            .extend(locations.iter().map(|l| (*l).to_owned()));
        self
    }

    #[must_use]
    pub fn field(mut self, field: Field) -> Self {
        self.fields.push(field);
        self
    }

    /// Lets the user pick schemas and relations.
    #[must_use]
    pub fn with_objects(mut self) -> Self {
        self.objects = true;
        self
    }

    /// What it writes or reads; the first whose condition holds applies.
    #[must_use]
    pub fn target(mut self, target: Target) -> Self {
        self.targets.push(target);
        self
    }
}

impl Field {
    fn new(id: &str, label: &str, section: &str, kind: FieldKind, default: String) -> Self {
        Self {
            id: id.to_owned(),
            label: label.to_owned(),
            help: String::new(),
            section: section.to_owned(),
            kind,
            default,
            when: None,
        }
    }

    #[must_use]
    pub fn flag(id: &str, label: &str, section: &str, default: bool) -> Self {
        Self::new(id, label, section, FieldKind::Flag, default.to_string())
    }

    #[must_use]
    pub fn text(id: &str, label: &str, section: &str) -> Self {
        Self::new(id, label, section, FieldKind::Text, String::new())
    }

    #[must_use]
    pub fn number(
        id: &str,
        label: &str,
        section: &str,
        range: std::ops::RangeInclusive<i64>,
        default: i64,
    ) -> Self {
        let kind = FieldKind::Number(NumberRange {
            min: *range.start(),
            max: *range.end(),
        });
        Self::new(id, label, section, kind, default.to_string())
    }

    /// One of `choices`, as value and label pairs.
    #[must_use]
    pub fn choice(
        id: &str,
        label: &str,
        section: &str,
        default: &str,
        choices: &[(&str, &str)],
    ) -> Self {
        let kind = FieldKind::Choice(
            choices
                .iter()
                .map(|(value, label)| Choice {
                    value: (*value).to_owned(),
                    label: (*label).to_owned(),
                })
                .collect(),
        );
        Self::new(id, label, section, kind, default.to_owned())
    }

    /// A sentence shown on hover.
    #[must_use]
    pub fn help(mut self, help: &str) -> Self {
        help.clone_into(&mut self.help);
        self
    }

    /// Shown only while `field` has one of `values`.
    #[must_use]
    pub fn when(mut self, field: &str, values: &[&str]) -> Self {
        self.when = Some(condition(field, values));
        self
    }
}

impl Target {
    /// A file, with `extension` (without the dot) suggested.
    #[must_use]
    pub fn file(extension: &str) -> Self {
        Self {
            when: None,
            kind: PathKind::File,
            extension: extension.to_owned(),
        }
    }

    #[must_use]
    pub fn directory() -> Self {
        Self {
            when: None,
            kind: PathKind::Directory,
            extension: String::new(),
        }
    }

    #[must_use]
    pub fn when(mut self, field: &str, values: &[&str]) -> Self {
        self.when = Some(condition(field, values));
        self
    }
}

fn condition(field: &str, values: &[&str]) -> Condition {
    Condition {
        field: field.to_owned(),
        values: values.iter().map(|v| (*v).to_owned()).collect(),
    }
}

impl Request {
    /// A setting's value; Violet sends every field the task declares.
    #[must_use]
    pub fn setting(&self, id: &str) -> &str {
        self.settings
            .iter()
            .find(|(k, _)| k == id)
            .map_or("", |(_, v)| v.as_str())
    }

    /// A flag field.
    #[must_use]
    pub fn flag(&self, id: &str) -> bool {
        self.setting(id) == "true"
    }

    /// A number field.
    #[must_use]
    pub fn number(&self, id: &str) -> Option<i64> {
        self.setting(id).parse().ok()
    }
}

impl ToolCommand {
    /// `program` with no arguments yet.
    #[must_use]
    pub fn new(program: &str) -> Self {
        Self {
            program: program.to_owned(),
            args: Vec::new(),
            env: Vec::new(),
            password_env: None,
            stdout_to_path: false,
            stdin_from_path: false,
        }
    }

    #[must_use]
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Adds `arg` when `on`.
    #[must_use]
    pub fn arg_if(self, on: bool, arg: impl Into<String>) -> Self {
        if on { self.arg(arg) } else { self }
    }

    #[must_use]
    pub fn env(mut self, name: &str, value: impl Into<String>) -> Self {
        self.env.push((name.to_owned(), value.into()));
        self
    }

    /// The variable Violet puts the password in.
    #[must_use]
    pub fn password_env(mut self, name: &str) -> Self {
        self.password_env = Some(name.to_owned());
        self
    }
}

/// Splits extra arguments the user typed as a shell would, without running
/// one: whitespace separates, quotes group, a backslash escapes the next
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extra_arguments_split_like_a_shell() {
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
}
