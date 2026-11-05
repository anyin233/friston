//! Diffs freshly computed chunks against what is already stored.

use std::collections::HashMap;

/// What the indexer must do to bring the store in line with a source.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SyncPlan {
    /// Stored chunk ids that still match a fresh chunk (`(stored_id, fresh_index)`); no re-embed.
    pub keep: Vec<(i64, usize)>,
    /// Indices into the fresh chunk list that need inserting and embedding.
    pub insert: Vec<usize>,
    /// Stored chunk ids whose content no longer exists.
    pub delete: Vec<i64>,
}

/// Computes the minimal change set, matching chunks by content hash (as a multiset).
///
/// Matching by hash rather than position means inserting a paragraph near the
/// top of a note does not re-embed everything below it.
///
/// Params: `stored` – `(chunk_id, hash)` currently in the DB; `fresh` – hashes of new chunks in order.
/// Returns: the plan; `keep.len() + insert.len() == fresh.len()`.
pub fn plan(stored: &[(i64, String)], fresh: &[String]) -> SyncPlan {
    let mut by_hash: HashMap<&str, Vec<i64>> = HashMap::new();
    for (id, hash) in stored {
        by_hash.entry(hash.as_str()).or_default().push(*id);
    }
    let mut out = SyncPlan::default();
    for (i, hash) in fresh.iter().enumerate() {
        match by_hash.get_mut(hash.as_str()).and_then(|ids| ids.pop()) {
            Some(id) => out.keep.push((id, i)),
            None => out.insert.push(i),
        }
    }
    out.delete = by_hash.into_values().flatten().collect();
    out.delete.sort_unstable();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn unchanged_source_is_a_no_op() {
        let stored = vec![(1, "a".into()), (2, "b".into())];
        let p = plan(&stored, &s(&["a", "b"]));
        assert!(p.insert.is_empty() && p.delete.is_empty());
        assert_eq!(p.keep.len(), 2);
    }

    #[test]
    fn insertion_at_top_only_embeds_the_new_chunk() {
        let stored = vec![(1, "a".into()), (2, "b".into())];
        let p = plan(&stored, &s(&["new", "a", "b"]));
        assert_eq!(p.insert, vec![0]);
        assert!(p.delete.is_empty());
    }

    #[test]
    fn edits_and_deletions_are_detected_and_duplicates_respected() {
        let stored = vec![(1, "a".into()), (2, "a".into()), (3, "c".into())];
        let p = plan(&stored, &s(&["a", "c2"]));
        assert_eq!(p.insert, vec![1]);
        assert_eq!(p.keep.len(), 1);
        assert_eq!(p.delete.len(), 2); // one duplicate "a" and the old "c"
    }
}
