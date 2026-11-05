//! Strongly typed identifiers.
//!
//! UUIDv7 is used everywhere because it is time-ordered, which keeps SQLite
//! B-tree inserts append-only and makes IDs sortable by creation time.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! define_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            /// Creates a new time-ordered identifier.
            ///
            /// Returns: a fresh UUIDv7-backed identifier.
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

define_id!(
    /// Identifies a meeting (scheduled, live or finished).
    MeetingId
);
define_id!(
    /// Identifies a person known to the workbench (participant or enrolled voice).
    PersonId
);
define_id!(
    /// Identifies a stored voiceprint sample belonging to a person.
    VoiceprintId
);
define_id!(
    /// Identifies a media asset (recording, screenshot, attachment).
    AssetId
);
define_id!(
    /// Identifies an indexable chunk (text span, audio window, video window, image).
    ChunkId
);
