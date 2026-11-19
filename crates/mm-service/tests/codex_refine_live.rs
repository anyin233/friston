//! Live check of the unified Codex post-meeting route (transcription + speaker diarization +
//! minutes in one turn). Ignored by default because it invokes the user's signed-in `codex`
//! CLI. Synthesises a short two-voice meeting with `say`, then runs `codex::transcribe_and_summarize`
//! → `external::heard_to_segments`:
//!
//! `cargo test -p mm-service --test codex_refine_live -- --ignored --nocapture`
//!
//! Set `MM_CODEX_RECORDING=/path/to/mic.caf` and optionally `MM_CODEX_MODEL=<id>` to test
//! with a real recording or specific Codex model.

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use mm_core::{AudioOrigin, MeetingId, SpeakerLabel, TranscriptSegment};
use mm_llm::{AudioTrack, MinutesInput, codex};
use mm_service::external;

/// Renders `text` with a macOS voice into a 16 kHz mono WAV.
fn say(voice: &str, text: &str, out: &std::path::Path) {
    let ok = Command::new("say")
        .args(["-v", voice, "-o"])
        .arg(out)
        .args(["--file-format=WAVE", "--data-format=LEI16@16000", text])
        .status()
        .expect("say")
        .success();
    assert!(ok, "say -v {voice} failed");
}

/// Returns `MM_CODEX_RECORDING`, or synthesises a two-voice Chinese recording in `dir`.
fn test_recording(dir: &std::path::Path) -> PathBuf {
    match std::env::var_os("MM_CODEX_RECORDING") {
        Some(p) => PathBuf::from(p),
        None => {
            let a = dir.join("a.wav");
            let b = dir.join("b.wav");
            say("Tingting", "今天讨论退款接口联调，赵磊周五之前完成。", &a);
            say(
                "Eddy (Chinese (China mainland))",
                "好的，预算已经批了，三十七万五千元。",
                &b,
            );
            let (_, mut s) = external::read_wav_mono16(&a).unwrap();
            s.extend(vec![0i16; 8_000]);
            s.extend(external::read_wav_mono16(&b).unwrap().1);
            let out = dir.join("mic.wav");
            external::write_wav_mono16(&out, 16_000, &s).unwrap();
            let _ = std::fs::remove_file(&a);
            let _ = std::fs::remove_file(&b);
            out
        }
    }
}

/// Runs `codex::transcribe_and_summarize` directly against the audio path on disk.
#[test]
#[ignore = "uses the signed-in Codex account"]
fn codex_transcribes_diarizes_and_writes_minutes_in_one_run() {
    let codex_path = codex::find_codex().expect("codex CLI not found");
    let account = codex::read_account(&codex_path).expect("account/read");
    assert!(
        account.signed_in,
        "codex is not signed in; run `codex login`"
    );

    let dir = tempfile::tempdir().unwrap();
    let mic = test_recording(dir.path());
    let model = std::env::var("MM_CODEX_MODEL")
        .ok()
        .filter(|m| !m.trim().is_empty());

    let input = MinutesInput {
        title: "退款接口联调".into(),
        when: "2026-11-19 14:00".into(),
        attendees: vec!["赵磊".into()],
        language: Some("zh".into()),
        ..Default::default()
    };

    let (heard, minutes_res) = codex::transcribe_and_summarize(
        &codex_path,
        &[AudioTrack {
            origin: AudioOrigin::Microphone,
            path: mic,
        }],
        Some("会议：退款接口联调\n参会人：赵磊"),
        Some("zh"),
        &input,
        model.as_deref(),
        Duration::from_secs(600),
    )
    .expect("codex transcribe_and_summarize");

    println!("HEARD: {heard:#?}");
    let minutes = minutes_res.expect("minutes in combined output");
    println!("MINUTES: {minutes:#?}");

    let meeting = MeetingId::new();
    // Provide a live segment only for the first speaker so fresh cluster allocation is exercised too.
    let live = vec![TranscriptSegment {
        meeting_id: meeting,
        start_ms: 0,
        end_ms: 3_500,
        text: "今天讨论".into(),
        language: Some("zh".into()),
        speaker: SpeakerLabel::Cluster { index: 1 },
        origin: AudioOrigin::Microphone,
        spatial: None,
    }];
    let segments = external::heard_to_segments(meeting, 0, &heard, &live, Some("zh".into()));
    println!("SEGMENTS: {segments:#?}");

    assert!(!segments.is_empty());
    assert!(!minutes.summary.is_empty());

    if std::env::var_os("MM_CODEX_RECORDING").is_none() {
        let all: String = segments.iter().map(|s| s.text.as_str()).collect();
        assert!(all.contains("赵磊"), "{all}");
        assert!(all.contains("退款") || all.contains("预算"), "{all}");
        let distinct_speakers: HashSet<String> = segments
            .iter()
            .map(|s| format!("{:?}", s.speaker))
            .collect();
        assert!(
            distinct_speakers.len() >= 2,
            "expected >= 2 distinct speakers, got {distinct_speakers:?}"
        );
    }
}
