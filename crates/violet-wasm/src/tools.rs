//! Backup and restore: a provider's `tools` export, in Violet's terms.
//! Everything it returns is checked before use.

use violet_core::{
    Choice, Condition, DbError, ErrorKind, Field, FieldKind, INSPECT_ENTRIES as ENTRIES,
    INSPECT_HEAD as HEAD, PathKind, Result, Target, Task, TaskKind, TaskRequest, TaskSettings,
    ToolCommand,
};

use crate::Provider;
use crate::bindings::exports::violet::provider::tools as api;
use crate::bindings::types as wit;
use crate::convert;
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

/// The command for `request`, unchecked. `params` carries no password.
pub(crate) async fn command(
    provider: &Provider,
    params: wit::ConnectParams,
    request: &TaskRequest,
) -> Result<ToolCommand> {
    let (mut instance, tools) = start(provider)
        .await?
        .ok_or_else(|| DbError::unsupported("This provider has no backup or restore"))?;
    let kind = match request.kind {
        TaskKind::Backup => api::TaskKind::Backup,
        TaskKind::Restore => api::TaskKind::Restore,
    };
    let wire = api::Request {
        params,
        settings: request
            .settings
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        objects: request
            .objects
            .iter()
            .map(|object| api::ObjectRef {
                schema: object.schema.clone(),
                name: object.name.clone(),
            })
            .collect(),
        path: request.path.clone(),
    };
    let command = tools
        .call_command(&mut instance.store, kind, &wire)
        .await
        .map_err(|error| crashed(&error))?
        .map_err(convert::error)?;
    Ok(ToolCommand {
        program: command.program,
        args: command.args,
        env: command.env,
        password_env: command.password_env,
        stdout_to_path: command.stdout_to_path,
        stdin_from_path: command.stdin_from_path,
    })
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
                when: field.when.map(condition),
            })
            .collect(),
        objects: task.objects,
        targets: task
            .targets
            .into_iter()
            .map(|target| Target {
                when: target.when.map(condition),
                kind: match target.kind {
                    api::PathKind::File => PathKind::File,
                    api::PathKind::Directory => PathKind::Directory,
                },
                extension: target.extension,
            })
            .collect(),
    }
}

fn condition(condition: api::Condition) -> Condition {
    Condition {
        field: condition.field,
        values: condition.values,
    }
}
