//! JSON-RPC 2.0 message shapes (newline-delimited, one object per line).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A request sent from the app to an engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
}

impl RpcRequest {
    /// Builds a JSON-RPC 2.0 request.
    ///
    /// Params: `id` – correlation id; `method` – method name; `params` – JSON params.
    /// Returns: the request value.
    pub fn new(id: u64, method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            method: method.into(),
            params,
        }
    }
}

/// JSON-RPC error object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error("engine error {code}: {message}")]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Any message an engine can write to stdout.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// Successful response to request `id`.
    Result { id: u64, result: Value },
    /// Failed response to request `id`.
    Error { id: u64, error: RpcError },
    /// Server-initiated notification (captions, levels, capture state, progress …).
    Notification { method: String, params: Value },
}

/// Wire form used only for decoding; `Incoming` is the validated form.
#[derive(Deserialize)]
struct RawIncoming {
    id: Option<u64>,
    method: Option<String>,
    #[serde(default)]
    params: Value,
    result: Option<Value>,
    error: Option<RpcError>,
}

impl Incoming {
    /// Parses one stdout line from an engine.
    ///
    /// Params: `line` – a single JSON object without the trailing newline.
    /// Returns: the decoded message, or a descriptive error for malformed input.
    pub fn parse(line: &str) -> Result<Self, String> {
        let raw: RawIncoming = serde_json::from_str(line)
            .map_err(|e| format!("invalid JSON-RPC line {line:?}: {e}"))?;
        match (raw.id, raw.method, raw.result, raw.error) {
            (Some(id), None, _, Some(error)) => Ok(Incoming::Error { id, error }),
            (Some(id), None, result, None) => Ok(Incoming::Result {
                id,
                result: result.unwrap_or(Value::Null),
            }),
            (None, Some(method), _, _) => Ok(Incoming::Notification {
                method,
                params: raw.params,
            }),
            _ => Err(format!("unsupported JSON-RPC message shape: {line:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_result_error_and_notification() {
        assert_eq!(
            Incoming::parse(r#"{"jsonrpc":"2.0","id":3,"result":{"ok":true}}"#).unwrap(),
            Incoming::Result {
                id: 3,
                result: json!({"ok": true})
            }
        );
        let err =
            Incoming::parse(r#"{"jsonrpc":"2.0","id":4,"error":{"code":-32601,"message":"nope"}}"#)
                .unwrap();
        assert!(matches!(err, Incoming::Error { id: 4, ref error } if error.code == -32601));
        let n = Incoming::parse(
            r#"{"jsonrpc":"2.0","method":"caption/partial","params":{"text":"hi"}}"#,
        )
        .unwrap();
        assert!(
            matches!(n, Incoming::Notification { ref method, .. } if method == "caption/partial")
        );
    }

    #[test]
    fn rejects_garbage_with_context() {
        let e = Incoming::parse("not json").unwrap_err();
        assert!(
            e.contains("not json"),
            "error should quote the offending line: {e}"
        );
    }
}
