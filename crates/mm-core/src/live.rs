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

/// `spatial/cue`: where one final utterance came from, sent right after its `caption/final`.
///
/// Positions are relative to this meeting (no geometry needed) and mirror-corrected for the
/// screen: `position[0]` is left (−1) to right (1), combining whichever of time and level
/// difference the engine trusts for this microphone; `position[1]`/`position[2]` are those two
/// raw relative axes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpatialCueEvent {
    pub session_id: String,
    pub utterance_id: u64,
    pub origin: AudioOrigin,
    pub start_ms: u64,
    pub end_ms: u64,
    pub position: Vec<f32>,
    /// 0..1 reliability.
    pub confidence: f32,
    /// Degrees from straight ahead, positive to the right; only with a known mic spacing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub azimuth_deg: Option<f32>,
    pub speaker: SpeakerLabel,
}

impl SpatialCueEvent {
    /// The left-right coordinate.
    ///
    /// Returns: `position[0]`, or `None` for an empty position.
    pub fn lateral(&self) -> Option<f32> {
        self.position.first().copied().filter(|x| x.is_finite())
    }
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

/// `permissions/status` result: macOS privacy grants as attributed to the app.
/// Each value is `granted`, `denied`, `notDetermined` or `restricted`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionStatus {
    /// Screen & System Audio Recording (window capture and remote participants' audio).
    pub screen_capture: String,
    pub microphone: String,
    pub calendar: String,
}

impl PermissionStatus {
    /// Permissions not granted yet, as `(kind, Chinese label)` for the settings page.
    ///
    /// Returns: missing permissions in a fixed order.
    pub fn missing(&self) -> Vec<(&'static str, &'static str)> {
        [
            ("screenCapture", "屏幕与系统录音", &self.screen_capture),
            ("microphone", "麦克风", &self.microphone),
            ("calendar", "日历", &self.calendar),
        ]
        .into_iter()
        .filter(|(_, _, v)| v.as_str() != "granted")
        .map(|(k, l, _)| (k, l))
        .collect()
    }
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
    fn spatial_cue_decodes_the_engine_shape() {
        // Same keys as `spatialCueEventUsesCamelCaseAndOmitsUnknownAzimuth` in CaptionPipelineTests.swift.
        let cue: SpatialCueEvent = serde_json::from_str(
            r#"{"sessionId":"s","utteranceId":7,"origin":"microphone","startMs":1200,"endMs":2500,
                "position":[-0.8,-0.75,0.1],"confidence":0.6,"speaker":{"kind":"cluster","index":1}}"#,
        )
        .expect("decode spatial/cue");
        assert_eq!(cue.lateral(), Some(-0.8));
        assert_eq!(cue.azimuth_deg, None);
        assert_eq!(cue.speaker, SpeakerLabel::Cluster { index: 1 });
        let with_azimuth: SpatialCueEvent = serde_json::from_str(
            r#"{"sessionId":"s","utteranceId":7,"origin":"microphone","startMs":0,"endMs":1,
                "position":[],"confidence":1,"azimuthDeg":30,"speaker":{"kind":"unknown"}}"#,
        )
        .expect("decode spatial/cue with azimuth");
        assert_eq!(with_azimuth.azimuth_deg, Some(30.0));
        assert_eq!(with_azimuth.lateral(), None);
    }

    #[test]
    fn permission_status_lists_what_is_missing() {
        let p: PermissionStatus = serde_json::from_str(
            r#"{"screenCapture":"denied","microphone":"granted","calendar":"notDetermined"}"#,
        )
        .expect("decode permissions/status");
        assert_eq!(
            p.missing(),
            vec![("screenCapture", "屏幕与系统录音"), ("calendar", "日历")]
        );
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
