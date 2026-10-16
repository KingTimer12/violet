//! A result set written out as a file: CSV, TSV, Excel, JSON, SQL
//! `INSERT`s, Markdown, HTML or XML.
//!
//! Values arrive as the server's text. Columns the driver marks numeric are
//! written as numbers where the format has them, booleans as booleans, and
//! `NULL` as each format spells it.

use std::borrow::Cow;
use std::fmt::Write as _;
use std::io::Write as _;

use violet_core::{ColumnMeta, ResultSet};

/// The formats a result can be exported to, in the order the menu lists
/// them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Csv,
    Tsv,
    Xlsx,
    Json,
    Sql,
    Markdown,
    Html,
    Xml,
}

impl Format {
    pub const ALL: [Self; 8] = [
        Self::Csv,
        Self::Tsv,
        Self::Xlsx,
        Self::Json,
        Self::Sql,
        Self::Markdown,
        Self::Html,
        Self::Xml,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Csv => "CSV",
            Self::Tsv => "TSV",
            Self::Xlsx => "Excel (XLSX)",
            Self::Json => "JSON",
            Self::Sql => "SQL INSERT statements",
            Self::Markdown => "Markdown table",
            Self::Html => "HTML table",
            Self::Xml => "XML",
        }
    }

    pub const fn extension(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Tsv => "tsv",
            Self::Xlsx => "xlsx",
            Self::Json => "json",
            Self::Sql => "sql",
            Self::Markdown => "md",
            Self::Html => "html",
            Self::Xml => "xml",
        }
    }
}

/// What an export needs besides the rows.
pub struct Options<'a> {
    /// The table `INSERT`s go into, already quoted.
    pub table: &'a str,
    /// Quotes a column name for SQL.
    pub quote: &'a dyn Fn(&str) -> String,
    /// A title for the formats that have one (the sheet, the HTML page).
    pub title: &'a str,
}

/// The file's bytes.
pub fn render(set: &ResultSet, format: Format, options: &Options<'_>) -> anyhow::Result<Vec<u8>> {
    Ok(match format {
        Format::Csv => to_delimited(set, ',').into_bytes(),
        Format::Tsv => to_delimited(set, '\t').into_bytes(),
        Format::Xlsx => xlsx(set, options.title)?,
        Format::Json => json(set).into_bytes(),
        Format::Sql => sql(set, options).into_bytes(),
        Format::Markdown => markdown(set).into_bytes(),
        Format::Html => html(set, options.title).into_bytes(),
        Format::Xml => xml(set).into_bytes(),
    })
}

/// How a value is written where formats tell types apart.
enum Value<'a> {
    Null,
    Number(&'a str),
    Bool(bool),
    Text(&'a str),
}

fn is_bool(column: &ColumnMeta) -> bool {
    column.type_name.as_deref().is_some_and(|name| {
        name.eq_ignore_ascii_case("bool") || name.eq_ignore_ascii_case("boolean")
    })
}

/// A number JSON and SQL accept as written: no `NaN`, `Infinity`, hex or
/// leading `+`.
fn plain_number(text: &str) -> bool {
    serde_json::from_str::<serde_json::Number>(text).is_ok()
}

fn value<'a>(column: &ColumnMeta, text: Option<&'a str>) -> Value<'a> {
    let Some(text) = text else {
        return Value::Null;
    };
    if column.numeric && plain_number(text) {
        return Value::Number(text);
    }
    if is_bool(column) {
        match text.to_ascii_lowercase().as_str() {
            "t" | "true" => return Value::Bool(true),
            "f" | "false" => return Value::Bool(false),
            _ => {}
        }
    }
    Value::Text(text)
}

/// Appends one delimited line of `cells` to `out`; `NULL` is empty.
pub fn write_row<'a>(
    out: &mut String,
    cells: impl Iterator<Item = Option<&'a str>>,
    separator: char,
) {
    for (index, value) in cells.enumerate() {
        if index > 0 {
            out.push(separator);
        }
        write_field(out, value.unwrap_or_default(), separator);
    }
}

/// Appends a field, quoted when it holds the separator, a quote or a line
/// break.
fn write_field(out: &mut String, value: &str, separator: char) {
    let quote = value
        .chars()
        .any(|c| c == separator || matches!(c, '"' | '\n' | '\r'));
    if quote {
        out.push('"');
        for (index, part) in value.split('"').enumerate() {
            if index > 0 {
                out.push_str("\"\"");
            }
            out.push_str(part);
        }
        out.push('"');
    } else {
        out.push_str(value);
    }
}

/// The set as CSV (or TSV), header first, built in one buffer sized up
/// front.
#[hotpath::measure]
pub fn to_delimited(set: &ResultSet, separator: char) -> String {
    let cells = set.rows.len() * set.columns.len();
    let mut out = String::with_capacity(set.rows.text_len() + cells + 64);
    write_row(
        &mut out,
        set.columns.iter().map(|c| Some(c.name.as_str())),
        separator,
    );
    for row in set.rows.iter() {
        out.push('\n');
        write_row(&mut out, row.iter(), separator);
    }
    out
}

/// An array of objects, one per row and one per line.
fn json(set: &ResultSet) -> String {
    let keys: Vec<String> = set
        .columns
        .iter()
        .map(|c| serde_json::to_string(&c.name).unwrap_or_default())
        .collect();
    let mut out = String::with_capacity(set.rows.text_len() * 2 + 16);
    out.push('[');
    for (index, row) in set.rows.iter().enumerate() {
        out.push_str(if index == 0 { "\n  {" } else { ",\n  {" });
        for (column, text) in row.iter().enumerate() {
            if column > 0 {
                out.push_str(", ");
            }
            out.push_str(&keys[column]);
            out.push_str(": ");
            match value(&set.columns[column], text) {
                Value::Null => out.push_str("null"),
                Value::Number(number) => out.push_str(number),
                Value::Bool(flag) => out.push_str(if flag { "true" } else { "false" }),
                Value::Text(text) => {
                    out.push_str(&serde_json::to_string(text).unwrap_or_default());
                }
            }
        }
        out.push('}');
    }
    out.push_str(if set.rows.is_empty() { "]\n" } else { "\n]\n" });
    out
}

/// One `INSERT` per row.
fn sql(set: &ResultSet, options: &Options<'_>) -> String {
    let columns: Vec<String> = set
        .columns
        .iter()
        .map(|c| (options.quote)(&c.name))
        .collect();
    let head = format!(
        "INSERT INTO {} ({}) VALUES (",
        options.table,
        columns.join(", ")
    );
    let mut out = String::with_capacity(set.rows.text_len() + set.rows.len() * (head.len() + 8));
    for row in set.rows.iter() {
        out.push_str(&head);
        for (column, text) in row.iter().enumerate() {
            if column > 0 {
                out.push_str(", ");
            }
            match value(&set.columns[column], text) {
                Value::Null => out.push_str("NULL"),
                Value::Number(number) => out.push_str(number),
                Value::Bool(flag) => out.push_str(if flag { "TRUE" } else { "FALSE" }),
                Value::Text(text) => {
                    out.push('\'');
                    out.push_str(&text.replace('\'', "''"));
                    out.push('\'');
                }
            }
        }
        out.push_str(");\n");
    }
    out
}

/// A GitHub-flavoured Markdown table; `NULL` is empty.
fn markdown(set: &ResultSet) -> String {
    fn cell(out: &mut String, text: &str) {
        let text = text
            .replace('\\', "\\\\")
            .replace('|', "\\|")
            .replace("\r\n", "<br>")
            .replace(['\n', '\r'], "<br>");
        out.push(' ');
        out.push_str(&text);
        out.push_str(" |");
    }
    let mut out = String::with_capacity(set.rows.text_len() * 2 + 64);
    out.push('|');
    for column in &set.columns {
        cell(&mut out, &column.name);
    }
    out.push_str("\n|");
    for column in &set.columns {
        out.push_str(if column.numeric { " ---: |" } else { " --- |" });
    }
    for row in set.rows.iter() {
        out.push_str("\n|");
        for text in row.iter() {
            cell(&mut out, text.unwrap_or_default());
        }
    }
    out.push('\n');
    out
}

/// `text` with the characters markup gives meaning to escaped, and the
/// ones XML 1.0 forbids dropped.
fn escape_markup(text: &str) -> Cow<'_, str> {
    let plain = text
        .chars()
        .all(|c| !matches!(c, '<' | '>' | '&' | '"' | '\'') && allowed_in_xml(c));
    if plain {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 16);
    for c in text.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c if allowed_in_xml(c) => out.push(c),
            _ => {}
        }
    }
    Cow::Owned(out)
}

fn allowed_in_xml(c: char) -> bool {
    !c.is_control() || matches!(c, '\t' | '\n' | '\r')
}

/// A standalone page with the table.
fn html(set: &ResultSet, title: &str) -> String {
    let mut out = String::with_capacity(set.rows.text_len() * 2 + 1024);
    let _ = write!(
        out,
        "<!DOCTYPE html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n<title>{}</title>\n\
         <style>\n\
         body {{ font-family: system-ui, sans-serif; margin: 24px; }}\n\
         table {{ border-collapse: collapse; font-size: 13px; }}\n\
         th, td {{ border: 1px solid #ccc; padding: 4px 8px; text-align: left; vertical-align: top; white-space: pre-wrap; }}\n\
         th {{ background: #f3f3f3; position: sticky; top: 0; }}\n\
         td.number {{ text-align: right; font-variant-numeric: tabular-nums; }}\n\
         td.null {{ color: #999; font-style: italic; }}\n\
         </style>\n</head>\n<body>\n<table>\n<thead>\n<tr>",
        escape_markup(title)
    );
    for column in &set.columns {
        let _ = write!(out, "<th>{}</th>", escape_markup(&column.name));
    }
    out.push_str("</tr>\n</thead>\n<tbody>\n");
    for row in set.rows.iter() {
        out.push_str("<tr>");
        for (column, text) in row.iter().enumerate() {
            match text {
                None => out.push_str("<td class=\"null\">NULL</td>"),
                Some(text) if set.columns[column].numeric => {
                    let _ = write!(out, "<td class=\"number\">{}</td>", escape_markup(text));
                }
                Some(text) => {
                    let _ = write!(out, "<td>{}</td>", escape_markup(text));
                }
            }
        }
        out.push_str("</tr>\n");
    }
    out.push_str("</tbody>\n</table>\n</body>\n</html>\n");
    out
}

/// `<rows><row><field name="…">…</field></row></rows>`; `NULL` fields carry
/// `null="true"`.
fn xml(set: &ResultSet) -> String {
    let names: Vec<Cow<'_, str>> = set.columns.iter().map(|c| escape_markup(&c.name)).collect();
    let mut out = String::with_capacity(set.rows.text_len() * 2 + 64);
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<rows>\n");
    for row in set.rows.iter() {
        out.push_str("  <row>\n");
        for (column, text) in row.iter().enumerate() {
            let name = &names[column];
            match text {
                None => {
                    let _ = writeln!(out, "    <field name=\"{name}\" null=\"true\"/>");
                }
                Some(text) => {
                    let _ = writeln!(
                        out,
                        "    <field name=\"{name}\">{}</field>",
                        escape_markup(text)
                    );
                }
            }
        }
        out.push_str("  </row>\n");
    }
    out.push_str("</rows>\n");
    out
}

/// Rows a sheet holds, header included.
const XLSX_MAX_ROWS: usize = 1_048_576;
/// Characters a cell holds.
const XLSX_MAX_TEXT: usize = 32_767;
/// Digits Excel keeps of a number; longer ones are written as text so
/// keys and big integers are not rounded.
const XLSX_DIGITS: usize = 15;

/// `A`, `B`, … `Z`, `AA`, … for a zero-based column.
fn column_letters(mut index: usize) -> String {
    let mut letters = Vec::new();
    loop {
        letters.push(b'A' + (index % 26) as u8);
        if index < 26 {
            break;
        }
        index = index / 26 - 1;
    }
    letters.reverse();
    String::from_utf8(letters).unwrap_or_default()
}

/// A workbook with one sheet: a bold, frozen header with filters, numbers
/// as numbers and booleans as booleans.
fn xlsx(set: &ResultSet, title: &str) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        set.rows.len() < XLSX_MAX_ROWS,
        "Excel sheets hold at most {} rows",
        XLSX_MAX_ROWS - 1
    );
    let letters: Vec<String> = (0..set.columns.len()).map(column_letters).collect();
    let last = letters.last().cloned().unwrap_or_else(|| "A".to_owned());
    let mut sheet = String::with_capacity(set.rows.text_len() * 2 + 4096);
    sheet.push_str(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
         <worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\">\
         <sheetViews><sheetView workbookViewId=\"0\">\
         <pane ySplit=\"1\" topLeftCell=\"A2\" activePane=\"bottomLeft\" state=\"frozen\"/>\
         </sheetView></sheetViews>",
    );
    // Widths from the header and the first rows, as the grid sizes them.
    if !set.columns.is_empty() {
        sheet.push_str("<cols>");
        for (index, column) in set.columns.iter().enumerate() {
            let longest = set
                .rows
                .iter()
                .take(200)
                .filter_map(|row| row.get(index).flatten())
                .map(|text| text.lines().next().unwrap_or_default().chars().count())
                .max()
                .unwrap_or(0)
                .max(column.name.chars().count() + 2);
            let width = (longest as f32 + 2.0).clamp(8.0, 60.0);
            let _ = write!(
                sheet,
                "<col min=\"{n}\" max=\"{n}\" width=\"{width:.1}\" customWidth=\"1\"/>",
                n = index + 1
            );
        }
        sheet.push_str("</cols>");
    }
    sheet.push_str("<sheetData><row r=\"1\">");
    for (index, column) in set.columns.iter().enumerate() {
        xlsx_text(&mut sheet, &letters[index], 1, &column.name, true);
    }
    sheet.push_str("</row>");
    for (number, row) in set.rows.iter().enumerate() {
        let line = number + 2;
        let _ = write!(sheet, "<row r=\"{line}\">");
        for (index, text) in row.iter().enumerate() {
            let at = &letters[index];
            match value(&set.columns[index], text) {
                Value::Null => {}
                Value::Number(number)
                    if number.chars().filter(char::is_ascii_digit).count() <= XLSX_DIGITS =>
                {
                    let _ = write!(sheet, "<c r=\"{at}{line}\"><v>{number}</v></c>");
                }
                Value::Bool(flag) => {
                    let _ = write!(
                        sheet,
                        "<c r=\"{at}{line}\" t=\"b\"><v>{}</v></c>",
                        u8::from(flag)
                    );
                }
                Value::Number(text) | Value::Text(text) => {
                    xlsx_text(&mut sheet, at, line, text, false);
                }
            }
        }
        sheet.push_str("</row>");
    }
    sheet.push_str("</sheetData>");
    if !set.columns.is_empty() {
        let _ = write!(
            sheet,
            "<autoFilter ref=\"A1:{last}{}\"/>",
            set.rows.len() + 1
        );
    }
    sheet.push_str("</worksheet>");

    // Sheet names: at most 31 characters, none of `[]:*?/\`.
    let name: String = title
        .chars()
        .filter(|c| !matches!(c, '[' | ']' | ':' | '*' | '?' | '/' | '\\') && allowed_in_xml(*c))
        .take(31)
        .collect();
    let name = if name.trim().is_empty() {
        "Result".to_owned()
    } else {
        name
    };
    let workbook = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
         <workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" \
         xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\">\
         <sheets><sheet name=\"{}\" sheetId=\"1\" r:id=\"rId1\"/></sheets>\
         {}</workbook>",
        escape_markup(&name),
        if set.columns.is_empty() {
            String::new()
        } else {
            format!(
                "<definedNames><definedName name=\"_xlnm._FilterDatabase\" localSheetId=\"0\" \
                 hidden=\"1\">'{}'!$A$1:${last}${}</definedName></definedNames>",
                escape_markup(&name.replace('\'', "''")),
                set.rows.len() + 1
            )
        }
    );

    let files: [(&str, &str); 6] = [
        (
            "[Content_Types].xml",
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
             <Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
             <Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
             <Default Extension=\"xml\" ContentType=\"application/xml\"/>\
             <Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/>\
             <Override PartName=\"/xl/worksheets/sheet1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/>\
             <Override PartName=\"/xl/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml\"/>\
             </Types>",
        ),
        (
            "_rels/.rels",
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
             <Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
             <Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"xl/workbook.xml\"/>\
             </Relationships>",
        ),
        ("xl/workbook.xml", &workbook),
        (
            "xl/_rels/workbook.xml.rels",
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
             <Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
             <Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"worksheets/sheet1.xml\"/>\
             <Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\"/>\
             </Relationships>",
        ),
        (
            "xl/styles.xml",
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
             <styleSheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\">\
             <fonts count=\"2\"><font><sz val=\"11\"/><name val=\"Calibri\"/></font>\
             <font><b/><sz val=\"11\"/><name val=\"Calibri\"/></font></fonts>\
             <fills count=\"2\"><fill><patternFill patternType=\"none\"/></fill>\
             <fill><patternFill patternType=\"gray125\"/></fill></fills>\
             <borders count=\"1\"><border><left/><right/><top/><bottom/><diagonal/></border></borders>\
             <cellStyleXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\"/></cellStyleXfs>\
             <cellXfs count=\"2\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\"/>\
             <xf numFmtId=\"0\" fontId=\"1\" fillId=\"0\" borderId=\"0\" xfId=\"0\" applyFont=\"1\"/></cellXfs>\
             <cellStyles count=\"1\"><cellStyle name=\"Normal\" xfId=\"0\" builtinId=\"0\"/></cellStyles>\
             </styleSheet>",
        ),
        ("xl/worksheets/sheet1.xml", &sheet),
    ];
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (path, contents) in files {
        zip.start_file(path, options)?;
        zip.write_all(contents.as_bytes())?;
    }
    Ok(zip.finish()?.into_inner())
}

/// An inline string cell; the header's is bold.
fn xlsx_text(sheet: &mut String, column: &str, row: usize, text: &str, bold: bool) {
    let text = match text.char_indices().nth(XLSX_MAX_TEXT) {
        Some((end, _)) => &text[..end],
        None => text,
    };
    let style = if bold { " s=\"1\"" } else { "" };
    let _ = write!(
        sheet,
        "<c r=\"{column}{row}\"{style} t=\"inlineStr\"><is><t xml:space=\"preserve\">{}</t></is></c>",
        escape_markup(text)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ResultSet {
        let column = |name: &str, kind: &str, numeric: bool| ColumnMeta {
            name: name.into(),
            type_name: Some(kind.into()),
            numeric,
        };
        let mut set = ResultSet {
            columns: vec![
                column("id", "int4", true),
                column("note", "text", false),
                column("active", "bool", false),
            ],
            ..ResultSet::default()
        };
        set.rows.push([Some("1"), Some("plain"), Some("t")]);
        set.rows.push([Some("2"), Some("a, \"b\"\nc"), Some("f")]);
        set.rows.push([Some("3"), None, None]);
        set
    }

    fn options() -> Options<'static> {
        Options {
            table: "\"notes\"",
            quote: &|name| format!("\"{name}\""),
            title: "notes",
        }
    }

    #[test]
    fn csv_quotes_only_what_needs_it() {
        assert_eq!(
            to_delimited(&sample(), ','),
            "id,note,active\n1,plain,t\n2,\"a, \"\"b\"\"\nc\",f\n3,,"
        );
    }

    #[test]
    fn json_keeps_types() {
        let text = json(&sample());
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(parsed[0]["id"], 1);
        assert_eq!(parsed[0]["active"], true);
        assert_eq!(parsed[1]["note"], "a, \"b\"\nc");
        assert!(parsed[2]["note"].is_null());
        assert_eq!(json(&ResultSet::default()), "[]\n");
    }

    #[test]
    fn sql_writes_one_insert_per_row() {
        let text = sql(&sample(), &options());
        assert_eq!(
            text.lines().next(),
            Some("INSERT INTO \"notes\" (\"id\", \"note\", \"active\") VALUES (1, 'plain', TRUE);")
        );
        assert!(text.contains("VALUES (3, NULL, NULL);"));
    }

    #[test]
    fn markdown_escapes_pipes_and_breaks() {
        let mut set = sample();
        set.rows.push([Some("4"), Some("x|y\nz"), Some("t")]);
        let text = markdown(&set);
        assert!(text.starts_with("| id | note | active |\n| ---: | --- | --- |\n"));
        assert!(text.contains("| x\\|y<br>z |"));
    }

    #[test]
    fn markup_is_escaped() {
        let mut set = sample();
        set.rows.push([Some("4"), Some("<b>&\u{1}"), Some("t")]);
        assert!(html(&set, "t").contains("<td>&lt;b&gt;&amp;</td>"));
        assert!(xml(&set).contains("<field name=\"note\">&lt;b&gt;&amp;</field>"));
        assert!(xml(&set).contains("<field name=\"note\" null=\"true\"/>"));
    }

    #[test]
    fn columns_are_lettered_like_excel() {
        assert_eq!(column_letters(0), "A");
        assert_eq!(column_letters(25), "Z");
        assert_eq!(column_letters(26), "AA");
        assert_eq!(column_letters(701), "ZZ");
        assert_eq!(column_letters(702), "AAA");
    }

    #[test]
    fn xlsx_is_a_zip_with_the_sheet() {
        let bytes = xlsx(&sample(), "notes").expect("workbook");
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("zip");
        let mut sheet = String::new();
        std::io::Read::read_to_string(
            &mut archive.by_name("xl/worksheets/sheet1.xml").expect("sheet"),
            &mut sheet,
        )
        .expect("read");
        assert!(sheet.contains("<c r=\"A2\"><v>1</v></c>"));
        assert!(sheet.contains("<c r=\"C2\" t=\"b\"><v>1</v></c>"));
        assert!(sheet.contains("<autoFilter ref=\"A1:C4\"/>"));
    }
}
