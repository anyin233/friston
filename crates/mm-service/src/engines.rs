//! Locating, starting and calling the two Swift engines with typed results.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use mm_core::{
    AudioInput, AudioLevels, CalendarEvent, CaptionFinal, CaptionPartial, CapturePreset,
    CaptureSources, CaptureStarted, CaptureStateEvent, CaptureStopped, ClipRequest,
    FileTranscript, SpeakerRelabel, VoiceprintSample,
};
use mm_engine::methods::{common, embed, meeting};
use mm_engine::{EngineClient, EngineEvent, RpcError};
use mm_index::pipeline::{EmbedItem, Embedder};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::ServiceError;

/// Executable names of the helpers.
pub const MEETINGD: &str = "mm-meetingd";
pub const EMBEDD: &str = "mm-embedd";

/// Finds a helper executable.
///
/// Search order: `MM_ENGINES_DIR`; next to the running executable (inside the `.app`
/// bundle, `Contents/MacOS`); then `target/engines/{Debug,Release}` in any ancestor
/// folder (development runs from `target/debug`).
///
/// Params: `name` – executable name.
/// Returns: the path, or `None` when not built yet.
pub fn locate_engine(name: &str) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(dir) = std::env::var_os("MM_ENGINES_DIR") {
        candidates.push(PathBuf::from(dir).join(name));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(name));
        }
        for ancestor in exe.ancestors().skip(1) {
            for config in ["Debug", "Release"] {
                candidates.push(ancestor.join("target/engines").join(config).join(name));
                candidates.push(ancestor.join("engines").join(config).join(name));
            }
        }
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// Decodes a result value into `T`, naming the method on failure.
fn decode<T: DeserializeOwned>(method: &str, value: Value) -> Result<T, ServiceError> {
    serde_json::from_value(value.clone()).map_err(|e| {
        let head: String = value.to_string().chars().take(300).collect();
        ServiceError::Engine(format!("decoding {method} result ({e}): {head}"))
    })
}

/// Maps an RPC error, keeping the permission codes distinguishable for the UI.
fn rpc(method: &str, e: RpcError) -> ServiceError {
    match e.code {
        -32010 => ServiceError::Permission(format!("屏幕录制权限：{}", e.message)),
        -32011 => ServiceError::Permission(format!("日历权限：{}", e.message)),
        -32012 => ServiceError::Permission(format!("麦克风权限：{}", e.message)),
        _ => ServiceError::Engine(format!("{method}: {}", e.message)),
    }
}

/// A notification from `mm-meetingd`, decoded.
#[derive(Debug, Clone, PartialEq)]
pub enum MeetingNotice {
    Partial(CaptionPartial),
    Final(CaptionFinal),
    Relabel(SpeakerRelabel),
    Levels(AudioLevels),
    State(CaptureStateEvent),
    /// `spatial/cue` and anything newer than this build.
    Other { method: String, params: Value },
}

/// Decodes one notification.
///
/// Params: `method`, `params` – as received.
/// Returns: the typed notice, or an error naming the method.
pub fn decode_notice(method: &str, params: Value) -> Result<MeetingNotice, ServiceError> {
    Ok(match method {
        meeting::N_CAPTION_PARTIAL => MeetingNotice::Partial(decode(method, params)?),
        meeting::N_CAPTION_FINAL => MeetingNotice::Final(decode(method, params)?),
        meeting::N_SPEAKER_RELABEL => MeetingNotice::Relabel(decode(method, params)?),
        meeting::N_AUDIO_LEVELS => MeetingNotice::Levels(decode(method, params)?),
        meeting::N_CAPTURE_STATE => MeetingNotice::State(decode(method, params)?),
        _ => MeetingNotice::Other {
            method: method.to_string(),
            params,
        },
    })
}

/// Typed client for `mm-meetingd`. Every call blocks; run it off the UI thread.
#[derive(Clone)]
pub struct MeetingEngine {
    client: EngineClient,
}

impl MeetingEngine {
    /// Starts the engine process.
    ///
    /// Params: `path` – executable.
    /// Returns: the engine and its event stream.
    pub fn spawn(path: &Path) -> Result<(Self, async_channel::Receiver<EngineEvent>), ServiceError> {
        let (client, events) = EngineClient::spawn(path, &[])
            .map_err(|e| ServiceError::Engine(format!("starting {}: {e}", path.display())))?;
        Ok((Self { client }, events))
    }

    /// Wraps an existing client (tests).
    pub fn from_client(client: EngineClient) -> Self {
        Self { client }
    }

    fn call<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T, ServiceError> {
        let v = self.client.call_blocking(method, params).map_err(|e| rpc(method, e))?;
        decode(method, v)
    }

    /// Handshake; returns the engine's capabilities object.
    pub fn initialize(&self) -> Result<Value, ServiceError> {
        self.call(common::INITIALIZE, Value::Null)
    }

    /// Lists audio inputs.
    pub fn probe_inputs(&self) -> Result<Vec<AudioInput>, ServiceError> {
        self.call(meeting::AUDIO_PROBE_INPUTS, Value::Null)
    }

    /// Lists recordable windows and displays (needs Screen Recording permission).
    pub fn list_sources(&self) -> Result<CaptureSources, ServiceError> {
        self.call(meeting::CAPTURE_LIST_SOURCES, Value::Null)
    }

    /// Calendar events in `[from, to)`.
    pub fn calendar_events(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<CalendarEvent>, ServiceError> {
        self.call(
            meeting::CALENDAR_EVENTS,
            json!({"from": from.to_rfc3339(), "to": to.to_rfc3339()}),
        )
    }

    /// Starts recording + live captions.
    pub fn capture_start(&self, preset: &CapturePreset) -> Result<CaptureStarted, ServiceError> {
        self.call(
            meeting::CAPTURE_START,
            serde_json::to_value(preset).expect("preset serializes"),
        )
    }

    /// Stops recording and finalises the files.
    pub fn capture_stop(&self) -> Result<CaptureStopped, ServiceError> {
        self.call(meeting::CAPTURE_STOP, Value::Null)
    }

    /// Speaker embedding from a slice of a recording.
    pub fn voiceprint_from_clip(&self, req: &ClipRequest) -> Result<VoiceprintSample, ServiceError> {
        self.call(
            meeting::VOICEPRINT_FROM_CLIP,
            serde_json::to_value(req).expect("clip request serializes"),
        )
    }

    /// Records `seconds` from the microphone and returns a speaker embedding.
    pub fn voiceprint_enroll(&self, microphone_uid: Option<&str>, seconds: u32) -> Result<VoiceprintSample, ServiceError> {
        let mut params = json!({"seconds": seconds});
        if let Some(uid) = microphone_uid {
            params["microphoneUID"] = json!(uid);
        }
        self.call(meeting::VOICEPRINT_ENROLL, params)
    }

    /// Offline transcription + diarization of one file.
    pub fn transcribe_file(&self, path: &Path, language: Option<&str>) -> Result<FileTranscript, ServiceError> {
        let mut params = json!({"path": path});
        if let Some(l) = language {
            params["language"] = json!(l);
        }
        self.call(meeting::TRANSCRIBE_FILE, params)
    }

    /// Asks the engine to finish recordings and exit.
    pub fn shutdown(&self) {
        if let Err(e) = self.client.call_blocking(common::SHUTDOWN, Value::Null) {
            tracing::warn!("mm-meetingd shutdown: {e}");
        }
    }
}

/// Typed client for `mm-embedd`.
#[derive(Clone)]
pub struct EmbedEngine {
    client: EngineClient,
    modalities: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

/// `model/status` result.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
pub struct EmbedStatus {
    pub loaded: bool,
    #[serde(default)]
    pub directory: Option<String>,
    #[serde(default)]
    pub modalities: Vec<String>,
}

impl EmbedEngine {
    /// Starts the engine process.
    pub fn spawn(path: &Path) -> Result<(Self, async_channel::Receiver<EngineEvent>), ServiceError> {
        let (client, events) = EngineClient::spawn(path, &[])
            .map_err(|e| ServiceError::Engine(format!("starting {}: {e}", path.display())))?;
        Ok((Self::from_client(client), events))
    }

    /// Wraps an existing client (tests).
    pub fn from_client(client: EngineClient) -> Self {
        Self {
            client,
            modalities: Default::default(),
        }
    }

    fn call<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T, ServiceError> {
        let v = self.client.call_blocking(method, params).map_err(|e| rpc(method, e))?;
        decode(method, v)
    }

    /// Loads a checkpoint folder.
    pub fn load(&self, dir: &Path) -> Result<EmbedStatus, ServiceError> {
        let status: EmbedStatus = self.call(embed::MODEL_LOAD, json!({"directory": dir}))?;
        *self.modalities.lock().expect("modalities lock") = normalise_modalities(&status.modalities);
        Ok(status)
    }

    /// Reports what is loaded.
    pub fn status(&self) -> Result<EmbedStatus, ServiceError> {
        let status: EmbedStatus = self.call(embed::MODEL_STATUS, Value::Null)?;
        *self.modalities.lock().expect("modalities lock") = normalise_modalities(&status.modalities);
        Ok(status)
    }

    /// Frees GPU memory.
    pub fn unload(&self) -> Result<(), ServiceError> {
        self.client
            .call_blocking(embed::MODEL_UNLOAD, Value::Null)
            .map(|_| ())
            .map_err(|e| rpc(embed::MODEL_UNLOAD, e))
    }

    /// Embeds a search query (`search_query` task).
    pub fn embed_query(&self, text: &str) -> Result<Vec<f32>, ServiceError> {
        let v: Value = self.call(
            embed::EMBED,
            json!({"items": [{"text": text}], "task": "search_query"}),
        )?;
        let mut vectors: Vec<Vec<f32>> = decode(embed::EMBED, v["vectors"].clone())?;
        vectors
            .pop()
            .ok_or_else(|| ServiceError::Engine("embed/batch returned no vector".into()))
    }

    /// Asks the engine to exit.
    pub fn shutdown(&self) {
        if let Err(e) = self.client.call_blocking(common::SHUTDOWN, Value::Null) {
            tracing::warn!("mm-embedd shutdown: {e}");
        }
    }
}

/// Maps Swift's modality names (`text`, `image`, `audio`, `video` or enum-ish
/// spellings) to the store's modality strings.
fn normalise_modalities(raw: &[String]) -> Vec<String> {
    let mut out: Vec<String> = raw
        .iter()
        .filter_map(|m| {
            let m = m.to_ascii_lowercase();
            ["text", "image", "audio", "video"]
                .into_iter()
                .find(|k| m.contains(k))
                .map(String::from)
        })
        .collect();
    out.sort();
    out.dedup();
    if out.is_empty() {
        out.push("text".into());
    }
    out
}

impl Embedder for EmbedEngine {
    fn embed_documents(&self, items: &[EmbedItem]) -> Result<Vec<Vec<f32>>, String> {
        let v = self
            .client
            .call_blocking(embed::EMBED, json!({"items": items, "task": "document"}))
            .map_err(|e| e.to_string())?;
        serde_json::from_value(v["vectors"].clone()).map_err(|e| format!("decoding vectors: {e}"))
    }

    fn modalities(&self) -> Vec<String> {
        self.modalities.lock().expect("modalities lock").clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notices_decode_by_method() {
        let n = decode_notice(
            "caption/final",
            json!({"sessionId":"s","utteranceId":1,"origin":"microphone","startMs":0,"endMs":900,"text":"好","speaker":{"kind":"unknown"}}),
        )
        .unwrap();
        assert!(matches!(n, MeetingNotice::Final(f) if f.text == "好"));
        let err = decode_notice("caption/partial", json!({"text": 1})).unwrap_err();
        assert!(err.to_string().contains("caption/partial"));
        assert!(matches!(
            decode_notice("spatial/cue", json!({})).unwrap(),
            MeetingNotice::Other { .. }
        ));
    }

    #[test]
    fn permission_errors_are_distinguished() {
        let e = rpc("capture/start", RpcError { code: -32012, message: "denied".into(), data: None });
        assert!(matches!(e, ServiceError::Permission(m) if m.contains("麦克风")));
    }

    #[test]
    fn modality_names_are_normalised() {
        assert_eq!(
            normalise_modalities(&["MultimodalEmbedding.Modality.audio".into(), "text".into()]),
            vec!["audio", "text"]
        );
        assert_eq!(normalise_modalities(&[]), vec!["text"]);
    }
}
