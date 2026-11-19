//! Pricing catalog and cost estimation for OpenAI and Google Gemini speech transcription.

/// Cloud transcription provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AsrProvider {
    OpenAi,
    Gemini,
}

impl AsrProvider {
    /// Wire/database identifier (`"openai"` or `"gemini"`).
    ///
    /// Returns: stable lowercase string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Gemini => "gemini",
        }
    }

    /// Parses a wire/database provider name.
    ///
    /// Params: `raw` – `"openai"` or `"gemini"` (case-insensitive).
    /// Returns: the provider, if recognized.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "openai" => Some(Self::OpenAi),
            "gemini" | "google" => Some(Self::Gemini),
            _ => None,
        }
    }
}

/// Transcription pipeline mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AsrMode {
    Live,
    Refine,
}

impl AsrMode {
    /// Wire/database identifier (`"live"` or `"refine"`).
    ///
    /// Returns: stable lowercase string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Refine => "refine",
        }
    }
}

/// Pricing specification for one transcription model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AsrModelSpec {
    pub id: &'static str,
    pub provider: AsrProvider,
    pub mode: AsrMode,
    /// Approximate (or exact flat) USD cost per audio minute.
    pub usd_per_minute: f64,
    /// Optional token-based audio input rate (USD per 1M input tokens).
    pub input_audio_usd_per_1m: Option<f64>,
    /// Optional token-based text output rate (USD per 1M output tokens).
    pub output_text_usd_per_1m: Option<f64>,
}

/// Dynamically fetched transcription models for one cloud provider, split by mode.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AsrModelList {
    pub live: Vec<crate::ModelOption>,
    pub refine: Vec<crate::ModelOption>,
}

impl AsrModelList {
    /// Models available for the given transcription `mode`.
    ///
    /// Params: `mode`.
    /// Returns: slice of fetched model options.
    pub fn for_mode(&self, mode: AsrMode) -> &[crate::ModelOption] {
        match mode {
            AsrMode::Live => &self.live,
            AsrMode::Refine => &self.refine,
        }
    }
}

pub const DEFAULT_OPENAI_LIVE_MODEL: &str = "gpt-live-transcribe";
pub const DEFAULT_OPENAI_REFINE_MODEL: &str = "gpt-transcribe";
pub const DEFAULT_GEMINI_LIVE_MODEL: &str = "gemini-3.5-transcribe-live";
pub const DEFAULT_GEMINI_REFINE_MODEL: &str = "gemini-3.5-transcribe";

const CATALOG: &[AsrModelSpec] = &[
    // OpenAI Live
    AsrModelSpec {
        id: "gpt-live-transcribe",
        provider: AsrProvider::OpenAi,
        mode: AsrMode::Live,
        usd_per_minute: 0.017,
        input_audio_usd_per_1m: None,
        output_text_usd_per_1m: None,
    },
    AsrModelSpec {
        id: "gpt-4o-transcribe",
        provider: AsrProvider::OpenAi,
        mode: AsrMode::Live,
        usd_per_minute: 0.006,
        input_audio_usd_per_1m: Some(2.50),
        output_text_usd_per_1m: Some(10.00),
    },
    AsrModelSpec {
        id: "gpt-4o-mini-transcribe",
        provider: AsrProvider::OpenAi,
        mode: AsrMode::Live,
        usd_per_minute: 0.003,
        input_audio_usd_per_1m: Some(1.25),
        output_text_usd_per_1m: Some(5.00),
    },
    // OpenAI Refine
    AsrModelSpec {
        id: "gpt-transcribe",
        provider: AsrProvider::OpenAi,
        mode: AsrMode::Refine,
        usd_per_minute: 0.0045,
        input_audio_usd_per_1m: None,
        output_text_usd_per_1m: None,
    },
    AsrModelSpec {
        id: "gpt-4o-transcribe-diarize",
        provider: AsrProvider::OpenAi,
        mode: AsrMode::Refine,
        usd_per_minute: 0.006,
        input_audio_usd_per_1m: Some(2.50),
        output_text_usd_per_1m: Some(10.00),
    },
    AsrModelSpec {
        id: "gpt-4o-transcribe",
        provider: AsrProvider::OpenAi,
        mode: AsrMode::Refine,
        usd_per_minute: 0.006,
        input_audio_usd_per_1m: Some(2.50),
        output_text_usd_per_1m: Some(10.00),
    },
    AsrModelSpec {
        id: "gpt-4o-mini-transcribe",
        provider: AsrProvider::OpenAi,
        mode: AsrMode::Refine,
        usd_per_minute: 0.003,
        input_audio_usd_per_1m: Some(1.25),
        output_text_usd_per_1m: Some(5.00),
    },
    AsrModelSpec {
        id: "whisper-1",
        provider: AsrProvider::OpenAi,
        mode: AsrMode::Refine,
        usd_per_minute: 0.006,
        input_audio_usd_per_1m: None,
        output_text_usd_per_1m: None,
    },
    // Gemini Live
    AsrModelSpec {
        id: "gemini-3.5-transcribe-live",
        provider: AsrProvider::Gemini,
        mode: AsrMode::Live,
        usd_per_minute: 0.009,
        input_audio_usd_per_1m: Some(3.50),
        output_text_usd_per_1m: Some(21.00),
    },
    // Gemini Refine
    AsrModelSpec {
        id: "gemini-3.5-transcribe",
        provider: AsrProvider::Gemini,
        mode: AsrMode::Refine,
        usd_per_minute: 0.005,
        input_audio_usd_per_1m: Some(2.00),
        output_text_usd_per_1m: Some(12.00),
    },
    AsrModelSpec {
        id: "gemini-3.1-flash-lite",
        provider: AsrProvider::Gemini,
        mode: AsrMode::Refine,
        usd_per_minute: 0.001,
        input_audio_usd_per_1m: Some(0.50),
        output_text_usd_per_1m: Some(1.50),
    },
    AsrModelSpec {
        id: "gemini-3.8-flash",
        provider: AsrProvider::Gemini,
        mode: AsrMode::Refine,
        usd_per_minute: 0.0018,
        input_audio_usd_per_1m: Some(0.75),
        output_text_usd_per_1m: Some(3.75),
    },
    AsrModelSpec {
        id: "gemini-2.5-flash",
        provider: AsrProvider::Gemini,
        mode: AsrMode::Refine,
        usd_per_minute: 0.002,
        input_audio_usd_per_1m: Some(1.00),
        output_text_usd_per_1m: Some(2.50),
    },
];

/// Default model identifier for `(provider, mode)`.
///
/// Params: `provider`; `mode`.
/// Returns: model id string.
pub fn default_model(provider: AsrProvider, mode: AsrMode) -> &'static str {
    match (provider, mode) {
        (AsrProvider::OpenAi, AsrMode::Live) => DEFAULT_OPENAI_LIVE_MODEL,
        (AsrProvider::OpenAi, AsrMode::Refine) => DEFAULT_OPENAI_REFINE_MODEL,
        (AsrProvider::Gemini, AsrMode::Live) => DEFAULT_GEMINI_LIVE_MODEL,
        (AsrProvider::Gemini, AsrMode::Refine) => DEFAULT_GEMINI_REFINE_MODEL,
    }
}

/// Preset models for `(provider, mode)`, in UI display order.
///
/// Params: `provider`; `mode`.
/// Returns: matching pricing specs.
pub fn models_for(provider: AsrProvider, mode: AsrMode) -> Vec<AsrModelSpec> {
    CATALOG
        .iter()
        .copied()
        .filter(|s| s.provider == provider && s.mode == mode)
        .collect()
}

/// Looks up known pricing for `model` (exact id or dated `<id>-YYYY-MM-DD` snapshot prefix).
///
/// Params: `provider`; `mode`; `model` – configured id.
/// Returns: matching specification, or `None` when the model is not in the catalog.
pub fn known_model_spec(provider: AsrProvider, mode: AsrMode, model: &str) -> Option<AsrModelSpec> {
    let clean = model.trim();
    if clean.is_empty() {
        return None;
    }
    if let Some(exact) = CATALOG
        .iter()
        .copied()
        .find(|s| s.provider == provider && s.mode == mode && s.id.eq_ignore_ascii_case(clean))
    {
        return Some(exact);
    }
    let lower = clean.to_ascii_lowercase();
    CATALOG
        .iter()
        .copied()
        .filter(|s| {
            s.provider == provider
                && s.mode == mode
                && lower.starts_with(&format!("{}-", s.id.to_ascii_lowercase()))
        })
        .max_by_key(|s| s.id.len())
}

/// Looks up pricing for `model`, falling back to the provider/mode default for custom ids.
///
/// Params: `provider`; `mode`; `model` – configured id or empty string.
/// Returns: matching or fallback pricing specification.
pub fn model_spec(provider: AsrProvider, mode: AsrMode, model: &str) -> AsrModelSpec {
    if let Some(found) = known_model_spec(provider, mode, model) {
        return found;
    }
    let fallback = default_model(provider, mode);
    CATALOG
        .iter()
        .copied()
        .find(|s| s.provider == provider && s.mode == mode && s.id == fallback)
        .expect("default model always exists in CATALOG")
}

/// Estimates USD cost for one transcription call.
///
/// Uses exact token pricing when `input_tokens > 0` and the model specifies token rates;
/// otherwise falls back to audio duration multiplied by `usd_per_minute`.
///
/// Params: `provider`; `mode`; `model`; `audio_ms`; `input_tokens`; `output_tokens`.
/// Returns: estimated USD cost (non-negative).
pub fn estimate_cost_usd(
    provider: AsrProvider,
    mode: AsrMode,
    model: &str,
    audio_ms: u64,
    input_tokens: u64,
    output_tokens: u64,
) -> f64 {
    let spec = model_spec(provider, mode, model);
    if input_tokens > 0
        && let Some(in_rate) = spec.input_audio_usd_per_1m
    {
        let out_rate = spec.output_text_usd_per_1m.unwrap_or(0.0);
        return (input_tokens as f64 / 1_000_000.0) * in_rate
            + (output_tokens as f64 / 1_000_000.0) * out_rate;
    }
    (audio_ms as f64 / 60_000.0) * spec.usd_per_minute
}

/// Reference minutes purchasable for `usd` at the model's per-minute rate.
///
/// Params: `provider`; `mode`; `model`; `usd`.
/// Returns: estimated audio minutes (0.0 when `usd <= 0`).
pub fn minutes_for_usd(provider: AsrProvider, mode: AsrMode, model: &str, usd: f64) -> f64 {
    if !usd.is_finite() || usd <= 0.0 {
        return 0.0;
    }
    let rate = model_spec(provider, mode, model).usd_per_minute;
    if rate <= 0.0 { 0.0 } else { usd / rate }
}

/// Whether an OpenAI transcription model returns segment-level timestamps natively.
///
/// `gpt-4o-transcribe-diarize` (`diarized_json`) and `whisper-1` (`verbose_json`) emit
/// timestamps; `gpt-transcribe`, `gpt-4o-transcribe` and `gpt-4o-mini-transcribe` emit flat
/// text only, so callers split long recordings by speech pauses first.
///
/// Params: `model`.
/// Returns: `true` when the API response carries per-segment timestamps.
pub fn openai_model_has_timestamps(model: &str) -> bool {
    let lower = model.trim().to_ascii_lowercase();
    lower.contains("diarize") || lower.starts_with("whisper")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_presets_cover_every_combination() {
        for (provider, mode) in [
            (AsrProvider::OpenAi, AsrMode::Live),
            (AsrProvider::OpenAi, AsrMode::Refine),
            (AsrProvider::Gemini, AsrMode::Live),
            (AsrProvider::Gemini, AsrMode::Refine),
        ] {
            let list = models_for(provider, mode);
            assert!(!list.is_empty());
            assert_eq!(list[0].id, default_model(provider, mode));
        }
    }

    #[test]
    fn estimate_prefers_token_billing_when_tokens_and_rates_exist() {
        // Gemini 3.5 Transcribe: 1,500 input tokens @ $2/1M + 200 output tokens @ $12/1M
        let token_cost = estimate_cost_usd(
            AsrProvider::Gemini,
            AsrMode::Refine,
            "gemini-3.5-transcribe",
            60_000,
            1_500,
            200,
        );
        assert!((token_cost - 0.0054).abs() < 1e-9);

        // Without tokens, falls back to per-minute duration estimate.
        let dur_cost = estimate_cost_usd(
            AsrProvider::Gemini,
            AsrMode::Refine,
            "gemini-3.5-transcribe",
            60_000,
            0,
            0,
        );
        assert!((dur_cost - 0.005).abs() < 1e-9);

        // Flat-rate models ignore tokens and always use duration.
        let oa_cost = estimate_cost_usd(
            AsrProvider::OpenAi,
            AsrMode::Refine,
            "gpt-transcribe",
            60_000,
            1_500,
            45,
        );
        assert!((oa_cost - 0.0045).abs() < 1e-9);
    }

    #[test]
    fn unknown_model_falls_back_and_reference_minutes_match_rates() {
        let spec = model_spec(AsrProvider::OpenAi, AsrMode::Refine, "gpt-custom-future");
        assert_eq!(spec.id, DEFAULT_OPENAI_REFINE_MODEL);

        let mins = minutes_for_usd(
            AsrProvider::OpenAi,
            AsrMode::Refine,
            "gpt-transcribe",
            100.0,
        );
        assert!((mins - (100.0 / 0.0045)).abs() < 1e-3);
        assert_eq!(
            minutes_for_usd(AsrProvider::Gemini, AsrMode::Live, "", -5.0),
            0.0
        );
    }

    #[test]
    fn timestamp_capability_matches_openai_models() {
        assert!(openai_model_has_timestamps("gpt-4o-transcribe-diarize"));
        assert!(openai_model_has_timestamps(
            "gpt-4o-transcribe-diarize-2026-01-01"
        ));
        assert!(openai_model_has_timestamps("whisper-1"));
        assert!(openai_model_has_timestamps("whisper-2"));
        assert!(!openai_model_has_timestamps("gpt-transcribe"));
        assert!(!openai_model_has_timestamps("gpt-4o-transcribe"));
        assert!(!openai_model_has_timestamps("gpt-4o-mini-transcribe"));
    }

    #[test]
    fn known_model_spec_matches_exact_and_dated_snapshots() {
        assert_eq!(
            known_model_spec(AsrProvider::OpenAi, AsrMode::Refine, ""),
            None
        );
        assert_eq!(
            known_model_spec(AsrProvider::OpenAi, AsrMode::Refine, "gpt-9-transcribe"),
            None
        );
        let mini = known_model_spec(
            AsrProvider::OpenAi,
            AsrMode::Refine,
            "gpt-4o-mini-transcribe-2025-12-15",
        )
        .expect("dated snapshot matches base spec");
        assert_eq!(mini.id, "gpt-4o-mini-transcribe");

        let diarize = known_model_spec(
            AsrProvider::OpenAi,
            AsrMode::Refine,
            "gpt-4o-transcribe-diarize-2026-02-01",
        )
        .expect("longest prefix wins over gpt-4o-transcribe");
        assert_eq!(diarize.id, "gpt-4o-transcribe-diarize");
    }
}
