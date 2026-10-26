//! An MCP server (<https://modelcontextprotocol.io>) for the agent panel's
//! agent: tools that read what Violet knows of the user's databases, their
//! connections, schemas, tables and the open editor, so the agent can write
//! queries for them. No tool runs a query: the agent writes SQL, the user
//! runs it.
//!
//! It speaks MCP's Streamable HTTP transport on a loopback port, each
//! answer as plain JSON, and takes only requests that carry its random
//! token. Calls go to the interface thread, which answers from its state.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use serde_json::{Value, json};

/// The largest request taken, in bytes.
const MAX_BODY: usize = 1 << 20;

/// A tool call for the interface thread to answer.
#[derive(Debug)]
pub struct Call {
    pub tool: String,
    pub arguments: Value,
    /// The tool's text, or why it failed.
    pub reply: Sender<Result<String, String>>,
}

/// The running server. It stops with the process; its thread holds only
/// the listener and the channel.
pub struct Server {
    pub url: String,
    pub token: String,
    pub calls: Receiver<Call>,
}

impl Server {
    /// Listens on a free loopback port; `wake` runs when a call comes.
    pub fn start(wake: impl Fn() + Send + Sync + 'static) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let url = format!("http://{}/mcp", listener.local_addr()?);
        let token = uuid::Uuid::new_v4().simple().to_string();
        let (sender, calls) = channel();
        let expected = format!("Bearer {token}");
        std::thread::Builder::new()
            .name("mcp".to_owned())
            .spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { continue };
                    let sender = sender.clone();
                    let expected = expected.clone();
                    let wake = &wake;
                    // One request at a time: the agent waits on each call.
                    if let Err(error) = serve(stream, &expected, &sender, wake) {
                        log::debug!("mcp: {error}");
                    }
                }
            })?;
        Ok(Self { url, token, calls })
    }
}

struct HttpRequest {
    method: String,
    authorization: Option<String>,
    body: Vec<u8>,
}

fn read_request(stream: &mut BufReader<&TcpStream>) -> std::io::Result<HttpRequest> {
    let mut line = String::new();
    stream.read_line(&mut line)?;
    let method = line
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned();
    let mut length = 0;
    let mut authorization = None;
    loop {
        line.clear();
        if stream.read_line(&mut line)? == 0 || line.trim().is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            length = value.parse().unwrap_or(0);
        } else if name.eq_ignore_ascii_case("authorization") {
            authorization = Some(value.to_owned());
        }
    }
    if length > MAX_BODY {
        return Err(std::io::Error::other("request too large"));
    }
    let mut body = vec![0; length];
    stream.read_exact(&mut body)?;
    Ok(HttpRequest {
        method,
        authorization,
        body,
    })
}

fn respond(stream: &mut TcpStream, status: &str, body: Option<&Value>) -> std::io::Result<()> {
    let body = body.map(Value::to_string).unwrap_or_default();
    let kind = if body.is_empty() {
        ""
    } else {
        "Content-Type: application/json\r\n"
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\n{kind}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}

fn serve(
    mut stream: TcpStream,
    expected: &str,
    sender: &Sender<Call>,
    wake: &(dyn Fn() + Send + Sync),
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let request = read_request(&mut BufReader::new(&stream))?;
    if request.authorization.as_deref() != Some(expected) {
        return respond(&mut stream, "401 Unauthorized", None);
    }
    if request.method != "POST" {
        return respond(&mut stream, "405 Method Not Allowed", None);
    }
    let Ok(message) = serde_json::from_slice::<Value>(&request.body) else {
        return respond(
            &mut stream,
            "400 Bad Request",
            Some(&rpc_error(&Value::Null, -32700, "not JSON")),
        );
    };
    match answer(&message, sender, wake) {
        Some(reply) => respond(&mut stream, "200 OK", Some(&reply)),
        // A notification, or a response: nothing to say.
        None => respond(&mut stream, "202 Accepted", None),
    }
}

fn rpc_error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// The answer to one JSON-RPC message; `None` for notifications.
fn answer(
    message: &Value,
    sender: &Sender<Call>,
    wake: &(dyn Fn() + Send + Sync),
) -> Option<Value> {
    let id = message.get("id")?.clone();
    let method = message.get("method").and_then(Value::as_str)?;
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => {
            let version = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("2025-06-18");
            json!({
                "protocolVersion": version,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "violet", "version": env!("CARGO_PKG_VERSION")},
                "instructions": "Violet's view of the user's databases. Read the schema \
                    here before writing a query. Nothing here runs a query."
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({"tools": tools()}),
        "tools/call" => {
            let Some(tool) = params.get("name").and_then(Value::as_str) else {
                return Some(rpc_error(&id, -32602, "name is missing"));
            };
            if !tools()
                .as_array()
                .is_some_and(|tools| tools.iter().any(|known| known["name"] == tool))
            {
                return Some(rpc_error(&id, -32602, &format!("no tool {tool}")));
            }
            let (reply, answer) = channel();
            let call = Call {
                tool: tool.to_owned(),
                arguments: params.get("arguments").cloned().unwrap_or(json!({})),
                reply,
            };
            if sender.send(call).is_err() {
                return Some(rpc_error(&id, -32603, "Violet is closing"));
            }
            wake();
            let (text, failed) = match answer.recv_timeout(Duration::from_secs(60)) {
                Ok(Ok(text)) => (text, false),
                Ok(Err(error)) => (error, true),
                Err(_) => ("Violet did not answer in time.".to_owned(), true),
            };
            json!({"content": [{"type": "text", "text": text}], "isError": failed})
        }
        _ => return Some(rpc_error(&id, -32601, &format!("no method {method}"))),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

/// The tools: all of them read, none runs anything on a database.
pub fn tools() -> Value {
    let connection = json!({
        "type": "string",
        "description": "The connection's name, as list_connections gives it. Leave it out for the active editor's connection."
    });
    json!([
        {
            "name": "list_connections",
            "description": "The user's saved connections: name, database product and version, whether connected, and which one the active editor uses.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false},
            "annotations": {"readOnlyHint": true}
        },
        {
            "name": "describe_schema",
            "description": "Tables and views of a connection with their columns, types, primary keys, comments and foreign keys. Without a schema, the ones the connection searches by default.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "connection": connection,
                    "schema": {"type": "string", "description": "A schema (or database, for MySQL) to describe."}
                },
                "additionalProperties": false
            },
            "annotations": {"readOnlyHint": true}
        },
        {
            "name": "describe_table",
            "description": "One table or view: every column with its type, nullability, default, key and comment, and the foreign keys from and to it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "connection": connection,
                    "schema": {"type": "string"},
                    "table": {"type": "string"}
                },
                "required": ["table"],
                "additionalProperties": false
            },
            "annotations": {"readOnlyHint": true}
        },
        {
            "name": "get_editor",
            "description": "The active SQL editor: its text, its selection and its connection. Passwords in it are hidden.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false},
            "annotations": {"readOnlyHint": true}
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post(url: &str, token: &str, body: &Value) -> (u16, Option<Value>) {
        let address = url.trim_start_matches("http://").trim_end_matches("/mcp");
        let mut stream = TcpStream::connect(address).expect("connect");
        let body = body.to_string();
        write!(
            stream,
            "POST /mcp HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .expect("write");
        let mut text = String::new();
        stream.read_to_string(&mut text).expect("read");
        let status = text[9..12].parse().expect("status");
        let body = text
            .split_once("\r\n\r\n")
            .map(|(_, body)| body)
            .unwrap_or("");
        (status, serde_json::from_str(body).ok())
    }

    #[test]
    fn tools_list_and_calls_go_to_the_interface() {
        let server = Server::start(|| {}).expect("start");
        let (status, _) = post(&server.url, "wrong", &json!({}));
        assert_eq!(status, 401);
        let (status, reply) = post(
            &server.url,
            &server.token,
            &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}),
        );
        assert_eq!(status, 200);
        assert_eq!(
            reply.expect("reply")["result"]["protocolVersion"],
            "2025-06-18"
        );
        let (status, _) = post(
            &server.url,
            &server.token,
            &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        );
        assert_eq!(status, 202);
        let (_, reply) = post(
            &server.url,
            &server.token,
            &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        );
        assert_eq!(
            reply.expect("reply")["result"]["tools"]
                .as_array()
                .map(Vec::len),
            Some(4)
        );

        let calls = server.calls;
        let interface = std::thread::spawn(move || {
            let call = calls.recv().expect("a call");
            assert_eq!(call.tool, "get_editor");
            call.reply.send(Ok("SELECT 1".into())).expect("reply");
        });
        let (_, reply) = post(
            &server.url,
            &server.token,
            &json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "get_editor", "arguments": {}}}),
        );
        interface.join().expect("interface");
        let reply = reply.expect("reply");
        assert_eq!(reply["result"]["content"][0]["text"], "SELECT 1");
        assert_eq!(reply["result"]["isError"], false);
        let (_, reply) = post(
            &server.url,
            &server.token,
            &json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "run_query"}}),
        );
        assert_eq!(reply.expect("reply")["error"]["code"], -32602);
    }
}
