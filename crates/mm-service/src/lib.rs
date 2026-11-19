//! Headless application services for Friston.
//!
//! The GPUI app owns windows and rendering; everything it *does* lives here so it can
//! be tested without a display or engines:
//!
//! * [`config`] – settings persisted in `~/Library/Application Support/Friston`.
//! * [`engines`] – locating and calling `mm-meetingd` / `mm-embedd` with typed results.
//! * [`live`] – live caption board and speaker names/hues.
//! * [`flow`] – note → recording → transcript → minutes steps of one meeting.
//! * [`indexer`] – vault watcher, debouncing and background embedding.
//! * [`external`] – mapping externally transcribed sentences (e.g. Codex unified pass)
//!   back onto this meeting's speakers.
//! * [`hub`] – downloading model checkpoints from Hugging Face (token in the Keychain).

pub mod config;
pub mod engines;
pub mod external;
pub mod flow;
pub mod hub;
pub mod indexer;
pub mod legacy;
pub mod live;

pub use config::{AppConfig, LiveAsrBackend, RefineBackend, data_dir, models_dir};
pub use mm_core::CloudLiveMode;

/// Errors surfaced to the UI. Messages carry the path, method or value involved.
///
/// `Display` is implemented by hand (not with `#[error("…")]`) so the prefix follows the
/// UI language; the transparent variants keep `#[error(transparent)]` for `source()`.
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    Config(String),
    Engine(String),
    /// A macOS privacy permission is missing; the UI links to System Settings.
    Permission(String),
    Vault(String),
    Index(String),
    External(String),
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

impl std::fmt::Display for ServiceError {
    /// Writes the message in the current UI language, prefixed with the area it came from.
    ///
    /// Params: `f` – formatter.
    /// Returns: the formatter's result.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use mm_i18n::trf;
        let text = match self {
            ServiceError::Config(m) => trf!("设置：{m}", "Settings: {m}"),
            ServiceError::Engine(m) => trf!("引擎：{m}", "Engine: {m}"),
            ServiceError::Permission(m) => m.clone(),
            ServiceError::Vault(m) => trf!("Obsidian：{m}", "Obsidian: {m}"),
            ServiceError::Index(m) => trf!("索引：{m}", "Index: {m}"),
            ServiceError::External(m) => trf!("外部精转：{m}", "External refine: {m}"),
            // Transparent: show the wrapped error exactly as it describes itself.
            ServiceError::Store(e) => return std::fmt::Display::fmt(e, f),
            ServiceError::VaultOp(e) => return std::fmt::Display::fmt(e, f),
            ServiceError::Indexing(e) => return std::fmt::Display::fmt(e, f),
            ServiceError::Llm(e) => return std::fmt::Display::fmt(e, f),
            ServiceError::Hub(e) => return std::fmt::Display::fmt(e, f),
        };
        f.write_str(&text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mm_i18n::{Lang, with_lang};

    #[test]
    fn display_follows_the_ui_language() {
        let e = ServiceError::Config("config.json".into());
        assert_eq!(with_lang(Lang::Zh, || e.to_string()), "设置：config.json");
        assert_eq!(
            with_lang(Lang::En, || e.to_string()),
            "Settings: config.json"
        );
        let hub = ServiceError::from(hub::HubError::Cancelled);
        assert_eq!(
            with_lang(Lang::En, || hub.to_string()),
            "Download cancelled"
        );
        assert!(
            std::error::Error::source(&hub).is_none(),
            "transparent keeps source()"
        );
    }
}
