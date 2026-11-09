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

/// Which backend the user picked for minutes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MinutesBackend {
    /// API key → Codex → local model, the first one available.
    #[default]
    Auto,
    /// The BYOK provider whose key is in the Keychain.
    ApiKey,
    /// The signed-in Codex CLI (ChatGPT account).
    Codex,
    /// The signed-in Antigravity CLI (`agy`).
    Antigravity,
}

/// A model a CLI offers, as shown in settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelOption {
    /// Id passed back to the CLI (`--model`, `thread/start.model`).
    pub id: String,
    /// Display name reported by the CLI.
    pub label: String,
    /// The CLI's own default.
    pub is_default: bool,
}

/// What the user configured plus what the environment offers.
#[derive(Debug, Clone, Default)]
pub struct ProviderConfig {
    /// Backend the user picked; the others still serve as the fallback chain.
    pub backend: MinutesBackend,
    /// Configured BYOK provider; only honoured when `byok_key_present` is true.
    pub byok: Option<ByokProvider>,
    /// Whether a key exists in the Keychain for `byok` (the key itself never leaves the Keychain layer).
    pub byok_key_present: bool,
    /// Path to a `codex` binary that answered `account/read` as signed in.
    pub codex_signed_in: Option<String>,
    /// Whether the automatic chain may use Codex (an explicit Codex pick always may).
    pub codex_in_auto: bool,
    /// `thread/start.model` for Codex; `None` keeps Codex's own default.
    pub codex_model: Option<String>,
    /// Path to the `agy` binary, when installed.
    pub agy: Option<String>,
    /// `agy --model` id used for minutes.
    pub antigravity_model: String,
    /// Identifier of a downloaded local MLX model usable for minutes.
    pub local_model: Option<String>,
}

/// The provider that will run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderChoice {
    Byok(ByokProvider),
    Codex {
        codex_path: String,
        model: Option<String>,
    },
    Antigravity {
        agy_path: String,
        model: String,
    },
    LocalMlx {
        model: String,
    },
}

/// Providers to try, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderPlan {
    /// Runs first.
    pub primary: ProviderChoice,
    /// Runs when `primary` fails: the automatic chain without the provider that just failed.
    /// Only set for an explicit pick.
    pub fallback: Option<ProviderChoice>,
    /// The explicit pick, when it could not run at all (not configured or not signed in),
    /// so `primary` already comes from the automatic chain.
    pub unavailable: Option<MinutesBackend>,
}

/// Returned when nothing can generate minutes.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProviderError {
    #[error(
        "no minutes provider available: add an API key, sign in to Codex with ChatGPT (`codex login`), or download a local model"
    )]
    NoneAvailable,
}

/// The explicitly picked provider, when it can run.
///
/// Params: `cfg`.
/// Returns: the choice, or `None` for [`MinutesBackend::Auto`] or a pick that is not set up.
fn explicit(cfg: &ProviderConfig) -> Option<ProviderChoice> {
    match cfg.backend {
        MinutesBackend::Auto => None,
        MinutesBackend::ApiKey => cfg
            .byok
            .clone()
            .filter(|_| cfg.byok_key_present)
            .map(ProviderChoice::Byok),
        MinutesBackend::Codex => cfg.codex_signed_in.clone().map(|p| ProviderChoice::Codex {
            codex_path: p,
            model: cfg.codex_model.clone(),
        }),
        MinutesBackend::Antigravity => cfg.agy.clone().map(|p| ProviderChoice::Antigravity {
            agy_path: p,
            model: cfg.antigravity_model.clone(),
        }),
    }
}

/// Applies the automatic BYOK → Codex → local policy.
///
/// Params: `cfg`; `skip` – a backend that already failed, so the chain does not retry it.
/// Returns: the first available provider.
fn auto_chain(cfg: &ProviderConfig, skip: MinutesBackend) -> Option<ProviderChoice> {
    if skip != MinutesBackend::ApiKey
        && let (Some(p), true) = (&cfg.byok, cfg.byok_key_present)
    {
        return Some(ProviderChoice::Byok(p.clone()));
    }
    if skip != MinutesBackend::Codex
        && cfg.codex_in_auto
        && let Some(path) = &cfg.codex_signed_in
    {
        return Some(ProviderChoice::Codex {
            codex_path: path.clone(),
            model: cfg.codex_model.clone(),
        });
    }
    cfg.local_model
        .as_ref()
        .map(|model| ProviderChoice::LocalMlx {
            model: model.clone(),
        })
}

/// Picks the providers for one minutes run: the explicit pick first, then the automatic chain.
///
/// Params: `cfg` – user configuration and detected environment.
/// Returns: the plan, or [`ProviderError::NoneAvailable`] when nothing can run.
pub fn plan_providers(cfg: &ProviderConfig) -> Result<ProviderPlan, ProviderError> {
    match explicit(cfg) {
        Some(primary) => Ok(ProviderPlan {
            primary,
            fallback: auto_chain(cfg, cfg.backend),
            unavailable: None,
        }),
        None => Ok(ProviderPlan {
            primary: auto_chain(cfg, MinutesBackend::Auto).ok_or(ProviderError::NoneAvailable)?,
            fallback: None,
            unavailable: (cfg.backend != MinutesBackend::Auto).then_some(cfg.backend),
        }),
    }
}

/// The first provider of [`plan_providers`].
///
/// Params: `cfg`.
/// Returns: the chosen provider or [`ProviderError::NoneAvailable`].
pub fn select_provider(cfg: &ProviderConfig) -> Result<ProviderChoice, ProviderError> {
    plan_providers(cfg).map(|p| p.primary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codex_cfg() -> ProviderConfig {
        ProviderConfig {
            codex_signed_in: Some("/opt/homebrew/bin/codex".into()),
            codex_in_auto: true,
            ..Default::default()
        }
    }

    #[test]
    fn byok_wins_only_with_a_key() {
        let mut cfg = ProviderConfig {
            byok: Some(ByokProvider::Anthropic {
                model: "claude-sonnet-5-5".into(),
            }),
            byok_key_present: false,
            ..codex_cfg()
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

    #[test]
    fn explicit_codex_ignores_the_auto_switch_and_carries_its_model() {
        let cfg = ProviderConfig {
            backend: MinutesBackend::Codex,
            codex_in_auto: false,
            codex_model: Some("gpt-6.1-terra".into()),
            local_model: Some("/m".into()),
            ..codex_cfg()
        };
        let plan = plan_providers(&cfg).unwrap();
        assert_eq!(
            plan.primary,
            ProviderChoice::Codex {
                codex_path: "/opt/homebrew/bin/codex".into(),
                model: Some("gpt-6.1-terra".into()),
            }
        );
        // The fallback never retries the provider that just failed.
        assert_eq!(
            plan.fallback,
            Some(ProviderChoice::LocalMlx { model: "/m".into() })
        );
        assert_eq!(plan.unavailable, None);
    }

    #[test]
    fn explicit_antigravity_falls_back_to_the_auto_chain() {
        let mut cfg = ProviderConfig {
            backend: MinutesBackend::Antigravity,
            agy: Some("/u/.local/bin/agy".into()),
            antigravity_model: "claude-opus-5-5-high".into(),
            ..codex_cfg()
        };
        let plan = plan_providers(&cfg).unwrap();
        assert_eq!(
            plan.primary,
            ProviderChoice::Antigravity {
                agy_path: "/u/.local/bin/agy".into(),
                model: "claude-opus-5-5-high".into(),
            }
        );
        assert!(matches!(plan.fallback, Some(ProviderChoice::Codex { .. })));

        // agy missing: the auto chain runs and the plan says why.
        cfg.agy = None;
        let plan = plan_providers(&cfg).unwrap();
        assert!(matches!(plan.primary, ProviderChoice::Codex { .. }));
        assert_eq!(plan.fallback, None);
        assert_eq!(plan.unavailable, Some(MinutesBackend::Antigravity));
    }

    #[test]
    fn explicit_api_key_needs_a_key() {
        let cfg = ProviderConfig {
            backend: MinutesBackend::ApiKey,
            byok: Some(ByokProvider::OpenAi {
                model: "gpt-6".into(),
            }),
            byok_key_present: false,
            ..codex_cfg()
        };
        let plan = plan_providers(&cfg).unwrap();
        assert!(matches!(plan.primary, ProviderChoice::Codex { .. }));
        assert_eq!(plan.unavailable, Some(MinutesBackend::ApiKey));
    }
}
