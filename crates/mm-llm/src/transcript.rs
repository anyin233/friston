//! Provider-agnostic types and JSON Schemas for external audio transcription + minutes.

use std::path::PathBuf;

use mm_core::AudioOrigin;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{LlmError, Minutes, minutes_json_schema};

/// One recorded audio file passed to an external transcription/summary backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioTrack {
    pub origin: AudioOrigin,
    /// Absolute path to the recording file on disk (e.g. `microphone.caf` or `.wav`).
    pub path: PathBuf,
}

/// One utterance heard in the recording.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeardLine {
    /// Which recorded stream the sentence came from.
    pub source: AudioOrigin,
    /// Offsets from the start of the recording, in milliseconds.
    pub start_ms: u64,
    pub end_ms: u64,
    /// Speaker label assigned within the run (e.g. `"说话人1"`).
    pub speaker: String,
    pub text: String,
}

/// Strict draft-07 schema for `{"transcript": [HeardLine…]}`.
///
/// Returns: a JSON Schema whose root is an object.
pub fn transcript_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["transcript"],
        "properties": {
            "transcript": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["source", "start_ms", "end_ms", "speaker", "text"],
                    "properties": {
                        "source": {"type": "string", "enum": ["microphone", "system"]},
                        "start_ms": {"type": "integer"},
                        "end_ms": {"type": "integer"},
                        "speaker": {"type": "string"},
                        "text": {"type": "string"}
                    }
                }
            }
        }
    })
}

/// Strict draft-07 schema for a combined transcribe-and-summarize run:
/// `{"transcript": [HeardLine…], "minutes": Minutes}`.
///
/// Returns: a JSON Schema whose root is an object.
pub fn transcript_and_minutes_schema() -> Value {
    let transcript = transcript_schema()["properties"]["transcript"].clone();
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["transcript", "minutes"],
        "properties": {
            "transcript": transcript,
            "minutes": minutes_json_schema()
        }
    })
}

/// Strips an optional surrounding Markdown code fence from model output.
fn strip_code_fence(text: &str) -> &str {
    let t = text.trim();
    t.strip_prefix("```json")
        .or_else(|| t.strip_prefix("```"))
        .map(|r| r.trim_end().trim_end_matches("```"))
        .unwrap_or(t)
        .trim()
}

/// Parses the JSON output of a unified transcribe-and-summarize run.
///
/// Empty utterances and inverted timestamps (`end_ms < start_ms`) are filtered out of the
/// transcript. If `minutes` fails schema decoding while `transcript` is valid, the
/// transcript is still returned alongside the `minutes` error.
///
/// Params: `text` – raw assistant output (optionally wrapped in a Markdown code fence).
/// Returns: `(heard_lines, minutes_result)`.
pub fn parse_transcript_and_minutes(
    text: &str,
) -> Result<(Vec<HeardLine>, Result<Minutes, LlmError>), LlmError> {
    let clean = strip_code_fence(text);
    let value: Value = serde_json::from_str(clean).map_err(|e| {
        let head: String = text.chars().take(200).collect();
        LlmError::Codex(format!("output is not valid JSON ({e}): {head}"))
    })?;
    let raw_lines: Vec<HeardLine> = serde_json::from_value(value["transcript"].clone())
        .map_err(|e| LlmError::Codex(format!("transcript did not match the schema ({e})")))?;
    let lines = raw_lines
        .into_iter()
        .filter(|l| !l.text.trim().is_empty() && l.end_ms >= l.start_ms)
        .collect();
    let minutes = serde_json::from_value(value["minutes"].clone())
        .map_err(|e| LlmError::Codex(format!("minutes did not match the schema ({e})")));
    Ok((lines, minutes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_requires_transcript_and_minutes() {
        let schema = transcript_and_minutes_schema();
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["required"], json!(["transcript", "minutes"]));
        assert_eq!(schema["properties"]["minutes"], minutes_json_schema());

        let t_schema = transcript_schema();
        assert_eq!(t_schema["additionalProperties"], false);
        assert_eq!(t_schema["required"], json!(["transcript"]));
    }

    #[test]
    fn parse_handles_fences_filters_bad_lines_and_preserves_transcript_on_minutes_error() {
        let payload = r#"```json
{
  "transcript": [
    {"source": "microphone", "start_ms": 100, "end_ms": 900, "speaker": "说话人1", "text": "大家好"},
    {"source": "microphone", "start_ms": 1000, "end_ms": 1200, "speaker": "说话人1", "text": "   "},
    {"source": "system", "start_ms": 2000, "end_ms": 1500, "speaker": "说话人2", "text": "倒置时间戳"},
    {"source": "system", "start_ms": 2500, "end_ms": 4000, "speaker": "说话人2", "text": "确认上线"}
  ],
  "minutes": {
    "title": "周会",
    "summary": "确认上线。",
    "decisions": ["按期上线"],
    "action_items": [{"owner": "赵磊", "task": "上线", "due": "周五", "evidence_ms": 2500}],
    "open_questions": [],
    "topics": ["上线计划"]
  }
}
```"#;
        let (lines, minutes) = parse_transcript_and_minutes(payload).unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].source, AudioOrigin::Microphone);
        assert_eq!(lines[0].text, "大家好");
        assert_eq!(lines[1].source, AudioOrigin::System);
        assert_eq!(lines[1].text, "确认上线");
        let m = minutes.unwrap();
        assert_eq!(m.title, "周会");
        assert_eq!(m.action_items[0].evidence_ms, Some(2500));

        // Invalid minutes still keeps the valid transcript.
        let bad_minutes = json!({
            "transcript": [
                {"source": "microphone", "start_ms": 0, "end_ms": 500, "speaker": "说话人1", "text": "测试"}
            ],
            "minutes": {"title": "缺字段"}
        })
        .to_string();
        let (lines, minutes) = parse_transcript_and_minutes(&bad_minutes).unwrap();
        assert_eq!(lines.len(), 1);
        assert!(minutes.is_err());

        assert!(parse_transcript_and_minutes("not json").is_err());
    }
}
