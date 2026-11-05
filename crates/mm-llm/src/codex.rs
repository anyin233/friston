//! Request builders for `codex app-server` (protocol v2, verified against
//! `schema/codex_app_server_protocol.v2.schemas.json` from codex-cli 0.166.0).
//!
//! Flow: `initialize` → `initialized` notification → `thread/start`
//! (ephemeral, read-only sandbox, no approvals) → `turn/start` with the
//! transcript, notes and media, constrained by `outputSchema` → stream
//! `item/agentMessage/delta` → `turn/completed`.

use serde_json::{Value, json};

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

#[cfg(test)]
mod tests {
    use super::*;

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
