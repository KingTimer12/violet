//! Values crossing between Violet and a provider. What comes back from a
//! provider is untrusted and checked here.

use violet_core::{
    Capabilities, ColumnInfo, ColumnMeta, ConnectionProfile, Credentials, DbError, DbObject,
    DriverId, DriverInfo, EntryPage, ErrorKind, Language, Login, ObjectKind, Paging, ResultSet, Rows, ServerInfo, SqlDialect,
    SslMode,
};

use crate::bindings::exports::violet::provider::data as data_api;
use crate::bindings::types as wit;

/// Longest id a provider may claim.
const MAX_ID: usize = 32;

pub(crate) fn connect_params(
    profile: &ConnectionProfile,
    credentials: Option<&Credentials>,
) -> wit::ConnectParams {
    wit::ConnectParams {
        host: profile.host.clone(),
        port: profile.port,
        database: profile.database.clone(),
        username: profile.username.clone(),
        password: credentials
            .and_then(|credentials| credentials.password.as_ref())
            .map(|password| password.expose().to_owned()),
        ssl: match ssl(profile) {
            SslMode::Disable => wit::SslMode::Disable,
            SslMode::Prefer => wit::SslMode::Prefer,
            SslMode::Require => wit::SslMode::Require,
        },
        options: profile
            .options
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    }
}

/// The mode the provider must honour: never a downgradable `Prefer` for a
/// remote host.
pub(crate) fn ssl(profile: &ConnectionProfile) -> SslMode {
    profile.ssl.effective(&profile.host)
}

/// Whether `id` is a usable provider id: it names folders and profiles.
#[must_use]
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !id.starts_with('-')
}

pub(crate) fn description(
    info: wit::ProviderInfo,
    extras: Option<data_api::ProviderExtras>,
) -> Result<crate::Description, DbError> {
    if !valid_id(&info.id) {
        return Err(DbError::new(
            ErrorKind::Provider,
            format!(
                "Invalid provider id {:?}: use up to {MAX_ID} lowercase letters, digits and -",
                info.id
            ),
        ));
    }
    let quotes_ok = |c: char| !c.is_alphanumeric() && !c.is_whitespace() && c != '\'';
    if !quotes_ok(info.dialect.quote_open) || !quotes_ok(info.dialect.quote_close) {
        return Err(DbError::new(
            ErrorKind::Provider,
            "The provider's identifier quotes are not usable",
        ));
    }
    let short = |text: String| text.chars().take(64).collect::<String>();
    let info_version = short(info.version);
    let description = info.description.chars().take(200).collect();
    let info = DriverInfo {
        id: DriverId::new(info.id),
        name: short(info.name),
        default_port: info.default_port,
        default_database: short(info.default_database),
        default_user: short(info.default_user),
        capabilities: Capabilities {
            databases: info.capabilities.databases,
            schemas: info.capabilities.schemas,
            cancel: info.capabilities.cancel,
            tls: info.capabilities.tls,
        },
        dialect: SqlDialect {
            quote_open: info.dialect.quote_open,
            quote_close: info.dialect.quote_close,
            paging: match info.dialect.paging {
                wit::Paging::LimitOffset => Paging::LimitOffset,
                wit::Paging::OffsetFetch => Paging::OffsetFetch,
            },
        },
        language: Language::Sql,
        user: Login::Required,
        password: Login::Optional,
        schema_label: String::new(),
    };
    let mut info = info;
    if let Some(extras) = extras {
        info.language = match extras.language {
            data_api::Language::Sql => Language::Sql,
            data_api::Language::Redis => Language::Redis,
            data_api::Language::Mongodb => Language::Mongodb,
        };
        info.user = login(extras.user);
        info.password = login(extras.password);
        info.schema_label = short(extras.schema_label);
    }
    Ok(crate::Description {
        info,
        version: info_version,
        description,
    })
}

fn login(login: data_api::Login) -> Login {
    match login {
        data_api::Login::Required => Login::Required,
        data_api::Login::Optional => Login::Optional,
        data_api::Login::None => Login::None,
    }
}

/// Longest key type a provider may name, and hover text it may show.
const MAX_SUBTYPE: usize = 32;
const MAX_DETAIL: usize = 200;

/// A page of entries, checked: at most `limit` of them, short labels.
pub(crate) fn entry_page(schema: &str, page: data_api::EntryPage, limit: u32) -> EntryPage {
    let objects = page
        .entries
        .into_iter()
        .take(limit as usize)
        .map(|entry| DbObject {
            schema: schema.to_owned(),
            name: entry.name,
            kind: ObjectKind::Key,
            comment: None,
            estimated_rows: entry.size,
            subtype: entry.subtype.filter(|subtype| {
                subtype.len() <= MAX_SUBTYPE
                    && subtype
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            }),
            detail: entry
                .detail
                .map(|detail| detail.chars().take(MAX_DETAIL).collect()),
        })
        .collect();
    EntryPage {
        objects,
        next: page.next.filter(|next| next.len() <= 1024),
    }
}

pub(crate) fn error(error: wit::DbError) -> DbError {
    DbError {
        kind: match error.kind {
            wit::ErrorKind::Connection => ErrorKind::Connection,
            wit::ErrorKind::Authentication => ErrorKind::Authentication,
            wit::ErrorKind::Tls => ErrorKind::Tls,
            wit::ErrorKind::Query => ErrorKind::Query,
            wit::ErrorKind::Cancelled => ErrorKind::Cancelled,
            wit::ErrorKind::Unsupported => ErrorKind::Unsupported,
            wit::ErrorKind::Other => ErrorKind::Other,
        },
        message: error.message,
        code: error.code,
        detail: error.detail,
        hint: error.hint,
        position: error.position,
    }
}

pub(crate) fn server_info(info: wit::ServerInfo) -> ServerInfo {
    ServerInfo {
        version: info.version,
        database: info.database,
        user: info.user,
    }
}

pub(crate) fn object(schema: &str, object: wit::DbObject) -> DbObject {
    DbObject {
        schema: schema.to_owned(),
        name: object.name,
        kind: match object.kind {
            wit::ObjectKind::Table => ObjectKind::Table,
            wit::ObjectKind::View => ObjectKind::View,
            wit::ObjectKind::MaterializedView => ObjectKind::MaterializedView,
            wit::ObjectKind::ForeignTable => ObjectKind::ForeignTable,
            wit::ObjectKind::Function => ObjectKind::Function,
            wit::ObjectKind::Sequence => ObjectKind::Sequence,
        },
        comment: object.comment,
        estimated_rows: object.estimated_rows,
        subtype: None,
        detail: None,
    }
}

pub(crate) fn column(column: wit::ColumnInfo) -> ColumnInfo {
    ColumnInfo {
        name: column.name,
        data_type: column.data_type,
        nullable: column.nullable,
        default: column.default,
        primary_key: column.primary_key,
        comment: column.comment,
    }
}

#[hotpath::measure]
pub(crate) fn result_set(set: wit::ResultSet) -> Result<ResultSet, DbError> {
    let width = set.columns.len();
    let rows = Rows::from_text(width, set.rows.text, &set.rows.ends)
        .ok_or_else(|| DbError::new(ErrorKind::Provider, "The provider returned malformed rows"))?;
    Ok(ResultSet {
        columns: set
            .columns
            .into_iter()
            .map(|column| ColumnMeta {
                name: column.name,
                type_name: column.type_name,
                numeric: column.numeric,
            })
            .collect(),
        rows,
        affected: set.affected,
        command: set.command,
        truncated: set.truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_name_folders_safely() {
        for id in ["postgres", "my-sql", "db2"] {
            assert!(valid_id(id), "{id}");
        }
        for id in ["", "../x", "Postgres", "a b", "-x", "a/b", &"x".repeat(33)] {
            assert!(!valid_id(id), "{id}");
        }
    }
}
