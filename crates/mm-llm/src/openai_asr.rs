//! OpenAI Speech-to-Text REST client (`/v1/audio/transcriptions`).

use std::cmp::Reverse;
use std::collections::HashSet;
use std::path::Path;

use mm_core::AudioOrigin;
use serde_json::Value;

use crate::byok::{decode_json_response, http_agent};
use crate::{AsrMode, AsrModelList, AsrProvider, HeardLine, LlmError, ModelOption, asr_pricing};

/// Default OpenAI API origin.
const OPENAI_API_BASE: &str = "https://api.openai.com";
/// Cap prompt hints so decoder conditioning stays concise.
const MAX_PROMPT_CHARS: usize = 600;

/// Transcript lines and metered usage returned by a cloud transcription request.
#[derive(Debug, Clone, PartialEq)]
pub struct CloudTranscriptResult {
    pub lines: Vec<HeardLine>,
    pub audio_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub model: String,
}

/// Request options for [`transcribe_file`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiTranscribeOptions {
    pub model: String,
    pub language: Option<String>,
    pub prompt: Option<String>,
}

/// Lists OpenAI transcription models available to `api_key`, separated into live & refine modes.
///
/// Params: `api_key` – OpenAI secret key.
/// Returns: sorted live and refine model options fetched from `/v1/models`.
pub fn list_models(api_key: &str) -> Result<AsrModelList, LlmError> {
    list_models_at(OPENAI_API_BASE, api_key)
}

/// Testable implementation of [`list_models`] against a configurable base URL.
///
/// Params: `base_url`; `api_key`.
/// Returns: sorted live and refine model options.
pub fn list_models_at(base_url: &str, api_key: &str) -> Result<AsrModelList, LlmError> {
    let url = format!("{}/v1/models", base_url.trim_end_matches('/'));
    let agent = http_agent(30);
    let resp = agent
        .get(&url)
        .header("authorization", format!("Bearer {}", api_key.trim()))
        .call()
        .map_err(|e| LlmError::Provider(format!("GET {url}: {e}")))?;
    let json = decode_json_response(&url, resp)?;

    let mut seen = HashSet::new();
    let mut ids = Vec::new();
    for entry in json["data"].as_array().into_iter().flatten() {
        let Some(raw_id) = entry["id"].as_str().map(str::trim) else {
            continue;
        };
        if raw_id.is_empty() || !seen.insert(raw_id.to_ascii_lowercase()) {
            continue;
        }
        ids.push(raw_id.to_string());
    }

    Ok(AsrModelList {
        live: filter_and_sort_models(&ids, AsrMode::Live, is_openai_live_asr_model),
        refine: filter_and_sort_models(&ids, AsrMode::Refine, is_openai_refine_asr_model),
    })
}

fn is_openai_live_asr_model(id: &str) -> bool {
    let lower = id.to_ascii_lowercase();
    (lower.contains("transcribe") || lower.contains("whisper"))
        && !lower.contains("diarize")
        && lower != "gpt-transcribe"
        && !lower.starts_with("gpt-transcribe-")
}

fn is_openai_refine_asr_model(id: &str) -> bool {
    let lower = id.to_ascii_lowercase();
    (lower.contains("transcribe") || lower.contains("whisper"))
        && !lower.contains("-live")
        && !lower.contains("realtime")
}

/// Splits a trailing `-YYYY-MM-DD` date snapshot suffix if present.
fn split_date_suffix(id: &str) -> (&str, Option<&str>) {
    let bytes = id.as_bytes();
    if bytes.len() >= 11 {
        let start = bytes.len() - 11;
        let tail = &bytes[start..];
        let is_date = tail[0] == b'-'
            && tail[1..5].iter().all(u8::is_ascii_digit)
            && tail[5] == b'-'
            && tail[6..8].iter().all(u8::is_ascii_digit)
            && tail[8] == b'-'
            && tail[9..11].iter().all(u8::is_ascii_digit);
        if is_date {
            return (&id[..start], Some(&id[start + 1..]));
        }
    }
    (id, None)
}

fn filter_and_sort_models(
    ids: &[String],
    mode: AsrMode,
    predicate: fn(&str) -> bool,
) -> Vec<ModelOption> {
    let catalog = asr_pricing::models_for(AsrProvider::OpenAi, mode);
    let default_id = asr_pricing::default_model(AsrProvider::OpenAi, mode);
    let mut matching: Vec<String> = ids.iter().filter(|id| predicate(id)).cloned().collect();
    matching.sort_by_key(|id| {
        let rank = catalog
            .iter()
            .position(|spec| spec.id.eq_ignore_ascii_case(id))
            .unwrap_or(usize::MAX);
        let (base, date) = split_date_suffix(id);
        (
            rank,
            date.is_some(),
            base.to_ascii_lowercase(),
            Reverse(date.map(str::to_string)),
        )
    });
    matching
        .into_iter()
        .map(|id| ModelOption {
            is_default: id.eq_ignore_ascii_case(default_id),
            label: id.clone(),
            id,
        })
        .collect()
}

/// Transcribes one audio file via OpenAI `/v1/audio/transcriptions`.
///
/// Params: `api_key` – OpenAI secret key; `origin` – stream origin assigned to returned lines;
/// `path` – audio file on disk (`.m4a`, `.wav`, `.mp3`, … ≤ 25 MB); `offset_ms` – start offset
/// of this chunk in the recording; `duration_ms` – chunk length used when the model does not
/// return per-segment timestamps; `opts` – model, language hint and optional prompt.
/// Returns: transcribed lines plus usage metrics.
pub fn transcribe_file(
    api_key: &str,
    origin: AudioOrigin,
    path: &Path,
    offset_ms: u64,
    duration_ms: u64,
    opts: &OpenAiTranscribeOptions,
) -> Result<CloudTranscriptResult, LlmError> {
    transcribe_file_at(
        OPENAI_API_BASE,
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
    opts: &OpenAiTranscribeOptions,
) -> Result<CloudTranscriptResult, LlmError> {
    let audio_bytes = std::fs::read(path)
        .map_err(|e| LlmError::Provider(format!("reading audio {}: {e}", path.display())))?;
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("audio.m4a");
    let mime = audio_mime(path);
    let model = opts.model.trim();
    let lower_model = model.to_ascii_lowercase();

    let boundary = format!(
        "----FristonBoundary{:016x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let mut form = MultipartBuilder::new(&boundary);
    form.file("file", file_name, mime, &audio_bytes);
    form.text("model", model);

    if let Some(lang) = normalize_iso639(opts.language.as_deref()) {
        form.text("language", lang);
    }

    if lower_model.contains("diarize") {
        // `gpt-4o-transcribe-diarize` rejects `prompt` and requires `chunking_strategy`.
        form.text("response_format", "diarized_json");
        form.text("chunking_strategy", "auto");
    } else if lower_model.starts_with("whisper") {
        form.text("response_format", "verbose_json");
        form.text("timestamp_granularities[]", "segment");
        if let Some(prompt) = clean_prompt(opts.prompt.as_deref()) {
            form.text("prompt", &prompt);
        }
    } else {
        form.text("response_format", "json");
        if let Some(prompt) = clean_prompt(opts.prompt.as_deref()) {
            form.text("prompt", &prompt);
        }
    }

    let body = form.finish();
    let url = format!("{}/v1/audio/transcriptions", base_url.trim_end_matches('/'));
    let agent = http_agent(600);
    let mut resp = agent
        .post(&url)
        .header("authorization", format!("Bearer {}", api_key.trim()))
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .send(body.as_slice())
        .map_err(|e| LlmError::Provider(format!("POST {url}: {e}")))?;

    let status = resp.status();
    let text = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| LlmError::Provider(format!("reading response from {url}: {e}")))?;
    if !status.is_success() {
        if status == 429
            && (text.contains("insufficient_quota") || text.contains("credit_balance_exhausted"))
        {
            return Err(LlmError::Provider(format!(
                "OpenAI API credits/quota exhausted (HTTP 429): {text}"
            )));
        }
        return Err(LlmError::Provider(format!(
            "HTTP {status} from {url}: {text}"
        )));
    }

    let json: Value = serde_json::from_str(&text)
        .map_err(|e| LlmError::Provider(format!("decoding response from {url}: {e}")))?;
    Ok(parse_response(&json, origin, offset_ms, duration_ms, model))
}

/// Parses OpenAI `json`, `verbose_json`, or `diarized_json` responses into [`CloudTranscriptResult`].
fn parse_response(
    json: &Value,
    origin: AudioOrigin,
    offset_ms: u64,
    duration_ms: u64,
    model: &str,
) -> CloudTranscriptResult {
    let mut lines = Vec::new();
    if let Some(segments) = json["segments"].as_array() {
        for seg in segments {
            let text = seg["text"].as_str().unwrap_or("").trim();
            if text.is_empty() {
                continue;
            }
            let start_s = seg["start"].as_f64().unwrap_or(0.0).max(0.0);
            let end_s = seg["end"].as_f64().unwrap_or(start_s).max(start_s);
            let start_ms = offset_ms.saturating_add((start_s * 1000.0).round() as u64);
            let end_ms = offset_ms
                .saturating_add((end_s * 1000.0).round() as u64)
                .max(start_ms);
            let speaker = seg["speaker"].as_str().unwrap_or("").trim().to_string();
            lines.push(HeardLine {
                source: origin,
                start_ms,
                end_ms,
                speaker,
                text: text.to_string(),
            });
        }
    } else if let Some(text) = json["text"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        lines.push(HeardLine {
            source: origin,
            start_ms: offset_ms,
            end_ms: offset_ms.saturating_add(duration_ms),
            speaker: String::new(),
            text: text.to_string(),
        });
    }

    let (input_tokens, output_tokens, usage_ms) = extract_usage(json);
    CloudTranscriptResult {
        lines,
        audio_ms: usage_ms.unwrap_or(duration_ms),
        input_tokens,
        output_tokens,
        model: model.to_string(),
    }
}

/// Extracts `(input_tokens, output_tokens, duration_ms)` from OpenAI response JSON.
fn extract_usage(json: &Value) -> (u64, u64, Option<u64>) {
    let usage = &json["usage"];
    let input_tokens = usage["input_tokens"].as_u64().unwrap_or(0);
    let output_tokens = usage["output_tokens"].as_u64().unwrap_or(0);
    let duration_ms = usage["seconds"]
        .as_f64()
        .or_else(|| json["duration"].as_f64())
        .filter(|s| s.is_finite() && *s > 0.0)
        .map(|s| (s * 1000.0).round() as u64);
    (input_tokens, output_tokens, duration_ms)
}

/// Normalizes a BCP-47 hint (`"zh-Hans-CN"`) into OpenAI's ISO-639-1 primary subtag (`"zh"`).
fn normalize_iso639(lang: Option<&str>) -> Option<&str> {
    let raw = lang?.trim();
    if raw.is_empty() || raw.eq_ignore_ascii_case("auto") {
        return None;
    }
    raw.split(['-', '_'])
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Trims and caps an optional prompt hint on a char boundary.
fn clean_prompt(prompt: Option<&str>) -> Option<String> {
    let clean = prompt?.trim();
    if clean.is_empty() {
        return None;
    }
    Some(clean.chars().take(MAX_PROMPT_CHARS).collect())
}

/// Guesses audio MIME type from the file extension.
fn audio_mime(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("m4a" | "mp4") => "audio/mp4",
        Some("wav") => "audio/wav",
        Some("mp3" | "mpeg") => "audio/mpeg",
        Some("webm") => "audio/webm",
        Some("caf") => "audio/x-caf",
        _ => "application/octet-stream",
    }
}

/// Minimal RFC 7578 `multipart/form-data` encoder.
struct MultipartBuilder<'a> {
    boundary: &'a str,
    buf: Vec<u8>,
}

impl<'a> MultipartBuilder<'a> {
    fn new(boundary: &'a str) -> Self {
        Self {
            boundary,
            buf: Vec::new(),
        }
    }

    fn text(&mut self, name: &str, value: &str) {
        self.buf
            .extend_from_slice(format!("--{}\r\n", self.boundary).as_bytes());
        self.buf.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n")
                .as_bytes(),
        );
    }

    fn file(&mut self, name: &str, filename: &str, mime: &str, bytes: &[u8]) {
        let safe_name = filename.replace(['"', '\r', '\n'], "_");
        self.buf
            .extend_from_slice(format!("--{}\r\n", self.boundary).as_bytes());
        self.buf.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"{name}\"; filename=\"{safe_name}\"\r\n\
                 Content-Type: {mime}\r\n\r\n"
            )
            .as_bytes(),
        );
        self.buf.extend_from_slice(bytes);
        self.buf.extend_from_slice(b"\r\n");
    }

    fn finish(mut self) -> Vec<u8> {
        self.buf
            .extend_from_slice(format!("--{}--\r\n", self.boundary).as_bytes());
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::*;

    type CapturedRequest = (String, Option<String>, Vec<u8>);

    fn mock_openai(status: u16, response: Value) -> (String, Arc<Mutex<Vec<CapturedRequest>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen: Arc<Mutex<Vec<CapturedRequest>>> = Arc::default();
        let log = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut req_line = String::new();
                if reader.read_line(&mut req_line).unwrap_or(0) == 0 {
                    continue;
                }
                let path = req_line.split_whitespace().nth(1).unwrap_or("").to_string();
                let mut auth = None;
                let mut content_len = 0_usize;
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    let h = h.trim_end();
                    if h.is_empty() {
                        break;
                    }
                    if let Some((k, v)) = h.split_once(": ") {
                        if k.eq_ignore_ascii_case("authorization") {
                            auth = Some(v.to_string());
                        } else if k.eq_ignore_ascii_case("content-length") {
                            content_len = v.parse().unwrap_or(0);
                        }
                    }
                }
                let mut body = vec![0u8; content_len];
                reader.read_exact(&mut body).unwrap();
                log.lock().unwrap().push((path, auth, body));
                let raw = response.to_string();
                let head = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    raw.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(raw.as_bytes());
            }
        });
        (base, seen)
    }

    #[test]
    fn gpt_transcribe_sends_prompt_and_extracts_usage() {
        let dir = tempfile::tempdir().unwrap();
        let audio = dir.path().join("utt.m4a");
        std::fs::write(&audio, b"fake-aac").unwrap();
        let (base, seen) = mock_openai(
            200,
            json!({
                "text": "今天讨论预算上调。",
                "usage": {"type": "duration", "seconds": 12.5}
            }),
        );
        let res = transcribe_file_at(
            &base,
            "sk-test",
            AudioOrigin::Microphone,
            &audio,
            5_000,
            12_000,
            &OpenAiTranscribeOptions {
                model: "gpt-transcribe".into(),
                language: Some("zh-Hans".into()),
                prompt: Some("参会人：赵磊".into()),
            },
        )
        .unwrap();

        assert_eq!(res.lines.len(), 1);
        assert_eq!(res.lines[0].start_ms, 5_000);
        assert_eq!(res.lines[0].end_ms, 17_000);
        assert_eq!(res.lines[0].text, "今天讨论预算上调。");
        assert_eq!(res.audio_ms, 12_500);

        let reqs = seen.lock().unwrap();
        assert_eq!(reqs[0].0, "/v1/audio/transcriptions");
        assert_eq!(reqs[0].1.as_deref(), Some("Bearer sk-test"));
        let body_str = String::from_utf8_lossy(&reqs[0].2);
        assert!(body_str.contains("name=\"model\"\r\n\r\ngpt-transcribe"));
        assert!(body_str.contains("name=\"language\"\r\n\r\nzh"));
        assert!(body_str.contains("name=\"response_format\"\r\n\r\njson"));
        assert!(body_str.contains("参会人：赵磊"));
    }

    #[test]
    fn diarize_omits_prompt_and_parses_speaker_segments() {
        let dir = tempfile::tempdir().unwrap();
        let audio = dir.path().join("chunk.wav");
        std::fs::write(&audio, b"RIFF").unwrap();
        let (base, seen) = mock_openai(
            200,
            json!({
                "duration": 9.2,
                "segments": [
                    {"start": 0.5, "end": 3.0, "speaker": "A", "text": "大家好"},
                    {"start": 3.5, "end": 8.0, "speaker": "B", "text": "同意"}
                ],
                "usage": {"type": "tokens", "input_tokens": 200, "output_tokens": 25}
            }),
        );
        let res = transcribe_file_at(
            &base,
            "sk-test",
            AudioOrigin::System,
            &audio,
            60_000,
            9_200,
            &OpenAiTranscribeOptions {
                model: "gpt-4o-transcribe-diarize".into(),
                language: None,
                prompt: Some("should not be sent".into()),
            },
        )
        .unwrap();

        assert_eq!(res.lines.len(), 2);
        assert_eq!(res.lines[0].source, AudioOrigin::System);
        assert_eq!(res.lines[0].start_ms, 60_500);
        assert_eq!(res.lines[0].end_ms, 63_000);
        assert_eq!(res.lines[0].speaker, "A");
        assert_eq!(res.lines[1].speaker, "B");
        assert_eq!(res.input_tokens, 200);
        assert_eq!(res.output_tokens, 25);

        let reqs = seen.lock().unwrap();
        let body_str = String::from_utf8_lossy(&reqs[0].2);
        assert!(body_str.contains("diarized_json"));
        assert!(body_str.contains("chunking_strategy"));
        assert!(!body_str.contains("should not be sent"));
    }

    #[test]
    fn whisper_requests_verbose_json_segments_and_surfaces_quota_errors() {
        let dir = tempfile::tempdir().unwrap();
        let audio = dir.path().join("a.wav");
        std::fs::write(&audio, b"RIFF").unwrap();
        let (base, seen) = mock_openai(
            200,
            json!({
                "duration": 4.0,
                "segments": [{"start": 0.0, "end": 2.5, "text": "hello"}]
            }),
        );
        let res = transcribe_file_at(
            &base,
            "sk-test",
            AudioOrigin::Microphone,
            &audio,
            0,
            4_000,
            &OpenAiTranscribeOptions {
                model: "whisper-1".into(),
                language: Some("en".into()),
                prompt: None,
            },
        )
        .unwrap();
        assert_eq!(res.lines[0].end_ms, 2_500);
        let reqs = seen.lock().unwrap();
        let body_str = String::from_utf8_lossy(&reqs[0].2);
        assert!(body_str.contains("verbose_json"));
        assert!(body_str.contains("timestamp_granularities[]"));

        let (err_base, _) = mock_openai(
            429,
            json!({"error": {"code": "credit_balance_exhausted", "message": "No credits"}}),
        );
        let err = transcribe_file_at(
            &err_base,
            "sk-test",
            AudioOrigin::Microphone,
            &audio,
            0,
            4_000,
            &OpenAiTranscribeOptions {
                model: "gpt-transcribe".into(),
                language: None,
                prompt: None,
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("exhausted") && err.contains("429"), "{err}");
    }

    #[test]
    fn list_models_filters_and_orders_live_and_refine_and_surfaces_auth_errors() {
        let (base, seen) = mock_openai(
            200,
            json!({
                "data": [
                    {"id": "gpt-5"},
                    {"id": "gpt-audio"},
                    {"id": "gpt-realtime-2"},
                    {"id": "tts-1"},
                    {"id": "text-embedding-3-small"},
                    {"id": "gpt-4o-mini-transcribe-2025-03-20"},
                    {"id": "gpt-4o-mini-transcribe-2025-12-15"},
                    {"id": "gpt-5-transcribe"},
                    {"id": "whisper-1"},
                    {"id": "gpt-realtime-whisper"},
                    {"id": "gpt-4o-transcribe-diarize"},
                    {"id": "gpt-4o-mini-transcribe"},
                    {"id": "gpt-4o-transcribe"},
                    {"id": "gpt-transcribe"},
                    {"id": "gpt-live-transcribe"}
                ]
            }),
        );

        let listed = list_models_at(&base, "sk-test-list").unwrap();
        let live_ids: Vec<&str> = listed.live.iter().map(|m| m.id.as_str()).collect();
        let refine_ids: Vec<&str> = listed.refine.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            live_ids,
            vec![
                "gpt-live-transcribe",
                "gpt-4o-transcribe",
                "gpt-4o-mini-transcribe",
                "gpt-5-transcribe",
                "gpt-realtime-whisper",
                "whisper-1",
                "gpt-4o-mini-transcribe-2025-12-15",
                "gpt-4o-mini-transcribe-2025-03-20",
            ]
        );
        assert!(listed.live[0].is_default);
        assert_eq!(
            refine_ids,
            vec![
                "gpt-transcribe",
                "gpt-4o-transcribe-diarize",
                "gpt-4o-transcribe",
                "gpt-4o-mini-transcribe",
                "whisper-1",
                "gpt-5-transcribe",
                "gpt-4o-mini-transcribe-2025-12-15",
                "gpt-4o-mini-transcribe-2025-03-20",
            ]
        );
        assert!(listed.refine[0].is_default);

        let reqs = seen.lock().unwrap();
        assert_eq!(reqs[0].0, "/v1/models");
        assert_eq!(reqs[0].1.as_deref(), Some("Bearer sk-test-list"));

        let (err_base, _) = mock_openai(401, json!({"error": {"message": "Incorrect API key"}}));
        let err = list_models_at(&err_base, "sk-bad").unwrap_err().to_string();
        assert!(err.contains("401"), "{err}");
    }
}
