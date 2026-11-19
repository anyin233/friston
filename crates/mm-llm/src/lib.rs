//! Minutes generation and unified Codex post-meeting transcription + summary.
//!
//! Provider policy (see `docs/tech-stack.md`):
//! 1. **BYOK** – if the user configured an API key, use that provider.
//! 2. **Local ChatGPT via Codex** – otherwise, if the Codex CLI is installed and
//!    signed in with ChatGPT, drive `codex app-server` over stdio JSON-RPC. It
//!    accepts text, local images, and `outputSchema` constraints. For the unified
//!    post-meeting route ([`codex::transcribe_and_summarize`]), Codex receives the
//!    recording paths in a read-only thread and handles transcription, speaker
//!    diarization, and minutes in one run.
//! 3. **On-device MLX** – optional offline fallback through `mm-embedd`.
//!
//! The user can also pick one minutes backend explicitly (`ApiKey` or `Codex`). An
//! explicit pick that fails falls back to the automatic chain above.

pub mod asr_pricing;
pub mod byok;
pub mod codex;
pub mod gemini_asr;
pub mod keychain;
pub mod minutes;
pub mod openai_asr;
pub mod openai_costs;
pub mod prompt;
pub mod provider;
pub mod render;
pub mod transcript;

pub use asr_pricing::{
    AsrMode, AsrModelList, AsrModelSpec, AsrProvider, DEFAULT_GEMINI_LIVE_MODEL,
    DEFAULT_GEMINI_REFINE_MODEL, DEFAULT_OPENAI_LIVE_MODEL, DEFAULT_OPENAI_REFINE_MODEL,
    estimate_cost_usd, known_model_spec, minutes_for_usd, model_spec, models_for,
    openai_model_has_timestamps,
};
pub use gemini_asr::GeminiTranscribeOptions;
pub use minutes::{ActionItem, Minutes, minutes_json_schema};
pub use openai_asr::{CloudTranscriptResult, OpenAiTranscribeOptions};
pub use openai_costs::OpenAiCostsReport;
pub use prompt::{
    MinutesInput, OpenAiModelFamily, TranscriptLine, codex_refine_developer_instructions,
    codex_refine_user_prompt, instructions,
};
pub use provider::{
    ByokProvider, MinutesBackend, ModelOption, ProviderChoice, ProviderConfig, ProviderError,
    ProviderPlan, plan_providers, select_provider,
};
pub use render::{RenderedMinutes, render_minutes};
pub use transcript::{AudioTrack, HeardLine, transcript_and_minutes_schema, transcript_schema};

/// Errors raised while generating minutes or transcribing via Codex.
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error(transparent)]
    NoProvider(#[from] ProviderError),
    #[error("Keychain: {0}")]
    Keychain(String),
    #[error("BYOK provider: {0}")]
    Provider(String),
    #[error("Codex: {0}")]
    Codex(String),
    #[error("local model: {0}")]
    Local(String),
}

/// Runs the chosen provider.
///
/// Params: `choice` – from [`select_provider`]; `input` – meeting material;
/// `local` – runs an on-device model (`model id`, input) when the choice is local.
/// Returns: parsed minutes.
pub fn generate_minutes(
    choice: &ProviderChoice,
    input: &MinutesInput,
    local: &dyn Fn(&str, &MinutesInput) -> Result<Minutes, LlmError>,
) -> Result<Minutes, LlmError> {
    match choice {
        ProviderChoice::Byok(p) => {
            let key = byok::read_key(p)?.ok_or_else(|| {
                LlmError::Keychain(format!(
                    "no API key stored for {}",
                    byok::keychain_account(p)
                ))
            })?;
            byok::generate(p, &key, input)
        }
        ProviderChoice::Codex { codex_path, model } => codex::generate(
            std::path::Path::new(codex_path),
            input,
            model.as_deref(),
            std::time::Duration::from_secs(15 * 60),
        ),
        ProviderChoice::LocalMlx { model } => local(model, input),
    }
}
