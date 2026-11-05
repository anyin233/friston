//! Index bookkeeping (sources and chunks) and hybrid search.

use mm_core::MeetingId;
use rusqlite::{OptionalExtension, params};
use serde_json::Value;

use crate::{EMBEDDING_DIMS, Store, StoreError, f32_bytes};

/// Reciprocal-rank-fusion constant; 60 is the value from the original RRF paper and
/// keeps one list's top hit from drowning the other list.
const RRF_K: f64 = 60.0;

/// Kind of indexed source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceKind {
    /// A Markdown note in the Obsidian vault (uri = vault-relative path).
    Note,
    /// A meeting transcript (uri = `transcript:<meeting id>`).
    Transcript,
    /// A media file: recording, screenshot (uri = absolute path).
    Asset,
}

impl SourceKind {
    /// Column value.
    pub fn as_str(self) -> &'static str {
        match self {
            SourceKind::Note => "note",
            SourceKind::Transcript => "transcript",
            SourceKind::Asset => "asset",
        }
    }

    fn parse(s: &str) -> SourceKind {
        match s {
            "transcript" => SourceKind::Transcript,
            "asset" => SourceKind::Asset,
            _ => SourceKind::Note,
        }
    }
}

/// A chunk to insert.
#[derive(Debug, Clone, PartialEq)]
pub struct NewChunk {
    /// `text` | `image` | `audio` | `video`.
    pub modality: String,
    /// Where the chunk lives: heading path + title, or time range + media path.
    pub locator: Value,
    /// Text content (or a text surrogate for media) for FTS and display.
    pub text: Option<String>,
    pub hash: String,
}

/// A chunk waiting for (re-)embedding.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingChunk {
    pub id: i64,
    pub modality: String,
    pub locator: Value,
    pub text: Option<String>,
    pub source_uri: String,
}

/// One search result.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub chunk_id: i64,
    /// Fused RRF score; higher is better.
    pub score: f64,
    pub modality: String,
    pub text: Option<String>,
    pub locator: Value,
    pub source_kind: SourceKind,
    pub source_uri: String,
    pub meeting_id: Option<String>,
    pub meeting_title: Option<String>,
    /// Whether the keyword index matched (the UI highlights these).
    pub keyword_match: bool,
}

/// Index size summary for the settings page.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexStats {
    pub sources: u64,
    pub chunks: u64,
    /// Chunks embedded with the current model stamp.
    pub embedded: u64,
}

/// Escapes a user query for FTS5: each whitespace-separated term becomes a quoted phrase.
fn fts_query(query: &str) -> Option<String> {
    let terms: Vec<String> = query
        .split_whitespace()
        // The trigram tokenizer cannot match terms shorter than three characters.
        .filter(|t| t.chars().count() >= 3)
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" AND "))
}

impl Store {
    /// Registers a source or updates its hash.
    ///
    /// Params: `kind`, `uri` – identity; `meeting` – owning meeting; `hash` – content hash.
    /// Returns: `(source_id, changed)`; `changed` is false when the stored hash already matched.
    pub fn upsert_source(
        &self,
        kind: SourceKind,
        uri: &str,
        meeting: Option<MeetingId>,
        hash: &str,
    ) -> Result<(i64, bool), StoreError> {
        let existing: Option<(i64, String)> = self
            .conn
            .query_row(
                "SELECT id, content_hash FROM sources WHERE uri = ?1",
                [uri],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let meeting = meeting.map(|m| m.to_string());
        match existing {
            Some((id, old)) if old == hash => Ok((id, false)),
            Some((id, _)) => {
                self.conn.execute(
                    "UPDATE sources SET content_hash = ?2, meeting_id = coalesce(?3, meeting_id),
                        updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id = ?1",
                    params![id, hash, meeting],
                )?;
                Ok((id, true))
            }
            None => {
                self.conn.execute(
                    "INSERT INTO sources(kind, uri, meeting_id, content_hash, updated_at)
                     VALUES (?1, ?2, ?3, ?4, strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
                    params![kind.as_str(), uri, meeting, hash],
                )?;
                Ok((self.conn.last_insert_rowid(), true))
            }
        }
    }

    /// Lists sources of one kind.
    ///
    /// Params: `kind` – source kind.
    /// Returns: `(id, uri)` pairs.
    pub fn sources(&self, kind: SourceKind) -> Result<Vec<(i64, String)>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, uri FROM sources WHERE kind = ?1 ORDER BY uri")?;
        let rows = stmt.query_map([kind.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Removes a source and (by cascade) its chunks and vectors.
    ///
    /// Params: `uri` – source identity.
    /// Returns: `true` if something was deleted.
    pub fn delete_source(&self, uri: &str) -> Result<bool, StoreError> {
        Ok(self
            .conn
            .execute("DELETE FROM sources WHERE uri = ?1", [uri])?
            > 0)
    }

    /// Stored `(chunk_id, hash)` pairs of a source, for `mm_index::sync::plan`.
    ///
    /// Params: `source_id` – source row id.
    /// Returns: pairs in ordinal order.
    pub fn chunk_hashes(&self, source_id: i64) -> Result<Vec<(i64, String)>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, content_hash FROM chunks WHERE source_id = ?1 ORDER BY id")?;
        let rows = stmt.query_map([source_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Applies a sync plan atomically: deletes stale chunks and inserts new ones.
    ///
    /// Params: `source_id` – source; `insert` – new chunks; `delete` – stale chunk ids.
    /// Returns: ids of the inserted chunks, in input order.
    pub fn apply_chunks(
        &mut self,
        source_id: i64,
        insert: &[NewChunk],
        delete: &[i64],
    ) -> Result<Vec<i64>, StoreError> {
        let tx = self.conn.transaction()?;
        for id in delete {
            tx.execute("DELETE FROM chunks WHERE id = ?1", [id])?;
        }
        let mut ids = Vec::with_capacity(insert.len());
        for c in insert {
            tx.execute(
                "INSERT INTO chunks(source_id, modality, locator_json, text, content_hash) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![source_id, c.modality, c.locator.to_string(), c.text, c.hash],
            )?;
            ids.push(tx.last_insert_rowid());
        }
        tx.commit()?;
        Ok(ids)
    }

    /// Chunks whose vector is missing or was made with another model stamp.
    ///
    /// Params: `stamp` – current model stamp; `modalities` – modalities the embedder can handle; `limit` – batch size.
    /// Returns: chunks, text first (cheapest and most useful), then by id.
    pub fn pending_chunks(
        &self,
        stamp: &str,
        modalities: &[&str],
        limit: usize,
    ) -> Result<Vec<PendingChunk>, StoreError> {
        let allowed = serde_json::to_string(modalities).expect("strings serialize");
        let mut stmt = self.conn.prepare(
            "SELECT c.id, c.modality, c.locator_json, c.text, s.uri FROM chunks c JOIN sources s ON s.id = c.source_id
             WHERE (c.embed_model IS NULL OR c.embed_model != ?1)
               AND c.modality IN (SELECT value FROM json_each(?2))
             ORDER BY c.modality != 'text', c.id LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![stamp, allowed, limit as i64], |r| {
            let locator: String = r.get(2)?;
            Ok(PendingChunk {
                id: r.get(0)?,
                modality: r.get(1)?,
                locator: serde_json::from_str(&locator).unwrap_or(Value::Null),
                text: r.get(3)?,
                source_uri: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Counts sources, chunks and chunks embedded with `stamp`.
    ///
    /// Params: `stamp` – current model stamp.
    /// Returns: the stats.
    pub fn index_stats(&self, stamp: &str) -> Result<IndexStats, StoreError> {
        Ok(self.conn.query_row(
            "SELECT (SELECT count(*) FROM sources), (SELECT count(*) FROM chunks),
                    (SELECT count(*) FROM chunks WHERE embed_model = ?1)",
            [stamp],
            |r| {
                Ok(IndexStats {
                    sources: r.get::<_, i64>(0)? as u64,
                    chunks: r.get::<_, i64>(1)? as u64,
                    embedded: r.get::<_, i64>(2)? as u64,
                })
            },
        )?)
    }

    /// Hybrid keyword + vector search fused with reciprocal rank fusion.
    ///
    /// Params: `query` – user text; `vector` – query embedding (`search_query` task) if the
    /// embedder is available; `k` – number of results.
    /// Returns: hits ordered by fused score.
    pub fn search(
        &self,
        query: &str,
        vector: Option<&[f32]>,
        k: usize,
    ) -> Result<Vec<SearchHit>, StoreError> {
        let pool = (k * 4).max(20);
        let mut keyword: Vec<i64> = Vec::new();
        if let Some(q) = fts_query(query) {
            let mut stmt = self
                .conn
                .prepare("SELECT rowid FROM chunk_fts WHERE chunk_fts MATCH ?1 ORDER BY rank LIMIT ?2")?;
            keyword = stmt
                .query_map(params![q, pool as i64], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
        } else if !query.trim().is_empty() {
            // Short CJK queries (“预算”) are below the trigram minimum; fall back to LIKE.
            let mut stmt = self
                .conn
                .prepare("SELECT id FROM chunks WHERE text LIKE ?1 ESCAPE '\\' ORDER BY id DESC LIMIT ?2")?;
            let pattern = format!(
                "%{}%",
                query.trim().replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
            );
            keyword = stmt
                .query_map(params![pattern, pool as i64], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
        }
        let semantic: Vec<i64> = match vector {
            Some(v) if v.len() == EMBEDDING_DIMS => {
                let mut stmt = self.conn.prepare(
                    "SELECT rowid FROM chunk_vec WHERE embedding MATCH ?1 AND k = ?2 ORDER BY distance",
                )?;
                stmt.query_map(params![f32_bytes(v), pool as i64], |r| r.get(0))?
                    .collect::<Result<_, _>>()?
            }
            Some(v) => {
                return Err(StoreError::Dims {
                    got: v.len(),
                    expected: EMBEDDING_DIMS,
                });
            }
            None => vec![],
        };

        let mut scores: std::collections::HashMap<i64, (f64, bool)> = Default::default();
        for (rank, id) in keyword.iter().enumerate() {
            let e = scores.entry(*id).or_default();
            e.0 += 1.0 / (RRF_K + rank as f64 + 1.0);
            e.1 = true;
        }
        for (rank, id) in semantic.iter().enumerate() {
            scores.entry(*id).or_default().0 += 1.0 / (RRF_K + rank as f64 + 1.0);
        }
        let mut ranked: Vec<(i64, f64, bool)> =
            scores.into_iter().map(|(id, (s, kw))| (id, s, kw)).collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(b.0.cmp(&a.0)));
        ranked.truncate(k);

        let mut stmt = self.conn.prepare(
            "SELECT c.modality, c.text, c.locator_json, s.kind, s.uri, s.meeting_id, m.title
             FROM chunks c JOIN sources s ON s.id = c.source_id LEFT JOIN meetings m ON m.id = s.meeting_id
             WHERE c.id = ?1",
        )?;
        let mut hits = Vec::with_capacity(ranked.len());
        for (id, score, kw) in ranked {
            let hit = stmt
                .query_row([id], |r| {
                    let locator: String = r.get(2)?;
                    let kind: String = r.get(3)?;
                    Ok(SearchHit {
                        chunk_id: id,
                        score,
                        modality: r.get(0)?,
                        text: r.get(1)?,
                        locator: serde_json::from_str(&locator).unwrap_or(Value::Null),
                        source_kind: SourceKind::parse(&kind),
                        source_uri: r.get(4)?,
                        meeting_id: r.get(5)?,
                        meeting_title: r.get(6)?,
                        keyword_match: kw,
                    })
                })
                .optional()?;
            hits.extend(hit);
        }
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn chunk(text: &str) -> NewChunk {
        NewChunk {
            modality: "text".into(),
            locator: json!({"title": "n"}),
            text: Some(text.into()),
            hash: blake(text),
        }
    }

    fn blake(s: &str) -> String {
        format!("h-{s}")
    }

    fn unit(hot: usize) -> Vec<f32> {
        let mut v = vec![0.0; EMBEDDING_DIMS];
        v[hot] = 1.0;
        v
    }

    #[test]
    fn source_hash_detects_changes() {
        let store = Store::open_in_memory().unwrap();
        let (id, changed) = store.upsert_source(SourceKind::Note, "a.md", None, "h1").unwrap();
        assert!(changed);
        assert_eq!(
            store.upsert_source(SourceKind::Note, "a.md", None, "h1").unwrap(),
            (id, false)
        );
        assert!(store.upsert_source(SourceKind::Note, "a.md", None, "h2").unwrap().1);
        assert_eq!(store.sources(SourceKind::Note).unwrap(), vec![(id, "a.md".into())]);
        assert!(store.delete_source("a.md").unwrap());
    }

    #[test]
    fn pending_chunks_follow_the_model_stamp() {
        let mut store = Store::open_in_memory().unwrap();
        let (src, _) = store.upsert_source(SourceKind::Note, "a.md", None, "h").unwrap();
        let ids = store
            .apply_chunks(src, &[chunk("一"), chunk("二")], &[])
            .unwrap();
        assert_eq!(store.pending_chunks("m@768/p1", &["text"], 10).unwrap().len(), 2);
        assert!(store.pending_chunks("m@768/p1", &["audio"], 10).unwrap().is_empty());
        store.put_embedding(ids[0], &unit(0), "m@768/p1").unwrap();
        assert_eq!(store.pending_chunks("m@768/p1", &["text"], 10).unwrap().len(), 1);
        // A new stamp makes everything pending again (automatic re-index).
        assert_eq!(store.pending_chunks("m@768/p2", &["text"], 10).unwrap().len(), 2);
        let stats = store.index_stats("m@768/p1").unwrap();
        assert_eq!((stats.sources, stats.chunks, stats.embedded), (1, 2, 1));

        store.apply_chunks(src, &[], &[ids[1]]).unwrap();
        assert_eq!(store.chunk_hashes(src).unwrap().len(), 1);
    }

    #[test]
    fn hybrid_search_fuses_keyword_and_vector_rankings() {
        let mut store = Store::open_in_memory().unwrap();
        let (src, _) = store.upsert_source(SourceKind::Note, "a.md", None, "h").unwrap();
        let ids = store
            .apply_chunks(
                src,
                &[
                    chunk("第三季度预算评审"),
                    chunk("产品发布计划"),
                    chunk("预算相关的发布"),
                ],
                &[],
            )
            .unwrap();
        for (i, id) in ids.iter().enumerate() {
            store.put_embedding(*id, &unit(i), "m").unwrap();
        }
        // Keyword only.
        let hits = store.search("预算评审", None, 5).unwrap();
        assert_eq!(hits[0].chunk_id, ids[0]);
        assert!(hits[0].keyword_match);
        // Short query falls back to LIKE and finds both budget chunks.
        assert_eq!(store.search("预算", None, 5).unwrap().len(), 2);
        // Vector pulls in a chunk without keyword overlap.
        let hits = store.search("预算", Some(&unit(1)), 5).unwrap();
        assert_eq!(hits.len(), 3);
        assert!(hits.iter().any(|h| h.chunk_id == ids[1] && !h.keyword_match));
        // Chunk that matched both lists ranks first.
        let both = store.search("预算", Some(&unit(2)), 5).unwrap();
        assert_eq!(both[0].chunk_id, ids[2]);
    }
}
