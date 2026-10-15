//! Column types as the protocol numbers them.

/// The `binary` character set: bytes, not text.
pub(crate) const BINARY: u16 = 63;

const UNSIGNED: u16 = 1 << 5;
const ENUM: u16 = 1 << 8;
const SET: u16 = 1 << 11;

const JSON: u8 = 245;

/// A column's description, as much of it as Violet uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Column {
    pub name: String,
    pub kind: u8,
    pub charset: u16,
    pub flags: u16,
}

impl Column {
    /// The type's name, as `SHOW COLUMNS` spells it (without lengths).
    pub(crate) fn type_name(&self) -> Option<String> {
        let binary = self.charset == BINARY;
        let name = match self.kind {
            0 | 246 => "decimal",
            1 => "tinyint",
            2 => "smallint",
            3 => "int",
            4 => "float",
            5 => "double",
            6 => "null",
            7 | 17 => "timestamp",
            8 => "bigint",
            9 => "mediumint",
            10 | 14 => "date",
            11 | 19 => "time",
            12 | 18 => "datetime",
            13 => "year",
            15 | 253 if self.flags & ENUM != 0 => "enum",
            15 | 253 if self.flags & SET != 0 => "set",
            15 | 253 if binary => "varbinary",
            15 | 253 => "varchar",
            16 => "bit",
            243 => "vector",
            JSON => "json",
            247 => "enum",
            248 => "set",
            249 if binary => "tinyblob",
            249 => "tinytext",
            250 if binary => "mediumblob",
            250 => "mediumtext",
            251 if binary => "longblob",
            251 => "longtext",
            252 if binary => "blob",
            252 => "text",
            254 if self.flags & ENUM != 0 => "enum",
            254 if self.flags & SET != 0 => "set",
            254 if binary => "binary",
            254 => "char",
            255 => "geometry",
            _ => return None,
        };
        let unsigned = self.flags & UNSIGNED != 0 && self.numeric() && self.kind != 13;
        Some(if unsigned {
            format!("{name} unsigned")
        } else {
            name.to_owned()
        })
    }

    /// Numbers are right aligned in the grid.
    pub(crate) fn numeric(&self) -> bool {
        matches!(self.kind, 0 | 1 | 2 | 3 | 4 | 5 | 8 | 9 | 13 | 246)
    }

    /// Bytes shown as hex: binary strings, blobs, bits and geometry. JSON
    /// also reports the binary character set, but is text.
    pub(crate) fn shows_hex(&self) -> bool {
        match self.kind {
            16 | 255 => true,
            15 | 249..=254 => self.charset == BINARY,
            _ => false,
        }
    }
}

/// `0x` and the bytes in hex, as MySQL reads them back.
pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    let mut text = String::with_capacity(2 + bytes.len() * 2);
    text.push_str("0x");
    for byte in bytes {
        text.push(char::from(DIGITS[usize::from(byte >> 4)]));
        text.push(char::from(DIGITS[usize::from(byte & 0xF)]));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(kind: u8, charset: u16, flags: u16) -> Column {
        Column {
            name: "c".into(),
            kind,
            charset,
            flags,
        }
    }

    #[test]
    fn names_follow_charset_and_flags() {
        assert_eq!(column(253, 255, 0).type_name().as_deref(), Some("varchar"));
        assert_eq!(
            column(253, BINARY, 0).type_name().as_deref(),
            Some("varbinary")
        );
        assert_eq!(column(254, 255, ENUM).type_name().as_deref(), Some("enum"));
        assert_eq!(column(252, BINARY, 0).type_name().as_deref(), Some("blob"));
        assert_eq!(
            column(8, BINARY, UNSIGNED).type_name().as_deref(),
            Some("bigint unsigned")
        );
        assert!(column(246, BINARY, 0).numeric());
        assert!(column(254, BINARY, 0).shows_hex());
        assert!(!column(JSON, BINARY, 0).shows_hex());
        assert!(!column(253, 255, 0).shows_hex());
        assert_eq!(hex(&[0x00, 0xAB, 0x7F]), "0x00AB7F");
    }
}
