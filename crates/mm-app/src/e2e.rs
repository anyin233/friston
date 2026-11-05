//! Live end-to-end check of one meeting through the app's own pipeline, minus the GUI:
//! real `mm-meetingd` capture on the default microphone → live captions persisted by the
//! `LiveWriter` → `finish_meeting` (stop, refine, minutes via the configured provider,
//! note write-back).
//!
//! Ignored by default: it records from the microphone, plays speech through the
//! speakers and uses the signed-in Codex account. Run with
//! `MM_E2E_ENGINE=<path to mm-meetingd> cargo test -p mm-app e2e -- --ignored --nocapture`.
//! The recording folder is deleted at the end because the microphone also hears the room.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mm_service::engines::{MeetingEngine, MeetingNotice, decode_notice};
use mm_service::indexer::Indexer;
use mm_service::live::CaptionBoard;
use mm_service::{AppConfig, flow};
use mm_store::Store;

use crate::logic::default_microphone;
use crate::pipeline::{FinishJob, MinutesCtx, detect_codex, finish_meeting};
use crate::writer::{LiveWriter, WriteMsg};

/// Lines played through the speakers during the recording.
const SCRIPT: &str = "大家好，我们开始周会。第一项，下季度预算上调百分之五。第二项，Alice 负责在周五之前更新预算表。发布日期还没有确定，下周再讨论。";

#[test]
#[ignore = "records the microphone, plays audio and uses the Codex account"]
fn e2e_record_stop_refine_minutes() {
    let engine_path = std::env::var("MM_E2E_ENGINE").expect("set MM_E2E_ENGINE to the mm-meetingd binary");
    let tmp = tempfile::tempdir().expect("temp dir");
    let vault = tmp.path().join("vault");
    std::fs::create_dir_all(&vault).expect("vault dir");
    let cfg = AppConfig {
        vault_path: Some(vault.clone()),
        recordings_dir: Some(tmp.path().join("rec")),
        record_screen: false,
        system_audio: false,
        caption_language: Some("zh".into()),
        ..Default::default()
    };
    let store = Arc::new(Mutex::new(Store::open(&tmp.path().join("db.sqlite")).expect("open store")));
    let (indexer, _ix_events) = Indexer::start(store.clone(), Some(vault.clone()));
    let (writer, _writer_errors) = LiveWriter::start(store.clone(), indexer);
    let (engine, events) = MeetingEngine::spawn(std::path::Path::new(&engine_path)).expect("spawn engine");
    engine.initialize().expect("initialize");

    let mic = default_microphone(&engine.probe_inputs().expect("probe inputs")).expect("a microphone");
    let mut meeting = {
        let s = store.lock().unwrap();
        let mut m = flow::quick_meeting(&s, "端到端测试周会").expect("meeting");
        flow::ensure_note(&cfg, &s, &mut m, &[]).expect("note").expect("vault configured");
        m
    };
    let mut preset = flow::build_preset(&cfg, &store.lock().unwrap(), &meeting, None).expect("preset");
    preset.microphone_uid = Some(mic);
    let started = engine.capture_start(&preset).expect("capture/start");
    flow::mark_live(&store.lock().unwrap(), &mut meeting, &started).expect("mark live");

    // Speak through the speakers while recording; the built-in mic picks it up.
    let speech = tmp.path().join("script.aiff");
    let ok = std::process::Command::new("say").args(["-v", "Tingting", "-o"]).arg(&speech).arg(SCRIPT).status().expect("say");
    assert!(ok.success());
    std::thread::sleep(Duration::from_secs(4)); // models finish loading
    let player = std::process::Command::new("afplay").args(["-v", "1"]).arg(&speech).spawn().expect("afplay");

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
    let note = std::fs::read_to_string(flow::note_path(&cfg, &report.meeting).expect("note path")).expect("read note");
    println!(
        "final transcript: {} segments; heard budget line: {}; status {:?}; minutes block filled: {}",
        transcript.len(),
        joined.contains("预算"),
        report.meeting.status,
        note.contains("### 摘要")
    );
    // The microphone also captured the room: never keep the recording.
    if let Err(e) = std::fs::remove_dir_all(&started.directory) {
        println!("could not delete recording {}: {e}", started.directory.display());
    }
    assert!(finals > 0, "no live captions arrived");
    assert!(report.minutes_done, "minutes were not generated: {:?}", report.warnings);
    assert_eq!(report.meeting.status, mm_core::MeetingStatus::Done);
    assert!(note.contains("### 摘要") && note.contains("<!-- mm:transcript:start -->"));
}
