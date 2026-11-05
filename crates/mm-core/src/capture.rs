//! Recording sessions: `capture/start` params and results, `capture/state` and
//! `audio/levels` notifications.
//!
//! The wire shape is owned by `native/NativeKit/Sources/MeetingCore/CapturePlan.swift`;
//! keys are camelCase and acronyms keep Swift's spelling (`windowID`, `microphoneUID`).

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// What one recording session should capture (`capture/start` params).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapturePreset {
    /// Meeting title; names the recording folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// ScreenCaptureKit window ID from `capture/listSources`.
    #[serde(rename = "windowID", default, skip_serializing_if = "Option::is_none")]
    pub window_id: Option<u32>,
    /// Whole display instead of a window.
    #[serde(rename = "displayID", default, skip_serializing_if = "Option::is_none")]
    pub display_id: Option<u32>,
    /// Core Audio UID from `audio/probeInputs`; all channels are kept.
    #[serde(
        rename = "microphoneUID",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub microphone_uid: Option<String>,
    /// Record what the Mac plays (remote participants).
    #[serde(default)]
    pub system_audio: bool,
    /// Caption language hint (`zh`, `en`, …); `None` auto-detects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption_language: Option<String>,
    /// Screen frame rate, 1–60; the engine defaults to 15.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<u32>,
    /// Overrides `~/Movies/Meeting Master`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_directory: Option<PathBuf>,
    /// Enrolled voiceprints so live captions can name known people.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub voiceprints: Vec<crate::PresetVoiceprint>,
    /// Mirror direction cues (the mic's first channel is on the right).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub spatial_flip: bool,
    /// Distance between the two spatial mics in centimetres; lets the engine report
    /// `azimuthDeg`. `None` means unknown geometry (relative positions only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spatial_spacing_cm: Option<f64>,
}

/// Files of one session; one per source so ASR can keep stream origin apart.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureFiles {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub microphone: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_audio: Option<PathBuf>,
}

/// `capture/start` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureStarted {
    pub session_id: String,
    pub directory: PathBuf,
    pub files: CaptureFiles,
    /// Channels kept in `mic.caf`; two or more real channels enable spatial cues.
    #[serde(default)]
    pub microphone_channels: Option<u32>,
    pub started_at: DateTime<Utc>,
}

/// `capture/stop` result; `files` lists only files that received data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureStopped {
    pub session_id: String,
    pub duration_ms: u64,
    pub files: CaptureFiles,
}

/// Capture lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureState {
    Idle,
    Starting,
    Recording,
    Stopping,
    Stopped,
    Failed,
}

/// `capture/state` notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureStateEvent {
    pub session_id: String,
    pub state: CaptureState,
    /// Present when `state` is `Failed`.
    #[serde(default)]
    pub error: Option<String>,
}

/// Stream measured by `audio/levels`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LevelSource {
    Microphone,
    System,
}

/// `audio/levels` notification, at most every 100 ms per source; one value per channel, in dBFS (floor −96).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioLevels {
    pub session_id: String,
    pub source: LevelSource,
    pub rms_db: Vec<f32>,
    pub peak_db: Vec<f32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_matches_the_swift_wire_shape() {
        // Same JSON as `presetDecodesTheWireShape` in CapturePlanTests.swift.
        let json = r#"{"title":"周会","windowID":4242,"microphoneUID":"usb","systemAudio":true,"captionLanguage":"zh","fps":15,"spatialFlip":true,"spatialSpacingCm":14.5}"#;
        let preset: CapturePreset = serde_json::from_str(json).expect("decode preset");
        assert_eq!(preset.window_id, Some(4242));
        assert!(preset.spatial_flip);
        assert_eq!(preset.spatial_spacing_cm, Some(14.5));
        assert_eq!(preset.microphone_uid.as_deref(), Some("usb"));
        assert!(preset.system_audio);
        let back = serde_json::to_value(&preset).expect("encode preset");
        assert_eq!(
            back,
            serde_json::from_str::<serde_json::Value>(json).unwrap()
        );
    }

    #[test]
    fn spatial_preset_fields_are_omitted_when_unset() {
        let v = serde_json::to_value(CapturePreset::default()).expect("encode preset");
        assert_eq!(v, serde_json::json!({"systemAudio": false}));
    }

    #[test]
    fn engine_results_and_events_decode() {
        // Captured from a real mm-meetingd run (paths shortened).
        let started: CaptureStarted = serde_json::from_str(
            r#"{"directory":"/r/2026-11-05 1908 冒烟-测试","files":{"microphone":"/r/2026-11-05 1908 冒烟-测试/mic.caf"},
               "microphoneChannels":1,"sessionId":"b914","startedAt":"2026-11-05T19:08:40Z"}"#,
        )
        .expect("decode capture/start result");
        assert_eq!(started.microphone_channels, Some(1));
        assert!(started.files.screen.is_none());

        let state: CaptureStateEvent =
            serde_json::from_str(r#"{"sessionId":"b914","state":"failed","error":"permission"}"#)
                .expect("decode capture/state");
        assert_eq!(state.state, CaptureState::Failed);

        let levels: AudioLevels = serde_json::from_str(
            r#"{"peakDb":[-44.5],"rmsDb":[-60.0],"sessionId":"b914","source":"microphone"}"#,
        )
        .expect("decode audio/levels");
        assert_eq!(levels.source, LevelSource::Microphone);
    }
}
