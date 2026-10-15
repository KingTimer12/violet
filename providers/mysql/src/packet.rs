//! Reading and writing the fields of MySQL packets: fixed-size little
//! endian integers, length-encoded integers and strings, and strings
//! ended by a NUL.

use violet_sdk::DbError;

/// Marks a `NULL` cell where a length-encoded string was expected.
pub(crate) const NULL: u8 = 0xFB;

/// Reads a packet's payload front to back.
pub(crate) struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

fn short() -> DbError {
    DbError::protocol("The server sent a packet shorter than announced")
}

impl<'a> Reader<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.at >= self.bytes.len()
    }

    pub(crate) fn position(&self) -> usize {
        self.at
    }

    pub(crate) fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    pub(crate) fn bytes(&mut self, n: usize) -> Result<&'a [u8], DbError> {
        let end = self.at.checked_add(n).ok_or_else(short)?;
        let bytes = self.bytes.get(self.at..end).ok_or_else(short)?;
        self.at = end;
        Ok(bytes)
    }

    pub(crate) fn skip(&mut self, n: usize) -> Result<(), DbError> {
        self.bytes(n).map(|_| ())
    }

    pub(crate) fn u8(&mut self) -> Result<u8, DbError> {
        Ok(self.bytes(1)?[0])
    }

    pub(crate) fn u16(&mut self) -> Result<u16, DbError> {
        let b = self.bytes(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub(crate) fn u32(&mut self) -> Result<u32, DbError> {
        let b = self.bytes(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// A length-encoded integer; `None` for the `NULL` marker.
    pub(crate) fn lenenc(&mut self) -> Result<Option<u64>, DbError> {
        let first = self.u8()?;
        let n = match first {
            NULL => return Ok(None),
            0xFC => 2,
            0xFD => 3,
            0xFE => 8,
            // 0xFF starts an error packet, never a number.
            0xFF => return Err(DbError::protocol("Malformed length in a packet")),
            small => return Ok(Some(u64::from(small))),
        };
        let mut value = [0u8; 8];
        value[..n].copy_from_slice(self.bytes(n)?);
        Ok(Some(u64::from_le_bytes(value)))
    }

    /// A length-encoded string; `None` for `NULL`.
    pub(crate) fn lenenc_bytes(&mut self) -> Result<Option<&'a [u8]>, DbError> {
        match self.lenenc()? {
            None => Ok(None),
            Some(n) => {
                let n = usize::try_from(n).map_err(|_| short())?;
                self.bytes(n).map(Some)
            }
        }
    }

    /// A length-encoded string, as text; `NULL` reads as empty.
    pub(crate) fn lenenc_text(&mut self) -> Result<String, DbError> {
        Ok(self
            .lenenc_bytes()?
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_default())
    }

    /// Up to the next NUL, or the end of the packet when there is none.
    pub(crate) fn nul_bytes(&mut self) -> &'a [u8] {
        let rest = &self.bytes[self.at.min(self.bytes.len())..];
        match rest.iter().position(|b| *b == 0) {
            Some(n) => {
                self.at += n + 1;
                &rest[..n]
            }
            None => {
                self.at = self.bytes.len();
                rest
            }
        }
    }

    pub(crate) fn nul_text(&mut self) -> String {
        String::from_utf8_lossy(self.nul_bytes()).into_owned()
    }

    /// Everything left.
    pub(crate) fn rest(&mut self) -> &'a [u8] {
        let rest = &self.bytes[self.at.min(self.bytes.len())..];
        self.at = self.bytes.len();
        rest
    }
}

/// Appends a length-encoded integer.
pub(crate) fn put_lenenc(out: &mut Vec<u8>, n: u64) {
    match n {
        0..=0xFA => out.push(n as u8),
        0xFB..=0xFFFF => {
            out.push(0xFC);
            out.extend_from_slice(&(n as u16).to_le_bytes());
        }
        0x1_0000..=0xFF_FFFF => {
            out.push(0xFD);
            out.extend_from_slice(&(n as u32).to_le_bytes()[..3]);
        }
        _ => {
            out.push(0xFE);
            out.extend_from_slice(&n.to_le_bytes());
        }
    }
}

/// Appends a length-encoded string.
pub(crate) fn put_lenenc_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put_lenenc(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lengths_round_trip() {
        for n in [
            0,
            250,
            251,
            0xFFFF,
            0x1_0000,
            0xFF_FFFF,
            0x100_0000,
            u64::MAX,
        ] {
            let mut out = Vec::new();
            put_lenenc(&mut out, n);
            assert_eq!(Reader::new(&out).lenenc().expect("length"), Some(n), "{n}");
        }
        assert_eq!(Reader::new(&[NULL]).lenenc().expect("null"), None);
        assert!(Reader::new(&[0xFC, 1]).lenenc().is_err());
    }

    #[test]
    fn strings_end_at_nul_or_the_packet() {
        let mut reader = Reader::new(b"8.4.11\0rest");
        assert_eq!(reader.nul_text(), "8.4.11");
        assert_eq!(reader.nul_text(), "rest");
        assert!(reader.is_empty());
        let mut out = Vec::new();
        put_lenenc_bytes(&mut out, b"abc");
        let mut reader = Reader::new(&out);
        assert_eq!(reader.lenenc_bytes().expect("text"), Some(&b"abc"[..]));
    }
}
