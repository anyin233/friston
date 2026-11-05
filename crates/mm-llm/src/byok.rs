//! Bring-your-own-key providers over plain HTTPS.
//!
//! Keys live in the macOS Keychain (service `Meeting Master`, account = provider kind)
//! and are read only at request time.

use base64::Engine as _;
use serde_json::{Value, json};

use crate::provider::ByokProvider;
use crate::{LlmError, Minutes, MinutesInput, instructions, minutes_json_schema};

/// Keychain service name for API keys.
pub const KEYCHAIN_SERVICE: &str = "Meeting Master";

/// Anthropic Messages API version header value.
const ANTHROPIC_VERSION: &str = "2026-10-21";
/// Server-side refusal fallback; routes by refusal category so no model list is maintained.
const ANTHROPIC_FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
/// Images are attached up to this count; more add cost without changing the minutes.
const MAX_IMAGES: usize = 8;

/// Keychain account used for a provider's key.
///
/// Params: `p` – provider.
/// Returns: the account name.
pub fn keychain_account(p: &ByokProvider) -> &'static str {
    match p {
        ByokProvider::OpenAi { .. } => "openai",
        ByokProvider::Anthropic { .. } => "anthropic",
        ByokProvider::Gemini { .. } => "gemini",
        ByokProvider::OpenAiCompatible { .. } => "openai-compatible",
    }
}

/// Reads a provider's API key from the Keychain.
///
/// Params: `p` – provider.
/// Returns: the key, or `None` if none is stored.
pub fn read_key(p: &ByokProvider) -> Result<Option<String>, LlmError> {
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, keychain_account(p))
        .map_err(|e| LlmError::Keychain(format!("{}: {e}", keychain_account(p))))?;
    match entry.get_password() {
        Ok(k) => Ok(Some(k)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(LlmError::Keychain(format!("reading {}: {e}", keychain_account(p)))),
    }
}

/// Stores (or with `None`, deletes) a provider's API key in the Keychain.
///
/// Params: `p` – provider; `key` – the secret.
/// Returns: `Ok(())` once the Keychain accepted the change.
pub fn write_key(p: &ByokProvider, key: Option<&str>) -> Result<(), LlmError> {
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, keychain_account(p))
        .map_err(|e| LlmError::Keychain(format!("{}: {e}", keychain_account(p))))?;
    let result = match key {
        Some(k) => entry.set_password(k),
        None => match entry.delete_credential() {
            Err(keyring::Error::NoEntry) => Ok(()),
            other => other,
        },
    };
    result.map_err(|e| LlmError::Keychain(format!("writing {}: {e}", keychain_account(p))))
}

/// Guesses an image MIME type from the file extension.
fn mime(path: &str) -> &'static str {
    match path.rsplit('.').next().map(str::to_ascii_lowercase).as_deref() {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => "image/png",
    }
}

/// Reads images as base64, skipping unreadable files with a log line.
fn images_b64(paths: &[String]) -> Vec<(&'static str, String)> {
    paths
        .iter()
        .take(MAX_IMAGES)
        .filter_map(|p| match std::fs::read(p) {
            Ok(bytes) => Some((mime(p), base64::engine::general_purpose::STANDARD.encode(bytes))),
            Err(e) => {
                tracing::warn!("skipping screenshot {p} for minutes: {e}");
                None
            }
        })
        .collect()
}

/// Builds the Anthropic Messages request body.
///
/// Params: `model` – model id; `input` – meeting material.
/// Returns: the JSON body.
pub fn anthropic_body(model: &str, input: &MinutesInput) -> Value {
    let mut content: Vec<Value> = images_b64(&input.images)
        .into_iter()
        .map(|(m, data)| json!({"type": "image", "source": {"type": "base64", "media_type": m, "data": data}}))
        .collect();
    content.push(json!({"type": "text", "text": input.render()}));
    let mut body = json!({
        "model": model,
        "max_tokens": 16000,
        "system": instructions(input.language.as_deref()),
        "messages": [{"role": "user", "content": content}],
        "output_config": {"effort": "medium", "format": {"type": "json_schema", "schema": minutes_json_schema()}},
    });
    if anthropic_supports_fallback(model) {
        body["fallbacks"] = json!("default");
    }
    body
}

/// Models that accept the `fallbacks: "default"` server-side refusal fallback.
fn anthropic_supports_fallback(model: &str) -> bool {
    matches!(
        model,
        "claude-fable-5-1" | "claude-opus-5-5" | "claude-opus-5" | "claude-sonnet-5-5"
    )
}

/// Builds an OpenAI-style Chat Completions body with a strict JSON schema.
///
/// Params: `model`; `input`.
/// Returns: the JSON body.
pub fn openai_body(model: &str, input: &MinutesInput) -> Value {
    let mut content: Vec<Value> = vec![json!({"type": "text", "text": input.render()})];
    content.extend(images_b64(&input.images).into_iter().map(|(m, data)| {
        json!({"type": "image_url", "image_url": {"url": format!("data:{m};base64,{data}")}})
    }));
    json!({
        "model": model,
        "messages": [
            {"role": "system", "content": instructions(input.language.as_deref())},
            {"role": "user", "content": content}
        ],
        "response_format": {"type": "json_schema", "json_schema": {"name": "minutes", "strict": true, "schema": minutes_json_schema()}}
    })
}

/// Extracts the minutes JSON text from an Anthropic response.
///
/// Params: `resp` – decoded response body.
/// Returns: the JSON text of the first text block.
pub fn anthropic_text(resp: &Value) -> Result<String, LlmError> {
    if resp["stop_reason"] == "refusal" {
        let cat = resp["stop_details"]["category"].as_str().unwrap_or("unspecified");
        return Err(LlmError::Provider(format!("Anthropic declined the request (category {cat})")));
    }
    if resp["stop_reason"] == "max_tokens" {
        return Err(LlmError::Provider("Anthropic output hit max_tokens; minutes are incomplete".into()));
    }
    resp["content"]
        .as_array()
        .and_then(|blocks| blocks.iter().find(|b| b["type"] == "text"))
        .and_then(|b| b["text"].as_str())
        .map(String::from)
        .ok_or_else(|| LlmError::Provider(format!("no text block in Anthropic response: {resp}")))
}

/// Extracts the message text from a Chat Completions response.
///
/// Params: `resp` – decoded response body.
/// Returns: the assistant message content.
pub fn openai_text(resp: &Value) -> Result<String, LlmError> {
    let choice = &resp["choices"][0];
    if let Some(refusal) = choice["message"]["refusal"].as_str() {
        return Err(LlmError::Provider(format!("model refused: {refusal}")));
    }
    choice["message"]["content"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| LlmError::Provider(format!("no message content in response: {resp}")))
}

/// Parses minutes JSON, tolerating a Markdown code fence around it.
///
/// Params: `text` – model output.
/// Returns: the minutes.
pub fn parse_minutes(text: &str) -> Result<Minutes, LlmError> {
    let t = text.trim();
    let t = t
        .strip_prefix("```json")
        .or_else(|| t.strip_prefix("```"))
        .map(|r| r.trim_end().trim_end_matches("```"))
        .unwrap_or(t);
    serde_json::from_str(t.trim()).map_err(|e| {
        let head: String = text.chars().take(200).collect();
        LlmError::Provider(format!("minutes JSON did not match the schema ({e}): {head}"))
    })
}

/// POSTs JSON and returns the decoded body, turning HTTP errors into messages with the body.
fn post_json(url: &str, headers: &[(&str, String)], body: &Value) -> Result<Value, LlmError> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(std::time::Duration::from_secs(600)))
        .build()
        .into();
    let mut req = agent.post(url);
    for (k, v) in headers {
        req = req.header(*k, v.as_str());
    }
    let mut resp = req
        .send_json(body)
        .map_err(|e| LlmError::Provider(format!("POST {url}: {e}")))?;
    let status = resp.status();
    let text = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| LlmError::Provider(format!("reading response from {url}: {e}")))?;
    if !status.is_success() {
        return Err(LlmError::Provider(format!("POST {url} returned {status}: {text}")));
    }
    serde_json::from_str(&text).map_err(|e| LlmError::Provider(format!("decoding response from {url}: {e}")))
}

/// Generates minutes with a BYOK provider.
///
/// Params: `p` – provider; `key` – API key; `input` – meeting material.
/// Returns: parsed minutes.
pub fn generate(p: &ByokProvider, key: &str, input: &MinutesInput) -> Result<Minutes, LlmError> {
    let text = match p {
        ByokProvider::Anthropic { model } => {
            let mut headers = vec![
                ("x-api-key", key.to_string()),
                ("anthropic-version", ANTHROPIC_VERSION.to_string()),
            ];
            if anthropic_supports_fallback(model) {
                headers.push(("anthropic-beta", ANTHROPIC_FALLBACK_BETA.to_string()));
            }
            anthropic_text(&post_json(
                "https://api.anthropic.com/v1/messages",
                &headers,
                &anthropic_body(model, input),
            )?)?
        }
        ByokProvider::OpenAi { model } => openai_text(&post_json(
            "https://api.openai.com/v1/chat/completions",
            &[("authorization", format!("Bearer {key}"))],
            &openai_body(model, input),
        )?)?,
        ByokProvider::Gemini { model } => openai_text(&post_json(
            // Gemini's OpenAI-compatible endpoint supports the same json_schema response format.
            "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions",
            &[("authorization", format!("Bearer {key}"))],
            &openai_body(model, input),
        )?)?,
        ByokProvider::OpenAiCompatible { base_url, model } => openai_text(&post_json(
            &format!("{}/chat/completions", base_url.trim_end_matches('/')),
            &[("authorization", format!("Bearer {key}"))],
            &openai_body(model, input),
        )?)?,
    };
    parse_minutes(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> MinutesInput {
        MinutesInput {
            title: "周会".into(),
            ..Default::default()
        }
    }

    #[test]
    fn anthropic_body_uses_structured_output_and_fallbacks() {
        let b = anthropic_body("claude-opus-5-5", &input());
        assert_eq!(b["output_config"]["format"]["type"], "json_schema");
        assert_eq!(b["fallbacks"], "default");
        assert!(b.get("temperature").is_none(), "sampling params are rejected by the 2026-10-21 API");
        assert!(anthropic_body("claude-haiku-5-5", &input()).get("fallbacks").is_none());
    }

    #[test]
    fn response_extraction_and_refusals() {
        let ok = json!({"stop_reason":"end_turn","content":[{"type":"thinking","thinking":""},{"type":"text","text":"{}"}]});
        assert_eq!(anthropic_text(&ok).unwrap(), "{}");
        let refused = json!({"stop_reason":"refusal","stop_details":{"category":"cyber"},"content":[]});
        assert!(anthropic_text(&refused).unwrap_err().to_string().contains("cyber"));
        let oa = json!({"choices":[{"message":{"content":"{\"a\":1}"}}]});
        assert_eq!(openai_text(&oa).unwrap(), "{\"a\":1}");
        assert_eq!(openai_body("gpt", &input())["response_format"]["json_schema"]["strict"], true);
    }

    #[test]
    fn parses_fenced_minutes() {
        let m = parse_minutes(
            "```json\n{\"title\":\"t\",\"summary\":\"s\",\"decisions\":[],\"action_items\":[],\"open_questions\":[],\"topics\":[]}\n```",
        )
        .unwrap();
        assert_eq!(m.summary, "s");
        assert!(parse_minutes("not json").is_err());
    }
}
