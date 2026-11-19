//! OpenAI Organization Costs API client (`/v1/organization/costs`).
//!
//! Standard project API keys cannot read account balances or spend; an Organization Admin
//! API key (`sk-admin-...`) is required for `/v1/organization/costs`.

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::LlmError;
use crate::byok::{decode_json_response, http_agent};

/// Default OpenAI API origin.
const OPENAI_API_BASE: &str = "https://api.openai.com";
/// Safety cap on paginated `/v1/organization/costs` requests.
const MAX_PAGES: usize = 20;

/// Aggregated OpenAI organization spend over a queried time window.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenAiCostsReport {
    pub total_usd: f64,
    pub buckets: usize,
    pub start_time_unix: i64,
    pub end_time_unix: i64,
    pub fetched_at: DateTime<Utc>,
}

/// Fetches total USD spend from `GET /v1/organization/costs` starting at `start_time_unix`.
///
/// Params: `admin_key` – OpenAI Organization Admin API key (`sk-admin-...`);
/// `start_time_unix` – inclusive Unix timestamp (seconds); `end_time_unix` – optional
/// exclusive end Unix timestamp (defaults to now for metadata when `None`).
/// Returns: aggregated USD spend across all pages (up to 20 pages of 180 daily buckets).
pub fn fetch_openai_costs(
    admin_key: &str,
    start_time_unix: i64,
    end_time_unix: Option<i64>,
) -> Result<OpenAiCostsReport, LlmError> {
    fetch_openai_costs_from_base(OPENAI_API_BASE, admin_key, start_time_unix, end_time_unix)
}

/// Testable implementation of [`fetch_openai_costs`] against a configurable base URL.
///
/// Params: `base_url`; `admin_key`; `start_time_unix`; `end_time_unix`.
/// Returns: aggregated [`OpenAiCostsReport`].
pub fn fetch_openai_costs_from_base(
    base_url: &str,
    admin_key: &str,
    start_time_unix: i64,
    end_time_unix: Option<i64>,
) -> Result<OpenAiCostsReport, LlmError> {
    let base = base_url.trim_end_matches('/');
    let agent = http_agent(30);
    let mut total_usd = 0.0_f64;
    let mut buckets = 0_usize;
    let mut page: Option<String> = None;

    for _ in 0..MAX_PAGES {
        let mut url = format!(
            "{base}/v1/organization/costs?start_time={start_time_unix}&bucket_width=1d&limit=180"
        );
        if let Some(end) = end_time_unix {
            url.push_str(&format!("&end_time={end}"));
        }
        if let Some(cursor) = &page {
            url.push_str(&format!("&page={}", url_encode(cursor)));
        }

        let resp = agent
            .get(&url)
            .header("authorization", format!("Bearer {}", admin_key.trim()))
            .header("content-type", "application/json")
            .call()
            .map_err(|e| LlmError::Provider(format!("GET {url}: {e}")))?;

        let status = resp.status();
        if status == 401 || status == 403 {
            let body = resp.into_body().read_to_string().unwrap_or_default();
            return Err(LlmError::Provider(format!(
                "HTTP {status} from {url}: OpenAI /v1/organization/costs requires an Organization Admin API key (sk-admin-...), not a project API key: {body}"
            )));
        }

        let json = decode_json_response(&url, resp)?;
        if let Some(data) = json["data"].as_array() {
            buckets += data.len();
            for bucket in data {
                if let Some(results) = bucket["results"].as_array() {
                    for item in results {
                        if let Some(val) = parse_amount_usd(&item["amount"]["value"]) {
                            total_usd += val;
                        }
                    }
                }
            }
        }

        let has_more = json["has_more"].as_bool().unwrap_or(false);
        let next_page = json["next_page"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);
        if has_more && let Some(next) = next_page {
            page = Some(next);
        } else {
            break;
        }
    }

    let now = Utc::now();
    Ok(OpenAiCostsReport {
        total_usd,
        buckets,
        start_time_unix,
        end_time_unix: end_time_unix.unwrap_or_else(|| now.timestamp()),
        fetched_at: now,
    })
}

/// Parses `amount.value` whether OpenAI encodes it as a JSON number or string.
fn parse_amount_usd(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
        .filter(|x| x.is_finite())
}

/// Minimal percent-encoding for pagination cursors in query parameters.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::*;

    type CapturedRequest = (String, Option<String>);

    fn mock_server(
        routes: HashMap<String, (u16, Value)>,
    ) -> (String, Arc<Mutex<Vec<CapturedRequest>>>) {
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
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    let h = h.trim_end();
                    if h.is_empty() {
                        break;
                    }
                    if let Some((k, v)) = h.split_once(": ")
                        && k.eq_ignore_ascii_case("authorization")
                    {
                        auth = Some(v.to_string());
                    }
                }
                log.lock().unwrap().push((path.clone(), auth));
                let (status, body) = routes.get(&path).cloned().unwrap_or((404, json!({})));
                let raw = body.to_string();
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
    fn aggregates_paginated_float_and_string_costs() {
        let routes = HashMap::from([
            (
                "/v1/organization/costs?start_time=1000&bucket_width=1d&limit=180".into(),
                (
                    200,
                    json!({
                        "object": "page",
                        "data": [
                            {"object": "bucket", "results": [
                                {"amount": {"value": 1.25, "currency": "usd"}},
                                {"amount": {"value": "0.50", "currency": "usd"}}
                            ]}
                        ],
                        "has_more": true,
                        "next_page": "cur+2"
                    }),
                ),
            ),
            (
                "/v1/organization/costs?start_time=1000&bucket_width=1d&limit=180&page=cur%2B2"
                    .into(),
                (
                    200,
                    json!({
                        "object": "page",
                        "data": [
                            {"object": "bucket", "results": [
                                {"amount": {"value": 0.75, "currency": "usd"}}
                            ]},
                            {"object": "bucket", "results": []}
                        ],
                        "has_more": false,
                        "next_page": null
                    }),
                ),
            ),
        ]);
        let (base, seen) = mock_server(routes);
        let report = fetch_openai_costs_from_base(&base, "sk-admin-xyz", 1000, None).unwrap();
        assert!((report.total_usd - 2.50).abs() < 1e-9);
        assert_eq!(report.buckets, 3);
        let reqs = seen.lock().unwrap();
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0].1.as_deref(), Some("Bearer sk-admin-xyz"));
    }

    #[test]
    fn surfaces_helpful_error_for_non_admin_keys() {
        let routes = HashMap::from([(
            "/v1/organization/costs?start_time=1000&bucket_width=1d&limit=180&end_time=2000".into(),
            (
                403,
                json!({"error": {"message": "Insufficient permissions"}}),
            ),
        )]);
        let (base, _) = mock_server(routes);
        let err = fetch_openai_costs_from_base(&base, "sk-proj-bad", 1000, Some(2000))
            .unwrap_err()
            .to_string();
        assert!(err.contains("403"), "{err}");
        assert!(err.contains("sk-admin-"), "{err}");
    }
}
