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

use crate::{
    LlmError, Minutes, MinutesInput, ModelOption, OpenAiModelFamily,
    byok::parse_minutes,
    codex_refine_developer_instructions, codex_refine_user_prompt, instructions,
    minutes_json_schema,
    transcript::{
        AudioTrack, HeardLine, parse_transcript_and_minutes, transcript_and_minutes_schema,
    },
};

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
    json!({"clientInfo": {"name": "meeting-master", "title": "Friston", "version": version}})
}

/// `thread/start` params for a side-effect-free Codex thread, optionally rooted at `cwd`.
///
/// Params: `developer_instructions` – system-style guidance; `model` – a `model/list` id,
/// or `None` for Codex's configured default; `cwd` – optional read-only working root (e.g.
/// the recording folder so Codex can inspect local audio files).
/// Returns: params that keep the thread ephemeral, read-only and approval-free.
pub fn thread_start_params_in(
    developer_instructions: &str,
    model: Option<&str>,
    cwd: Option<&Path>,
) -> Value {
    let mut params = json!({
        "ephemeral": true,
        // Never modify the workspace or prompt interactively for approvals.
        "sandbox": "read-only",
        "approvalPolicy": "never",
        "developerInstructions": developer_instructions,
        "serviceName": "meeting-master"
    });
    if let Some(m) = model.map(str::trim).filter(|m| !m.is_empty()) {
        params["model"] = json!(m);
    }
    if let Some(dir) = cwd {
        params["cwd"] = json!(dir.display().to_string());
    }
    params
}

/// `thread/start` params for a side-effect-free minutes thread.
///
/// Params: `developer_instructions` – system-style guidance for minutes writing; `model` –
/// a `model/list` id, or `None` for Codex's configured default.
/// Returns: params that keep the thread ephemeral, read-only and approval-free.
pub fn thread_start_params(developer_instructions: &str, model: Option<&str>) -> Value {
    thread_start_params_in(developer_instructions, model, None)
}

/// `model/list` params for one page.
///
/// Params: `cursor` – `nextCursor` of the previous page.
/// Returns: params that leave hidden models out, as Codex's own picker does.
pub fn model_list_params(cursor: Option<&str>) -> Value {
    let mut params = json!({"includeHidden": false});
    if let Some(c) = cursor {
        params["cursor"] = json!(c);
    }
    params
}

/// Reads one `model/list` page.
///
/// Params: `resp` – the response.
/// Returns: the visible models and the next cursor.
pub fn parse_model_page(resp: &Value) -> (Vec<ModelOption>, Option<String>) {
    let models = resp["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["hidden"] != true)
        .filter_map(|m| {
            let id = m["model"]
                .as_str()
                .or_else(|| m["id"].as_str())?
                .to_string();
            Some(ModelOption {
                label: m["displayName"].as_str().unwrap_or(&id).to_string(),
                is_default: m["isDefault"] == true,
                id,
            })
        })
        .collect();
    (models, resp["nextCursor"].as_str().map(String::from))
}

/// Lists the models the signed-in account may use.
///
/// Params: `codex` – executable path.
/// Returns: the models in Codex's order.
pub fn list_models(codex: &Path) -> Result<Vec<ModelOption>, LlmError> {
    let server = AppServer::start(codex)?;
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    // A bounded loop: a server that keeps returning cursors must not hang settings.
    for _ in 0..20 {
        let resp = server.call("model/list", model_list_params(cursor.as_deref()))?;
        let (page, next) = parse_model_page(&resp);
        out.extend(page);
        match next {
            Some(n) if Some(&n) != cursor.as_ref() => cursor = Some(n),
            _ => break,
        }
    }
    Ok(out)
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

/// Finds a `codex` executable.
///
/// Returns: the path, if found.
pub fn find_codex() -> Option<PathBuf> {
    find_executable("codex")
}

/// Finds a CLI tool: `$PATH` first, then Homebrew's usual prefixes and `~/.local/bin` (GUI
/// apps launched from Finder do not inherit the shell's PATH).
///
/// Params: `name` – executable file name.
/// Returns: the path, if found.
pub fn find_executable(name: &str) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from));
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".local/bin"));
    }
    dirs.into_iter().map(|d| d.join(name)).find(|p| p.is_file())
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
        let (client, events) = EngineClient::spawn(codex, &["app-server"]).map_err(|e| {
            LlmError::Codex(format!("starting {} app-server: {e}", codex.display()))
        })?;
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
            return Err(LlmError::Codex(
                "timed out waiting for turn/completed".into(),
            ));
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
                            Some("completed") => last_message.ok_or_else(|| {
                                LlmError::Codex("turn completed without an agent message".into())
                            }),
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
            EngineEvent::Closed => {
                return Err(LlmError::Codex("codex app-server exited mid-turn".into()));
            }
        }
    }
}

/// Longest common ancestor directory across the parent directories of `tracks`.
///
/// Params: `tracks` – audio track paths.
/// Returns: the shared directory, or `None` when `tracks` is empty or has no common non-empty parent.
pub fn common_parent_dir(tracks: &[AudioTrack]) -> Option<PathBuf> {
    let parents: Vec<&Path> = tracks
        .iter()
        .filter_map(|t| t.path.parent().filter(|p| !p.as_os_str().is_empty()))
        .collect();
    let first = (*parents.first()?).to_path_buf();
    let mut common = first;
    for p in &parents[1..] {
        while !p.starts_with(&common) {
            if !common.pop() {
                return None;
            }
        }
    }
    (!common.as_os_str().is_empty()).then_some(common)
}

/// Runs a single ephemeral, read-only Codex turn and returns its final agent message text.
fn run_turn(
    codex: &Path,
    developer_instructions: &str,
    model: Option<&str>,
    cwd: Option<&Path>,
    inputs: &[CodexInput],
    output_schema: &Value,
    timeout: Duration,
) -> Result<String, LlmError> {
    let deadline = Instant::now() + timeout;
    let server = AppServer::start(codex)?;
    let thread = server.call(
        "thread/start",
        thread_start_params_in(developer_instructions, model, cwd),
    )?;
    let thread_id = thread["thread"]["id"]
        .as_str()
        .ok_or_else(|| LlmError::Codex(format!("thread/start returned no thread id: {thread}")))?
        .to_string();
    server.call(
        "turn/start",
        turn_start_params(&thread_id, inputs, output_schema),
    )?;
    let client = server.client.clone();
    collect_turn(&server.events, &thread_id, deadline, &|id, method| {
        // The thread is read-only with approvalPolicy=never, so any request is unexpected: decline it.
        tracing::warn!("declining unexpected codex request {method}");
        if let Err(e) =
            client.respond_error(id, -32601, "meeting-master does not handle this request")
        {
            tracing::warn!("{e}");
        }
    })
}

/// Generates minutes through the user's signed-in Codex (ChatGPT account).
///
/// Params: `codex` – executable; `input` – meeting material; `model` – `model/list` id, or
/// `None` for Codex's default; `timeout` – overall limit.
/// Returns: parsed minutes.
pub fn generate(
    codex: &Path,
    input: &MinutesInput,
    model: Option<&str>,
    timeout: Duration,
) -> Result<Minutes, LlmError> {
    let mut inputs = vec![CodexInput::Text(input.render())];
    inputs.extend(input.images.iter().cloned().map(CodexInput::LocalImage));
    let text = run_turn(
        codex,
        &instructions(input.language.as_deref()),
        model,
        None,
        &inputs,
        &minutes_json_schema(),
        timeout,
    )?;
    parse_minutes(&text)
}

/// Transcribes audio recordings, separates speakers, and writes meeting minutes in a single
/// Codex turn by giving Codex direct read-only access to the recording file paths.
///
/// Params: `codex` – executable; `tracks` – recording files on disk; `context` – title/names/terms
/// spelling hint; `caption_language` – spoken transcript language hint (`None` auto-detects);
/// `input` – metadata, user notes, attached images, and minutes language (its live transcript is
/// ignored); `model` – `model/list` id or `None` for Codex's default; `timeout` – overall limit.
/// Returns: `(heard_lines, minutes_result)`.
pub fn transcribe_and_summarize(
    codex: &Path,
    tracks: &[AudioTrack],
    context: Option<&str>,
    caption_language: Option<&str>,
    input: &MinutesInput,
    model: Option<&str>,
    timeout: Duration,
) -> Result<(Vec<HeardLine>, Result<Minutes, LlmError>), LlmError> {
    if tracks.is_empty() {
        return Err(LlmError::Codex("no audio tracks provided".into()));
    }
    let cwd = common_parent_dir(tracks);
    let family = OpenAiModelFamily::from_model(model);
    let dev =
        codex_refine_developer_instructions(family, caption_language, input.language.as_deref());
    let user = codex_refine_user_prompt(tracks, context, caption_language, input);
    let mut inputs = vec![CodexInput::Text(user)];
    inputs.extend(input.images.iter().cloned().map(CodexInput::LocalImage));
    let text = run_turn(
        codex,
        &dev,
        model,
        cwd.as_deref(),
        &inputs,
        &transcript_and_minutes_schema(),
        timeout,
    )?;
    parse_transcript_and_minutes(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_turn_returns_last_agent_message_of_our_thread() {
        let (tx, rx) = async_channel::unbounded();
        let n = |method: &str, params: Value| EngineEvent::Notification {
            method: method.into(),
            params,
        };
        tx.send_blocking(n(
            "item/completed",
            json!({"threadId":"other","item":{"type":"agentMessage","text":"x"}}),
        ))
        .unwrap();
        tx.send_blocking(n(
            "item/completed",
            json!({"threadId":"t","item":{"type":"agentMessage","text":"draft"}}),
        ))
        .unwrap();
        tx.send_blocking(EngineEvent::Request {
            id: json!(9),
            method: "approve".into(),
            params: Value::Null,
        })
        .unwrap();
        tx.send_blocking(n(
            "item/completed",
            json!({"threadId":"t","item":{"type":"agentMessage","text":"{}"}}),
        ))
        .unwrap();
        tx.send_blocking(n(
            "turn/completed",
            json!({"threadId":"t","turn":{"status":"completed"}}),
        ))
        .unwrap();
        let declined = std::cell::Cell::new(0);
        let text = collect_turn(
            &rx,
            "t",
            Instant::now() + Duration::from_secs(5),
            &|_, _| declined.set(declined.get() + 1),
        )
        .unwrap();
        assert_eq!(text, "{}");
        assert_eq!(declined.get(), 1);

        tx.send_blocking(n(
            "turn/completed",
            json!({"threadId":"t","turn":{"status":"failed","error":{"message":"quota"}}}),
        ))
        .unwrap();
        let err = collect_turn(
            &rx,
            "t",
            Instant::now() + Duration::from_secs(5),
            &|_, _| {},
        )
        .unwrap_err();
        assert!(err.to_string().contains("quota"));
    }

    #[test]
    fn model_pages_skip_hidden_models() {
        let resp = json!({
            "data": [
                {"id": "gpt-6.1-sol", "model": "gpt-6.1-sol", "displayName": "GPT-6.1-Sol", "isDefault": true, "hidden": false},
                {"id": "x", "model": "x-internal", "displayName": "X", "isDefault": false, "hidden": true},
                {"id": "gpt-6-luna", "model": "gpt-6-luna", "displayName": "GPT-6-Luna", "isDefault": false, "hidden": false}
            ],
            "nextCursor": "c2"
        });
        let (models, next) = parse_model_page(&resp);
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["gpt-6.1-sol", "gpt-6-luna"]
        );
        assert!(models[0].is_default);
        assert_eq!(models[1].label, "GPT-6-Luna");
        assert_eq!(next.as_deref(), Some("c2"));
        assert_eq!(parse_model_page(&json!({"data": []})).1, None);
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
        check(
            "ThreadStartParams",
            &thread_start_params("Write minutes.", None),
        );
        let with_model = thread_start_params("Write minutes.", Some("gpt-6.1-terra"));
        assert_eq!(with_model["model"], "gpt-6.1-terra");
        check("ThreadStartParams", &with_model);
        let with_cwd =
            thread_start_params_in("Transcribe.", Some("gpt-6.2-sol"), Some(Path::new("/rec")));
        assert_eq!(with_cwd["cwd"], "/rec");
        check("ThreadStartParams", &with_cwd);
        assert!(thread_start_params("w", Some(" ")).get("model").is_none());
        check("ModelListParams", &model_list_params(Some("c1")));
        for key in ["model", "displayName", "isDefault", "hidden", "id"] {
            assert!(
                defs["Model"]["properties"].get(key).is_some(),
                "Model has no field `{key}`"
            );
        }
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
        let refine_turn = turn_start_params(
            "thr_2",
            &[CodexInput::Text("p".into())],
            &transcript_and_minutes_schema(),
        );
        check("TurnStartParams", &refine_turn);
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

    #[test]
    fn common_parent_dir_handles_same_mixed_and_empty_tracks() {
        use mm_core::AudioOrigin;

        assert_eq!(common_parent_dir(&[]), None);
        let same = [
            AudioTrack {
                origin: AudioOrigin::Microphone,
                path: PathBuf::from("/Users/me/Movies/Friston/s1/microphone.caf"),
            },
            AudioTrack {
                origin: AudioOrigin::System,
                path: PathBuf::from("/Users/me/Movies/Friston/s1/system.caf"),
            },
        ];
        assert_eq!(
            common_parent_dir(&same),
            Some(PathBuf::from("/Users/me/Movies/Friston/s1"))
        );

        let mixed = [
            AudioTrack {
                origin: AudioOrigin::Microphone,
                path: PathBuf::from("/Users/me/Movies/Friston/s1/mic.caf"),
            },
            AudioTrack {
                origin: AudioOrigin::System,
                path: PathBuf::from("/Users/me/Movies/Friston/s2/sys.caf"),
            },
        ];
        assert_eq!(
            common_parent_dir(&mixed),
            Some(PathBuf::from("/Users/me/Movies/Friston"))
        );
    }
}
