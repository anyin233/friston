-- Cloud ASR usage & estimated spend ledger (accumulated locally per utterance / refine job).
CREATE TABLE IF NOT EXISTS asr_usage (
    id              INTEGER PRIMARY KEY,
    recorded_at     TEXT NOT NULL,
    meeting_id      TEXT REFERENCES meetings(id) ON DELETE SET NULL,
    provider        TEXT NOT NULL,
    mode            TEXT NOT NULL,
    model           TEXT NOT NULL,
    audio_ms        INTEGER NOT NULL,
    input_tokens    INTEGER NOT NULL DEFAULT 0,
    output_tokens   INTEGER NOT NULL DEFAULT 0,
    cost_usd        REAL NOT NULL
);
CREATE INDEX IF NOT EXISTS asr_usage_by_provider_time ON asr_usage(provider, recorded_at);
