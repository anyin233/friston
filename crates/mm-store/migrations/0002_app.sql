-- Meeting Master schema v2: transcripts, richer meetings, people metadata.

ALTER TABLE meetings ADD COLUMN participants_json TEXT NOT NULL DEFAULT '[]';
ALTER TABLE meetings ADD COLUMN tags_json        TEXT NOT NULL DEFAULT '[]';
ALTER TABLE meetings ADD COLUMN recording_dir    TEXT;
ALTER TABLE meetings ADD COLUMN minutes_json     TEXT;   -- structured minutes (mm-llm::Minutes)
CREATE INDEX IF NOT EXISTS meetings_by_start ON meetings(starts_at);
CREATE UNIQUE INDEX IF NOT EXISTS meetings_by_event ON meetings(calendar_event) WHERE calendar_event IS NOT NULL;

-- Finalised caption / transcript segments. Live captions and the post-meeting pass both land here;
-- `pass` tells them apart so re-transcription can replace the live text wholesale.
CREATE TABLE IF NOT EXISTS segments (
    id              INTEGER PRIMARY KEY,
    meeting_id      TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    pass            TEXT NOT NULL DEFAULT 'live',  -- live | final
    utterance_id    INTEGER,
    origin          TEXT NOT NULL,                 -- microphone | system
    start_ms        INTEGER NOT NULL,
    end_ms          INTEGER NOT NULL,
    text            TEXT NOT NULL,
    language        TEXT,
    speaker_json    TEXT NOT NULL,
    spatial_json    TEXT
);
CREATE INDEX IF NOT EXISTS segments_by_meeting ON segments(meeting_id, pass, start_ms);

-- Speaker hue is identity: one person keeps one hue on every surface.
ALTER TABLE people ADD COLUMN hue INTEGER;
ALTER TABLE voiceprints ADD COLUMN created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'));
