//! Replies as grids, shaped by the command that asked: pairs for
//! `HGETALL`, members and scores for `ZRANGE ... WITHSCORES`, ids and
//! fields for `XRANGE`...

use violet_lang::redis::display;
use violet_sdk::{ColumnMeta, DbError, Result, ResultSet, RowsBuilder};

use crate::resp::Reply;

/// How a command's reply reads as rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    /// One cell.
    Value,
    /// Field and value pairs.
    Pairs(&'static str, &'static str),
    /// Members and their scores.
    Scored,
    /// Items with their position, from `first`.
    Indexed(u64),
    /// Items alone, under this name.
    Items(&'static str),
    /// A stream's entries: id and fields.
    Stream,
    /// `[cursor, items]` of a `SCAN`, the items shaped as given.
    Scan(&'static Shape),
    /// `INFO`'s `name:value` lines.
    Info,
}

/// The shape of `args`' reply.
pub fn shape(args: &[String]) -> Shape {
    let upper: Vec<String> = args.iter().map(|a| a.to_ascii_uppercase()).collect();
    let has = |word: &str| upper.iter().skip(1).any(|w| w == word);
    let name = upper.first().map_or("", String::as_str);
    match name {
        "HGETALL" | "CONFIG" if name == "HGETALL" || upper.get(1).is_some_and(|w| w == "GET") => {
            Shape::Pairs("field", "value")
        }
        "HRANDFIELD" if has("WITHVALUES") => Shape::Pairs("field", "value"),
        "HKEYS" => Shape::Items("field"),
        "SMEMBERS" | "SINTER" | "SUNION" | "SDIFF" | "SRANDMEMBER" | "SPOP" => {
            Shape::Items("member")
        }
        "KEYS" => Shape::Items("key"),
        "ZRANGE" | "ZREVRANGE" | "ZRANGEBYSCORE" | "ZREVRANGEBYSCORE" | "ZRANDMEMBER"
        | "ZUNION" | "ZINTER" | "ZDIFF"
            if has("WITHSCORES") =>
        {
            Shape::Scored
        }
        "ZPOPMIN" | "ZPOPMAX" => Shape::Scored,
        "LRANGE" => Shape::Indexed(
            args.get(2)
                .and_then(|start| start.parse::<u64>().ok())
                .unwrap_or(0),
        ),
        "XRANGE" | "XREVRANGE" => Shape::Stream,
        "SCAN" => Shape::Scan(&Shape::Items("key")),
        "SSCAN" => Shape::Scan(&Shape::Items("member")),
        "HSCAN" if has("NOVALUES") => Shape::Scan(&Shape::Items("field")),
        "HSCAN" => Shape::Scan(&Shape::Pairs("field", "value")),
        "ZSCAN" => Shape::Scan(&Shape::Scored),
        "INFO" => Shape::Info,
        _ => Shape::Value,
    }
}

/// A reply as text: bulk strings as they are (escaped when not UTF-8),
/// aggregates as JSON.
pub fn cell(reply: &Reply) -> Option<String> {
    match reply {
        Reply::Bulk(None) | Reply::Array(None) | Reply::Null => None,
        Reply::Bulk(Some(bytes)) => Some(display(bytes)),
        Reply::Skipped(size) => Some(format!("… ({size} bytes, too long to show)")),
        Reply::Status(text) | Reply::Double(text) => Some(text.clone()),
        Reply::Error(text) => Some(format!("(error) {text}")),
        Reply::Integer(n) => Some(n.to_string()),
        Reply::Boolean(b) => Some(b.to_string()),
        Reply::Array(Some(_)) | Reply::Map(_) => {
            let mut out = String::new();
            json(reply, &mut out);
            Some(out)
        }
    }
}

fn json_string(text: &str, out: &mut String) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn json(reply: &Reply, out: &mut String) {
    match reply {
        Reply::Array(Some(items)) => {
            out.push('[');
            for (at, item) in items.iter().enumerate() {
                if at > 0 {
                    out.push_str(", ");
                }
                json(item, out);
            }
            out.push(']');
        }
        Reply::Map(pairs) => {
            out.push('{');
            for (at, (key, value)) in pairs.iter().enumerate() {
                if at > 0 {
                    out.push_str(", ");
                }
                json_string(&cell(key).unwrap_or_default(), out);
                out.push_str(": ");
                json(value, out);
            }
            out.push('}');
        }
        Reply::Integer(_) | Reply::Double(_) | Reply::Boolean(_) => {
            out.push_str(&cell(reply).unwrap_or_default());
        }
        Reply::Bulk(None) | Reply::Array(None) | Reply::Null => out.push_str("null"),
        other => json_string(&cell(other).unwrap_or_default(), out),
    }
}

/// A stream entry's fields, `[f, v, f, v]`, as a JSON object.
fn fields(reply: &Reply) -> Option<String> {
    let Reply::Array(Some(items)) = reply else {
        return cell(reply);
    };
    let mut out = String::from("{");
    for (at, pair) in items.chunks(2).enumerate() {
        if at > 0 {
            out.push_str(", ");
        }
        json_string(&cell(&pair[0]).unwrap_or_default(), &mut out);
        out.push_str(": ");
        json_string(&pair.get(1).and_then(cell).unwrap_or_default(), &mut out);
    }
    out.push('}');
    Some(out)
}

/// Pairs from a RESP3 map, an array of two-element arrays, or a flat
/// RESP2 array.
fn pairs(reply: Reply) -> Vec<(Reply, Reply)> {
    match reply {
        Reply::Map(pairs) => pairs,
        Reply::Array(Some(items)) => {
            if items
                .iter()
                .all(|item| matches!(item, Reply::Array(Some(pair)) if pair.len() == 2))
            {
                items
                    .into_iter()
                    .filter_map(|item| {
                        let mut pair = item.into_array()?.into_iter();
                        Some((pair.next()?, pair.next()?))
                    })
                    .collect()
            } else {
                let mut out = Vec::with_capacity(items.len() / 2);
                let mut items = items.into_iter();
                while let (Some(k), Some(v)) = (items.next(), items.next()) {
                    out.push((k, v));
                }
                out
            }
        }
        _ => Vec::new(),
    }
}

fn text_column(name: &str) -> ColumnMeta {
    ColumnMeta::new(name, None, false)
}

fn number_column(name: &str, type_name: &str) -> ColumnMeta {
    ColumnMeta::new(name, Some(type_name.into()), true)
}

/// The columns and rows of a reply shaped as `shape`, at most `max_rows`
/// of them.
pub fn rows(shape: Shape, reply: Reply, max_rows: usize) -> Result<(Vec<ColumnMeta>, RowsBuilder)> {
    Ok(match shape {
        Shape::Pairs(key, value) => {
            let mut rows = RowsBuilder::new(2, max_rows);
            for (k, v) in pairs(reply) {
                rows.push([cell(&k).as_deref(), cell(&v).as_deref()])?;
            }
            (vec![text_column(key), text_column(value)], rows)
        }
        Shape::Scored => {
            let mut rows = RowsBuilder::new(2, max_rows);
            for (member, score) in pairs(reply) {
                rows.push([cell(&member).as_deref(), cell(&score).as_deref()])?;
            }
            (
                vec![text_column("member"), number_column("score", "double")],
                rows,
            )
        }
        Shape::Indexed(first) => {
            let mut rows = RowsBuilder::new(2, max_rows);
            for (at, item) in reply.into_array().unwrap_or_default().iter().enumerate() {
                let index = (first + at as u64).to_string();
                rows.push([Some(index.as_str()), cell(item).as_deref()])?;
            }
            (
                vec![number_column("#", "index"), text_column("value")],
                rows,
            )
        }
        Shape::Items(name) => {
            let mut rows = RowsBuilder::new(1, max_rows);
            for item in reply.into_array().unwrap_or_default() {
                rows.push([cell(&item).as_deref()])?;
            }
            (vec![text_column(name)], rows)
        }
        Shape::Stream => {
            let mut rows = RowsBuilder::new(2, max_rows);
            for entry in reply.into_array().unwrap_or_default() {
                let mut parts = entry.into_array().unwrap_or_default().into_iter();
                let id = parts.next().as_ref().and_then(cell);
                let fields = parts.next().as_ref().and_then(fields);
                rows.push([id.as_deref(), fields.as_deref()])?;
            }
            (vec![text_column("id"), text_column("fields")], rows)
        }
        Shape::Scan(inner) => {
            let mut parts = reply.into_array().unwrap_or_default().into_iter();
            let _cursor = parts.next();
            let items = parts.next().unwrap_or(Reply::Array(Some(Vec::new())));
            return rows(*inner, items, max_rows);
        }
        Shape::Info => {
            let text = cell(&reply).unwrap_or_default();
            let mut rows = RowsBuilder::new(2, max_rows);
            for line in text.lines() {
                if let Some((name, value)) = line.split_once(':') {
                    rows.push([Some(name), Some(value)])?;
                }
            }
            (vec![text_column("field"), text_column("value")], rows)
        }
        Shape::Value => {
            let column = match &reply {
                Reply::Integer(_) => number_column("integer", "integer"),
                Reply::Double(_) => number_column("double", "double"),
                _ => text_column("value"),
            };
            let mut rows = RowsBuilder::new(1, max_rows);
            rows.push([cell(&reply).as_deref()])?;
            (vec![column], rows)
        }
    })
}

/// The grid for one command's reply. Errors become [`DbError`]s, with the
/// error's first word (`WRONGTYPE`, `ERR`, `NOAUTH`...) as their code.
pub fn result_set(
    args: &[String],
    reply: Reply,
    truncated: bool,
    max_rows: usize,
) -> Result<ResultSet> {
    if let Reply::Error(message) = &reply {
        return Err(error(message));
    }
    if let Reply::Status(status) = &reply
        && matches!(status.as_str(), "OK" | "QUEUED" | "PONG")
    {
        return Ok(ResultSet::command(status.clone(), None));
    }
    let shape = shape(args);
    let (columns, rows) = rows(shape, reply, max_rows)?;
    let mut set = ResultSet::query(columns, rows);
    set.truncated |= truncated;
    Ok(set)
}

pub fn error(message: &str) -> DbError {
    let code = message.split_whitespace().next().unwrap_or("ERR");
    let mut error = DbError::query(message.to_owned());
    if code.chars().all(|c| c.is_ascii_uppercase()) {
        error = error.with_code(code);
    }
    error
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bulk(text: &str) -> Reply {
        Reply::Bulk(Some(text.as_bytes().to_vec()))
    }

    fn args(line: &str) -> Vec<String> {
        line.split(' ').map(str::to_owned).collect()
    }

    fn cells(set: &ResultSet) -> Vec<String> {
        let mut out = Vec::new();
        let mut start = 0;
        for &end in &set.rows.ends {
            let at = (end & !(1 << 31)) as usize;
            out.push(if end & (1 << 31) != 0 {
                "NULL".into()
            } else {
                set.rows.text[start..at].to_owned()
            });
            start = at;
        }
        out
    }

    #[test]
    fn replies_take_the_shape_of_their_command() {
        let flat = Reply::Array(Some(vec![bulk("f"), bulk("v")]));
        let set = result_set(&args("HGETALL h"), flat.clone(), false, 10).expect("set");
        assert_eq!(set.columns[0].name, "field");
        assert_eq!(cells(&set), ["f", "v"]);

        let set = result_set(&args("ZRANGE z 0 -1 WITHSCORES"), flat, false, 10).expect("set");
        assert_eq!(set.columns[1].name, "score");

        let list = Reply::Array(Some(vec![bulk("a"), bulk("b")]));
        let set = result_set(&args("LRANGE l 5 6"), list, false, 10).expect("set");
        assert_eq!(cells(&set), ["5", "a", "6", "b"]);

        let entry = Reply::Array(Some(vec![Reply::Array(Some(vec![
            bulk("1-0"),
            Reply::Array(Some(vec![bulk("k"), bulk("v")])),
        ]))]));
        let set = result_set(&args("XRANGE s - +"), entry, false, 10).expect("set");
        assert_eq!(cells(&set), ["1-0", r#"{"k": "v"}"#]);

        let scan = Reply::Array(Some(vec![
            bulk("0"),
            Reply::Array(Some(vec![bulk("m"), bulk("2")])),
        ]));
        let set = result_set(&args("ZSCAN z 0"), scan, false, 10).expect("set");
        assert_eq!(cells(&set), ["m", "2"]);

        let set = result_set(&args("SET a b"), Reply::Status("OK".into()), false, 10).expect("ok");
        assert_eq!(set.command.as_deref(), Some("OK"));
        let set = result_set(&args("GET a"), Reply::Bulk(None), false, 10).expect("null");
        assert_eq!(cells(&set), ["NULL"]);
        let set = result_set(&args("INCR a"), Reply::Integer(3), false, 10).expect("int");
        assert!(set.columns[0].numeric);
    }

    #[test]
    fn errors_carry_their_code() {
        let error = result_set(
            &args("GET h"),
            Reply::Error("WRONGTYPE Operation".into()),
            false,
            1,
        )
        .expect_err("error");
        assert_eq!(error.code.as_deref(), Some("WRONGTYPE"));
    }
}
