//! Meetings and their lifecycle.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{MeetingId, PersonId};

/// Where a meeting happens; drives which capture sources are proposed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MeetingVenue {
    /// Everyone is in the room; only the microphone (array) is relevant.
    InPerson { room: Option<String> },
    /// Remote call; `app_bundle_id` lets the engine capture that app's window/audio only.
    Online {
        join_url: Option<String>,
        app_bundle_id: Option<String>,
    },
    /// Some people in the room, some remote.
    Hybrid {
        room: Option<String>,
        join_url: Option<String>,
        app_bundle_id: Option<String>,
    },
}

/// Lifecycle state of a meeting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeetingStatus {
    /// On the calendar, not started yet.
    Scheduled,
    /// Recording / live captioning in progress.
    Live,
    /// Recording stopped; final transcription, diarization and minutes are running.
    Processing,
    /// Minutes generated; notes may still be edited (indexing stays automatic).
    Done,
    /// Cancelled or skipped.
    Cancelled,
}

impl MeetingStatus {
    /// Tells whether a transition from `self` to `next` is allowed.
    ///
    /// Params: `next` – the requested status.
    /// Returns: `true` when the state machine permits the transition.
    pub fn can_transition_to(self, next: MeetingStatus) -> bool {
        use MeetingStatus::*;
        matches!(
            (self, next),
            (Scheduled, Live)
                | (Scheduled, Cancelled)
                | (Live, Processing)
                // Re-running minutes generation after edits is legitimate.
                | (Processing, Done)
                | (Done, Processing)
        )
    }
}

/// A meeting as shown in the schedule and the library.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meeting {
    pub id: MeetingId,
    pub title: String,
    pub status: MeetingStatus,
    pub venue: MeetingVenue,
    pub starts_at: DateTime<Utc>,
    pub ends_at: Option<DateTime<Utc>>,
    /// Identifier of the source calendar event (EventKit `calendarItemIdentifier`).
    pub calendar_event_id: Option<String>,
    pub participants: Vec<PersonId>,
    /// Vault-relative path of the Obsidian note that holds notes + minutes.
    pub note_path: Option<String>,
    pub tags: Vec<String>,
    /// Folder holding `screen.mov` / `mic.caf` / `system.caf` once recorded.
    #[serde(default)]
    pub recording_dir: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::MeetingStatus::*;

    #[test]
    fn lifecycle_allows_normal_flow() {
        assert!(Scheduled.can_transition_to(Live));
        assert!(Live.can_transition_to(Processing));
        assert!(Processing.can_transition_to(Done));
        assert!(Done.can_transition_to(Processing));
    }

    #[test]
    fn lifecycle_rejects_skipping_recording() {
        assert!(!Scheduled.can_transition_to(Done));
        assert!(!Cancelled.can_transition_to(Live));
        assert!(!Done.can_transition_to(Live));
    }
}
