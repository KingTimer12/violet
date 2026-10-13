//! An open PostgreSQL connection.

use std::time::Duration;

use async_trait::async_trait;
use futures_util::TryStreamExt as _;
use tokio::task::JoinHandle;
use tokio_postgres::types::Type;
use tokio_postgres::{CancelToken, Client, Config, NoTls, SimpleQueryMessage};
use violet_core::driver::AnsiDialect;
use violet_core::{
    Canceller, ColumnInfo, ColumnMeta, ConnectionProfile, Credentials, DbObject, Dialect,
    ExecLimits, ObjectKind, Result, ResultSet, Rows, ServerInfo, Session, SslMode,
};

use crate::{catalog, error, tls};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// A connected client and the task driving its socket.
pub struct PgSession {
    client: Client,
    connection: JoinHandle<()>,
    ssl: SslMode,
}

impl PgSession {
    pub(crate) async fn connect(
        profile: &ConnectionProfile,
        credentials: &Credentials,
    ) -> Result<Self> {
        // Never let `Prefer` silently drop to plain text on a remote host.
        let ssl = profile.ssl.effective(&profile.host);
        let mut config = Config::new();
        config
            .host(&profile.host)
            .port(profile.port)
            .user(&profile.username)
            .application_name("Violet")
            .connect_timeout(CONNECT_TIMEOUT)
            .ssl_mode(match ssl {
                SslMode::Disable => tokio_postgres::config::SslMode::Disable,
                SslMode::Prefer => tokio_postgres::config::SslMode::Prefer,
                SslMode::Require => tokio_postgres::config::SslMode::Require,
            });
        if !profile.database.trim().is_empty() {
            config.dbname(&profile.database);
        }
        if let Some(password) = &credentials.password {
            config.password(password.expose());
        }

        let (client, connection) = match ssl {
            SslMode::Disable => {
                let (client, connection) =
                    config.connect(NoTls).await.map_err(|e| error::map(&e))?;
                (client, tokio::spawn(drive(connection)))
            }
            SslMode::Prefer | SslMode::Require => {
                let (client, connection) = config
                    .connect(tls::connector()?)
                    .await
                    .map_err(|e| error::map(&e))?;
                (client, tokio::spawn(drive(connection)))
            }
        };
        log::info!(target: "violet::postgres", "connected ({ssl:?})");
        Ok(Self {
            client,
            connection,
            ssl,
        })
    }

    /// The column types of a single statement, learnt by preparing it.
    /// `None` for several statements, or ones the server will not prepare.
    async fn column_types(&self, sql: &str) -> Option<Vec<Type>> {
        let statement = self.client.prepare(sql).await.ok()?;
        Some(
            statement
                .columns()
                .iter()
                .map(|column| column.type_().clone())
                .collect(),
        )
    }
}

async fn drive<S, T>(connection: tokio_postgres::Connection<S, T>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    if let Err(error) = connection.await {
        log::warn!(target: "violet::postgres", "connection closed: {error}");
    }
}

fn is_numeric(ty: &Type) -> bool {
    matches!(
        *ty,
        Type::INT2
            | Type::INT4
            | Type::INT8
            | Type::FLOAT4
            | Type::FLOAT8
            | Type::NUMERIC
            | Type::OID
            | Type::MONEY
    )
}

fn relation_kind(relkind: &str) -> Option<ObjectKind> {
    Some(match relkind {
        "r" | "p" => ObjectKind::Table,
        "v" => ObjectKind::View,
        "m" => ObjectKind::MaterializedView,
        "f" => ObjectKind::ForeignTable,
        "S" => ObjectKind::Sequence,
        _ => return None,
    })
}

#[async_trait]
impl Session for PgSession {
    async fn server_info(&self) -> Result<ServerInfo> {
        let row = self
            .client
            .query_one(catalog::SERVER_INFO, &[])
            .await
            .map_err(|e| error::map(&e))?;
        Ok(ServerInfo {
            version: format!("PostgreSQL {}", row.get::<_, String>(0)),
            database: row.get(1),
            user: row.get(2),
        })
    }

    async fn databases(&self) -> Result<Vec<String>> {
        let rows = self
            .client
            .query(catalog::DATABASES, &[])
            .await
            .map_err(|e| error::map(&e))?;
        Ok(rows.iter().map(|row| row.get(0)).collect())
    }

    async fn schemas(&self) -> Result<Vec<String>> {
        let rows = self
            .client
            .query(catalog::SCHEMAS, &[])
            .await
            .map_err(|e| error::map(&e))?;
        Ok(rows.iter().map(|row| row.get(0)).collect())
    }

    async fn objects(&self, schema: &str) -> Result<Vec<DbObject>> {
        let params: [&(dyn tokio_postgres::types::ToSql + Sync); 1] = [&schema];
        let (relations, functions) = tokio::try_join!(
            self.client.query(catalog::RELATIONS, &params),
            self.client.query(catalog::FUNCTIONS, &params),
        )
        .map_err(|e| error::map(&e))?;

        let mut objects: Vec<DbObject> = relations
            .iter()
            .filter_map(|row| {
                Some(DbObject {
                    schema: schema.to_owned(),
                    name: row.get(0),
                    kind: relation_kind(row.get(1))?,
                    comment: row.get(2),
                    estimated_rows: row.get(3),
                })
            })
            .collect();
        objects.extend(functions.iter().map(|row| DbObject {
            schema: schema.to_owned(),
            name: row.get(0),
            kind: ObjectKind::Function,
            comment: row.get(1),
            estimated_rows: None,
        }));
        Ok(objects)
    }

    async fn columns(&self, schema: &str, relation: &str) -> Result<Vec<ColumnInfo>> {
        let rows = self
            .client
            .query(catalog::COLUMNS, &[&schema, &relation])
            .await
            .map_err(|e| error::map(&e))?;
        Ok(rows
            .iter()
            .map(|row| ColumnInfo {
                name: row.get(0),
                data_type: row.get(1),
                nullable: row.get(2),
                default: row.get(3),
                primary_key: row.get(4),
                comment: row.get(5),
            })
            .collect())
    }

    async fn execute(&self, sql: &str, limits: ExecLimits) -> Result<Vec<ResultSet>> {
        let types = self.column_types(sql).await;
        let stream = self
            .client
            .simple_query_raw(sql)
            .await
            .map_err(|e| error::map(&e))?;
        let mut stream = std::pin::pin!(stream);

        let mut sets = Vec::new();
        let mut current: Option<ResultSet> = None;
        while let Some(message) = stream.try_next().await.map_err(|e| error::map(&e))? {
            match message {
                SimpleQueryMessage::RowDescription(columns) => {
                    // Prepared types only describe a single statement.
                    let types = types
                        .as_ref()
                        .filter(|types| sets.is_empty() && types.len() == columns.len());
                    current = Some(ResultSet {
                        rows: Rows::new(columns.len()),
                        columns: columns
                            .iter()
                            .enumerate()
                            .map(|(index, column)| {
                                let ty = types.map(|types| &types[index]);
                                ColumnMeta {
                                    name: column.name().to_owned(),
                                    type_name: ty.map(|ty| ty.name().to_owned()),
                                    numeric: ty.is_some_and(is_numeric),
                                }
                            })
                            .collect(),
                        ..ResultSet::default()
                    });
                }
                SimpleQueryMessage::Row(row) => {
                    let set = current.get_or_insert_with(ResultSet::default);
                    if set.rows.len() < limits.max_rows {
                        set.rows.push((0..row.len()).map(|i| row.get(i)));
                    } else {
                        // Keep reading so later statements still run, but
                        // hold no more rows in memory.
                        set.truncated = true;
                    }
                }
                SimpleQueryMessage::CommandComplete(count) => {
                    let mut set = current.take().unwrap_or_default();
                    set.affected = Some(count);
                    sets.push(set);
                }
                _ => {}
            }
        }
        if let Some(set) = current {
            sets.push(set);
        }
        Ok(sets)
    }

    fn canceller(&self) -> Option<Box<dyn Canceller>> {
        Some(Box::new(PgCanceller {
            token: self.client.cancel_token(),
            ssl: self.ssl,
        }))
    }

    fn dialect(&self) -> &dyn Dialect {
        &AnsiDialect
    }

    async fn close(&self) {
        self.connection.abort();
    }
}

impl Drop for PgSession {
    fn drop(&mut self) {
        self.connection.abort();
    }
}

struct PgCanceller {
    token: CancelToken,
    ssl: SslMode,
}

#[async_trait]
impl Canceller for PgCanceller {
    async fn cancel(&self) -> Result<()> {
        let result = match self.ssl {
            SslMode::Disable => self.token.cancel_query(NoTls).await,
            SslMode::Prefer | SslMode::Require => self.token.cancel_query(tls::connector()?).await,
        };
        result.map_err(|e| error::map(&e))
    }
}
