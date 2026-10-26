//! A coding agent beside the editor, such as Claude Code, spoken to over
//! the Agent Client Protocol (<https://agentclientprotocol.com>): JSON-RPC
//! 2.0, one message a line, on the agent's standard input and output.
//!
//! The agent runs as a child process started by a command the user can
//! change. It goes through an interactive login shell, so programs set up
//! in the user's shell files (`npx` from nvm, say) are found from a GUI app
//! too.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

/// Claude Code, through its ACP adapter.
pub const DEFAULT_COMMAND: &str = "npx -y @agentclientprotocol/claude-agent-acp";

/// The protocol version spoken.
pub const PROTOCOL_VERSION: u64 = 1;

/// Lines of the agent's standard error kept, to explain a failure.
const STDERR_LINES: usize = 40;

/// What came from the agent.
#[derive(Debug)]
pub enum Incoming {
    /// The answer to one of our requests.
    Response {
        id: u64,
        result: Result<Value, RpcError>,
    },
    /// The agent asks something of us; it waits for [`Agent::respond`].
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    /// The process ended; the last lines it wrote to standard error.
    Exited(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// ACP's error for a session that needs the user to log in first.
pub const AUTH_REQUIRED: i64 = -32000;

/// A running agent.
pub struct Agent {
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    next_id: AtomicU64,
    pub incoming: Receiver<Incoming>,
}

impl Agent {
    /// Starts `command` in `cwd`; `wake` runs whenever a message arrives.
    pub fn spawn(
        command: &str,
        cwd: &Path,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> std::io::Result<Self> {
        let mut child = shell(command)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdin = child.stdin.take().ok_or_else(missing_pipe)?;
        let stdout = child.stdout.take().ok_or_else(missing_pipe)?;
        let stderr = child.stderr.take().ok_or_else(missing_pipe)?;
        let (sender, incoming) = channel();
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(wake);
        let tail = Arc::new(Mutex::new(VecDeque::new()));
        {
            let tail = Arc::clone(&tail);
            std::thread::Builder::new()
                .name("agent-stderr".to_owned())
                .spawn(move || {
                    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                        log::debug!("agent: {line}");
                        let mut tail = lock(&tail);
                        if tail.len() == STDERR_LINES {
                            tail.pop_front();
                        }
                        tail.push_back(line);
                    }
                })?;
        }
        std::thread::Builder::new()
            .name("agent-stdout".to_owned())
            .spawn(move || read(stdout, &sender, &tail, wake.as_ref()))?;
        Ok(Self {
            child,
            stdin: Arc::new(Mutex::new(stdin)),
            next_id: AtomicU64::new(0),
            incoming,
        })
    }

    /// Sends a request; its answer comes as [`Incoming::Response`] with
    /// the id returned.
    pub fn request(&self, method: &str, params: Value) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        id
    }

    pub fn notify(&self, method: &str, params: Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// Answers one of the agent's requests.
    pub fn respond(&self, id: &Value, result: Result<Value, RpcError>) {
        let message = match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(error) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": error.code, "message": error.message}
            }),
        };
        self.send(&message);
    }

    fn send(&self, message: &Value) {
        // `Value`'s text never holds a raw newline: one message a line.
        let mut line = message.to_string();
        line.push('\n');
        let mut stdin = lock(&self.stdin);
        if let Err(error) = stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.flush())
        {
            log::warn!("agent: could not write: {error}");
        }
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn missing_pipe() -> std::io::Error {
    std::io::Error::other("the agent's pipes were not opened")
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// `command` run by the user's login shell, which sets up their `PATH`.
fn shell(command: &str) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        /// No console window flashing up for the agent.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut shell = Command::new("cmd");
        shell.args(["/C", command]).creation_flags(CREATE_NO_WINDOW);
        shell
    }
    #[cfg(not(windows))]
    {
        let program = std::env::var("SHELL")
            .ok()
            .filter(|shell| !shell.is_empty())
            .unwrap_or_else(|| "/bin/sh".to_owned());
        let mut shell = Command::new(program);
        shell.arg("-lic").arg(format!("exec {command}"));
        shell
    }
}

fn read(
    stdout: impl std::io::Read,
    sender: &Sender<Incoming>,
    tail: &Mutex<VecDeque<String>>,
    wake: &(dyn Fn() + Send + Sync),
) {
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        match parse(&line) {
            Some(message) => {
                if sender.send(message).is_err() {
                    return;
                }
                wake();
            }
            None => log::warn!("agent: not a message: {line}"),
        }
    }
    let tail = lock(tail).iter().cloned().collect::<Vec<_>>().join("\n");
    let _ = sender.send(Incoming::Exited(tail));
    wake();
}

/// Reads one line of JSON-RPC.
pub fn parse(line: &str) -> Option<Incoming> {
    let mut value: Value = serde_json::from_str(line).ok()?;
    let method = value
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let params = value.get_mut("params").map_or(Value::Null, Value::take);
    match (method, value.get("id").cloned()) {
        (Some(method), Some(id)) if !id.is_null() => Some(Incoming::Request { id, method, params }),
        (Some(method), _) => Some(Incoming::Notification { method, params }),
        (None, Some(id)) => {
            let id = id.as_u64()?;
            let result = match value.get_mut("error") {
                Some(error) if !error.is_null() => Err(RpcError {
                    code: error
                        .get("code")
                        .and_then(Value::as_i64)
                        .unwrap_or_default(),
                    message: error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("the agent failed")
                        .to_owned(),
                }),
                _ => Ok(value.get_mut("result").map_or(Value::Null, Value::take)),
            };
            Some(Incoming::Response { id, result })
        }
        (None, None) => None,
    }
}

/// The `initialize` request's parameters. Violet offers the agent no
/// files and no terminal: it only writes queries.
pub fn initialize_params() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "clientCapabilities": {
            "fs": {"readTextFile": false, "writeTextFile": false},
            "terminal": false
        },
        "clientInfo": {
            "name": "violet",
            "title": "Violet",
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_read_by_their_shape() {
        assert!(matches!(
            parse(r#"{"jsonrpc":"2.0","id":3,"result":{"sessionId":"s"}}"#),
            Some(Incoming::Response {
                id: 3,
                result: Ok(_)
            })
        ));
        assert!(matches!(
            parse(
                r#"{"jsonrpc":"2.0","id":4,"error":{"code":-32000,"message":"Authentication required"}}"#
            ),
            Some(Incoming::Response {
                id: 4,
                result: Err(RpcError {
                    code: AUTH_REQUIRED,
                    ..
                })
            })
        ));
        let Some(Incoming::Request { id, method, params }) = parse(
            r#"{"jsonrpc":"2.0","id":"p1","method":"session/request_permission","params":{"options":[]}}"#,
        ) else {
            panic!("a request");
        };
        assert_eq!(
            (id, method.as_str()),
            (json!("p1"), "session/request_permission")
        );
        assert_eq!(params, json!({"options": []}));
        assert!(matches!(
            parse(r#"{"jsonrpc":"2.0","method":"session/update","params":{}}"#),
            Some(Incoming::Notification { .. })
        ));
        assert!(parse("not json").is_none());
    }
}
