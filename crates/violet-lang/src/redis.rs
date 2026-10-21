//! Redis commands, as `redis-cli` reads them: one command per line, words
//! split on blanks, `"..."` with escapes (`\n`, `\t`, `\xHH`, `\"`) and
//! `'...'` taken literally. Lines starting with `#` or `//` are comments.
//!
//! Valkey, KeyDB, Dragonfly and Garnet speak the same language.

use std::ops::Range;

/// What a run of text is, for colouring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    /// The command's name (and a container's subcommand, `CONFIG GET`).
    Command,
    /// A command Violet does not know, in the command's place.
    Unknown,
    Argument,
    String,
    Number,
    Comment,
    Whitespace,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    /// Byte range in the source.
    pub range: Range<usize>,
}

/// How a command touches data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// Only reads keys.
    Read,
    /// May change keys.
    Write,
    /// Acts on the server or the connection.
    Admin,
}

/// A command Violet knows: for completion, colours and deciding what a
/// script does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Command {
    /// Upper case; containers' subcommands after a space (`CONFIG GET`).
    pub name: &'static str,
    /// Its arguments, as the documentation writes them.
    pub syntax: &'static str,
    pub access: Access,
}

const fn c(name: &'static str, syntax: &'static str, access: Access) -> Command {
    Command {
        name,
        syntax,
        access,
    }
}

use Access::{Admin as A, Read as R, Write as W};

/// The commands Violet knows, sorted by name.
pub const COMMANDS: &[Command] = &[
    c("ACL CAT", "[category]", A),
    c("ACL DELUSER", "username [username ...]", A),
    c("ACL GETUSER", "username", A),
    c("ACL LIST", "", A),
    c("ACL LOG", "[count | RESET]", A),
    c("ACL SETUSER", "username [rule [rule ...]]", A),
    c("ACL USERS", "", A),
    c("ACL WHOAMI", "", A),
    c("APPEND", "key value", W),
    c("AUTH", "[username] password", A),
    c("BGREWRITEAOF", "", A),
    c("BGSAVE", "[SCHEDULE]", A),
    c("BITCOUNT", "key [start end [BYTE | BIT]]", R),
    c(
        "BITFIELD",
        "key [GET encoding offset | SET encoding offset value | INCRBY encoding offset increment ...]",
        W,
    ),
    c("BITOP", "AND | OR | XOR | NOT destkey key [key ...]", W),
    c("BITPOS", "key bit [start [end [BYTE | BIT]]]", R),
    c(
        "BLMOVE",
        "source destination LEFT | RIGHT LEFT | RIGHT timeout",
        W,
    ),
    c("BLPOP", "key [key ...] timeout", W),
    c("BRPOP", "key [key ...] timeout", W),
    c("BZPOPMAX", "key [key ...] timeout", W),
    c("BZPOPMIN", "key [key ...] timeout", W),
    c("CLIENT GETNAME", "", A),
    c("CLIENT ID", "", A),
    c("CLIENT INFO", "", A),
    c(
        "CLIENT KILL",
        "[ID client-id] [ADDR ip:port] [USER username] ...",
        A,
    ),
    c(
        "CLIENT LIST",
        "[TYPE NORMAL | MASTER | REPLICA | PUBSUB]",
        A,
    ),
    c("CLIENT SETNAME", "connection-name", A),
    c("CLUSTER INFO", "", A),
    c("CLUSTER NODES", "", A),
    c("COMMAND COUNT", "", A),
    c("COMMAND DOCS", "[command-name [command-name ...]]", A),
    c("COMMAND INFO", "[command-name [command-name ...]]", A),
    c("CONFIG GET", "parameter [parameter ...]", A),
    c("CONFIG RESETSTAT", "", A),
    c("CONFIG REWRITE", "", A),
    c("CONFIG SET", "parameter value [parameter value ...]", A),
    c(
        "COPY",
        "source destination [DB destination-db] [REPLACE]",
        W,
    ),
    c("DBSIZE", "", R),
    c("DEBUG", "subcommand [argument ...]", A),
    c("DECR", "key", W),
    c("DECRBY", "key decrement", W),
    c("DEL", "key [key ...]", W),
    c("DISCARD", "", A),
    c("DUMP", "key", R),
    c("ECHO", "message", R),
    c("EVAL", "script numkeys [key [key ...]] [arg [arg ...]]", W),
    c("EVALSHA", "sha1 numkeys [key [key ...]] [arg [arg ...]]", W),
    c(
        "EVAL_RO",
        "script numkeys [key [key ...]] [arg [arg ...]]",
        R,
    ),
    c("EXEC", "", A),
    c("EXISTS", "key [key ...]", R),
    c("EXPIRE", "key seconds [NX | XX | GT | LT]", W),
    c("EXPIREAT", "key unix-time-seconds [NX | XX | GT | LT]", W),
    c("EXPIRETIME", "key", R),
    c(
        "FCALL",
        "function numkeys [key [key ...]] [arg [arg ...]]",
        W,
    ),
    c(
        "FCALL_RO",
        "function numkeys [key [key ...]] [arg [arg ...]]",
        R,
    ),
    c("FLUSHALL", "[ASYNC | SYNC]", W),
    c("FLUSHDB", "[ASYNC | SYNC]", W),
    c("FUNCTION FLUSH", "[ASYNC | SYNC]", A),
    c(
        "FUNCTION LIST",
        "[LIBRARYNAME library-name-pattern] [WITHCODE]",
        A,
    ),
    c(
        "GEOADD",
        "key [NX | XX] [CH] longitude latitude member [longitude latitude member ...]",
        W,
    ),
    c("GEODIST", "key member1 member2 [M | KM | FT | MI]", R),
    c("GEOHASH", "key [member [member ...]]", R),
    c("GEOPOS", "key [member [member ...]]", R),
    c(
        "GEOSEARCH",
        "key FROMMEMBER member | FROMLONLAT longitude latitude BYRADIUS radius M | KM | FT | MI | BYBOX width height M | KM | FT | MI [ASC | DESC] [COUNT count [ANY]] [WITHCOORD] [WITHDIST] [WITHHASH]",
        R,
    ),
    c("GET", "key", R),
    c("GETBIT", "key offset", R),
    c("GETDEL", "key", W),
    c(
        "GETEX",
        "key [EX seconds | PX milliseconds | EXAT unix-time-seconds | PXAT unix-time-milliseconds | PERSIST]",
        W,
    ),
    c("GETRANGE", "key start end", R),
    c("GETSET", "key value", W),
    c("HDEL", "key field [field ...]", W),
    c(
        "HELLO",
        "[protover [AUTH username password] [SETNAME clientname]]",
        A,
    ),
    c("HEXISTS", "key field", R),
    c(
        "HEXPIRE",
        "key seconds [NX | XX | GT | LT] FIELDS numfields field [field ...]",
        W,
    ),
    c("HGET", "key field", R),
    c("HGETALL", "key", R),
    c("HINCRBY", "key field increment", W),
    c("HINCRBYFLOAT", "key field increment", W),
    c("HKEYS", "key", R),
    c("HLEN", "key", R),
    c("HMGET", "key field [field ...]", R),
    c("HMSET", "key field value [field value ...]", W),
    c("HRANDFIELD", "key [count [WITHVALUES]]", R),
    c(
        "HSCAN",
        "key cursor [MATCH pattern] [COUNT count] [NOVALUES]",
        R,
    ),
    c("HSET", "key field value [field value ...]", W),
    c("HSETNX", "key field value", W),
    c("HSTRLEN", "key field", R),
    c("HTTL", "key FIELDS numfields field [field ...]", R),
    c("HVALS", "key", R),
    c("INCR", "key", W),
    c("INCRBY", "key increment", W),
    c("INCRBYFLOAT", "key increment", W),
    c("INFO", "[section [section ...]]", A),
    c("JSON.ARRAPPEND", "key path value [value ...]", W),
    c("JSON.ARRLEN", "key [path]", R),
    c("JSON.DEL", "key [path]", W),
    c(
        "JSON.GET",
        "key [INDENT indent] [NEWLINE newline] [SPACE space] [path [path ...]]",
        R,
    ),
    c("JSON.MGET", "key [key ...] path", R),
    c("JSON.NUMINCRBY", "key path value", W),
    c("JSON.OBJKEYS", "key [path]", R),
    c("JSON.SET", "key path value [NX | XX]", W),
    c("JSON.TYPE", "key [path]", R),
    c("KEYS", "pattern", R),
    c("LASTSAVE", "", A),
    c("LINDEX", "key index", R),
    c("LINSERT", "key BEFORE | AFTER pivot element", W),
    c("LLEN", "key", R),
    c("LMOVE", "source destination LEFT | RIGHT LEFT | RIGHT", W),
    c("LPOP", "key [count]", W),
    c(
        "LPOS",
        "key element [RANK rank] [COUNT num-matches] [MAXLEN len]",
        R,
    ),
    c("LPUSH", "key element [element ...]", W),
    c("LPUSHX", "key element [element ...]", W),
    c("LRANGE", "key start stop", R),
    c("LREM", "key count element", W),
    c("LSET", "key index element", W),
    c("LTRIM", "key start stop", W),
    c("MEMORY USAGE", "key [SAMPLES count]", R),
    c("MGET", "key [key ...]", R),
    c(
        "MIGRATE",
        "host port key | \"\" destination-db timeout [COPY] [REPLACE] [AUTH password | AUTH2 username password] [KEYS key [key ...]]",
        W,
    ),
    c("MONITOR", "", A),
    c("MOVE", "key db", W),
    c("MSET", "key value [key value ...]", W),
    c("MSETNX", "key value [key value ...]", W),
    c("MULTI", "", A),
    c("OBJECT ENCODING", "key", R),
    c("OBJECT FREQ", "key", R),
    c("OBJECT IDLETIME", "key", R),
    c("OBJECT REFCOUNT", "key", R),
    c("PERSIST", "key", W),
    c("PEXPIRE", "key milliseconds [NX | XX | GT | LT]", W),
    c(
        "PEXPIREAT",
        "key unix-time-milliseconds [NX | XX | GT | LT]",
        W,
    ),
    c("PEXPIRETIME", "key", R),
    c("PFADD", "key [element [element ...]]", W),
    c("PFCOUNT", "key [key ...]", R),
    c("PFMERGE", "destkey [sourcekey [sourcekey ...]]", W),
    c("PING", "[message]", R),
    c("PSETEX", "key milliseconds value", W),
    c("PSUBSCRIBE", "pattern [pattern ...]", A),
    c("PTTL", "key", R),
    c("PUBLISH", "channel message", A),
    c("PUBSUB CHANNELS", "[pattern]", A),
    c("RANDOMKEY", "", R),
    c("RENAME", "key newkey", W),
    c("RENAMENX", "key newkey", W),
    c(
        "RESTORE",
        "key ttl serialized-value [REPLACE] [ABSTTL] [IDLETIME seconds] [FREQ frequency]",
        W,
    ),
    c("ROLE", "", A),
    c("RPOP", "key [count]", W),
    c("RPUSH", "key element [element ...]", W),
    c("RPUSHX", "key element [element ...]", W),
    c("SADD", "key member [member ...]", W),
    c("SAVE", "", A),
    c(
        "SCAN",
        "cursor [MATCH pattern] [COUNT count] [TYPE type]",
        R,
    ),
    c("SCARD", "key", R),
    c("SCRIPT FLUSH", "[ASYNC | SYNC]", A),
    c("SCRIPT LOAD", "script", A),
    c("SDIFF", "key [key ...]", R),
    c("SDIFFSTORE", "destination key [key ...]", W),
    c("SELECT", "index", A),
    c(
        "SET",
        "key value [NX | XX] [GET] [EX seconds | PX milliseconds | EXAT unix-time-seconds | PXAT unix-time-milliseconds | KEEPTTL]",
        W,
    ),
    c("SETBIT", "key offset value", W),
    c("SETEX", "key seconds value", W),
    c("SETNX", "key value", W),
    c("SETRANGE", "key offset value", W),
    c("SHUTDOWN", "[NOSAVE | SAVE] [NOW] [FORCE] [ABORT]", A),
    c("SINTER", "key [key ...]", R),
    c("SINTERCARD", "numkeys key [key ...] [LIMIT limit]", R),
    c("SINTERSTORE", "destination key [key ...]", W),
    c("SISMEMBER", "key member", R),
    c("SLOWLOG GET", "[count]", A),
    c("SLOWLOG RESET", "", A),
    c("SMEMBERS", "key", R),
    c("SMISMEMBER", "key member [member ...]", R),
    c("SMOVE", "source destination member", W),
    c(
        "SORT",
        "key [BY pattern] [LIMIT offset count] [GET pattern [GET pattern ...]] [ASC | DESC] [ALPHA] [STORE destination]",
        W,
    ),
    c(
        "SORT_RO",
        "key [BY pattern] [LIMIT offset count] [GET pattern [GET pattern ...]] [ASC | DESC] [ALPHA]",
        R,
    ),
    c("SPOP", "key [count]", W),
    c("SRANDMEMBER", "key [count]", R),
    c("SREM", "key member [member ...]", W),
    c("SSCAN", "key cursor [MATCH pattern] [COUNT count]", R),
    c("STRLEN", "key", R),
    c("SUBSCRIBE", "channel [channel ...]", A),
    c("SUNION", "key [key ...]", R),
    c("SUNIONSTORE", "destination key [key ...]", W),
    c("SWAPDB", "index1 index2", W),
    c("TIME", "", R),
    c("TOUCH", "key [key ...]", R),
    c("TTL", "key", R),
    c("TYPE", "key", R),
    c("UNLINK", "key [key ...]", W),
    c("UNWATCH", "", A),
    c("WAIT", "numreplicas timeout", A),
    c("WATCH", "key [key ...]", A),
    c("XACK", "key group id [id ...]", W),
    c(
        "XADD",
        "key [NOMKSTREAM] [MAXLEN | MINID [= | ~] threshold] * | id field value [field value ...]",
        W,
    ),
    c("XDEL", "key id [id ...]", W),
    c("XGROUP CREATE", "key group id | $ [MKSTREAM]", W),
    c("XINFO STREAM", "key [FULL [COUNT count]]", R),
    c("XLEN", "key", R),
    c("XRANGE", "key start end [COUNT count]", R),
    c(
        "XREAD",
        "[COUNT count] [BLOCK milliseconds] STREAMS key [key ...] id [id ...]",
        R,
    ),
    c(
        "XREADGROUP",
        "GROUP group consumer [COUNT count] [BLOCK milliseconds] [NOACK] STREAMS key [key ...] id [id ...]",
        W,
    ),
    c("XREVRANGE", "key end start [COUNT count]", R),
    c(
        "XTRIM",
        "key MAXLEN | MINID [= | ~] threshold [LIMIT count]",
        W,
    ),
    c(
        "ZADD",
        "key [NX | XX] [GT | LT] [CH] [INCR] score member [score member ...]",
        W,
    ),
    c("ZCARD", "key", R),
    c("ZCOUNT", "key min max", R),
    c("ZDIFF", "numkeys key [key ...] [WITHSCORES]", R),
    c("ZINCRBY", "key increment member", W),
    c(
        "ZINTER",
        "numkeys key [key ...] [WEIGHTS weight [weight ...]] [AGGREGATE SUM | MIN | MAX] [WITHSCORES]",
        R,
    ),
    c("ZLEXCOUNT", "key min max", R),
    c("ZMSCORE", "key member [member ...]", R),
    c("ZPOPMAX", "key [count]", W),
    c("ZPOPMIN", "key [count]", W),
    c("ZRANDMEMBER", "key [count [WITHSCORES]]", R),
    c(
        "ZRANGE",
        "key start stop [BYSCORE | BYLEX] [REV] [LIMIT offset count] [WITHSCORES]",
        R,
    ),
    c(
        "ZRANGEBYSCORE",
        "key min max [WITHSCORES] [LIMIT offset count]",
        R,
    ),
    c("ZRANK", "key member [WITHSCORE]", R),
    c("ZREM", "key member [member ...]", W),
    c("ZREMRANGEBYSCORE", "key min max", W),
    c("ZREVRANGE", "key start stop [WITHSCORES]", R),
    c("ZREVRANK", "key member [WITHSCORE]", R),
    c("ZSCAN", "key cursor [MATCH pattern] [COUNT count]", R),
    c("ZSCORE", "key member", R),
    c(
        "ZUNION",
        "numkeys key [key ...] [WEIGHTS weight [weight ...]] [AGGREGATE SUM | MIN | MAX] [WITHSCORES]",
        R,
    ),
    c(
        "ZUNIONSTORE",
        "destination numkeys key [key ...] [WEIGHTS weight [weight ...]] [AGGREGATE SUM | MIN | MAX]",
        W,
    ),
];

/// Commands that take subcommands: their name alone is not a command.
const CONTAINERS: &[&str] = &[
    "ACL", "CLIENT", "CLUSTER", "COMMAND", "CONFIG", "FUNCTION", "MEMORY", "OBJECT", "PUBSUB",
    "SCRIPT", "SLOWLOG", "XGROUP", "XINFO",
];

/// Whether `name` (upper case) takes subcommands.
#[must_use]
pub fn is_container(name: &str) -> bool {
    CONTAINERS.contains(&name)
}

/// The command `words` start with, by its first word or, for a container,
/// its first two.
#[must_use]
pub fn command<S: AsRef<str>>(words: &[S]) -> Option<&'static Command> {
    let first = words.first()?.as_ref().to_ascii_uppercase();
    let name = if is_container(&first) {
        format!("{first} {}", words.get(1)?.as_ref().to_ascii_uppercase())
    } else {
        first
    };
    COMMANDS
        .binary_search_by(|command| command.name.cmp(name.as_str()))
        .ok()
        .map(|at| &COMMANDS[at])
}

fn is_blank(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r')
}

/// Whether a line, without its leading blanks, is a comment.
fn is_comment(line: &str) -> bool {
    line.starts_with('#') || line.starts_with("//")
}

/// The end of a quoted word that starts at `at` with `quote`: past the
/// closing quote, or the end of the line when there is none.
fn quoted_end(bytes: &[u8], mut at: usize, quote: u8) -> usize {
    at += 1;
    while at < bytes.len() && bytes[at] != b'\n' {
        if quote == b'"' && bytes[at] == b'\\' && at + 1 < bytes.len() && bytes[at + 1] != b'\n' {
            at += 2;
            continue;
        }
        if bytes[at] == quote {
            return at + 1;
        }
        at += 1;
    }
    at
}

/// The words of every line, for colouring. Never fails, and covers all of
/// `source`.
#[must_use]
pub fn tokens(source: &str) -> Vec<Token> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut at = 0;
    // Words of the current line so far, to tell the command from its
    // arguments.
    let mut words: Vec<String> = Vec::new();
    while at < bytes.len() {
        let start = at;
        let b = bytes[at];
        if b == b'\n' || is_blank(b) {
            if b == b'\n' {
                words.clear();
            }
            at += 1;
            while at < bytes.len() && is_blank(bytes[at]) {
                at += 1;
            }
            tokens.push(Token {
                kind: TokenKind::Whitespace,
                range: start..at,
            });
            continue;
        }
        if words.is_empty() && is_comment(&source[at..]) {
            at = source[at..].find('\n').map_or(bytes.len(), |n| at + n);
            tokens.push(Token {
                kind: TokenKind::Comment,
                range: start..at,
            });
            continue;
        }
        let kind;
        if b == b'"' || b == b'\'' {
            at = quoted_end(bytes, at, b);
            kind = TokenKind::String;
            words.push(String::new());
        } else {
            while at < bytes.len() && !is_blank(bytes[at]) && bytes[at] != b'\n' {
                at += 1;
            }
            let word = &source[start..at];
            words.push(word.to_owned());
            kind = if words.len() == 1 {
                let upper = word.to_ascii_uppercase();
                if is_container(&upper) || command(&[word]).is_some() {
                    TokenKind::Command
                } else {
                    TokenKind::Unknown
                }
            } else if words.len() == 2
                && is_container(&words[0].to_ascii_uppercase())
                && command(&words).is_some()
            {
                TokenKind::Command
            } else if word.parse::<f64>().is_ok() {
                TokenKind::Number
            } else {
                TokenKind::Argument
            };
        }
        tokens.push(Token {
            kind,
            range: start..at,
        });
    }
    tokens
}

/// The byte range of each command: every line that is neither blank nor a
/// comment, without its surrounding blanks.
#[must_use]
pub fn statements(source: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut start = 0;
    for line in source.split_inclusive('\n') {
        let end = start + line.len();
        let trimmed_start = start + (line.len() - line.trim_start().len());
        let trimmed_end = start + line.trim_end().len();
        if trimmed_end > trimmed_start && !is_comment(&source[trimmed_start..trimmed_end]) {
            ranges.push(trimmed_start..trimmed_end);
        }
        start = end;
    }
    ranges
}

/// The first word of a command, upper case.
fn first_word(statement: &str) -> String {
    statement
        .split(|c: char| c.is_ascii_whitespace())
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase()
}

/// What runs for the cursor at byte `cursor`: the command on its line, or
/// when that line is inside a `MULTI` ... `EXEC` (or `DISCARD`) block, the
/// whole block, which only means something together. On a blank line, the
/// closest command before it.
#[must_use]
pub fn statement_at(source: &str, cursor: usize) -> Option<Range<usize>> {
    let ranges = statements(source);
    let line_start = source[..cursor.min(source.len())]
        .rfind('\n')
        .map_or(0, |at| at + 1);
    let index = ranges
        .iter()
        .rposition(|range| range.start <= cursor.max(line_start) || range.start < line_start)
        .or_else(|| (!ranges.is_empty()).then_some(0))?;
    // The block around it, if any.
    let mut open = None;
    for (at, range) in ranges.iter().enumerate() {
        match first_word(&source[range.clone()]).as_str() {
            "MULTI" => open = Some(at),
            "EXEC" | "DISCARD" => {
                if let Some(begin) = open.take()
                    && (begin..=at).contains(&index)
                {
                    return Some(ranges[begin].start..range.end);
                }
            }
            _ => {}
        }
        if at > index && open.is_none() {
            break;
        }
    }
    if let Some(begin) = open
        && begin <= index
    {
        return Some(ranges[begin].start..ranges.last()?.end);
    }
    Some(ranges[index].clone())
}

/// Splits one command into its arguments, as `redis-cli` does.
///
/// # Errors
///
/// An unbalanced quote, or a closing quote not followed by a blank.
pub fn argv(line: &str) -> Result<Vec<Vec<u8>>, String> {
    let bytes = line.as_bytes();
    let mut args = Vec::new();
    let mut at = 0;
    loop {
        while at < bytes.len() && (is_blank(bytes[at]) || bytes[at] == b'\n') {
            at += 1;
        }
        if at == bytes.len() {
            return Ok(args);
        }
        let mut arg = Vec::new();
        match bytes[at] {
            b'"' => {
                at += 1;
                loop {
                    let Some(&b) = bytes.get(at) else {
                        return Err("Unbalanced quotes".into());
                    };
                    match b {
                        b'\\' if at + 1 < bytes.len() => {
                            let next = bytes[at + 1];
                            let hex = |b: u8| (b as char).to_digit(16);
                            if next == b'x'
                                && let (Some(&h), Some(&l)) = (bytes.get(at + 2), bytes.get(at + 3))
                                && let (Some(h), Some(l)) = (hex(h), hex(l))
                            {
                                arg.push((h * 16 + l) as u8);
                                at += 4;
                                continue;
                            }
                            arg.push(match next {
                                b'n' => b'\n',
                                b'r' => b'\r',
                                b't' => b'\t',
                                b'b' => 8,
                                b'a' => 7,
                                other => other,
                            });
                            at += 2;
                        }
                        b'"' => {
                            at += 1;
                            break;
                        }
                        other => {
                            arg.push(other);
                            at += 1;
                        }
                    }
                }
            }
            b'\'' => {
                at += 1;
                loop {
                    let Some(&b) = bytes.get(at) else {
                        return Err("Unbalanced quotes".into());
                    };
                    if b == b'\\' && bytes.get(at + 1) == Some(&b'\'') {
                        arg.push(b'\'');
                        at += 2;
                    } else if b == b'\'' {
                        at += 1;
                        break;
                    } else {
                        arg.push(b);
                        at += 1;
                    }
                }
            }
            _ => {
                while at < bytes.len() && !is_blank(bytes[at]) && bytes[at] != b'\n' {
                    arg.push(bytes[at]);
                    at += 1;
                }
                args.push(arg);
                continue;
            }
        }
        if at < bytes.len() && !is_blank(bytes[at]) && bytes[at] != b'\n' {
            return Err("A closing quote must be followed by a space".into());
        }
        args.push(arg);
    }
}

/// `arg` as one word [`argv`] reads back: bare when it can be, else in
/// double quotes with escapes.
#[must_use]
pub fn quote(arg: &[u8]) -> String {
    let bare = !arg.is_empty()
        && arg
            .iter()
            .all(|&b| b.is_ascii_graphic() && !matches!(b, b'"' | b'\'' | b'\\'))
        && !arg.starts_with(b"#")
        && !arg.starts_with(b"//");
    if bare {
        return String::from_utf8_lossy(arg).into_owned();
    }
    let mut out = String::from("\"");
    let text = String::from_utf8_lossy(arg);
    let valid = std::str::from_utf8(arg).is_ok();
    if valid {
        for ch in text.chars() {
            match ch {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if c.is_control() => out.push_str(&format!("\\x{:02x}", c as u32)),
                c => out.push(c),
            }
        }
    } else {
        for &b in arg {
            match b {
                b'"' => out.push_str("\\\""),
                b'\\' => out.push_str("\\\\"),
                b if b.is_ascii_graphic() || b == b' ' => out.push(b as char),
                b => out.push_str(&format!("\\x{b:02x}")),
            }
        }
    }
    out.push('"');
    out
}

/// Text for a cell: UTF-8 as is, other bytes escaped as `redis-cli` does
/// (`\xHH`), so [`argv`] reads `"..."` around it back to the same bytes.
#[must_use]
pub fn display(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => {
            let quoted = quote(bytes);
            quoted[1..quoted.len() - 1].to_owned()
        }
    }
}

/// The bytes a cell's text stands for: the inverse of [`display`]. Text
/// that only reads as binary once its `\xHH`, `\\` and `\"` escapes are
/// undone (not UTF-8 any more) is that binary; any other text is itself,
/// backslashes included. (A UTF-8 value spelling out an invalid escape,
/// such as the four characters `\xff`, reads as binary: the one case
/// that does not round-trip.)
#[must_use]
pub fn raw(text: &str) -> Vec<u8> {
    if !text.contains('\\') {
        return text.as_bytes().to_vec();
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[at] == b'\\' {
            match bytes.get(at + 1) {
                Some(b'\\' | b'"') => {
                    out.push(bytes[at + 1]);
                    at += 2;
                    continue;
                }
                Some(b'x') => {
                    if let (Some(h), Some(l)) = (
                        bytes.get(at + 2).copied().and_then(hex),
                        bytes.get(at + 3).copied().and_then(hex),
                    ) {
                        out.push((h * 16 + l) as u8);
                        at += 4;
                        continue;
                    }
                }
                _ => {}
            }
        }
        out.push(bytes[at]);
        at += 1;
    }
    if std::str::from_utf8(&out).is_ok() {
        text.as_bytes().to_vec()
    } else {
        out
    }
}

/// A cell's or a key's shown text as one argument, for the exact bytes it
/// stands for.
#[must_use]
pub fn argument(text: &str) -> String {
    quote(&raw(text))
}

/// The words of a command as text, for deciding what it does.
fn words(statement: &str) -> Vec<String> {
    argv(statement)
        .unwrap_or_else(|_| {
            statement
                .split_ascii_whitespace()
                .map(|word| word.as_bytes().to_vec())
                .collect()
        })
        .into_iter()
        .map(|word| String::from_utf8_lossy(&word).into_owned())
        .collect()
}

/// What the app may do with a script besides running it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Every command only reads keys.
    pub reads_only: bool,
}

#[must_use]
pub fn plan(source: &str) -> Plan {
    let ranges = statements(source);
    let reads_only = !ranges.is_empty()
        && ranges.iter().all(|range| {
            command(&words(&source[range.clone()])).is_some_and(|c| c.access == Access::Read)
        });
    Plan { reads_only }
}

/// Why each dangerous command in `source` deserves a second look, such as
/// `FLUSHALL deletes every key of every database`. Empty when there is
/// none.
#[must_use]
pub fn dangers(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    for range in statements(source) {
        let words = words(&source[range]);
        let Some(first) = words.first().map(|w| w.to_ascii_uppercase()) else {
            continue;
        };
        let second = words.get(1).map(|w| w.to_ascii_uppercase());
        let why = match (first.as_str(), second.as_deref()) {
            ("FLUSHALL", _) => "FLUSHALL deletes every key of every database".to_owned(),
            ("FLUSHDB", _) => "FLUSHDB deletes every key of the database".to_owned(),
            ("KEYS", _) => format!(
                "KEYS {} reads the whole keyspace at once and blocks the server meanwhile; \
                 the tree uses SCAN instead",
                words.get(1).map_or("", String::as_str)
            ),
            ("SWAPDB", _) => "SWAPDB swaps two whole databases".to_owned(),
            ("SHUTDOWN", _) => "SHUTDOWN stops the server".to_owned(),
            ("DEBUG", _) => "DEBUG can crash or freeze the server".to_owned(),
            ("CONFIG", Some("SET" | "RESETSTAT" | "REWRITE")) => {
                format!(
                    "CONFIG {} changes the server's configuration",
                    second.unwrap_or_default()
                )
            }
            ("SCRIPT" | "FUNCTION", Some("FLUSH")) => {
                format!("{first} FLUSH deletes every stored script")
            }
            ("CLIENT", Some("KILL")) => "CLIENT KILL drops other connections".to_owned(),
            ("ACL", Some("DELUSER")) => "ACL DELUSER deletes users".to_owned(),
            _ => continue,
        };
        found.push(why);
    }
    found
}

/// Where in a command's words a password is, if anywhere: `AUTH [user]
/// pass`, `HELLO ... AUTH user pass`, `CONFIG SET requirepass|masterauth
/// value`, `ACL SETUSER` rules `>pass`, `<pass`, `#hash` and `!hash`,
/// `MIGRATE ... AUTH pass | AUTH2 user pass`.
fn secret_words(words: &[String]) -> Vec<usize> {
    let upper: Vec<String> = words.iter().map(|w| w.to_ascii_uppercase()).collect();
    let first = upper.first().map_or("", String::as_str);
    let mut secret = Vec::new();
    match first {
        "AUTH" => secret.extend(1..words.len()),
        "HELLO" | "MIGRATE" => {
            for (at, word) in upper.iter().enumerate() {
                match word.as_str() {
                    "AUTH" if first == "HELLO" => secret.extend([at + 1, at + 2]),
                    "AUTH" => secret.push(at + 1),
                    "AUTH2" => secret.extend([at + 1, at + 2]),
                    _ => {}
                }
            }
        }
        "CONFIG" if upper.get(1).is_some_and(|w| w == "SET") => {
            let mut at = 2;
            while at + 1 < words.len() {
                if upper[at].contains("PASS") || upper[at].contains("AUTH") {
                    secret.push(at + 1);
                }
                at += 2;
            }
        }
        "ACL" if upper.get(1).is_some_and(|w| w == "SETUSER") => {
            secret
                .extend((3..words.len()).filter(|&at| words[at].starts_with(['>', '<', '#', '!'])));
        }
        _ => {}
    }
    secret.retain(|&at| at < words.len());
    secret
}

/// Whether `source` may carry a password ([`redact_secrets`] hides it).
#[must_use]
pub fn sets_secret(source: &str) -> bool {
    statements(source)
        .into_iter()
        .any(|range| !secret_words(&words(&source[range])).is_empty())
}

/// `source` with every password replaced by `•••`, so the history never
/// keeps one. Other lines are kept as they are.
#[must_use]
pub fn redact_secrets(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    for line in source.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        let words = words(body);
        let secret = secret_words(&words);
        if secret.is_empty() || is_comment(body.trim_start()) {
            out.push_str(line);
            continue;
        }
        let rebuilt: Vec<String> = words
            .iter()
            .enumerate()
            .map(|(at, word)| {
                if !secret.contains(&at) {
                    quote(word.as_bytes())
                } else if word.starts_with(['>', '<', '#', '!']) && at > 2 {
                    format!("{}•••", &word[..1])
                } else {
                    "•••".to_owned()
                }
            })
            .collect();
        out.push_str(&rebuilt.join(" "));
        out.push_str(&line[body.len()..]);
    }
    out
}

/// Key types Violet shows, as `TYPE` names them (and RedisJSON's
/// `ReJSON-RL` as `json`).
pub const TYPES: &[&str] = &["string", "hash", "list", "set", "zset", "stream", "json"];

/// The type `TYPE` answers with, as [`TYPES`] names it.
#[must_use]
pub fn subtype(type_reply: &str) -> String {
    match type_reply {
        "ReJSON-RL" => "json".to_owned(),
        other => other.to_ascii_lowercase(),
    }
}

/// The command a data view reads a key of type `subtype` with. `filter` is
/// a glob on fields or members, for the types that scan.
#[must_use]
pub fn browse(subtype: Option<&str>, key: &str, filter: Option<&str>) -> String {
    let key = argument(key);
    let filter = filter.map(str::trim).filter(|f| !f.is_empty());
    let matching = |scan: &str| match filter {
        Some(filter) => format!("{scan} {key} 0 MATCH {}", quote(filter.as_bytes())),
        None => String::new(),
    };
    let scanned = match subtype {
        Some("hash") => matching("HSCAN"),
        Some("set") => matching("SSCAN"),
        Some("zset") => matching("ZSCAN"),
        _ => String::new(),
    };
    if !scanned.is_empty() {
        return scanned;
    }
    match subtype {
        Some("string") => format!("GET {key}"),
        Some("hash") => format!("HGETALL {key}"),
        Some("list") => format!("LRANGE {key} 0 -1"),
        Some("set") => format!("SMEMBERS {key}"),
        Some("zset") => format!("ZRANGE {key} 0 -1 WITHSCORES"),
        Some("stream") => format!("XRANGE {key} - +"),
        // The root as it is; `$` would wrap it in an array of matches.
        Some("json") => format!("JSON.GET {key}"),
        _ => format!("TYPE {key}"),
    }
}

/// The columns a data view of each type shows; [`edit`] writes back
/// through them.
#[must_use]
pub fn columns(subtype: &str) -> &'static [&'static str] {
    match subtype {
        "hash" => &["field", "value"],
        "list" => &["#", "value"],
        "set" => &["member"],
        "zset" => &["member", "score"],
        "stream" => &["id", "fields"],
        _ => &["value"],
    }
}

/// Whether a data view of `subtype` can be changed cell by cell.
#[must_use]
pub fn editable(subtype: &str, column: &str) -> bool {
    match subtype {
        "string" | "json" | "set" => true,
        "hash" | "zset" => true,
        "list" => column == "value",
        _ => false,
    }
}

/// One changed cell of a data view: the row as shown, by [`columns`], and
/// the column's new text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellChange<'a> {
    pub row: Vec<Option<&'a str>>,
    pub column: usize,
    pub value: &'a str,
}

/// The commands that apply `changes` to `key` of type `subtype`, one per
/// line, for the app to show and run inside `MULTI` ... `EXEC`.
///
/// # Errors
///
/// A type or column that cannot be written back.
pub fn edit(subtype: &str, key: &str, changes: &[CellChange<'_>]) -> Result<String, String> {
    let key = argument(key);
    let mut lines = Vec::new();
    // Values are typed as they show, escapes included for binary ones.
    let arg = argument;
    for change in changes {
        let cell = |at: usize| change.row.get(at).copied().flatten().unwrap_or_default();
        let value = arg(change.value);
        match (subtype, change.column) {
            ("string", 0) => lines.push(format!("SET {key} {value} KEEPTTL")),
            ("json", 0) => lines.push(format!(
                "JSON.SET {key} $ {}",
                quote(change.value.as_bytes())
            )),
            ("hash", 1) => lines.push(format!("HSET {key} {} {value}", arg(cell(0)))),
            ("hash", 0) => {
                lines.push(format!("HDEL {key} {}", arg(cell(0))));
                lines.push(format!("HSET {key} {value} {}", arg(cell(1))));
            }
            ("list", 1) => lines.push(format!("LSET {key} {} {value}", cell(0))),
            ("set", 0) => {
                lines.push(format!("SREM {key} {}", arg(cell(0))));
                lines.push(format!("SADD {key} {value}"));
            }
            ("zset", 1) => {
                if change.value.trim().parse::<f64>().is_err() {
                    return Err(format!("{:?} is not a score", change.value));
                }
                lines.push(format!(
                    "ZADD {key} XX {} {}",
                    change.value.trim(),
                    arg(cell(0))
                ));
            }
            ("zset", 0) => {
                lines.push(format!("ZREM {key} {}", arg(cell(0))));
                lines.push(format!("ZADD {key} {} {value}", cell(1)));
            }
            _ => return Err(format!("A {subtype} cannot be changed here")),
        }
    }
    Ok(lines.join("\n"))
}

/// Commands for the tree's actions on a key.
pub mod actions {
    use super::argument;

    /// `seconds` to live, or none to keep it for ever.
    #[must_use]
    pub fn expire(key: &str, seconds: Option<u64>) -> String {
        let key = argument(key);
        match seconds {
            Some(seconds) => format!("EXPIRE {key} {seconds}"),
            None => format!("PERSIST {key}"),
        }
    }

    /// Refuses when `to` exists.
    #[must_use]
    pub fn rename(key: &str, to: &str) -> String {
        format!("RENAMENX {} {}", argument(key), argument(to))
    }

    #[must_use]
    pub fn delete(keys: &[&str]) -> String {
        let keys: Vec<String> = keys.iter().map(|key| argument(key)).collect();
        format!("UNLINK {}", keys.join(" "))
    }

    /// Switches the session to database `schema` (`db3` or `3`).
    #[must_use]
    pub fn select(schema: &str) -> Option<String> {
        super::database_index(schema).map(|n| format!("SELECT {n}"))
    }
}

/// The index of a database named `dbN` or `N`.
#[must_use]
pub fn database_index(name: &str) -> Option<u32> {
    let name = name.trim();
    name.strip_prefix("db").unwrap_or(name).parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_sorted_for_binary_search() {
        assert!(COMMANDS.windows(2).all(|pair| pair[0].name < pair[1].name));
        assert_eq!(command(&["get", "k"]).map(|c| c.name), Some("GET"));
        assert_eq!(
            command(&["config", "get"]).map(|c| c.access),
            Some(Access::Admin)
        );
        assert!(command(&["config"]).is_none());
    }

    #[test]
    fn words_split_as_redis_cli_does() {
        assert_eq!(
            argv(r#"SET "a b" 'c\'d' "\x41\n" plain"#).expect("argv"),
            vec![
                b"SET".to_vec(),
                b"a b".to_vec(),
                b"c'd".to_vec(),
                b"A\n".to_vec(),
                b"plain".to_vec()
            ]
        );
        assert!(argv(r#"SET "open"#).is_err());
        assert!(argv(r#"SET "a"b"#).is_err());
        for raw in [
            &b"plain"[..],
            b"with space",
            b"q\"uote",
            b"\xff\x00bin",
            b"#hash",
            b"",
        ] {
            let quoted = quote(raw);
            assert_eq!(
                argv(&quoted).expect("round trip"),
                vec![raw.to_vec()],
                "{quoted}"
            );
        }
        assert_eq!(display(b"\xffa"), r"\xffa");
    }

    #[test]
    fn one_command_per_line_and_blocks_together() {
        let script = "# note\nSET a 1\n\nMULTI\nINCR a\nEXEC\nGET a\n";
        let texts: Vec<&str> = statements(script).into_iter().map(|r| &script[r]).collect();
        assert_eq!(texts, ["SET a 1", "MULTI", "INCR a", "EXEC", "GET a"]);
        let at = |needle: &str| {
            let range =
                statement_at(script, script.find(needle).expect("in script")).expect("statement");
            &script[range]
        };
        assert_eq!(at("SET"), "SET a 1");
        assert_eq!(at("INCR"), "MULTI\nINCR a\nEXEC");
        assert_eq!(at("GET"), "GET a");
    }

    #[test]
    fn tokens_cover_the_source_and_know_commands() {
        let script = "hset k f \"v w\" 12\n// c\nnope x";
        let tokens = tokens(script);
        let joined: String = tokens.iter().map(|t| &script[t.range.clone()]).collect();
        assert_eq!(joined, script);
        let kind = |text: &str| {
            tokens
                .iter()
                .find(|t| &script[t.range.clone()] == text)
                .map(|t| t.kind)
        };
        assert_eq!(kind("hset"), Some(TokenKind::Command));
        assert_eq!(kind("\"v w\""), Some(TokenKind::String));
        assert_eq!(kind("12"), Some(TokenKind::Number));
        assert_eq!(kind("// c"), Some(TokenKind::Comment));
        assert_eq!(kind("nope"), Some(TokenKind::Unknown));
    }

    #[test]
    fn passwords_never_reach_the_history() {
        for (typed, kept) in [
            ("AUTH s3cret", "AUTH •••"),
            ("auth alice s3cret", "auth ••• •••"),
            ("HELLO 3 AUTH alice s3cret", "HELLO 3 AUTH ••• •••"),
            (
                "CONFIG SET requirepass s3cret",
                "CONFIG SET requirepass •••",
            ),
            (
                "ACL SETUSER bob on >s3cret ~* +@all",
                "ACL SETUSER bob on >••• ~* +@all",
            ),
        ] {
            assert!(sets_secret(typed), "{typed}");
            assert_eq!(redact_secrets(typed), kept);
        }
        assert!(!sets_secret("GET password"));
        assert_eq!(redact_secrets("GET a\nAUTH x\n"), "GET a\nAUTH •••\n");
    }

    #[test]
    fn dangerous_commands_are_named() {
        assert_eq!(dangers("GET a\nFLUSHALL").len(), 1);
        assert!(dangers("keys *")[0].starts_with("KEYS *"));
        assert!(dangers("SCAN 0 MATCH *").is_empty());
        assert!(plan("GET a\nHGETALL b").reads_only);
        assert!(!plan("GET a\nDEL b").reads_only);
    }

    #[test]
    fn data_views_read_and_write_by_type() {
        assert_eq!(browse(Some("hash"), "user:1", None), "HGETALL user:1");
        assert_eq!(
            browse(Some("hash"), "a b", Some("n*")),
            "HSCAN \"a b\" 0 MATCH n*"
        );
        assert_eq!(browse(Some("zset"), "z", None), "ZRANGE z 0 -1 WITHSCORES");
        let change =
            |row: Vec<Option<&'static str>>, column, value| CellChange { row, column, value };
        assert_eq!(
            edit(
                "hash",
                "h",
                &[change(vec![Some("f"), Some("old")], 1, "new v")]
            )
            .expect("edit"),
            "HSET h f \"new v\""
        );
        assert_eq!(
            edit("zset", "z", &[change(vec![Some("m"), Some("1")], 1, "2.5")]).expect("edit"),
            "ZADD z XX 2.5 m"
        );
        assert!(edit("zset", "z", &[change(vec![Some("m"), Some("1")], 1, "x")]).is_err());
        assert_eq!(
            edit(
                "list",
                "l",
                &[change(vec![Some("3"), Some("a")], 1, r"\xff")]
            )
            .expect("edit"),
            "LSET l 3 \"\\xff\""
        );
        assert!(edit("stream", "s", &[change(vec![Some("1-0"), None], 1, "x")]).is_err());
        assert_eq!(actions::select("db3").as_deref(), Some("SELECT 3"));
    }

    #[test]
    fn shown_text_goes_back_as_the_same_bytes() {
        for bytes in [
            &b"plain"[..],
            b"tab\\tliteral",
            b"C:\\x41",
            b"\xff\x00bin\"q\\",
            "ação".as_bytes(),
        ] {
            assert_eq!(raw(&display(bytes)), bytes, "{:?}", display(bytes));
            let arg = argument(&display(bytes));
            assert_eq!(argv(&arg).expect("argv"), vec![bytes.to_vec()], "{arg}");
        }
        assert_eq!(browse(Some("json"), "j", None), "JSON.GET j");
        assert_eq!(
            edit(
                "hash",
                "h",
                &[CellChange {
                    row: vec![Some("a\\tb"), Some("x")],
                    column: 1,
                    value: "y"
                }]
            )
            .expect("edit"),
            "HSET h \"a\\\\tb\" y"
        );
    }
}
