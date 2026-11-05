//! Structured minutes produced by any provider.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// A follow-up task extracted from the meeting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionItem {
    pub owner: Option<String>,
    pub task: String,
    pub due: Option<String>,
    /// Transcript timestamp (ms) supporting this item, for click-to-seek.
    pub evidence_ms: Option<u64>,
}

/// Minutes document; rendered into the note's managed blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Minutes {
    pub title: String,
    pub summary: String,
    pub decisions: Vec<String>,
    pub action_items: Vec<ActionItem>,
    pub open_questions: Vec<String>,
    pub topics: Vec<String>,
}

/// JSON Schema handed to providers that support constrained output (Codex `outputSchema`,
/// OpenAI `response_format`, Gemini `responseSchema`).
///
/// Returns: a strict draft-07 schema matching [`Minutes`].
pub fn minutes_json_schema() -> Value {
    let strings = json!({"type": "array", "items": {"type": "string"}});
    // anyOf instead of a type array: every provider's constrained decoder accepts it.
    let nullable_string = json!({"anyOf": [{"type": "string"}, {"type": "null"}]});
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["title", "summary", "decisions", "action_items", "open_questions", "topics"],
        "properties": {
            "title": {"type": "string"},
            "summary": {"type": "string"},
            "decisions": strings,
            "open_questions": strings,
            "topics": strings,
            "action_items": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["owner", "task", "due", "evidence_ms"],
                    "properties": {
                        "owner": nullable_string,
                        "task": {"type": "string"},
                        "due": nullable_string,
                        "evidence_ms": {"anyOf": [{"type": "integer"}, {"type": "null"}]}
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_requires_every_field_of_minutes() {
        let schema = minutes_json_schema();
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        let sample = Minutes {
            title: "t".into(),
            summary: "s".into(),
            decisions: vec![],
            action_items: vec![],
            open_questions: vec![],
            topics: vec![],
        };
        let fields = serde_json::to_value(sample).unwrap();
        for key in fields.as_object().unwrap().keys() {
            assert!(required.contains(&key.as_str()), "schema misses {key}");
        }
    }
}
