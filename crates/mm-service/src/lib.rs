//! Headless application services for Meeting Master.
//!
//! The GPUI app owns windows and rendering; everything it *does* lives here so it can
//! be tested without a display or engines:
//!
//! * [`config`] – settings persisted in `~/Library/Application Support/Meeting Master`.
//! * [`engines`] – locating and calling `mm-meetingd` / `mm-embedd` with typed results.
//! * [`live`] – live caption board and speaker names/hues.
//! * [`flow`] – note → recording → transcript → minutes steps of one meeting.
//! * [`indexer`] – vault watcher, debouncing and background embedding.
//! * [`hub`] – downloading model checkpoints from Hugging Face (token in the Keychain).

pub mod config;
pub mod engines;
pub mod flow;
pub mod hub;
pub mod indexer;
pub mod live;

pub use config::{AppConfig, data_dir, models_dir};

/// Errors surfaced to the UI. Messages carry the path, method or value involved.
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("设置：{0}")]
    Config(String),
    #[error("引擎：{0}")]
    Engine(String),
    /// A macOS privacy permission is missing; the UI links to System Settings.
    #[error("{0}")]
    Permission(String),
    #[error("Obsidian：{0}")]
    Vault(String),
    #[error("索引：{0}")]
    Index(String),
    #[error(transparent)]
    Store(#[from] mm_store::StoreError),
    #[error(transparent)]
    VaultOp(#[from] mm_vault::VaultError),
    #[error(transparent)]
    Indexing(#[from] mm_index::pipeline::IndexError),
    #[error(transparent)]
    Llm(#[from] mm_llm::LlmError),
    #[error(transparent)]
    Hub(#[from] hub::HubError),
}
