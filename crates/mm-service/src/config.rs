//! User settings, persisted as JSON in the app's support folder.

use std::path::{Path, PathBuf};

use mm_i18n::Lang;
use mm_llm::{ByokProvider, MinutesBackend};
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
    /// Overrides `~/Movies/Friston` ([`default_recordings_dir`]).
    pub recordings_dir: Option<PathBuf>,
    /// Local EmbeddingGemma 2 checkpoint folder.
    pub embedding_model_dir: Option<PathBuf>,
    /// Hugging Face repo the 下载 button fetches the embedding model from.
    pub embedding_model_url: String,
    /// With no `embedding_model_dir`, load `embedding_model_url` at launch: from the app's
    /// models folder or the Hugging Face cache when present, otherwise by downloading it.
    /// On by default so semantic search works without any setup.
    pub auto_download_embedding: bool,
    /// Local MLX instruct model folder for offline minutes (last resort provider).
    pub local_llm_dir: Option<PathBuf>,
    /// Configured BYOK provider (its key lives in the Keychain).
    pub byok: Option<ByokProvider>,
    /// Allow the automatic minutes chain to use the local Codex (ChatGPT sign-in).
    pub use_codex: bool,
    /// Who writes minutes; an explicit pick that fails falls back to the automatic chain.
    pub minutes_backend: MinutesBackend,
    /// Codex model for minutes (`model/list` id); `None` keeps Codex's default.
    pub codex_model: Option<String>,
    /// `agy --model` id for minutes through Antigravity. When it equals `antigravity_model`
    /// and the Antigravity refine runs, one agy run does both jobs.
    pub antigravity_minutes_model: String,
    /// Re-transcribe recordings after the meeting with the larger model.
    pub refine_after_meeting: bool,
    /// Qwen3-ASR checkpoint for the post-meeting pass (Hugging Face repo id or absolute
    /// folder); `None` uses the engine default, Qwen3-ASR 1.7B 8bit. Live captions keep their
    /// own fast model, so a stronger refine model costs nothing during the meeting.
    pub refine_asr_model: Option<String>,
    /// Who does the post-meeting pass: the local Qwen3-ASR engine, or Gemini through the
    /// Antigravity CLI.
    pub refine_backend: RefineBackend,
    /// `agy --model` id for [`RefineBackend::Antigravity`] (must hear audio).
    pub antigravity_model: String,
    /// Generate minutes automatically when a meeting ends.
    pub auto_minutes: bool,
    /// Show the always-on-top caption window during meetings.
    pub floating_captions: bool,
    /// Minutes language (`zh`, `en`, `ja`).
    pub minutes_language: String,
    /// Interface language; `None` follows the macOS preferred language
    /// ([`mm_i18n::system_lang`]). Also used for the text the app writes into notes.
    pub ui_language: Option<Lang>,
    /// Mirror the microphone's direction cues (its first channel is on the right).
    pub spatial_flip: bool,
    /// Distance between the two spatial mics in centimetres; `None` = unknown geometry.
    pub spatial_spacing_cm: Option<f64>,
}

impl AppConfig {
    /// The interface language this config asks for.
    ///
    /// Returns: `ui_language`, or the system language when it is unset.
    pub fn effective_ui_language(&self) -> Lang {
        self.ui_language.unwrap_or_else(mm_i18n::system_lang)
    }
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
            embedding_model_url: crate::hub::DEFAULT_EMBEDDING_REPO_URL.into(),
            auto_download_embedding: true,
            local_llm_dir: None,
            byok: None,
            use_codex: true,
            minutes_backend: MinutesBackend::Auto,
            codex_model: None,
            antigravity_minutes_model: mm_llm::antigravity::DEFAULT_MODEL.into(),
            refine_after_meeting: true,
            refine_asr_model: None,
            refine_backend: RefineBackend::Local,
            antigravity_model: mm_llm::antigravity::DEFAULT_MODEL.into(),
            auto_minutes: true,
            floating_captions: true,
            minutes_language: "zh".into(),
            ui_language: None,
            spatial_flip: false,
            spatial_spacing_cm: None,
        }
    }
}

/// Engine for the post-meeting re-transcription.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefineBackend {
    /// Qwen3-ASR in `mm-meetingd`.
    #[default]
    Local,
    /// Gemini hears the recordings via `agy`. Falls back to [`RefineBackend::Local`] when it fails.
    Antigravity,
}

/// Folder name used for the app's data and recordings.
pub const APP_DIR_NAME: &str = "Friston";

/// Whether `MM_DATA_DIR` redirects the data folder (tests, sandboxes).
///
/// Returns: `true` when the override is set; legacy migration then stays out of the way.
pub fn data_dir_overridden() -> bool {
    std::env::var_os("MM_DATA_DIR").is_some()
}

/// Folder holding `config.json` and the database.
///
/// Returns: `MM_DATA_DIR` if set (tests, sandboxes), else `~/Library/Application Support/Friston`.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("MM_DATA_DIR") {
        return PathBuf::from(dir);
    }
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_DIR_NAME)
}

/// Where the engine writes recordings when `recordings_dir` is unset.
///
/// Must match `CaptureLayout`'s default in `native/NativeKit/Sources/MeetingCore/CapturePlan.swift`.
/// Returns: `~/Movies/Friston`.
pub fn default_recordings_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Movies")
        .join(APP_DIR_NAME)
}

/// Folder downloaded models are written to.
///
/// Params: `data_dir` – the app's data folder.
/// Returns: `<data_dir>/models`.
pub fn models_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("models")
}

impl AppConfig {
    /// Loads `config.json` from `dir`, returning defaults when the file does not exist.
    ///
    /// Params: `dir` – data folder.
    /// Returns: the config.
    pub fn load(dir: &Path) -> Result<Self, ServiceError> {
        let path = dir.join("config.json");
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text)
                .map_err(|e| ServiceError::Config(format!("parsing {}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(ServiceError::Config(format!(
                "reading {}: {e}",
                path.display()
            ))),
        }
    }

    /// Parses `config.json` text, carrying older settings forward.
    ///
    /// Before `minutes_backend` existed, the Antigravity refine also wrote the minutes; a
    /// config from then keeps that behaviour by picking Antigravity for minutes too.
    ///
    /// Params: `text` – file contents.
    /// Returns: the config.
    pub fn parse(text: &str) -> Result<Self, serde_json::Error> {
        let raw: serde_json::Value = serde_json::from_str(text)?;
        let legacy_antigravity =
            raw.get("minutes_backend").is_none() && raw["refine_backend"] == "antigravity";
        let mut cfg: Self = serde_json::from_value(raw)?;
        if legacy_antigravity {
            cfg.minutes_backend = MinutesBackend::Antigravity;
            cfg.antigravity_minutes_model = cfg.antigravity_model.clone();
        }
        Ok(cfg)
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

    /// Points folder settings that lived inside `old` at the same place inside `new`.
    ///
    /// Used after the data folder moved (models downloaded in-app live under it).
    /// Params: `old` – previous folder; `new` – its new location.
    /// Returns: `true` if any setting changed (the caller then saves).
    pub fn rebase_paths(&mut self, old: &Path, new: &Path) -> bool {
        let mut changed = false;
        for slot in [
            &mut self.recordings_dir,
            &mut self.embedding_model_dir,
            &mut self.local_llm_dir,
        ] {
            if let Some(p) = slot.as_ref()
                && let Ok(rest) = p.strip_prefix(old)
            {
                *slot = Some(new.join(rest));
                changed = true;
            }
        }
        changed
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
    #[test]
    fn rebase_paths_only_touches_folders_inside_the_old_one() {
        let old = std::path::Path::new("/u/AS/Meeting Master");
        let new = std::path::Path::new("/u/AS/Friston");
        let mut cfg = super::AppConfig {
            embedding_model_dir: Some(old.join("models/google--embeddinggemma")),
            local_llm_dir: Some("/u/AS/Meeting Master Old/llm".into()),
            ..Default::default()
        };

        assert!(cfg.rebase_paths(old, new));

        assert_eq!(
            cfg.embedding_model_dir,
            Some(new.join("models/google--embeddinggemma"))
        );
        assert_eq!(
            cfg.local_llm_dir,
            Some("/u/AS/Meeting Master Old/llm".into())
        );
        assert!(!cfg.rebase_paths(old, new));
    }

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
        cfg.minutes_backend = MinutesBackend::Codex;
        cfg.codex_model = Some("gpt-6.1-terra".into());
        cfg.save(dir.path()).unwrap();
        assert_eq!(AppConfig::load(dir.path()).unwrap(), cfg);
    }

    #[test]
    fn legacy_antigravity_refine_keeps_writing_minutes() {
        let old = r#"{"refine_backend":"antigravity","antigravity_model":"gemini-3.8-flash-high"}"#;
        let cfg = AppConfig::parse(old).unwrap();
        assert_eq!(cfg.minutes_backend, MinutesBackend::Antigravity);
        assert_eq!(cfg.antigravity_minutes_model, "gemini-3.8-flash-high");

        // Once the key exists, the user's own pick wins.
        let new = r#"{"refine_backend":"antigravity","minutes_backend":"auto"}"#;
        assert_eq!(
            AppConfig::parse(new).unwrap().minutes_backend,
            MinutesBackend::Auto
        );
        assert_eq!(AppConfig::parse("{}").unwrap(), AppConfig::default());
    }

    #[test]
    fn ui_language_round_trips_and_defaults_to_system() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.json"), r#"{"fps": 30}"#).unwrap();
        let mut cfg = AppConfig::load(dir.path()).unwrap();
        assert_eq!(
            cfg.ui_language, None,
            "older files follow the system language"
        );
        assert_eq!(cfg.effective_ui_language(), mm_i18n::system_lang());
        cfg.ui_language = Some(Lang::En);
        cfg.save(dir.path()).unwrap();
        let raw = std::fs::read_to_string(dir.path().join("config.json")).unwrap();
        assert!(raw.contains(r#""ui_language": "en""#), "{raw}");
        let back = AppConfig::load(dir.path()).unwrap();
        assert_eq!(back.effective_ui_language(), Lang::En);
    }

    #[test]
    fn partial_files_keep_defaults_for_new_keys() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.json"), r#"{"fps": 30}"#).unwrap();
        let cfg = AppConfig::load(dir.path()).unwrap();
        assert_eq!(cfg.fps, 30);
        assert!(cfg.system_audio);
        assert!(
            !cfg.spatial_flip,
            "files from before the spatial settings load"
        );
        assert_eq!(cfg.spatial_spacing_cm, None);
        assert_eq!(
            cfg.embedding_model_url,
            crate::hub::DEFAULT_EMBEDDING_REPO_URL,
            "files from before the download setting get the default repo"
        );
        assert!(
            cfg.auto_download_embedding,
            "files from before the setting fetch the default model"
        );
        std::fs::write(dir.path().join("config.json"), "{").unwrap();
        assert!(
            AppConfig::load(dir.path())
                .unwrap_err()
                .to_string()
                .contains("config.json")
        );
    }
}
