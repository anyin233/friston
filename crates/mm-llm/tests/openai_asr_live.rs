//! Live end-to-end checks against OpenAI Speech-to-Text & Costs APIs.
//!
//! Ignored by default; run with:
//! `OPENAI_API_KEY=... cargo test -p mm-llm --test openai_asr_live -- --ignored --nocapture`

use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::Utc;
use mm_core::AudioOrigin;
use mm_llm::asr_pricing::{self, AsrMode, AsrProvider, DEFAULT_OPENAI_REFINE_MODEL};
use mm_llm::openai_asr::{OpenAiTranscribeOptions, transcribe_file};
use mm_llm::openai_costs::fetch_openai_costs;

/// Reads `OPENAI_API_KEY` from the environment (never hardcoded).
fn require_openai_key() -> String {
    std::env::var("OPENAI_API_KEY")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .expect("set OPENAI_API_KEY before running ignored OpenAI live tests")
}

/// Synthesizes a 16 kHz mono AAC `.m4a` clip using macOS `say` + `/usr/bin/afconvert`.
fn synthesize_m4a(dir: &Path, text: &str) -> PathBuf {
    let aiff = dir.join("speech.aiff");
    let m4a = dir.join("speech.m4a");
    let say_ok = Command::new("say")
        .args(["-v", "Tingting", "-o"])
        .arg(&aiff)
        .arg(text)
        .status()
        .expect("run say")
        .success();
    assert!(say_ok, "macOS `say` failed");

    let conv_ok = Command::new("/usr/bin/afconvert")
        .args(["-f", "m4af", "-d", "aac", "-c", "1", "-r", "16000"])
        .arg(&aiff)
        .arg(&m4a)
        .status()
        .expect("run afconvert")
        .success();
    assert!(conv_ok, "`/usr/bin/afconvert` failed");
    m4a
}

#[test]
#[ignore = "calls live OpenAI Audio Transcription API with OPENAI_API_KEY"]
fn openai_transcribe_live_models() {
    let api_key = require_openai_key();
    let dir = tempfile::tempdir().expect("tempdir");
    let m4a = synthesize_m4a(
        dir.path(),
        "大家好，我们开始周会。下季度预算上调百分之五，请 Alice 在周五之前更新预算表。",
    );

    let models = [
        DEFAULT_OPENAI_REFINE_MODEL,
        "gpt-4o-transcribe",
        "gpt-4o-mini-transcribe",
        "gpt-4o-transcribe-diarize",
        "whisper-1",
    ];

    for model in models {
        let out = transcribe_file(
            &api_key,
            AudioOrigin::Microphone,
            &m4a,
            0,
            6_000,
            &OpenAiTranscribeOptions {
                model: model.to_string(),
                language: Some("zh".into()),
                prompt: Some("会议：周会；参会人：Alice；关键词：预算".into()),
            },
        );
        match out {
            Ok(res) => {
                let text: String = res
                    .lines
                    .iter()
                    .map(|l| l.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" ");
                let usd = asr_pricing::estimate_cost_usd(
                    AsrProvider::OpenAi,
                    AsrMode::Refine,
                    model,
                    res.audio_ms,
                    res.input_tokens,
                    res.output_tokens,
                );
                println!(
                    "[OK] model={model} segments={} audio_ms={} in_tok={} out_tok={} est_usd=${usd:.6} text={text:?}",
                    res.lines.len(),
                    res.audio_ms,
                    res.input_tokens,
                    res.output_tokens,
                );
                assert!(!res.lines.is_empty(), "{model}: returned zero lines");
                assert!(!text.trim().is_empty(), "{model}: returned empty text");
                assert!(
                    text.contains("预算")
                        || text.to_ascii_lowercase().contains("alice")
                        || text.contains("周会"),
                    "{model}: unexpected transcript content: {text:?}"
                );
            }
            Err(err) => panic!("[FAIL] model={model}: {err}"),
        }
    }
}

#[test]
#[ignore = "calls live OpenAI Organization Costs API with OPENAI_API_KEY"]
fn openai_costs_rejects_project_key_with_helpful_error() {
    let api_key = require_openai_key();
    let now = Utc::now().timestamp();
    let err = fetch_openai_costs(&api_key, now - 86_400, Some(now))
        .expect_err("project API key should not be authorized for /v1/organization/costs");
    let msg = err.to_string();
    println!("costs error with project key: {msg}");
    assert!(
        msg.contains("Admin") || msg.contains("401") || msg.contains("403"),
        "expected helpful Admin/401/403 error message, got: {msg}"
    );
}

#[test]
#[ignore = "calls live OpenAI /v1/models API with OPENAI_API_KEY"]
fn openai_list_models_live() {
    let api_key = require_openai_key();
    let listed = mm_llm::openai_asr::list_models(&api_key).expect("list OpenAI ASR models");
    let live_ids: Vec<&str> = listed.live.iter().map(|m| m.id.as_str()).collect();
    let refine_ids: Vec<&str> = listed.refine.iter().map(|m| m.id.as_str()).collect();
    println!("OpenAI live ASR models ({}): {live_ids:?}", live_ids.len());
    println!(
        "OpenAI refine ASR models ({}): {refine_ids:?}",
        refine_ids.len()
    );
    assert!(!live_ids.is_empty(), "expected non-empty OpenAI live list");
    assert!(
        !refine_ids.is_empty(),
        "expected non-empty OpenAI refine list"
    );
    assert!(
        live_ids
            .iter()
            .any(|id| *id == "gpt-live-transcribe" || *id == "gpt-4o-transcribe"),
        "expected live transcription model in {live_ids:?}"
    );
    assert!(
        refine_ids
            .iter()
            .any(|id| *id == "gpt-transcribe" || *id == "whisper-1"),
        "expected refine transcription model in {refine_ids:?}"
    );
}
