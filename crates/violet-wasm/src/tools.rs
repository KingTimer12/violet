//! Backup and restore: a provider's `tools` export, in Violet's terms.
//! Everything it returns is checked before use.

use violet_core::{
    Argument, Binding, Choice, CommandPart, CommandTemplate, Condition, Connection, DbError,
    Download, ErrorKind, Field, FieldKind, INSPECT_ENTRIES as ENTRIES, INSPECT_HEAD as HEAD,
    ObjectArguments, PathKind, Piece, Quoting, Result, Target, Task, TaskKind, TaskSettings,
    ValuePiece, Variable,
};

use crate::Provider;
use crate::bindings::exports::violet::provider::tools as api;
use crate::session::{Instance, crashed, refuel};

/// A fresh instance and its `tools` export, or `None` when the provider
/// predates them.
async fn start(provider: &Provider) -> Result<Option<(Instance, api::Guest)>> {
    if provider.tools.is_none() {
        return Ok(None);
    }
    let mut instance = Instance::start(provider, None).await?;
    refuel(&mut instance.store);
    let tools = instance.exports().tools().cloned();
    Ok(tools.map(|tools| (instance, tools)))
}

/// The tasks a provider offers, checked; none when it predates them.
pub(crate) async fn tasks(provider: &Provider) -> Result<Vec<Task>> {
    let Some((mut instance, tools)) = start(provider).await? else {
        return Ok(Vec::new());
    };
    let tasks = tools
        .call_tasks(&mut instance.store)
        .await
        .map_err(|error| crashed(&error))?;
    let mut checked: Vec<Task> = Vec::with_capacity(tasks.len());
    for task in tasks.into_iter().take(4) {
        let task = task_from(task);
        task.check()?;
        if checked.iter().any(|other| other.kind == task.kind) {
            return Err(DbError::new(
                ErrorKind::Provider,
                format!("The provider offers two {} tasks", task.kind),
            ));
        }
        checked.push(task);
    }
    Ok(checked)
}

/// The programs the provider would have downloaded, checked.
pub(crate) async fn downloads(provider: &Provider) -> Result<Vec<Download>> {
    let Some((mut instance, tools)) = start(provider).await? else {
        return Ok(Vec::new());
    };
    let downloads = tools
        .call_downloads(&mut instance.store)
        .await
        .map_err(|error| crashed(&error))?;
    let downloads: Vec<Download> = downloads
        .into_iter()
        .take(16)
        .map(|d| Download {
            platform: d.platform,
            url: d.url,
            sha256: d.sha256.to_ascii_lowercase(),
            size: d.size,
            label: d.label,
            root: d.root,
            extract: d.extract,
            bin: d.bin,
        })
        .collect();
    for download in &downloads {
        download.check()?;
    }
    Ok(downloads)
}

/// Settings the provider suggests for restoring from a path.
pub(crate) async fn inspect(
    provider: &Provider,
    head: &[u8],
    entries: &[String],
    directory: bool,
) -> Result<TaskSettings> {
    let Some((mut instance, tools)) = start(provider).await? else {
        return Ok(TaskSettings::new());
    };
    let head = &head[..head.len().min(HEAD)];
    let entries = &entries[..entries.len().min(ENTRIES)];
    let settings = tools
        .call_inspect(&mut instance.store, head, entries, directory)
        .await
        .map_err(|error| crashed(&error))?;
    Ok(settings
        .into_iter()
        .take(64)
        .filter(|(k, v)| k.len() <= 32 && v.len() <= 256)
        .collect())
}

fn task_from(task: api::Task) -> Task {
    Task {
        kind: match task.kind {
            api::TaskKind::Backup => TaskKind::Backup,
            api::TaskKind::Restore => TaskKind::Restore,
        },
        programs: task.programs,
        locations: task.locations,
        version_arg: task.version_arg,
        fields: task
            .fields
            .into_iter()
            .map(|field| Field {
                id: field.id,
                label: field.label,
                help: field.help,
                section: field.section,
                kind: match field.kind {
                    api::FieldKind::Flag => FieldKind::Flag,
                    api::FieldKind::Text => FieldKind::Text,
                    api::FieldKind::Number(range) => FieldKind::Number {
                        min: range.min,
                        max: range.max,
                    },
                    api::FieldKind::Choice(choices) => FieldKind::Choice(
                        choices
                            .into_iter()
                            .map(|choice| Choice {
                                value: choice.value,
                                label: choice.label,
                            })
                            .collect(),
                    ),
                },
                default: field.default,
                when: conditions(field.when),
            })
            .collect(),
        objects: task.objects,
        targets: task
            .targets
            .into_iter()
            .map(|target| Target {
                when: conditions(target.when),
                kind: match target.kind {
                    api::PathKind::File => PathKind::File,
                    api::PathKind::Directory => PathKind::Directory,
                },
                extension: target.extension,
            })
            .collect(),
        connection: Connection {
            host: task.connection.host.map(binding),
            port: task.connection.port.map(binding),
            user: task.connection.user.map(binding),
            database: task.connection.database.map(binding),
            password_env: task.connection.password_env,
            database_field: task.connection.database_field,
        },
        commands: task
            .commands
            .into_iter()
            .map(|command| CommandTemplate {
                program: command.program,
                when: conditions(command.when),
                parts: command
                    .parts
                    .into_iter()
                    .map(|part| match part {
                        api::CommandPart::Argument(argument) => CommandPart::Argument(Argument {
                            when: conditions(argument.when),
                            pieces: pieces(argument.pieces),
                            each: argument.each,
                        }),
                        api::CommandPart::Objects(objects) => {
                            CommandPart::Objects(ObjectArguments {
                                schema_prefix: objects.schema_prefix,
                                table_prefix: objects.table_prefix,
                                quoting: match objects.quoting {
                                    api::Quoting::Bare => Quoting::None,
                                    api::Quoting::Double => Quoting::Double,
                                    api::Quoting::Backtick => Quoting::Backtick,
                                    api::Quoting::Bracket => Quoting::Bracket,
                                },
                                schema_tables: objects.schema_tables,
                            })
                        }
                        api::CommandPart::Extra(field) => CommandPart::Extra(field),
                    })
                    .collect(),
                variables: command
                    .variables
                    .into_iter()
                    .map(|variable| Variable {
                        when: conditions(variable.when),
                        name: variable.name,
                        pieces: pieces(variable.pieces),
                    })
                    .collect(),
                stdout_to_path: command.stdout_to_path,
                stdin_from_path: command.stdin_from_path,
                stdin_lines: command.stdin_lines,
            })
            .collect(),
    }
}

fn conditions(conditions: Vec<api::Condition>) -> Vec<Condition> {
    conditions
        .into_iter()
        .map(|condition| Condition {
            field: condition.field,
            values: condition.values,
            except: condition.except,
        })
        .collect()
}

fn binding(binding: api::Binding) -> Binding {
    match binding {
        api::Binding::Env(name) => Binding::Env(name),
        api::Binding::Prefix(option) => Binding::Option(option),
    }
}

fn pieces(pieces: Vec<api::Piece>) -> Vec<Piece> {
    pieces
        .into_iter()
        .map(|piece| match piece {
            api::Piece::Text(text) => Piece::Text(text),
            api::Piece::Value(value) => Piece::Value(ValuePiece {
                field: value.field,
                map: value.replace,
            }),
            api::Piece::Path => Piece::Path,
            api::Piece::Host => Piece::Host,
            api::Piece::Port => Piece::Port,
            api::Piece::User => Piece::User,
            api::Piece::Database => Piece::Database,
        })
        .collect()
}
