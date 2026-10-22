//! Backup and restore through the database's own client programs.
//!
//! A provider cannot start programs, and does not build command lines: it
//! declares each [`Task`] with its settings and a [`Command`] template.
//! Violet fills the template, passes the profile's connection the way
//! [`Connection`] says (the password only through
//! [`Connection::password_env`]), finds the program, shows the whole
//! command and runs it. [`Download`]s name client programs Violet fetches
//! with the provider, so backups work without installing anything.
//!
//! ```ignore
//! use violet_sdk::tools::*;
//!
//! fn tasks() -> Vec<Task> {
//!     vec![Task::new(TaskKind::Backup, &["litedump"])
//!         .connection(Connection::env("LITE_HOST", "LITE_PORT", "LITE_USER", "LITE_DB")
//!             .password("LITE_PASSWORD"))
//!         .field(Field::choice("format", "Format", "Output", "sql", &[
//!             ("sql", "SQL script"),
//!             ("bin", "Binary"),
//!         ]))
//!         .field(Field::flag("data", "Include data", "Content", true))
//!         .target(Target::file("sql").when("format", &["sql"]))
//!         .target(Target::file("lite"))
//!         .command(Command::new("litedump")
//!             .arg([text("--format="), value("format")])
//!             .flag("data", false, "--no-data")
//!             .arg([text("--out="), path()]))]
//! }
//! ```

pub use crate::bindings::exports::violet::provider::tools::{
    Argument, Binding, Choice, Command, CommandPart, Condition, Connection, Download, Field,
    FieldKind, NumberRange, ObjectArguments, PathKind, Piece, Quoting, Target, Task, TaskKind,
    ValuePiece, Variable,
};

/// Fixed text.
#[must_use]
pub fn text(text: &str) -> Piece {
    Piece::Text(text.to_owned())
}

/// A setting's value.
#[must_use]
pub fn value(field: &str) -> Piece {
    mapped(field, &[])
}

/// A setting's value, with listed values replaced (`custom` → `c`).
#[must_use]
pub fn mapped(field: &str, replace: &[(&str, &str)]) -> Piece {
    Piece::Value(ValuePiece {
        field: field.to_owned(),
        replace: replace
            .iter()
            .map(|(from, to)| ((*from).to_owned(), (*to).to_owned()))
            .collect(),
    })
}

/// The file or folder the user chose.
#[must_use]
pub fn path() -> Piece {
    Piece::Path
}

/// Holds when `field` has one of `values`. `@ssl` is the profile's TLS
/// mode: `disable`, `prefer` or `require`.
#[must_use]
pub fn is(field: &str, values: &[&str]) -> Condition {
    Condition {
        field: field.to_owned(),
        values: values.iter().map(|v| (*v).to_owned()).collect(),
        except: false,
    }
}

/// Holds when `field` has none of `values`.
#[must_use]
pub fn not(field: &str, values: &[&str]) -> Condition {
    Condition {
        except: true,
        ..is(field, values)
    }
}

/// Holds when a flag is on.
#[must_use]
pub fn on(field: &str) -> Condition {
    is(field, &["true"])
}

impl Task {
    /// A task running `programs`, with nothing declared yet.
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
            connection: Connection {
                host: None,
                port: None,
                user: None,
                database: None,
                password_env: None,
                database_field: None,
            },
            commands: Vec::new(),
        }
    }

    /// Folders the programs usually live in; `*` stands for part of one
    /// folder name, such as a version.
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

    /// Lets the user pick schemas and relations; see [`Command::objects`].
    #[must_use]
    pub fn with_objects(mut self) -> Self {
        self.objects = true;
        self
    }

    /// What it writes or reads; the first whose conditions hold applies.
    #[must_use]
    pub fn target(mut self, target: Target) -> Self {
        self.targets.push(target);
        self
    }

    #[must_use]
    pub fn connection(mut self, connection: Connection) -> Self {
        self.connection = connection;
        self
    }

    /// A command line; the first whose conditions hold runs.
    #[must_use]
    pub fn command(mut self, command: Command) -> Self {
        self.commands.push(command);
        self
    }
}

impl Connection {
    /// Host, port, user and database through these environment variables.
    #[must_use]
    pub fn env(host: &str, port: &str, user: &str, database: &str) -> Self {
        let env = |name: &str| Some(Binding::Env(name.to_owned()));
        Self {
            host: env(host),
            port: env(port),
            user: env(user),
            database: env(database),
            password_env: None,
            database_field: None,
        }
    }

    /// The variable that carries the password.
    #[must_use]
    pub fn password(mut self, name: &str) -> Self {
        self.password_env = Some(name.to_owned());
        self
    }

    /// A text field naming another database to use instead of the
    /// profile's, when filled.
    #[must_use]
    pub fn database_field(mut self, field: &str) -> Self {
        self.database_field = Some(field.to_owned());
        self
    }
}

impl Command {
    #[must_use]
    pub fn new(program: &str) -> Self {
        Self {
            program: program.to_owned(),
            when: Vec::new(),
            parts: Vec::new(),
            variables: Vec::new(),
            stdout_to_path: false,
            stdin_from_path: false,
            stdin_lines: Vec::new(),
        }
    }

    /// Runs only while `condition` holds.
    #[must_use]
    pub fn when(mut self, condition: Condition) -> Self {
        self.when.push(condition);
        self
    }

    /// An argument, its pieces joined.
    #[must_use]
    pub fn arg(self, pieces: impl IntoIterator<Item = Piece>) -> Self {
        self.arg_when(&[], pieces)
    }

    /// An argument added while every condition holds.
    #[must_use]
    pub fn arg_when(mut self, when: &[Condition], pieces: impl IntoIterator<Item = Piece>) -> Self {
        self.parts.push(CommandPart::Argument(Argument {
            when: when.to_vec(),
            pieces: pieces.into_iter().collect(),
            each: None,
        }));
        self
    }

    /// `argument` when the flag `field` is `set`.
    #[must_use]
    pub fn flag(self, field: &str, set: bool, argument: &str) -> Self {
        self.arg_when(
            &[is(field, &[if set { "true" } else { "false" }])],
            [text(argument)],
        )
    }

    /// An argument per comma-separated item of the text field `field`,
    /// the item standing for its [`value`].
    #[must_use]
    pub fn each(
        mut self,
        when: &[Condition],
        field: &str,
        pieces: impl IntoIterator<Item = Piece>,
    ) -> Self {
        self.parts.push(CommandPart::Argument(Argument {
            when: when.to_vec(),
            pieces: pieces.into_iter().collect(),
            each: Some(field.to_owned()),
        }));
        self
    }

    /// An argument per schema or relation the user picked. With
    /// `schema_tables`, whole schemas picked alongside relations are
    /// written as `table_prefix`, the schema and it (`.*`), for programs
    /// that ignore schemas once tables are named.
    #[must_use]
    pub fn objects(
        mut self,
        schema_prefix: &str,
        table_prefix: &str,
        quoting: Quoting,
        schema_tables: Option<&str>,
    ) -> Self {
        self.parts.push(CommandPart::Objects(ObjectArguments {
            schema_prefix: schema_prefix.to_owned(),
            table_prefix: table_prefix.to_owned(),
            quoting,
            schema_tables: schema_tables.map(str::to_owned),
        }));
        self
    }

    /// The arguments the user typed in the text field `field`.
    #[must_use]
    pub fn extra(mut self, field: &str) -> Self {
        self.parts.push(CommandPart::Extra(field.to_owned()));
        self
    }

    /// An environment variable, set while every condition holds.
    #[must_use]
    pub fn env(
        mut self,
        when: &[Condition],
        name: &str,
        pieces: impl IntoIterator<Item = Piece>,
    ) -> Self {
        self.variables.push(Variable {
            when: when.to_vec(),
            name: name.to_owned(),
            pieces: pieces.into_iter().collect(),
        });
        self
    }

    /// Violet writes standard output to the path.
    #[must_use]
    pub fn stdout_to_path(mut self) -> Self {
        self.stdout_to_path = true;
        self
    }

    /// Violet feeds the path to standard input.
    #[must_use]
    pub fn stdin_from_path(mut self) -> Self {
        self.stdin_from_path = true;
        self
    }

    /// While feeding the path, Violet swaps each line reading `from` for `to`.
    #[must_use]
    pub fn replace_line(mut self, from: &str, to: &str) -> Self {
        self.stdin_lines.push((from.to_owned(), to.to_owned()));
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
            when: Vec::new(),
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

    /// Shown only while `field` has one of `values` (and any other
    /// condition holds).
    #[must_use]
    pub fn when(mut self, field: &str, values: &[&str]) -> Self {
        self.when.push(is(field, values));
        self
    }
}

impl Target {
    /// A file, with `extension` (without the dot) suggested.
    #[must_use]
    pub fn file(extension: &str) -> Self {
        Self {
            when: Vec::new(),
            kind: PathKind::File,
            extension: extension.to_owned(),
        }
    }

    #[must_use]
    pub fn directory() -> Self {
        Self {
            when: Vec::new(),
            kind: PathKind::Directory,
            extension: String::new(),
        }
    }

    #[must_use]
    pub fn when(mut self, field: &str, values: &[&str]) -> Self {
        self.when.push(is(field, values));
        self
    }
}
