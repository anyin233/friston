//! Live end-to-end check of one meeting through the app's own pipeline, minus the GUI:
//! real `mm-meetingd` capture on the default microphone → live captions persisted by the
//! `LiveWriter` → `finish_meeting` (stop, refine, minutes via the configured provider,
//! note write-back).
//!
//! Ignored by default: it records from the microphone, plays speech through the
//! speakers and uses the signed-in Codex account. Run with
//! `MM_E2E_ENGINE=<path to mm-meetingd> cargo test -p mm-app e2e -- --ignored --nocapture`.
//! The recording folder is deleted at the end because the microphone also hears the room.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{Local, Utc};
use mm_core::{
    CaptureFiles, ClipRequest, PersonId, PresetVoiceprint, SpeakerLabel, Voiceprint, VoiceprintId,
    VoiceprintSource,
};
use mm_llm::{AsrMode, AsrProvider, MinutesBackend, asr_pricing, byok};
use mm_service::engines::{EmbedEngine, MeetingEngine, MeetingNotice, decode_notice};
use mm_service::hub::{self, HubRepo};
use mm_service::indexer::{Indexer, IndexerMsg, current_stamp};
use mm_service::live::CaptionBoard;
use mm_service::{AppConfig, LiveAsrBackend, RefineBackend, external, flow};
use mm_store::{AsrUsageRecord, Store};
use serde_json::json;

use crate::logic::{self, default_microphone};
use crate::pipeline::{
    FinishJob, MinutesCtx, StoppedMeeting, detect_codex, finish_meeting, process_meeting,
};
use crate::writer::{LiveWriter, WriteMsg};

/// Lines played through the speakers during the recording.
const SCRIPT: &str = "大家好，我们开始周会。第一项，下季度预算上调百分之五。第二项，Alice 负责在周五之前更新预算表。发布日期还没有确定，下周再讨论。";

#[test]
#[ignore = "records the microphone, plays audio and uses the Codex account"]
fn e2e_record_stop_refine_minutes() {
    run_live_e2e(|vault, rec| AppConfig {
        vault_path: Some(vault.to_path_buf()),
        recordings_dir: Some(rec.to_path_buf()),
        record_screen: false,
        system_audio: false,
        caption_language: Some("zh".into()),
        ..Default::default()
    });
}

#[test]
#[ignore = "records the microphone, plays audio and uses Codex unified refine + minutes"]
fn e2e_record_stop_codex_unified() {
    run_live_e2e(|vault, rec| AppConfig {
        vault_path: Some(vault.to_path_buf()),
        recordings_dir: Some(rec.to_path_buf()),
        record_screen: false,
        system_audio: false,
        caption_language: Some("zh".into()),
        refine_after_meeting: true,
        refine_backend: RefineBackend::Codex,
        auto_minutes: true,
        ..Default::default()
    });
}

fn run_live_e2e(make_cfg: impl FnOnce(&Path, &Path) -> AppConfig) {
    let engine_path = e2e_engine_binary();
    let tmp = tempfile::tempdir().expect("temp dir");
    let vault = tmp.path().join("vault");
    std::fs::create_dir_all(&vault).expect("vault dir");
    let cfg = make_cfg(&vault, &tmp.path().join("rec"));
    let store = Arc::new(Mutex::new(
        Store::open(&tmp.path().join("db.sqlite")).expect("open store"),
    ));
    let (indexer, _ix_events) = Indexer::start(store.clone(), Some(vault.clone()));
    let (writer, _writer_errors) = LiveWriter::start(store.clone(), indexer);
    let (engine, events) = MeetingEngine::spawn(&engine_path).expect("spawn engine");
    engine.initialize().expect("initialize");

    let mic =
        default_microphone(&engine.probe_inputs().expect("probe inputs")).expect("a microphone");
    let mut meeting = {
        let s = store.lock().unwrap();
        let mut m = flow::quick_meeting(&s, "端到端测试周会").expect("meeting");
        flow::ensure_note(&cfg, &s, &mut m, &[])
            .expect("note")
            .expect("vault configured");
        m
    };
    let mut preset =
        flow::build_preset(&cfg, &store.lock().unwrap(), &meeting, None).expect("preset");
    preset.microphone_uid = Some(mic);
    let started = engine.capture_start(&preset).expect("capture/start");
    flow::mark_live(&store.lock().unwrap(), &mut meeting, &started).expect("mark live");

    // Speak through the speakers while recording; the built-in mic picks it up.
    let speech = tmp.path().join("script.aiff");
    let ok = std::process::Command::new("say")
        .args(["-v", "Tingting", "-o"])
        .arg(&speech)
        .arg(SCRIPT)
        .status()
        .expect("say");
    assert!(ok.success());
    std::thread::sleep(Duration::from_secs(4)); // models finish loading
    let player = std::process::Command::new("afplay")
        .args(["-v", "1"])
        .arg(&speech)
        .spawn()
        .expect("afplay");

    let mut board = CaptionBoard::default();
    board.begin(&started.session_id);
    let (mut partials, mut finals) = (0, 0);
    let t0 = Instant::now();
    let deadline = t0 + Duration::from_secs(30);
    while Instant::now() < deadline {
        let Ok(ev) = events.try_recv() else {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        };
        if let mm_engine::EngineEvent::Notification { method, params } = ev {
            match decode_notice(&method, params) {
                Ok(MeetingNotice::Partial(p)) => partials += board.apply_partial(&p) as usize,
                Ok(MeetingNotice::Final(f)) => {
                    if let Some(seg) = board.apply_final(&f, meeting.id) {
                        finals += 1;
                        writer.send(WriteMsg::Segment(seg, f.utterance_id));
                    }
                }
                Ok(MeetingNotice::Relabel(r)) => {
                    board.apply_relabel(&r);
                    writer.send(WriteMsg::Relabel(meeting.id, r.from, r.to));
                }
                _ => {}
            }
        }
    }
    drop(player);
    println!("live: {partials} partial updates, {finals} finals");

    let codex = detect_codex();
    let job = FinishJob {
        minutes: MinutesCtx {
            cfg: cfg.clone(),
            store: store.clone(),
            codex: Some(codex),
            embed: None,
        },
        engine: Some(engine.clone()),
        writer: writer.clone(),
        meeting,
        started: started.clone(),
        elapsed_ms: t0.elapsed().as_millis() as u64,
    };
    let report = finish_meeting(job, &|step| println!("step: {step}")).expect("finish meeting");
    for w in &report.warnings {
        println!("warning: {w}");
    }
    engine.shutdown();

    let s = store.lock().unwrap();
    let transcript = s.transcript(report.meeting.id).expect("transcript");
    let joined: String = transcript.iter().map(|t| t.text.as_str()).collect();
    let note = std::fs::read_to_string(flow::note_path(&cfg, &report.meeting).expect("note path"))
        .expect("read note");
    println!(
        "final transcript: {} segments; heard budget line: {}; status {:?}; minutes block filled: {}",
        transcript.len(),
        joined.contains("预算"),
        report.meeting.status,
        note.contains("### 摘要")
    );
    // The microphone also captured the room: never keep the recording.
    if let Err(e) = std::fs::remove_dir_all(&started.directory) {
        println!(
            "could not delete recording {}: {e}",
            started.directory.display()
        );
    }
    assert!(finals > 0, "no live captions arrived");
    assert!(
        report.minutes_done,
        "minutes were not generated: {:?}",
        report.warnings
    );
    assert_eq!(report.meeting.status, mm_core::MeetingStatus::Done);
    assert!(note.contains("### 摘要") && note.contains("<!-- mm:transcript:start -->"));
}

/// Restores or removes a macOS Keychain account entry when dropped.
struct KeychainGuard {
    account: &'static str,
    prev: Option<String>,
}

impl KeychainGuard {
    fn install(account: &'static str, key: &str) -> Self {
        let prev = byok::read_account_key(account).expect("read existing key");
        byok::write_account_key(account, Some(key)).expect("store test key");
        Self { account, prev }
    }
}

impl Drop for KeychainGuard {
    fn drop(&mut self) {
        let _ = byok::write_account_key(self.account, self.prev.as_deref());
    }
}

fn locate_built_engine(env_var: &str, name: &str) -> PathBuf {
    if let Some(p) = std::env::var_os(env_var) {
        let pb = PathBuf::from(p);
        assert!(pb.is_file(), "{env_var} does not exist: {}", pb.display());
        return pb;
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let candidates = [
        root.join("target/engines/Debug").join(name).join(name),
        root.join("target/debug/FristonDev.app/Contents/Resources/engines")
            .join(name)
            .join(name),
        root.join("target/engines/Release").join(name).join(name),
    ];
    for candidate in &candidates {
        if candidate.is_file() {
            return candidate.clone();
        }
    }
    panic!(
        "{name} binary not found; searched: {:?}",
        candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
    );
}

/// Resolves the `mm-meetingd` executable from env or workspace build outputs.
pub(crate) fn e2e_engine_binary() -> PathBuf {
    locate_built_engine("MM_E2E_ENGINE", "mm-meetingd")
}

/// Resolves the `mm-embedd` executable from env or workspace build outputs.
pub(crate) fn e2e_embed_binary() -> PathBuf {
    locate_built_engine("MM_E2E_EMBED_ENGINE", "mm-embedd")
}

/// Resolves a locally cached Hugging Face model snapshot without downloading from the network.
pub(crate) fn find_cached_hf_model(repo_id: &str) -> Option<PathBuf> {
    let repo = HubRepo::parse(repo_id).ok()?;
    let empty_root = Path::new("/nonexistent-mm-models");
    hub::find_local_snapshot(&repo, empty_root, hub::hf_cache_dir().as_deref())
}

fn run_say_wav(voice: Option<&str>, text: &str, rate: u32, out: &Path) {
    let mut cmd = Command::new("say");
    if let Some(v) = voice {
        cmd.args(["-v", v]);
    }
    let ok = cmd
        .arg("-o")
        .arg(out)
        .arg("--file-format=WAVE")
        .arg(format!("--data-format=LEI16@{rate}"))
        .arg(text)
        .status()
        .expect("run say")
        .success();
    assert!(ok, "say failed for {voice:?}: {text}");
}

fn run_english_say_wav(text: &str, rate: u32, out: &Path) {
    for v in [Some("Samantha"), Some("Fred"), None] {
        let mut cmd = Command::new("say");
        if let Some(voice) = v {
            cmd.args(["-v", voice]);
        }
        if let Ok(status) = cmd
            .arg("-o")
            .arg(out)
            .arg("--file-format=WAVE")
            .arg(format!("--data-format=LEI16@{rate}"))
            .arg(text)
            .status()
            && status.success()
        {
            return;
        }
    }
    panic!("macOS `say` failed for English utterance: {text}");
}

/// Writes a 2-channel interleaved 16-bit PCM WAV file.
fn write_wav_stereo16(path: &Path, rate: u32, left: &[i16], right: &[i16]) {
    let n = left.len().min(right.len());
    let data_bytes = (n * 4) as u32;
    let riff_size = 36 + data_bytes;
    let mut bytes = Vec::with_capacity(44 + data_bytes as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&riff_size.to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend_from_slice(b"fmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
    bytes.extend_from_slice(&2u16.to_le_bytes()); // 2 channels
    bytes.extend_from_slice(&rate.to_le_bytes());
    bytes.extend_from_slice(&(rate * 4).to_le_bytes());
    bytes.extend_from_slice(&4u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_bytes.to_le_bytes());
    for i in 0..n {
        bytes.extend_from_slice(&left[i].to_le_bytes());
        bytes.extend_from_slice(&right[i].to_le_bytes());
    }
    std::fs::write(path, bytes).expect("write stereo wav");
}

/// Multi-track synthesized meeting audio for live caption, voiceprint, spatial and refine E2E.
pub(crate) struct SynthesizedMeetingAudio {
    pub speaker_a_ref: PathBuf,
    pub stereo_mic_wav: PathBuf,
    pub mono_mic_wav: PathBuf,
    pub remote_system_wav: PathBuf,
    pub duration_ms: u64,
}

/// Synthesizes speaker reference, stereo spatially separated two-speaker mic WAV, mono mic WAV,
/// and English remote system-audio WAV in `dir`.
pub(crate) fn synthesize_meeting_audio(dir: &Path) -> SynthesizedMeetingAudio {
    const VP_TEXT: &str =
        "大家好，我是赵磊，现在录制一段用于声纹识别的测试语音，今天主要同步退款接口进度。";
    const U1_TEXT: &str = "大家好，我们开始周会。第一项，下季度退款接口联调必须在周五之前完成。";
    const U2_TEXT: &str = "好的，下季度预算三十七万五千元已经审批通过，财务下周一打款。";
    const SYS_TEXT: &str =
        "Hello everyone, the quarterly budget is approved for Friday deployment.";

    let speaker_a_ref = dir.join("speaker_a_ref.wav");
    run_say_wav(Some("Tingting"), VP_TEXT, 16_000, &speaker_a_ref);

    // 16 kHz mono mic track (2 speakers separated by 1.5s silence + 1.0s tail).
    let m1 = dir.join("m1.wav");
    let m2 = dir.join("m2.wav");
    run_say_wav(Some("Tingting"), U1_TEXT, 16_000, &m1);
    run_say_wav(
        Some("Eddy (Chinese (China mainland))"),
        U2_TEXT,
        16_000,
        &m2,
    );
    let (r1, mut mono_samples) = external::read_wav_mono16(&m1).expect("read m1");
    assert_eq!(r1, 16_000);
    mono_samples.extend(vec![0i16; 24_000]);
    mono_samples.extend(external::read_wav_mono16(&m2).expect("read m2").1);
    mono_samples.extend(vec![0i16; 16_000]);
    let duration_ms = ((mono_samples.len() as u64) * 1000) / 16_000;
    let mono_mic_wav = dir.join("mic_mono.wav");
    external::write_wav_mono16(&mono_mic_wav, 16_000, &mono_samples).expect("write mono mic");
    let _ = std::fs::remove_file(&m1);
    let _ = std::fs::remove_file(&m2);

    // 48 kHz stereo mic track: Utterance 1 louder/earlier on ch0 (Left), Utterance 2 on ch1 (Right).
    let s1 = dir.join("s1.wav");
    let s2 = dir.join("s2.wav");
    run_say_wav(Some("Tingting"), U1_TEXT, 48_000, &s1);
    run_say_wav(
        Some("Eddy (Chinese (China mainland))"),
        U2_TEXT,
        48_000,
        &s2,
    );
    let (sr1, u1) = external::read_wav_mono16(&s1).expect("read s1");
    let (sr2, u2) = external::read_wav_mono16(&s2).expect("read s2");
    assert_eq!(sr1, 48_000);
    assert_eq!(sr2, 48_000);
    let _ = std::fs::remove_file(&s1);
    let _ = std::fs::remove_file(&s2);

    let delay = 14_usize; // ~0.29 ms @ 48 kHz
    let attenuate = |s: i16| -> i16 { ((s as f32) * 0.70).round() as i16 };

    let mut left = Vec::with_capacity(u1.len() + u2.len() + 144_000);
    let mut right = Vec::with_capacity(u1.len() + u2.len() + 144_000);
    // U1: Left (ch0 leading, ch1 delayed by `delay` + attenuated).
    left.extend_from_slice(&u1);
    right.extend(std::iter::repeat_n(0i16, delay.min(u1.len())));
    right.extend(
        u1[..u1.len().saturating_sub(delay)]
            .iter()
            .copied()
            .map(attenuate),
    );
    // 1.5s stereo pause.
    left.extend(std::iter::repeat_n(0i16, 72_000));
    right.extend(std::iter::repeat_n(0i16, 72_000));
    // U2: Right (ch1 leading, ch0 delayed by `delay` + attenuated).
    left.extend(std::iter::repeat_n(0i16, delay.min(u2.len())));
    left.extend(
        u2[..u2.len().saturating_sub(delay)]
            .iter()
            .copied()
            .map(attenuate),
    );
    right.extend_from_slice(&u2);
    // 1.5s stereo tail silence so final utterance closes cleanly.
    left.extend(std::iter::repeat_n(0i16, 72_000));
    right.extend(std::iter::repeat_n(0i16, 72_000));

    let stereo_mic_wav = dir.join("mic_stereo.wav");
    write_wav_stereo16(&stereo_mic_wav, 48_000, &left, &right);

    // 16 kHz mono remote English system audio track.
    let s_sys = dir.join("sys_raw.wav");
    run_english_say_wav(SYS_TEXT, 16_000, &s_sys);
    let (sys_rate, mut sys_samples) = external::read_wav_mono16(&s_sys).expect("read sys_raw");
    assert_eq!(sys_rate, 16_000);
    sys_samples.extend(vec![0i16; 19_200]); // 1.2s tail
    let remote_system_wav = dir.join("system.wav");
    external::write_wav_mono16(&remote_system_wav, 16_000, &sys_samples).expect("write sys wav");
    let _ = std::fs::remove_file(&s_sys);

    SynthesizedMeetingAudio {
        speaker_a_ref,
        stereo_mic_wav,
        mono_mic_wav,
        remote_system_wav,
        duration_ms,
    }
}

fn synthesize_openai_e2e_wav(out: &Path) -> u64 {
    let dir = out.parent().expect("wav parent dir");
    let a = dir.join("utt1.wav");
    let b = dir.join("utt2.wav");
    for (text, target) in [
        ("大家好，我们开始今天的周会。", &a),
        ("下季度预算上调百分之五，由 Alice 负责更新。", &b),
    ] {
        let ok = Command::new("say")
            .args(["-v", "Tingting", "-o"])
            .arg(target)
            .args(["--file-format=WAVE", "--data-format=LEI16@16000", text])
            .status()
            .expect("run say")
            .success();
        assert!(ok, "say failed for {text}");
    }
    let (rate, mut samples) = external::read_wav_mono16(&a).expect("read utt1");
    assert_eq!(rate, 16_000);
    samples.extend(vec![0i16; 24_000]); // 1.5s silence between utterances
    samples.extend(external::read_wav_mono16(&b).expect("read utt2").1);
    samples.extend(vec![0i16; 16_000]); // 1.0s tail silence
    external::write_wav_mono16(out, 16_000, &samples).expect("write wav");
    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);
    ((samples.len() as u64) * 1000) / 16_000
}

/// Shared live-simulation + post-meeting cloud refine + SQLite spend E2E flow.
fn run_cloud_live_and_refine_e2e(
    account: &'static str,
    api_key: &str,
    live_backend: LiveAsrBackend,
    refine_backend: RefineBackend,
    expected_provider: &str,
    title: &str,
) {
    let _key_guard = KeychainGuard::install(account, api_key);

    let engine_path = e2e_engine_binary();
    let tmp = tempfile::tempdir().expect("tempdir");
    let vault = tmp.path().join("vault");
    let rec_dir = tmp.path().join("rec");
    std::fs::create_dir_all(&vault).expect("vault dir");
    std::fs::create_dir_all(&rec_dir).expect("rec dir");

    let cfg = AppConfig {
        vault_path: Some(vault.clone()),
        recordings_dir: Some(rec_dir.clone()),
        record_screen: false,
        system_audio: false,
        caption_language: Some("zh".into()),
        live_asr_backend: live_backend,
        refine_after_meeting: true,
        refine_backend,
        auto_minutes: false,
        ..Default::default()
    };

    let live_cfg = flow::resolve_live_asr(&cfg)
        .expect("resolve_live_asr should succeed with key in Keychain")
        .expect("LiveAsrConfig should be Some for cloud backend");
    assert_eq!(live_cfg.provider, expected_provider);

    let store = Arc::new(Mutex::new(
        Store::open(&tmp.path().join("db.sqlite")).expect("open store"),
    ));
    let (indexer, _ix_events) = Indexer::start(store.clone(), Some(vault));
    let (writer, _writer_errors) = LiveWriter::start(store.clone(), indexer);
    let (engine, events) = MeetingEngine::spawn(&engine_path).expect("spawn engine");
    engine.initialize().expect("initialize");

    let meeting = {
        let s = store.lock().unwrap();
        let mut m = flow::quick_meeting(&s, title).expect("quick meeting");
        flow::ensure_note(&cfg, &s, &mut m, &["Alice".into()])
            .expect("ensure note")
            .expect("vault configured");
        m
    };
    let meeting_rec_dir = rec_dir.join(meeting.id.to_string());
    std::fs::create_dir_all(&meeting_rec_dir).expect("meeting rec dir");
    let mic_wav = meeting_rec_dir.join("mic.wav");
    let _duration_ms = synthesize_openai_e2e_wav(&mic_wav);

    let sim = engine
        .simulate_file(json!({
            "path": mic_wav,
            "origin": "microphone",
            "speed": 1.0,
            "language": "zh",
            "liveAsr": live_cfg,
        }))
        .expect("caption/simulateFile");

    let mut board = CaptionBoard::default();
    let sim_session = sim["sessionId"]
        .as_str()
        .expect("simulateFile sessionId")
        .to_string();
    board.begin(&sim_session);
    let mut live_finals = 0_usize;
    let mut live_usage_events = 0_usize;
    while let Ok(ev) = events.try_recv() {
        if let mm_engine::EngineEvent::Notification { method, params } = ev {
            match decode_notice(&method, params) {
                Ok(MeetingNotice::Partial(p)) => {
                    board.apply_partial(&p);
                }
                Ok(MeetingNotice::Final(f)) => {
                    if let Some(seg) = board.apply_final(&f, meeting.id) {
                        live_finals += 1;
                        writer.send(WriteMsg::Segment(seg, f.utterance_id));
                    }
                }
                Ok(MeetingNotice::Usage(u)) => {
                    if let Some(provider) = AsrProvider::parse(&u.provider) {
                        let cost_usd = asr_pricing::estimate_cost_usd(
                            provider,
                            AsrMode::Live,
                            &u.model,
                            u.audio_ms,
                            u.input_tokens,
                            u.output_tokens,
                        );
                        live_usage_events += 1;
                        writer.send(WriteMsg::AsrUsage(AsrUsageRecord {
                            recorded_at: Utc::now(),
                            meeting_id: Some(meeting.id),
                            provider: provider.as_str().to_string(),
                            mode: AsrMode::Live.as_str().to_string(),
                            model: u.model,
                            audio_ms: u.audio_ms,
                            input_tokens: u.input_tokens,
                            output_tokens: u.output_tokens,
                            cost_usd,
                        }));
                    }
                }
                _ => {}
            }
        }
    }
    writer.flush_blocking();
    engine.shutdown();

    assert!(live_finals > 0, "expected live finals during simulateFile");
    assert!(
        live_usage_events > 0,
        "expected caption/usage notification during live {expected_provider} stream"
    );

    let month_start = logic::local_month_start_utc(Local::now());
    let live_summary = store
        .lock()
        .unwrap()
        .asr_usage_summary(expected_provider, month_start)
        .expect("asr_usage_summary after live");
    println!(
        "[{expected_provider}] after live: finals={live_finals}, usage_events={live_usage_events}, summary={live_summary:?}"
    );
    assert!(live_summary.total.calls >= 1);
    assert!(live_summary.total.audio_ms > 0);

    // Ensure post-meeting refine runs cleanly on the recorded file and records refine spend.
    let minutes = MinutesCtx {
        cfg: cfg.clone(),
        store: store.clone(),
        codex: None,
        embed: None,
    };
    let stopped = StoppedMeeting {
        meeting,
        files: CaptureFiles {
            screen: None,
            microphone: Some(mic_wav),
            system_audio: None,
        },
        warnings: vec![],
    };
    let report = process_meeting(
        &minutes,
        || {
            Err(format!(
                "unexpected local fallback during {expected_provider} cloud refine E2E"
            ))
        },
        stopped,
        &|step| println!("[{expected_provider}] step: {step}"),
    )
    .expect("process_meeting");
    for w in &report.warnings {
        println!("[{expected_provider}] warning: {w}");
    }
    assert!(
        !report
            .warnings
            .iter()
            .any(|w| w.contains("回退") || w.contains("本地精转写")),
        "cloud refine unexpectedly fell back locally: {:?}",
        report.warnings
    );

    let final_summary = store
        .lock()
        .unwrap()
        .asr_usage_summary(expected_provider, month_start)
        .expect("asr_usage_summary after refine");
    let transcript = store
        .lock()
        .unwrap()
        .transcript(report.meeting.id)
        .expect("transcript");
    let note = std::fs::read_to_string(flow::note_path(&cfg, &report.meeting).expect("note path"))
        .expect("read Obsidian note");
    println!(
        "[{expected_provider}] after refine: segments={}, total_summary={final_summary:?}\nnote:\n{note}",
        transcript.len()
    );

    assert!(!transcript.is_empty(), "refined transcript is empty");
    assert!(
        note.contains("<!-- mm:transcript:start -->"),
        "Obsidian note missing transcript block"
    );
    assert!(
        final_summary.total.calls > live_summary.total.calls,
        "expected refine usage record to be added to SQLite"
    );
    assert!(
        final_summary.total.cost_usd > 0.0 && final_summary.total.audio_ms > 0,
        "expected positive cost and audio duration in SQLite usage summary"
    );
}

#[test]
#[ignore = "runs cloud live simulation + cloud refine + SQLite usage ledger + Obsidian note with OPENAI_API_KEY"]
fn e2e_openai_live_and_refine_pipeline() {
    let api_key = std::env::var("OPENAI_API_KEY")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .expect("set OPENAI_API_KEY before running e2e_openai_live_and_refine_pipeline");
    run_cloud_live_and_refine_e2e(
        byok::OPENAI_KEY_ACCOUNT,
        &api_key,
        LiveAsrBackend::OpenAi,
        RefineBackend::OpenAi,
        "openai",
        "OpenAI 云端全链路测试",
    );
}

#[test]
#[ignore = "runs Gemini Live simulation + Gemini cloud refine + SQLite usage ledger + Obsidian note with GEMINI_API_KEY"]
fn e2e_gemini_live_and_refine_pipeline() {
    let api_key = std::env::var("GEMINI_API_KEY")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .expect("set GEMINI_API_KEY before running e2e_gemini_live_and_refine_pipeline");
    run_cloud_live_and_refine_e2e(
        byok::GEMINI_KEY_ACCOUNT,
        &api_key,
        LiveAsrBackend::Gemini,
        RefineBackend::Gemini,
        "gemini",
        "Gemini 云端全链路测试",
    );
}

#[test]
#[ignore = "probes real mm-meetingd, mm-embedd, live caption streaming, voiceprint, HF models, Codex and Keychain readiness"]
fn e2e_probe_real_capabilities() {
    let meetingd = e2e_engine_binary();
    let embedd = e2e_embed_binary();
    let dir = tempfile::tempdir().expect("tempdir");
    let audio = synthesize_meeting_audio(dir.path());

    let (engine, events) = MeetingEngine::spawn(&meetingd).expect("spawn mm-meetingd");
    let caps = engine.initialize().expect("mm-meetingd initialize");
    assert!(
        caps["capabilities"].is_object(),
        "missing capabilities in initialize: {caps}"
    );
    let perms = engine.permissions().expect("permissions/status");
    let inputs = engine.probe_inputs().expect("audio/probeInputs");
    let preload = engine
        .preload_models(Some("zh"))
        .expect("models/preload zh");
    println!(
        "[PROBE] permissions: mic={}, screen={}, calendar={}",
        perms.microphone, perms.screen_capture, perms.calendar
    );
    println!(
        "[PROBE] audio inputs: {} (default={:?})",
        inputs.len(),
        default_microphone(&inputs)
    );
    println!(
        "[PROBE] speech models loaded={:?}, unavailable={:?}",
        preload.models, preload.unavailable
    );
    assert!(
        preload.models.contains_key("liveASR"),
        "expected liveASR model loaded: {preload:?}"
    );
    assert!(
        preload.models.contains_key("vad"),
        "expected vad model loaded: {preload:?}"
    );

    let sim = engine
        .simulate_file(json!({
            "path": audio.mono_mic_wav,
            "origin": "microphone",
            "speed": 1.0,
            "language": "zh",
        }))
        .expect("probe simulateFile live captions");
    let mut partials = 0_usize;
    let mut finals = 0_usize;
    while let Ok(ev) = events.try_recv() {
        if let mm_engine::EngineEvent::Notification { method, params } = ev {
            match decode_notice(&method, params) {
                Ok(MeetingNotice::Partial(_)) => partials += 1,
                Ok(MeetingNotice::Final(_)) => finals += 1,
                _ => {}
            }
        }
    }
    let sim_finals = sim["finals"].as_array().cloned().unwrap_or_default();
    let text: String = sim_finals
        .iter()
        .filter_map(|f| f["text"].as_str())
        .collect::<Vec<_>>()
        .join(" ");
    println!(
        "[PROBE] live simulate: notice_partials={partials}, notice_finals={finals}, result_finals={}, text={text:?}",
        sim_finals.len()
    );
    assert!(
        !sim_finals.is_empty(),
        "real live caption probe returned zero finalized segments"
    );
    assert!(
        ["预算", "退款", "周会", "周五", "联调"]
            .iter()
            .any(|kw| text.contains(kw)),
        "real live caption text did not recognize expected Chinese speech: {text:?}"
    );

    let vp = engine
        .voiceprint_from_clip(&ClipRequest {
            path: audio.speaker_a_ref,
            start_ms: None,
            end_ms: None,
        })
        .expect("voiceprint/fromClip");
    println!(
        "[PROBE] voiceprint: model={}, dims={}, speech_ms={}, quality={:?}",
        vp.model,
        vp.vector.len(),
        vp.speech_ms,
        vp.quality
    );
    assert!(!vp.vector.is_empty());
    assert!(vp.speech_ms >= 1000);
    engine.shutdown();

    let (embed, _ee) = EmbedEngine::spawn(&embedd).expect("spawn mm-embedd");
    let embed_status = embed.status().expect("mm-embedd status");
    let llm_status = embed.llm_status().expect("mm-embedd llm_status");
    embed.shutdown();
    println!("[PROBE] mm-embedd: status={embed_status:?}, llm={llm_status:?}");

    let hf_embed = find_cached_hf_model("mlx-community/embeddinggemma-2-bf16");
    let hf_llm = find_cached_hf_model("mlx-community/Qwen3-4B-Instruct-2507-4bit");
    let codex = detect_codex();
    let openai_env = std::env::var("OPENAI_API_KEY")
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let gemini_env = std::env::var("GEMINI_API_KEY")
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let openai_kc = byok::read_account_key(byok::OPENAI_KEY_ACCOUNT)
        .ok()
        .flatten()
        .is_some();
    let gemini_kc = byok::read_account_key(byok::GEMINI_KEY_ACCOUNT)
        .ok()
        .flatten()
        .is_some();
    println!("[PROBE] HF embeddinggemma-2 cached: {}", hf_embed.is_some());
    println!("[PROBE] HF Qwen3-4B-Instruct cached: {}", hf_llm.is_some());
    println!(
        "[PROBE] Codex signed_in={}, label={:?}",
        codex.signed_in, codex.label
    );
    println!(
        "[PROBE] Cloud keys present (boolean only): openai(env={openai_env}, kc={openai_kc}), gemini(env={gemini_env}, kc={gemini_kc})"
    );
}

#[test]
#[ignore = "probes real mm-meetingd live captions: partials, finals, voiceprint Known match, stereo spatial L/R + flip, system audio and invalid-key cloud fallback"]
fn e2e_real_live_captions_local_voiceprint_spatial_and_fallback() {
    let bin = e2e_engine_binary();
    let dir = tempfile::tempdir().expect("tempdir");
    let audio = synthesize_meeting_audio(dir.path());

    let (engine, events) = MeetingEngine::spawn(&bin).expect("spawn mm-meetingd");
    engine.initialize().expect("initialize");
    let preload = engine.preload_models(Some("zh")).expect("preload models");
    println!("[LIVE-E2E] preload={preload:?}");

    let person_id = PersonId::new();
    let sample = engine
        .voiceprint_from_clip(&ClipRequest {
            path: audio.speaker_a_ref.clone(),
            start_ms: None,
            end_ms: None,
        })
        .expect("extract speaker A voiceprint");
    let preset_vp = PresetVoiceprint {
        person_id: person_id.to_string(),
        model: sample.model.clone(),
        vector: sample.vector.clone(),
    };

    // Sub-probe A: Stereo mic + Voiceprint + Partials + Finals + Spatial L/R.
    let sim_a = engine
        .simulate_file(json!({
            "path": audio.stereo_mic_wav,
            "origin": "microphone",
            "speed": 1.0,
            "language": "zh",
            "spatialSpacingCm": 14.0,
            "voiceprints": [preset_vp.clone()],
        }))
        .expect("simulate stereo mic");
    let session_a = sim_a["sessionId"].as_str().expect("sessionId").to_string();
    let mut board_a = CaptionBoard::default();
    board_a.begin(&session_a);
    let meeting_a = mm_core::MeetingId::new();
    let mut partials_a = 0_usize;
    let mut spatial_a = Vec::new();
    while let Ok(ev) = events.try_recv() {
        if let mm_engine::EngineEvent::Notification { method, params } = ev {
            match decode_notice(&method, params) {
                Ok(MeetingNotice::Partial(p)) => {
                    partials_a += board_a.apply_partial(&p) as usize;
                }
                Ok(MeetingNotice::Final(f)) => {
                    board_a.apply_final(&f, meeting_a);
                }
                Ok(MeetingNotice::Relabel(r)) => {
                    board_a.apply_relabel(&r);
                }
                Ok(MeetingNotice::Spatial(c)) => spatial_a.push(c),
                _ => {}
            }
        }
    }
    let lines_a = board_a.lines();
    let joined_a: String = lines_a
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let has_known_a = lines_a
        .iter()
        .any(|l| matches!(l.speaker, SpeakerLabel::Known { person_id: p, .. } if p == person_id));
    let laterals_a: Vec<f32> = spatial_a.iter().filter_map(|c| c.lateral()).collect();
    let min_lat_a = laterals_a.iter().copied().fold(f32::INFINITY, f32::min);
    let max_lat_a = laterals_a.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let known_lat_a = spatial_a
        .iter()
        .find(|c| matches!(c.speaker, SpeakerLabel::Known { person_id: p, .. } if p == person_id))
        .and_then(|c| c.lateral())
        .or_else(|| laterals_a.first().copied());
    println!(
        "[LIVE-E2E A] partials={partials_a}, lines={}, known={has_known_a}, spatial_count={}, min_lat={min_lat_a:.3}, max_lat={max_lat_a:.3}, texts={joined_a:?}",
        lines_a.len(),
        spatial_a.len()
    );
    assert!(
        partials_a > 0,
        "expected streaming caption/partial notices in speed=1.0 simulation"
    );
    assert!(
        lines_a.len() >= 2,
        "expected >= 2 finalized utterances, got {}: {:?}",
        lines_a.len(),
        lines_a
    );
    assert!(
        ["退款", "联调", "周五", "周会"]
            .iter()
            .any(|k| joined_a.contains(k)),
        "missing utterance 1 keywords in {joined_a:?}"
    );
    assert!(
        ["预算", "三十七", "审批", "财务", "打款"]
            .iter()
            .any(|k| joined_a.contains(k)),
        "missing utterance 2 keywords in {joined_a:?}"
    );
    assert!(
        has_known_a,
        "expected enrolled voiceprint speaker to be Known; got speakers={:?}",
        lines_a.iter().map(|l| &l.speaker).collect::<Vec<_>>()
    );
    assert!(
        spatial_a.len() >= 2 && min_lat_a < -0.1 && max_lat_a > 0.1,
        "expected Left (< -0.1) and Right (> 0.1) spatial cues, got laterals={laterals_a:?}"
    );

    // Sub-probe B: Spatial flip reverses the sign of lateral coordinates.
    let _sim_b = engine
        .simulate_file(json!({
            "path": audio.stereo_mic_wav,
            "origin": "microphone",
            "speed": 0.0,
            "language": "zh",
            "spatialFlip": true,
            "spatialSpacingCm": 14.0,
            "voiceprints": [preset_vp],
        }))
        .expect("simulate spatial flip");
    let mut spatial_b = Vec::new();
    while let Ok(ev) = events.try_recv() {
        if let mm_engine::EngineEvent::Notification { method, params } = ev
            && let Ok(MeetingNotice::Spatial(c)) = decode_notice(&method, params)
        {
            spatial_b.push(c);
        }
    }
    let known_lat_b = spatial_b
        .iter()
        .find(|c| matches!(c.speaker, SpeakerLabel::Known { person_id: p, .. } if p == person_id))
        .and_then(|c| c.lateral())
        .or_else(|| spatial_b.first().and_then(|c| c.lateral()));
    println!(
        "[LIVE-E2E B] known_lat_before={known_lat_a:?}, known_lat_flipped={known_lat_b:?}, all_flipped={:?}",
        spatial_b
            .iter()
            .filter_map(|c| c.lateral())
            .collect::<Vec<_>>()
    );
    assert!(
        known_lat_a.is_some_and(|x| x < -0.05) && known_lat_b.is_some_and(|x| x > 0.05),
        "expected spatialFlip to reverse Known speaker lateral sign: before={known_lat_a:?}, after={known_lat_b:?}"
    );

    // Sub-probe C: System audio (remote English).
    let sim_c = engine
        .simulate_file(json!({
            "path": audio.remote_system_wav,
            "origin": "system",
            "speed": 1.0,
            "language": "en",
        }))
        .expect("simulate system audio");
    while events.try_recv().is_ok() {}
    assert_eq!(sim_c["origin"].as_str(), Some("system"));
    let finals_c = sim_c["finals"].as_array().cloned().unwrap_or_default();
    let joined_c = finals_c
        .iter()
        .filter_map(|f| f["text"].as_str())
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    println!("[LIVE-E2E C] remote system text={joined_c:?}");
    assert!(
        !finals_c.is_empty(),
        "expected remote system audio finalized caption"
    );
    assert!(
        ["budget", "friday", "approved", "quarterly", "deployment"]
            .iter()
            .any(|k| joined_c.contains(k)),
        "missing expected English remote words in {joined_c:?}"
    );

    // Sub-probe D: Invalid cloud API key falls back transparently to local Qwen3-ASR.
    let sim_d = engine
        .simulate_file(json!({
            "path": audio.mono_mic_wav,
            "origin": "microphone",
            "speed": 1.0,
            "language": "zh",
            "liveAsr": {
                "provider": "openai",
                "model": "gpt-4o-transcribe",
                "apiKey": "sk-invalid-e2e-probe",
            }
        }))
        .expect("simulate cloud fallback");
    while events.try_recv().is_ok() {}
    let finals_d = sim_d["finals"].as_array().cloned().unwrap_or_default();
    let usage_d = sim_d["usage"].as_array().cloned().unwrap_or_default();
    let joined_d: String = finals_d
        .iter()
        .filter_map(|f| f["text"].as_str())
        .collect::<Vec<_>>()
        .join(" ");
    println!(
        "[LIVE-E2E D] fallback finals={}, usage={}, text={joined_d:?}",
        finals_d.len(),
        usage_d.len()
    );
    assert!(
        !finals_d.is_empty(),
        "expected local Qwen3-ASR fallback to emit finals"
    );
    assert!(
        usage_d.is_empty(),
        "invalid cloud key must fall back locally without emitting cloud usage: {usage_d:?}"
    );
    assert!(
        ["预算", "退款", "周会", "联调"]
            .iter()
            .any(|k| joined_d.contains(k)),
        "fallback local ASR did not recognize expected Chinese words: {joined_d:?}"
    );

    engine.shutdown();
}

#[test]
#[ignore = "runs offline local Qwen3-ASR refine + local Qwen3-4B minutes + EmbeddingGemma 2 indexing & hybrid semantic search via mm-embedd"]
fn e2e_real_local_refine_and_local_llm_minutes_offline() {
    let engine_path = e2e_engine_binary();
    let embed_path = e2e_embed_binary();
    let embed_dir = find_cached_hf_model("mlx-community/embeddinggemma-2-bf16")
        .expect("cached mlx-community/embeddinggemma-2-bf16 required");
    let qwen_dir = find_cached_hf_model("mlx-community/Qwen3-4B-Instruct-2507-4bit")
        .expect("cached mlx-community/Qwen3-4B-Instruct-2507-4bit required");

    let tmp = tempfile::tempdir().expect("tempdir");
    let vault = tmp.path().join("vault");
    let rec_dir = tmp.path().join("rec");
    std::fs::create_dir_all(&vault).expect("vault dir");
    std::fs::create_dir_all(&rec_dir).expect("rec dir");
    let audio = synthesize_meeting_audio(tmp.path());

    let cfg = AppConfig {
        vault_path: Some(vault.clone()),
        recordings_dir: Some(rec_dir),
        record_screen: false,
        system_audio: false,
        caption_language: Some("zh".into()),
        refine_after_meeting: true,
        refine_backend: RefineBackend::Local,
        auto_minutes: true,
        minutes_backend: MinutesBackend::Auto,
        use_codex: false,
        local_llm_dir: Some(qwen_dir.clone()),
        embedding_model_dir: Some(embed_dir.clone()),
        ..Default::default()
    };

    let store = Arc::new(Mutex::new(
        Store::open(&tmp.path().join("db.sqlite")).expect("open store"),
    ));
    let (indexer, _ix_events) = Indexer::start(store.clone(), Some(vault.clone()));
    let (writer, _writer_errors) = LiveWriter::start(store.clone(), indexer.clone());

    // Step 1: Enroll Speaker A voiceprint & run live caption simulation via mm-meetingd.
    let (engine, events) = MeetingEngine::spawn(&engine_path).expect("spawn mm-meetingd");
    engine.initialize().expect("initialize");
    let vp_sample = engine
        .voiceprint_from_clip(&ClipRequest {
            path: audio.speaker_a_ref.clone(),
            start_ms: None,
            end_ms: None,
        })
        .expect("voiceprint_from_clip");

    let person_id = PersonId::new();
    let mut meeting = {
        let s = store.lock().unwrap();
        s.upsert_person(
            &mm_core::Person {
                id: person_id,
                display_name: "赵磊".into(),
                email: None,
                note_path: None,
            },
            None,
        )
        .expect("upsert person");
        s.add_voiceprint(&Voiceprint {
            id: VoiceprintId::new(),
            person_id,
            model: vp_sample.model.clone(),
            vector: vp_sample.vector.clone(),
            source: VoiceprintSource::Enrollment,
            speech_ms: vp_sample.speech_ms,
        })
        .expect("add voiceprint");
        let mut m = flow::quick_meeting(&s, "离线本地全链路评审会").expect("quick meeting");
        flow::ensure_note(&cfg, &s, &mut m, &["赵磊".into()])
            .expect("ensure note")
            .expect("vault note");
        m
    };

    let sim = engine
        .simulate_file(json!({
            "path": audio.mono_mic_wav,
            "origin": "microphone",
            "speed": 1.0,
            "language": "zh",
            "voiceprints": [PresetVoiceprint {
                person_id: person_id.to_string(),
                model: vp_sample.model,
                vector: vp_sample.vector,
            }],
        }))
        .expect("simulateFile");
    let sim_session = sim["sessionId"].as_str().expect("sessionId").to_string();
    let mut board = CaptionBoard::default();
    board.begin(&sim_session);
    while let Ok(ev) = events.try_recv() {
        if let mm_engine::EngineEvent::Notification { method, params } = ev {
            match decode_notice(&method, params) {
                Ok(MeetingNotice::Final(f)) => {
                    if let Some(seg) = board.apply_final(&f, meeting.id) {
                        writer.send(WriteMsg::Segment(seg, f.utterance_id));
                    }
                }
                Ok(MeetingNotice::Relabel(r)) => {
                    board.apply_relabel(&r);
                    writer.send(WriteMsg::Relabel(meeting.id, r.from, r.to));
                }
                _ => {}
            }
        }
    }
    writer.flush_blocking();
    engine.shutdown();

    meeting.status = mm_core::MeetingStatus::Processing;
    let _ = audio.duration_ms;
    store
        .lock()
        .unwrap()
        .set_meeting_status(meeting.id, mm_core::MeetingStatus::Live)
        .expect("mark live");
    store
        .lock()
        .unwrap()
        .set_meeting_status(meeting.id, mm_core::MeetingStatus::Processing)
        .expect("mark processing");

    // Step 2: Spawn mm-embedd and load local Qwen3-4B LLM for offline minutes generation.
    let (embed, _ee) = EmbedEngine::spawn(&embed_path).expect("spawn mm-embedd");
    let llm_loaded = embed.llm_load(&qwen_dir).expect("llm/load Qwen3-4B");
    assert!(
        llm_loaded.loaded,
        "expected Qwen3-4B loaded: {llm_loaded:?}"
    );

    let minutes_ctx = MinutesCtx {
        cfg: cfg.clone(),
        store: store.clone(),
        codex: Some(crate::pipeline::CodexStatus::default()),
        embed: Some(embed.clone()),
    };
    let stopped = StoppedMeeting {
        meeting,
        files: CaptureFiles {
            screen: None,
            microphone: Some(audio.mono_mic_wav),
            system_audio: None,
        },
        warnings: vec![],
    };
    let report = process_meeting(
        &minutes_ctx,
        || mm_service::engines::RefineWorker::spawn(&engine_path).map_err(|e| e.to_string()),
        stopped,
        &|step| println!("[OFFLINE-E2E] step: {step}"),
    )
    .expect("process_meeting");
    println!("[OFFLINE-E2E] warnings={:?}", report.warnings);
    assert!(
        report.minutes_done,
        "offline minutes failed: {:?}",
        report.warnings
    );
    assert_eq!(report.meeting.status, mm_core::MeetingStatus::Done);

    let (transcript, min_val) = {
        let s = store.lock().unwrap();
        let t = s.transcript(report.meeting.id).expect("transcript");
        let m = s.minutes(report.meeting.id).expect("minutes query");
        (t, m)
    };
    let joined: String = transcript.iter().map(|t| t.text.as_str()).collect();
    let has_known = transcript
        .iter()
        .any(|t| matches!(t.speaker, SpeakerLabel::Known { person_id: p, .. } if p == person_id));
    let min_val = min_val.expect("stored minutes JSON");
    let min: mm_llm::Minutes = serde_json::from_value(min_val).expect("parse minutes JSON");
    let note_abs = flow::note_path(&cfg, &report.meeting).expect("note path");
    let note = std::fs::read_to_string(&note_abs).expect("read Obsidian note");
    println!(
        "[OFFLINE-E2E] segments={}, known={has_known}, summary={:?}",
        transcript.len(),
        min.summary
    );
    assert!(!transcript.is_empty());
    assert!(
        joined.contains("预算") || joined.contains("退款"),
        "expected Chinese words in refined transcript: {joined:?}"
    );
    assert!(
        has_known,
        "expected voiceprint match in local refine transcript: {transcript:?}"
    );
    assert!(!min.summary.trim().is_empty(), "empty local LLM summary");
    assert!(note.contains("### 摘要") && note.contains("<!-- mm:transcript:start -->"));

    // Step 3: Unload LLM, load EmbeddingGemma 2, index the note, and run real semantic search.
    embed.llm_unload().expect("llm/unload");
    assert!(
        !embed.llm_status().expect("llm/status").loaded,
        "expected LLM unloaded"
    );
    let em_status = embed.load(&embed_dir).expect("model/load EmbeddingGemma 2");
    assert!(em_status.loaded, "expected embedder loaded");

    indexer.send(IndexerMsg::SetEmbedder(Some(Arc::new(embed.clone()))));
    indexer.send(IndexerMsg::Touch(note_abs, mm_index::Priority::Interactive));

    let stamp = current_stamp();
    let deadline = Instant::now() + Duration::from_secs(30);
    let stats = loop {
        let s = store
            .lock()
            .unwrap()
            .index_stats(&stamp)
            .expect("index stats");
        if s.chunks >= 1 && s.embedded == s.chunks {
            break s;
        }
        assert!(
            Instant::now() < deadline,
            "EmbeddingGemma indexer did not converge: {s:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    println!("[OFFLINE-E2E] index stats: {stats:?}");

    let q_vec = embed
        .embed_query("财务审批额度")
        .expect("embed_query with EmbeddingGemma 2");
    let hits = store
        .lock()
        .unwrap()
        .search("财务审批额度", Some(&q_vec), 5)
        .expect("hybrid search");
    println!("[OFFLINE-E2E] semantic search hits={}", hits.len());
    assert!(!hits.is_empty(), "expected semantic search hits");
    assert_eq!(
        hits[0].meeting_title.as_deref(),
        Some("离线本地全链路评审会")
    );

    indexer.send(IndexerMsg::Stop);
    embed.unload().expect("model/unload");
    embed.shutdown();
}

#[test]
#[ignore = "verifies cloud refine failure falls back cleanly to real local Qwen3-ASR refine without mutating Keychain"]
fn e2e_cloud_refine_fallback_to_local_pipeline() {
    let engine_path = e2e_engine_binary();
    let tmp = tempfile::tempdir().expect("tempdir");
    let vault = tmp.path().join("vault");
    let rec_dir = tmp.path().join("rec");
    std::fs::create_dir_all(&vault).expect("vault dir");
    std::fs::create_dir_all(&rec_dir).expect("rec dir");
    let audio = synthesize_meeting_audio(tmp.path());

    let cfg = AppConfig {
        vault_path: Some(vault.clone()),
        recordings_dir: Some(rec_dir),
        record_screen: false,
        system_audio: false,
        caption_language: Some("zh".into()),
        refine_after_meeting: true,
        refine_backend: RefineBackend::OpenAi,
        refine_openai_model: Some("nonexistent-e2e-fallback-model".into()),
        auto_minutes: false,
        ..Default::default()
    };

    let store = Arc::new(Mutex::new(
        Store::open(&tmp.path().join("db.sqlite")).expect("open store"),
    ));
    let meeting = {
        let s = store.lock().unwrap();
        let mut m = flow::quick_meeting(&s, "云端精转失败本地回退测试").expect("quick meeting");
        flow::ensure_note(&cfg, &s, &mut m, &[])
            .expect("ensure note")
            .expect("vault configured");
        s.set_meeting_status(m.id, mm_core::MeetingStatus::Live)
            .expect("live");
        s.set_meeting_status(m.id, mm_core::MeetingStatus::Processing)
            .expect("processing");
        m.status = mm_core::MeetingStatus::Processing;
        m
    };
    let _ = audio.duration_ms;

    let minutes = MinutesCtx {
        cfg: cfg.clone(),
        store: store.clone(),
        codex: None,
        embed: None,
    };
    let stopped = StoppedMeeting {
        meeting,
        files: CaptureFiles {
            screen: None,
            microphone: Some(audio.mono_mic_wav),
            system_audio: None,
        },
        warnings: vec![],
    };
    let report = process_meeting(
        &minutes,
        || mm_service::engines::RefineWorker::spawn(&engine_path).map_err(|e| e.to_string()),
        stopped,
        &|step| println!("[FALLBACK-E2E] step: {step}"),
    )
    .expect("process_meeting");
    println!("[FALLBACK-E2E] warnings={:?}", report.warnings);
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("改用本地精转写") || w.contains("local refine ran instead")),
        "expected cloud fallback warning, got: {:?}",
        report.warnings
    );

    let transcript = store
        .lock()
        .unwrap()
        .transcript(report.meeting.id)
        .expect("transcript");
    let joined: String = transcript.iter().map(|t| t.text.as_str()).collect();
    let note = std::fs::read_to_string(flow::note_path(&cfg, &report.meeting).expect("note path"))
        .expect("read Obsidian note");
    println!("[FALLBACK-E2E] fallback transcript={joined:?}");
    assert!(!transcript.is_empty(), "expected local fallback transcript");
    assert!(
        joined.contains("预算") || joined.contains("退款") || joined.contains("周五"),
        "expected recognizable local fallback words: {joined:?}"
    );
    assert!(note.contains("<!-- mm:transcript:start -->"));
}

/// Reads one HTTP/1.1 request (headers + full body by Content-Length or chunked framing).
fn read_http_request_body(stream: &mut std::net::TcpStream) -> Option<(String, Vec<u8>)> {
    use std::io::Read;
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        raw.extend_from_slice(&buf[..n]);
        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&raw[..header_end]).to_string();
    let mut content_length = 0usize;
    let mut chunked = false;
    for line in head.lines() {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                content_length = v.trim().parse().unwrap_or(0);
            } else if k.trim().eq_ignore_ascii_case("transfer-encoding")
                && v.to_ascii_lowercase().contains("chunked")
            {
                chunked = true;
            }
        }
    }
    if chunked {
        while !raw[header_end..].windows(5).any(|w| w == b"0\r\n\r\n") {
            let n = stream.read(&mut buf).ok()?;
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&buf[..n]);
        }
        // Decode HTTP/1.1 chunked body into raw bytes.
        let mut out = Vec::new();
        let mut cursor = &raw[header_end..];
        while let Some(line_end) = cursor.windows(2).position(|w| w == b"\r\n") {
            let size_hex = String::from_utf8_lossy(&cursor[..line_end]);
            let size_hex = size_hex.split(';').next().unwrap_or("").trim();
            let Ok(sz) = usize::from_str_radix(size_hex, 16) else {
                break;
            };
            cursor = &cursor[line_end + 2..];
            if sz == 0 || cursor.len() < sz {
                break;
            }
            out.extend_from_slice(&cursor[..sz]);
            cursor = &cursor[sz..];
            if cursor.starts_with(b"\r\n") {
                cursor = &cursor[2..];
            }
        }
        return Some((head, out));
    }
    while raw.len() < header_end + content_length {
        let n = stream.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..n]);
    }
    let body_end = (header_end + content_length).min(raw.len());
    Some((head, raw[header_end..body_end].to_vec()))
}

/// Extracts PCM16 mono 16 kHz duration (ms) from a RIFF/WAVE payload embedded in a multipart body.
fn wav_ms_in_multipart(body: &[u8]) -> Option<u64> {
    let riff = body.windows(4).position(|w| w == b"RIFF")?;
    if body.len() < riff + 44 || &body[riff + 8..riff + 12] != b"WAVE" {
        return None;
    }
    let channels = u16::from_le_bytes([body[riff + 22], body[riff + 23]]);
    let sample_rate = u32::from_le_bytes([
        body[riff + 24],
        body[riff + 25],
        body[riff + 26],
        body[riff + 27],
    ]);
    assert_eq!(channels, 1, "expected mono WAV in utterance upload");
    assert_eq!(
        sample_rate, 16_000,
        "expected 16 kHz WAV in utterance upload"
    );
    let data_pos = body[riff..].windows(4).position(|w| w == b"data")? + riff;
    let data_bytes = u32::from_le_bytes([
        body[data_pos + 4],
        body[data_pos + 5],
        body[data_pos + 6],
        body[data_pos + 7],
    ]) as u64;
    Some((data_bytes * 1000) / (u64::from(sample_rate) * 2))
}

#[test]
#[ignore = "verifies real mm-meetingd Silero VAD + RMS gating, Solution B HTTP utterance upload ratio, and HTTP 401 local Qwen3-ASR fallback"]
fn e2e_real_cloud_utterance_vad_gating_and_fallback() {
    use std::io::Write;
    use std::net::TcpListener;

    let engine_path = e2e_engine_binary();
    let dir = tempfile::tempdir().expect("tempdir");

    // Construct a ~10s WAV:
    // 2.5s zeros + 0.15s short blip + 1.5s faint -60 dBFS hiss + ~3s Mandarin speech + 2.5s zeros.
    let speech_wav = dir.path().join("speech.wav");
    run_say_wav(
        Some("Tingting"),
        "今天下午三点召开技术方案评审会，确认接口预算。",
        16_000,
        &speech_wav,
    );
    let (sr, speech) = external::read_wav_mono16(&speech_wav).expect("read speech.wav");
    assert_eq!(sr, 16_000);

    let mut mix: Vec<i16> = Vec::new();
    mix.extend(std::iter::repeat_n(0i16, 40_000)); // 2.5s pure silence
    // 0.15s short 300 Hz pulse blip (2,400 samples = 150ms < Silero 250ms minSpeechDuration)
    for i in 0..2_400 {
        let s = (2.0 * std::f32::consts::PI * 300.0 * (i as f32) / 16_000.0).sin() * 6_000.0;
        mix.push(s.round() as i16);
    }
    mix.extend(std::iter::repeat_n(0i16, 16_000)); // 1.0s gap
    // 1.5s faint -60 dBFS hiss (~32/32768 -> ~0.001 RMS < CloudUploadGate minActiveRMS 0.003)
    for i in 0..24_000 {
        mix.push(if i % 2 == 0 { 30 } else { -30 });
    }
    mix.extend_from_slice(&speech);
    mix.extend(std::iter::repeat_n(0i16, 40_000)); // 2.5s trailing silence
    let total_ms = ((mix.len() as u64) * 1000) / 16_000;
    let gated_wav = dir.path().join("gated_utterance.wav");
    external::write_wav_mono16(&gated_wav, 16_000, &mix).expect("write gated wav");

    // 1. Start local mock OpenAI `/v1/audio/transcriptions` HTTP 200 server.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock HTTP server");
    let port = listener.local_addr().unwrap().port();
    let mock_url = format!("http://127.0.0.1:{port}");
    let uploaded_ms_total = Arc::new(Mutex::new((0usize, 0u64)));
    let recorded_clone = uploaded_ms_total.clone();
    let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_clone = stop_flag.clone();
    listener.set_nonblocking(true).ok();

    let server_handle = std::thread::spawn(move || {
        while !stop_clone.load(std::sync::atomic::Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).ok();
                    if let Some((head, body)) = read_http_request_body(&mut stream) {
                        assert!(
                            head.contains("/v1/audio/transcriptions"),
                            "unexpected path: {head}"
                        );
                        let wav_ms = wav_ms_in_multipart(&body)
                            .expect("multipart upload must carry 16 kHz mono WAV");
                        let mut guard = recorded_clone.lock().unwrap();
                        guard.0 += 1;
                        guard.1 += wav_ms;
                        let resp_body = r#"{"text":"测试按句按需上传","usage":{"input_tokens":14,"output_tokens":9}}"#;
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            resp_body.len(),
                            resp_body
                        );
                        let _ = stream.write_all(resp.as_bytes());
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });

    let (engine, _events) =
        MeetingEngine::spawn_with_env(&engine_path, &[("MM_CLOUD_UTTERANCE_BASE_URL", &mock_url)])
            .expect("spawn mm-meetingd with mock utterance endpoint");
    engine.initialize().expect("initialize");

    let sim = engine
        .simulate_file(json!({
            "path": gated_wav,
            "origin": "microphone",
            "speed": 0.0,
            "language": "zh",
            "liveAsr": {
                "provider": "openai",
                "model": "gpt-transcribe",
                "mode": "utterance",
                "apiKey": "sk-mock-utterance"
            }
        }))
        .expect("simulate Solution B utterance HTTP");
    engine.shutdown();
    stop_flag.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = server_handle.join();

    let (req_count, uploaded_ms) = *uploaded_ms_total.lock().unwrap();
    let finals = sim["finals"].as_array().cloned().unwrap_or_default();
    let usage = sim["usage"].as_array().cloned().unwrap_or_default();
    let joined: String = finals
        .iter()
        .filter_map(|f| f["text"].as_str())
        .collect::<Vec<_>>()
        .join(" ");
    println!(
        "[UTTERANCE-E2E] total_ms={total_ms}, requests={req_count}, uploaded_ms={uploaded_ms}, finals={joined:?}, usage={usage:?}"
    );
    assert!(req_count >= 1, "expected at least 1 utterance upload");
    assert!(
        uploaded_ms >= 1_000 && uploaded_ms < (total_ms * 60) / 100,
        "expected silence/blip/hiss to be gated out (uploaded {uploaded_ms} ms of {total_ms} ms)"
    );
    assert!(
        joined.contains("测试按句按需上传"),
        "expected mock transcript in finals: {finals:?}"
    );
    assert!(!usage.is_empty(), "expected caption/usage events");
    assert_eq!(usage[0]["mode"], "utterance");
    let metered_ms = usage
        .iter()
        .filter_map(|u| u["audioMs"].as_u64())
        .sum::<u64>();
    assert!(
        metered_ms >= 1_000 && metered_ms < (total_ms * 60) / 100,
        "expected usage audioMs to reflect gated speech only ({metered_ms} ms vs {total_ms} ms)"
    );

    // 2. Verify HTTP 401 auth failure in Solution B triggers immediate local Qwen3-ASR fallback.
    let err_listener = TcpListener::bind("127.0.0.1:0").expect("bind 401 server");
    let err_port = err_listener.local_addr().unwrap().port();
    let err_url = format!("http://127.0.0.1:{err_port}");
    let err_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let err_stop_c = err_stop.clone();
    err_listener.set_nonblocking(true).ok();
    let err_handle = std::thread::spawn(move || {
        while !err_stop_c.load(std::sync::atomic::Ordering::Relaxed) {
            match err_listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).ok();
                    let _ = read_http_request_body(&mut stream);
                    let err_body = r#"{"error":{"message":"Incorrect API key provided (invalid_api_key)","type":"invalid_request_error"}}"#;
                    let resp = format!(
                        "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        err_body.len(),
                        err_body
                    );
                    let _ = stream.write_all(resp.as_bytes());
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });

    let (engine2, _events2) =
        MeetingEngine::spawn_with_env(&engine_path, &[("MM_CLOUD_UTTERANCE_BASE_URL", &err_url)])
            .expect("spawn mm-meetingd with 401 endpoint");
    engine2.initialize().expect("initialize");
    let fallback_sim = engine2
        .simulate_file(json!({
            "path": speech_wav,
            "origin": "microphone",
            "speed": 0.0,
            "language": "zh",
            "liveAsr": {
                "provider": "openai",
                "model": "gpt-transcribe",
                "mode": "utterance",
                "apiKey": "sk-invalid-utterance-key"
            }
        }))
        .expect("simulate Solution B 401 fallback");
    engine2.shutdown();
    err_stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = err_handle.join();

    let fb_finals = fallback_sim["finals"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let fb_usage = fallback_sim["usage"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let fb_text: String = fb_finals
        .iter()
        .filter_map(|f| f["text"].as_str())
        .collect::<Vec<_>>()
        .join(" ");
    println!("[UTTERANCE-E2E 401] fallback_finals={fb_text:?}, usage={fb_usage:?}");
    assert!(
        fb_usage.is_empty(),
        "401 auth failure must not emit cloud usage: {fb_usage:?}"
    );
    assert!(
        !fb_finals.is_empty(),
        "expected immediate local Qwen3-ASR fallback on HTTP 401"
    );
    assert!(
        ["评审", "接口", "预算", "技术", "下午"]
            .iter()
            .any(|k| fb_text.contains(k)),
        "local fallback did not recognize expected Chinese words: {fb_text:?}"
    );
}
