//! A Redis session: one connection for the editor, whose `SELECT` and
//! `MULTI` state the user drives, and a second one, opened when first
//! needed, for the tree and data views, so browsing never lands inside an
//! open transaction or another database.

use std::cell::RefCell;
use std::collections::VecDeque;

use violet_lang::redis::{self as lang, database_index, statements};
use violet_sdk::data::{Entry, EntryPage};
use violet_sdk::net::Connection;
use violet_sdk::{
    ColumnInfo, ConnectParams, DbError, DbObject, ErrorKind, Result, ResultSet, Rows, ServerInfo,
    Session, SslMode,
};

use crate::render::{self, Shape};
use crate::resp::{self, Reply};

/// Keys a `SCAN` asks for at a time, and the most rounds one page of the
/// tree waits for when few keys match.
const SCAN_COUNT: usize = 500;
const SCAN_ROUNDS: usize = 64;

/// One connection, logged in, in database `db`.
pub struct Client {
    connection: Connection,
    db: u32,
}

impl Client {
    pub fn open(params: &ConnectParams) -> Result<Self> {
        let mut connection = Connection::open()?;
        // Redis has no in-band upgrade: TLS starts with the first byte.
        if params.ssl == SslMode::Require {
            connection.start_tls()?;
        }
        let mut client = Self { connection, db: 0 };
        if let Some(password) = params.password.as_deref().filter(|p| !p.is_empty()) {
            let user = params.username.trim();
            let reply = if user.is_empty() {
                client.call(&["AUTH", password])?
            } else {
                client.call(&["AUTH", user, password])?
            };
            if let Reply::Error(message) = reply {
                return Err(DbError::authentication(message));
            }
        }
        // Old servers and proxies may not know it; the name is only a label.
        let _ = client.call(&["CLIENT", "SETNAME", "violet"])?;
        let db = database_index(&params.database).unwrap_or(0);
        match client.call(&["PING"])? {
            Reply::Error(message) if is_auth_error(&message) => {
                return Err(DbError::authentication(if message.starts_with("NOAUTH") {
                    "The server needs a password".to_owned()
                } else {
                    message
                }));
            }
            Reply::Error(message) => return Err(DbError::connection(message)),
            _ => {}
        }
        client.select(db)?;
        Ok(client)
    }

    /// Sends one command and reads its reply, keeping all of it.
    pub fn call<A: AsRef<[u8]>>(&mut self, args: &[A]) -> Result<Reply> {
        let mut out = Vec::new();
        resp::encode(args, &mut out);
        self.connection.send(&out)?;
        Ok(resp::read(&mut self.connection, usize::MAX)?.0)
    }

    /// Sends commands together and reads their replies, keeping `keep`
    /// elements of each.
    pub fn pipeline(&mut self, commands: &[Vec<Vec<u8>>], keep: usize) -> Result<Vec<(Reply, bool)>> {
        let mut out = Vec::new();
        for command in commands {
            resp::encode(command, &mut out);
        }
        self.connection.send(&out)?;
        commands
            .iter()
            .map(|_| resp::read(&mut self.connection, keep))
            .collect()
    }

    pub fn select(&mut self, db: u32) -> Result<()> {
        if db == self.db {
            return Ok(());
        }
        match self.call(&["SELECT", &db.to_string()])? {
            Reply::Error(message) => Err(DbError::query(message)),
            _ => {
                self.db = db;
                Ok(())
            }
        }
    }
}

fn is_auth_error(message: &str) -> bool {
    message.starts_with("NOAUTH") || message.starts_with("WRONGPASS") || message.contains("invalid password")
}

fn schema_db(schema: &str) -> Result<u32> {
    database_index(schema).ok_or_else(|| DbError::query(format!("{schema:?} is not a database")))
}

/// A `SCAN`-like walk over a key's elements, kept between pages so the
/// next one goes on from where the last stopped.
struct Scan {
    schema: String,
    statement: String,
    /// The command without its cursor: `HSCAN key` plus options.
    command: Vec<Vec<u8>>,
    /// Where the cursor goes in it.
    cursor_at: usize,
    /// Replies per element: 2 for pairs.
    width: usize,
    cursor: Vec<u8>,
    done: bool,
    /// Elements handed out so far.
    position: u64,
    /// Elements read and not handed out yet.
    buffer: VecDeque<Vec<Reply>>,
}

/// How to walk a statement with a `*SCAN`.
struct Walk {
    command: Vec<Vec<u8>>,
    cursor_at: usize,
    width: usize,
    shape: Shape,
}

pub struct RedisSession {
    params: ConnectParams,
    main: RefCell<Client>,
    catalog: RefCell<Option<Client>>,
    version: String,
    /// The commands queued since `MULTI`, to name `EXEC`'s results.
    queued: RefCell<Option<Vec<Vec<String>>>>,
    scan: RefCell<Option<Scan>>,
}

/// Commands that change the connection for good or never answer once:
/// Violet waits for each reply, with no timeout.
fn refused(args: &[String]) -> Option<String> {
    let upper: Vec<String> = args.iter().map(|a| a.to_ascii_uppercase()).collect();
    let name = upper.first().map_or("", String::as_str);
    let last = upper.last().map_or("", String::as_str);
    let why = match name {
        "SUBSCRIBE" | "PSUBSCRIBE" | "SSUBSCRIBE" | "MONITOR" => {
            "streams messages without end, which an editor cannot show"
        }
        "SYNC" | "PSYNC" | "REPLCONF" => "turns the connection into a replica",
        "HELLO" | "RESET" | "QUIT" => "changes the connection Violet relies on",
        "CLIENT" if upper.get(1).is_some_and(|w| w == "REPLY") => {
            "changes the connection Violet relies on"
        }
        "BLPOP" | "BRPOP" | "BLMOVE" | "BRPOPLPUSH" | "BZPOPMIN" | "BZPOPMAX" | "WAIT"
        | "WAITAOF"
            if last == "0" =>
        {
            "waits for ever with a timeout of 0; give it one in seconds"
        }
        "BLMPOP" | "BZMPOP" if upper.get(1).is_some_and(|w| w == "0") => {
            "waits for ever with a timeout of 0; give it one in seconds"
        }
        "XREAD" | "XREADGROUP"
            if upper
                .windows(2)
                .any(|pair| pair[0] == "BLOCK" && pair[1] == "0") =>
        {
            "waits for ever with BLOCK 0; give it a timeout in milliseconds"
        }
        _ => return None,
    };
    Some(format!("{name} is not run from Violet: it {why}"))
}

fn strings(args: &[Vec<u8>]) -> Vec<String> {
    args.iter()
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect()
}

/// `rows` without their first `skip` (each `width` cells wide).
fn skip_rows(rows: Rows, width: usize, skip: usize) -> Rows {
    let cells = skip * width;
    if cells == 0 || width == 0 {
        return rows;
    }
    if cells >= rows.ends.len() {
        return Rows {
            text: String::new(),
            ends: Vec::new(),
        };
    }
    const NULL: u32 = 1 << 31;
    let base = rows.ends[cells - 1] & !NULL;
    Rows {
        text: rows.text[base as usize..].to_owned(),
        ends: rows.ends[cells..]
            .iter()
            .map(|end| ((end & !NULL) - base) | (end & NULL))
            .collect(),
    }
}

impl RedisSession {
    pub fn connect(params: ConnectParams) -> Result<Self> {
        let mut main = Client::open(&params)?;
        let info = main.call(&["INFO", "server"])?;
        let version = info
            .text()
            .map(|text| product(&text))
            .unwrap_or_else(|| "Redis".to_owned());
        Ok(Self {
            params,
            main: RefCell::new(main),
            catalog: RefCell::new(None),
            version,
            queued: RefCell::new(None),
            scan: RefCell::new(None),
        })
    }

    /// Runs `work` on the catalog connection, in database `schema`,
    /// opening it the first time.
    fn browse<T>(&self, schema: &str, work: impl FnOnce(&mut Client) -> Result<T>) -> Result<T> {
        let db = schema_db(schema)?;
        let mut catalog = self.catalog.borrow_mut();
        if catalog.is_none() {
            *catalog = Some(Client::open(&self.params)?);
        }
        let Some(client) = catalog.as_mut() else {
            return Err(DbError::connection("No catalog connection"));
        };
        let result = client.select(db).and_then(|()| work(client));
        // A broken connection is opened again next time.
        if result.as_ref().is_err_and(|error| error.kind == ErrorKind::Connection) {
            *catalog = None;
        }
        result
    }

    /// The elements `scan` hands out next, at most `count`.
    fn pull(client: &mut Client, scan: &mut Scan, count: usize) -> Result<()> {
        while scan.buffer.len() < count && !scan.done {
            let mut command = scan.command.clone();
            command.insert(scan.cursor_at, scan.cursor.clone());
            command.extend([b"COUNT".to_vec(), SCAN_COUNT.max(count).to_string().into_bytes()]);
            let reply = client.call(&command)?;
            if let Reply::Error(message) = reply {
                return Err(render::error(&message));
            }
            let mut parts = reply.into_array().unwrap_or_default().into_iter();
            let cursor = parts.next().and_then(|c| c.text()).unwrap_or_else(|| "0".into());
            let items = parts.next().and_then(Reply::into_array).unwrap_or_default();
            let mut items = items.into_iter();
            loop {
                let element: Vec<Reply> = items.by_ref().take(scan.width).collect();
                if element.len() < scan.width {
                    break;
                }
                scan.buffer.push_back(element);
            }
            scan.done = cursor == "0";
            scan.cursor = cursor.into_bytes();
        }
        Ok(())
    }

    /// One page of a statement read by walking it with a `*SCAN`.
    fn scanned(
        &self,
        client: &mut Client,
        schema: &str,
        statement: &str,
        walk: Walk,
        offset: u64,
        limit: usize,
    ) -> Result<ResultSet> {
        let Walk {
            command,
            cursor_at,
            width,
            shape,
        } = walk;
        let mut memo = self.scan.borrow_mut();
        let fresh = || Scan {
            schema: schema.to_owned(),
            statement: statement.to_owned(),
            command: command.clone(),
            cursor_at,
            width,
            cursor: b"0".to_vec(),
            done: false,
            position: 0,
            buffer: VecDeque::new(),
        };
        let reuse = memo.as_ref().is_some_and(|scan| {
            scan.schema == schema && scan.statement == statement && scan.position == offset
        });
        if !reuse {
            let mut scan = fresh();
            // Walk up to the page asked for.
            while scan.position < offset {
                let want = usize::try_from(offset - scan.position).unwrap_or(usize::MAX).min(10_000);
                Self::pull(client, &mut scan, want)?;
                let drop = want.min(scan.buffer.len());
                if drop == 0 {
                    break;
                }
                scan.buffer.drain(..drop);
                scan.position += drop as u64;
            }
            *memo = Some(scan);
        }
        let Some(scan) = memo.as_mut() else {
            return Err(DbError::protocol("No scan"));
        };
        Self::pull(client, scan, limit + 1)?;
        let items: Vec<Reply> = scan.buffer.iter().take(limit + 1).flatten().cloned().collect();
        let handed = limit.min(scan.buffer.len());
        scan.buffer.drain(..handed);
        scan.position += handed as u64;
        let (columns, rows) = render::rows(shape, Reply::Array(Some(items)), limit + 1)?;
        Ok(ResultSet::query(columns, rows))
    }
}

/// `Redis 7.4.1`, `Valkey 8.0.2`, `Dragonfly 1.25.0` from `INFO server`.
fn product(info: &str) -> String {
    let field = |name: &str| {
        info.lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
            .map(str::trim)
    };
    if let Some(version) = field("dragonfly_version") {
        return format!("Dragonfly {}", version.trim_start_matches(['d', 'f', '-', 'v']));
    }
    if let Some(version) = field("valkey_version") {
        return format!("Valkey {version}");
    }
    let name = match field("server_name") {
        Some("valkey") => "Valkey",
        Some("keydb") => "KeyDB",
        _ => "Redis",
    };
    match field("redis_version") {
        Some(version) => format!("{name} {version}"),
        None => name.to_owned(),
    }
}

/// How many elements a key of each type holds, and what to call them.
fn size_command(subtype: &str) -> Option<(&'static str, &'static str)> {
    Some(match subtype {
        "string" => ("STRLEN", "bytes"),
        "hash" => ("HLEN", "fields"),
        "list" => ("LLEN", "items"),
        "set" => ("SCARD", "members"),
        "zset" => ("ZCARD", "members"),
        "stream" => ("XLEN", "entries"),
        _ => return None,
    })
}

fn ttl_text(pttl: i64) -> String {
    match pttl {
        -1 => "no expiry".to_owned(),
        ms if ms < 0 => "expired".to_owned(),
        ms if ms < 10_000 => format!("TTL {:.1} s", ms as f64 / 1000.0),
        ms => format!("TTL {} s", ms / 1000),
    }
}

impl Session for RedisSession {
    fn server_info(&self) -> Result<ServerInfo> {
        let user = self.params.username.trim();
        Ok(ServerInfo {
            version: self.version.clone(),
            database: format!("db{}", self.main.borrow().db),
            user: if user.is_empty() { "default".into() } else { user.into() },
        })
    }

    fn schemas(&self) -> Result<Vec<String>> {
        let current = self.main.borrow().db;
        self.browse("db0", |client| {
            let count = match client.call(&["CONFIG", "GET", "databases"])? {
                reply @ (Reply::Array(Some(_)) | Reply::Map(_)) => reply
                    .into_array()
                    .and_then(|items| items.get(1).and_then(Reply::text))
                    .and_then(|n| n.parse::<u32>().ok()),
                _ => None,
            };
            let count = match count {
                Some(count) => count,
                // Managed servers often refuse CONFIG: the databases with
                // keys, and the session's own.
                None => {
                    let keyspace = client.call(&["INFO", "keyspace"])?.text().unwrap_or_default();
                    keyspace
                        .lines()
                        .filter_map(|line| line.strip_prefix("db")?.split(':').next()?.parse::<u32>().ok())
                        .max()
                        .map_or(1, |max| max + 1)
                }
            };
            let count = count.clamp(1, 1024).max(current + 1);
            Ok((0..count).map(|n| format!("db{n}")).collect())
        })
    }

    fn objects(&self, _schema: &str) -> Result<Vec<DbObject>> {
        // Keys are listed a page at a time, through `entries`.
        Ok(Vec::new())
    }

    fn columns(&self, schema: &str, key: &str) -> Result<Vec<ColumnInfo>> {
        self.browse(schema, |client| {
            let subtype = client.call(&["TYPE", key])?.text().unwrap_or_default();
            let subtype = lang::subtype(&subtype);
            Ok(lang::columns(&subtype)
                .iter()
                .enumerate()
                .map(|(at, name)| ColumnInfo {
                    name: (*name).to_owned(),
                    data_type: if *name == "score" { "double" } else { "string" }.to_owned(),
                    nullable: false,
                    default: None,
                    primary_key: at == 0 && lang::columns(&subtype).len() > 1,
                    comment: None,
                })
                .collect())
        })
    }

    fn execute(&self, sql: &str, max_rows: usize) -> Result<Vec<ResultSet>> {
        let mut commands = Vec::new();
        let mut starts = Vec::new();
        for range in statements(sql) {
            let line = &sql[range.clone()];
            let position = sql[..range.start].chars().count() as u32 + 1;
            let args = lang::argv(line).map_err(|message| {
                let mut error = DbError::query(message);
                error.position = Some(position);
                error
            })?;
            if let Some(why) = refused(&strings(&args)) {
                let mut error = DbError::unsupported(why);
                error.position = Some(position);
                return Err(error);
            }
            commands.push(args);
            starts.push(position);
        }
        if commands.is_empty() {
            return Ok(Vec::new());
        }
        let replies = self.main.borrow_mut().pipeline(&commands, max_rows)?;
        let mut sets = Vec::new();
        let mut failure = None;
        for ((args, (reply, truncated)), position) in commands.iter().zip(replies).zip(starts) {
            let args = strings(args);
            let name = args.first().map(|a| a.to_ascii_uppercase()).unwrap_or_default();
            let ok = !matches!(reply, Reply::Error(_));
            let mut queued = self.queued.borrow_mut();
            match name.as_str() {
                "MULTI" if ok => *queued = Some(Vec::new()),
                "DISCARD" if ok => *queued = None,
                "SELECT" if ok => {
                    if let Some(db) = args.get(1).and_then(|n| n.parse().ok()) {
                        self.main.borrow_mut().db = db;
                    }
                }
                "EXEC" => {
                    let names = queued.take().unwrap_or_default();
                    if let Some(items) = reply.clone().into_array() {
                        for (args, item) in names.iter().zip(items) {
                            match item {
                                Reply::Error(message) => {
                                    sets.push(ResultSet::command(format!("(error) {message}"), None));
                                }
                                item => sets.push(render::result_set(args, item, false, max_rows)?),
                            }
                        }
                        continue;
                    }
                }
                _ => {}
            }
            if let Some(list) = queued.as_mut()
                && matches!(&reply, Reply::Status(s) if s == "QUEUED")
            {
                list.push(args);
                continue;
            }
            if name == "MULTI" && ok {
                continue;
            }
            drop(queued);
            match render::result_set(&args, reply, truncated, max_rows) {
                Ok(set) => sets.push(set),
                Err(mut error) => {
                    // The rest already ran; the first error is the answer,
                    // pointing at its line.
                    if failure.is_none() {
                        error.position = Some(position);
                        failure = Some(error);
                    }
                }
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(sets),
        }
    }

    fn entries(
        &self,
        schema: &str,
        pattern: Option<&str>,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<EntryPage> {
        let pattern = pattern.map(str::trim).filter(|p| !p.is_empty()).unwrap_or("*");
        let limit = limit.max(1);
        self.browse(schema, |client| {
            let mut cursor = cursor.unwrap_or("0").to_owned();
            let mut names: Vec<Vec<u8>> = Vec::new();
            for _ in 0..SCAN_ROUNDS {
                // A batch cannot be split (the cursor resumes after all of
                // it), so ask for about what is still missing.
                let count = (limit - names.len().min(limit)).clamp(10, SCAN_COUNT).to_string();
                let reply = client.call(&["SCAN", cursor.as_str(), "MATCH", pattern, "COUNT", count.as_str()])?;
                if let Reply::Error(message) = reply {
                    return Err(render::error(&message));
                }
                let mut parts = reply.into_array().unwrap_or_default().into_iter();
                cursor = parts.next().and_then(|c| c.text()).unwrap_or_else(|| "0".into());
                for item in parts.next().and_then(Reply::into_array).unwrap_or_default() {
                    if let Reply::Bulk(Some(name)) = item
                        && !names.contains(&name)
                    {
                        names.push(name);
                    }
                }
                if cursor == "0" || names.len() >= limit {
                    break;
                }
            }
            let types: Vec<Vec<Vec<u8>>> = names
                .iter()
                .map(|name| vec![b"TYPE".to_vec(), name.clone()])
                .collect();
            let types: Vec<String> = client
                .pipeline(&types, 1)?
                .into_iter()
                .map(|(reply, _)| lang::subtype(&reply.text().unwrap_or_default()))
                .collect();
            let mut more = Vec::new();
            for (name, subtype) in names.iter().zip(&types) {
                more.push(vec![b"PTTL".to_vec(), name.clone()]);
                let size = size_command(subtype).map_or("EXISTS", |(command, _)| command);
                more.push(vec![size.as_bytes().to_vec(), name.clone()]);
            }
            let replies = client.pipeline(&more, 1)?;
            let mut entries = Vec::with_capacity(names.len());
            for ((name, subtype), pair) in names.iter().zip(&types).zip(replies.chunks(2)) {
                let pttl = match pair.first() {
                    Some((Reply::Integer(n), _)) => Some(*n),
                    _ => None,
                };
                let size = match (pair.get(1), size_command(subtype)) {
                    (Some((Reply::Integer(n), _)), Some(_)) => Some(*n),
                    _ => None,
                };
                let mut detail = vec![subtype.clone()];
                if let (Some(size), Some((_, unit))) = (size, size_command(subtype)) {
                    detail.push(format!("{size} {unit}"));
                }
                if let Some(pttl) = pttl {
                    detail.push(ttl_text(pttl));
                }
                entries.push(Entry {
                    name: lang::display(name),
                    subtype: Some(subtype.clone()).filter(|s| !s.is_empty() && s != "none"),
                    detail: Some(detail.join(" · ")),
                    size,
                });
            }
            Ok(EntryPage {
                entries,
                next: (cursor != "0").then_some(cursor),
            })
        })
    }

    fn page(&self, schema: &str, statement: &str, offset: u64, limit: usize) -> Result<Vec<ResultSet>> {
        let args = lang::argv(statement.trim()).map_err(DbError::query)?;
        let text = strings(&args);
        let upper: Vec<String> = text.iter().map(|a| a.to_ascii_uppercase()).collect();
        if let Some(why) = refused(&text) {
            return Err(DbError::unsupported(why));
        }
        let key = args.get(1).cloned().unwrap_or_default();
        let whole = upper.get(2).is_some_and(|w| w == "0") && upper.get(3).is_some_and(|w| w == "-1");
        let end = (offset + limit as u64).to_string();
        let start = offset.to_string();
        self.browse(schema, |client| {
            let scan = |name: &str, width: usize, shape: Shape, options: &[Vec<u8>]| {
                let mut command = vec![name.as_bytes().to_vec(), key.clone()];
                command.extend_from_slice(options);
                Walk {
                    command,
                    cursor_at: 2,
                    width,
                    shape,
                }
            };
            // `*SCAN key 0 [MATCH p] [NOVALUES]`: the options after the cursor.
            let options: Vec<Vec<u8>> = args.iter().skip(3).cloned().collect();
            let walk = match upper.first().map(String::as_str) {
                Some("HGETALL") => Some(scan("HSCAN", 2, Shape::Pairs("field", "value"), &[])),
                Some("SMEMBERS") => Some(scan("SSCAN", 1, Shape::Items("member"), &[])),
                Some("HSCAN") if upper.iter().any(|w| w == "NOVALUES") => {
                    Some(scan("HSCAN", 1, Shape::Items("field"), &options))
                }
                Some("HSCAN") => Some(scan("HSCAN", 2, Shape::Pairs("field", "value"), &options)),
                Some("SSCAN") => Some(scan("SSCAN", 1, Shape::Items("member"), &options)),
                Some("ZSCAN") => Some(scan("ZSCAN", 2, Shape::Scored, &options)),
                Some("SCAN") => {
                    let mut command = vec![b"SCAN".to_vec()];
                    command.extend(args.iter().skip(2).cloned());
                    Some(Walk {
                        command,
                        cursor_at: 1,
                        width: 1,
                        shape: Shape::Items("key"),
                    })
                }
                _ => None,
            };
            if let Some(walk) = walk {
                return Ok(vec![self.scanned(client, schema, statement, walk, offset, limit)?]);
            }
            let ranged: Option<Vec<Vec<u8>>> = match upper.first().map(String::as_str) {
                Some("LRANGE") if whole => Some(vec![
                    b"LRANGE".to_vec(),
                    key.clone(),
                    start.clone().into_bytes(),
                    end.clone().into_bytes(),
                ]),
                Some("ZRANGE") if whole => {
                    let mut command = vec![
                        b"ZRANGE".to_vec(),
                        key.clone(),
                        start.clone().into_bytes(),
                        end.clone().into_bytes(),
                    ];
                    command.extend(args.iter().skip(4).cloned());
                    Some(command)
                }
                _ => None,
            };
            if let Some(command) = ranged {
                let reply = client.call(&command)?;
                return Ok(vec![render::result_set(&strings(&command), reply, false, limit + 1)?]);
            }
            // Anything else runs whole; the page is cut from its rows.
            let keep = usize::try_from(offset).unwrap_or(usize::MAX).saturating_add(limit + 1);
            let command = if upper.first().is_some_and(|w| w == "XRANGE" || w == "XREVRANGE")
                && !upper.iter().any(|w| w == "COUNT")
            {
                let mut command = args.clone();
                command.extend([b"COUNT".to_vec(), keep.to_string().into_bytes()]);
                command
            } else {
                args.clone()
            };
            let (reply, truncated) = client.pipeline(std::slice::from_ref(&command), keep)?.remove(0);
            let mut set = render::result_set(&text, reply, truncated, keep)?;
            let width = set.columns.len();
            set.rows = skip_rows(set.rows, width, usize::try_from(offset).unwrap_or(usize::MAX));
            set.affected = Some((set.rows.ends.len() / width.max(1)) as u64);
            Ok(vec![set])
        })
    }

    fn count(&self, schema: &str, statement: &str) -> Result<Option<u64>> {
        let args = lang::argv(statement.trim()).map_err(DbError::query)?;
        let upper: Vec<String> = strings(&args).iter().map(|a| a.to_ascii_uppercase()).collect();
        let Some(key) = args.get(1) else {
            return Ok(None);
        };
        let whole = upper.get(2).is_some_and(|w| w == "0") && upper.get(3).is_some_and(|w| w == "-1");
        let filtered = upper.iter().any(|w| w == "MATCH");
        let length = match upper[0].as_str() {
            "HGETALL" => "HLEN",
            "HSCAN" if !filtered => "HLEN",
            "SMEMBERS" => "SCARD",
            "SSCAN" if !filtered => "SCARD",
            "ZSCAN" if !filtered => "ZCARD",
            "LRANGE" if whole => "LLEN",
            "ZRANGE" if whole && upper.len() <= 5 => "ZCARD",
            "XRANGE" | "XREVRANGE" if upper.len() == 4 && matches!(upper[2].as_str(), "-" | "+") => "XLEN",
            "GET" | "JSON.GET" | "TYPE" => return Ok(Some(1)),
            _ => return Ok(None),
        };
        self.browse(schema, |client| {
            Ok(match client.call(&[length.as_bytes(), key.as_slice()])? {
                Reply::Integer(n) => u64::try_from(n).ok(),
                _ => None,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn products_are_told_apart() {
        assert_eq!(product("# Server\r\nredis_version:7.4.1\r\n"), "Redis 7.4.1");
        assert_eq!(
            product("redis_version:7.2.4\r\nserver_name:valkey\r\nvalkey_version:8.0.2\r\n"),
            "Valkey 8.0.2"
        );
        assert_eq!(product("redis_version:7.4.0\r\ndragonfly_version:df-v1.25.0\r\n"), "Dragonfly 1.25.0");
    }

    #[test]
    fn commands_that_never_answer_are_refused() {
        let args = |line: &str| line.split(' ').map(str::to_owned).collect::<Vec<_>>();
        assert!(refused(&args("SUBSCRIBE news")).is_some());
        assert!(refused(&args("BLPOP q 0")).is_some());
        assert!(refused(&args("BLPOP q 5")).is_none());
        assert!(refused(&args("XREAD BLOCK 0 STREAMS s $")).is_some());
        assert!(refused(&args("GET k")).is_none());
    }

    #[test]
    fn rows_are_skipped_whole() {
        let rows = Rows {
            text: "abcd".into(),
            ends: vec![1, 2 | (1 << 31), 3, 4],
        };
        let rows = skip_rows(rows, 2, 1);
        assert_eq!(rows.text, "cd");
        assert_eq!(rows.ends, vec![1, 2]);
        let null = Rows {
            text: "ab".into(),
            ends: vec![1, 1 | (1 << 31), 2, 2 | (1 << 31)],
        };
        assert_eq!(skip_rows(null, 2, 1).ends, vec![1, 1 | (1 << 31)]);
    }

    #[test]
    fn ttls_read_naturally() {
        assert_eq!(ttl_text(-1), "no expiry");
        assert_eq!(ttl_text(3_600_000), "TTL 3600 s");
        assert_eq!(ttl_text(1500), "TTL 1.5 s");
    }
}
