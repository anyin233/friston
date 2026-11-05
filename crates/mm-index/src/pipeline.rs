//! Brings the store in line with sources and fills in missing embeddings.
//!
//! Every function here is synchronous and side-effect-free apart from the store and
//! the embedder, so the app can run it on a background thread and tests can run it
//! with an in-memory store and a fake embedder.

use std::path::{Path, PathBuf};

use mm_core::{AudioOrigin, MeetingId, Modality, SpeakerLabel, TranscriptSegment};
use mm_store::{NewChunk, SourceKind, Store, StoreError};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::chunk::{TARGET_CHARS, chunk_markdown};
use crate::media::{AUDIO_WINDOW_MS, VIDEO_WINDOW_MS, windows};
use crate::sync::plan;

/// Errors raised while indexing.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("reading {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("embedding batch of {count} chunks: {message}")]
    Embed { count: usize, message: String },
}

/// What changed for one source.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SourceUpdate {
    pub inserted: usize,
    pub deleted: usize,
    pub kept: usize,
}

/// One item for the embedding engine (`embed/batch` item shape).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbedItem {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub images: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub video: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub audio: Vec<String>,
    /// Time range inside `audio` / `video`, so whole recordings need no pre-cut clips.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_ms: Option<u64>,
}

/// Anything that turns items into unit vectors (the `mm-embedd` client, or a test fake).
pub trait Embedder {
    /// Embeds corpus items with the `document` task.
    ///
    /// Params: `items` – batch.
    /// Returns: one vector per item, or an error message.
    fn embed_documents(&self, items: &[EmbedItem]) -> Result<Vec<Vec<f32>>, String>;

    /// Modalities the loaded model supports (`text`, `image`, `audio`, `video`).
    fn modalities(&self) -> Vec<String>;
}

/// blake3 hex of a byte string.
fn hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Diffs `fresh` against the stored chunks of `source_id` and applies the change.
fn sync_source(
    store: &mut Store,
    source_id: i64,
    fresh: Vec<NewChunk>,
) -> Result<SourceUpdate, IndexError> {
    let stored = store.chunk_hashes(source_id)?;
    let hashes: Vec<String> = fresh.iter().map(|c| c.hash.clone()).collect();
    let p = plan(&stored, &hashes);
    let insert: Vec<NewChunk> = p.insert.iter().map(|&i| fresh[i].clone()).collect();
    store.apply_chunks(source_id, &insert, &p.delete)?;
    Ok(SourceUpdate {
        inserted: insert.len(),
        deleted: p.delete.len(),
        kept: p.keep.len(),
    })
}

/// Title of a note: frontmatter `title:` if present, else the file stem.
///
/// Params: `path` – note path; `markdown` – contents.
/// Returns: the title.
pub fn note_title(path: &Path, markdown: &str) -> String {
    if let Some(rest) = markdown.strip_prefix("---\n")
        && let Some(end) = rest.find("\n---")
    {
        for line in rest[..end].lines() {
            if let Some(v) = line.strip_prefix("title:") {
                let v = v.trim();
                // Titles we write are JSON-quoted; hand-written ones may be bare.
                let v = serde_json::from_str::<String>(v).unwrap_or_else(|_| v.trim_matches('\'').to_string());
                if !v.is_empty() {
                    return v;
                }
            }
        }
    }
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Extracts `meeting_id:` from a note's frontmatter so note chunks link to their meeting.
fn frontmatter_meeting(markdown: &str) -> Option<MeetingId> {
    let rest = markdown.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    rest[..end].lines().find_map(|l| {
        let v = l.strip_prefix("meeting_id:")?.trim();
        let v = serde_json::from_str::<String>(v).unwrap_or_else(|_| v.to_string());
        v.parse().ok().map(MeetingId)
    })
}

/// Indexes one vault note.
///
/// Params: `store`; `uri` – vault-relative path (identity); `markdown` – contents.
/// Returns: the change applied (all zero when the note is unchanged).
pub fn index_note(store: &mut Store, uri: &str, markdown: &str) -> Result<SourceUpdate, IndexError> {
    let meeting = frontmatter_meeting(markdown);
    let (source_id, changed) =
        store.upsert_source(SourceKind::Note, uri, meeting, &hash(markdown.as_bytes()))?;
    if !changed {
        return Ok(SourceUpdate::default());
    }
    let title = note_title(Path::new(uri), markdown);
    let fresh = chunk_markdown(&title, markdown)
        .into_iter()
        .map(|c| NewChunk {
            modality: "text".into(),
            locator: json!({"title": c.title, "headings": c.heading_path, "ordinal": c.ordinal}),
            text: Some(c.text),
            hash: c.hash,
        })
        .collect();
    sync_source(store, source_id, fresh)
}

/// Lists Markdown files of a vault, skipping Obsidian's own folders and our temp files.
///
/// Params: `root` – vault folder.
/// Returns: absolute paths.
pub fn vault_notes(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            tracing::warn!("cannot list vault folder {}", dir.display());
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') {
                continue; // .obsidian, .trash, .git
            }
            if p.is_dir() {
                stack.push(p);
            } else if name.ends_with(".md") {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// Vault-relative URI with forward slashes.
///
/// Params: `root` – vault folder; `path` – file inside it.
/// Returns: the relative path, or `None` if `path` is outside the vault.
pub fn vault_uri(root: &Path, path: &Path) -> Option<String> {
    path.strip_prefix(root)
        .ok()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
}

/// Re-indexes one vault file after a change signal; deletes it from the index if it is gone.
///
/// Params: `store`; `root` – vault folder; `path` – changed file.
/// Returns: the change applied.
pub fn refresh_note(store: &mut Store, root: &Path, path: &Path) -> Result<SourceUpdate, IndexError> {
    let Some(uri) = vault_uri(root, path) else {
        return Ok(SourceUpdate::default());
    };
    match std::fs::read_to_string(path) {
        Ok(md) => index_note(store, &uri, &md),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let gone = store.delete_source(&uri)?;
            Ok(SourceUpdate {
                deleted: gone as usize,
                ..Default::default()
            })
        }
        Err(source) => Err(IndexError::Read {
            path: path.display().to_string(),
            source,
        }),
    }
}

/// Full vault pass: indexes every note and drops sources whose file disappeared.
///
/// Params: `store`; `root` – vault folder.
/// Returns: totals over all notes.
pub fn scan_vault(store: &mut Store, root: &Path) -> Result<SourceUpdate, IndexError> {
    let mut total = SourceUpdate::default();
    let notes = vault_notes(root);
    let live: std::collections::HashSet<String> =
        notes.iter().filter_map(|p| vault_uri(root, p)).collect();
    for path in &notes {
        let u = refresh_note(store, root, path)?;
        total.inserted += u.inserted;
        total.deleted += u.deleted;
        total.kept += u.kept;
    }
    for (_, uri) in store.sources(SourceKind::Note)? {
        if !live.contains(&uri) && store.delete_source(&uri)? {
            total.deleted += 1;
        }
    }
    Ok(total)
}

/// Display name for a speaker label in transcript chunks.
fn speaker_name(label: &SpeakerLabel, names: &dyn Fn(&SpeakerLabel) -> String) -> String {
    names(label)
}

/// Indexes a meeting transcript as time-ranged text chunks.
///
/// Consecutive segments are packed up to [`TARGET_CHARS`]; packing is by time so a
/// growing live transcript only changes its last chunk.
///
/// Params: `store`; `meeting` – owner; `title` – meeting title (document title for the embedder);
/// `segments` – ordered segments; `names` – resolves speaker labels to display names.
/// Returns: the change applied.
pub fn index_transcript(
    store: &mut Store,
    meeting: MeetingId,
    title: &str,
    segments: &[TranscriptSegment],
    names: &dyn Fn(&SpeakerLabel) -> String,
) -> Result<SourceUpdate, IndexError> {
    let uri = format!("transcript:{meeting}");
    let mut fresh = Vec::new();
    let mut body = String::new();
    let mut start = None;
    let mut end = 0;
    let mut flush = |body: &mut String, start: &mut Option<u64>, end: u64| {
        if let Some(s) = start.take() {
            let text = std::mem::take(body);
            fresh.push(NewChunk {
                modality: "text".into(),
                locator: json!({"title": title, "meeting": meeting.to_string(), "start_ms": s, "end_ms": end}),
                hash: hash(format!("{title}\x1f{text}").as_bytes()),
                text: Some(text),
            });
        }
    };
    for seg in segments {
        let origin = match seg.origin {
            AudioOrigin::Microphone => "",
            AudioOrigin::System => "（远程）",
        };
        let line = format!(
            "[{}] {}{}：{}",
            fmt_ms(seg.start_ms),
            speaker_name(&seg.speaker, names),
            origin,
            seg.text.trim()
        );
        if !body.is_empty() && body.chars().count() + line.chars().count() > TARGET_CHARS {
            flush(&mut body, &mut start, end);
        }
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&line);
        start.get_or_insert(seg.start_ms);
        end = seg.end_ms;
    }
    flush(&mut body, &mut start, end);
    let all = fresh.iter().map(|c| c.hash.as_str()).collect::<Vec<_>>().join(",");
    let (source_id, changed) =
        store.upsert_source(SourceKind::Transcript, &uri, Some(meeting), &hash(all.as_bytes()))?;
    if !changed {
        return Ok(SourceUpdate::default());
    }
    sync_source(store, source_id, fresh)
}

/// Formats milliseconds as `mm:ss` or `h:mm:ss`.
///
/// Params: `ms` – offset.
/// Returns: the clock string.
pub fn fmt_ms(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

/// Indexes a media asset as time windows (audio/video) or a single chunk (image).
///
/// Params: `store`; `path` – absolute file path; `modality`; `duration_ms` – media length
/// (ignored for images); `meeting` – owner; `fingerprint` – cheap change marker such as
/// size+mtime, so a growing recording is re-windowed.
/// Returns: the change applied.
pub fn index_asset(
    store: &mut Store,
    path: &Path,
    modality: Modality,
    duration_ms: u64,
    meeting: Option<MeetingId>,
    fingerprint: &str,
) -> Result<SourceUpdate, IndexError> {
    let uri = path.display().to_string();
    let (source_id, changed) = store.upsert_source(SourceKind::Asset, &uri, meeting, &hash(fingerprint.as_bytes()))?;
    if !changed {
        return Ok(SourceUpdate::default());
    }
    let (name, window) = match modality {
        Modality::Audio => ("audio", AUDIO_WINDOW_MS),
        Modality::Video => ("video", VIDEO_WINDOW_MS),
        Modality::Image => ("image", 0),
        Modality::Text => ("text", 0),
    };
    let ranges = if window == 0 {
        vec![(0, 0)]
    } else {
        // 5 s overlap so a sentence on a boundary is fully inside some window.
        windows(duration_ms, window, window - 5_000)
    };
    let fresh = ranges
        .into_iter()
        .map(|(s, e)| NewChunk {
            modality: name.into(),
            locator: json!({"path": uri, "start_ms": s, "end_ms": e}),
            text: None,
            hash: hash(format!("{uri}\x1f{s}\x1f{e}").as_bytes()),
        })
        .collect();
    sync_source(store, source_id, fresh)
}

/// Embeds up to `limit` pending chunks with `embedder` and stores the vectors.
///
/// Params: `store`; `embedder`; `stamp` – current model stamp; `limit` – batch size.
/// Returns: number of chunks embedded (0 means nothing is pending).
pub fn embed_pending(
    store: &mut Store,
    embedder: &dyn Embedder,
    stamp: &str,
    limit: usize,
) -> Result<usize, IndexError> {
    let modalities = embedder.modalities();
    let refs: Vec<&str> = modalities.iter().map(String::as_str).collect();
    let pending = store.pending_chunks(stamp, &refs, limit)?;
    if pending.is_empty() {
        return Ok(0);
    }
    let items: Vec<EmbedItem> = pending
        .iter()
        .map(|c| {
            let loc = &c.locator;
            let path = loc["path"].as_str().map(String::from);
            let range = |k: &str| loc[k].as_u64();
            let mut item = EmbedItem {
                title: loc["title"].as_str().map(String::from),
                ..Default::default()
            };
            match (c.modality.as_str(), path) {
                ("image", Some(p)) => item.images = vec![p],
                ("audio", Some(p)) => {
                    item.audio = vec![p];
                    (item.start_ms, item.end_ms) = (range("start_ms"), range("end_ms"));
                }
                ("video", Some(p)) => {
                    item.video = vec![p];
                    (item.start_ms, item.end_ms) = (range("start_ms"), range("end_ms"));
                }
                _ => item.text = c.text.clone(),
            }
            item
        })
        .collect();
    let vectors = embedder
        .embed_documents(&items)
        .map_err(|message| IndexError::Embed {
            count: items.len(),
            message,
        })?;
    if vectors.len() != pending.len() {
        return Err(IndexError::Embed {
            count: items.len(),
            message: format!("engine returned {} vectors", vectors.len()),
        });
    }
    for (c, v) in pending.iter().zip(vectors) {
        store.put_embedding(c.id, &v, stamp)?;
    }
    Ok(pending.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Deterministic fake: one-hot on a hash of the text, records what it was asked.
    struct FakeEmbedder {
        seen: RefCell<Vec<EmbedItem>>,
    }

    impl Embedder for FakeEmbedder {
        fn embed_documents(&self, items: &[EmbedItem]) -> Result<Vec<Vec<f32>>, String> {
            self.seen.borrow_mut().extend_from_slice(items);
            Ok(items
                .iter()
                .map(|i| {
                    let mut v = vec![0.0; mm_store::EMBEDDING_DIMS];
                    let h = blake3::hash(format!("{i:?}").as_bytes()).as_bytes()[0] as usize;
                    v[h] = 1.0;
                    v
                })
                .collect())
        }
        fn modalities(&self) -> Vec<String> {
            vec!["text".into(), "audio".into()]
        }
    }

    const STAMP: &str = "embeddinggemma-2@768/p1";

    #[test]
    fn vault_scan_indexes_updates_and_forgets_notes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".obsidian")).unwrap();
        std::fs::write(root.join(".obsidian/x.md"), "# hidden").unwrap();
        std::fs::create_dir_all(root.join("Meetings")).unwrap();
        let note = root.join("Meetings/周会.md");
        std::fs::write(&note, "---\ntitle: \"周会\"\n---\n# 决策\n\n预算上调\n").unwrap();
        let mut store = Store::open_in_memory().unwrap();

        let first = scan_vault(&mut store, root).unwrap();
        assert_eq!(first.inserted, 1);
        assert_eq!(scan_vault(&mut store, root).unwrap(), SourceUpdate { kept: 0, ..Default::default() });

        std::fs::write(&note, "---\ntitle: \"周会\"\n---\n# 决策\n\n预算上调\n\n# 待办\n\n- 跟进\n").unwrap();
        let u = refresh_note(&mut store, root, &note).unwrap();
        assert_eq!((u.inserted, u.kept, u.deleted), (1, 1, 0));

        std::fs::remove_file(&note).unwrap();
        assert_eq!(scan_vault(&mut store, root).unwrap().deleted, 1);
        assert!(store.sources(SourceKind::Note).unwrap().is_empty());
    }

    #[test]
    fn note_title_and_meeting_come_from_frontmatter() {
        let id = MeetingId::new();
        let md = format!("---\nmeeting_id: \"{id}\"\ntitle: \"Q3 评审\"\n---\nbody");
        assert_eq!(note_title(Path::new("x/y.md"), &md), "Q3 评审");
        assert_eq!(frontmatter_meeting(&md), Some(id));
        assert_eq!(note_title(Path::new("x/y.md"), "body"), "y");
    }

    #[test]
    fn transcript_and_media_chunks_embed_with_ranges() {
        let mut store = Store::open_in_memory().unwrap();
        let m = mm_store::new_meeting("周会", chrono::Utc::now());
        store.upsert_meeting(&m).unwrap();
        let seg = |s: u64, t: &str| TranscriptSegment {
            meeting_id: m.id,
            start_ms: s,
            end_ms: s + 900,
            text: t.into(),
            language: None,
            speaker: SpeakerLabel::Cluster { index: 1 },
            origin: AudioOrigin::System,
            spatial: None,
        };
        let names = |_: &SpeakerLabel| "说话人 1".to_string();
        let segs = vec![seg(0, "大家好"), seg(61_000, "开始吧")];
        let u = index_transcript(&mut store, m.id, "周会", &segs, &names).unwrap();
        assert_eq!(u.inserted, 1);
        assert_eq!(index_transcript(&mut store, m.id, "周会", &segs, &names).unwrap(), SourceUpdate::default());

        index_asset(&mut store, Path::new("/r/mic.caf"), Modality::Audio, 70_000, Some(m.id), "size=1").unwrap();
        index_asset(&mut store, Path::new("/r/screen.mov"), Modality::Video, 10_000, Some(m.id), "size=1").unwrap();

        let fake = FakeEmbedder { seen: RefCell::new(vec![]) };
        // Video is not in the fake's modalities, so it stays pending.
        let n = embed_pending(&mut store, &fake, STAMP, 100).unwrap();
        assert_eq!(n, 1 + 3);
        let seen = fake.seen.borrow();
        assert!(seen[0].text.as_deref().unwrap().contains("[01:01] 说话人 1（远程）：开始吧"));
        assert_eq!(seen[0].title.as_deref(), Some("周会"));
        assert!(seen.iter().any(|i| i.audio == ["/r/mic.caf"] && i.start_ms == Some(40_000)));
        drop(seen);
        assert_eq!(embed_pending(&mut store, &fake, STAMP, 100).unwrap(), 0);

        let hits = store.search("开始吧", None, 5).unwrap();
        assert_eq!(hits[0].meeting_title.as_deref(), Some("周会"));
    }

    #[test]
    fn clock_format() {
        assert_eq!(fmt_ms(61_000), "01:01");
        assert_eq!(fmt_ms(3_723_000), "1:02:03");
    }
}
