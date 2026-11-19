//! Live end-to-end checks against Google Gemini Speech-to-Text REST & Interactions APIs.
//!
//! Ignored by default; run with:
//! `GEMINI_API_KEY=... cargo test -p mm-llm --test gemini_asr_live -- --ignored --nocapture`

use std::path::{Path, PathBuf};
use std::process::Command;

use mm_core::AudioOrigin;
use mm_llm::asr_pricing::{self, AsrMode, AsrProvider, DEFAULT_GEMINI_REFINE_MODEL};
use mm_llm::gemini_asr::{GeminiTranscribeOptions, transcribe_file};

/// Reads `GEMINI_API_KEY` from the environment (never hardcoded).
fn require_gemini_key() -> String {
    std::env::var("GEMINI_API_KEY")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .expect("set GEMINI_API_KEY before running ignored Gemini live tests")
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
#[ignore = "calls live Google Gemini Speech-to-Text API with GEMINI_API_KEY"]
fn gemini_transcribe_live_models() {
    let api_key = require_gemini_key();
    let dir = tempfile::tempdir().expect("tempdir");
    let m4a = synthesize_m4a(
        dir.path(),
        "大家好，我们开始周会。下季度预算上调百分之五，请 Alice 在周五之前更新预算表。",
    );

    let models = [DEFAULT_GEMINI_REFINE_MODEL, "gemini-2.5-flash"];

    for model in models {
        let out = transcribe_file(
            &api_key,
            AudioOrigin::Microphone,
            &m4a,
            0,
            6_000,
            &GeminiTranscribeOptions {
                model: model.to_string(),
                language: Some("zh".into()),
                context: Some("会议：周会；参会人：Alice；关键词：预算".into()),
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
                    AsrProvider::Gemini,
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
