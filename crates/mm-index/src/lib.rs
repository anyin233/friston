//! Automatic embedding management.
//!
//! Users type notes during a meeting and keep editing afterwards (in the app or
//! directly in Obsidian), so indexing must be continuous and incremental:
//!
//! 1. A change signal (editor keystroke, FSEvents on the vault, new transcript
//!    segment, finished recording) `touch`es a source in the [`debounce::Debouncer`].
//! 2. When the source goes quiet, it is re-chunked ([`chunk`]) with *stable*
//!    boundaries, so an edit only changes the chunks it touches.
//! 3. [`sync::plan`] diffs fresh chunk hashes against stored ones; only new or
//!    changed chunks are sent to the embedding engine, deleted ones are dropped.
//! 4. Media is windowed ([`media`]) to fit EmbeddingGemma 2's 8K-token context.

pub mod chunk;
pub mod debounce;
pub mod media;
pub mod pipeline;
pub mod sync;

/// Scheduling priority for embedding jobs (lower value runs first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    /// Notes being typed in a live meeting: searchable within seconds.
    Live = 0,
    /// Edits after the meeting, new transcripts, minutes.
    Interactive = 1,
    /// Audio/video windows and re-index after a model change; runs when idle.
    Backfill = 2,
}

/// Model stamp stored with every vector; a different stamp means "re-embed".
///
/// Params: `model` – model id; `dims` – stored dimensions; `prompt_rev` – task-prompt revision.
/// Returns: e.g. `embeddinggemma-2@768/p1`.
pub fn model_stamp(model: &str, dims: usize, prompt_rev: u32) -> String {
    format!("{model}@{dims}/p{prompt_rev}")
}
