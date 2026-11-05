//! Request builders for `codex app-server` (protocol v2, verified against
//! `schema/codex_app_server_protocol.v2.schemas.json` from codex-cli 0.166.0).
//!
//! Flow: `initialize` → `initialized` notification → `thread/start`
//! (ephemeral, read-only sandbox, no approvals) → `turn/start` with the
//! transcript, notes and media, constrained by `outputSchema` → stream
//! `item/agentMessage/delta` → `turn/completed`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mm_engine::{EngineClient, EngineEvent};
use serde_json::{Value, json};

use crate::{LlmError, Minutes, MinutesInput, byok::parse_minutes, instructions, minutes_json_schema};

/// A piece of user input for a Codex turn (subset of the protocol's `UserInput`).
#[derive(Debug, Clone, PartialEq)]
pub enum CodexInput {
    Text(String),
    /// Absolute path to an image (screenshot, slide keyframe, whiteboard photo).
    LocalImage(String),
    /// Absolute path to an audio clip (e.g. a passage the user flagged as unclear).
    LocalAudio(String),
}

impl CodexInput {
    /// Serialises to the protocol's tagged `UserInput` object.
    fn to_json(&self) -> Value {
        match self {
            CodexInput::Text(text) => json!({"type": "text", "text": text}),
            CodexInput::LocalImage(path) => json!({"type": "localImage", "path": path}),
            CodexInput::LocalAudio(path) => json!({"type": "localAudio", "path": path}),
        }
    }
}

/// `initialize` params.
///
/// Params: `version` – this app's version string.
/// Returns: params with `clientInfo`.
pub fn initialize_params(version: &str) -> Value {
    json!({"clientInfo": {"name": "meeting-master", "title": "Meeting Master", "version": version}})
}

/// `thread/start` params for a side-effect-free minutes thread.
///
/// Params: `developer_instructions` – system-style guidance for minutes writing.
/// Returns: params that keep the thread ephemeral, read-only and approval-free.
pub fn thread_start_params(developer_instructions: &str) -> Value {
    json!({
        "ephemeral": true,
        // Minutes generation never needs to touch the filesystem or run commands.
        "sandbox": "read-only",
        "approvalPolicy": "never",
        "developerInstructions": developer_instructions,
        "serviceName": "meeting-master"
    })
}

/// `turn/start` params.
///
/// Params: `thread_id` – id returned by `thread/start`; `inputs` – turn inputs; `output_schema` – JSON Schema.
/// Returns: params ready to send.
pub fn turn_start_params(thread_id: &str, inputs: &[CodexInput], output_schema: &Value) -> Value {
    json!({
        "threadId": thread_id,
        "input": inputs.iter().map(CodexInput::to_json).collect::<Vec<_>>(),
        "outputSchema": output_schema
    })
}

/// Finds a `codex` executable: `$PATH` first, then Homebrew's usual prefixes (GUI apps
/// launched from Finder do not inherit the shell's PATH).
///
/// Returns: the path, if found.
pub fn find_codex() -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from));
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".local/bin"));
    }
    dirs.into_iter().map(|d| d.join("codex")).find(|p| p.is_file())
}

/// Whether Codex is signed in, and as whom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexAccount {
    pub signed_in: bool,
    /// Human-readable account label (email or plan) when available.
    pub label: Option<String>,
}

/// A running `codex app-server` with the handshake done.
struct AppServer {
    client: EngineClient,
    events: async_channel::Receiver<EngineEvent>,
}

impl AppServer {
    /// Spawns `codex app-server` and performs `initialize` + `initialized`.
    fn start(codex: &Path) -> Result<Self, LlmError> {
        let (client, events) = EngineClient::spawn(codex, &["app-server"])
            .map_err(|e| LlmError::Codex(format!("starting {} app-server: {e}", codex.display())))?;
        client
            .call_blocking("initialize", initialize_params(env!("CARGO_PKG_VERSION")))
            .map_err(|e| LlmError::Codex(format!("initialize: {e}")))?;
        client
            .notify("initialized", Value::Null)
            .map_err(LlmError::Codex)?;
        Ok(Self { client, events })
    }

    /// Calls a method, mapping errors with the method name.
    fn call(&self, method: &str, params: Value) -> Result<Value, LlmError> {
        self.client
            .call_blocking(method, params)
            .map_err(|e| LlmError::Codex(format!("{method}: {e}")))
    }
}

impl Drop for AppServer {
    fn drop(&mut self) {
        self.client.kill();
    }
}

/// Asks Codex whether a ChatGPT (or API-key) account is signed in.
///
/// Params: `codex` – executable path.
/// Returns: the account state.
pub fn read_account(codex: &Path) -> Result<CodexAccount, LlmError> {
    let server = AppServer::start(codex)?;
    let resp = server.call("account/read", json!({}))?;
    let account = &resp["account"];
    Ok(CodexAccount {
        signed_in: !account.is_null(),
        label: account["email"]
            .as_str()
            .or_else(|| account["planType"].as_str())
            .or_else(|| account["type"].as_str())
            .map(String::from),
    })
}

/// Pulls the final agent message out of a Codex event stream for one turn.
///
/// Params: `events` – notification iterator; `thread_id` – our thread; `deadline` – give up after.
/// `on_request` – answers server-initiated requests (we never approve anything).
/// Returns: the last agent message text of the turn.
fn collect_turn(
    events: &async_channel::Receiver<EngineEvent>,
    thread_id: &str,
    deadline: Instant,
    on_request: &dyn Fn(Value, &str),
) -> Result<String, LlmError> {
    let mut last_message: Option<String> = None;
    loop {
        if Instant::now() > deadline {
            return Err(LlmError::Codex("timed out waiting for turn/completed".into()));
        }
        let ev = match events.try_recv() {
            Ok(ev) => ev,
            Err(async_channel::TryRecvError::Empty) => {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            Err(async_channel::TryRecvError::Closed) => {
                return Err(LlmError::Codex("codex app-server exited mid-turn".into()));
            }
        };
        match ev {
            EngineEvent::Notification { method, params } => {
                if params["threadId"].as_str().is_some_and(|t| t != thread_id) {
                    continue;
                }
                match method.as_str() {
                    "item/completed" if params["item"]["type"] == "agentMessage" => {
                        last_message = params["item"]["text"].as_str().map(String::from);
                    }
                    "error" if params["willRetry"] != true => {
                        return Err(LlmError::Codex(format!("turn error: {}", params["error"])));
                    }
                    "turn/completed" => {
                        let turn = &params["turn"];
                        return match turn["status"].as_str() {
                            Some("completed") => last_message
                                .ok_or_else(|| LlmError::Codex("turn completed without an agent message".into())),
                            other => Err(LlmError::Codex(format!(
                                "turn ended with status {other:?}: {}",
                                turn["error"]
                            ))),
                        };
                    }
                    _ => {}
                }
            }
            EngineEvent::Request { id, method, .. } => on_request(id, &method),
            EngineEvent::ProtocolError(e) => tracing::warn!("codex app-server: {e}"),
            EngineEvent::Closed => return Err(LlmError::Codex("codex app-server exited mid-turn".into())),
        }
    }
}

/// Generates minutes through the user's signed-in Codex (ChatGPT account).
///
/// Params: `codex` – executable; `input` – meeting material; `timeout` – overall limit.
/// Returns: parsed minutes.
pub fn generate(codex: &Path, input: &MinutesInput, timeout: Duration) -> Result<Minutes, LlmError> {
    let deadline = Instant::now() + timeout;
    let server = AppServer::start(codex)?;
    let thread = server.call(
        "thread/start",
        thread_start_params(&instructions(input.language.as_deref())),
    )?;
    let thread_id = thread["thread"]["id"]
        .as_str()
        .ok_or_else(|| LlmError::Codex(format!("thread/start returned no thread id: {thread}")))?
        .to_string();
    let mut inputs = vec![CodexInput::Text(input.render())];
    inputs.extend(input.images.iter().cloned().map(CodexInput::LocalImage));
    server.call(
        "turn/start",
        turn_start_params(&thread_id, &inputs, &minutes_json_schema()),
    )?;
    let client = server.client.clone();
    let text = collect_turn(&server.events, &thread_id, deadline, &|id, method| {
        // The thread is read-only with approvalPolicy=never, so any request is unexpected: decline it.
        tracing::warn!("declining unexpected codex request {method}");
        if let Err(e) = client.respond_error(id, -32601, "meeting-master does not handle this request") {
            tracing::warn!("{e}");
        }
    })?;
    parse_minutes(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_turn_returns_last_agent_message_of_our_thread() {
        let (tx, rx) = async_channel::unbounded();
        let n = |method: &str, params: Value| EngineEvent::Notification { method: method.into(), params };
        tx.send_blocking(n("item/completed", json!({"threadId":"other","item":{"type":"agentMessage","text":"x"}}))).unwrap();
        tx.send_blocking(n("item/completed", json!({"threadId":"t","item":{"type":"agentMessage","text":"draft"}}))).unwrap();
        tx.send_blocking(EngineEvent::Request { id: json!(9), method: "approve".into(), params: Value::Null }).unwrap();
        tx.send_blocking(n("item/completed", json!({"threadId":"t","item":{"type":"agentMessage","text":"{}"}}))).unwrap();
        tx.send_blocking(n("turn/completed", json!({"threadId":"t","turn":{"status":"completed"}}))).unwrap();
        let declined = std::cell::Cell::new(0);
        let text = collect_turn(&rx, "t", Instant::now() + Duration::from_secs(5), &|_, _| declined.set(declined.get() + 1)).unwrap();
        assert_eq!(text, "{}");
        assert_eq!(declined.get(), 1);

        tx.send_blocking(n("turn/completed", json!({"threadId":"t","turn":{"status":"failed","error":{"message":"quota"}}}))).unwrap();
        let err = collect_turn(&rx, "t", Instant::now() + Duration::from_secs(5), &|_, _| {}).unwrap_err();
        assert!(err.to_string().contains("quota"));
    }

    /// Guards against silent protocol drift: every field we send must exist in the vendored schema.
    #[test]
    fn requests_match_vendored_protocol_schema() {
        let schema: Value = serde_json::from_str(include_str!(
            "../schema/codex_app_server_protocol.v2.schemas.json"
        ))
        .unwrap();
        let defs = &schema["definitions"];
        let check = |def: &str, params: &Value| {
            let props = defs[def]["properties"]
                .as_object()
                .unwrap_or_else(|| panic!("{def} missing"));
            for key in params.as_object().unwrap().keys() {
                assert!(props.contains_key(key), "{def} has no field `{key}`");
            }
            for req in defs[def]["required"].as_array().into_iter().flatten() {
                assert!(
                    params.get(req.as_str().unwrap()).is_some(),
                    "{def} requires `{req}`"
                );
            }
        };
        check("InitializeParams", &initialize_params("0.1.0"));
        check("ThreadStartParams", &thread_start_params("Write minutes."));
        let turn = turn_start_params(
            "thr_1",
            &[
                CodexInput::Text("t".into()),
                CodexInput::LocalImage("/a.png".into()),
                CodexInput::LocalAudio("/a.m4a".into()),
            ],
            &json!({"type": "object"}),
        );
        check("TurnStartParams", &turn);
        let input_types: Vec<String> = defs["UserInput"]["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|o| {
                o["properties"]["type"]["enum"][0]
                    .as_str()
                    .map(String::from)
            })
            .collect();
        for item in turn["input"].as_array().unwrap() {
            assert!(input_types.contains(&item["type"].as_str().unwrap().to_string()));
        }
    }
}
