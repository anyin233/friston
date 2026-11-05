//! Meetings, people, voiceprints and transcript segments.

use chrono::{DateTime, Utc};
use mm_core::{
    AudioOrigin, Meeting, MeetingId, MeetingStatus, MeetingVenue, Person, PersonId, SpatialCue,
    SpeakerLabel, TranscriptSegment, Voiceprint, VoiceprintId, VoiceprintSource,
};
use rusqlite::{OptionalExtension, Row, params};

use crate::{Store, StoreError, f32_bytes, f32_from_bytes};

/// Which transcription pass produced a segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentPass {
    /// Streamed during the meeting.
    Live,
    /// Post-meeting re-transcription; replaces the live pass when present.
    Final,
}

impl SegmentPass {
    fn as_str(self) -> &'static str {
        match self {
            SegmentPass::Live => "live",
            SegmentPass::Final => "final",
        }
    }
}

/// Encodes a serde value as a JSON string column.
fn to_json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("domain types always serialize")
}

/// Decodes a JSON column, naming the column on failure.
fn from_json<T: serde::de::DeserializeOwned>(raw: &str, column: &str) -> rusqlite::Result<T> {
    serde_json::from_str(raw).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("decoding {column} {raw:?}: {e}").into(),
        )
    })
}

/// Parses an RFC 3339 timestamp column.
fn parse_time(raw: &str, column: &str) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                format!("parsing {column} {raw:?}: {e}").into(),
            )
        })
}

fn status_str(s: MeetingStatus) -> String {
    to_json(&s).trim_matches('"').to_string()
}

const MEETING_COLUMNS: &str = "id, title, status, venue_json, starts_at, ends_at, calendar_event, note_path, participants_json, tags_json, recording_dir";

/// Maps a `meetings` row selected with [`MEETING_COLUMNS`].
fn meeting_from_row(r: &Row<'_>) -> rusqlite::Result<Meeting> {
    let id: String = r.get(0)?;
    let status: String = r.get(2)?;
    let venue: String = r.get(3)?;
    let starts: String = r.get(4)?;
    let ends: Option<String> = r.get(5)?;
    let participants: String = r.get(8)?;
    let tags: String = r.get(9)?;
    Ok(Meeting {
        id: MeetingId(from_json(&format!("\"{id}\""), "meetings.id")?),
        title: r.get(1)?,
        status: from_json(&format!("\"{status}\""), "meetings.status")?,
        venue: from_json(&venue, "meetings.venue_json")?,
        starts_at: parse_time(&starts, "meetings.starts_at")?,
        ends_at: ends
            .map(|e| parse_time(&e, "meetings.ends_at"))
            .transpose()?,
        calendar_event_id: r.get(6)?,
        note_path: r.get(7)?,
        participants: from_json(&participants, "meetings.participants_json")?,
        tags: from_json(&tags, "meetings.tags_json")?,
        recording_dir: r.get(10)?,
    })
}

impl Store {
    /// Inserts or fully replaces a meeting row (minutes are kept).
    ///
    /// Params: `m` – the meeting.
    /// Returns: `Ok(())` once written.
    pub fn upsert_meeting(&self, m: &Meeting) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO meetings(id, title, status, venue_json, starts_at, ends_at, calendar_event, note_path,
                                  participants_json, tags_json, recording_dir)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(id) DO UPDATE SET title=excluded.title, status=excluded.status,
                venue_json=excluded.venue_json, starts_at=excluded.starts_at, ends_at=excluded.ends_at,
                calendar_event=excluded.calendar_event, note_path=excluded.note_path,
                participants_json=excluded.participants_json, tags_json=excluded.tags_json,
                recording_dir=excluded.recording_dir",
            params![
                m.id.to_string(),
                m.title,
                status_str(m.status),
                to_json(&m.venue),
                m.starts_at.to_rfc3339(),
                m.ends_at.map(|t| t.to_rfc3339()),
                m.calendar_event_id,
                m.note_path,
                to_json(&m.participants),
                to_json(&m.tags),
                m.recording_dir,
            ],
        )?;
        Ok(())
    }

    /// Loads one meeting.
    ///
    /// Params: `id` – meeting id.
    /// Returns: the meeting, or `None` if unknown.
    pub fn meeting(&self, id: MeetingId) -> Result<Option<Meeting>, StoreError> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {MEETING_COLUMNS} FROM meetings WHERE id = ?1"),
                [id.to_string()],
                meeting_from_row,
            )
            .optional()?)
    }

    /// Finds the meeting created for a calendar event.
    ///
    /// Params: `event_id` – EventKit identifier.
    /// Returns: the meeting, if one was created already.
    pub fn meeting_for_event(&self, event_id: &str) -> Result<Option<Meeting>, StoreError> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {MEETING_COLUMNS} FROM meetings WHERE calendar_event = ?1"),
                [event_id],
                meeting_from_row,
            )
            .optional()?)
    }

    /// Lists meetings, newest first.
    ///
    /// Params: `limit` – maximum rows.
    /// Returns: meetings ordered by start time descending.
    pub fn recent_meetings(&self, limit: usize) -> Result<Vec<Meeting>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {MEETING_COLUMNS} FROM meetings WHERE status != 'cancelled' ORDER BY starts_at DESC LIMIT ?1"
        ))?;
        let rows = stmt.query_map([limit as i64], meeting_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Changes a meeting's status, enforcing the lifecycle state machine.
    ///
    /// Params: `id` – meeting; `next` – requested status.
    /// Returns: `StoreError::Transition` when the move is not allowed.
    pub fn set_meeting_status(&self, id: MeetingId, next: MeetingStatus) -> Result<(), StoreError> {
        let current = self
            .meeting(id)?
            .ok_or_else(|| StoreError::NotFound(format!("meeting {id}")))?
            .status;
        if current != next && !current.can_transition_to(next) {
            return Err(StoreError::Transition {
                id: id.to_string(),
                from: status_str(current),
                to: status_str(next),
            });
        }
        self.conn.execute(
            "UPDATE meetings SET status = ?2 WHERE id = ?1",
            params![id.to_string(), status_str(next)],
        )?;
        Ok(())
    }

    /// Stores the structured minutes JSON for a meeting.
    ///
    /// Params: `id` – meeting; `minutes` – JSON value (mm-llm `Minutes`).
    /// Returns: `Ok(())` once written.
    pub fn set_minutes(&self, id: MeetingId, minutes: &serde_json::Value) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE meetings SET minutes_json = ?2 WHERE id = ?1",
            params![id.to_string(), minutes.to_string()],
        )?;
        Ok(())
    }

    /// Loads the structured minutes JSON, if generated.
    ///
    /// Params: `id` – meeting.
    /// Returns: the JSON value or `None`.
    pub fn minutes(&self, id: MeetingId) -> Result<Option<serde_json::Value>, StoreError> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT minutes_json FROM meetings WHERE id = ?1",
                [id.to_string()],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        raw.map(|s| {
            serde_json::from_str(&s).map_err(|e| StoreError::Decode(format!("minutes of {id}: {e}")))
        })
        .transpose()
    }

    /// Deletes a meeting and (by cascade) its segments and speaker map.
    ///
    /// Params: `id` – meeting.
    /// Returns: `Ok(())` even if it did not exist.
    pub fn delete_meeting(&self, id: MeetingId) -> Result<(), StoreError> {
        self.conn
            .execute("DELETE FROM meetings WHERE id = ?1", [id.to_string()])?;
        Ok(())
    }

    // ---- people -------------------------------------------------------------------------

    /// Inserts or updates a person.
    ///
    /// Params: `p` – person; `hue` – stable display hue (0–359), kept if `None`.
    /// Returns: `Ok(())` once written.
    pub fn upsert_person(&self, p: &Person, hue: Option<u16>) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO people(id, display_name, email, note_path, hue) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET display_name=excluded.display_name, email=excluded.email,
                note_path=excluded.note_path, hue=coalesce(excluded.hue, people.hue)",
            params![p.id.to_string(), p.display_name, p.email, p.note_path, hue],
        )?;
        Ok(())
    }

    /// Lists people with their hue, by name.
    ///
    /// Returns: `(person, hue)` pairs.
    pub fn people(&self) -> Result<Vec<(Person, Option<u16>)>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, display_name, email, note_path, hue FROM people ORDER BY display_name",
        )?;
        let rows = stmt.query_map([], |r| {
            let id: String = r.get(0)?;
            Ok((
                Person {
                    id: PersonId(from_json(&format!("\"{id}\""), "people.id")?),
                    display_name: r.get(1)?,
                    email: r.get(2)?,
                    note_path: r.get(3)?,
                },
                r.get::<_, Option<u16>>(4)?,
            ))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Finds a person by exact display name (calendar attendees are matched this way).
    ///
    /// Params: `name` – display name.
    /// Returns: the person id if known.
    pub fn person_by_name(&self, name: &str) -> Result<Option<PersonId>, StoreError> {
        let id: Option<String> = self
            .conn
            .query_row(
                "SELECT id FROM people WHERE display_name = ?1",
                [name],
                |r| r.get(0),
            )
            .optional()?;
        id.map(|s| {
            s.parse()
                .map(PersonId)
                .map_err(|e| StoreError::Decode(format!("person id {s}: {e}")))
        })
        .transpose()
    }

    /// Deletes a person and their voiceprints.
    ///
    /// Params: `id` – person.
    /// Returns: `Ok(())`.
    pub fn delete_person(&self, id: PersonId) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE meeting_speakers SET person_id = NULL WHERE person_id = ?1",
            [id.to_string()],
        )?;
        self.conn
            .execute("DELETE FROM people WHERE id = ?1", [id.to_string()])?;
        Ok(())
    }

    /// Stores a voiceprint sample.
    ///
    /// Params: `v` – the sample.
    /// Returns: `Ok(())` once written.
    pub fn add_voiceprint(&self, v: &Voiceprint) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO voiceprints(id, person_id, model, vector, source_json, speech_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                v.id.to_string(),
                v.person_id.to_string(),
                v.model,
                f32_bytes(&v.vector),
                to_json(&v.source),
                v.speech_ms as i64
            ],
        )?;
        Ok(())
    }

    /// Lists voiceprints, optionally for one person.
    ///
    /// Params: `person` – filter, or `None` for all.
    /// Returns: samples, oldest first.
    pub fn voiceprints(&self, person: Option<PersonId>) -> Result<Vec<Voiceprint>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, person_id, model, vector, source_json, speech_ms FROM voiceprints
             WHERE ?1 IS NULL OR person_id = ?1 ORDER BY created_at",
        )?;
        let rows = stmt.query_map([person.map(|p| p.to_string())], |r| {
            let id: String = r.get(0)?;
            let pid: String = r.get(1)?;
            let source: String = r.get(4)?;
            let blob: Vec<u8> = r.get(3)?;
            Ok(Voiceprint {
                id: VoiceprintId(from_json(&format!("\"{id}\""), "voiceprints.id")?),
                person_id: PersonId(from_json(&format!("\"{pid}\""), "voiceprints.person_id")?),
                model: r.get(2)?,
                vector: f32_from_bytes(&blob),
                source: from_json::<VoiceprintSource>(&source, "voiceprints.source_json")?,
                speech_ms: r.get::<_, i64>(5)? as u64,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Deletes one voiceprint sample.
    ///
    /// Params: `id` – sample id.
    /// Returns: `Ok(())`.
    pub fn delete_voiceprint(&self, id: VoiceprintId) -> Result<(), StoreError> {
        self.conn
            .execute("DELETE FROM voiceprints WHERE id = ?1", [id.to_string()])?;
        Ok(())
    }

    // ---- speakers of a meeting ---------------------------------------------------------

    /// Names an anonymous cluster of a meeting ("说话人 2" → 赵磊).
    ///
    /// Params: `meeting` – meeting; `cluster` – cluster index; `person` – person or `None` to clear.
    /// Returns: `Ok(())`.
    pub fn name_cluster(
        &self,
        meeting: MeetingId,
        cluster: u32,
        person: Option<PersonId>,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO meeting_speakers(meeting_id, cluster_index, person_id) VALUES (?1, ?2, ?3)
             ON CONFLICT(meeting_id, cluster_index) DO UPDATE SET person_id = excluded.person_id",
            params![meeting.to_string(), cluster, person.map(|p| p.to_string())],
        )?;
        Ok(())
    }

    /// Cluster → person assignments of a meeting.
    ///
    /// Params: `meeting` – meeting.
    /// Returns: `(cluster_index, person_id)` pairs.
    pub fn cluster_names(&self, meeting: MeetingId) -> Result<Vec<(u32, PersonId)>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT cluster_index, person_id FROM meeting_speakers WHERE meeting_id = ?1 AND person_id IS NOT NULL",
        )?;
        let rows = stmt.query_map([meeting.to_string()], |r| {
            let pid: String = r.get(1)?;
            Ok((
                r.get(0)?,
                PersonId(from_json(&format!("\"{pid}\""), "meeting_speakers.person_id")?),
            ))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // ---- transcript segments -----------------------------------------------------------

    /// Appends a finalised segment.
    ///
    /// Params: `seg` – segment; `pass` – live or final; `utterance_id` – engine id (live only).
    /// Returns: the new row id.
    pub fn add_segment(
        &self,
        seg: &TranscriptSegment,
        pass: SegmentPass,
        utterance_id: Option<u64>,
    ) -> Result<i64, StoreError> {
        self.conn.execute(
            "INSERT INTO segments(meeting_id, pass, utterance_id, origin, start_ms, end_ms, text, language, speaker_json, spatial_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                seg.meeting_id.to_string(),
                pass.as_str(),
                utterance_id.map(|u| u as i64),
                to_json(&seg.origin).trim_matches('"'),
                seg.start_ms as i64,
                seg.end_ms as i64,
                seg.text,
                seg.language,
                to_json(&seg.speaker),
                seg.spatial.as_ref().map(to_json),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Replaces all segments of one pass (used by the post-meeting re-transcription).
    ///
    /// Params: `meeting` – meeting; `pass` – pass to replace; `segments` – new segments.
    /// Returns: `Ok(())` once the transaction committed.
    pub fn replace_segments(
        &mut self,
        meeting: MeetingId,
        pass: SegmentPass,
        segments: &[TranscriptSegment],
    ) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM segments WHERE meeting_id = ?1 AND pass = ?2",
            params![meeting.to_string(), pass.as_str()],
        )?;
        for seg in segments {
            tx.execute(
                "INSERT INTO segments(meeting_id, pass, origin, start_ms, end_ms, text, language, speaker_json, spatial_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    meeting.to_string(),
                    pass.as_str(),
                    to_json(&seg.origin).trim_matches('"'),
                    seg.start_ms as i64,
                    seg.end_ms as i64,
                    seg.text,
                    seg.language,
                    to_json(&seg.speaker),
                    seg.spatial.as_ref().map(to_json),
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The best available transcript: the final pass if it exists, otherwise the live pass.
    ///
    /// Params: `meeting` – meeting.
    /// Returns: segments ordered by start time.
    pub fn transcript(&self, meeting: MeetingId) -> Result<Vec<TranscriptSegment>, StoreError> {
        let has_final: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM segments WHERE meeting_id = ?1 AND pass = 'final')",
            [meeting.to_string()],
            |r| r.get(0),
        )?;
        let pass = if has_final { "final" } else { "live" };
        let mut stmt = self.conn.prepare(
            "SELECT origin, start_ms, end_ms, text, language, speaker_json, spatial_json FROM segments
             WHERE meeting_id = ?1 AND pass = ?2 ORDER BY start_ms, id",
        )?;
        let rows = stmt.query_map(params![meeting.to_string(), pass], |r| {
            let origin: String = r.get(0)?;
            let speaker: String = r.get(5)?;
            let spatial: Option<String> = r.get(6)?;
            Ok(TranscriptSegment {
                meeting_id: meeting,
                origin: from_json::<AudioOrigin>(&format!("\"{origin}\""), "segments.origin")?,
                start_ms: r.get::<_, i64>(1)? as u64,
                end_ms: r.get::<_, i64>(2)? as u64,
                text: r.get(3)?,
                language: r.get(4)?,
                speaker: from_json::<SpeakerLabel>(&speaker, "segments.speaker_json")?,
                spatial: spatial
                    .map(|s| from_json::<SpatialCue>(&s, "segments.spatial_json"))
                    .transpose()?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Rewrites the speaker of every segment labelled `from` (engine `speaker/relabel`).
    ///
    /// Params: `meeting` – meeting; `from` / `to` – labels.
    /// Returns: number of rows changed.
    pub fn relabel_segments(
        &self,
        meeting: MeetingId,
        from: &SpeakerLabel,
        to: &SpeakerLabel,
    ) -> Result<usize, StoreError> {
        Ok(self.conn.execute(
            "UPDATE segments SET speaker_json = ?3 WHERE meeting_id = ?1 AND speaker_json = ?2",
            params![meeting.to_string(), to_json(from), to_json(to)],
        )?)
    }
}

/// Builds a fresh scheduled in-person meeting (helper for tests and quick record).
///
/// Params: `title` – title; `starts_at` – start time.
/// Returns: a meeting with defaults.
pub fn new_meeting(title: &str, starts_at: DateTime<Utc>) -> Meeting {
    Meeting {
        id: MeetingId::new(),
        title: title.to_string(),
        status: MeetingStatus::Scheduled,
        venue: MeetingVenue::InPerson { room: None },
        starts_at,
        ends_at: None,
        calendar_event_id: None,
        participants: vec![],
        note_path: None,
        tags: vec![],
        recording_dir: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn seg(meeting: MeetingId, start: u64, text: &str, speaker: SpeakerLabel) -> TranscriptSegment {
        TranscriptSegment {
            meeting_id: meeting,
            start_ms: start,
            end_ms: start + 1000,
            text: text.into(),
            language: Some("zh".into()),
            speaker,
            origin: AudioOrigin::Microphone,
            spatial: None,
        }
    }

    #[test]
    fn meeting_round_trip_and_lifecycle() {
        let store = Store::open_in_memory().unwrap();
        let mut m = new_meeting("周会", Utc.with_ymd_and_hms(2026, 11, 5, 6, 0, 0).unwrap());
        m.calendar_event_id = Some("ev1".into());
        store.upsert_meeting(&m).unwrap();
        assert_eq!(store.meeting(m.id).unwrap().unwrap(), m);
        assert_eq!(store.meeting_for_event("ev1").unwrap().unwrap().id, m.id);

        store.set_meeting_status(m.id, MeetingStatus::Live).unwrap();
        let err = store
            .set_meeting_status(m.id, MeetingStatus::Scheduled)
            .unwrap_err();
        assert!(err.to_string().contains("live"), "{err}");

        store
            .set_minutes(m.id, &serde_json::json!({"summary": "ok"}))
            .unwrap();
        assert_eq!(store.minutes(m.id).unwrap().unwrap()["summary"], "ok");
        assert_eq!(store.recent_meetings(10).unwrap().len(), 1);
    }

    #[test]
    fn transcript_prefers_final_pass_and_relabels() {
        let mut store = Store::open_in_memory().unwrap();
        let m = new_meeting("t", Utc::now());
        store.upsert_meeting(&m).unwrap();
        let c1 = SpeakerLabel::Cluster { index: 1 };
        let c2 = SpeakerLabel::Cluster { index: 2 };
        store
            .add_segment(&seg(m.id, 2000, "第二句", c2.clone()), SegmentPass::Live, Some(2))
            .unwrap();
        store
            .add_segment(&seg(m.id, 0, "第一句", c1.clone()), SegmentPass::Live, Some(1))
            .unwrap();
        let live = store.transcript(m.id).unwrap();
        assert_eq!(live[0].text, "第一句");

        assert_eq!(store.relabel_segments(m.id, &c2, &c1).unwrap(), 1);
        assert!(store.transcript(m.id).unwrap().iter().all(|s| s.speaker == c1));

        store
            .replace_segments(m.id, SegmentPass::Final, &[seg(m.id, 0, "精修", c1.clone())])
            .unwrap();
        let best = store.transcript(m.id).unwrap();
        assert_eq!(best.len(), 1);
        assert_eq!(best[0].text, "精修");
    }

    #[test]
    fn people_voiceprints_and_cluster_names() {
        let store = Store::open_in_memory().unwrap();
        let p = Person {
            id: PersonId::new(),
            display_name: "赵磊".into(),
            email: None,
            note_path: None,
        };
        store.upsert_person(&p, Some(210)).unwrap();
        store.upsert_person(&p, None).unwrap();
        assert_eq!(store.people().unwrap()[0].1, Some(210), "hue is kept");
        assert_eq!(store.person_by_name("赵磊").unwrap(), Some(p.id));

        let v = Voiceprint {
            id: VoiceprintId::new(),
            person_id: p.id,
            model: "wespeaker".into(),
            vector: vec![0.5, -0.25],
            source: VoiceprintSource::Enrollment,
            speech_ms: 15_000,
        };
        store.add_voiceprint(&v).unwrap();
        assert_eq!(store.voiceprints(Some(p.id)).unwrap(), vec![v]);

        let m = new_meeting("t", Utc::now());
        store.upsert_meeting(&m).unwrap();
        store.name_cluster(m.id, 4, Some(p.id)).unwrap();
        assert_eq!(store.cluster_names(m.id).unwrap(), vec![(4, p.id)]);

        store.delete_person(p.id).unwrap();
        assert!(store.voiceprints(None).unwrap().is_empty());
        assert!(store.cluster_names(m.id).unwrap().is_empty());
    }
}
