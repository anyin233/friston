//! Minutes generation.
//!
//! Provider policy (see `docs/tech-stack.md`):
//! 1. **BYOK** – if the user configured an API key, use that provider.
//! 2. **Local ChatGPT via Codex** – otherwise, if the Codex CLI is installed and
//!    signed in with ChatGPT, drive `codex app-server` over stdio JSON-RPC. It
//!    accepts text, local images and local audio inputs plus an `outputSchema`
//!    that constrains the final message to our minutes JSON.
//! 3. **On-device MLX** – optional offline fallback through `mm-embedd`.

pub mod byok;
pub mod codex;
pub mod keychain;
pub mod minutes;
pub mod prompt;
pub mod provider;
pub mod render;

pub use minutes::{ActionItem, Minutes, minutes_json_schema};
pub use prompt::{MinutesInput, TranscriptLine, instructions};
pub use provider::{ByokProvider, ProviderChoice, ProviderConfig, ProviderError, select_provider};
pub use render::{RenderedMinutes, render_minutes};

/// Errors raised while generating minutes.
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
        ProviderChoice::Codex { codex_path } => codex::generate(
            std::path::Path::new(codex_path),
            input,
            std::time::Duration::from_secs(15 * 60),
        ),
        ProviderChoice::LocalMlx { model } => local(model, input),
    }
}
