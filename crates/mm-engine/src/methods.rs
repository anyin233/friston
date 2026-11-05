//! Method and notification names shared with the Swift engines.
//!
//! Keep in sync with `native/EngineKit/Sources/EngineKit/Methods.swift`
//! and `docs/engine-protocol.md`.

/// Methods served by every engine.
pub mod common {
    /// Handshake; returns engine name, version and capabilities.
    pub const INITIALIZE: &str = "initialize";
    /// Graceful shutdown; the process exits after replying.
    pub const SHUTDOWN: &str = "shutdown";
}

/// `mm-meetingd`: capture, speech, speakers, calendar.
pub mod meeting {
    pub const CAPTURE_LIST_SOURCES: &str = "capture/listSources";
    pub const CAPTURE_START: &str = "capture/start";
    pub const CAPTURE_STOP: &str = "capture/stop";
    pub const AUDIO_PROBE_INPUTS: &str = "audio/probeInputs";
    pub const VOICEPRINT_ENROLL: &str = "voiceprint/enroll";
    pub const VOICEPRINT_FROM_CLIP: &str = "voiceprint/fromClip";
    pub const TRANSCRIBE_FILE: &str = "transcribe/file";
    pub const CALENDAR_EVENTS: &str = "calendar/events";

    // Notifications (engine → app)
    pub const N_CAPTION_PARTIAL: &str = "caption/partial";
    pub const N_CAPTION_FINAL: &str = "caption/final";
    pub const N_SPEAKER_RELABEL: &str = "speaker/relabel";
    pub const N_SPATIAL_CUE: &str = "spatial/cue";
    pub const N_AUDIO_LEVELS: &str = "audio/levels";
    pub const N_CAPTURE_STATE: &str = "capture/state";
}

/// `mm-embedd`: EmbeddingGemma 2.
pub mod embed {
    pub const EMBED: &str = "embed/batch";
    pub const MODEL_STATUS: &str = "model/status";
    pub const MODEL_LOAD: &str = "model/load";
    pub const MODEL_UNLOAD: &str = "model/unload";
    /// Local language model for offline minutes (same MLX process as the embedder).
    pub const LLM_LOAD: &str = "llm/load";
    pub const LLM_UNLOAD: &str = "llm/unload";
    pub const LLM_STATUS: &str = "llm/status";
    pub const LLM_GENERATE: &str = "llm/generate";
}
