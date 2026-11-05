//! Minutes generation.
//!
//! Provider policy (see `docs/tech-stack.md`):
//! 1. **BYOK** – if the user configured an API key, use that provider.
//! 2. **Local ChatGPT via Codex** – otherwise, if the Codex CLI is installed and
//!    signed in with ChatGPT, drive `codex app-server` over stdio JSON-RPC. It
//!    accepts text, local images and local audio inputs plus an `outputSchema`
//!    that constrains the final message to our minutes JSON.
//! 3. **On-device MLX** – optional offline fallback through `mm-embedd`.

pub mod codex;
pub mod minutes;
pub mod provider;

pub use minutes::{ActionItem, Minutes, minutes_json_schema};
pub use provider::{ProviderChoice, ProviderConfig, ProviderError, select_provider};
