-- Meeting Master schema v1.
-- Every indexable thing (note section, transcript span, audio/video window, image)
-- is a row in `chunks`; vectors live in `chunk_vec` keyed by the same rowid.

CREATE TABLE IF NOT EXISTS meetings (
    id              TEXT PRIMARY KEY,
    title           TEXT NOT NULL,
    status          TEXT NOT NULL,
    venue_json      TEXT NOT NULL,
    starts_at       TEXT NOT NULL,
    ends_at         TEXT,
    calendar_event  TEXT,
    note_path       TEXT,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

CREATE TABLE IF NOT EXISTS people (
    id              TEXT PRIMARY KEY,
    display_name    TEXT NOT NULL,
    email           TEXT,
    note_path       TEXT
);

CREATE TABLE IF NOT EXISTS voiceprints (
    id              TEXT PRIMARY KEY,
    person_id       TEXT NOT NULL REFERENCES people(id) ON DELETE CASCADE,
    model           TEXT NOT NULL,
    vector          BLOB NOT NULL,           -- little-endian f32
    source_json     TEXT NOT NULL,
    speech_ms       INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS meeting_speakers (
    meeting_id      TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    cluster_index   INTEGER NOT NULL,
    person_id       TEXT REFERENCES people(id),
    centroid        BLOB,                    -- speaker-embedding centroid for relabel/merge
    azimuth_deg     REAL,                    -- dominant direction in this meeting, if known
    PRIMARY KEY (meeting_id, cluster_index)
);

-- A source is anything the indexer watches: a vault note, a transcript, a media asset.
CREATE TABLE IF NOT EXISTS sources (
    id              INTEGER PRIMARY KEY,
    kind            TEXT NOT NULL,           -- note | transcript | asset
    uri             TEXT NOT NULL UNIQUE,    -- vault-relative path or asset path
    meeting_id      TEXT REFERENCES meetings(id) ON DELETE SET NULL,
    content_hash    TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS chunks (
    id              INTEGER PRIMARY KEY,
    source_id       INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
    modality        TEXT NOT NULL,           -- text | image | audio | video
    locator_json    TEXT NOT NULL,           -- heading path / time range / page
    text            TEXT,                    -- text content or caption/transcript surrogate
    content_hash    TEXT NOT NULL,
    embed_model     TEXT,                    -- NULL until embedded; model+dims stamp otherwise
    embedded_at     TEXT
);
CREATE INDEX IF NOT EXISTS chunks_by_source ON chunks(source_id);
CREATE INDEX IF NOT EXISTS chunks_pending ON chunks(embed_model) WHERE embed_model IS NULL;

CREATE VIRTUAL TABLE IF NOT EXISTS chunk_fts USING fts5(
    text, content='chunks', content_rowid='id', tokenize='trigram'
);
CREATE TRIGGER IF NOT EXISTS chunks_ai AFTER INSERT ON chunks BEGIN
    INSERT INTO chunk_fts(rowid, text) VALUES (new.id, coalesce(new.text, ''));
END;
CREATE TRIGGER IF NOT EXISTS chunks_ad AFTER DELETE ON chunks BEGIN
    INSERT INTO chunk_fts(chunk_fts, rowid, text) VALUES ('delete', old.id, coalesce(old.text, ''));
    DELETE FROM chunk_vec WHERE rowid = old.id;
END;
CREATE TRIGGER IF NOT EXISTS chunks_au AFTER UPDATE OF text ON chunks BEGIN
    INSERT INTO chunk_fts(chunk_fts, rowid, text) VALUES ('delete', old.id, coalesce(old.text, ''));
    INSERT INTO chunk_fts(rowid, text) VALUES (new.id, coalesce(new.text, ''));
END;

-- EmbeddingGemma 2 native dimension is 768 (MRL allows 512/256/128 prefixes).
CREATE VIRTUAL TABLE IF NOT EXISTS chunk_vec USING vec0(
    embedding float[768] distance_metric=cosine
);
