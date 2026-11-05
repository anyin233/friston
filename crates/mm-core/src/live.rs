//! Wire types for `mm-meetingd` results and live notifications other than capture.
//!
//! Keys are camelCase like the Swift side; the nested [`SpeakerLabel`] and
//! [`SpatialCue`] objects keep their Rust (snake_case) shape because the engine
//! mirrors them verbatim. See `docs/engine-protocol.md`.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{AudioOrigin, SpatialCue, SpeakerLabel};

/// `caption/partial`: a volatile hypothesis that replaces the previous one with the same id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptionPartial {
    pub session_id: String,
    pub utterance_id: u64,
    pub origin: AudioOrigin,
    /// Milliseconds since capture start.
    pub start_ms: u64,
    pub text: String,
    pub speaker: SpeakerLabel,
}

/// `caption/final`: stable text; retires the partial with the same `utterance_id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptionFinal {
    pub session_id: String,
    pub utterance_id: u64,
    pub origin: AudioOrigin,
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
    #[serde(default)]
    pub language: Option<String>,
    pub speaker: SpeakerLabel,
    #[serde(default)]
    pub spatial: Option<SpatialCue>,
}

/// `speaker/relabel`: every caption labelled `from` is now `to`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeakerRelabel {
    pub session_id: String,
    pub from: SpeakerLabel,
    pub to: SpeakerLabel,
}

/// A known voiceprint handed to the engine at `capture/start` so captions can name people.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PresetVoiceprint {
    pub person_id: String,
    /// Speaker-embedding model id; vectors of other models are ignored by the engine.
    pub model: String,
    pub vector: Vec<f32>,
}

/// `calendar/events` item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CalendarEvent {
    /// EventKit `calendarItemIdentifier`.
    pub id: String,
    pub title: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    #[serde(default)]
    pub calendar: String,
    #[serde(default)]
    pub location: Option<String>,
    /// First conferencing link found (Zoom, Teams, Meet, 腾讯会议, 飞书 …).
    #[serde(rename = "joinURL", default)]
    pub join_url: Option<String>,
    #[serde(default)]
    pub attendees: Vec<String>,
}

/// `audio/probeInputs` item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioInput {
    pub uid: String,
    pub name: String,
    pub channels: u32,
    pub sample_rate: f64,
    pub is_virtual: bool,
}

impl AudioInput {
    /// Whether direction-of-arrival cues are possible (≥ 2 real microphone channels).
    ///
    /// Returns: `true` for multichannel physical devices.
    pub fn supports_spatial_cues(&self) -> bool {
        self.channels >= 2 && !self.is_virtual
    }
}

/// `capture/listSources` window entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureWindow {
    #[serde(rename = "windowID")]
    pub window_id: u32,
    pub title: String,
    pub app_name: String,
    #[serde(rename = "bundleID")]
    pub bundle_id: String,
    pub width: f64,
    pub height: f64,
}

/// `capture/listSources` result.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CaptureSources {
    pub windows: Vec<CaptureWindow>,
    pub displays: Vec<u32>,
}

/// Quality indicators returned with a voiceprint.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceprintQuality {
    #[serde(default)]
    pub single_speaker: bool,
    #[serde(default)]
    pub snr_db: Option<f32>,
}

/// `voiceprint/enroll` and `voiceprint/fromClip` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceprintSample {
    pub model: String,
    pub vector: Vec<f32>,
    pub speech_ms: u64,
    #[serde(default)]
    pub quality: VoiceprintQuality,
}

/// `voiceprint/fromClip` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipRequest {
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_ms: Option<u64>,
}

/// One segment of `transcribe/file`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSegment {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
    pub speaker: SpeakerLabel,
    #[serde(default)]
    pub origin: Option<AudioOrigin>,
}

/// `transcribe/file` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileTranscript {
    #[serde(default)]
    pub language: Option<String>,
    pub segments: Vec<FileSegment>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caption_notifications_decode_the_engine_shape() {
        let partial: CaptionPartial = serde_json::from_str(
            r#"{"sessionId":"s","utteranceId":3,"origin":"microphone","startMs":1200,"text":"大家","speaker":{"kind":"cluster","index":1}}"#,
        )
        .expect("decode caption/partial");
        assert_eq!(partial.speaker, SpeakerLabel::Cluster { index: 1 });

        let fin: CaptionFinal = serde_json::from_str(
            r#"{"sessionId":"s","utteranceId":3,"origin":"system","startMs":1200,"endMs":2400,"text":"大家好","language":"zh",
                "speaker":{"kind":"unknown"},"spatial":{"azimuth_deg":null,"tdoa_samples":1.5,"confidence":0.7}}"#,
        )
        .expect("decode caption/final");
        assert_eq!(fin.origin, AudioOrigin::System);
        assert_eq!(fin.spatial.map(|s| s.tdoa_samples), Some(1.5));

        let relabel: SpeakerRelabel = serde_json::from_str(
            r#"{"sessionId":"s","from":{"kind":"cluster","index":2},"to":{"kind":"cluster","index":1}}"#,
        )
        .expect("decode speaker/relabel");
        assert_eq!(relabel.to, SpeakerLabel::Cluster { index: 1 });
    }

    #[test]
    fn calendar_and_sources_decode_swift_keys() {
        let ev: CalendarEvent = serde_json::from_str(
            r#"{"id":"e1","title":"周会","start":"2026-11-05T06:00:00Z","end":"2026-11-05T07:00:00Z","calendar":"工作",
                "joinURL":"https://meeting.tencent.com/x","attendees":["Alice"]}"#,
        )
        .expect("decode calendar event");
        assert_eq!(
            ev.join_url.as_deref(),
            Some("https://meeting.tencent.com/x")
        );

        let src: CaptureSources = serde_json::from_str(
            r#"{"windows":[{"windowID":7,"title":"Zoom","appName":"zoom.us","bundleID":"us.zoom.xos","width":800,"height":600}],"displays":[1]}"#,
        )
        .expect("decode capture sources");
        assert_eq!(src.windows[0].window_id, 7);

        let input: AudioInput = serde_json::from_str(
            r#"{"uid":"u","name":"WeMeet","channels":2,"sampleRate":48000,"isVirtual":true}"#,
        )
        .expect("decode audio input");
        assert!(
            !input.supports_spatial_cues(),
            "virtual devices are never spatial"
        );
    }
}
