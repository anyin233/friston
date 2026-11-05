//! Media assets and the modalities the indexer understands.

use serde::{Deserialize, Serialize};

use crate::{AssetId, MeetingId};

/// Content modality. EmbeddingGemma 2 maps all of them into one 768-d space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modality {
    Text,
    Image,
    Audio,
    Video,
}

/// A recorded or attached media file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Asset {
    pub id: AssetId,
    pub meeting_id: Option<MeetingId>,
    pub modality: Modality,
    /// Absolute path on disk (recordings live outside the vault; notes embed links).
    pub path: String,
    pub duration_ms: Option<u64>,
    /// Content hash used to detect replaced files without re-reading them.
    pub content_hash: String,
}
