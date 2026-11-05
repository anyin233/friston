//! Transcript segments and live caption events.

use serde::{Deserialize, Serialize};

use crate::{AudioOrigin, MeetingId, PersonId, SpatialCue};

/// Speaker attribution for a segment, from most to least certain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SpeakerLabel {
    /// Matched to an enrolled or confirmed voiceprint.
    Known { person_id: PersonId, score: f32 },
    /// Stable anonymous cluster within this meeting ("Speaker 2"); can be named later.
    Cluster { index: u32 },
    /// Not enough evidence yet (very short utterance, overlap).
    Unknown,
}

/// A finalized transcript segment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptSegment {
    pub meeting_id: MeetingId,
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
    pub language: Option<String>,
    pub speaker: SpeakerLabel,
    pub origin: AudioOrigin,
    pub spatial: Option<SpatialCue>,
}

/// Streaming caption event pushed by the speech engine for the live caption bar.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CaptionEvent {
    /// Volatile hypothesis; replaces the previous partial with the same `utterance_id`.
    Partial {
        utterance_id: u64,
        text: String,
        speaker: SpeakerLabel,
    },
    /// Stable text; the partial with this `utterance_id` is retired.
    Final {
        utterance_id: u64,
        segment: TranscriptSegment,
    },
    /// Speaker attribution changed retroactively (e.g. a cluster got matched to a person).
    Relabel {
        from: SpeakerLabel,
        to: SpeakerLabel,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caption_event_uses_tagged_json() {
        let ev = CaptionEvent::Partial {
            utterance_id: 7,
            text: "大家好".into(),
            speaker: SpeakerLabel::Cluster { index: 1 },
        };
        let json = serde_json::to_value(&ev).expect("serialize caption event");
        assert_eq!(json["type"], "partial");
        assert_eq!(json["speaker"]["kind"], "cluster");
        let back: CaptionEvent = serde_json::from_value(json).expect("round-trip caption event");
        assert_eq!(back, ev);
    }
}
