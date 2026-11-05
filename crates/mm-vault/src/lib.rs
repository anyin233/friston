//! Obsidian integration.
//!
//! The vault is the system of record for human-readable notes and minutes:
//! Meeting Master writes plain Markdown files into it (so everything works while
//! Obsidian is closed) and uses `obsidian://` URIs to open them in Obsidian.
//! The optional Obsidian CLI (1.12+, requires the app running) is only used for
//! conveniences such as revealing a note.
//!
//! Notes are shared with the user: they keep typing in Obsidian after the
//! meeting. Generated content is therefore confined to *managed blocks*
//! delimited by HTML comments, and only those blocks are ever rewritten.

pub mod discover;
pub mod note;
pub mod uri;

pub use discover::{Vault, discover_vaults};
pub use note::{MeetingNote, replace_managed_block, write_atomic};
pub use uri::open_note_uri;

/// Errors raised by vault operations.
#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error("reading Obsidian config {path}: {source}")]
    Config {
        path: String,
        source: std::io::Error,
    },
    #[error("parsing Obsidian config {path}: {source}")]
    ConfigJson {
        path: String,
        source: serde_json::Error,
    },
    #[error("writing note {path}: {source}")]
    Write {
        path: String,
        source: std::io::Error,
    },
    #[error("managed block `{0}` is unbalanced (start without end or vice versa)")]
    UnbalancedBlock(String),
}
