//! Built-in type ids, so common columns are named without asking the
//! server. Others are looked up in `pg_type` once per session.

/// The name of a built-in type, as `pg_type.typname` spells it.
pub(crate) fn name(oid: u32) -> Option<&'static str> {
    Some(match oid {
        16 => "bool",
        17 => "bytea",
        18 => "char",
        19 => "name",
        20 => "int8",
        21 => "int2",
        23 => "int4",
        24 => "regproc",
        25 => "text",
        26 => "oid",
        114 => "json",
        142 => "xml",
        650 => "cidr",
        700 => "float4",
        701 => "float8",
        790 => "money",
        829 => "macaddr",
        869 => "inet",
        1000 => "_bool",
        1005 => "_int2",
        1007 => "_int4",
        1009 => "_text",
        1016 => "_int8",
        1021 => "_float4",
        1022 => "_float8",
        1042 => "bpchar",
        1043 => "varchar",
        1082 => "date",
        1083 => "time",
        1114 => "timestamp",
        1184 => "timestamptz",
        1186 => "interval",
        1231 => "_numeric",
        1266 => "timetz",
        1560 => "bit",
        1562 => "varbit",
        1700 => "numeric",
        2205 => "regclass",
        2249 => "record",
        2950 => "uuid",
        3802 => "jsonb",
        _ => return None,
    })
}

/// Numbers are right aligned in the grid.
pub(crate) fn numeric(oid: u32) -> bool {
    matches!(oid, 20 | 21 | 23 | 26 | 700 | 701 | 790 | 1700)
}
