//! Catalog queries. Every value from the user travels as a bind parameter,
//! never spliced into the SQL.

pub(crate) const SERVER_INFO: &str = "SELECT current_setting('server_version'), \
     current_database()::text, current_user::text";

pub(crate) const DATABASES: &str = "SELECT datname::text FROM pg_catalog.pg_database \
     WHERE NOT datistemplate AND datallowconn ORDER BY datname";

/// User schemas first, then the system ones; never toast or temp schemas.
pub(crate) const SCHEMAS: &str = "SELECT nspname::text FROM pg_catalog.pg_namespace \
     WHERE nspname NOT LIKE 'pg\\_toast%' AND nspname NOT LIKE 'pg\\_temp\\_%' \
     ORDER BY nspname IN ('pg_catalog', 'information_schema'), nspname";

pub(crate) const RELATIONS: &str = "SELECT c.relname::text, c.relkind::text, \
     obj_description(c.oid, 'pg_class'), \
     CASE WHEN c.reltuples < 0 THEN NULL ELSE c.reltuples::bigint END \
     FROM pg_catalog.pg_class c \
     JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
     WHERE n.nspname = $1 AND c.relkind IN ('r', 'p', 'v', 'm', 'f', 'S') \
     ORDER BY c.relname";

pub(crate) const FUNCTIONS: &str = "SELECT p.proname || '(' || \
     pg_catalog.pg_get_function_identity_arguments(p.oid) || ')', \
     obj_description(p.oid, 'pg_proc') \
     FROM pg_catalog.pg_proc p \
     JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
     WHERE n.nspname = $1 AND p.prokind IN ('f', 'p') \
     ORDER BY 1";

pub(crate) const COLUMNS: &str = "SELECT a.attname::text, \
     pg_catalog.format_type(a.atttypid, a.atttypmod), \
     NOT a.attnotnull, \
     pg_catalog.pg_get_expr(d.adbin, d.adrelid), \
     EXISTS (SELECT 1 FROM pg_catalog.pg_index i \
             WHERE i.indrelid = a.attrelid AND i.indisprimary \
             AND a.attnum = ANY (i.indkey)), \
     col_description(a.attrelid, a.attnum) \
     FROM pg_catalog.pg_attribute a \
     JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
     JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
     LEFT JOIN pg_catalog.pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
     WHERE n.nspname = $1 AND c.relname = $2 AND a.attnum > 0 AND NOT a.attisdropped \
     ORDER BY a.attnum";
