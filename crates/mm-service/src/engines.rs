//! Locating, starting and calling the two Swift engines with typed results.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use mm_core::{
    AudioInput, AudioLevels, CalendarEvent, CaptionFinal, CaptionPartial, CapturePreset,
    CaptureSources, CaptureStarted, CaptureStateEvent, CaptureStopped, ClipRequest, FileTranscript,
    SpatialCueEvent, SpeakerRelabel, VoiceprintSample,
};
use mm_engine::methods::{common, embed, meeting};
use mm_engine::{EngineClient, EngineEvent, RpcError};
use mm_i18n::trf;
use mm_index::pipeline::{EmbedItem, Embedder};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::ServiceError;

/// Executable names of the helpers.
pub const MEETINGD: &str = "mm-meetingd";
pub const EMBEDD: &str = "mm-embedd";

/// Finds a helper executable.
///
/// Each engine lives in its own folder (`<name>/<name>` plus its resource bundles and
/// `mlx.metallib`) because the two engines ship different MLX builds. Search order:
/// `MM_ENGINES_DIR`; the app bundle's `Contents/Resources/engines`; next to the running executable;
/// then `target/engines/{Debug,Release}` in any ancestor folder (development runs from
/// `target/debug`).
///
/// Params: `name` – executable name.
/// Returns: the path, or `None` when not built yet.
pub fn locate_engine(name: &str) -> Option<PathBuf> {
    engine_candidates(
        name,
        std::env::var_os("MM_ENGINES_DIR").map(PathBuf::from),
        std::env::current_exe().ok(),
    )
    .into_iter()
    .find(|p| p.is_file())
}

/// Candidate paths for [`locate_engine`], in priority order (pure, for tests).
fn engine_candidates(name: &str, env_dir: Option<PathBuf>, exe: Option<PathBuf>) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(dir) = env_dir {
        candidates.push(dir.join(name).join(name));
        candidates.push(dir.join(name));
    }
    if let Some(exe) = exe {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("../Resources/engines").join(name).join(name));
            candidates.push(dir.join(name).join(name));
        }
        for ancestor in exe.ancestors().skip(1) {
            for config in ["Debug", "Release"] {
                candidates.push(
                    ancestor
                        .join("target/engines")
                        .join(config)
                        .join(name)
                        .join(name),
                );
                candidates.push(ancestor.join("engines").join(config).join(name).join(name));
            }
        }
    }
    candidates
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
        -32010 => ServiceError::Permission(trf!(
            "屏幕录制权限：{}",
            "Screen recording permission: {}",
            e.message
        )),
        -32011 => {
            ServiceError::Permission(trf!("日历权限：{}", "Calendar permission: {}", e.message))
        }
        -32012 => ServiceError::Permission(trf!(
            "麦克风权限：{}",
            "Microphone permission: {}",
            e.message
        )),
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
    Spatial(SpatialCueEvent),
    /// Anything newer than this build.
    Other {
        method: String,
        params: Value,
    },
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
        meeting::N_SPATIAL_CUE => MeetingNotice::Spatial(decode(method, params)?),
        _ => MeetingNotice::Other {
            method: method.to_string(),
            params,
        },
    })
}

/// What the post-meeting pass asks of `transcribe/file`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RefineRequest {
    /// Language hint (`zh`, `en`, …); `None` auto-detects.
    pub language: Option<String>,
    /// Enrolled voiceprints, so known people come back as `known`.
    pub voiceprints: Vec<mm_core::PresetVoiceprint>,
    /// Qwen3-ASR checkpoint (repo id or absolute folder); `None` = engine default.
    pub model: Option<String>,
    /// Decoder hint: meeting title, names, terms.
    pub context: Option<String>,
}

/// Builds `transcribe/file` params, leaving out what the request does not set.
///
/// Params: `path` – audio file; `req` – the request.
/// Returns: the JSON params.
pub fn refine_params(path: &Path, req: &RefineRequest) -> Value {
    let mut params = json!({"path": path, "voiceprints": req.voiceprints});
    for (key, value) in [
        ("language", &req.language),
        ("model", &req.model),
        ("context", &req.context),
    ] {
        if let Some(v) = value.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
            params[key] = json!(v);
        }
    }
    params
}

/// `models/preload` result: the live caption models now resident in `mm-meetingd`.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
pub struct SpeechModelsStatus {
    /// Role (`liveASR`, `vad`, `speakerEmbedding`, `liveDiarization`, `draft`) → model id.
    pub models: std::collections::BTreeMap<String, String>,
    /// Optional roles that failed to load; captions still run without them.
    #[serde(default)]
    pub unavailable: Vec<String>,
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
    pub fn spawn(
        path: &Path,
    ) -> Result<(Self, async_channel::Receiver<EngineEvent>), ServiceError> {
        let (client, events) = EngineClient::spawn(path, &[])
            .map_err(|e| ServiceError::Engine(format!("starting {}: {e}", path.display())))?;
        Ok((Self { client }, events))
    }

    /// Wraps an existing client (tests).
    pub fn from_client(client: EngineClient) -> Self {
        Self { client }
    }

    fn call<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T, ServiceError> {
        let v = self
            .client
            .call_blocking(method, params)
            .map_err(|e| rpc(method, e))?;
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
    pub fn calendar_events(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<CalendarEvent>, ServiceError> {
        self.call(
            meeting::CALENDAR_EVENTS,
            json!({"from": from.to_rfc3339(), "to": to.to_rfc3339()}),
        )
    }

    /// macOS privacy grants as attributed to the app (no prompts).
    pub fn permissions(&self) -> Result<mm_core::PermissionStatus, ServiceError> {
        self.call(meeting::PERMISSIONS_STATUS, Value::Null)
    }

    /// Asks macOS for one permission (`screenCapture`, `microphone`, `calendar`).
    ///
    /// Params: `kind` – permission. Returns: the status afterwards.
    pub fn request_permission(
        &self,
        kind: &str,
    ) -> Result<mm_core::PermissionStatus, ServiceError> {
        self.call(meeting::PERMISSIONS_REQUEST, json!({"kind": kind}))
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
    pub fn voiceprint_from_clip(
        &self,
        req: &ClipRequest,
    ) -> Result<VoiceprintSample, ServiceError> {
        self.call(
            meeting::VOICEPRINT_FROM_CLIP,
            serde_json::to_value(req).expect("clip request serializes"),
        )
    }

    /// Records `seconds` from the microphone and returns a speaker embedding.
    pub fn voiceprint_enroll(
        &self,
        microphone_uid: Option<&str>,
        seconds: u32,
    ) -> Result<VoiceprintSample, ServiceError> {
        let mut params = json!({"seconds": seconds});
        if let Some(uid) = microphone_uid {
            params["microphoneUID"] = json!(uid);
        }
        self.call(meeting::VOICEPRINT_ENROLL, params)
    }

    /// Offline transcription + diarization of one file.
    pub fn transcribe_file(
        &self,
        path: &Path,
        language: Option<&str>,
    ) -> Result<FileTranscript, ServiceError> {
        let mut params = json!({"path": path});
        if let Some(l) = language {
            params["language"] = json!(l);
        }
        self.call(meeting::TRANSCRIBE_FILE, params)
    }

    /// Offline transcription that also names enrolled speakers (`known` labels).
    ///
    /// Params: `path` – audio file; `language` – hint; `voiceprints` – enrolled samples.
    /// Returns: the transcript.
    pub fn transcribe_file_with_voiceprints(
        &self,
        path: &Path,
        language: Option<&str>,
        voiceprints: &[mm_core::PresetVoiceprint],
    ) -> Result<FileTranscript, ServiceError> {
        let mut params = json!({"path": path, "voiceprints": voiceprints});
        if let Some(l) = language {
            params["language"] = json!(l);
        }
        self.call(meeting::TRANSCRIBE_FILE, params)
    }

    /// Post-meeting transcription with a chosen model and a decoder hint.
    ///
    /// Params: `path` – audio file; `req` – language, voiceprints, model and context.
    /// Returns: the transcript, whose `model` names the checkpoint actually used.
    pub fn refine_file(
        &self,
        path: &Path,
        req: &RefineRequest,
    ) -> Result<FileTranscript, ServiceError> {
        self.call(meeting::TRANSCRIBE_FILE, refine_params(path, req))
    }

    /// Loads the live caption models ahead of the first recording (`models/preload`).
    ///
    /// Params: `language` – caption language hint; the Mandarin draft model loads only for `zh`.
    /// Returns: the loaded model ids and the optional roles that failed. Blocks until loaded,
    /// which takes minutes on a first run that downloads the models.
    pub fn preload_models(
        &self,
        language: Option<&str>,
    ) -> Result<SpeechModelsStatus, ServiceError> {
        let params = match language {
            Some(l) => json!({"language": l}),
            None => json!({}),
        };
        self.call(meeting::MODELS_PRELOAD, params)
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
    pub fn spawn(
        path: &Path,
    ) -> Result<(Self, async_channel::Receiver<EngineEvent>), ServiceError> {
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
        let v = self
            .client
            .call_blocking(method, params)
            .map_err(|e| rpc(method, e))?;
        decode(method, v)
    }

    /// Loads a checkpoint folder.
    pub fn load(&self, dir: &Path) -> Result<EmbedStatus, ServiceError> {
        let status: EmbedStatus = self.call(embed::MODEL_LOAD, json!({"directory": dir}))?;
        *self.modalities.lock().expect("modalities lock") =
            normalise_modalities(&status.modalities);
        Ok(status)
    }

    /// Reports what is loaded.
    pub fn status(&self) -> Result<EmbedStatus, ServiceError> {
        let status: EmbedStatus = self.call(embed::MODEL_STATUS, Value::Null)?;
        *self.modalities.lock().expect("modalities lock") =
            normalise_modalities(&status.modalities);
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

    /// Loads a local MLX language model for offline minutes.
    pub fn llm_load(&self, dir: &Path) -> Result<LlmStatus, ServiceError> {
        self.call(embed::LLM_LOAD, json!({"directory": dir}))
    }

    /// Reports whether a language model is loaded.
    pub fn llm_status(&self) -> Result<LlmStatus, ServiceError> {
        self.call(embed::LLM_STATUS, Value::Null)
    }

    /// Frees the language model.
    pub fn llm_unload(&self) -> Result<(), ServiceError> {
        self.client
            .call_blocking(embed::LLM_UNLOAD, Value::Null)
            .map(|_| ())
            .map_err(|e| rpc(embed::LLM_UNLOAD, e))
    }

    /// Generates minutes on-device, constrained to the minutes JSON Schema.
    ///
    /// Params: `input` – meeting material.
    /// Returns: parsed minutes.
    pub fn local_minutes(
        &self,
        input: &mm_llm::MinutesInput,
    ) -> Result<mm_llm::Minutes, mm_llm::LlmError> {
        let v = self
            .client
            .call_blocking(
                embed::LLM_GENERATE,
                json!({
                    "system": mm_llm::instructions(input.language.as_deref()),
                    "prompt": input.render(),
                    "jsonSchema": mm_llm::minutes_json_schema(),
                    "maxTokens": 4096
                }),
            )
            .map_err(|e| mm_llm::LlmError::Local(format!("llm/generate: {}", e.message)))?;
        let text = v["text"].as_str().ok_or_else(|| {
            mm_llm::LlmError::Local(format!("llm/generate returned no text: {v}"))
        })?;
        mm_llm::byok::parse_minutes(text)
    }

    /// Asks the engine to exit.
    pub fn shutdown(&self) {
        if let Err(e) = self.client.call_blocking(common::SHUTDOWN, Value::Null) {
            tracing::warn!("mm-embedd shutdown: {e}");
        }
    }
}

/// `llm/status` result.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
pub struct LlmStatus {
    pub loaded: bool,
    #[serde(default)]
    pub directory: Option<String>,
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
    fn preload_result_decodes() {
        let s: SpeechModelsStatus = decode(
            "models/preload",
            json!({"models":{"liveASR":"mlx-community/Qwen3-ASR-1.7B-4bit","vad":"v"},"unavailable":["draft"]}),
        )
        .unwrap();
        assert_eq!(s.models["liveASR"], "mlx-community/Qwen3-ASR-1.7B-4bit");
        assert_eq!(s.unavailable, vec!["draft".to_string()]);
        let bare: SpeechModelsStatus =
            decode("models/preload", json!({"models":{"liveASR":"a"}})).unwrap();
        assert!(
            bare.unavailable.is_empty(),
            "older engines omit `unavailable`"
        );
    }

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
        let cue = decode_notice(
            "spatial/cue",
            json!({"sessionId":"s","utteranceId":1,"origin":"microphone","startMs":0,"endMs":900,
                   "position":[0.5,0.4,0.1],"confidence":0.7,"speaker":{"kind":"cluster","index":0}}),
        )
        .unwrap();
        assert!(matches!(cue, MeetingNotice::Spatial(c) if c.lateral() == Some(0.5)));
        assert!(
            decode_notice("spatial/cue", json!({}))
                .unwrap_err()
                .to_string()
                .contains("spatial/cue")
        );
        assert!(matches!(
            decode_notice("spatial/summary", json!({})).unwrap(),
            MeetingNotice::Other { .. }
        ));
    }

    #[test]
    fn refine_params_send_only_what_is_set() {
        let bare = refine_params(Path::new("/r/mic.caf"), &RefineRequest::default());
        assert_eq!(bare, json!({"path": "/r/mic.caf", "voiceprints": []}));
        let full = refine_params(
            Path::new("/r/mic.caf"),
            &RefineRequest {
                language: Some("zh".into()),
                voiceprints: vec![],
                model: Some(" mlx-community/Qwen3-ASR-1.7B-8bit ".into()),
                context: Some("会议：周会".into()),
            },
        );
        assert_eq!(full["language"], "zh");
        assert_eq!(full["model"], "mlx-community/Qwen3-ASR-1.7B-8bit");
        assert_eq!(full["context"], "会议：周会");
        let blank = refine_params(
            Path::new("/r/mic.caf"),
            &RefineRequest {
                model: Some("  ".into()),
                ..Default::default()
            },
        );
        assert!(
            blank.get("model").is_none(),
            "a blank setting means the default"
        );
    }

    #[test]
    fn permission_errors_are_distinguished() {
        let e = rpc(
            "capture/start",
            RpcError {
                code: -32012,
                message: "denied".into(),
                data: None,
            },
        );
        assert!(matches!(e, ServiceError::Permission(m) if m.contains("麦克风")));
        let en = mm_i18n::with_lang(mm_i18n::Lang::En, || {
            rpc(
                "capture/start",
                RpcError {
                    code: -32012,
                    message: "denied".into(),
                    data: None,
                },
            )
        });
        assert!(matches!(en, ServiceError::Permission(m) if m == "Microphone permission: denied"));
    }

    #[test]
    fn engine_candidates_prefer_env_then_app_helpers_then_build_tree() {
        let c = engine_candidates(
            "mm-embedd",
            Some("/e".into()),
            Some("/A.app/Contents/MacOS/meeting-master".into()),
        );
        assert_eq!(c[0], PathBuf::from("/e/mm-embedd/mm-embedd"));
        assert_eq!(
            c[2],
            PathBuf::from("/A.app/Contents/MacOS/../Resources/engines/mm-embedd/mm-embedd")
        );
        let dev = engine_candidates(
            "mm-meetingd",
            None,
            Some("/repo/target/debug/meeting-master".into()),
        );
        assert!(dev.contains(&PathBuf::from(
            "/repo/target/engines/Debug/mm-meetingd/mm-meetingd"
        )));
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
