//! Live end-to-end check of `mm-meetingd` cloud streaming captions via `caption/simulateFile`
//! against OpenAI Realtime Transcription.
//!
//! Ignored by default; run with:
//! `OPENAI_API_KEY=... cargo test -p mm-service --test openai_live -- --ignored --nocapture`

use std::path::{Path, PathBuf};
use std::process::Command;

use mm_engine::EngineEvent;
use mm_llm::DEFAULT_OPENAI_LIVE_MODEL;
use mm_service::engines::{MeetingEngine, MeetingNotice, decode_notice};
use mm_service::external;
use serde_json::{Value, json};

/// Reads `OPENAI_API_KEY` from env (never stored in files or logs).
fn require_openai_key() -> String {
    std::env::var("OPENAI_API_KEY")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .expect("set OPENAI_API_KEY before running openai_live test")
}

/// Resolves `mm-meetingd` binary from `MM_E2E_ENGINE` or `target/engines/Debug/mm-meetingd`.
fn engine_binary() -> PathBuf {
    if let Some(p) = std::env::var_os("MM_E2E_ENGINE") {
        let pb = PathBuf::from(p);
        assert!(
            pb.is_file(),
            "MM_E2E_ENGINE does not exist: {}",
            pb.display()
        );
        return pb;
    }
    let candidate = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/engines/Debug/mm-meetingd/mm-meetingd");
    assert!(
        candidate.is_file(),
        "mm-meetingd binary not found at {}; build engines first",
        candidate.display()
    );
    candidate
}

/// Synthesizes two Chinese/English utterances separated by 1.5s of silence so Silero VAD
/// emits clean turn boundaries and commits both utterances.
fn synthesize_two_turn_wav(dir: &Path) -> PathBuf {
    let a = dir.join("turn1.wav");
    let b = dir.join("turn2.wav");
    for (voice, text, out) in [
        ("Tingting", "大家好，我们开始今天的周会。", &a),
        ("Tingting", "下季度预算由 Alice 负责更新。", &b),
    ] {
        let ok = Command::new("say")
            .args(["-v", voice, "-o"])
            .arg(out)
            .args(["--file-format=WAVE", "--data-format=LEI16@16000", text])
            .status()
            .expect("run say")
            .success();
        assert!(ok, "say failed for {text}");
    }
    let (rate, mut samples) = external::read_wav_mono16(&a).expect("read turn1");
    assert_eq!(rate, 16_000);
    samples.extend(vec![0i16; 24_000]); // 1.5s pause to close turn 1
    samples.extend(external::read_wav_mono16(&b).expect("read turn2").1);
    samples.extend(vec![0i16; 16_000]); // 1.0s tail so turn 2 finalizes cleanly
    let combined = dir.join("two_turns.wav");
    external::write_wav_mono16(&combined, 16_000, &samples).expect("write combined wav");
    combined
}

#[test]
#[ignore = "spawns mm-meetingd and calls live OpenAI Realtime API with OPENAI_API_KEY"]
fn mm_meetingd_openai_realtime_live_captions() {
    let api_key = require_openai_key();
    let bin = engine_binary();
    let dir = tempfile::tempdir().expect("tempdir");
    let wav = synthesize_two_turn_wav(dir.path());

    let (engine, events) = MeetingEngine::spawn(&bin).expect("spawn mm-meetingd");
    engine.initialize().expect("initialize engine");

    let sim_res: Value = engine
        .simulate_file(json!({
            "path": wav,
            "origin": "microphone",
            "speed": 1.0,
            "language": "zh",
            "liveAsr": {
                "provider": "openai",
                "model": DEFAULT_OPENAI_LIVE_MODEL,
                "apiKey": api_key,
            }
        }))
        .expect("caption/simulateFile");

    let mut notice_partials = 0_usize;
    let mut notice_finals = Vec::new();
    let mut notice_usage = Vec::new();
    while let Ok(ev) = events.try_recv() {
        if let EngineEvent::Notification { method, params } = ev {
            match decode_notice(&method, params) {
                Ok(MeetingNotice::Partial(_)) => notice_partials += 1,
                Ok(MeetingNotice::Final(f)) => notice_finals.push(f),
                Ok(MeetingNotice::Usage(u)) => notice_usage.push(u),
                _ => {}
            }
        }
    }
    engine.shutdown();

    let finals = sim_res["finals"].as_array().cloned().unwrap_or_default();
    let usage = sim_res["usage"].as_array().cloned().unwrap_or_default();
    let texts: Vec<String> = finals
        .iter()
        .filter_map(|f| f["text"].as_str().map(str::to_string))
        .collect();
    println!(
        "simulateFile: partials={notice_partials}, finals={}, usage_events={}, texts={texts:?}, usage={usage:?}",
        finals.len(),
        usage.len()
    );

    assert!(
        !finals.is_empty(),
        "expected at least one finalized caption from OpenAI Realtime"
    );
    let joined = texts.join(" ");
    assert!(
        !joined.trim().is_empty(),
        "finalized transcripts should be non-empty"
    );
    assert_eq!(
        joined.matches("周会").count(),
        1,
        "expected 周会 exactly once (no duplicate streaming audio): {joined:?}"
    );
    assert_eq!(
        joined.matches("预算").count(),
        1,
        "expected 预算 exactly once (no duplicate streaming audio): {joined:?}"
    );
    assert!(
        !notice_finals.is_empty(),
        "expected caption/final notifications"
    );
    assert!(
        !usage.is_empty() && !notice_usage.is_empty(),
        "expected caption/usage notifications and result usage entries"
    );
    assert_eq!(usage[0]["provider"].as_str(), Some("openai"));
    assert!(
        usage[0]["audioMs"].as_u64().unwrap_or(0) > 0,
        "expected positive audioMs in usage"
    );
}
