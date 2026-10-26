//! Writing queries from a description, with a language model.
//!
//! The model sees the request, the connection's product and the schema as
//! the tree has it (tables, columns, keys); never rows. What it writes goes
//! into the editor for review and is never run on its own.
//!
//! Two kinds of provider: Anthropic's Messages API, and any server that
//! speaks OpenAI's chat completions (OpenAI, OpenRouter, LM Studio,
//! Ollama). Their API keys live in the keychain.

use std::fmt::Write as _;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use violet_core::{ObjectKind, Secret};

use crate::completion::{Catalog, Need};

/// Relations described to the model at most; past them it gets names only.
const MAX_TABLES: usize = 150;
/// Columns described per relation at most.
const MAX_COLUMNS: usize = 100;
/// Schemas described when the connection has no search path.
const MAX_SCHEMAS: usize = 5;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Provider {
    #[default]
    Anthropic,
    /// A server with OpenAI's chat completions API.
    OpenAi,
}

impl Provider {
    pub const ALL: [Self; 2] = [Self::Anthropic, Self::OpenAi];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic",
            Self::OpenAi => "OpenAI-compatible",
        }
    }

    /// The name its API key is kept under in the keychain.
    pub const fn key_name(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiSettings {
    pub provider: Provider,
    pub anthropic_model: String,
    /// The API's root, up to and with `/v1`.
    pub openai_url: String,
    pub openai_model: String,
}

impl Default for AiSettings {
    fn default() -> Self {
        Self {
            provider: Provider::Anthropic,
            anthropic_model: "claude-opus-5-5".to_owned(),
            openai_url: "https://api.openai.com/v1".to_owned(),
            openai_model: String::new(),
        }
    }
}

impl AiSettings {
    pub fn model(&self) -> &str {
        match self.provider {
            Provider::Anthropic => &self.anthropic_model,
            Provider::OpenAi => &self.openai_model,
        }
    }

    /// Whether a request can go out: a model, and a key where the
    /// provider needs one. Local servers often take none.
    pub fn ready(&self, has_key: bool) -> bool {
        !self.model().trim().is_empty()
            && match self.provider {
                Provider::Anthropic => has_key,
                Provider::OpenAi => !self.openai_url.trim().is_empty(),
            }
    }
}

/// The schema as the model reads it, and what is still to load for it.
#[derive(Debug, Default)]
pub struct Schema {
    pub text: String,
    pub needs: Vec<Need>,
    /// Relations described with their columns.
    pub described: usize,
    pub relations: usize,
}

/// Describes `schemas` from `catalog`: each table and view with its
/// columns, then the foreign keys. What is not loaded yet is listed in
/// [`Schema::needs`]; past [`MAX_TABLES`] relations only names follow.
pub fn describe(catalog: &dyn Catalog, schemas: &[String], foreign_keys: bool) -> Schema {
    let mut schema = Schema::default();
    for name in schemas {
        let Some(objects) = catalog.objects(name) else {
            schema.needs.push(Need::Objects(name.clone()));
            continue;
        };
        let _ = writeln!(schema.text, "schema {name}");
        let mut names_only = Vec::new();
        for object in objects {
            let kind = match object.kind {
                ObjectKind::Table | ObjectKind::ForeignTable => "table",
                ObjectKind::View => "view",
                ObjectKind::MaterializedView => "materialized view",
                ObjectKind::Key => "key",
                ObjectKind::Function | ObjectKind::Sequence => continue,
            };
            schema.relations += 1;
            if schema.described >= MAX_TABLES || object.kind == ObjectKind::Key {
                names_only.push(object.name.as_str());
                continue;
            }
            let Some(columns) = catalog.columns(name, &object.name) else {
                schema
                    .needs
                    .push(Need::Columns(name.clone(), object.name.clone()));
                continue;
            };
            schema.described += 1;
            let _ = write!(schema.text, "  {kind} {}", object.name);
            if let Some(rows) = object.estimated_rows.filter(|rows| *rows >= 0) {
                let _ = write!(schema.text, " (~{rows} rows)");
            }
            if let Some(comment) = object.comment.as_deref().filter(|c| !c.is_empty()) {
                let _ = write!(schema.text, " -- {}", one_line(comment));
            }
            schema.text.push('\n');
            for column in columns.iter().take(MAX_COLUMNS) {
                let _ = write!(schema.text, "    {} {}", column.name, column.data_type);
                if column.primary_key {
                    schema.text.push_str(" PRIMARY KEY");
                } else if !column.nullable {
                    schema.text.push_str(" NOT NULL");
                }
                if let Some(comment) = column.comment.as_deref().filter(|c| !c.is_empty()) {
                    let _ = write!(schema.text, " -- {}", one_line(comment));
                }
                schema.text.push('\n');
            }
            if columns.len() > MAX_COLUMNS {
                let _ = writeln!(schema.text, "    … {} more", columns.len() - MAX_COLUMNS);
            }
        }
        if !names_only.is_empty() {
            let _ = writeln!(schema.text, "  also: {}", names_only.join(", "));
        }
        if !foreign_keys {
            continue;
        }
        match catalog.foreign_keys(name) {
            Some(keys) => {
                for key in keys {
                    let _ = writeln!(
                        schema.text,
                        "  foreign key {}({}) -> {}.{}({})",
                        key.table,
                        key.columns.join(", "),
                        key.ref_schema,
                        key.ref_table,
                        key.ref_columns.join(", "),
                    );
                }
            }
            None => schema.needs.push(Need::ForeignKeys(name.clone())),
        }
    }
    schema
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The schemas to describe: the search path, else the first few.
pub fn schemas_to_describe(search_path: Vec<String>, all: &[String]) -> Vec<String> {
    if search_path.is_empty() {
        all.iter().take(MAX_SCHEMAS).cloned().collect()
    } else {
        search_path
    }
}

/// What the model is asked to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Task {
    /// Write a statement from a description.
    Write,
    /// Explain a statement in plain language.
    Explain,
    /// Correct a statement that failed with this error.
    Fix(String),
}

/// What a request is about.
#[derive(Clone, Debug)]
pub struct Request {
    /// The product, such as `PostgreSQL 17.2`.
    pub product: String,
    /// The language, such as `SQL` or `Redis commands`.
    pub language: String,
    pub schema: String,
    pub task: Task,
    /// The statement the task is about, secrets taken out: the selection
    /// when writing, the statement explained or fixed.
    pub statement: Option<String>,
    /// What the user typed; may be empty when explaining or fixing.
    pub prompt: String,
}

/// What the model wrote.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Answer {
    pub sql: String,
    #[serde(default)]
    pub notes: String,
}

fn system_prompt(request: &Request) -> String {
    let product = &request.product;
    let task = match &request.task {
        Task::Write => format!(
            "The user describes what they want; answer with the statement that does it, using \
             only the schemas, tables and columns listed below. Quote identifiers only where \
             {product} needs it. Prefer one statement; write several only when the request \
             needs them. If the request changes data or structure, still write it, and say so \
             in the notes. If the schema lacks what the request needs, leave `sql` empty and \
             say what is missing in the notes. Keep the notes to one or two short sentences."
        ),
        Task::Explain => "Explain the user's statement in plain language, for someone who \
             knows the data but not necessarily the syntax: what it reads or changes, how the \
             tables are joined and filtered, what each row of the result is, and anything \
             surprising, such as a join that repeats rows, a write without a WHERE, NULLs \
             compared with `=`, or work that cannot use an index. Put the explanation in the \
             notes, as a short paragraph or a few lines starting with \"- \". Leave `sql` empty."
            .to_owned(),
        Task::Fix(_) => "The user's statement failed with the error given. Write the \
             corrected statement in `sql`, changing only what the error needs and keeping the \
             rest of the text, its formatting and comments as they are. Use only the schemas, \
             tables and columns listed below. In the notes say in one or two short sentences \
             what was wrong. If it cannot be fixed from what is known, leave `sql` empty and say \
             why."
            .to_owned(),
    };
    format!(
        "You work with {language} for {product}, inside a database client. {task} Write the \
         notes in the language the user wrote their request in, or English when there is none. \
         Any statement goes into the user's editor for review; it is never run for them.\n\n\
         Schema:\n{schema}",
        language = request.language,
        schema = if request.schema.is_empty() {
            "(nothing loaded)"
        } else {
            &request.schema
        },
    )
}

fn user_prompt(request: &Request) -> String {
    let mut text = request.prompt.trim().to_owned();
    if let Some(statement) = &request.statement {
        let label = match request.task {
            Task::Write => "The statement selected in the editor, which the request may be about:",
            Task::Explain => "The statement to explain:",
            Task::Fix(_) => "The statement that failed:",
        };
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        let _ = write!(text, "{label}\n{statement}");
    }
    if let Task::Fix(error) = &request.task {
        let _ = write!(text, "\n\nThe error:\n{error}");
    }
    text
}

fn answer_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "sql": {"type": "string", "description": "The statement, without a code fence."},
            "notes": {"type": "string", "description": "What the statement does, or why there is none."}
        },
        "required": ["sql", "notes"],
        "additionalProperties": false
    })
}

/// Models that take Anthropic's refusal fallbacks.
const FALLBACK_MODELS: &[&str] = &[
    "claude-fable-5-1",
    "claude-opus-5-5",
    "claude-opus-5",
    "claude-sonnet-5-5",
];

fn anthropic_body(model: &str, request: &Request) -> Value {
    let mut body = json!({
        "model": model,
        "max_tokens": 16000,
        "system": system_prompt(request),
        "messages": [{"role": "user", "content": user_prompt(request)}],
        "output_config": {
            "effort": "medium",
            "format": {"type": "json_schema", "schema": answer_schema()}
        }
    });
    if FALLBACK_MODELS.contains(&model) {
        body["fallbacks"] = json!("default");
    }
    body
}

fn openai_body(model: &str, request: &Request) -> Value {
    json!({
        "model": model,
        "messages": [
            {"role": "system", "content": system_prompt(request)},
            {"role": "user", "content": user_prompt(request)}
        ],
        "response_format": {
            "type": "json_schema",
            "json_schema": {"name": "query", "strict": true, "schema": answer_schema()}
        }
    })
}

fn agent() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(15)))
            .timeout_global(Some(Duration::from_secs(5 * 60)))
            .build(),
    )
}

/// Asks the model. Blocks: run it off the interface thread.
pub fn ask(
    settings: &AiSettings,
    key: Option<&Secret>,
    request: &Request,
) -> Result<Answer, String> {
    let key = key.map(Secret::expose).filter(|key| !key.is_empty());
    let model = settings.model().trim();
    let (status, body) = match settings.provider {
        Provider::Anthropic => {
            let key = key.ok_or("No Anthropic API key is set.")?;
            let mut call = agent()
                .post("https://api.anthropic.com/v1/messages")
                .header("x-api-key", key)
                .header("anthropic-version", "2026-10-21")
                .header("content-type", "application/json");
            if FALLBACK_MODELS.contains(&model) {
                call = call.header("anthropic-beta", "server-side-fallback-2026-07-01");
            }
            send(call, &anthropic_body(model, request))?
        }
        Provider::OpenAi => {
            let url = format!(
                "{}/chat/completions",
                settings.openai_url.trim().trim_end_matches('/')
            );
            let mut call = agent()
                .post(&url)
                .header("content-type", "application/json");
            if let Some(key) = key {
                call = call.header("authorization", &format!("Bearer {key}"));
            }
            send(call, &openai_body(model, request))?
        }
    };
    if !(200..300).contains(&status) {
        return Err(api_error(status, &body));
    }
    match settings.provider {
        Provider::Anthropic => anthropic_answer(&body),
        Provider::OpenAi => openai_answer(&body),
    }
}

fn send(
    call: ureq::RequestBuilder<ureq::typestate::WithBody>,
    body: &Value,
) -> Result<(u16, Value), String> {
    let mut response = call
        .send(body.to_string())
        .map_err(|error| format!("The provider could not be reached: {error}"))?;
    let status = response.status().as_u16();
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|error| format!("The answer could not be read: {error}"))?;
    let value = serde_json::from_str(&text).unwrap_or(Value::String(text));
    Ok((status, value))
}

/// The error a provider answered with, as both APIs spell it:
/// `{"error": {"message": …}}`.
fn api_error(status: u16, body: &Value) -> String {
    let message = body
        .pointer("/error/message")
        .and_then(Value::as_str)
        .or_else(|| body.get("error").and_then(Value::as_str))
        .or_else(|| body.as_str())
        .unwrap_or("no details");
    let why = match status {
        401 | 403 => "The API key was refused",
        404 => "The model or address was not found",
        429 => "Too many requests for now",
        500.. => "The provider failed",
        _ => "The request was refused",
    };
    format!("{why} (HTTP {status}): {}", one_line(message))
}

fn anthropic_answer(body: &Value) -> Result<Answer, String> {
    match body.get("stop_reason").and_then(Value::as_str) {
        Some("refusal") => return Err("The model declined to answer this.".to_owned()),
        Some("max_tokens") => return Err("The answer was cut short.".to_owned()),
        _ => {}
    }
    let text: String = body
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect();
    parse_answer(&text)
}

fn openai_answer(body: &Value) -> Result<Answer, String> {
    let choice = body
        .pointer("/choices/0")
        .ok_or("The provider sent no answer.")?;
    if let Some(refusal) = choice.pointer("/message/refusal").and_then(Value::as_str) {
        return Err(format!("The model declined: {}", one_line(refusal)));
    }
    if choice.get("finish_reason").and_then(Value::as_str) == Some("length") {
        return Err("The answer was cut short.".to_owned());
    }
    let text = choice
        .pointer("/message/content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    parse_answer(text)
}

/// `fixed`, with the comment lines `original` starts with put back when
/// the model left them out: a fix replaces the whole run text, comments
/// above the statement included.
pub fn keep_leading_comments(original: &str, fixed: &str) -> String {
    let lead: String = original
        .split_inclusive('\n')
        .take_while(|line| {
            let line = line.trim();
            line.is_empty() || line.starts_with("--")
        })
        .collect();
    let comments = lead.trim();
    if comments.is_empty() || fixed.trim_start().starts_with(comments) {
        fixed.to_owned()
    } else {
        format!("{lead}{}", fixed.trim_start())
    }
}

/// Reads the answer: the JSON asked for, or, from servers that ignore the
/// format, the statement in a code fence or the whole text.
pub fn parse_answer(text: &str) -> Result<Answer, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("The model sent an empty answer.".to_owned());
    }
    let json = strip_fence(text).unwrap_or(text);
    if let Ok(mut answer) = serde_json::from_str::<Answer>(json) {
        answer.sql = strip_fence(&answer.sql)
            .unwrap_or(&answer.sql)
            .trim()
            .to_owned();
        answer.notes = answer.notes.trim().to_owned();
        return Ok(answer);
    }
    Ok(Answer {
        sql: strip_fence(text).unwrap_or(text).trim().to_owned(),
        notes: String::new(),
    })
}

/// The inside of a Markdown code fence, when `text` holds one.
fn strip_fence(text: &str) -> Option<&str> {
    let start = text.find("```")?;
    let after = &text[start + 3..];
    // The fence's language, such as `sql`, runs to the line's end.
    let body = &after[after.find('\n')? + 1..];
    let end = body.find("```")?;
    Some(body[..end].trim())
}

#[cfg(test)]
mod tests {
    use violet_core::{ColumnInfo, DbObject, ForeignKey};

    use super::*;

    struct Fake {
        objects: Vec<DbObject>,
        columns: Vec<ColumnInfo>,
        keys: Vec<ForeignKey>,
    }

    impl Catalog for Fake {
        fn schemas(&self) -> &[String] {
            &[]
        }

        fn objects(&self, schema: &str) -> Option<&[DbObject]> {
            (schema == "public").then_some(&self.objects[..])
        }

        fn columns(&self, _: &str, relation: &str) -> Option<&[ColumnInfo]> {
            (relation == "users").then_some(&self.columns[..])
        }

        fn foreign_keys(&self, schema: &str) -> Option<&[ForeignKey]> {
            (schema == "public").then_some(&self.keys[..])
        }
    }

    fn object(name: &str, kind: ObjectKind) -> DbObject {
        DbObject {
            schema: "public".into(),
            name: name.into(),
            kind,
            comment: None,
            estimated_rows: None,
            subtype: None,
            detail: None,
        }
    }

    fn column(name: &str, data_type: &str, primary_key: bool) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            data_type: data_type.into(),
            nullable: !primary_key,
            default: None,
            primary_key,
            comment: None,
        }
    }

    #[test]
    fn the_schema_reads_as_text_and_lists_what_is_missing() {
        let catalog = Fake {
            objects: vec![
                object("users", ObjectKind::Table),
                object("orders", ObjectKind::Table),
                object("next_id", ObjectKind::Sequence),
            ],
            columns: vec![
                column("id", "integer", true),
                column("email", "text", false),
            ],
            keys: vec![ForeignKey {
                name: "orders_user".into(),
                schema: "public".into(),
                table: "orders".into(),
                columns: vec!["user_id".into()],
                ref_schema: "public".into(),
                ref_table: "users".into(),
                ref_columns: vec!["id".into()],
            }],
        };
        let schema = describe(&catalog, &["public".into(), "audit".into()], true);
        assert_eq!(
            schema.text,
            "schema public\n  table users\n    id integer PRIMARY KEY\n    email text\n  \
             foreign key orders(user_id) -> public.users(id)\n"
        );
        assert_eq!(
            schema.needs,
            vec![
                Need::Columns("public".into(), "orders".into()),
                Need::Objects("audit".into()),
            ]
        );
        assert_eq!((schema.described, schema.relations), (1, 2));
    }

    #[test]
    fn a_fix_sends_the_statement_and_its_error() {
        let request = Request {
            product: "PostgreSQL 17".into(),
            language: "SQL".into(),
            schema: String::new(),
            task: Task::Fix("column \"nme\" does not exist".into()),
            statement: Some("SELECT nme FROM users".into()),
            prompt: String::new(),
        };
        assert_eq!(
            user_prompt(&request),
            "The statement that failed:\nSELECT nme FROM users\n\n\
             The error:\ncolumn \"nme\" does not exist"
        );
        assert!(system_prompt(&request).contains("(nothing loaded)"));
        let write = Request {
            task: Task::Write,
            statement: None,
            prompt: " every user ".into(),
            ..request
        };
        assert_eq!(user_prompt(&write), "every user");
    }

    #[test]
    fn a_fix_keeps_the_comments_above_the_statement() {
        let original = "-- totals\n\nSELECT nme FROM t;";
        assert_eq!(
            keep_leading_comments(original, "SELECT name FROM t;"),
            "-- totals\n\nSELECT name FROM t;"
        );
        assert_eq!(
            keep_leading_comments(original, "-- totals\nSELECT name FROM t;"),
            "-- totals\nSELECT name FROM t;"
        );
        assert_eq!(keep_leading_comments("SELECT 1", "SELECT 2"), "SELECT 2");
    }

    #[test]
    fn answers_read_from_json_or_from_a_fence() {
        assert_eq!(
            parse_answer(r#"{"sql": "SELECT 1;", "notes": " One row. "}"#),
            Ok(Answer {
                sql: "SELECT 1;".into(),
                notes: "One row.".into()
            })
        );
        assert_eq!(
            parse_answer("Here:\n```sql\nSELECT 2;\n```\nDone.").map(|a| a.sql),
            Ok("SELECT 2;".into())
        );
        assert_eq!(
            parse_answer("```json\n{\"sql\": \"SELECT 3\", \"notes\": \"\"}\n```").map(|a| a.sql),
            Ok("SELECT 3".into())
        );
        assert!(parse_answer("  ").is_err());
    }

    #[test]
    fn refusals_and_errors_read_as_messages() {
        let refused = json!({"stop_reason": "refusal", "content": []});
        assert!(anthropic_answer(&refused).is_err());
        let ok = json!({
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "{\"sql\":\"SELECT 1\",\"notes\":\"\"}"}]
        });
        assert_eq!(anthropic_answer(&ok).map(|a| a.sql), Ok("SELECT 1".into()));
        let error =
            json!({"error": {"type": "authentication_error", "message": "invalid x-api-key"}});
        assert_eq!(
            api_error(401, &error),
            "The API key was refused (HTTP 401): invalid x-api-key"
        );
        let chat =
            json!({"choices": [{"finish_reason": "stop", "message": {"content": "SELECT 4"}}]});
        assert_eq!(openai_answer(&chat).map(|a| a.sql), Ok("SELECT 4".into()));
    }
}
