//! Local persistence for Meeting Master.
//!
//! One SQLite file holds metadata, an FTS5 keyword index (trigram tokenizer so
//! Chinese works without a segmenter) and a sqlite-vec `vec0` table for
//! EmbeddingGemma 2 vectors. Hybrid search fuses both rankings.

use std::path::Path;
use std::sync::Once;

use rusqlite::{Connection, params};

/// Embedding width stored in `chunk_vec` (EmbeddingGemma 2 native output).
pub const EMBEDDING_DIMS: usize = 768;

const MIGRATIONS: &[(&str, &str)] = &[("0001_init", include_str!("../migrations/0001_init.sql"))];

/// Errors raised by the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("migration {name} failed: {source}")]
    Migration {
        name: &'static str,
        source: rusqlite::Error,
    },
    #[error("vector has {got} dims, store expects {expected}")]
    Dims { got: usize, expected: usize },
}

/// Handle to the workbench database.
pub struct Store {
    conn: Connection,
}

/// Registers sqlite-vec for every connection opened by this process (idempotent).
fn register_sqlite_vec() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: `sqlite3_vec_init` has the exact entry-point signature SQLite expects
        // for auto extensions; the transmute only erases the function-pointer type.
        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<
                *const (),
                unsafe extern "C" fn(
                    *mut rusqlite::ffi::sqlite3,
                    *mut *mut std::os::raw::c_char,
                    *const rusqlite::ffi::sqlite3_api_routines,
                ) -> std::os::raw::c_int,
            >(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
    });
}

impl Store {
    /// Opens (or creates) the database at `path` and applies pending migrations.
    ///
    /// Params: `path` – database file path.
    /// Returns: a ready-to-use store.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        register_sqlite_vec();
        Self::init(Connection::open(path)?)
    }

    /// Opens a throwaway in-memory database (tests, previews).
    ///
    /// Returns: a migrated in-memory store.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        register_sqlite_vec();
        Self::init(Connection::open_in_memory()?)
    }

    /// Applies pragmas and migrations to a fresh connection.
    fn init(conn: Connection) -> Result<Self, StoreError> {
        // WAL keeps the UI responsive while the indexer writes in the background.
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS _migrations (name TEXT PRIMARY KEY);",
        )?;
        for (name, sql) in MIGRATIONS {
            let done: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM _migrations WHERE name = ?1)",
                [name],
                |r| r.get(0),
            )?;
            if !done {
                conn.execute_batch(sql)
                    .map_err(|source| StoreError::Migration { name, source })?;
                conn.execute("INSERT INTO _migrations(name) VALUES (?1)", [name])?;
            }
        }
        Ok(Self { conn })
    }

    /// Returns the sqlite-vec version string (sanity check that the extension loaded).
    pub fn vec_version(&self) -> Result<String, StoreError> {
        Ok(self
            .conn
            .query_row("SELECT vec_version()", [], |r| r.get(0))?)
    }

    /// Stores (or replaces) the embedding of chunk `chunk_id`.
    ///
    /// Params: `chunk_id` – rowid in `chunks`; `vector` – L2-normalised embedding; `model` – model stamp.
    /// Returns: `Ok(())` once both the vector and the chunk's model stamp are written.
    pub fn put_embedding(
        &mut self,
        chunk_id: i64,
        vector: &[f32],
        model: &str,
    ) -> Result<(), StoreError> {
        if vector.len() != EMBEDDING_DIMS {
            return Err(StoreError::Dims {
                got: vector.len(),
                expected: EMBEDDING_DIMS,
            });
        }
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM chunk_vec WHERE rowid = ?1", [chunk_id])?;
        tx.execute(
            "INSERT INTO chunk_vec(rowid, embedding) VALUES (?1, ?2)",
            params![chunk_id, f32_bytes(vector)],
        )?;
        tx.execute(
            "UPDATE chunks SET embed_model = ?2, embedded_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id = ?1",
            params![chunk_id, model],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// K-nearest-neighbour search over chunk embeddings (cosine distance).
    ///
    /// Params: `query` – L2-normalised query vector; `k` – number of hits.
    /// Returns: `(chunk_id, distance)` pairs, nearest first.
    pub fn knn(&self, query: &[f32], k: usize) -> Result<Vec<(i64, f64)>, StoreError> {
        if query.len() != EMBEDDING_DIMS {
            return Err(StoreError::Dims {
                got: query.len(),
                expected: EMBEDDING_DIMS,
            });
        }
        let mut stmt = self.conn.prepare(
            "SELECT rowid, distance FROM chunk_vec WHERE embedding MATCH ?1 AND k = ?2 ORDER BY distance",
        )?;
        let rows = stmt.query_map(params![f32_bytes(query), k as i64], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Direct access for crates that own their own queries (indexer, search).
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Mutable access for transactional writers.
    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }
}

/// Serialises a vector the way sqlite-vec expects (packed little-endian f32).
fn f32_bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(dim_hot: usize) -> Vec<f32> {
        let mut v = vec![0.0; EMBEDDING_DIMS];
        v[dim_hot] = 1.0;
        v
    }

    fn seed_chunk(store: &Store, text: &str) -> i64 {
        let c = store.conn();
        c.execute(
            "INSERT OR IGNORE INTO sources(kind, uri, content_hash, updated_at) VALUES ('note','a.md','h','now')",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO chunks(source_id, modality, locator_json, text, content_hash)
             VALUES ((SELECT id FROM sources WHERE uri='a.md'), 'text', '{}', ?1, ?1)",
            [text],
        )
        .unwrap();
        c.last_insert_rowid()
    }

    #[test]
    fn sqlite_vec_is_loaded() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.vec_version().unwrap().starts_with('v'));
    }

    #[test]
    fn knn_returns_nearest_chunk_and_delete_cascades() {
        let mut store = Store::open_in_memory().unwrap();
        let a = seed_chunk(&store, "预算评审");
        let b = seed_chunk(&store, "launch plan");
        store
            .put_embedding(a, &unit(0), "embeddinggemma-2@768")
            .unwrap();
        store
            .put_embedding(b, &unit(1), "embeddinggemma-2@768")
            .unwrap();
        let hits = store.knn(&unit(1), 1).unwrap();
        assert_eq!(hits[0].0, b);

        store
            .conn()
            .execute("DELETE FROM chunks WHERE id = ?1", [b])
            .unwrap();
        let hits = store.knn(&unit(1), 2).unwrap();
        assert!(
            hits.iter().all(|(id, _)| *id != b),
            "vector must be removed with its chunk"
        );
    }

    #[test]
    fn fts_trigram_matches_chinese_substrings() {
        let store = Store::open_in_memory().unwrap();
        let id = seed_chunk(&store, "第三季度预算评审会议");
        let hit: i64 = store
            .conn()
            .query_row(
                "SELECT rowid FROM chunk_fts WHERE chunk_fts MATCH '预算评审'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hit, id);
    }

    #[test]
    fn rejects_wrong_dimensions() {
        let mut store = Store::open_in_memory().unwrap();
        let id = seed_chunk(&store, "x");
        assert!(matches!(
            store.put_embedding(id, &[1.0; 3], "m"),
            Err(StoreError::Dims { got: 3, .. })
        ));
    }
}
