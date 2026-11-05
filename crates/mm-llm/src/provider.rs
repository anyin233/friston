//! Chooses which backend generates minutes.

use serde::{Deserialize, Serialize};

/// BYOK cloud providers the user can configure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ByokProvider {
    OpenAi {
        model: String,
    },
    Anthropic {
        model: String,
    },
    Gemini {
        model: String,
    },
    /// Any OpenAI-compatible endpoint (OpenRouter, a LAN server, …).
    OpenAiCompatible {
        base_url: String,
        model: String,
    },
}

/// What the user configured plus what the environment offers.
#[derive(Debug, Clone, Default)]
pub struct ProviderConfig {
    /// Configured BYOK provider; only honoured when `byok_key_present` is true.
    pub byok: Option<ByokProvider>,
    /// Whether a key exists in the Keychain for `byok` (the key itself never leaves the Keychain layer).
    pub byok_key_present: bool,
    /// Path to a `codex` binary that answered `account/read` as signed in.
    pub codex_signed_in: Option<String>,
    /// Identifier of a downloaded local MLX model usable for minutes.
    pub local_model: Option<String>,
}

/// The provider that will run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderChoice {
    Byok(ByokProvider),
    Codex { codex_path: String },
    LocalMlx { model: String },
}

/// Returned when nothing can generate minutes.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProviderError {
    #[error(
        "no minutes provider available: add an API key, sign in to Codex with ChatGPT (`codex login`), or download a local model"
    )]
    NoneAvailable,
}

/// Applies the BYOK → Codex → local policy.
///
/// Params: `cfg` – user configuration and detected environment.
/// Returns: the chosen provider or [`ProviderError::NoneAvailable`].
pub fn select_provider(cfg: &ProviderConfig) -> Result<ProviderChoice, ProviderError> {
    if let (Some(p), true) = (&cfg.byok, cfg.byok_key_present) {
        return Ok(ProviderChoice::Byok(p.clone()));
    }
    if let Some(path) = &cfg.codex_signed_in {
        return Ok(ProviderChoice::Codex {
            codex_path: path.clone(),
        });
    }
    if let Some(model) = &cfg.local_model {
        return Ok(ProviderChoice::LocalMlx {
            model: model.clone(),
        });
    }
    Err(ProviderError::NoneAvailable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byok_wins_only_with_a_key() {
        let mut cfg = ProviderConfig {
            byok: Some(ByokProvider::Anthropic {
                model: "claude-sonnet-5-5".into(),
            }),
            byok_key_present: false,
            codex_signed_in: Some("/opt/homebrew/bin/codex".into()),
            local_model: None,
        };
        assert!(matches!(
            select_provider(&cfg),
            Ok(ProviderChoice::Codex { .. })
        ));
        cfg.byok_key_present = true;
        assert!(matches!(select_provider(&cfg), Ok(ProviderChoice::Byok(_))));
    }

    #[test]
    fn falls_back_to_local_then_errors() {
        let cfg = ProviderConfig {
            local_model: Some("gemma-4-e4b-4bit".into()),
            ..Default::default()
        };
        assert_eq!(
            select_provider(&cfg),
            Ok(ProviderChoice::LocalMlx {
                model: "gemma-4-e4b-4bit".into()
            })
        );
        assert_eq!(
            select_provider(&ProviderConfig::default()),
            Err(ProviderError::NoneAvailable)
        );
    }
}
