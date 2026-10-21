//! Geometry values, read from the well-known binary the servers send into
//! the well-known text people read.
//!
//! PostGIS sends `geometry` and `geography` as hex EWKB: WKB whose type
//! word may carry flags for Z, M and an SRID. MySQL sends its geometry as
//! `0x`, then a little-endian SRID and the WKB.

/// Nesting deeper than this is refused rather than followed.
const DEPTH: u8 = 32;

/// The text form of a geometry value, `SRID=4326;POINT(1 2)` when it has an
/// SRID; `None` when `value` is not one.
pub fn wkt(value: &str) -> Option<String> {
    let (mysql, digits) = match value.strip_prefix("0x") {
        Some(digits) => (true, digits),
        None => (false, value),
    };
    let bytes = unhex(digits)?;
    let (mut srid, wkb) = if mysql {
        let (srid, wkb) = bytes.split_at_checked(4)?;
        let srid = u32::from_le_bytes(srid.try_into().ok()?);
        ((srid != 0).then_some(srid), wkb)
    } else {
        (None, &bytes[..])
    };
    let mut reader = Reader {
        bytes: wkb,
        at: 0,
        little: true,
    };
    let mut text = String::new();
    reader.geometry(&mut text, 0, false, &mut srid)?;
    if reader.at != wkb.len() {
        return None;
    }
    Some(match srid {
        Some(srid) => format!("SRID={srid};{text}"),
        None => text,
    })
}

fn unhex(digits: &str) -> Option<Vec<u8>> {
    let digits = digits.as_bytes();
    if digits.is_empty() || !digits.len().is_multiple_of(2) {
        return None;
    }
    let nibble = |digit: u8| char::from(digit).to_digit(16).map(|n| n as u8);
    digits
        .as_chunks::<2>()
        .0
        .iter()
        .map(|[high, low]| Some(nibble(*high)? << 4 | nibble(*low)?))
        .collect()
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
    /// The byte order of the geometry being read.
    little: bool,
}

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let bytes = self.bytes.get(self.at..self.at.checked_add(N)?)?;
        self.at += N;
        bytes.try_into().ok()
    }

    fn u32(&mut self) -> Option<u32> {
        let bytes = self.take()?;
        Some(if self.little {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        })
    }

    fn f64(&mut self) -> Option<f64> {
        let bytes = self.take()?;
        Some(if self.little {
            f64::from_le_bytes(bytes)
        } else {
            f64::from_be_bytes(bytes)
        })
    }

    /// One geometry into `text`. In a multi-geometry the members go
    /// `bare`: without their type's name, `MULTIPOINT((1 2),(3 4))`. The
    /// outermost geometry's EWKB SRID goes to `srid`.
    fn geometry(
        &mut self,
        text: &mut String,
        depth: u8,
        bare: bool,
        srid: &mut Option<u32>,
    ) -> Option<()> {
        if depth > DEPTH {
            return None;
        }
        self.little = match self.take::<1>()?[0] {
            0 => false,
            1 => true,
            _ => return None,
        };
        let word = self.u32()?;
        let (mut z, mut m) = (word & 0x8000_0000 != 0, word & 0x4000_0000 != 0);
        if word & 0x2000_0000 != 0 {
            let embedded = self.u32()?;
            if depth == 0 {
                *srid = Some(embedded);
            }
        }
        // ISO WKB counts Z and M in thousands: 1001 is POINT Z.
        let code = word & 0x0FFF_FFFF;
        match code / 1000 {
            0 => {}
            1 => z = true,
            2 => m = true,
            3 => (z, m) = (true, true),
            _ => return None,
        }
        let kind = code % 1000;
        let name = match kind {
            1 => "POINT",
            2 => "LINESTRING",
            3 => "POLYGON",
            4 => "MULTIPOINT",
            5 => "MULTILINESTRING",
            6 => "MULTIPOLYGON",
            7 => "GEOMETRYCOLLECTION",
            _ => return None,
        };
        let dimensions = match (z, m) {
            (true, true) => " ZM",
            (true, false) => " Z",
            (false, true) => " M",
            (false, false) => "",
        };
        // A collection's members keep their names.
        if !bare {
            text.push_str(name);
            text.push_str(dimensions);
        }
        let ordinates = 2 + usize::from(z) + usize::from(m);
        let mut body = String::new();
        let empty = match kind {
            1 => {
                let start = self.at;
                let point = (0..ordinates)
                    .map(|_| self.f64())
                    .collect::<Option<Vec<f64>>>()?;
                // An empty point is all NaN.
                if point.iter().all(|ordinate| ordinate.is_nan()) {
                    true
                } else {
                    self.at = start;
                    body.push('(');
                    self.point(&mut body, ordinates)?;
                    body.push(')');
                    false
                }
            }
            2 => self.points(&mut body, ordinates)?,
            3 => self.rings(&mut body, ordinates)?,
            _ => {
                let count = self.u32()?;
                body.push('(');
                for index in 0..count {
                    if index > 0 {
                        body.push(',');
                    }
                    self.geometry(&mut body, depth + 1, kind != 7, srid)?;
                }
                body.push(')');
                count == 0
            }
        };
        if empty {
            text.push_str(if bare { "EMPTY" } else { " EMPTY" });
        } else {
            if !bare && !dimensions.is_empty() {
                text.push(' ');
            }
            text.push_str(&body);
        }
        Some(())
    }

    fn point(&mut self, text: &mut String, ordinates: usize) -> Option<()> {
        for index in 0..ordinates {
            if index > 0 {
                text.push(' ');
            }
            text.push_str(&self.f64()?.to_string());
        }
        Some(())
    }

    /// A counted list of points, `(1 2,3 4)`; true when there are none.
    fn points(&mut self, text: &mut String, ordinates: usize) -> Option<bool> {
        let count = self.u32()?;
        text.push('(');
        for index in 0..count {
            if index > 0 {
                text.push(',');
            }
            self.point(text, ordinates)?;
        }
        text.push(')');
        Some(count == 0)
    }

    /// A counted list of rings, `((0 0,1 0,0 0))`; true when there are none.
    fn rings(&mut self, text: &mut String, ordinates: usize) -> Option<bool> {
        let count = self.u32()?;
        text.push('(');
        for index in 0..count {
            if index > 0 {
                text.push(',');
            }
            self.points(text, ordinates)?;
        }
        text.push(')');
        Some(count == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn postgis_ewkb_reads_as_text() {
        // SELECT 'SRID=4326;POINT(1 2)'::geometry
        assert_eq!(
            wkt("0101000020E6100000000000000000F03F0000000000000040").as_deref(),
            Some("SRID=4326;POINT(1 2)")
        );
        // SELECT 'LINESTRING(0 0,1.5 -2)'::geometry, big-endian
        assert_eq!(
            wkt(&packed(
                "00 00000002 00000002
                 0000000000000000 0000000000000000
                 3FF8000000000000 C000000000000000"
            ))
            .as_deref(),
            Some("LINESTRING(0 0,1.5 -2)")
        );
        // SELECT 'POINT Z (1 2 3)'::geometry
        assert_eq!(
            wkt("0101000080000000000000F03F00000000000000400000000000000840").as_deref(),
            Some("POINT Z (1 2 3)")
        );
        // SELECT 'POINT EMPTY'::geometry
        assert_eq!(
            wkt("0101000000000000000000F87F000000000000F87F").as_deref(),
            Some("POINT EMPTY")
        );
    }

    /// `hex` without the spaces that group its bytes.
    fn packed(hex: &str) -> String {
        hex.split_whitespace().collect()
    }

    #[test]
    fn mysql_geometry_reads_as_text() {
        // SELECT ST_GeomFromText('MULTIPOINT((1 2),(3 4))')
        let multipoint = packed(
            "0x 00000000 01 04000000 02000000
             01 01000000 000000000000F03F 0000000000000040
             01 01000000 0000000000000840 0000000000001040",
        );
        assert_eq!(wkt(&multipoint).as_deref(), Some("MULTIPOINT((1 2),(3 4))"));
        // SELECT ST_GeomFromText('POLYGON((0 0,1 0,0 1,0 0))', 4326)
        let polygon = packed(
            "0x E6100000 01 03000000 01000000 04000000
             0000000000000000 0000000000000000
             000000000000F03F 0000000000000000
             0000000000000000 000000000000F03F
             0000000000000000 0000000000000000",
        );
        assert_eq!(
            wkt(&polygon).as_deref(),
            Some("SRID=4326;POLYGON((0 0,1 0,0 1,0 0))")
        );
        // A collection keeps its members' names.
        let collection = packed(
            "0x 00000000 01 07000000 02000000
             01 01000000 000000000000F03F 0000000000000040
             01 02000000 00000000",
        );
        assert_eq!(
            wkt(&collection).as_deref(),
            Some("GEOMETRYCOLLECTION(POINT(1 2),LINESTRING EMPTY)")
        );
    }

    #[test]
    fn other_values_are_not_geometry() {
        assert_eq!(wkt(""), None);
        assert_eq!(wkt("hello"), None);
        assert_eq!(wkt("0xCAFE"), None);
        // A point cut short.
        assert_eq!(wkt("0101000000000000000000F03F"), None);
    }
}
