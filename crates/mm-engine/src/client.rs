//! Transport-agnostic JSON-RPC client.
//!
//! The client owns a writer thread and a reader thread so that it works with any
//! executor (GPUI's, tokio, or plain blocking tests). Responses are routed to the
//! caller through one-shot channels; notifications go to a shared event channel.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::Value;

use crate::protocol::{Incoming, RpcError, RpcRequest};

type Pending = Arc<Mutex<HashMap<u64, async_channel::Sender<Result<Value, RpcError>>>>>;

/// Events surfaced to the app besides request/response pairs.
#[derive(Debug, Clone, PartialEq)]
pub enum EngineEvent {
    /// A JSON-RPC notification from the engine.
    Notification { method: String, params: Value },
    /// The engine asked us something; answer with [`EngineClient::respond_error`] or `respond`.
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// The engine wrote something that is not valid JSON-RPC.
    ProtocolError(String),
    /// The engine closed stdout (crashed or exited).
    Closed,
}

/// Handle used to call an engine. Cheap to clone.
#[derive(Clone)]
pub struct EngineClient {
    next_id: Arc<AtomicU64>,
    pending: Pending,
    outbox: async_channel::Sender<String>,
    child: Option<Arc<Mutex<Child>>>,
}

impl EngineClient {
    /// Spawns an engine executable and connects to its stdio.
    ///
    /// Params: `program` – path to the helper binary; `args` – CLI arguments.
    /// Returns: the client plus the receiver for notifications/lifecycle events.
    pub fn spawn(
        program: &std::path::Path,
        args: &[&str],
    ) -> std::io::Result<(Self, async_channel::Receiver<EngineEvent>)> {
        Self::spawn_with_env(program, args, &[])
    }

    /// Spawns an engine executable with additional environment variables and connects to its stdio.
    ///
    /// Params: `program` – path to the helper binary; `args` – CLI arguments; `env` – extra env pairs.
    /// Returns: the client plus the receiver for notifications/lifecycle events.
    pub fn spawn_with_env(
        program: &std::path::Path,
        args: &[&str],
        env: &[(&str, &str)],
    ) -> std::io::Result<(Self, async_channel::Receiver<EngineEvent>)> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // stderr is reserved for engine logs; inherit so they land in the app log.
            .stderr(Stdio::inherit());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().map_err(|e| {
            std::io::Error::new(e.kind(), format!("spawning {}: {e}", program.display()))
        })?;
        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");
        let (mut client, events) = Self::connect(stdout, stdin);
        client.child = Some(Arc::new(Mutex::new(child)));
        Ok((client, events))
    }

    /// Connects to an engine over arbitrary byte streams (used by tests and sockets).
    ///
    /// Params: `reader` – engine stdout; `writer` – engine stdin.
    /// Returns: the client plus the receiver for notifications/lifecycle events.
    pub fn connect<R, W>(reader: R, writer: W) -> (Self, async_channel::Receiver<EngineEvent>)
    where
        R: Read + Send + 'static,
        W: Write + Send + 'static,
    {
        let pending: Pending = Arc::default();
        let (event_tx, event_rx) = async_channel::unbounded();
        let (out_tx, out_rx) = async_channel::unbounded::<String>();

        thread::Builder::new()
            .name("engine-writer".into())
            .spawn(move || {
                let mut writer = writer;
                while let Ok(line) = out_rx.recv_blocking() {
                    if let Err(e) = writer
                        .write_all(line.as_bytes())
                        .and_then(|_| writer.flush())
                    {
                        tracing::warn!("engine stdin closed while writing request: {e}");
                        break;
                    }
                }
            })
            .expect("spawn engine writer thread");

        let reader_pending = pending.clone();
        thread::Builder::new()
            .name("engine-reader".into())
            .spawn(move || {
                for line in BufReader::new(reader).lines() {
                    let line = match line {
                        Ok(l) if l.trim().is_empty() => continue,
                        Ok(l) => l,
                        Err(e) => {
                            let _ = event_tx.send_blocking(EngineEvent::ProtocolError(format!(
                                "reading engine stdout: {e}"
                            )));
                            break;
                        }
                    };
                    match Incoming::parse(&line) {
                        Ok(Incoming::Result { id, result }) => {
                            route(&reader_pending, id, Ok(result))
                        }
                        Ok(Incoming::Error { id, error }) => route(&reader_pending, id, Err(error)),
                        Ok(Incoming::Notification { method, params }) => {
                            let _ = event_tx
                                .send_blocking(EngineEvent::Notification { method, params });
                        }
                        Ok(Incoming::Request { id, method, params }) => {
                            let _ =
                                event_tx.send_blocking(EngineEvent::Request { id, method, params });
                        }
                        Err(e) => {
                            let _ = event_tx.send_blocking(EngineEvent::ProtocolError(e));
                        }
                    }
                }
                // Fail every in-flight call so no caller waits forever on a dead engine.
                let drained: Vec<_> = reader_pending
                    .lock()
                    .expect("pending lock")
                    .drain()
                    .collect();
                for (_, tx) in drained {
                    let _ = tx.send_blocking(Err(RpcError {
                        code: -32000,
                        message: "engine exited before replying".into(),
                        data: None,
                    }));
                }
                let _ = event_tx.send_blocking(EngineEvent::Closed);
            })
            .expect("spawn engine reader thread");

        let client = Self {
            next_id: Arc::new(AtomicU64::new(1)),
            pending,
            outbox: out_tx,
            child: None,
        };
        (client, event_rx)
    }

    /// Sends a request and returns a receiver that yields exactly one reply.
    ///
    /// Params: `method` – JSON-RPC method; `params` – JSON params.
    /// Returns: a channel whose single message is the result or the engine error.
    pub fn call(
        &self,
        method: &str,
        params: Value,
    ) -> async_channel::Receiver<Result<Value, RpcError>> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = async_channel::bounded(1);
        self.pending.lock().expect("pending lock").insert(id, tx);
        let mut line = serde_json::to_string(&RpcRequest::new(id, method, params))
            .expect("JSON-RPC request is always serializable");
        line.push('\n');
        if self.outbox.send_blocking(line).is_err() {
            route(
                &self.pending,
                id,
                Err(RpcError {
                    code: -32000,
                    message: format!("engine writer closed; cannot send {method}"),
                    data: None,
                }),
            );
        }
        rx
    }

    /// Blocking convenience wrapper around [`EngineClient::call`].
    ///
    /// Params: `method` – JSON-RPC method; `params` – JSON params.
    /// Returns: the result value or the engine error.
    pub fn call_blocking(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.call(method, params)
            .recv_blocking()
            .unwrap_or_else(|_| {
                Err(RpcError {
                    code: -32000,
                    message: format!("reply channel dropped for {method}"),
                    data: None,
                })
            })
    }

    /// Sends a notification (no reply expected), e.g. Codex's `initialized`.
    ///
    /// Params: `method` – notification name; `params` – JSON params (`null` omits them).
    /// Returns: an error string when the writer is gone.
    pub fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        let mut msg = serde_json::json!({"jsonrpc": "2.0", "method": method});
        if !params.is_null() {
            msg["params"] = params;
        }
        self.send_line(msg, method)
    }

    /// Replies to a server-initiated request.
    ///
    /// Params: `id` – the request id as received; `result` – reply value.
    /// Returns: an error string when the writer is gone.
    pub fn respond(&self, id: Value, result: Value) -> Result<(), String> {
        self.send_line(
            serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}),
            "response",
        )
    }

    /// Rejects a server-initiated request.
    ///
    /// Params: `id` – request id; `code` / `message` – JSON-RPC error.
    /// Returns: an error string when the writer is gone.
    pub fn respond_error(&self, id: Value, code: i64, message: &str) -> Result<(), String> {
        self.send_line(
            serde_json::json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
            "error response",
        )
    }

    /// Queues one JSON line for the writer thread.
    fn send_line(&self, msg: Value, what: &str) -> Result<(), String> {
        let mut line = msg.to_string();
        line.push('\n');
        self.outbox
            .send_blocking(line)
            .map_err(|_| format!("engine writer closed; cannot send {what}"))
    }

    /// Kills the child process if this client spawned one.
    pub fn kill(&self) {
        if let Some(child) = &self.child
            && let Err(e) = child.lock().expect("child lock").kill()
        {
            tracing::warn!("failed to kill engine process: {e}");
        }
    }
}

/// Delivers a reply to the caller waiting on `id`, if any.
fn route(pending: &Pending, id: u64, reply: Result<Value, RpcError>) {
    match pending.lock().expect("pending lock").remove(&id) {
        Some(tx) => {
            let _ = tx.send_blocking(reply);
        }
        None => tracing::warn!("engine replied to unknown request id {id}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{BufRead, BufReader, Write};

    /// Fake engine: echoes `params` back as result and emits one notification first.
    fn fake_engine() -> (EngineClient, async_channel::Receiver<EngineEvent>) {
        let (app_read, mut engine_write) = std::io::pipe().expect("pipe");
        let (engine_read, app_write) = std::io::pipe().expect("pipe");
        thread::spawn(move || {
            for line in BufReader::new(engine_read).lines() {
                let req: Value = serde_json::from_str(&line.expect("line")).expect("request json");
                writeln!(
                    engine_write,
                    r#"{{"jsonrpc":"2.0","method":"audio/levels","params":{{"rms":0.5}}}}"#
                )
                .expect("write notification");
                if req["method"] == "boom" {
                    writeln!(
                        engine_write,
                        r#"{{"jsonrpc":"2.0","id":{},"error":{{"code":-1,"message":"boom"}}}}"#,
                        req["id"]
                    )
                    .expect("write error");
                } else {
                    let resp = json!({"jsonrpc":"2.0","id":req["id"],"result":req["params"]});
                    writeln!(engine_write, "{resp}").expect("write result");
                }
            }
        });
        EngineClient::connect(app_read, app_write)
    }

    #[test]
    fn routes_results_errors_and_notifications() {
        let (client, events) = fake_engine();
        assert_eq!(
            client.call_blocking("echo", json!({"x": 1})).unwrap(),
            json!({"x": 1})
        );
        let err = client.call_blocking("boom", Value::Null).unwrap_err();
        assert_eq!(err.message, "boom");
        let ev = events.recv_blocking().expect("notification");
        assert_eq!(
            ev,
            EngineEvent::Notification {
                method: "audio/levels".into(),
                params: json!({"rms": 0.5})
            }
        );
    }

    #[test]
    fn in_flight_calls_fail_when_engine_exits() {
        let (app_read, engine_write) = std::io::pipe().expect("pipe");
        let (_engine_read, app_write) = std::io::pipe().expect("pipe");
        let (client, events) = EngineClient::connect(app_read, app_write);
        let reply = client.call("never", Value::Null);
        drop(engine_write); // engine "crashes"
        let err = reply.recv_blocking().expect("reply").unwrap_err();
        assert!(err.message.contains("exited"));
        assert_eq!(events.recv_blocking().unwrap(), EngineEvent::Closed);
    }
}
