//! Native headless GPUI GUI integration and E2E tests powered by `gpui-kit` `test-support`.
//!
//! No `use gpui_kit::*;` here so GPUI's `test` macro never shadows Rust's `#[test]`.
#![allow(clippy::await_holding_lock)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use chrono::Utc;
use gpui_kit::base::test_support::texts;
use gpui_kit::test::{TestAppContextExt as _, TestQueryExt as _, TestWindowExt as _};
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, ElementId, Entity, InputEvent as _, ScrollDelta,
    ScrollEvent, TestAppContext, Window, WindowBounds, WindowOptions, point, px, size,
};
use mm_core::{
    AudioLevels, AudioOrigin, CalendarEvent, CaptionFinal, CaptionPartial, CaptionUsage,
    CaptureFiles, CaptureStarted, CaptureState, CaptureStateEvent, ClipRequest, CloudLiveMode,
    LevelSource, Meeting, MeetingStatus, PermissionStatus, Person, PersonId, PresetVoiceprint,
    SpatialCueEvent, SpeakerLabel, SpeakerRelabel, TranscriptSegment, Voiceprint, VoiceprintId,
    VoiceprintSource,
};
use mm_llm::{
    ActionItem, AsrModelList, AsrProvider, ByokProvider, Minutes, MinutesBackend, ModelOption,
};
use mm_service::engines::{EmbedEngine, MeetingEngine, MeetingNotice, decode_notice};
use mm_service::indexer::{IndexerMsg, current_stamp};
use mm_service::{LiveAsrBackend, RefineBackend, external, flow};
use mm_store::{AsrUsageRecord, SegmentPass};
use tempfile::TempDir;

use crate::e2e::{
    e2e_embed_binary, e2e_engine_binary, find_cached_hf_model, synthesize_meeting_audio,
};
use crate::pipeline::{
    CodexStatus, FinishJob, MinutesCtx, StoppedMeeting, detect_codex, finish_meeting,
    process_meeting,
};
use crate::state::{AppState, LoadError, Loadable, apply_ui_language};
use crate::workbench::Workbench;
use crate::writer::LiveWriter;

const VOICE_A_TEXT: &str = "今天讨论退款接口联调，赵磊周五之前完成。";
const VOICE_B_TEXT: &str = "好的，预算已经批了，三十七万五千元。";

/// Serializes GUI tests in this module so `mm_i18n`'s global language cannot race across threads
/// and resets back to Simplified Chinese on drop.
struct GuiTestGuard(#[allow(dead_code)] MutexGuard<'static, ()>);

impl Drop for GuiTestGuard {
    fn drop(&mut self) {
        apply_ui_language(mm_i18n::Lang::Zh);
    }
}

fn lock_gui() -> GuiTestGuard {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let guard = LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    apply_ui_language(mm_i18n::Lang::Zh);
    GuiTestGuard(guard)
}

/// Creates an isolated temp data directory with `vault/` and `recordings/` subfolders.
fn create_test_env(config: serde_json::Value) -> (TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let vault = tmp.path().join("vault");
    let rec = tmp.path().join("recordings");
    std::fs::create_dir_all(&vault).expect("vault dir");
    std::fs::create_dir_all(&rec).expect("rec dir");

    let mut cfg = config;
    if let Some(obj) = cfg.as_object_mut() {
        obj.entry("vault_path")
            .or_insert_with(|| serde_json::json!(vault));
        obj.entry("recordings_dir")
            .or_insert_with(|| serde_json::json!(rec));
        obj.entry("auto_download_embedding")
            .or_insert_with(|| serde_json::json!(false));
        obj.entry("record_screen")
            .or_insert_with(|| serde_json::json!(false));
        obj.entry("system_audio")
            .or_insert_with(|| serde_json::json!(false));
        obj.entry("ui_language")
            .or_insert_with(|| serde_json::json!("zh"));
    }
    std::fs::write(
        tmp.path().join("config.json"),
        serde_json::to_string_pretty(&cfg).expect("serialize config"),
    )
    .expect("write test config");
    (tmp, vault, rec)
}

/// Opens the full `Workbench` backed by an isolated directory in a headless `1440x900` window
/// and installs application keyboard shortcuts + menus.
fn open_workbench(
    cx: &mut TestAppContext,
    dir: &Path,
) -> (AnyWindowHandle, Entity<AppState>, Entity<Workbench>) {
    // Allow OS worker threads (`live-writer`, `indexer`) spawned by AppState without disabling
    // deterministic simulated test timers during `wait_for`.
    cx.executor().allow_parking();
    cx.executor().forbid_parking();
    cx.update(|cx| {
        gpui_kit::init(cx);
        cx.set_reduce_motion(true);
    });
    let data_dir = dir.to_path_buf();
    let mut state_slot = None;
    let (handle, workbench) = cx.update(|cx| {
        let bounds = Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(1440.), px(900.)),
        };
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            cx,
            |window, cx| {
                let state = cx.new(|cx| AppState::new_for_test(data_dir, cx));
                state_slot = Some(state.clone());
                let s = state.clone();
                cx.new(|cx| Workbench::new(s, window, cx))
            },
        )
        .expect("open test workbench window")
    });
    let state = state_slot.expect("AppState initialized");
    cx.update(|cx| {
        crate::shortcuts::init(state.clone(), handle, cx);
    });
    cx.run_until_parked();
    (handle, state, workbench)
}

/// Dispatches a vertical mouse-wheel scroll inside the active right-hand content pane.
fn scroll_right_pane(window: &mut Window, dy: f32, cx: &mut App) {
    window.render_frame(cx);
    window.dispatch_event(
        ScrollEvent {
            position: point(px(800.), px(450.)),
            delta: ScrollDelta::Pixels(point(px(0.), px(dy))),
            modifiers: window.modifiers(),
            ..Default::default()
        }
        .to_platform_input(),
        cx,
    );
    window.render_frame(cx);
}

/// Scrolls the right-hand pane up or down until `id` is comfortably inside the viewport.
fn scroll_right_pane_to_id(window: &mut Window, id: impl Into<ElementId> + Clone, cx: &mut App) {
    window.render_frame(cx);
    let mut last_y = None;
    for _ in 0..40 {
        if let Some(snap) = window.try_find(id.clone()) {
            let b = snap.bounds();
            if snap.visible()
                && ((b.origin.y >= px(90.) && b.origin.y + b.size.height <= px(820.))
                    || last_y == Some(b.origin.y))
            {
                return;
            }
            last_y = Some(b.origin.y);
            let dy = if b.origin.y < px(90.) { 220. } else { -220. };
            scroll_right_pane(window, dy, cx);
        } else {
            last_y = None;
            scroll_right_pane(window, -220., cx);
        }
    }
    panic!(
        "ElementId {:?} never became centered in viewport while scrolling",
        id.into()
    );
}

/// Scrolls the right-hand pane down until painted text containing `needle` is visible.
fn scroll_right_pane_to_text(window: &mut Window, needle: &str, cx: &mut App) {
    window.render_frame(cx);
    for _ in 0..40 {
        if has_visible_text(window, needle) {
            return;
        }
        scroll_right_pane(window, -260., cx);
    }
    let painted: Vec<_> = texts(window)
        .into_iter()
        .map(|t| t.text().to_string())
        .collect();
    panic!("text containing {needle:?} never appeared while scrolling; painted={painted:?}");
}

/// Checks whether any visible painted text contains `needle`.
fn has_visible_text(window: &Window, needle: &str) -> bool {
    texts(window)
        .into_iter()
        .any(|t| t.visible() && t.text().contains(needle))
}

/// Checks whether any painted text (even off-screen) contains `needle`.
fn has_any_text(window: &Window, needle: &str) -> bool {
    texts(window).into_iter().any(|t| t.text().contains(needle))
}

/// Reads and parses `<dir>/config.json`.
fn read_saved_config(dir: &Path) -> serde_json::Value {
    let raw = std::fs::read_to_string(dir.join("config.json")).expect("read saved config.json");
    serde_json::from_str(&raw).expect("parse saved config.json")
}

/// Asserts no visible/painted UI text or accessibility label references Antigravity/agy.
/// (`Gemini` stays allowed on the API-key provider segmented control.)
fn assert_no_antigravity_in_ui(window: &Window) {
    let forbidden = ["antigravity", "agy"];
    for t in texts(window) {
        let lower = t.text().to_lowercase();
        for bad in forbidden {
            assert!(
                !lower.contains(bad),
                "unexpected legacy reference {bad:?} in painted text: {:?}",
                t.text()
            );
        }
    }
    for el in window.elements() {
        if let Some(label) = el.label() {
            let lower = label.to_lowercase();
            for bad in forbidden {
                assert!(
                    !lower.contains(bad),
                    "unexpected legacy reference {bad:?} in element label: {label:?}"
                );
            }
        }
    }
}

/// Inserts a finished two-speaker meeting with optional full minutes & indexed Obsidian note.
fn seed_meeting_with_transcript(
    state: &Entity<AppState>,
    title: &str,
    with_minutes: bool,
    cx: &mut App,
) -> Meeting {
    let (cfg, store) = {
        let app = state.read(cx);
        (app.cfg.clone(), app.store.clone())
    };
    let mut s = store.lock().expect("lock store");
    let mut meeting = flow::quick_meeting(&s, title).expect("create meeting");
    flow::ensure_note(&cfg, &s, &mut meeting, &["赵磊".into()]).expect("ensure note");
    s.set_meeting_status(meeting.id, MeetingStatus::Live)
        .expect("set live status");
    meeting.status = MeetingStatus::Live;

    let segments = vec![
        TranscriptSegment {
            meeting_id: meeting.id,
            start_ms: 0,
            end_ms: 3_000,
            text: VOICE_A_TEXT.into(),
            language: Some("zh".into()),
            speaker: SpeakerLabel::Cluster { index: 1 },
            origin: AudioOrigin::Microphone,
            spatial: None,
        },
        TranscriptSegment {
            meeting_id: meeting.id,
            start_ms: 3_500,
            end_ms: 6_500,
            text: VOICE_B_TEXT.into(),
            language: Some("zh".into()),
            speaker: SpeakerLabel::Cluster { index: 2 },
            origin: AudioOrigin::Microphone,
            spatial: None,
        },
    ];
    s.replace_segments(meeting.id, SegmentPass::Final, &segments)
        .expect("save segments");
    flow::sync_transcript(&cfg, &mut s, &meeting).expect("sync transcript");

    if with_minutes {
        let minutes = Minutes {
            title: title.into(),
            summary: "本次会议确认周五前完成退款接口联调，预算三十七万五千元已获批。".into(),
            decisions: vec!["赵磊负责周五前完成退款接口联调".into()],
            action_items: vec![ActionItem {
                owner: Some("赵磊".into()),
                task: "提交退款接口联调测试报告".into(),
                due: Some("周五前".into()),
                evidence_ms: Some(0),
            }],
            open_questions: vec!["下周确定灰度发布窗口".into()],
            topics: vec!["退款接口".into()],
        };
        flow::apply_minutes(&cfg, &mut s, &mut meeting, &minutes).expect("apply minutes");
    } else {
        s.set_meeting_status(meeting.id, MeetingStatus::Processing)
            .expect("set processing");
        s.set_meeting_status(meeting.id, MeetingStatus::Done)
            .expect("set done");
        meeting.status = MeetingStatus::Done;
    }
    drop(s);
    state.update(cx, |s, cx| s.refresh_meetings(cx));
    meeting
}

/// Synthesizes two Chinese utterances via macOS `say` and stitches them into `mic.wav`.
fn synthesize_two_speaker_wav(dir: &Path) -> PathBuf {
    fn speak(voice: &str, text: &str, out: &Path) {
        let status = Command::new("say")
            .args(["-v", voice, "-o"])
            .arg(out)
            .args(["--file-format=WAVE", "--data-format=LEI16@16000", text])
            .status()
            .expect("invoke macOS say");
        assert!(status.success(), "say -v {voice} failed");
    }

    let a = dir.join("speaker_a.wav");
    let b = dir.join("speaker_b.wav");
    speak("Tingting", VOICE_A_TEXT, &a);
    speak("Eddy (Chinese (China mainland))", VOICE_B_TEXT, &b);

    let (_, mut samples) = external::read_wav_mono16(&a).expect("read speaker A wav");
    samples.extend(vec![0i16; 8_000]);
    samples.extend(external::read_wav_mono16(&b).expect("read speaker B wav").1);

    let out = dir.join("mic.wav");
    external::write_wav_mono16(&out, 16_000, &samples).expect("write stitched wav");
    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);
    out
}

#[gpui_kit::test]
async fn gui_navigation_shortcuts_and_deep_links(cx: &mut TestAppContext) {
    let _guard = lock_gui();
    let (tmp, _, _) = create_test_env(serde_json::json!({}));
    let (handle, state, _) = open_workbench(cx, tmp.path());

    // Verify sidebar navigation across all 6 pages.
    let sidebar_checks = [
        ("0-0-0", "今天的日程"),
        ("0-0-1", "日程"),
        ("0-0-2", "还没有开始记录"),
        ("0-0-3", "纪要库"),
        ("0-0-4", "说话人"),
        ("0-0-5", "设置"),
    ];
    cx.update_window(handle, |_, window, cx| {
        for (nav_id, expected_text) in sidebar_checks {
            window.click(nav_id, cx);
            window.render_frame(cx);
            assert!(
                has_any_text(window, expected_text),
                "clicking sidebar {nav_id} did not render {expected_text:?}"
            );
        }
    })
    .expect("sidebar navigation");

    // Verify all navigation keyboard shortcuts + Cmd-F library search focus.
    let shortcut_checks = [
        ("cmd-1", "今天的日程"),
        ("cmd-2", "日程"),
        ("cmd-3", "还没有开始记录"),
        ("cmd-4", "纪要库"),
        ("cmd-5", "说话人"),
        ("cmd-,", "设置"),
    ];
    for (key, expected_text) in shortcut_checks {
        cx.update_window(handle, |_, window, cx| {
            window.press(key, cx);
        })
        .expect("press navigation shortcut");
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(
                has_any_text(window, expected_text),
                "shortcut {key} did not render {expected_text:?}"
            );
        })
        .expect("verify shortcut destination");
    }

    cx.update_window(handle, |_, window, cx| {
        window.press("cmd-f", cx);
    })
    .expect("press cmd-f");
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("lib-search").focused(), Some(true));
    })
    .expect("cmd-f focused library search");

    // Seed a meeting and exercise both friston:// and legacy meeting-master:// deep links.
    let meeting = cx.update(|cx| seed_meeting_with_transcript(&state, "深链接测试会议", true, cx));
    cx.run_until_parked();

    for scheme in ["friston", "meeting-master"] {
        let url = format!("{scheme}://open?meeting={}&t=3500", meeting.id);
        cx.update(|cx| {
            state.update(cx, |s, cx| s.open_url(&url, cx));
        });
        cx.wait_for(handle, Duration::from_secs(2), |window, _| {
            window.try_find("regen").is_some()
        })
        .await;
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(has_any_text(window, "从链接定位到 00:03"));
            assert!(has_any_text(window, "深链接测试会议"));
        })
        .expect("verify deep link opened meeting at timestamp");
    }

    // Invalid URL should warn safely without panicking or losing page state.
    cx.update(|cx| {
        state.update(cx, |s, cx| s.open_url("friston://unknown-action", cx));
    });
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(has_any_text(window, "深链接测试会议"));
    })
    .expect("invalid deep link handled cleanly");
}

#[gpui_kit::test]
async fn gui_today_and_schedule_pages_events_permissions_and_actions(cx: &mut TestAppContext) {
    let _guard = lock_gui();
    let (tmp, _, _) = create_test_env(serde_json::json!({}));
    let (handle, state, _) = open_workbench(cx, tmp.path());

    // Empty Today + empty Schedule states.
    cx.update(|cx| {
        state.update(cx, |s, cx| {
            s.set_events_for_test(Loadable::Ready(vec![]), cx)
        });
    });
    cx.run_until_parked();

    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(has_any_text(window, "没有安排。"));
        assert!(has_any_text(window, "还没有会议记录。"));
        assert!(has_any_text(window, "纪要里的待办会出现在这里。"));

        window.click("0-0-1", cx);
        window.render_frame(cx);
        assert!(has_any_text(window, "未来 7 天没有日历上的会议。"));
    })
    .expect("verify empty Today & Schedule");

    // Calendar permission error renders System Settings helper on both Today and Schedule.
    cx.update(|cx| {
        state.update(cx, |s, cx| {
            s.set_events_for_test(
                Loadable::Failed(LoadError {
                    message: "calendar access denied".into(),
                    permission: true,
                }),
                cx,
            );
        });
    });
    cx.run_until_parked();

    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("cal-err").visible());
        assert!(has_any_text(window, "calendar access denied"));
        window.click("0-0-0", cx);
        window.render_frame(cx);
        assert!(window.find("cal-err-today").visible());
    })
    .expect("verify calendar permission error cards");

    // Populate two calendar events (one recordable now/soon, one tomorrow) + a completed meeting.
    let now = Utc::now();
    let ev1 = CalendarEvent {
        id: "ev-now".into(),
        title: "产品周会 Sync".into(),
        start: now + chrono::Duration::minutes(5),
        end: now + chrono::Duration::minutes(35),
        calendar: "Work".into(),
        location: Some("Zoom".into()),
        join_url: Some("https://meet.example.com/standup".into()),
        attendees: vec!["赵磊".into(), "李明".into()],
    };
    let ev2 = CalendarEvent {
        id: "ev-tomorrow".into(),
        title: "预算评审会".into(),
        start: now + chrono::Duration::days(1),
        end: now + chrono::Duration::days(1) + chrono::Duration::hours(1),
        calendar: "Finance".into(),
        location: None,
        join_url: Some("https://meet.example.com/budget".into()),
        attendees: vec!["王强".into()],
    };
    cx.update(|cx| {
        seed_meeting_with_transcript(&state, "昨日复盘会议", true, cx);
        state.update(cx, |s, cx| {
            s.set_events_for_test(Loadable::Ready(vec![ev1, ev2]), cx);
        });
    });
    cx.run_until_parked();

    // Sidebar offers both calendar-meeting record button and ad-hoc record button; Today links work.
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("record-event").label(), Some("记录当前会议"));
        assert_eq!(window.find("quick-record").label(), Some("临时记录"));
        assert!(has_any_text(window, "产品周会 Sync"));
        assert!(has_any_text(window, "赵磊、李明"));
        assert!(has_any_text(window, "昨日复盘会议"));
        assert!(has_any_text(window, "提交退款接口联调测试报告"));

        window.click("next-join", cx);
    })
    .expect("click join URL on Today");
    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://meet.example.com/standup")
    );

    // "View week" emits AppEvent::Navigate(Schedule); flush before asserting Schedule UI.
    cx.update_window(handle, |_, window, cx| {
        window.click("today-schedule", cx);
    })
    .expect("click View week on Today");
    cx.run_until_parked();

    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(has_any_text(window, "产品周会 Sync"));
        assert!(has_any_text(window, "预算评审会"));
        window.click(("join", 2usize), cx);
    })
    .expect("verify Schedule and click join");
    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://meet.example.com/budget")
    );

    // Clicking recent meeting on Today's right rail navigates to Library & opens its detail.
    cx.update_window(handle, |_, window, cx| {
        window.click("0-0-0", cx);
        window.render_frame(cx);
        window.click(("recent", 0usize), cx);
    })
    .expect("click recent meeting on Today");
    cx.wait_for(handle, Duration::from_secs(2), |window, _| {
        window.try_find("regen").is_some()
    })
    .await;
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(has_any_text(window, "昨日复盘会议"));
    })
    .expect("Library opened clicked recent meeting");
}

#[gpui_kit::test]
async fn gui_live_session_captions_speaker_assign_notes_floating_and_post_queue(
    cx: &mut TestAppContext,
) {
    let _guard = lock_gui();
    let (tmp, _, rec_dir) = create_test_env(serde_json::json!({
        "system_audio": true,
        "auto_minutes": true
    }));
    let (handle, state, _) = open_workbench(cx, tmp.path());

    // Idle Live page -> "Recording settings" navigates to Settings via AppEvent::Navigate.
    cx.update_window(handle, |_, window, cx| {
        window.click("0-0-2", cx);
        window.render_frame(cx);
        assert!(has_any_text(window, "还没有开始记录"));
        window.click("live-open-settings", cx);
    })
    .expect("click Recording settings from idle Live page");
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(has_any_text(window, "修改后立即保存"));
        window.click("0-0-2", cx);
    })
    .expect("verify Settings arrived and return to Live");

    // Start a mock LiveSession backed by a real DB row & Obsidian note file.
    let done_meeting =
        cx.update(|cx| seed_meeting_with_transcript(&state, "已整理完成测试", true, cx));
    cx.run_until_parked();

    let (live_meeting, note_file) = cx.update(|cx| {
        let (cfg, store) = {
            let app = state.read(cx);
            (app.cfg.clone(), app.store.clone())
        };
        let session_dir = rec_dir.join("live-session-1");
        std::fs::create_dir_all(&session_dir).expect("session dir");
        let s = store.lock().expect("lock store");
        let mut m = flow::quick_meeting(&s, "实时会议全功能测试").expect("create live meeting");
        let note = flow::ensure_note(&cfg, &s, &mut m, &[])
            .expect("ensure note")
            .expect("note path");
        let started = CaptureStarted {
            session_id: "sess-1".into(),
            directory: session_dir,
            files: CaptureFiles::default(),
            microphone_channels: Some(2),
            started_at: Utc::now(),
        };
        flow::mark_live(&s, &mut m, &started).expect("mark live");
        drop(s);

        state.update(cx, |app, cx| {
            app.start_mock_live_for_test(
                m.clone(),
                started,
                Some(note.clone()),
                "会议前预设笔记".into(),
                cx,
            );
        });
        (m, note)
    });
    cx.run_until_parked();

    // Feed live partial, final (mic & remote system), relabel, audio levels, and spatial cue notices.
    cx.update(|cx| {
        state.update(cx, |app, cx| {
            app.inject_notice_for_test(
                MeetingNotice::Partial(CaptionPartial {
                    session_id: "sess-1".into(),
                    utterance_id: 1,
                    origin: AudioOrigin::Microphone,
                    start_ms: 0,
                    text: "正在讨论".into(),
                    speaker: SpeakerLabel::Cluster { index: 1 },
                    stable_chars: 2,
                }),
                cx,
            );
            app.inject_notice_for_test(
                MeetingNotice::Final(CaptionFinal {
                    session_id: "sess-1".into(),
                    utterance_id: 1,
                    origin: AudioOrigin::Microphone,
                    start_ms: 0,
                    end_ms: 2_500,
                    text: "请赵磊牵头退款接口。".into(),
                    language: Some("zh".into()),
                    speaker: SpeakerLabel::Cluster { index: 1 },
                    spatial: None,
                }),
                cx,
            );
            app.inject_notice_for_test(
                MeetingNotice::Final(CaptionFinal {
                    session_id: "sess-1".into(),
                    utterance_id: 2,
                    origin: AudioOrigin::System,
                    start_ms: 3_000,
                    end_ms: 5_000,
                    text: "远程参会人已确认预算。".into(),
                    language: Some("zh".into()),
                    speaker: SpeakerLabel::Cluster { index: 2 },
                    spatial: None,
                }),
                cx,
            );
            app.inject_notice_for_test(
                MeetingNotice::Spatial(SpatialCueEvent {
                    session_id: "sess-1".into(),
                    utterance_id: 1,
                    origin: AudioOrigin::Microphone,
                    start_ms: 0,
                    end_ms: 2_500,
                    position: vec![-0.45, -0.45, 0.0],
                    confidence: 0.9,
                    azimuth_deg: Some(-25.0),
                    speaker: SpeakerLabel::Cluster { index: 1 },
                }),
                cx,
            );
            app.inject_notice_for_test(
                MeetingNotice::Levels(AudioLevels {
                    session_id: "sess-1".into(),
                    source: LevelSource::Microphone,
                    rms_db: vec![-18.0, -20.0],
                    peak_db: vec![-8.0, -9.0],
                }),
                cx,
            );
            app.inject_notice_for_test(
                MeetingNotice::Relabel(SpeakerRelabel {
                    session_id: "sess-1".into(),
                    from: SpeakerLabel::Cluster { index: 99 },
                    to: SpeakerLabel::Cluster { index: 1 },
                }),
                cx,
            );
            app.inject_notice_for_test(
                MeetingNotice::Usage(CaptionUsage {
                    session_id: "sess-1".into(),
                    provider: "openai".into(),
                    model: "gpt-4o-transcribe".into(),
                    origin: AudioOrigin::Microphone,
                    audio_ms: 15_000,
                    input_tokens: 120,
                    output_tokens: 45,
                    mode: None,
                }),
                cx,
            );
        });
    });
    cx.run_until_parked();

    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("quick-record").label(), Some("结束记录"));
        assert_eq!(window.find("live-stop").label(), Some("结束并生成纪要"));
        assert!(has_any_text(window, "● 录制中"));
        assert!(has_any_text(window, "实时会议全功能测试"));
        assert!(has_any_text(window, "请赵磊牵头退款接口。"));
        assert!(has_any_text(window, "远程参会人已确认预算。"));
        assert!(has_any_text(window, "远程"));
        assert!(has_any_text(window, "说话人：音色 + 方位"));
        assert!(has_any_text(window, "系统声音"));
    })
    .expect("verify live capture bar & captions");

    // Click "open-obsidian" before speaker-assign toast appears -> opens obsidian://open? URI.
    cx.update_window(handle, |_, window, cx| {
        window.click("open-obsidian", cx);
    })
    .expect("click open-obsidian");
    assert!(
        cx.opened_url()
            .is_some_and(|u| u.starts_with("obsidian://open?")),
        "expected obsidian://open? URL, got {:?}",
        cx.opened_url()
    );

    // Click anonymous speaker label ("spk", 0usize), open "Who is speaker 1?", create "李明".
    cx.update_window(handle, |_, window, cx| {
        window.click(("spk", 0usize), cx);
        window.render_frame(cx);
        assert!(window.find("assign-name").visible());
        window.click("assign-name", cx);
        window.input("李明", cx);
        window.click("assign-new", cx);
    })
    .expect("assign speaker via dialog");

    cx.wait_for(handle, Duration::from_secs(2), |window, _| {
        has_any_text(window, "李明")
    })
    .await;

    // Edit live notes, insert timestamp, flush debounce, and verify saved Obsidian note on disk.
    cx.update_window(handle, |_, window, cx| {
        window.click("live-notes-box", cx);
        window.input(" | 补充确认周五前交付", cx);
        window.click("ts", cx);
    })
    .expect("type live notes and insert timestamp");

    cx.wait_for(handle, Duration::from_secs(3), |window, _| {
        let _ = window;
        std::fs::read_to_string(&note_file)
            .map(|s| s.contains("补充确认周五前交付"))
            .unwrap_or(false)
    })
    .await;
    let saved_note = std::fs::read_to_string(&note_file).expect("read updated live note");
    assert!(saved_note.contains("会议前预设笔记"));
    assert!(saved_note.contains("补充确认周五前交付"));
    assert!(saved_note.contains("[00:"));

    // Open floating captions window, verify captions inside it, then close via "float-close".
    cx.update_window(handle, |_, window, cx| {
        window.click("float", cx);
    })
    .expect("click float toggle");

    cx.wait_for(handle, Duration::from_secs(2), |_, cx| {
        cx.windows().len() == 2
    })
    .await;

    let float_handle = cx.read(|cx| {
        cx.windows()
            .into_iter()
            .find(|w| *w != handle)
            .expect("floating window handle")
    });

    cx.update_window(float_handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(has_any_text(window, "请赵磊牵头退款接口。"));
        window.click("float-close", cx);
    })
    .expect("verify floating window content and close it");

    cx.wait_for(handle, Duration::from_secs(2), |_, cx| {
        cx.windows().len() == 1
    })
    .await;

    // End mock live session and test PostQueue progress cards + "View in library" button.
    cx.update(|cx| {
        state.update(cx, |app, cx| {
            app.end_mock_live_for_test(cx);
            let q = app.post_queue_mut_for_test();
            // Finished card at index 0 ("post-open", 0usize).
            q.push_stopping(done_meeting.id, done_meeting.title.clone());
            q.stopped(
                done_meeting.id,
                StoppedMeeting {
                    meeting: done_meeting.clone(),
                    files: CaptureFiles::default(),
                    warnings: vec![],
                },
                vec!["录音已停止".into()],
                vec![],
            );
            let _ = q.start_next();
            q.finish(done_meeting.id, vec!["提示：测试非致命警告".into()]);

            // Running Codex unified card at index 1.
            q.push_stopping(live_meeting.id, live_meeting.title.clone());
            q.stopped(
                live_meeting.id,
                StoppedMeeting {
                    meeting: live_meeting.clone(),
                    files: CaptureFiles::default(),
                    warnings: vec![],
                },
                vec!["录音已停止".into()],
                vec![],
            );
            let _ = q.start_next();
            q.step(live_meeting.id, "正在用 Codex 转写录音并生成纪要…".into());
        });
    });
    cx.run_until_parked();

    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(has_any_text(window, "会后整理：已整理完成测试"));
        assert!(has_any_text(window, "⚠ 提示：测试非致命警告"));
        assert!(has_any_text(window, "正在用 Codex 转写录音并生成纪要…"));
        window.click(("post-open", 0usize), cx);
    })
    .expect("click View in library on completed post-meeting card");

    cx.wait_for(handle, Duration::from_secs(2), |window, _| {
        window.try_find("regen").is_some()
    })
    .await;
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(has_any_text(window, "已整理完成测试"));
    })
    .expect("Library opened meeting from post-meeting card");
}

#[gpui_kit::test]
async fn gui_library_list_detail_search_and_speaker_rename(cx: &mut TestAppContext) {
    let _guard = lock_gui();
    let (tmp, _, _) = create_test_env(serde_json::json!({}));
    let (handle, state, _) = open_workbench(cx, tmp.path());

    // Open Library while empty.
    cx.update_window(handle, |_, window, cx| {
        window.click("0-0-3", cx);
        window.render_frame(cx);
        assert!(has_any_text(
            window,
            "还没有会议。开始一场记录后会出现在这里。"
        ));
    })
    .expect("verify empty Library");

    // Seed two meetings (one full minutes + transcript, one without minutes) and an existing person.
    let meeting_id = cx.update(|cx| {
        let store = state.read(cx).store.clone();
        {
            let s = store.lock().expect("lock store");
            let p = Person {
                id: PersonId::new(),
                display_name: "王强".into(),
                email: None,
                note_path: None,
            };
            s.upsert_person(&p, Some(120)).expect("insert 王强");
        }
        state.update(cx, |s, cx| s.refresh_people(cx));
        let _bare = seed_meeting_with_transcript(&state, "无纪要草稿会议", false, cx);
        seed_meeting_with_transcript(&state, "退款联调纪要详查", true, cx).id
    });
    cx.run_until_parked();

    cx.wait_for(handle, Duration::from_secs(2), |window, _| {
        window
            .try_find(("meeting", 0usize))
            .is_some_and(|s| s.visible())
    })
    .await;

    // Click the most recent meeting ("退款联调纪要详查") -> verify all sections.
    cx.update_window(handle, |_, window, cx| {
        window.click(("meeting", 0usize), cx);
    })
    .expect("open meeting detail");

    cx.wait_for(handle, Duration::from_secs(2), |window, _| {
        window.try_find("regen").is_some()
    })
    .await;

    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("regen").visible());
        assert!(window.find("obsidian").visible());
        assert!(window.find("rec-folder").visible());
        assert!(has_any_text(window, "退款联调纪要详查"));
        assert!(has_any_text(
            window,
            "本次会议确认周五前完成退款接口联调，预算三十七万五千元已获批。"
        ));
        assert!(has_any_text(window, "赵磊负责周五前完成退款接口联调"));
        assert!(has_any_text(window, "提交退款接口联调测试报告"));
        assert!(has_any_text(window, "下周确定灰度发布窗口"));
        assert!(has_any_text(window, "退款接口"));
        assert!(has_any_text(window, VOICE_A_TEXT));
        assert!(has_any_text(window, VOICE_B_TEXT));
        window.click("obsidian", cx);
    })
    .expect("verify full Library detail and click Open in Obsidian");

    assert!(
        cx.opened_url()
            .is_some_and(|u| u.starts_with("obsidian://open?"))
    );

    // Click cluster label ("seg-spk", 0usize) and assign existing person "王强".
    scroll_right_pane_down_in_library(handle, cx);
    cx.update_window(handle, |_, window, cx| {
        window.click(("seg-spk", 0usize), cx);
        window.render_frame(cx);
        window.click(("assign-person", 0usize), cx);
    })
    .expect("rename speaker in Library transcript");

    cx.wait_for(handle, Duration::from_secs(2), |window, _| {
        has_any_text(window, "王强")
    })
    .await;

    cx.read(|cx| {
        let store = state.read(cx).store.clone();
        let s = store.lock().expect("lock store");
        let names = s.cluster_names(meeting_id).expect("cluster_names");
        assert!(
            !names.is_empty(),
            "speaker assignment did not persist in DB"
        );
    });

    // Search Library for a matching keyword ("三十七万五千元"), click hit, verify detail & seek banner.
    cx.update_window(handle, |_, window, cx| {
        window.click("lib-search", cx);
        window.input("三十七万五千元", cx);
        window.press("enter", cx);
    })
    .expect("search Library");

    cx.wait_for(handle, Duration::from_secs(3), |window, _| {
        window
            .try_find(("hit", 0usize))
            .is_some_and(|s| s.visible())
    })
    .await;

    cx.update_window(handle, |_, window, cx| {
        window.click(("hit", 0usize), cx);
    })
    .expect("click search hit");

    cx.wait_for(handle, Duration::from_secs(2), |window, _| {
        window.try_find("regen").is_some()
    })
    .await;

    // Search for an unmatched keyword -> empty search message.
    cx.update_window(handle, |_, window, cx| {
        window.click("lib-search", cx);
        window.press("cmd-a", cx);
        window.input("不存在的关键词XYZ", cx);
        window.press("enter", cx);
    })
    .expect("search unmatched keyword");

    cx.wait_for(handle, Duration::from_secs(3), |window, _| {
        has_any_text(window, "没有找到“不存在的关键词XYZ”。")
    })
    .await;
}

/// Scrolls the Library detail pane down so transcript rows are in the viewport if needed.
fn scroll_right_pane_down_in_library(handle: AnyWindowHandle, cx: &mut TestAppContext) {
    let _ = cx.update_window(handle, |_, window, cx| {
        scroll_right_pane_to_id(window, ("seg-spk", 0usize), cx);
    });
}

#[gpui_kit::test]
async fn gui_people_crud_voiceprints_and_validation(cx: &mut TestAppContext) {
    let _guard = lock_gui();
    let (tmp, _, _) = create_test_env(serde_json::json!({}));
    let (handle, state, _) = open_workbench(cx, tmp.path());

    // Open Speakers ("0-0-4") -> verify empty state, then add "赵磊".
    cx.update_window(handle, |_, window, cx| {
        window.click("0-0-4", cx);
        window.render_frame(cx);
        assert!(has_any_text(window, "还没有人物。先在上面添加。"));
        window.click("person-name", cx);
        window.input("赵磊", cx);
        window.click("add-person", cx);
    })
    .expect("add person 赵磊");

    cx.wait_for(handle, Duration::from_secs(2), |window, _| {
        window
            .try_find(("person", 0usize))
            .is_some_and(|s| s.visible())
    })
    .await;

    cx.update_window(handle, |_, window, cx| {
        window.click(("person", 0usize), cx);
        window.render_frame(cx);
        assert!(window.find("enroll").visible());
        assert!(window.find("clip").visible());
        assert!(window.find("del-person").visible());
        assert!(has_any_text(window, "赵磊"));
        assert!(has_any_text(window, "声纹样本（0）"));

        // Submitting clip with empty path shows warning safely without crashing.
        window.click("clip", cx);
    })
    .expect("select person and test empty clip validation");

    // Seed a stored Voiceprint for 赵磊, refresh, verify sample renders, then delete voiceprint & person.
    cx.update(|cx| {
        let store = state.read(cx).store.clone();
        let pid = {
            let s = store.lock().expect("lock store");
            s.person_by_name("赵磊")
                .expect("query person")
                .expect("赵磊 stored")
        };
        let vp = Voiceprint {
            id: VoiceprintId::new(),
            person_id: pid,
            model: "wespeaker-resnet34".into(),
            vector: vec![0.1, 0.2, 0.3],
            source: VoiceprintSource::Clip {
                asset_path: "session/mic.caf".into(),
                start_ms: 1_000,
                end_ms: 4_000,
            },
            speech_ms: 3_000,
        };
        store
            .lock()
            .expect("lock store")
            .add_voiceprint(&vp)
            .expect("add voiceprint");
        state.update(cx, |s, cx| s.refresh_people(cx));
    });

    cx.wait_for(handle, Duration::from_secs(2), |window, _| {
        window
            .try_find(("del-vp", 0usize))
            .is_some_and(|s| s.visible())
    })
    .await;

    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(has_any_text(window, "声纹样本（1）"));
        assert!(has_any_text(window, "录音片段 session/mic.caf 00:01–00:04"));
        window.click(("del-vp", 0usize), cx);
    })
    .expect("delete voiceprint sample");

    cx.wait_for(handle, Duration::from_secs(2), |window, _| {
        has_any_text(window, "声纹样本（0）")
    })
    .await;

    cx.update_window(handle, |_, window, cx| {
        scroll_right_pane_to_id(window, "del-person", cx);
        window.click("del-person", cx);
    })
    .expect("delete person");

    cx.wait_for(handle, Duration::from_secs(2), |window, _| {
        has_any_text(window, "还没有人物。先在上面添加。")
    })
    .await;
    cx.read(|cx| {
        let store = state.read(cx).store.clone();
        let s = store.lock().expect("lock store");
        assert!(s.people().expect("people").is_empty());
    });
}

#[gpui_kit::test]
async fn gui_settings_migration_antigravity_removed_codex_unified_and_status_hover(
    cx: &mut TestAppContext,
) {
    let _guard = lock_gui();
    let (tmp, _, _) = create_test_env(serde_json::json!({
        "refine_backend": "antigravity",
        "minutes_backend": "antigravity",
        "antigravity_model": "gemini-3.8-flash-medium",
        "antigravity_minutes_model": "claude-opus-5-5-high"
    }));
    let (handle, state, _) = open_workbench(cx, tmp.path());

    // Simulate a signed-in Codex CLI cleanly without spawning external subprocesses.
    cx.update(|cx| {
        state.update(cx, |s, cx| {
            s.set_codex_status_for_test(
                CodexStatus {
                    path: Some(PathBuf::from("/usr/local/bin/codex")),
                    signed_in: true,
                    label: Some("user@example.com".into()),
                    error: None,
                },
                cx,
            );
        });
    });
    cx.run_until_parked();

    // Navigate to Settings via the sidebar item ("0-0-5").
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("0-0-5", cx);
        window.render_frame(cx);
        assert_no_antigravity_in_ui(window);
    })
    .expect("open Settings");

    // Verify legacy migration in state and segmented options before switching to Codex.
    cx.read(|cx| {
        let cfg = &state.read(cx).cfg;
        assert_eq!(cfg.refine_backend, RefineBackend::Local);
        assert_eq!(cfg.minutes_backend, MinutesBackend::Auto);
    });

    cx.update_window(handle, |_, window, cx| {
        scroll_right_pane_to_id(window, ("refine-backend", 1usize), cx);
        assert_eq!(
            window.find(("refine-backend", 0usize)).label(),
            Some("本地 Qwen3-ASR")
        );
        assert_eq!(
            window.find(("refine-backend", 1usize)).label(),
            Some("Codex 一体化")
        );
        assert_eq!(
            window.find(("refine-backend", 2usize)).label(),
            Some("OpenAI 转写")
        );
        assert_eq!(
            window.find(("refine-backend", 3usize)).label(),
            Some("Google Gemini")
        );
        assert!(window.try_find(("refine-backend", 4usize)).is_none());
        assert!(window.try_find("codex-refine-refresh").is_none());

        scroll_right_pane_to_id(window, ("min-backend", 2usize), cx);
        assert_eq!(window.find(("min-backend", 0usize)).label(), Some("自动"));
        assert_eq!(
            window.find(("min-backend", 1usize)).label(),
            Some("API 密钥")
        );
        assert_eq!(window.find(("min-backend", 2usize)).label(), Some("Codex"));
        assert!(window.try_find(("min-backend", 3usize)).is_none());
        assert_no_antigravity_in_ui(window);
    })
    .expect("verify migrated options");

    // Scroll back up to "Codex 一体化", switch refine backend, and verify persisted config + helper UI.
    cx.update_window(handle, |_, window, cx| {
        scroll_right_pane_to_id(window, ("refine-backend", 1usize), cx);
        window.click(("refine-backend", 1usize), cx);
        window.render_frame(cx);
        scroll_right_pane_to_id(window, "codex-refine-refresh", cx);
        assert_eq!(
            window.find("codex-refine-refresh").label(),
            Some("重新检测")
        );
        scroll_right_pane_to_text(window, "Codex 精转模型", cx);
        assert!(has_any_text(window, "Codex 已登录"));
        scroll_right_pane_to_text(
            window,
            "已开启 Codex 一体化精转：会后将在精转写阶段一并生成纪要；这里的设置用于手动重新生成纪要或回退。",
            cx,
        );
        assert_no_antigravity_in_ui(window);
    })
    .expect("switch to Codex unified refine");

    cx.run_until_parked();
    cx.read(|cx| {
        assert_eq!(state.read(cx).cfg.refine_backend, RefineBackend::Codex);
    });
    let saved_raw =
        std::fs::read_to_string(tmp.path().join("config.json")).expect("read saved config");
    let saved_json: serde_json::Value = serde_json::from_str(&saved_raw).expect("parse json");
    assert_eq!(saved_json["refine_backend"], "codex");
    assert!(!saved_raw.to_lowercase().contains("antigravity"));

    // Hover the bottom-left status dots and wait for the HoverCard popover to open.
    cx.update_window(handle, |_, window, cx| {
        window.hover("status-lights", cx);
    })
    .expect("hover status lights");

    cx.wait_for(handle, Duration::from_millis(500), |window, _| {
        window
            .try_find("status-card-body")
            .is_some_and(|snapshot| snapshot.visible())
    })
    .await;

    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            has_visible_text(window, "会后由 Codex 一体化路线在精转写时一并生成纪要"),
            "status card did not show unified Codex detail"
        );
        assert_no_antigravity_in_ui(window);
    })
    .expect("verify status hover card");
}

#[gpui_kit::test]
async fn gui_settings_all_controls_switches_byok_folders_and_i18n_switch(cx: &mut TestAppContext) {
    let _guard = lock_gui();
    let (tmp, _, _) = create_test_env(serde_json::json!({}));
    let (handle, state, _) = open_workbench(cx, tmp.path());

    // Open Settings and test missing permissions banner + System Settings deep link.
    cx.update(|cx| {
        state.update(cx, |s, cx| {
            s.set_permissions_for_test(
                Loadable::Ready(PermissionStatus {
                    screen_capture: "denied".into(),
                    microphone: "authorized".into(),
                    calendar: "denied".into(),
                }),
                cx,
            );
        });
    });
    cx.update_window(handle, |_, window, cx| {
        window.click("0-0-5", cx);
        window.render_frame(cx);
        scroll_right_pane_to_id(window, "perm-settings", cx);
        assert!(window.find("perm-screenCapture").visible());
        assert!(window.find("perm-calendar").visible());
        window.click("perm-settings", cx);
    })
    .expect("verify missing permission controls");
    assert_eq!(
        cx.opened_url().as_deref(),
        Some("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
    );

    // Edit Obsidian notes_folder input and confirm on Enter.
    cx.update_window(handle, |_, window, cx| {
        scroll_right_pane_to_id(window, "settings-folder", cx);
        window.click("settings-folder", cx);
        window.press("cmd-a", cx);
        window.input("WeeklyNotes", cx);
        window.press("enter", cx);
    })
    .expect("update notes_folder");

    // Toggle all 5 capture/refine switches.
    for sw in ["s-screen", "s-sys", "s-float", "s-flip", "s-refine"] {
        cx.update_window(handle, |_, window, cx| {
            scroll_right_pane_to_id(window, sw, cx);
            window.click(sw, cx);
        })
        .expect("toggle settings switch");
    }

    // Toggle live ASR backends (OpenAI -> Gemini -> Local), check billing sync button & spend reset.
    cx.update(|cx| {
        let store = state.read(cx).store.clone();
        store
            .lock()
            .expect("lock store")
            .record_asr_usage(&AsrUsageRecord {
                recorded_at: Utc::now(),
                meeting_id: None,
                provider: "openai".into(),
                mode: "live".into(),
                model: "gpt-4o-transcribe".into(),
                audio_ms: 60_000,
                input_tokens: 100,
                output_tokens: 40,
                cost_usd: 0.006,
            })
            .expect("seed asr usage");
        state.update(cx, |s, cx| s.refresh_asr_usage(cx));
    });
    cx.run_until_parked();

    cx.update_window(handle, |_, window, cx| {
        scroll_right_pane_to_id(window, ("live-asr-backend", 1usize), cx);
        window.click(("live-asr-backend", 1usize), cx);
    })
    .expect("pick OpenAI live ASR");
    cx.read(|cx| {
        assert_eq!(state.read(cx).cfg.live_asr_backend, LiveAsrBackend::OpenAi);
    });

    cx.update_window(handle, |_, window, cx| {
        scroll_right_pane_to_id(window, ("live-asr-backend", 2usize), cx);
        window.click(("live-asr-backend", 2usize), cx);
    })
    .expect("pick Gemini live ASR");
    cx.read(|cx| {
        assert_eq!(state.read(cx).cfg.live_asr_backend, LiveAsrBackend::Gemini);
    });

    cx.update_window(handle, |_, window, cx| {
        scroll_right_pane_to_id(window, ("live-asr-backend", 0usize), cx);
        window.click(("live-asr-backend", 0usize), cx);

        scroll_right_pane_to_id(window, "sync-openai-costs", cx);
        assert!(window.find("sync-openai-costs").visible());

        scroll_right_pane_to_id(window, "spend-openai-reset", cx);
        assert!(window.find("spend-gemini-reset").visible());
        window.click("spend-openai-reset", cx);
    })
    .expect("exercise cloud live ASR controls and reset OpenAI spend ledger");

    cx.run_until_parked();
    cx.read(|cx| {
        assert_eq!(state.read(cx).cfg.live_asr_backend, LiveAsrBackend::Local);
        assert_eq!(state.read(cx).openai_usage.total.calls, 0);
    });

    // Switch caption language & minutes language, then exercise BYOK provider buttons.
    cx.update_window(handle, |_, window, cx| {
        scroll_right_pane_to_id(window, ("cap-lang", 1usize), cx);
        window.click(("cap-lang", 1usize), cx);

        scroll_right_pane_to_id(window, ("min-lang", 1usize), cx);
        window.click(("min-lang", 1usize), cx);

        scroll_right_pane_to_id(window, ("min-backend", 1usize), cx);
        window.click(("min-backend", 1usize), cx);
        window.render_frame(cx);

        // Index 0 is "None", index 1 is "OpenAI".
        scroll_right_pane_to_id(window, ("byok", 1usize), cx);
        window.click(("byok", 1usize), cx);
        window.render_frame(cx);
        scroll_right_pane_to_id(window, "settings-byok-model", cx);
        window.click("settings-byok-model", cx);
        window.press("cmd-a", cx);
        window.input("gpt-5", cx);
        window.press("enter", cx);
        scroll_right_pane_to_id(window, "key-save", cx);
        assert!(window.find("key-del").visible());
    })
    .expect("exercise language and BYOK controls");

    // Flush earlier background config saves before switching minutes_backend to Codex.
    cx.run_until_parked();

    cx.update_window(handle, |_, window, cx| {
        // Switch to Codex explicit minutes backend, verify Codex model section renders.
        scroll_right_pane_to_id(window, ("min-backend", 2usize), cx);
        window.click(("min-backend", 2usize), cx);
        window.render_frame(cx);
        scroll_right_pane_to_id(window, "codex-refresh", cx);
        assert!(window.find("codex-refresh").visible());
    })
    .expect("switch minutes backend to Codex");

    cx.run_until_parked();
    cx.read(|cx| {
        let cfg = &state.read(cx).cfg;
        assert_eq!(cfg.meetings_folder, "WeeklyNotes");
        assert!(cfg.record_screen);
        assert!(cfg.system_audio);
        assert!(!cfg.floating_captions);
        assert!(cfg.spatial_flip);
        assert!(!cfg.refine_after_meeting);
        assert_eq!(cfg.minutes_backend, MinutesBackend::Codex);
        assert!(matches!(
            cfg.byok,
            Some(ByokProvider::OpenAi { ref model }) if model == "gpt-5"
        ));
    });
    let saved = read_saved_config(tmp.path());
    assert_eq!(saved["meetings_folder"], "WeeklyNotes");
    assert_eq!(saved["record_screen"], true);
    assert_eq!(saved["system_audio"], true);
    assert_eq!(saved["floating_captions"], false);
    assert_eq!(saved["spatial_flip"], true);
    assert_eq!(saved["refine_after_meeting"], false);
    assert_eq!(saved["minutes_backend"], "codex");

    // Verify local models & data directory section at bottom.
    cx.update_window(handle, |_, window, cx| {
        scroll_right_pane_to_id(window, "engine-restart", cx);
        assert!(window.find("stats-refresh").visible());
        assert!(has_any_text(window, &tmp.path().display().to_string()));
    })
    .expect("verify bottom settings section");

    // Scroll back up to "ui-lang" and switch UI language Zh -> En -> Zh live.
    cx.update_window(handle, |_, window, cx| {
        scroll_right_pane_to_id(window, ("ui-lang", 2usize), cx);
        window.click(("ui-lang", 2usize), cx);
        window.render_frame(cx);
        assert!(has_any_text(window, "Settings"));
        assert!(has_any_text(window, "Recording and captions"));
        assert_eq!(window.find("quick-record").label(), Some("Start recording"));
    })
    .expect("switch UI language to English");

    cx.run_until_parked();
    assert_eq!(read_saved_config(tmp.path())["ui_language"], "en");

    cx.update_window(handle, |_, window, cx| {
        scroll_right_pane_to_id(window, ("ui-lang", 1usize), cx);
        window.click(("ui-lang", 1usize), cx);
        window.render_frame(cx);
        assert!(has_any_text(window, "设置"));
        assert_eq!(window.find("quick-record").label(), Some("开始记录"));
    })
    .expect("switch UI language back to Chinese");

    cx.run_until_parked();
    assert_eq!(read_saved_config(tmp.path())["ui_language"], "zh");
}

/// Live Codex GUI E2E test: synthesized two-speaker audio -> `finish_meeting` (Codex unified) ->
/// SQLite + Obsidian Markdown -> real GPUI `Workbench` `LibraryPage` verification.
#[gpui_kit::test]
#[ignore = "calls local codex CLI on synthesized two-speaker Chinese audio"]
async fn gui_live_codex_unified_audio_to_library_e2e(cx: &mut TestAppContext) {
    let _guard = lock_gui();
    let codex = detect_codex();
    assert!(
        codex.signed_in,
        "local Codex CLI must be signed in for live E2E: {codex:?}"
    );

    let (tmp, vault, rec_root) = create_test_env(serde_json::json!({}));
    let rec_dir = rec_root.join("session-codex");
    std::fs::create_dir_all(&rec_dir).expect("session rec dir");
    let mic_wav = synthesize_two_speaker_wav(&rec_dir);

    let cfg = mm_service::AppConfig {
        vault_path: Some(vault),
        recordings_dir: Some(rec_root),
        refine_after_meeting: true,
        refine_backend: RefineBackend::Codex,
        auto_minutes: true,
        minutes_backend: MinutesBackend::Auto,
        auto_download_embedding: false,
        record_screen: false,
        system_audio: false,
        ui_language: Some(mm_i18n::Lang::Zh),
        ..Default::default()
    };
    cfg.save(tmp.path()).expect("save test config");

    let db_path = tmp.path().join("meeting-master.db");
    let store = Arc::new(Mutex::new(
        mm_store::Store::open(&db_path).expect("open store"),
    ));
    let (indexer, _ix) = mm_service::indexer::Indexer::start(store.clone(), cfg.vault_path.clone());
    let (writer, _we) = LiveWriter::start(store.clone(), indexer);

    let (mut meeting, started) = {
        let s = store.lock().expect("lock store");
        let mut m = flow::quick_meeting(&s, "Codex 一体化端到端联调").expect("quick meeting");
        flow::ensure_note(&cfg, &s, &mut m, &["赵磊".into()]).expect("ensure note");
        let started = CaptureStarted {
            session_id: "session-codex".into(),
            directory: rec_dir,
            files: CaptureFiles {
                screen: None,
                microphone: Some(mic_wav),
                system_audio: None,
            },
            microphone_channels: Some(1),
            started_at: Utc::now(),
        };
        flow::mark_live(&s, &mut m, &started).expect("mark live");
        (m, started)
    };

    let job = FinishJob {
        minutes: MinutesCtx {
            cfg: cfg.clone(),
            store: store.clone(),
            codex: Some(codex),
            embed: None,
        },
        engine: None,
        writer,
        meeting: meeting.clone(),
        started,
        elapsed_ms: 8_000,
    };

    let report = finish_meeting(job, &|step| println!("codex-gui-e2e step: {step}"))
        .expect("finish_meeting");
    meeting = report.meeting.clone();
    println!("warnings: {:?}", report.warnings);
    assert!(
        report.minutes_done,
        "minutes not done: {:?}",
        report.warnings
    );
    assert_eq!(meeting.status, MeetingStatus::Done);

    let (minutes_summary, segments) = {
        let s = store.lock().expect("lock store");
        let min_val = s
            .minutes(meeting.id)
            .expect("read minutes")
            .expect("minutes stored");
        let min: Minutes = serde_json::from_value(min_val).expect("parse stored minutes");
        let segs = s.transcript(meeting.id).expect("read transcript");
        println!("Codex summary: {}", min.summary);
        println!("Codex segments: {}", segs.len());
        (min.summary, segs)
    };
    assert!(
        segments.len() >= 2,
        "expected >= 2 speaker segments, got {}",
        segments.len()
    );
    assert!(!minutes_summary.trim().is_empty());

    let note_file = flow::note_path(&cfg, &meeting).expect("note path");
    let note_md = std::fs::read_to_string(&note_file).expect("read note markdown");
    assert!(note_md.contains("### 摘要"), "note missing summary heading");
    assert!(
        note_md.contains("<!-- mm:transcript:start -->"),
        "note missing transcript block"
    );

    drop(store);

    let (handle, _, _) = open_workbench(cx, tmp.path());
    cx.update_window(handle, |_, window, cx| {
        window.click("0-0-3", cx);
    })
    .expect("open Library");

    cx.wait_for(handle, Duration::from_secs(2), |window, _| {
        window
            .try_find(("meeting", 0usize))
            .is_some_and(|snapshot| snapshot.visible())
    })
    .await;

    cx.update_window(handle, |_, window, cx| {
        window.click(("meeting", 0usize), cx);
    })
    .expect("click Codex meeting");

    cx.wait_for(handle, Duration::from_secs(2), |window, _| {
        window.try_find("regen").is_some()
    })
    .await;

    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(has_any_text(window, &meeting.title));
        assert!(has_any_text(window, &minutes_summary));
        assert!(has_any_text(window, &segments[0].text));
        assert!(has_any_text(window, &segments[1].text));
    })
    .expect("verify live Codex output inside LibraryPage");
}

#[gpui_kit::test]
async fn gui_settings_cloud_asr_chips_inputs_and_live_usage_ledger(cx: &mut TestAppContext) {
    let _guard = lock_gui();
    let (tmp, _, rec_dir) = create_test_env(serde_json::json!({}));
    let (handle, state, _) = open_workbench(cx, tmp.path());

    // Open Settings and verify no hardcoded model chips exist without keys,
    // custom inputs still work, and refresh buttons are disabled until keys exist.
    cx.update_window(handle, |_, window, cx| {
        window.click("0-0-5", cx);
        window.render_frame(cx);

        scroll_right_pane_to_id(window, ("live-asr-backend", 1usize), cx);
        window.click(("live-asr-backend", 1usize), cx);
        window.render_frame(cx);
        scroll_right_pane_to_text(window, "尚未保存 OpenAI API 密钥", cx);
        assert!(window.try_find("live-openai-select").is_some());
        assert_eq!(window.find("live-openai-refresh").disabled(), Some(true));

        scroll_right_pane_to_id(window, "live-openai-input", cx);
        window.click("live-openai-input", cx);
        window.press("cmd-a", cx);
        window.input("gpt-4o-mini-transcribe", cx);
        window.press("enter", cx);
    })
    .expect("verify OpenAI live missing-key UI, disabled refresh, and custom input");
    cx.run_until_parked();
    cx.read(|cx| {
        assert_eq!(
            state.read(cx).cfg.live_openai_model.as_deref(),
            Some("gpt-4o-mini-transcribe")
        );
    });

    cx.update_window(handle, |_, window, cx| {
        scroll_right_pane_to_id(window, ("live-asr-backend", 2usize), cx);
        window.click(("live-asr-backend", 2usize), cx);
        window.render_frame(cx);
        scroll_right_pane_to_text(window, "尚未保存 Gemini API 密钥", cx);
        assert_eq!(window.find("live-gemini-refresh").disabled(), Some(true));

        scroll_right_pane_to_id(window, ("refine-backend", 2usize), cx);
        window.click(("refine-backend", 2usize), cx);
        window.render_frame(cx);
        assert_eq!(window.find("refine-openai-refresh").disabled(), Some(true));

        scroll_right_pane_to_id(window, ("refine-backend", 3usize), cx);
        window.click(("refine-backend", 3usize), cx);
        window.render_frame(cx);
        assert_eq!(window.find("refine-gemini-refresh").disabled(), Some(true));
    })
    .expect("verify Gemini live and OpenAI/Gemini refine missing-key states");

    // Simulate Loading state for OpenAI while key is present.
    cx.update(|cx| {
        state.update(cx, |app, cx| {
            app.set_asr_key_present_for_test(AsrProvider::OpenAi, true, cx);
            app.set_asr_models_for_test(AsrProvider::OpenAi, Loadable::Loading, cx);
        });
    });
    cx.run_until_parked();

    cx.update_window(handle, |_, window, cx| {
        scroll_right_pane_to_id(window, ("live-asr-backend", 1usize), cx);
        window.click(("live-asr-backend", 1usize), cx);
        window.render_frame(cx);
        scroll_right_pane_to_text(window, "正在从 OpenAI 拉取可用模型列表", cx);
        assert_eq!(window.find("live-openai-refresh").disabled(), Some(true));
    })
    .expect("verify Loading status when fetching OpenAI models");

    // Simulate Failed state for Gemini while key is present.
    cx.update(|cx| {
        state.update(cx, |app, cx| {
            app.set_asr_key_present_for_test(AsrProvider::Gemini, true, cx);
            app.set_asr_models_for_test(
                AsrProvider::Gemini,
                Loadable::Failed(LoadError::from("HTTP 403 Forbidden".to_string())),
                cx,
            );
        });
    });
    cx.run_until_parked();

    cx.update_window(handle, |_, window, cx| {
        scroll_right_pane_to_id(window, ("live-asr-backend", 2usize), cx);
        window.click(("live-asr-backend", 2usize), cx);
        window.render_frame(cx);
        scroll_right_pane_to_text(window, "拉取模型列表失败：HTTP 403 Forbidden", cx);
        assert_ne!(window.find("live-gemini-refresh").disabled(), Some(true));
    })
    .expect("verify Failed status for Gemini models");

    // Simulate Ready dynamically fetched OpenAI & Gemini model lists with novel future model IDs.
    cx.update(|cx| {
        state.update(cx, |app, cx| {
            app.set_asr_models_for_test(
                AsrProvider::OpenAi,
                Loadable::Ready(AsrModelList {
                    live: vec![
                        ModelOption {
                            id: "gpt-live-transcribe".into(),
                            label: "gpt-live-transcribe".into(),
                            is_default: true,
                        },
                        ModelOption {
                            id: "gpt-5-live-transcribe".into(),
                            label: "gpt-5-live-transcribe".into(),
                            is_default: false,
                        },
                    ],
                    refine: vec![
                        ModelOption {
                            id: "gpt-transcribe".into(),
                            label: "gpt-transcribe".into(),
                            is_default: true,
                        },
                        ModelOption {
                            id: "gpt-5-transcribe".into(),
                            label: "gpt-5-transcribe".into(),
                            is_default: false,
                        },
                    ],
                }),
                cx,
            );
            app.set_asr_models_for_test(
                AsrProvider::Gemini,
                Loadable::Ready(AsrModelList {
                    live: vec![
                        ModelOption {
                            id: "gemini-3.5-transcribe-live".into(),
                            label: "gemini-3.5-transcribe-live".into(),
                            is_default: true,
                        },
                        ModelOption {
                            id: "gemini-4.0-transcribe-live".into(),
                            label: "gemini-4.0-transcribe-live".into(),
                            is_default: false,
                        },
                    ],
                    refine: vec![
                        ModelOption {
                            id: "gemini-3.5-transcribe".into(),
                            label: "gemini-3.5-transcribe".into(),
                            is_default: true,
                        },
                        ModelOption {
                            id: "gemini-4.0-transcribe".into(),
                            label: "gemini-4.0-transcribe".into(),
                            is_default: false,
                        },
                    ],
                }),
                cx,
            );
        });
    });
    cx.run_until_parked();

    cx.update_window(handle, |_, window, cx| {
        scroll_right_pane_to_id(window, ("live-asr-backend", 1usize), cx);
        window.click(("live-asr-backend", 1usize), cx);
        window.render_frame(cx);
        assert!(!has_any_text(window, "尚未保存 OpenAI API 密钥"));
        scroll_right_pane_to_text(window, "已从 OpenAI 拉取 2 个可用模型", cx);
        assert_ne!(window.find("live-openai-refresh").disabled(), Some(true));

        scroll_right_pane_to_id(window, "live-openai-input", cx);
        window.click("live-openai-input", cx);
        window.press("cmd-a", cx);
        window.input("gpt-5-live-transcribe", cx);
        window.press("enter", cx);
    })
    .expect("verify dynamically fetched OpenAI live status and enter novel live model");
    cx.run_until_parked();
    cx.read(|cx| {
        assert_eq!(
            state.read(cx).cfg.live_openai_model.as_deref(),
            Some("gpt-5-live-transcribe")
        );
    });

    cx.update_window(handle, |_, window, cx| {
        scroll_right_pane_to_text(window, "未收录该模型单价", cx);

        scroll_right_pane_to_id(window, ("refine-backend", 2usize), cx);
        window.click(("refine-backend", 2usize), cx);
        window.render_frame(cx);
        scroll_right_pane_to_id(window, "refine-openai-input", cx);
        window.click("refine-openai-input", cx);
        window.press("cmd-a", cx);
        window.input("gpt-5-transcribe", cx);
        window.press("enter", cx);

        scroll_right_pane_to_id(window, ("refine-backend", 3usize), cx);
        window.click(("refine-backend", 3usize), cx);
        window.render_frame(cx);
        assert!(!has_any_text(window, "尚未保存 Gemini API 密钥"));
        scroll_right_pane_to_text(window, "已从 Google Gemini 拉取 2 个可用模型", cx);
        scroll_right_pane_to_id(window, "refine-gemini-input", cx);
        window.click("refine-gemini-input", cx);
        window.press("cmd-a", cx);
        window.input("gemini-4.0-transcribe", cx);
        window.press("enter", cx);
    })
    .expect("verify novel OpenAI/Gemini refine models and dynamic status");
    cx.run_until_parked();
    cx.read(|cx| {
        let cfg = &state.read(cx).cfg;
        assert_eq!(cfg.refine_openai_model.as_deref(), Some("gpt-5-transcribe"));
        assert_eq!(
            cfg.refine_gemini_model.as_deref(),
            Some("gemini-4.0-transcribe")
        );
    });

    // Inject live CaptionUsage notices for both OpenAI & Gemini during a live session, flush to SQLite.
    cx.update(|cx| {
        let store = state.read(cx).store.clone();
        let cfg = state.read(cx).cfg.clone();
        let session_dir = rec_dir.join("usage-sess");
        std::fs::create_dir_all(&session_dir).expect("session dir");
        let s = store.lock().expect("lock store");
        let mut m = flow::quick_meeting(&s, "云端计费账本测试").expect("meeting");
        let note = flow::ensure_note(&cfg, &s, &mut m, &[]).expect("note");
        let started = CaptureStarted {
            session_id: "sess-usage".into(),
            directory: session_dir,
            files: CaptureFiles::default(),
            microphone_channels: Some(1),
            started_at: Utc::now(),
        };
        flow::mark_live(&s, &mut m, &started).expect("mark live");
        drop(s);

        state.update(cx, |app, cx| {
            app.start_mock_live_for_test(m, started, note, String::new(), cx);
            app.inject_notice_for_test(
                MeetingNotice::Usage(CaptionUsage {
                    session_id: "sess-usage".into(),
                    provider: "openai".into(),
                    model: "gpt-4o-transcribe".into(),
                    origin: AudioOrigin::Microphone,
                    audio_ms: 30_000,
                    input_tokens: 150,
                    output_tokens: 35,
                    mode: None,
                }),
                cx,
            );
            app.inject_notice_for_test(
                MeetingNotice::Usage(CaptionUsage {
                    session_id: "sess-usage".into(),
                    provider: "gemini".into(),
                    model: "gemini-3.5-transcribe-live".into(),
                    origin: AudioOrigin::System,
                    audio_ms: 45_000,
                    input_tokens: 200,
                    output_tokens: 50,
                    mode: None,
                }),
                cx,
            );
            app.writer_flush_for_test();
            app.end_mock_live_for_test(cx);
            app.refresh_asr_usage(cx);
        });
    });
    cx.run_until_parked();

    cx.update_window(handle, |_, window, cx| {
        scroll_right_pane_to_id(window, "spend-openai-reset", cx);
        scroll_right_pane_to_id(window, "spend-gemini-reset", cx);
        assert!(has_any_text(window, "OpenAI"));
        assert!(has_any_text(window, "Google Gemini"));
        assert!(has_any_text(window, "1 次"));
        window.click("spend-openai-reset", cx);
        window.click("spend-gemini-reset", cx);
    })
    .expect("verify both OpenAI and Gemini spend rows and reset them");

    cx.run_until_parked();
    cx.read(|cx| {
        let app = state.read(cx);
        assert_eq!(app.openai_usage.total.calls, 0);
        assert_eq!(app.gemini_usage.total.calls, 0);
    });
}

#[gpui_kit::test]
async fn gui_live_capture_failure_retraction_and_stop_warning(cx: &mut TestAppContext) {
    let _guard = lock_gui();
    let (tmp, _, rec_dir) = create_test_env(serde_json::json!({
        "refine_after_meeting": false,
        "auto_minutes": false
    }));
    let (handle, state, _) = open_workbench(cx, tmp.path());

    cx.update(|cx| {
        let store = state.read(cx).store.clone();
        let cfg = state.read(cx).cfg.clone();
        let session_dir = rec_dir.join("err-sess");
        std::fs::create_dir_all(&session_dir).expect("session dir");
        let s = store.lock().expect("lock store");
        let mut m = flow::quick_meeting(&s, "故障与撤回测试").expect("meeting");
        let note = flow::ensure_note(&cfg, &s, &mut m, &[]).expect("note");
        let started = CaptureStarted {
            session_id: "sess-err".into(),
            directory: session_dir,
            files: CaptureFiles::default(),
            microphone_channels: Some(1),
            started_at: Utc::now(),
        };
        flow::mark_live(&s, &mut m, &started).expect("mark live");
        drop(s);

        state.update(cx, |app, cx| {
            app.start_mock_live_for_test(m, started, note, String::new(), cx);
            app.inject_notice_for_test(
                MeetingNotice::Partial(CaptionPartial {
                    session_id: "sess-err".into(),
                    utterance_id: 1,
                    origin: AudioOrigin::Microphone,
                    start_ms: 0,
                    text: "嗯...".into(),
                    speaker: SpeakerLabel::Unknown,
                    stable_chars: 0,
                }),
                cx,
            );
        });
    });
    cx.run_until_parked();

    // Navigate to Live page and confirm partial is shown, then retract with empty final.
    cx.update_window(handle, |_, window, cx| {
        window.click("0-0-2", cx);
        window.render_frame(cx);
        assert!(has_any_text(window, "嗯..."));
    })
    .expect("verify volatile partial before retraction");

    cx.update(|cx| {
        state.update(cx, |app, cx| {
            app.inject_notice_for_test(
                MeetingNotice::Final(CaptionFinal {
                    session_id: "sess-err".into(),
                    utterance_id: 1,
                    origin: AudioOrigin::Microphone,
                    start_ms: 0,
                    end_ms: 400,
                    text: "".into(),
                    language: None,
                    speaker: SpeakerLabel::Unknown,
                    spatial: None,
                }),
                cx,
            );
            app.inject_notice_for_test(
                MeetingNotice::State(CaptureStateEvent {
                    session_id: "sess-err".into(),
                    state: CaptureState::Failed,
                    error: Some("audio device disconnected".into()),
                }),
                cx,
            );
        });
    });
    cx.run_until_parked();

    cx.read(|cx| {
        let live = state
            .read(cx)
            .live
            .as_ref()
            .expect("live session still active");
        assert_eq!(live.capture_state, CaptureState::Failed);
        assert_eq!(
            live.capture_error.as_deref(),
            Some("audio device disconnected")
        );
    });

    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            !has_any_text(window, "嗯..."),
            "retracted partial still painted"
        );
        window.click("live-stop", cx);
    })
    .expect("verify retraction, failure state, and click live-stop");

    cx.read(|cx| {
        assert!(state.read(cx).live.is_none());
    });

    cx.wait_for(handle, Duration::from_secs(2), |window, _| {
        has_any_text(window, "字幕引擎不在运行")
    })
    .await;

    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(has_any_text(window, "停止录制时出错（字幕引擎不在运行）"));
        assert!(window.find(("post-open", 0usize)).visible());
    })
    .expect("verify stop warning rendered on post-meeting card");
}

/// Remaps an engine notification from `sim-*` session id onto the live UI `session_id`
/// and offsets `utterance_id` so multiple simulated streams never collide.
fn remap_notice_for_live(
    mut notice: MeetingNotice,
    target_session: &str,
    utterance_offset: u64,
) -> MeetingNotice {
    match &mut notice {
        MeetingNotice::Partial(p) => {
            p.session_id = target_session.to_string();
            p.utterance_id += utterance_offset;
        }
        MeetingNotice::Final(f) => {
            f.session_id = target_session.to_string();
            f.utterance_id += utterance_offset;
        }
        MeetingNotice::Relabel(r) => {
            r.session_id = target_session.to_string();
        }
        MeetingNotice::Spatial(s) => {
            s.session_id = target_session.to_string();
            s.utterance_id += utterance_offset;
        }
        MeetingNotice::Usage(u) => {
            u.session_id = target_session.to_string();
        }
        MeetingNotice::Levels(l) => {
            l.session_id = target_session.to_string();
        }
        MeetingNotice::State(st) => {
            st.session_id = target_session.to_string();
        }
        MeetingNotice::Other { .. } => {}
    }
    notice
}

/// Bundled GUI window/state handles for live engine stream pumping.
struct LiveGuiWindows<'a> {
    handle: AnyWindowHandle,
    float_handle: AnyWindowHandle,
    state: &'a Entity<AppState>,
    session_id: &'a str,
}

fn drain_engine_events_into_gui(
    events: &async_channel::Receiver<mm_engine::EngineEvent>,
    utterance_offset: u64,
    cx: &mut TestAppContext,
    ui: &LiveGuiWindows<'_>,
) -> (bool, usize) {
    let mut drained = false;
    let mut partials = 0_usize;
    while let Ok(ev) = events.try_recv() {
        drained = true;
        if let mm_engine::EngineEvent::Notification { method, params } = ev
            && let Ok(notice) = decode_notice(&method, params)
        {
            if matches!(notice, MeetingNotice::Partial(_)) {
                partials += 1;
            }
            let remapped = remap_notice_for_live(notice, ui.session_id, utterance_offset);
            cx.update(|cx| {
                ui.state
                    .update(cx, |app, cx| app.inject_notice_for_test(remapped, cx));
            });
        }
    }
    if drained {
        let _ = cx.update_window(ui.handle, |_, window, cx| window.render_frame(cx));
        let _ = cx.update_window(ui.float_handle, |_, window, cx| window.render_frame(cx));
        cx.run_until_parked();
    }
    (drained, partials)
}

/// Runs `mm-meetingd`'s `caption/simulateFile` on an OS thread while pumping live notifications
/// into `AppState` and rendering `Workbench` + `FloatingCaptions`.
fn pump_real_simulate_into_gui(
    engine: MeetingEngine,
    events: &async_channel::Receiver<mm_engine::EngineEvent>,
    sim_params: serde_json::Value,
    utterance_offset: u64,
    cx: &mut TestAppContext,
    ui: &LiveGuiWindows<'_>,
) -> usize {
    let sim_thread = std::thread::spawn(move || engine.simulate_file(sim_params));
    let mut partial_count = 0_usize;

    loop {
        let (_, p) = drain_engine_events_into_gui(events, utterance_offset, cx, ui);
        partial_count += p;
        if sim_thread.is_finished() {
            let (_, p) = drain_engine_events_into_gui(events, utterance_offset, cx, ui);
            partial_count += p;
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    sim_thread
        .join()
        .expect("join simulate_file thread")
        .expect("simulate_file RPC succeeded");
    partial_count
}

#[gpui_kit::test]
#[ignore = "spawns real mm-meetingd to drive live Workbench + FloatingCaptions windows with real ASR, voiceprint, spatial L/R and remote audio"]
async fn gui_real_live_captions_to_workbench_and_floating_e2e(cx: &mut TestAppContext) {
    let _guard = lock_gui();
    let engine_path = e2e_engine_binary();
    let (tmp, _, rec_dir) = create_test_env(serde_json::json!({
        "system_audio": true,
        "caption_language": "zh"
    }));
    let session_dir = rec_dir.join("real-gui-live");
    std::fs::create_dir_all(&session_dir).expect("session dir");

    // Probe real mm-meetingd models & extract real WeSpeaker voiceprint before opening GPUI window.
    let audio = synthesize_meeting_audio(&session_dir);
    let (engine, events) = MeetingEngine::spawn(&engine_path).expect("spawn mm-meetingd");
    engine.initialize().expect("initialize");
    let preload = engine.preload_models(Some("zh")).expect("preload models");
    let vp_sample = engine
        .voiceprint_from_clip(&ClipRequest {
            path: audio.speaker_a_ref.clone(),
            start_ms: None,
            end_ms: None,
        })
        .expect("voiceprint_from_clip");

    let (handle, state, _) = open_workbench(cx, tmp.path());
    let person_id = PersonId::new();
    let (note_file, session_id) = cx.update(|cx| {
        let (cfg, store) = {
            let app = state.read(cx);
            (app.cfg.clone(), app.store.clone())
        };
        let s = store.lock().expect("lock store");
        s.upsert_person(
            &Person {
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
        let mut m = flow::quick_meeting(&s, "真实实时字幕全链路 UI 测试").expect("meeting");
        let note = flow::ensure_note(&cfg, &s, &mut m, &["赵磊".into()])
            .expect("ensure note")
            .expect("note path");
        let started = CaptureStarted {
            session_id: "real-live-ui".into(),
            directory: session_dir,
            files: CaptureFiles {
                screen: None,
                microphone: Some(audio.mono_mic_wav.clone()),
                system_audio: Some(audio.remote_system_wav.clone()),
            },
            microphone_channels: Some(2),
            started_at: Utc::now(),
        };
        flow::mark_live(&s, &mut m, &started).expect("mark live");
        drop(s);

        let sid = started.session_id.clone();
        state.update(cx, |app, cx| {
            app.set_speech_models_for_test(Loadable::Ready(preload), cx);
            app.start_mock_live_for_test(m, started, Some(note.clone()), "实测初始笔记".into(), cx);
        });
        (note, sid)
    });
    cx.run_until_parked();

    // Navigate to Live page and open FloatingCaptions window.
    cx.update_window(handle, |_, window, cx| {
        window.click("0-0-2", cx);
        window.render_frame(cx);
        window.click("float", cx);
    })
    .expect("open LivePage and FloatingCaptions");

    cx.wait_for(handle, Duration::from_secs(2), |_, cx| {
        cx.windows().len() == 2
    })
    .await;
    let float_handle = cx.read(|cx| {
        cx.windows()
            .into_iter()
            .find(|w| *w != handle)
            .expect("floating window handle")
    });

    // Stream stereo microphone file with enrolled WeSpeaker voiceprint.
    let live_ui = LiveGuiWindows {
        handle,
        float_handle,
        state: &state,
        session_id: &session_id,
    };
    let preset_vp = PresetVoiceprint {
        person_id: person_id.to_string(),
        model: vp_sample.model,
        vector: vp_sample.vector,
    };
    let mic_partials = pump_real_simulate_into_gui(
        engine.clone(),
        &events,
        serde_json::json!({
            "path": audio.stereo_mic_wav,
            "origin": "microphone",
            "speed": 1.0,
            "language": "zh",
            "spatialSpacingCm": 14.0,
            "voiceprints": [preset_vp],
        }),
        0,
        cx,
        &live_ui,
    );

    // Stream remote English system-audio file on non-colliding utterance ids.
    let sys_partials = pump_real_simulate_into_gui(
        engine.clone(),
        &events,
        serde_json::json!({
            "path": audio.remote_system_wav,
            "origin": "system",
            "speed": 1.0,
            "language": "en",
        }),
        1_000,
        cx,
        &live_ui,
    );

    // Type live notes while real captions sit on stage, then flush LiveWriter & shut down engine.
    cx.update_window(handle, |_, window, cx| {
        window.click("live-notes-box", cx);
        window.input(" | 实时字幕实测记录完成", cx);
    })
    .expect("edit live notes during real caption session");

    cx.wait_for(handle, Duration::from_secs(3), |_, _| {
        std::fs::read_to_string(&note_file)
            .map(|s| s.contains("实时字幕实测记录完成"))
            .unwrap_or(false)
    })
    .await;
    cx.read(|cx| state.read(cx).writer_flush_for_test());
    engine.shutdown();

    // Verify Workbench LivePage rendered real captions, voiceprint identity, L/R spatial, and remote origin.
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let painted: Vec<String> = texts(window)
            .into_iter()
            .map(|t| t.text().to_string())
            .collect();
        let all = painted.join("\n");
        let lower = all.to_ascii_lowercase();
        println!("[GUI-LIVE-E2E] mic_partials={mic_partials}, sys_partials={sys_partials}");
        assert!(
            mic_partials > 0,
            "expected live streaming partial captions on mic"
        );
        assert!(
            all.contains("退款") || all.contains("周五") || all.contains("联调"),
            "missing U1 Chinese caption in LivePage: {all}"
        );
        assert!(
            all.contains("预算") || all.contains("财务") || all.contains("三十七"),
            "missing U2 Chinese caption in LivePage: {all}"
        );
        assert!(
            lower.contains("budget") || lower.contains("friday") || lower.contains("approved"),
            "missing remote English caption in LivePage: {all}"
        );
        assert!(
            all.contains("赵磊"),
            "missing enrolled voiceprint name 赵磊"
        );
        assert!(
            all.contains("偏左") && all.contains("偏右"),
            "missing L/R direction labels: {all}"
        );
        assert!(
            all.contains("远程"),
            "missing 远程 tag on system-audio caption"
        );
    })
    .expect("verify LivePage real captions UI");

    // Verify FloatingCaptions rendered live text and closes via "float-close".
    cx.update_window(float_handle, |_, window, cx| {
        window.render_frame(cx);
        let float_all: String = texts(window)
            .into_iter()
            .map(|t| t.text().to_string())
            .collect::<Vec<_>>()
            .join("\n")
            .to_ascii_lowercase();
        assert!(
            float_all.contains("budget")
                || float_all.contains("friday")
                || float_all.contains("预算"),
            "FloatingCaptions did not display latest real captions: {float_all}"
        );
        window.click("float-close", cx);
    })
    .expect("verify FloatingCaptions and close it");

    cx.wait_for(handle, Duration::from_secs(2), |_, cx| {
        cx.windows().len() == 1
    })
    .await;

    let (seg_count, note_md) = cx.read(|cx| {
        let app = state.read(cx);
        let s = app.store.lock().expect("lock store");
        let mid = app.live.as_ref().expect("live").meeting.id;
        let segs = s.transcript(mid).expect("transcript");
        let md = std::fs::read_to_string(&note_file).expect("read note");
        (segs.len(), md)
    });
    assert!(
        seg_count >= 3,
        "expected >= 3 persisted real segments (2 mic + 1 system), got {seg_count}"
    );
    assert!(note_md.contains("实时字幕实测记录完成"));
}

#[gpui_kit::test]
#[ignore = "spawns real mm-embedd with Qwen3-4B offline minutes + EmbeddingGemma 2 indexing and semantic search in LibraryPage UI"]
async fn gui_real_embedd_semantic_search_and_local_llm_minutes_e2e(cx: &mut TestAppContext) {
    let _guard = lock_gui();
    let embed_path = e2e_embed_binary();
    let embed_dir = find_cached_hf_model("mlx-community/embeddinggemma-2-bf16")
        .expect("cached mlx-community/embeddinggemma-2-bf16 required");
    let qwen_dir = find_cached_hf_model("mlx-community/Qwen3-4B-Instruct-2507-4bit")
        .expect("cached mlx-community/Qwen3-4B-Instruct-2507-4bit required");

    let (tmp, _, _) = create_test_env(serde_json::json!({
        "refine_after_meeting": false,
        "auto_minutes": true,
        "use_codex": false,
        "local_llm_dir": qwen_dir,
        "embedding_model_dir": embed_dir,
    }));

    // Seed distractor and target meetings & run real local Qwen3-4B minutes before launching GPUI.
    let (embed, _ee) = EmbedEngine::spawn(&embed_path).expect("spawn mm-embedd");
    let cfg = mm_service::AppConfig::load(tmp.path()).expect("load test config");
    let store = Arc::new(Mutex::new(
        mm_store::Store::open(&tmp.path().join("meeting-master.db")).expect("open db"),
    ));

    let (distractor_note, target_note, target_title, target_summary) = {
        let mut s = store.lock().expect("lock store");
        // 1. Distractor meeting (no finance/budget concepts).
        let mut dm = flow::quick_meeting(&s, "茶水间咖啡机维护").expect("distractor");
        let d_note = flow::ensure_note(&cfg, &s, &mut dm, &[])
            .expect("d note")
            .expect("d note path");
        s.set_meeting_status(dm.id, MeetingStatus::Live)
            .expect("live");
        s.replace_segments(
            dm.id,
            SegmentPass::Final,
            &[TranscriptSegment {
                meeting_id: dm.id,
                start_ms: 0,
                end_ms: 4_000,
                text: "今天安排茶水间咖啡机维护，更换咖啡豆和滤芯，周五清洗磨豆机。".into(),
                language: Some("zh".into()),
                speaker: SpeakerLabel::Cluster { index: 1 },
                origin: AudioOrigin::Microphone,
                spatial: None,
            }],
        )
        .expect("distractor seg");
        flow::sync_transcript(&cfg, &mut s, &dm).expect("sync distractor");
        s.set_meeting_status(dm.id, MeetingStatus::Processing)
            .expect("proc");
        s.set_meeting_status(dm.id, MeetingStatus::Done)
            .expect("done");

        // 2. Target meeting whose minutes will be written by local Qwen3-4B.
        let mut tm = flow::quick_meeting(&s, "项目预算评审").expect("target");
        let t_note = flow::ensure_note(&cfg, &s, &mut tm, &["赵磊".into()])
            .expect("t note")
            .expect("t note path");
        s.set_meeting_status(tm.id, MeetingStatus::Live)
            .expect("live");
        s.replace_segments(
            tm.id,
            SegmentPass::Final,
            &[
                TranscriptSegment {
                    meeting_id: tm.id,
                    start_ms: 0,
                    end_ms: 3_000,
                    text: VOICE_A_TEXT.into(),
                    language: Some("zh".into()),
                    speaker: SpeakerLabel::Cluster { index: 1 },
                    origin: AudioOrigin::Microphone,
                    spatial: None,
                },
                TranscriptSegment {
                    meeting_id: tm.id,
                    start_ms: 3_500,
                    end_ms: 6_500,
                    text: VOICE_B_TEXT.into(),
                    language: Some("zh".into()),
                    speaker: SpeakerLabel::Cluster { index: 2 },
                    origin: AudioOrigin::Microphone,
                    spatial: None,
                },
            ],
        )
        .expect("target segs");
        flow::sync_transcript(&cfg, &mut s, &tm).expect("sync target");
        s.set_meeting_status(tm.id, MeetingStatus::Processing)
            .expect("proc");
        drop(s);

        let llm_status = embed.llm_load(&qwen_dir).expect("load Qwen3-4B");
        assert!(llm_status.loaded);
        let minutes_ctx = MinutesCtx {
            cfg: cfg.clone(),
            store: store.clone(),
            codex: Some(CodexStatus::default()),
            embed: Some(embed.clone()),
        };
        let report = process_meeting(
            &minutes_ctx,
            || Err("refine disabled".into()),
            StoppedMeeting {
                meeting: tm.clone(),
                files: CaptureFiles::default(),
                warnings: vec![],
            },
            &|step| println!("[GUI-EMBED-E2E] step: {step}"),
        )
        .expect("process_meeting offline minutes");
        assert!(
            report.minutes_done,
            "local LLM minutes failed: {:?}",
            report.warnings
        );
        let min_val = store
            .lock()
            .unwrap()
            .minutes(tm.id)
            .expect("minutes")
            .expect("minutes JSON");
        let min: Minutes = serde_json::from_value(min_val).expect("parse minutes");
        println!("[GUI-EMBED-E2E] Qwen3-4B summary: {:?}", min.summary);
        assert!(!min.summary.trim().is_empty());

        embed.llm_unload().expect("unload Qwen3-4B");
        (d_note, t_note, tm.title, min.summary)
    };

    let em_status = embed.load(&embed_dir).expect("load EmbeddingGemma 2");
    assert!(em_status.loaded);
    drop(store);

    let (handle, state, _) = open_workbench(cx, tmp.path());
    cx.update(|cx| {
        state.update(cx, |app, cx| {
            app.attach_embed_engine_for_test(embed.clone(), em_status, cx);
            app.indexer.send(IndexerMsg::Touch(
                distractor_note,
                mm_index::Priority::Interactive,
            ));
            app.indexer.send(IndexerMsg::Touch(
                target_note,
                mm_index::Priority::Interactive,
            ));
        });
    });

    // Wait for the OS Indexer thread to embed all chunks of both Obsidian notes via mm-embedd.
    let stamp = current_stamp();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let stats = cx.read(|cx| {
            state
                .read(cx)
                .store
                .lock()
                .unwrap()
                .index_stats(&stamp)
                .expect("index stats")
        });
        if stats.sources >= 2 && stats.chunks >= 2 && stats.embedded == stats.chunks {
            println!("[GUI-EMBED-E2E] index stats converged: {stats:?}");
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "indexer did not converge with EmbeddingGemma 2: {stats:?}"
        );
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(50));
    }

    // Open LibraryPage, verify Qwen3-4B minutes summary renders, then run pure semantic query "财务审批额度".
    cx.update_window(handle, |_, window, cx| {
        window.click("0-0-3", cx);
        window.render_frame(cx);
    })
    .expect("open Library");

    cx.wait_for(handle, Duration::from_secs(2), |window, _| {
        window
            .try_find(("meeting", 0usize))
            .is_some_and(|snapshot| snapshot.visible())
            && window
                .try_find(("meeting", 1usize))
                .is_some_and(|snapshot| snapshot.visible())
    })
    .await;

    let (target_idx, distractor_idx) = cx.read(|cx| match &state.read(cx).meetings {
        Loadable::Ready(list) => {
            let t = list
                .iter()
                .position(|m| m.title == target_title)
                .expect("target meeting in AppState::meetings");
            let d = list
                .iter()
                .position(|m| m.title == "茶水间咖啡机维护")
                .expect("distractor meeting in AppState::meetings");
            (t, d)
        }
        other => panic!("expected Ready meetings, got {other:?}"),
    });

    cx.update_window(handle, |_, window, cx| {
        window.click(("meeting", target_idx), cx);
    })
    .expect("select target meeting in Library");

    cx.wait_for(handle, Duration::from_secs(3), |window, _| {
        window.try_find("regen").is_some() && has_any_text(window, &target_summary)
    })
    .await;

    cx.update_window(handle, |_, window, cx| {
        window.click(("meeting", distractor_idx), cx);
    })
    .expect("switch to distractor meeting before semantic search");

    cx.wait_for(handle, Duration::from_secs(3), |window, _| {
        window.try_find("regen").is_some()
            && has_any_text(window, "咖啡机维护")
            && !has_any_text(window, &target_summary)
    })
    .await;

    cx.update_window(handle, |_, window, cx| {
        // Semantic query has ZERO literal substring overlap with "预算三十七万五千元".
        window.click("lib-search", cx);
        window.input("财务审批额度", cx);
        window.press("enter", cx);
    })
    .expect("submit semantic search query");

    cx.wait_for(handle, Duration::from_secs(10), |window, _| {
        window
            .try_find(("hit", 0usize))
            .is_some_and(|snapshot| snapshot.visible())
    })
    .await;

    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            has_any_text(window, "关键词 + 语义"),
            "expected hybrid semantic search mode banner"
        );
        assert!(
            has_any_text(window, &target_title),
            "semantic search hits did not surface target meeting {target_title}"
        );
        window.click(("hit", 0usize), cx);
    })
    .expect("verify semantic search hit inside LibraryPage and open top hit");

    cx.wait_for(handle, Duration::from_secs(3), |window, _| {
        has_any_text(window, &target_summary)
    })
    .await;

    cx.update(|cx| {
        state.read(cx).indexer.send(IndexerMsg::Stop);
    });
    embed.unload().expect("unload embedder");
    embed.shutdown();
}

#[gpui_kit::test]
async fn gui_settings_cloud_live_mode_switch_and_utterance_pricing(cx: &mut TestAppContext) {
    let _guard = lock_gui();
    let (tmp, _, rec_dir) = create_test_env(serde_json::json!({
        "refine_after_meeting": false,
        "auto_minutes": false
    }));
    let (handle, state, _) = open_workbench(cx, tmp.path());

    // 1. Switch to OpenAI -> default Streaming mode with Live rate hint ($0.0170/min).
    cx.update_window(handle, |_, window, cx| {
        window.click("0-0-5", cx);
        window.render_frame(cx);

        scroll_right_pane_to_id(window, ("live-asr-backend", 1usize), cx);
        window.click(("live-asr-backend", 1usize), cx);
        window.render_frame(cx);

        scroll_right_pane_to_id(window, ("live-cloud-mode", 0usize), cx);
        assert!(window.try_find(("live-cloud-mode", 1usize)).is_some());
        scroll_right_pane_to_text(window, "$0.0170/分钟", cx);

        // Save custom streaming model first to prove it stays isolated from utterance model.
        scroll_right_pane_to_id(window, "live-openai-input", cx);
        window.click("live-openai-input", cx);
        window.press("cmd-a", cx);
        window.input("gpt-4o-transcribe", cx);
        window.press("enter", cx);
    })
    .expect("verify Streaming mode default & custom streaming model");
    cx.run_until_parked();
    cx.read(|cx| {
        let cfg = &state.read(cx).cfg;
        assert_eq!(cfg.live_asr_backend, LiveAsrBackend::OpenAi);
        assert_eq!(cfg.live_cloud_mode, CloudLiveMode::Streaming);
        assert_eq!(cfg.live_openai_model.as_deref(), Some("gpt-4o-transcribe"));
    });

    // 2. Switch to Utterance (Solution B) -> batch rate ($0.0045/min for gpt-transcribe),
    // edit utterance model without disturbing `live_openai_model`.
    cx.update_window(handle, |_, window, cx| {
        scroll_right_pane_to_id(window, ("live-cloud-mode", 1usize), cx);
        window.click(("live-cloud-mode", 1usize), cx);
        window.render_frame(cx);
        scroll_right_pane_to_text(window, "$0.0045/分钟", cx);

        scroll_right_pane_to_id(window, "live-openai-input", cx);
        window.click("live-openai-input", cx);
        window.press("cmd-a", cx);
        window.input("gpt-4o-mini-transcribe", cx);
        window.press("enter", cx);
    })
    .expect("switch to Utterance mode and configure custom utterance model");
    cx.run_until_parked();
    cx.read(|cx| {
        let cfg = &state.read(cx).cfg;
        assert_eq!(cfg.live_cloud_mode, CloudLiveMode::Utterance);
        assert_eq!(cfg.live_openai_model.as_deref(), Some("gpt-4o-transcribe"));
        assert_eq!(
            cfg.live_openai_utterance_model.as_deref(),
            Some("gpt-4o-mini-transcribe")
        );
    });

    cx.update_window(handle, |_, window, cx| {
        scroll_right_pane_to_text(window, "$0.0030/分钟", cx);

        // Switch back to Streaming and verify Live rate hint & model return.
        scroll_right_pane_to_id(window, ("live-cloud-mode", 0usize), cx);
        window.click(("live-cloud-mode", 0usize), cx);
        window.render_frame(cx);
        scroll_right_pane_to_text(window, "$0.0060/分钟", cx);
    })
    .expect("verify utterance rate hint and switching back to Streaming");

    // 3. Inject an Utterance-mode CaptionUsage for gpt-transcribe (60s) -> billed at $0.0045, not $0.0170.
    cx.update(|cx| {
        let store = state.read(cx).store.clone();
        let cfg = state.read(cx).cfg.clone();
        let session_dir = rec_dir.join("utt-usage-sess");
        std::fs::create_dir_all(&session_dir).expect("session dir");
        let s = store.lock().expect("lock store");
        let mut m = flow::quick_meeting(&s, "按句计费测试").expect("meeting");
        let note = flow::ensure_note(&cfg, &s, &mut m, &[]).expect("note");
        let started = CaptureStarted {
            session_id: "sess-utt-usage".into(),
            directory: session_dir,
            files: CaptureFiles::default(),
            microphone_channels: Some(1),
            started_at: Utc::now(),
        };
        flow::mark_live(&s, &mut m, &started).expect("mark live");
        drop(s);

        state.update(cx, |app, cx| {
            app.start_mock_live_for_test(m, started, note, String::new(), cx);
            app.inject_notice_for_test(
                MeetingNotice::Usage(CaptionUsage {
                    session_id: "sess-utt-usage".into(),
                    provider: "openai".into(),
                    model: "gpt-transcribe".into(),
                    origin: AudioOrigin::Microphone,
                    audio_ms: 60_000,
                    input_tokens: 0,
                    output_tokens: 0,
                    mode: Some(CloudLiveMode::Utterance),
                }),
                cx,
            );
            app.writer_flush_for_test();
            app.end_mock_live_for_test(cx);
            app.refresh_asr_usage(cx);
        });
    });
    cx.run_until_parked();
    cx.read(|cx| {
        let app = state.read(cx);
        assert_eq!(app.openai_usage.total.calls, 1);
        assert_eq!(app.openai_usage.total.audio_ms, 60_000);
        assert!(
            (app.openai_usage.total.cost_usd - 0.0045).abs() < 1e-6,
            "expected batch utterance rate $0.0045 for 60s, got {}",
            app.openai_usage.total.cost_usd
        );
    });
}
