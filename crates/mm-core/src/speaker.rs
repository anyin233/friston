//! People, voiceprints and spatial speaker cues.

use serde::{Deserialize, Serialize};

use crate::{PersonId, VoiceprintId};

/// A person the workbench can name in transcripts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Person {
    pub id: PersonId,
    pub display_name: String,
    pub email: Option<String>,
    /// Obsidian note for this person (backlinks from meeting notes), vault-relative.
    pub note_path: Option<String>,
}

/// How a voiceprint sample was obtained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VoiceprintSource {
    /// Recorded on purpose in the enrollment dialog.
    Enrollment,
    /// Trimmed by the user from an existing recording.
    Clip {
        asset_path: String,
        start_ms: u64,
        end_ms: u64,
    },
    /// Confirmed by the user when relabelling a diarized speaker after a meeting.
    Confirmed { meeting_note: Option<String> },
}

/// One speaker-embedding sample. A person usually owns several (different mics/rooms).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Voiceprint {
    pub id: VoiceprintId,
    pub person_id: PersonId,
    /// Speaker-embedding model identifier, e.g. `wespeaker-resnet34`; vectors from
    /// different models are never compared.
    pub model: String,
    pub vector: Vec<f32>,
    pub source: VoiceprintSource,
    /// Net speech duration that produced the vector; short samples get lower weight.
    pub speech_ms: u64,
}

/// Which physical stream a piece of speech came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioOrigin {
    /// Local microphone: people in the room (spatial cues may be available).
    Microphone,
    /// System / app audio: remote participants (no spatial cues, but never a local person).
    System,
}

/// Direction-of-arrival estimate for a speech frame, relative to the mic array.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SpatialCue {
    /// Azimuth in degrees (0 = array front, positive = clockwise), when geometry is known.
    pub azimuth_deg: Option<f32>,
    /// Raw inter-channel delay in samples; usable for clustering even without geometry.
    pub tdoa_samples: f32,
    /// 0..1 confidence (GCC-PHAT peak sharpness).
    pub confidence: f32,
}
