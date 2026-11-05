//! User settings, persisted as JSON in the app's support folder.

use std::path::{Path, PathBuf};

use mm_llm::ByokProvider;
use serde::{Deserialize, Serialize};

use crate::ServiceError;

/// Everything the settings page edits. Unknown/missing keys fall back to defaults so
/// older config files keep loading after upgrades.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// Obsidian vault folder; `None` until the user picks one (or we discover one).
    pub vault_path: Option<PathBuf>,
    /// Folder inside the vault for meeting notes.
    pub meetings_folder: String,
    /// Core Audio UID of the microphone; `None` uses the system default input.
    pub microphone_uid: Option<String>,
    /// Record what the Mac plays (remote participants).
    pub system_audio: bool,
    /// Record the meeting window/display.
    pub record_screen: bool,
    /// Caption language hint (`zh`, `en`, …); `None` auto-detects.
    pub caption_language: Option<String>,
    /// Screen frame rate.
    pub fps: u32,
    /// Overrides `~/Movies/Meeting Master`.
    pub recordings_dir: Option<PathBuf>,
    /// Local EmbeddingGemma 2 checkpoint folder.
    pub embedding_model_dir: Option<PathBuf>,
    /// Configured BYOK provider (its key lives in the Keychain).
    pub byok: Option<ByokProvider>,
    /// Allow minutes through the local Codex (ChatGPT sign-in).
    pub use_codex: bool,
    /// Re-transcribe recordings after the meeting with the larger model.
    pub refine_after_meeting: bool,
    /// Generate minutes automatically when a meeting ends.
    pub auto_minutes: bool,
    /// Show the always-on-top caption window during meetings.
    pub floating_captions: bool,
    /// Minutes language (`zh`, `en`, `ja`).
    pub minutes_language: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            vault_path: None,
            meetings_folder: "Meetings".into(),
            microphone_uid: None,
            system_audio: true,
            record_screen: true,
            caption_language: Some("zh".into()),
            fps: 15,
            recordings_dir: None,
            embedding_model_dir: None,
            byok: None,
            use_codex: true,
            refine_after_meeting: true,
            auto_minutes: true,
            floating_captions: true,
            minutes_language: "zh".into(),
        }
    }
}

/// Folder holding `config.json` and the database.
///
/// Returns: `MM_DATA_DIR` if set (tests, sandboxes), else `~/Library/Application Support/Meeting Master`.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("MM_DATA_DIR") {
        return PathBuf::from(dir);
    }
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Meeting Master")
}

impl AppConfig {
    /// Loads `config.json` from `dir`, returning defaults when the file does not exist.
    ///
    /// Params: `dir` – data folder.
    /// Returns: the config.
    pub fn load(dir: &Path) -> Result<Self, ServiceError> {
        let path = dir.join("config.json");
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| ServiceError::Config(format!("parsing {}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(ServiceError::Config(format!("reading {}: {e}", path.display()))),
        }
    }

    /// Writes `config.json` atomically.
    ///
    /// Params: `dir` – data folder.
    /// Returns: `Ok(())` once renamed into place.
    pub fn save(&self, dir: &Path) -> Result<(), ServiceError> {
        std::fs::create_dir_all(dir)
            .map_err(|e| ServiceError::Config(format!("creating {}: {e}", dir.display())))?;
        let path = dir.join("config.json");
        let tmp = dir.join("config.json.tmp");
        let text = serde_json::to_string_pretty(self).expect("config always serializes");
        std::fs::write(&tmp, text)
            .and_then(|_| std::fs::rename(&tmp, &path))
            .map_err(|e| ServiceError::Config(format!("writing {}: {e}", path.display())))
    }

    /// Fills in a vault from Obsidian's registry when none is configured yet.
    ///
    /// Returns: `true` if a vault was adopted.
    pub fn adopt_obsidian_vault(&mut self) -> bool {
        if self.vault_path.is_some() {
            return false;
        }
        let Some(cfg) = mm_vault::discover::default_config_path() else {
            return false;
        };
        match mm_vault::discover_vaults(&cfg) {
            Ok(vaults) => {
                self.vault_path = vaults.into_iter().next().map(|v| v.path);
                self.vault_path.is_some()
            }
            Err(e) => {
                tracing::info!("no Obsidian vault discovered: {e}");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_gives_defaults_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = AppConfig::load(dir.path()).unwrap();
        assert_eq!(cfg, AppConfig::default());
        cfg.vault_path = Some("/v".into());
        cfg.byok = Some(ByokProvider::Anthropic {
            model: "claude-opus-5-5".into(),
        });
        cfg.save(dir.path()).unwrap();
        assert_eq!(AppConfig::load(dir.path()).unwrap(), cfg);
    }

    #[test]
    fn partial_files_keep_defaults_for_new_keys() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.json"), r#"{"fps": 30}"#).unwrap();
        let cfg = AppConfig::load(dir.path()).unwrap();
        assert_eq!(cfg.fps, 30);
        assert!(cfg.system_audio);
        std::fs::write(dir.path().join("config.json"), "{").unwrap();
        assert!(AppConfig::load(dir.path()).unwrap_err().to_string().contains("config.json"));
    }
}
