//! RESP, the Redis protocol: commands out as arrays of bulk strings,
//! replies in, RESP2 or RESP3, read incrementally from whatever the socket
//! hands over.

use violet_sdk::DbError;

/// Longest bulk string kept; longer ones are read past and shown as their
/// size. A single Redis value may be 512 MB, the provider's whole memory.
pub const MAX_BULK: usize = 16 << 20;

/// Deepest nesting accepted, so a hostile server cannot exhaust the stack.
const MAX_DEPTH: usize = 32;

/// A server's answer.
#[derive(Clone, Debug, PartialEq)]
pub enum Reply {
    Status(String),
    Error(String),
    Integer(i64),
    /// `None` is RESP2's null bulk string.
    Bulk(Option<Vec<u8>>),
    /// A bulk string too long to keep: its size.
    Skipped(usize),
    /// `None` is RESP2's null array.
    Array(Option<Vec<Reply>>),
    Map(Vec<(Reply, Reply)>),
    Double(String),
    Boolean(bool),
    Null,
}

impl Reply {
    /// The text of a bulk or status reply.
    pub fn text(&self) -> Option<String> {
        match self {
            Self::Status(text) | Self::Double(text) => Some(text.clone()),
            Self::Bulk(Some(bytes)) => Some(String::from_utf8_lossy(bytes).into_owned()),
            Self::Integer(n) => Some(n.to_string()),
            _ => None,
        }
    }

    pub fn into_array(self) -> Option<Vec<Reply>> {
        match self {
            Self::Array(Some(items)) => Some(items),
            Self::Map(pairs) => Some(pairs.into_iter().flat_map(|(k, v)| [k, v]).collect()),
            _ => None,
        }
    }
}

/// Where replies are read from: a socket, or bytes in tests.
pub trait Source {
    /// Bytes not consumed yet, reading more when there are none; empty
    /// only at the end of the stream.
    fn fill(&mut self) -> Result<&[u8], DbError>;
    fn consume(&mut self, n: usize);
}

impl Source for violet_sdk::net::Connection {
    fn fill(&mut self) -> Result<&[u8], DbError> {
        violet_sdk::net::Connection::fill(self)
    }

    fn consume(&mut self, n: usize) {
        violet_sdk::net::Connection::consume(self, n);
    }
}

/// A command as RESP: an array of bulk strings.
pub fn encode<A: AsRef<[u8]>>(args: &[A], out: &mut Vec<u8>) {
    out.extend_from_slice(format!("*{}\r\n", args.len()).as_bytes());
    for arg in args {
        let arg = arg.as_ref();
        out.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
        out.extend_from_slice(arg);
        out.extend_from_slice(b"\r\n");
    }
}

fn closed() -> DbError {
    DbError::connection("The server closed the connection")
}

/// One line, without its `\r\n`.
fn line(source: &mut impl Source) -> Result<Vec<u8>, DbError> {
    let mut out = Vec::new();
    loop {
        let available = source.fill()?;
        if available.is_empty() {
            return Err(closed());
        }
        if let Some(at) = available.iter().position(|&b| b == b'\n') {
            out.extend_from_slice(&available[..at]);
            source.consume(at + 1);
            if out.last() == Some(&b'\r') {
                out.pop();
            }
            return Ok(out);
        }
        out.extend_from_slice(available);
        let n = available.len();
        source.consume(n);
        if out.len() > 64 * 1024 {
            return Err(DbError::protocol("The server sent an overlong line"));
        }
    }
}

/// `n` bytes, kept or not, then the `\r\n` after them.
fn bytes(source: &mut impl Source, n: usize, keep: bool) -> Result<Vec<u8>, DbError> {
    let mut out = Vec::with_capacity(if keep { n } else { 0 });
    let mut left = n + 2;
    while left > 0 {
        let available = source.fill()?;
        if available.is_empty() {
            return Err(closed());
        }
        let take = available.len().min(left);
        if keep {
            let data = take.min(left.saturating_sub(2));
            out.extend_from_slice(&available[..data]);
        }
        source.consume(take);
        left -= take;
    }
    Ok(out)
}

fn number(text: &[u8]) -> Result<i64, DbError> {
    std::str::from_utf8(text)
        .ok()
        .and_then(|text| text.parse().ok())
        .ok_or_else(|| DbError::protocol("The server sent a malformed length"))
}

/// Reads one reply. Of a top-level aggregate, only the first `keep`
/// elements are kept; the rest are read past, and `truncated` says so.
pub fn read(source: &mut impl Source, keep: usize) -> Result<(Reply, bool), DbError> {
    let mut truncated = false;
    let reply = loop {
        let reply = value(source, 0, keep, &mut truncated)?;
        // Out-of-band pushes and attributes come before or inside replies
        // the client asked for; they are not answers.
        if let Some(reply) = reply {
            break reply;
        }
    };
    Ok((reply, truncated))
}

/// One value; `None` for a push or attribute, which is read past.
fn value(
    source: &mut impl Source,
    depth: usize,
    keep: usize,
    truncated: &mut bool,
) -> Result<Option<Reply>, DbError> {
    if depth > MAX_DEPTH {
        return Err(DbError::protocol("The server's reply is nested too deeply"));
    }
    let head = line(source)?;
    let Some((&kind, rest)) = head.split_first() else {
        return Err(DbError::protocol("The server sent an empty line"));
    };
    let text = || String::from_utf8_lossy(rest).into_owned();
    let mut items = |count: usize, source: &mut _| -> Result<Vec<Reply>, DbError> {
        let mut items = Vec::new();
        let limit = if depth == 0 { keep } else { usize::MAX };
        let mut dropped = false;
        for _ in 0..count {
            let mut ignored = false;
            match value(source, depth + 1, keep, &mut ignored)? {
                Some(item) if items.len() < limit => items.push(item),
                Some(_) => dropped = true,
                None => {}
            }
        }
        if dropped {
            *truncated = true;
        }
        Ok(items)
    };
    Ok(Some(match kind {
        b'+' => Reply::Status(text()),
        b'-' => Reply::Error(text()),
        b':' => Reply::Integer(number(rest)?),
        b',' => Reply::Double(text()),
        b'(' => Reply::Double(text()),
        b'#' => Reply::Boolean(rest == b"t"),
        b'_' => Reply::Null,
        b'$' | b'=' | b'!' => {
            let n = number(rest)?;
            if n < 0 {
                return Ok(Some(Reply::Bulk(None)));
            }
            let n = usize::try_from(n).map_err(|_| DbError::protocol("Bad length"))?;
            if n > MAX_BULK {
                bytes(source, n, false)?;
                Reply::Skipped(n)
            } else {
                let mut data = bytes(source, n, true)?;
                if kind == b'=' && data.len() >= 4 {
                    // Verbatim strings start with their format: `txt:`.
                    data.drain(..4);
                }
                if kind == b'!' {
                    Reply::Error(String::from_utf8_lossy(&data).into_owned())
                } else {
                    Reply::Bulk(Some(data))
                }
            }
        }
        b'*' | b'~' | b'>' => {
            let n = number(rest)?;
            if n < 0 {
                return Ok(Some(Reply::Array(None)));
            }
            let list = items(usize::try_from(n).unwrap_or(0), source)?;
            if kind == b'>' {
                return Ok(None);
            }
            Reply::Array(Some(list))
        }
        b'%' | b'|' => {
            let n = usize::try_from(number(rest)?).unwrap_or(0);
            let flat = items(n * 2, source)?;
            if kind == b'|' {
                return Ok(None);
            }
            let mut pairs = Vec::with_capacity(flat.len() / 2);
            let mut flat = flat.into_iter();
            while let (Some(k), Some(v)) = (flat.next(), flat.next()) {
                pairs.push((k, v));
            }
            Reply::Map(pairs)
        }
        other => {
            return Err(DbError::protocol(format!(
                "The server sent an unknown reply type {:?}",
                other as char
            )));
        }
    }))
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Bytes handed over `step` at a time.
    pub struct Trickle {
        pub data: Vec<u8>,
        pub at: usize,
        pub step: usize,
    }

    impl Source for Trickle {
        fn fill(&mut self) -> Result<&[u8], DbError> {
            let end = (self.at + self.step).min(self.data.len());
            Ok(&self.data[self.at..end])
        }

        fn consume(&mut self, n: usize) {
            self.at += n;
        }
    }

    fn parse(data: &[u8], keep: usize) -> (Reply, bool) {
        let mut source = Trickle {
            data: data.to_vec(),
            at: 0,
            step: 1,
        };
        read(&mut source, keep).expect("reply")
    }

    #[test]
    fn replies_parse_one_byte_at_a_time() {
        assert_eq!(parse(b"+OK\r\n", 10).0, Reply::Status("OK".into()));
        assert_eq!(parse(b":-3\r\n", 10).0, Reply::Integer(-3));
        assert_eq!(parse(b"$3\r\na\r\n\r\n", 10).0, Reply::Bulk(Some(b"a\r\n".to_vec())));
        assert_eq!(parse(b"$-1\r\n", 10).0, Reply::Bulk(None));
        assert_eq!(
            parse(b"*2\r\n$1\r\na\r\n*1\r\n:1\r\n", 10).0,
            Reply::Array(Some(vec![
                Reply::Bulk(Some(b"a".to_vec())),
                Reply::Array(Some(vec![Reply::Integer(1)]))
            ]))
        );
        assert_eq!(
            parse(b"%1\r\n+k\r\n,1.5\r\n", 10).0,
            Reply::Map(vec![(Reply::Status("k".into()), Reply::Double("1.5".into()))])
        );
        // A push before the answer is not the answer.
        assert_eq!(parse(b">1\r\n+x\r\n:7\r\n", 10).0, Reply::Integer(7));
    }

    #[test]
    fn long_arrays_and_values_are_cut() {
        let (reply, truncated) = parse(b"*3\r\n:1\r\n:2\r\n:3\r\n", 2);
        assert!(truncated);
        assert_eq!(reply, Reply::Array(Some(vec![Reply::Integer(1), Reply::Integer(2)])));
        let big = MAX_BULK + 1;
        let mut data = format!("${big}\r\n").into_bytes();
        data.extend(std::iter::repeat_n(b'x', big));
        data.extend_from_slice(b"\r\n+next\r\n");
        let mut source = Trickle {
            data,
            at: 0,
            step: 1 << 20,
        };
        assert_eq!(read(&mut source, 1).expect("big").0, Reply::Skipped(big));
        assert_eq!(read(&mut source, 1).expect("next").0, Reply::Status("next".into()));
    }

    #[test]
    fn commands_encode_as_bulk_arrays() {
        let mut out = Vec::new();
        encode(&["SET", "k", ""], &mut out);
        assert_eq!(out, b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$0\r\n\r\n");
    }
}
