//! Google Gemini Speech-to-Text REST client (Interactions API + multimodal `generateContent`).

use std::collections::HashSet;
use std::path::Path;

use base64::Engine as _;
use mm_core::AudioOrigin;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::byok::{decode_json_response, http_agent};
use crate::openai_asr::CloudTranscriptResult;
use crate::{AsrMode, AsrModelList, AsrProvider, HeardLine, LlmError, ModelOption, asr_pricing};

/// Default Google Generative Language API origin.
const GEMINI_API_BASE: &str = "https://generativelanguage.googleapis.com";
/// Inline audio payloads above 15 MiB switch to the resumable Files API upload.
const MAX_INLINE_BYTES: usize = 15 * 1024 * 1024;
/// Maximum time spent waiting for an uploaded file to become `ACTIVE`.
const FILE_POLL_TIMEOUT_SECS: u64 = 60;

/// Request options for [`transcribe_file`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeminiTranscribeOptions {
    pub model: String,
    pub language: Option<String>,
    pub context: Option<String>,
}

/// Best-effort cleanup guard so uploaded meeting recordings are deleted from Gemini Files.
struct UploadedFileGuard<'a> {
    base: &'a str,
    api_key: &'a str,
    name: Option<String>,
}

impl Drop for UploadedFileGuard<'_> {
    fn drop(&mut self) {
        let Some(name) = self.name.take() else {
            return;
        };
        let url = format!(
            "{}/v1beta/{}",
            self.base.trim_end_matches('/'),
            name.trim_start_matches('/')
        );
        let agent = http_agent(15);
        if let Err(e) = agent
            .delete(&url)
            .header("x-goog-api-key", self.api_key.trim())
            .call()
        {
            tracing::debug!("deleting uploaded Gemini file {name}: {e}");
        }
    }
}

/// Lists Google Gemini transcription models available to `api_key`, separated into live & refine modes.
///
/// Params: `api_key` – Gemini API key.
/// Returns: sorted live and refine model options fetched from `/v1beta/models`.
pub fn list_models(api_key: &str) -> Result<AsrModelList, LlmError> {
    list_models_at(GEMINI_API_BASE, api_key)
}

/// Testable implementation of [`list_models`] against a configurable base URL.
///
/// Params: `base_url`; `api_key`.
/// Returns: sorted live and refine model options.
pub fn list_models_at(base_url: &str, api_key: &str) -> Result<AsrModelList, LlmError> {
    let base = base_url.trim_end_matches('/');
    let key = api_key.trim();
    let agent = http_agent(30);
    let mut page_token: Option<String> = None;
    let mut seen = HashSet::new();
    let mut live_ids = Vec::new();
    let mut refine_ids = Vec::new();

    for _ in 0..5 {
        let mut url = format!("{base}/v1beta/models?pageSize=100");
        if let Some(tok) = page_token.as_deref() {
            url.push_str("&pageToken=");
            push_query_encoded(&mut url, tok);
        }
        let resp = agent
            .get(&url)
            .header("x-goog-api-key", key)
            .call()
            .map_err(|e| LlmError::Provider(format!("GET {url}: {e}")))?;
        let json = decode_json_response(&url, resp)?;

        for entry in json["models"].as_array().into_iter().flatten() {
            let Some(raw_name) = entry["name"].as_str().map(str::trim) else {
                continue;
            };
            let id = raw_name.strip_prefix("models/").unwrap_or(raw_name).trim();
            if id.is_empty() || !seen.insert(id.to_ascii_lowercase()) {
                continue;
            }
            if !is_gemini_asr_candidate(id) {
                continue;
            }
            let methods: Vec<&str> = entry["supportedGenerationMethods"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str())
                .collect();
            if is_gemini_live_model(id, &methods) {
                live_ids.push(id.to_string());
            }
            if is_gemini_refine_model(id, &methods) {
                refine_ids.push(id.to_string());
            }
        }

        let next = json["nextPageToken"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);
        if next.is_none() || next == page_token {
            break;
        }
        page_token = next;
    }

    Ok(AsrModelList {
        live: sort_gemini_models(live_ids, AsrMode::Live),
        refine: sort_gemini_models(refine_ids, AsrMode::Refine),
    })
}

fn push_query_encoded(out: &mut String, s: &str) {
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(
                char::from_digit((b >> 4) as u32, 16)
                    .unwrap()
                    .to_ascii_uppercase(),
            );
            out.push(
                char::from_digit((b & 0xf) as u32, 16)
                    .unwrap()
                    .to_ascii_uppercase(),
            );
        }
    }
}

fn is_gemini_asr_candidate(id: &str) -> bool {
    let lower = id.to_ascii_lowercase();
    if !lower.starts_with("gemini-") {
        return false;
    }
    const EXCLUDED: &[&str] = &[
        "-tts",
        "-image",
        "-embedding",
        "embedding-",
        "-robotics",
        "-computer-use",
        "-vision",
        "deep-research",
        "aqa",
    ];
    !EXCLUDED.iter().any(|needle| lower.contains(needle))
}

fn is_gemini_live_model(id: &str, methods: &[&str]) -> bool {
    if methods
        .iter()
        .any(|m| m.eq_ignore_ascii_case("bidiGenerateContent"))
    {
        return true;
    }
    if methods.is_empty() {
        let lower = id.to_ascii_lowercase();
        return lower.contains("-live") || lower.contains("native-audio");
    }
    false
}

fn is_gemini_refine_model(id: &str, methods: &[&str]) -> bool {
    let lower = id.to_ascii_lowercase();
    if lower.contains("-live") || lower.contains("native-audio") {
        return false;
    }
    is_dedicated_transcribe_model(id)
        || methods.is_empty()
        || methods
            .iter()
            .any(|m| m.eq_ignore_ascii_case("generateContent"))
}

fn sort_gemini_models(mut ids: Vec<String>, mode: AsrMode) -> Vec<ModelOption> {
    let catalog = asr_pricing::models_for(AsrProvider::Gemini, mode);
    let default_id = asr_pricing::default_model(AsrProvider::Gemini, mode);
    ids.sort_by_key(|id| {
        let rank = catalog
            .iter()
            .position(|spec| spec.id.eq_ignore_ascii_case(id))
            .unwrap_or(usize::MAX);
        let lower = id.to_ascii_lowercase();
        let not_transcribe = !is_dedicated_transcribe_model(id);
        let unstable = lower.contains("-preview") || lower.contains("-exp");
        (rank, not_transcribe, unstable, lower)
    });
    ids.into_iter()
        .map(|id| ModelOption {
            is_default: id.eq_ignore_ascii_case(default_id),
            label: id.clone(),
            id,
        })
        .collect()
}

/// Transcribes one audio file via Google Gemini API.
///
/// Dedicated `gemini-*-transcribe` models use the Interactions API + Files API with speaker
/// diarization and word timestamps; general multimodal models (`gemini-3.8-flash`,
/// `gemini-3.1-flash-lite`, `gemini-2.5-flash`) use `generateContent` with a structured JSON
/// schema.
///
/// Params: `api_key`; `origin`; `path`; `offset_ms`; `duration_ms`; `opts`.
/// Returns: transcribed lines plus token/duration usage.
pub fn transcribe_file(
    api_key: &str,
    origin: AudioOrigin,
    path: &Path,
    offset_ms: u64,
    duration_ms: u64,
    opts: &GeminiTranscribeOptions,
) -> Result<CloudTranscriptResult, LlmError> {
    transcribe_file_at(
        GEMINI_API_BASE,
        api_key,
        origin,
        path,
        offset_ms,
        duration_ms,
        opts,
    )
}

/// Testable implementation of [`transcribe_file`] against a configurable base URL.
///
/// Params: `base_url`; `api_key`; `origin`; `path`; `offset_ms`; `duration_ms`; `opts`.
/// Returns: transcribed lines plus usage metrics.
pub fn transcribe_file_at(
    base_url: &str,
    api_key: &str,
    origin: AudioOrigin,
    path: &Path,
    offset_ms: u64,
    duration_ms: u64,
    opts: &GeminiTranscribeOptions,
) -> Result<CloudTranscriptResult, LlmError> {
    let base = base_url.trim_end_matches('/');
    let key = api_key.trim();
    let model = opts.model.trim();
    let audio_bytes = std::fs::read(path)
        .map_err(|e| LlmError::Provider(format!("reading audio {}: {e}", path.display())))?;
    let mime = audio_mime(path);
    let display_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("audio.m4a");

    if is_dedicated_transcribe_model(model) {
        let uploaded = upload_file(base, key, display_name, mime, &audio_bytes)?;
        let _guard = UploadedFileGuard {
            base,
            api_key: key,
            name: Some(uploaded.name.clone()),
        };
        wait_until_active(base, key, &uploaded)?;
        return call_interactions(
            base,
            key,
            model,
            &uploaded.uri,
            mime,
            origin,
            offset_ms,
            duration_ms,
            opts.language.as_deref(),
        );
    }

    let mut _guard = UploadedFileGuard {
        base,
        api_key: key,
        name: None,
    };
    let audio_part = if audio_bytes.len() <= MAX_INLINE_BYTES {
        let b64 = base64::engine::general_purpose::STANDARD.encode(&audio_bytes);
        json!({
            "inline_data": {
                "mime_type": mime,
                "data": b64,
            }
        })
    } else {
        let uploaded = upload_file(base, key, display_name, mime, &audio_bytes)?;
        _guard.name = Some(uploaded.name.clone());
        wait_until_active(base, key, &uploaded)?;
        json!({
            "file_data": {
                "mime_type": mime,
                "file_uri": uploaded.uri,
            }
        })
    };

    call_generate_content(
        base,
        key,
        model,
        audio_part,
        origin,
        offset_ms,
        duration_ms,
        opts,
    )
}

/// Whether `model` targets the dedicated `v1beta/interactions` speech-to-text endpoint.
fn is_dedicated_transcribe_model(model: &str) -> bool {
    model.to_ascii_lowercase().contains("transcribe")
}

struct UploadedFile {
    name: String,
    uri: String,
    state: String,
}

/// Uploads audio via Gemini's two-step resumable Files API.
fn upload_file(
    base: &str,
    api_key: &str,
    display_name: &str,
    mime: &str,
    bytes: &[u8],
) -> Result<UploadedFile, LlmError> {
    let start_url = format!("{base}/upload/v1beta/files");
    let agent = http_agent(600);
    let init_resp = agent
        .post(&start_url)
        .header("x-goog-api-key", api_key)
        .header("x-goog-upload-protocol", "resumable")
        .header("x-goog-upload-command", "start")
        .header(
            "x-goog-upload-header-content-length",
            bytes.len().to_string(),
        )
        .header("x-goog-upload-header-content-type", mime)
        .send_json(json!({"file": {"display_name": display_name}}))
        .map_err(|e| LlmError::Provider(format!("POST {start_url}: {e}")))?;

    let init_status = init_resp.status();
    let upload_url = init_resp
        .headers()
        .iter()
        .find(|(k, _)| k.as_str().eq_ignore_ascii_case("x-goog-upload-url"))
        .and_then(|(_, v)| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);

    if !init_status.is_success() {
        let body = init_resp.into_body().read_to_string().unwrap_or_default();
        return Err(LlmError::Provider(format!(
            "HTTP {init_status} starting Gemini upload at {start_url}: {body}"
        )));
    }
    let upload_url = upload_url.ok_or_else(|| {
        LlmError::Provider(format!(
            "Gemini resumable upload response from {start_url} missed x-goog-upload-url header"
        ))
    })?;

    let finalize_resp = agent
        .post(&upload_url)
        .header("x-goog-upload-offset", "0")
        .header("x-goog-upload-command", "upload, finalize")
        .send(bytes)
        .map_err(|e| LlmError::Provider(format!("uploading audio to {upload_url}: {e}")))?;
    let json = decode_json_response(&upload_url, finalize_resp)?;
    let file_obj = if json["file"].is_object() {
        &json["file"]
    } else {
        &json
    };
    let name = file_obj["name"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| LlmError::Provider(format!("Gemini file upload missing name: {json}")))?;
    let uri = file_obj["uri"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| LlmError::Provider(format!("Gemini file upload missing uri: {json}")))?;
    let state = file_obj["state"].as_str().unwrap_or("ACTIVE").to_string();
    Ok(UploadedFile { name, uri, state })
}

/// Polls until an uploaded Gemini file leaves `PROCESSING`.
fn wait_until_active(base: &str, api_key: &str, file: &UploadedFile) -> Result<(), LlmError> {
    if file.state != "PROCESSING" {
        if file.state == "FAILED" {
            return Err(LlmError::Provider(format!(
                "Gemini file {} processing failed",
                file.name
            )));
        }
        return Ok(());
    }
    let url = format!("{base}/v1beta/{}", file.name.trim_start_matches('/'));
    let agent = http_agent(30);
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(FILE_POLL_TIMEOUT_SECS);
    while std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let resp = agent
            .get(&url)
            .header("x-goog-api-key", api_key)
            .call()
            .map_err(|e| LlmError::Provider(format!("GET {url}: {e}")))?;
        let json = decode_json_response(&url, resp)?;
        let state = json["state"]
            .as_str()
            .or_else(|| json["file"]["state"].as_str())
            .unwrap_or("ACTIVE");
        match state {
            "ACTIVE" => return Ok(()),
            "FAILED" => {
                return Err(LlmError::Provider(format!(
                    "Gemini file {} processing failed: {json}",
                    file.name
                )));
            }
            _ => {}
        }
    }
    Err(LlmError::Provider(format!(
        "timed out waiting for Gemini file {} to become ACTIVE",
        file.name
    )))
}

/// Calls `POST /v1beta/interactions` for dedicated `gemini-*-transcribe` models.
#[allow(clippy::too_many_arguments)]
fn call_interactions(
    base: &str,
    api_key: &str,
    model: &str,
    file_uri: &str,
    mime: &str,
    origin: AudioOrigin,
    offset_ms: u64,
    duration_ms: u64,
    language: Option<&str>,
) -> Result<CloudTranscriptResult, LlmError> {
    let mut tx_cfg = json!({
        "mode": {
            "type": "verbatim",
            "diarization_mode": "speaker",
            "timestamp_granularities": ["word"],
        }
    });
    if let Some(code) = gemini_language_code(language) {
        tx_cfg["language_codes"] = json!([code]);
    }
    let body = json!({
        "model": model,
        "input": [{
            "role": "user",
            "content": [{
                "type": "audio",
                "uri": file_uri,
                "mime_type": mime,
            }]
        }],
        "generation_config": {
            "transcription_config": tx_cfg,
        }
    });
    let url = format!("{base}/v1beta/interactions");
    let agent = http_agent(600);
    let resp = agent
        .post(&url)
        .header("x-goog-api-key", api_key)
        .send_json(&body)
        .map_err(|e| LlmError::Provider(format!("POST {url}: {e}")))?;
    let json = decode_json_response(&url, resp)?;
    Ok(parse_interactions_response(
        &json,
        origin,
        offset_ms,
        duration_ms,
        model,
    ))
}

/// Calls `POST /v1beta/models/{model}:generateContent` with a structured JSON schema.
#[allow(clippy::too_many_arguments)]
fn call_generate_content(
    base: &str,
    api_key: &str,
    model: &str,
    audio_part: Value,
    origin: AudioOrigin,
    offset_ms: u64,
    duration_ms: u64,
    opts: &GeminiTranscribeOptions,
) -> Result<CloudTranscriptResult, LlmError> {
    let lang_hint = match opts.language.as_deref().map(str::trim) {
        Some("zh") | Some("zh-CN") | Some("zh-Hans") => {
            "Use Simplified Chinese for Mandarin speech."
        }
        Some("en") | Some("en-US") => "Transcribe speech in English.",
        Some("ja") | Some("ja-JP") => "Transcribe speech in Japanese.",
        Some(other) if !other.is_empty() && !other.eq_ignore_ascii_case("auto") => {
            "Follow the spoken language (hint provided)."
        }
        _ => "Auto-detect and preserve the spoken language(s) accurately.",
    };
    let mut prompt = format!(
        "Transcribe all audible human speech in this meeting audio segment accurately.\n\
         - {lang_hint}\n\
         - Return an empty `segments` array if the recording is silent or contains only noise; never fabricate speech.\n\
         - Separate speakers consistently (`说话人1`, `说话人2`, …).\n\
         - Report integer millisecond offsets `start_ms` and `end_ms` relative to the start of this audio clip (`0` ms), with `end_ms >= start_ms`."
    );
    if let Some(ctx) = opts
        .context
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let trimmed: String = ctx.chars().take(600).collect();
        prompt.push_str(&format!(
            "\n\n<spelling_hints>\n{trimmed}\n</spelling_hints>"
        ));
    }

    let body = json!({
        "contents": [{
            "parts": [
                audio_part,
                {"text": prompt}
            ]
        }],
        "generationConfig": {
            "responseMimeType": "application/json",
            "responseSchema": {
                "type": "OBJECT",
                "properties": {
                    "segments": {
                        "type": "ARRAY",
                        "items": {
                            "type": "OBJECT",
                            "properties": {
                                "start_ms": {"type": "INTEGER"},
                                "end_ms": {"type": "INTEGER"},
                                "speaker": {"type": "STRING"},
                                "text": {"type": "STRING"}
                            },
                            "required": ["start_ms", "end_ms", "speaker", "text"]
                        }
                    }
                },
                "required": ["segments"]
            }
        }
    });

    let url = format!("{base}/v1beta/models/{model}:generateContent");
    let agent = http_agent(600);
    let resp = agent
        .post(&url)
        .header("x-goog-api-key", api_key)
        .send_json(&body)
        .map_err(|e| LlmError::Provider(format!("POST {url}: {e}")))?;
    let json = decode_json_response(&url, resp)?;
    parse_generate_content_response(&json, origin, offset_ms, duration_ms, model)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TimedWord {
    text: String,
    speaker: String,
    start_ms: u64,
    end_ms: u64,
}

/// Parses an Interactions API response, preferring `word_info` annotations.
fn parse_interactions_response(
    json: &Value,
    origin: AudioOrigin,
    offset_ms: u64,
    duration_ms: u64,
    model: &str,
) -> CloudTranscriptResult {
    let mut words = Vec::new();
    if let Some(steps) = json["steps"].as_array() {
        for step in steps {
            if let Some(contents) = step["content"].as_array() {
                for block in contents {
                    if let Some(anns) = block["annotations"].as_array() {
                        for ann in anns {
                            if ann["type"] == "word_info"
                                && let Some(w) = parse_word_item(ann)
                            {
                                words.push(w);
                            }
                        }
                    }
                }
            }
        }
    }
    if words.is_empty()
        && let Some(word_info) = json["transcription_metadata"]["word_info"].as_array()
    {
        for item in word_info {
            if let Some(w) = parse_word_item(item) {
                words.push(w);
            }
        }
    }

    let lines = if !words.is_empty() {
        group_words_to_lines(&words, origin, offset_ms)
    } else {
        let fallback_text = json["output_text"]
            .as_str()
            .or_else(|| {
                json["output"]
                    .as_array()
                    .and_then(|arr| arr.first())
                    .and_then(|o| o["content"].as_array())
                    .and_then(|c| c.first())
                    .and_then(|b| b["text"].as_str())
            })
            .unwrap_or("")
            .trim();
        if fallback_text.is_empty() {
            Vec::new()
        } else {
            vec![HeardLine {
                source: origin,
                start_ms: offset_ms,
                end_ms: offset_ms.saturating_add(duration_ms),
                speaker: String::new(),
                text: fallback_text.to_string(),
            }]
        }
    };

    let (input_tokens, output_tokens) = extract_usage_tokens(json);
    CloudTranscriptResult {
        lines,
        audio_ms: duration_ms,
        input_tokens,
        output_tokens,
        model: model.to_string(),
    }
}

/// Parses one `word_info` JSON object from either `steps` or `transcription_metadata`.
fn parse_word_item(item: &Value) -> Option<TimedWord> {
    let text = item["text"]
        .as_str()
        .or_else(|| item["word"].as_str())?
        .trim();
    if text.is_empty() {
        return None;
    }
    let start_ms = parse_offset_ms(&item["start_offset"])?;
    let end_ms = parse_offset_ms(&item["end_offset"])
        .unwrap_or(start_ms)
        .max(start_ms);
    let speaker = item["speaker"]
        .as_str()
        .or_else(|| item["speaker_label"].as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    Some(TimedWord {
        text: text.to_string(),
        speaker,
        start_ms,
        end_ms,
    })
}

/// Parses `"0.450s"` or numeric seconds into milliseconds.
fn parse_offset_ms(v: &Value) -> Option<u64> {
    if let Some(secs) = v.as_f64() {
        return secs
            .is_finite()
            .then_some((secs.max(0.0) * 1000.0).round() as u64);
    }
    let s = v.as_str()?.trim();
    let num = s
        .strip_suffix('s')
        .unwrap_or(s)
        .trim()
        .parse::<f64>()
        .ok()?;
    num.is_finite()
        .then_some((num.max(0.0) * 1000.0).round() as u64)
}

/// Groups word-level items into sentence-sized [`HeardLine`] entries.
fn group_words_to_lines(
    words: &[TimedWord],
    origin: AudioOrigin,
    offset_ms: u64,
) -> Vec<HeardLine> {
    const MAX_PAUSE_MS: u64 = 1_000;
    const MAX_LINE_MS: u64 = 20_000;

    let mut out = Vec::new();
    let mut cur_speaker = String::new();
    let mut cur_start = 0_u64;
    let mut cur_end = 0_u64;
    let mut cur_text = String::new();

    for w in words {
        let should_split = !cur_text.is_empty()
            && (w.speaker != cur_speaker
                || w.start_ms.saturating_sub(cur_end) >= MAX_PAUSE_MS
                || w.end_ms.saturating_sub(cur_start) >= MAX_LINE_MS
                || ends_sentence(&cur_text));

        if should_split {
            out.push(HeardLine {
                source: origin,
                start_ms: offset_ms.saturating_add(cur_start),
                end_ms: offset_ms.saturating_add(cur_end.max(cur_start)),
                speaker: std::mem::take(&mut cur_speaker),
                text: std::mem::take(&mut cur_text),
            });
        }

        if cur_text.is_empty() {
            cur_speaker = w.speaker.clone();
            cur_start = w.start_ms;
        }
        cur_end = w.end_ms.max(cur_end);
        join_word_token(&mut cur_text, &w.text);
    }

    if !cur_text.is_empty() {
        out.push(HeardLine {
            source: origin,
            start_ms: offset_ms.saturating_add(cur_start),
            end_ms: offset_ms.saturating_add(cur_end.max(cur_start)),
            speaker: cur_speaker,
            text: cur_text,
        });
    }
    out
}

fn ends_sentence(s: &str) -> bool {
    matches!(s.chars().last(), Some('。' | '！' | '？' | '.' | '!' | '?'))
}

fn is_cjk(c: char) -> bool {
    matches!(
        c as u32,
        0x3040..=0x30FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xF900..=0xFAFF
            | 0xAC00..=0xD7AF
            | 0x3000..=0x303F
            | 0xFF00..=0xFFEF
    )
}

/// Appends `token` to `buf` without adding spaces around CJK text or attached punctuation.
fn join_word_token(buf: &mut String, token: &str) {
    if buf.is_empty() {
        buf.push_str(token);
        return;
    }
    let last = buf.chars().last();
    let first = token.chars().next();
    let attached_punct = matches!(
        first,
        Some(',' | '.' | '!' | '?' | ';' | ':' | ')' | ']' | '，' | '。' | '！' | '？' | '、')
    );
    let cjk_boundary = last.is_some_and(is_cjk) || first.is_some_and(is_cjk);
    if !attached_punct && !cjk_boundary {
        buf.push(' ');
    }
    buf.push_str(token);
}

#[derive(Debug, Deserialize)]
struct GenerateSegmentsPayload {
    segments: Vec<GenerateSegment>,
}

#[derive(Debug, Deserialize)]
struct GenerateSegment {
    start_ms: u64,
    end_ms: u64,
    #[serde(default)]
    speaker: String,
    text: String,
}

/// Parses a `generateContent` JSON Schema transcription response.
fn parse_generate_content_response(
    json: &Value,
    origin: AudioOrigin,
    offset_ms: u64,
    duration_ms: u64,
    model: &str,
) -> Result<CloudTranscriptResult, LlmError> {
    let text = json["candidates"][0]["content"]["parts"]
        .as_array()
        .and_then(|parts| parts.iter().find_map(|p| p["text"].as_str()))
        .ok_or_else(|| {
            LlmError::Provider(format!(
                "no text in Gemini generateContent response: {json}"
            ))
        })?;
    let clean = text
        .trim()
        .strip_prefix("```json")
        .or_else(|| text.trim().strip_prefix("```"))
        .map(|r| r.trim_end().trim_end_matches("```"))
        .unwrap_or(text.trim())
        .trim();
    let parsed: GenerateSegmentsPayload = serde_json::from_str(clean).map_err(|e| {
        let head: String = text.chars().take(200).collect();
        LlmError::Provider(format!(
            "Gemini transcript JSON did not match schema ({e}): {head}"
        ))
    })?;
    let lines = parsed
        .segments
        .into_iter()
        .filter_map(|seg| {
            let t = seg.text.trim();
            if t.is_empty() {
                return None;
            }
            let start_ms = offset_ms.saturating_add(seg.start_ms);
            let end_ms = offset_ms.saturating_add(seg.end_ms.max(seg.start_ms));
            Some(HeardLine {
                source: origin,
                start_ms,
                end_ms,
                speaker: seg.speaker.trim().to_string(),
                text: t.to_string(),
            })
        })
        .collect();
    let (input_tokens, output_tokens) = extract_usage_tokens(json);
    Ok(CloudTranscriptResult {
        lines,
        audio_ms: duration_ms,
        input_tokens,
        output_tokens,
        model: model.to_string(),
    })
}

/// Extracts `(input_tokens, output_tokens)` across snake_case and camelCase Gemini payloads.
fn extract_usage_tokens(json: &Value) -> (u64, u64) {
    let usage = if json["usage_metadata"].is_object() {
        &json["usage_metadata"]
    } else if json["usageMetadata"].is_object() {
        &json["usageMetadata"]
    } else {
        &json["usage"]
    };
    let input = usage["prompt_token_count"]
        .as_u64()
        .or_else(|| usage["promptTokenCount"].as_u64())
        .or_else(|| usage["input_tokens"].as_u64())
        .unwrap_or(0);
    let output = usage["candidates_token_count"]
        .as_u64()
        .or_else(|| usage["candidatesTokenCount"].as_u64())
        .or_else(|| usage["output_tokens"].as_u64())
        .unwrap_or(0);
    (input, output)
}

/// Maps short UI caption language tags (`"zh"`, `"en"`, `"ja"`) to Gemini region tags.
fn gemini_language_code(lang: Option<&str>) -> Option<String> {
    let clean = lang?.trim();
    if clean.is_empty() || clean.eq_ignore_ascii_case("auto") {
        return None;
    }
    Some(match clean.to_ascii_lowercase().as_str() {
        "zh" | "zh-hans" => "zh-CN".to_string(),
        "zh-hant" => "zh-TW".to_string(),
        "en" => "en-US".to_string(),
        "ja" => "ja-JP".to_string(),
        _ => clean.to_string(),
    })
}

/// Guesses audio MIME type for Gemini upload / inline payloads.
fn audio_mime(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("m4a") => "audio/m4a",
        Some("mp4") => "audio/mp4",
        Some("aac") => "audio/aac",
        Some("wav") => "audio/wav",
        Some("mp3" | "mpeg") => "audio/mp3",
        Some("ogg") => "audio/ogg",
        Some("flac") => "audio/flac",
        _ => "audio/m4a",
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::*;

    #[derive(Debug)]
    struct RecordedReq {
        method: String,
        path: String,
        api_key: Option<String>,
        body: Vec<u8>,
    }

    fn mock_gemini(
        responder: impl Fn(&str, &str, &[u8]) -> (u16, Vec<(String, String)>, Value) + Send + 'static,
    ) -> (String, Arc<Mutex<Vec<RecordedReq>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen: Arc<Mutex<Vec<RecordedReq>>> = Arc::default();
        let log = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut req_line = String::new();
                if reader.read_line(&mut req_line).unwrap_or(0) == 0 {
                    continue;
                }
                let mut parts = req_line.split_whitespace();
                let method = parts.next().unwrap_or("").to_string();
                let path = parts.next().unwrap_or("").to_string();
                let mut api_key = None;
                let mut content_len = 0_usize;
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    let h = h.trim_end();
                    if h.is_empty() {
                        break;
                    }
                    if let Some((k, v)) = h.split_once(": ") {
                        if k.eq_ignore_ascii_case("x-goog-api-key") {
                            api_key = Some(v.to_string());
                        } else if k.eq_ignore_ascii_case("content-length") {
                            content_len = v.parse().unwrap_or(0);
                        }
                    }
                }
                let mut body = vec![0u8; content_len];
                if content_len > 0 {
                    reader.read_exact(&mut body).unwrap();
                }
                let (status, extra_headers, json_body) = responder(&method, &path, &body);
                log.lock().unwrap().push(RecordedReq {
                    method,
                    path,
                    api_key,
                    body,
                });
                let raw = json_body.to_string();
                let mut head = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                    raw.len()
                );
                for (k, v) in extra_headers {
                    head.push_str(&format!("{k}: {v}\r\n"));
                }
                head.push_str("\r\n");
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(raw.as_bytes());
            }
        });
        (base, seen)
    }

    #[test]
    fn dedicated_transcribe_uploads_groups_words_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let audio = dir.path().join("chunk.m4a");
        std::fs::write(&audio, b"m4a-bytes").unwrap();

        let base_holder: Arc<Mutex<String>> = Arc::default();
        let base_for_resp = base_holder.clone();
        let (base, seen) = mock_gemini(move |method, path, _body| match (method, path) {
            ("POST", "/upload/v1beta/files") => {
                let b = base_for_resp.lock().unwrap().clone();
                (
                    200,
                    vec![("x-goog-upload-url".into(), format!("{b}/upload-session/1"))],
                    json!({}),
                )
            }
            ("POST", "/upload-session/1") => (
                200,
                vec![],
                json!({
                    "file": {
                        "name": "files/abc123",
                        "uri": "https://generativelanguage.googleapis.com/v1beta/files/abc123",
                        "state": "ACTIVE"
                    }
                }),
            ),
            ("POST", "/v1beta/interactions") => (
                200,
                vec![],
                json!({
                    "steps": [{
                        "type": "model_output",
                        "content": [{
                            "type": "text",
                            "text": "大家好。欢迎",
                            "annotations": [
                                {"type": "word_info", "text": "大家", "start_offset": "0.10s", "end_offset": "0.40s", "speaker": "spk_1"},
                                {"type": "word_info", "text": "好。", "start_offset": "0.40s", "end_offset": "0.80s", "speaker": "spk_1"},
                                {"type": "word_info", "text": "Hello", "start_offset": "0.90s", "end_offset": "1.20s", "speaker": "spk_2"},
                                {"type": "word_info", "text": "world!", "start_offset": "1.20s", "end_offset": "1.60s", "speaker": "spk_2"}
                            ]
                        }]
                    }],
                    "usage_metadata": {"prompt_token_count": 300, "candidates_token_count": 20}
                }),
            ),
            ("DELETE", "/v1beta/files/abc123") => (200, vec![], json!({})),
            _ => (404, vec![], json!({})),
        });
        *base_holder.lock().unwrap() = base.clone();

        let res = transcribe_file_at(
            &base,
            "AIza-test",
            AudioOrigin::Microphone,
            &audio,
            10_000,
            2_000,
            &GeminiTranscribeOptions {
                model: "gemini-3.5-transcribe".into(),
                language: Some("zh".into()),
                context: Some("参会人：赵磊".into()),
            },
        )
        .unwrap();

        assert_eq!(res.lines.len(), 2);
        assert_eq!(res.lines[0].text, "大家好。");
        assert_eq!(res.lines[0].speaker, "spk_1");
        assert_eq!(res.lines[0].start_ms, 10_100);
        assert_eq!(res.lines[0].end_ms, 10_800);
        assert_eq!(res.lines[1].text, "Hello world!");
        assert_eq!(res.lines[1].speaker, "spk_2");
        assert_eq!(res.input_tokens, 300);
        assert_eq!(res.output_tokens, 20);

        let reqs = seen.lock().unwrap();
        assert_eq!(reqs.len(), 4);
        assert_eq!(reqs[0].api_key.as_deref(), Some("AIza-test"));
        assert_eq!(reqs[3].method, "DELETE");
        assert_eq!(reqs[3].path, "/v1beta/files/abc123");
        let inter_body = String::from_utf8_lossy(&reqs[2].body);
        assert!(inter_body.contains("zh-CN"));
        assert!(!inter_body.contains("custom_vocabulary"));
    }

    #[test]
    fn multimodal_flash_uses_inline_json_schema() {
        let dir = tempfile::tempdir().unwrap();
        let audio = dir.path().join("chunk.m4a");
        std::fs::write(&audio, b"m4a-bytes").unwrap();
        let (base, seen) = mock_gemini(move |method, path, _body| {
            if method == "POST" && path == "/v1beta/models/gemini-3.8-flash:generateContent" {
                (
                    200,
                    vec![],
                    json!({
                        "candidates": [{
                            "content": {
                                "parts": [{
                                    "text": "{\"segments\":[{\"start_ms\":200,\"end_ms\":1800,\"speaker\":\"说话人1\",\"text\":\"确认上线。\"}]}"
                                }]
                            }
                        }],
                        "usageMetadata": {"promptTokenCount": 450, "candidatesTokenCount": 32}
                    }),
                )
            } else {
                (404, vec![], json!({}))
            }
        });

        let res = transcribe_file_at(
            &base,
            "AIza-test",
            AudioOrigin::System,
            &audio,
            3_000,
            5_000,
            &GeminiTranscribeOptions {
                model: "gemini-3.8-flash".into(),
                language: Some("zh".into()),
                context: Some("项目：Friston".into()),
            },
        )
        .unwrap();

        assert_eq!(res.lines.len(), 1);
        assert_eq!(res.lines[0].source, AudioOrigin::System);
        assert_eq!(res.lines[0].start_ms, 3_200);
        assert_eq!(res.lines[0].end_ms, 4_800);
        assert_eq!(res.lines[0].text, "确认上线。");
        assert_eq!(res.input_tokens, 450);
        assert_eq!(res.output_tokens, 32);

        let reqs = seen.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        let body = String::from_utf8_lossy(&reqs[0].body);
        assert!(body.contains("inline_data"));
        assert!(body.contains("Friston"));
    }

    #[test]
    fn list_models_paginates_filters_and_separates_live_and_refine() {
        let (base, seen) = mock_gemini(move |method, path, _body| match (method, path) {
            ("GET", "/v1beta/models?pageSize=100") => (
                200,
                vec![],
                json!({
                    "models": [
                        {
                            "name": "models/imagen-3.0-generate-002",
                            "supportedGenerationMethods": ["predict"]
                        },
                        {
                            "name": "models/gemini-embedding-001",
                            "supportedGenerationMethods": ["embedContent"]
                        },
                        {
                            "name": "models/gemini-2.5-flash-preview-tts",
                            "supportedGenerationMethods": ["generateContent"]
                        },
                        {
                            "name": "models/gemini-2.5-flash",
                            "supportedGenerationMethods": ["generateContent"]
                        },
                        {
                            "name": "models/gemini-4.0-flash-preview",
                            "supportedGenerationMethods": ["generateContent"]
                        },
                        {
                            "name": "models/gemini-3.5-transcribe-live",
                            "supportedGenerationMethods": ["bidiGenerateContent"]
                        }
                    ],
                    "nextPageToken": "page+2/tok="
                }),
            ),
            ("GET", "/v1beta/models?pageSize=100&pageToken=page%2B2%2Ftok%3D") => (
                200,
                vec![],
                json!({
                    "models": [
                        {
                            "name": "models/gemini-3.5-transcribe",
                            "supportedGenerationMethods": []
                        },
                        {
                            "name": "models/gemini-3.8-flash",
                            "supportedGenerationMethods": ["generateContent"]
                        },
                        {
                            "name": "models/gemini-2.5-flash-native-audio-preview",
                            "supportedGenerationMethods": ["bidiGenerateContent"]
                        }
                    ]
                }),
            ),
            _ => (404, vec![], json!({})),
        });

        let listed = list_models_at(&base, "AIza-list").unwrap();
        let live_ids: Vec<&str> = listed.live.iter().map(|m| m.id.as_str()).collect();
        let refine_ids: Vec<&str> = listed.refine.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            live_ids,
            vec![
                "gemini-3.5-transcribe-live",
                "gemini-2.5-flash-native-audio-preview"
            ]
        );
        assert!(listed.live[0].is_default);
        assert_eq!(
            refine_ids,
            vec![
                "gemini-3.5-transcribe",
                "gemini-3.8-flash",
                "gemini-2.5-flash",
                "gemini-4.0-flash-preview"
            ]
        );
        assert!(listed.refine[0].is_default);

        let reqs = seen.lock().unwrap();
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0].api_key.as_deref(), Some("AIza-list"));
    }
}
