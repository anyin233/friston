//! Live check of the Antigravity post-meeting pass. Ignored by default because it uploads
//! audio with the user's Google account. Synthesises a short two-voice meeting with `say`,
//! then runs prepare → agy transcription → speaker mapping → agy minutes:
//!
//! `cargo test -p mm-service --test antigravity_live -- --ignored --nocapture`
//!
//! Set `MM_AGY_RECORDING=/path/to/mic.caf` to use a real recording instead.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use mm_core::{AudioOrigin, MeetingId, SpeakerLabel, TranscriptSegment};
use mm_llm::antigravity::{self, DEFAULT_MODEL};
use mm_llm::{MinutesInput, TranscriptLine};
use mm_service::external;

/// Renders `text` with a macOS voice into a 16 kHz WAV.
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

#[test]
#[ignore = "uploads audio through the signed-in Antigravity CLI"]
fn antigravity_hears_a_meeting_and_writes_minutes() {
    let agy = antigravity::find_agy().expect("agy not found");
    let dir = tempfile::tempdir().unwrap();
    let mic = match std::env::var_os("MM_AGY_RECORDING") {
        Some(p) => PathBuf::from(p),
        None => {
            let a = dir.path().join("a.wav");
            let b = dir.path().join("b.wav");
            say("Tingting", "今天讨论退款接口联调，赵磊周五之前完成。", &a);
            say(
                "Eddy (Chinese (China mainland))",
                "好的，预算已经批了，三十七万五千元。",
                &b,
            );
            let (_, mut s) = external::read_wav_mono16(&a).unwrap();
            s.extend(vec![0i16; 8000]);
            s.extend(external::read_wav_mono16(&b).unwrap().1);
            let out = dir.path().join("mic.wav");
            external::write_wav_mono16(&out, 16_000, &s).unwrap();
            out
        }
    };
    // A silent system track must be dropped, never sent.
    let silent = dir.path().join("system.wav");
    external::write_wav_mono16(&silent, 16_000, &vec![0; 16_000]).unwrap();
    let chunks = external::prepare_recordings(
        &[
            (AudioOrigin::Microphone, mic),
            (AudioOrigin::System, silent),
        ],
        &dir.path().join("work"),
    )
    .unwrap();
    assert!(!chunks.is_empty());
    assert!(
        chunks
            .iter()
            .all(|c| c.tracks.iter().all(|t| t.origin == AudioOrigin::Microphone))
    );

    let meeting = MeetingId::new();
    // A live segment for the first speaker, so identity mapping is exercised.
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
    let mut segments = Vec::new();
    for c in &chunks {
        let heard = antigravity::transcribe(
            &agy,
            &c.dir,
            &c.tracks,
            Some("参会人：赵磊"),
            Some("zh"),
            DEFAULT_MODEL,
            Duration::from_secs(900),
        )
        .expect("agy transcription");
        println!("{heard:#?}");
        segments.extend(external::heard_to_segments(
            meeting,
            c.offset_ms,
            &heard.lines,
            &live,
            Some("zh".into()),
        ));
    }
    assert!(!segments.is_empty());
    if std::env::var_os("MM_AGY_RECORDING").is_none() {
        let all: String = segments.iter().map(|s| s.text.as_str()).collect();
        assert!(all.contains("赵磊"), "{all}");
        assert_eq!(segments[0].speaker, SpeakerLabel::Cluster { index: 1 });
    }

    let input = MinutesInput {
        title: "冒烟测试".into(),
        when: "2026-11-08 14:00".into(),
        transcript: segments
            .iter()
            .map(|s| TranscriptLine {
                start_ms: s.start_ms,
                speaker: format!("{:?}", s.speaker),
                text: s.text.clone(),
            })
            .collect(),
        language: Some("zh".into()),
        ..Default::default()
    };
    let empty = dir.path().join("minutes");
    std::fs::create_dir_all(&empty).unwrap();
    let minutes = antigravity::generate_minutes(
        &agy,
        &empty,
        &input,
        DEFAULT_MODEL,
        Duration::from_secs(900),
    )
    .expect("agy minutes");
    println!("{minutes:#?}");
    assert!(!minutes.summary.is_empty());
}
