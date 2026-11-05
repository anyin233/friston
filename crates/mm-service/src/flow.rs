//! The steps of one meeting: note → recording → transcript → minutes.
//!
//! Each step is a short, blocking function. Callers lock the store only around the
//! store-touching steps, never across engine or provider calls.

use std::path::{Path, PathBuf};

use chrono::{Local, Utc};
use mm_core::{
    AudioOrigin, CalendarEvent, CapturePreset, CaptureStarted, CaptureStopped, FileTranscript,
    Meeting, MeetingId, MeetingStatus, MeetingVenue, PresetVoiceprint, SpeakerLabel,
    TranscriptSegment,
};
use mm_llm::{Minutes, MinutesInput, TranscriptLine, render_minutes};
use mm_store::{SegmentPass, Store};
use mm_vault::{MeetingNote, replace_managed_block, write_atomic};

use crate::ServiceError;
use crate::config::AppConfig;
use crate::live::SpeakerDirectory;

/// URL scheme registered by the app; `open?meeting=<id>&t=<ms>` seeks a recording.
pub const URL_SCHEME: &str = "meeting-master";

/// Builds a seek link into a meeting.
///
/// Params: `meeting` – id; `ms` – offset.
/// Returns: the URL.
pub fn seek_url(meeting: MeetingId, ms: u64) -> String {
    format!("{URL_SCHEME}://open?meeting={meeting}&t={ms}")
}

/// Parses a seek link.
///
/// Params: `url` – as delivered by macOS.
/// Returns: meeting and offset (0 when absent).
pub fn parse_seek_url(url: &str) -> Option<(MeetingId, u64)> {
    let query = url.strip_prefix(&format!("{URL_SCHEME}://open?"))?;
    let mut meeting = None;
    let mut t = 0;
    for pair in query.split('&') {
        match pair.split_once('=') {
            Some(("meeting", v)) => meeting = v.parse().ok().map(MeetingId),
            Some(("t", v)) => t = v.parse().unwrap_or(0),
            _ => {}
        }
    }
    meeting.map(|m| (m, t))
}

/// Absolute path of a meeting's note, if a vault is configured and the meeting has one.
///
/// Params: `cfg`; `meeting`.
/// Returns: the path.
pub fn note_path(cfg: &AppConfig, meeting: &Meeting) -> Option<PathBuf> {
    Some(cfg.vault_path.as_ref()?.join(meeting.note_path.as_ref()?))
}

/// Creates the meeting's Obsidian note if it does not exist yet and records its path.
///
/// Params: `cfg`; `store`; `meeting` – updated in place with `note_path`; `attendees` – names.
/// Returns: the absolute note path, or `None` when no vault is configured.
pub fn ensure_note(
    cfg: &AppConfig,
    store: &Store,
    meeting: &mut Meeting,
    attendees: &[String],
) -> Result<Option<PathBuf>, ServiceError> {
    let Some(vault) = &cfg.vault_path else {
        return Ok(None);
    };
    if let Some(p) = note_path(cfg, meeting)
        && p.exists()
    {
        return Ok(Some(p));
    }
    let note = MeetingNote {
        meeting_id: meeting.id.to_string(),
        title: meeting.title.clone(),
        starts_at: meeting.starts_at,
        attendees: attendees.to_vec(),
        tags: meeting.tags.clone(),
        recording_path: meeting.recording_dir.clone(),
    };
    // MeetingNote paths start with `Meetings/`; honour a custom folder name.
    let rel = note.relative_path();
    let rel = match rel.strip_prefix("Meetings/") {
        Some(rest) if cfg.meetings_folder != "Meetings" => {
            format!("{}/{rest}", cfg.meetings_folder.trim_matches('/'))
        }
        _ => rel,
    };
    let abs = vault.join(&rel);
    if !abs.exists() {
        write_atomic(&abs, &note.render())?;
    }
    meeting.note_path = Some(rel);
    store.upsert_meeting(meeting)?;
    Ok(Some(abs))
}

/// Finds or creates the meeting row for a calendar event.
///
/// Params: `store`; `ev` – the event.
/// Returns: the meeting (persisted).
pub fn meeting_for_event(store: &Store, ev: &CalendarEvent) -> Result<Meeting, ServiceError> {
    if let Some(m) = store.meeting_for_event(&ev.id)? {
        return Ok(m);
    }
    let mut m = mm_store::new_meeting(&ev.title, ev.start);
    m.ends_at = Some(ev.end);
    m.calendar_event_id = Some(ev.id.clone());
    m.venue = match &ev.join_url {
        Some(url) => MeetingVenue::Online {
            join_url: Some(url.clone()),
            app_bundle_id: None,
        },
        None => MeetingVenue::InPerson {
            room: ev.location.clone(),
        },
    };
    for name in &ev.attendees {
        if let Some(p) = store.person_by_name(name)? {
            m.participants.push(p);
        }
    }
    store.upsert_meeting(&m)?;
    Ok(m)
}

/// Creates an ad-hoc meeting starting now ("开始记录" without a calendar event).
///
/// Params: `store`; `title` – empty uses a timestamped default.
/// Returns: the persisted meeting.
pub fn quick_meeting(store: &Store, title: &str) -> Result<Meeting, ServiceError> {
    let title = if title.trim().is_empty() {
        format!("{} 会议", Local::now().format("%m月%d日 %H:%M"))
    } else {
        title.trim().to_string()
    };
    let m = mm_store::new_meeting(&title, Utc::now());
    store.upsert_meeting(&m)?;
    Ok(m)
}

/// Builds the `capture/start` preset from settings and enrolled voiceprints.
///
/// Params: `cfg`; `store`; `meeting`; `window_id` – window to record, if chosen.
/// Returns: the preset.
pub fn build_preset(
    cfg: &AppConfig,
    store: &Store,
    meeting: &Meeting,
    window_id: Option<u32>,
) -> Result<CapturePreset, ServiceError> {
    let voiceprints = store
        .voiceprints(None)?
        .into_iter()
        .map(|v| PresetVoiceprint {
            person_id: v.person_id.to_string(),
            model: v.model,
            vector: v.vector,
        })
        .collect();
    Ok(CapturePreset {
        title: Some(meeting.title.clone()),
        window_id: if cfg.record_screen { window_id } else { None },
        display_id: None,
        // "default" lets the engine resolve the system default input at start time.
        microphone_uid: Some(
            cfg.microphone_uid
                .clone()
                .unwrap_or_else(|| "default".into()),
        ),
        system_audio: cfg.system_audio,
        caption_language: cfg.caption_language.clone(),
        fps: Some(cfg.fps),
        output_directory: cfg.recordings_dir.clone(),
        voiceprints,
        spatial_flip: cfg.spatial_flip,
        // A non-positive spacing would only make the engine's asin meaningless.
        spatial_spacing_cm: cfg
            .spatial_spacing_cm
            .filter(|cm| cm.is_finite() && *cm > 0.0),
    })
}

/// Microphone-only fallback for a preset whose screen/system-audio part needs the
/// Screen Recording permission (ScreenCaptureKit carries both).
///
/// Params: `preset` – the preset that failed.
/// Returns: the reduced preset, or `None` when it used no ScreenCaptureKit source or
/// has no microphone to fall back to.
pub fn without_screen_capture(preset: &CapturePreset) -> Option<CapturePreset> {
    let uses_sck = preset.window_id.is_some() || preset.display_id.is_some() || preset.system_audio;
    if !uses_sck || preset.microphone_uid.is_none() {
        return None;
    }
    Some(CapturePreset {
        window_id: None,
        display_id: None,
        system_audio: false,
        ..preset.clone()
    })
}

/// Records that capture started: status Live, recording folder.
///
/// Params: `store`; `meeting` – updated in place; `started` – engine result.
/// Returns: `Ok(())`.
pub fn mark_live(
    store: &Store,
    meeting: &mut Meeting,
    started: &CaptureStarted,
) -> Result<(), ServiceError> {
    meeting.recording_dir = Some(started.directory.display().to_string());
    if meeting.status == MeetingStatus::Scheduled {
        meeting.status = MeetingStatus::Live;
    }
    store.upsert_meeting(meeting)?;
    Ok(())
}

/// Records that capture stopped: status Processing and end time.
///
/// Params: `store`; `meeting` – updated in place; `stopped` – engine result.
/// Returns: `Ok(())`.
pub fn mark_stopped(
    store: &Store,
    meeting: &mut Meeting,
    stopped: &CaptureStopped,
) -> Result<(), ServiceError> {
    let _ = stopped;
    meeting.ends_at = Some(Utc::now());
    if meeting.status == MeetingStatus::Live {
        meeting.status = MeetingStatus::Processing;
    }
    store.upsert_meeting(meeting)?;
    Ok(())
}

/// Speaker directory for a meeting (all people + this meeting's named clusters).
///
/// Params: `store`; `meeting`.
/// Returns: the directory.
pub fn directory_for(store: &Store, meeting: MeetingId) -> Result<SpeakerDirectory, ServiceError> {
    let people = store
        .people()?
        .into_iter()
        .map(|(p, hue)| (p.id, p.display_name, hue));
    Ok(SpeakerDirectory::new(people, store.cluster_names(meeting)?))
}

/// Display name of a segment's speaker, marking remote speakers.
fn speaker_display(dir: &SpeakerDirectory, label: &SpeakerLabel, origin: AudioOrigin) -> String {
    let name = dir.name(label);
    match (origin, dir.person(label)) {
        (AudioOrigin::System, None) => format!("远程 · {name}"),
        _ => name,
    }
}

/// Renders the transcript as Markdown lines with seek links.
///
/// Params: `meeting`; `segments`; `dir`.
/// Returns: block body.
pub fn render_transcript(
    meeting: MeetingId,
    segments: &[TranscriptSegment],
    dir: &SpeakerDirectory,
) -> String {
    if segments.is_empty() {
        return "_暂无转写_".into();
    }
    segments
        .iter()
        .map(|s| {
            format!(
                "[{}]({}) **{}**：{}",
                mm_index::pipeline::fmt_ms(s.start_ms),
                seek_url(meeting, s.start_ms),
                speaker_display(dir, &s.speaker, s.origin),
                s.text.trim()
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Rewrites one managed block of the meeting note (user text stays byte-identical).
///
/// Params: `cfg`; `meeting`; `block` – block name; `body` – new content.
/// Returns: `Ok(false)` when the meeting has no note.
pub fn write_block(
    cfg: &AppConfig,
    meeting: &Meeting,
    block: &str,
    body: &str,
) -> Result<bool, ServiceError> {
    let Some(path) = note_path(cfg, meeting) else {
        return Ok(false);
    };
    let current = std::fs::read_to_string(&path)
        .map_err(|e| ServiceError::Vault(format!("reading note {}: {e}", path.display())))?;
    let updated = replace_managed_block(&current, block, body)?;
    if updated != current {
        write_atomic(&path, &updated)?;
    }
    Ok(true)
}

/// Writes the current transcript into the note's `transcript` block and re-indexes it.
///
/// Params: `cfg`; `store`; `meeting`.
/// Returns: `Ok(())`.
pub fn sync_transcript(
    cfg: &AppConfig,
    store: &mut Store,
    meeting: &Meeting,
) -> Result<(), ServiceError> {
    let segments = store.transcript(meeting.id)?;
    let dir = directory_for(store, meeting.id)?;
    write_block(
        cfg,
        meeting,
        "transcript",
        &render_transcript(meeting.id, &segments, &dir),
    )?;
    let names = |l: &SpeakerLabel| dir.name(l);
    mm_index::pipeline::index_transcript(store, meeting.id, &meeting.title, &segments, &names)?;
    Ok(())
}

/// Merges per-file offline transcripts into one final-pass transcript.
///
/// Both files start at capture start, so offsets are comparable; the system-audio file
/// carries remote speakers, whose cluster indices are shifted to stay distinct.
///
/// Params: `meeting`; `parts` – `(origin, transcript)` per recorded file.
/// Returns: segments sorted by start time.
pub fn merge_file_transcripts(
    meeting: MeetingId,
    parts: &[(AudioOrigin, FileTranscript)],
) -> Vec<TranscriptSegment> {
    let mut out = Vec::new();
    for (origin, t) in parts {
        let shift = if *origin == AudioOrigin::System {
            100
        } else {
            0
        };
        for s in &t.segments {
            if s.text.trim().is_empty() {
                continue;
            }
            let speaker = match &s.speaker {
                SpeakerLabel::Cluster { index } => SpeakerLabel::Cluster {
                    index: index + shift,
                },
                other => other.clone(),
            };
            out.push(TranscriptSegment {
                meeting_id: meeting,
                start_ms: s.start_ms,
                end_ms: s.end_ms,
                text: s.text.trim().to_string(),
                language: t.language.clone(),
                speaker,
                origin: s.origin.unwrap_or(*origin),
                spatial: None,
            });
        }
    }
    out.sort_by_key(|s| (s.start_ms, s.end_ms));
    out
}

/// Stores the post-meeting transcript as the final pass.
///
/// Params: `store`; `meeting`; `segments` – from [`merge_file_transcripts`].
/// Returns: `Ok(())`; an empty result keeps the live pass.
pub fn store_final_transcript(
    store: &mut Store,
    meeting: MeetingId,
    segments: &[TranscriptSegment],
) -> Result<(), ServiceError> {
    if !segments.is_empty() {
        store.replace_segments(meeting, SegmentPass::Final, segments)?;
    }
    Ok(())
}

/// The user's own notes: the `## 笔记` section of the note, without managed blocks.
///
/// Params: `markdown` – note text.
/// Returns: the section body (trimmed), or the whole body if the heading is missing.
pub fn notes_section(markdown: &str) -> String {
    let body = match markdown
        .strip_prefix("---\n")
        .and_then(|r| r.find("\n---").map(|e| &r[e + 4..]))
    {
        Some(rest) => rest,
        None => markdown,
    };
    let mut out = Vec::new();
    let mut inside = false;
    let mut seen_heading = false;
    for line in body.lines() {
        if line.starts_with("## ") {
            inside = line.contains("笔记") || line.to_lowercase().contains("notes");
            seen_heading |= inside;
            continue;
        }
        if inside {
            out.push(line);
        }
    }
    let text = if seen_heading {
        out.join("\n")
    } else {
        body.to_string()
    };
    strip_managed_blocks(&text).trim().to_string()
}

/// Removes `<!-- mm:…:start -->…<!-- mm:…:end -->` regions.
fn strip_managed_blocks(text: &str) -> String {
    let mut out = String::new();
    let mut skipping = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("<!-- mm:") && t.ends_with(":start -->") {
            skipping = true;
        } else if t.starts_with("<!-- mm:") && t.ends_with(":end -->") {
            skipping = false;
        } else if !skipping {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Images embedded in the notes section (`![[x.png]]` or `![](path)`), resolved in the vault.
fn note_images(vault: &Path, notes: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in notes.split("![").skip(1) {
        let target = if let Some(rest) = part.strip_prefix('[') {
            rest.split("]]").next()
        } else {
            part.split_once("](").and_then(|(_, r)| r.split(')').next())
        };
        let Some(t) = target.map(|t| t.split('|').next().unwrap_or(t).trim()) else {
            continue;
        };
        let p = if Path::new(t).is_absolute() {
            PathBuf::from(t)
        } else {
            find_in_vault(vault, t).unwrap_or_else(|| vault.join(t))
        };
        if p.is_file() {
            out.push(p.display().to_string());
        }
    }
    out
}

/// Resolves an Obsidian short link (`x.png`) by searching the vault's attachment folders.
fn find_in_vault(vault: &Path, name: &str) -> Option<PathBuf> {
    let direct = vault.join(name);
    if direct.is_file() {
        return Some(direct);
    }
    let mut stack = vec![vault.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).ok()?.flatten() {
            let p = e.path();
            if e.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            if p.is_dir() {
                stack.push(p);
            } else if p.file_name().is_some_and(|f| f.to_string_lossy() == name) {
                return Some(p);
            }
        }
    }
    None
}

/// Assembles the material for minutes generation.
///
/// Params: `cfg`; `store`; `meeting`.
/// Returns: the provider input.
pub fn minutes_input(
    cfg: &AppConfig,
    store: &Store,
    meeting: &Meeting,
) -> Result<MinutesInput, ServiceError> {
    let dir = directory_for(store, meeting.id)?;
    let transcript = store
        .transcript(meeting.id)?
        .iter()
        .map(|s| TranscriptLine {
            start_ms: s.start_ms,
            speaker: speaker_display(&dir, &s.speaker, s.origin),
            text: s.text.clone(),
        })
        .collect();
    let (notes, images) = match (note_path(cfg, meeting), &cfg.vault_path) {
        (Some(p), Some(vault)) => match std::fs::read_to_string(&p) {
            Ok(md) => {
                let notes = notes_section(&md);
                let images = note_images(vault, &notes);
                (notes, images)
            }
            Err(e) => {
                tracing::warn!("minutes without notes; cannot read {}: {e}", p.display());
                (String::new(), vec![])
            }
        },
        _ => (String::new(), vec![]),
    };
    let people = store.people()?;
    let attendees = meeting
        .participants
        .iter()
        .filter_map(|id| {
            people
                .iter()
                .find(|(p, _)| p.id == *id)
                .map(|(p, _)| p.display_name.clone())
        })
        .collect();
    Ok(MinutesInput {
        title: meeting.title.clone(),
        when: meeting
            .starts_at
            .with_timezone(&Local)
            .format("%Y-%m-%d %H:%M")
            .to_string(),
        attendees,
        notes,
        transcript,
        images,
        language: Some(cfg.minutes_language.clone()),
    })
}

/// Stores minutes, writes the note blocks and marks the meeting done.
///
/// Params: `cfg`; `store`; `meeting` – updated in place.
/// Returns: `Ok(())`.
pub fn apply_minutes(
    cfg: &AppConfig,
    store: &mut Store,
    meeting: &mut Meeting,
    minutes: &Minutes,
) -> Result<(), ServiceError> {
    store.set_minutes(
        meeting.id,
        &serde_json::to_value(minutes).expect("minutes serialize"),
    )?;
    let id = meeting.id;
    let seek = move |ms: u64| seek_url(id, ms);
    let rendered = render_minutes(minutes, Some(&seek));
    write_block(cfg, meeting, "minutes", &rendered.minutes)?;
    write_block(cfg, meeting, "actions", &rendered.actions)?;
    if meeting.status == MeetingStatus::Live {
        meeting.status = MeetingStatus::Processing;
    }
    if meeting.status == MeetingStatus::Processing {
        meeting.status = MeetingStatus::Done;
    }
    store.upsert_meeting(meeting)?;
    // Minutes are searchable too; the vault watcher would catch the note, but do it now.
    if let (Some(vault), Some(p)) = (&cfg.vault_path, note_path(cfg, meeting)) {
        mm_index::pipeline::refresh_note(store, vault, &p)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mm_core::{FileSegment, Person, PersonId};

    fn setup() -> (tempfile::TempDir, AppConfig, Store) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = AppConfig {
            vault_path: Some(dir.path().to_path_buf()),
            ..Default::default()
        };
        (dir, cfg, Store::open_in_memory().unwrap())
    }

    fn seg(
        m: MeetingId,
        start: u64,
        text: &str,
        speaker: SpeakerLabel,
        origin: AudioOrigin,
    ) -> TranscriptSegment {
        TranscriptSegment {
            meeting_id: m,
            start_ms: start,
            end_ms: start + 500,
            text: text.into(),
            language: None,
            speaker,
            origin,
            spatial: None,
        }
    }

    #[test]
    fn screen_capture_fallback_keeps_the_microphone() {
        let full = CapturePreset {
            window_id: Some(3),
            system_audio: true,
            microphone_uid: Some("default".into()),
            ..Default::default()
        };
        let reduced = without_screen_capture(&full).unwrap();
        assert_eq!((reduced.window_id, reduced.system_audio), (None, false));
        assert_eq!(reduced.microphone_uid.as_deref(), Some("default"));
        assert!(
            without_screen_capture(&reduced).is_none(),
            "nothing left to drop"
        );
        let no_mic = CapturePreset {
            microphone_uid: None,
            ..full
        };
        assert!(without_screen_capture(&no_mic).is_none());
    }

    #[test]
    fn seek_urls_round_trip() {
        let m = MeetingId::new();
        assert_eq!(parse_seek_url(&seek_url(m, 61_000)), Some((m, 61_000)));
        assert_eq!(parse_seek_url("https://x"), None);
    }

    #[test]
    fn full_flow_writes_note_blocks_and_keeps_user_text() {
        let (_dir, cfg, mut store) = setup();
        let zhao = Person {
            id: PersonId::new(),
            display_name: "赵磊".into(),
            email: None,
            note_path: None,
        };
        store.upsert_person(&zhao, None).unwrap();
        let ev = CalendarEvent {
            id: "ev1".into(),
            title: "周会".into(),
            start: Utc::now(),
            end: Utc::now(),
            calendar: "工作".into(),
            location: None,
            join_url: Some("https://meeting.tencent.com/x".into()),
            attendees: vec!["赵磊".into(), "陌生人".into()],
        };
        let mut m = meeting_for_event(&store, &ev).unwrap();
        assert_eq!(m.participants, vec![zhao.id]);
        assert_eq!(
            meeting_for_event(&store, &ev).unwrap().id,
            m.id,
            "event maps to one meeting"
        );

        let note = ensure_note(&cfg, &store, &mut m, &["赵磊".into()])
            .unwrap()
            .unwrap();
        assert!(note.starts_with(cfg.vault_path.as_ref().unwrap().join("Meetings")));
        let user = std::fs::read_to_string(&note)
            .unwrap()
            .replace("## 笔记 Notes\n\n", "## 笔记 Notes\n\n我关心预算\n");
        std::fs::write(&note, user).unwrap();

        let preset = build_preset(&cfg, &store, &m, Some(7)).unwrap();
        assert_eq!(preset.window_id, Some(7));
        assert_eq!(preset.microphone_uid.as_deref(), Some("default"));
        assert_eq!(preset.caption_language.as_deref(), Some("zh"));
        assert!(!preset.spatial_flip);
        assert_eq!(preset.spatial_spacing_cm, None);
        let mut spatial_cfg = cfg.clone();
        spatial_cfg.spatial_flip = true;
        spatial_cfg.spatial_spacing_cm = Some(14.0);
        let spatial = build_preset(&spatial_cfg, &store, &m, None).unwrap();
        assert!(spatial.spatial_flip);
        assert_eq!(spatial.spatial_spacing_cm, Some(14.0));
        spatial_cfg.spatial_spacing_cm = Some(0.0);
        assert_eq!(
            build_preset(&spatial_cfg, &store, &m, None)
                .unwrap()
                .spatial_spacing_cm,
            None
        );

        let started = CaptureStarted {
            session_id: "s".into(),
            directory: "/r/x".into(),
            files: Default::default(),
            microphone_channels: Some(1),
            started_at: Utc::now(),
        };
        mark_live(&store, &mut m, &started).unwrap();
        assert_eq!(m.status, MeetingStatus::Live);

        store.name_cluster(m.id, 1, Some(zhao.id)).unwrap();
        store
            .add_segment(
                &seg(
                    m.id,
                    0,
                    "预算上调",
                    SpeakerLabel::Cluster { index: 1 },
                    AudioOrigin::Microphone,
                ),
                SegmentPass::Live,
                Some(1),
            )
            .unwrap();
        store
            .add_segment(
                &seg(
                    m.id,
                    1000,
                    "同意",
                    SpeakerLabel::Cluster { index: 2 },
                    AudioOrigin::System,
                ),
                SegmentPass::Live,
                Some(2),
            )
            .unwrap();
        sync_transcript(&cfg, &mut store, &m).unwrap();
        let md = std::fs::read_to_string(&note).unwrap();
        assert!(md.contains("**赵磊**：预算上调"));
        assert!(md.contains("**远程 · 说话人 2**：同意"));
        assert!(md.contains("我关心预算"));

        mark_stopped(
            &store,
            &mut m,
            &CaptureStopped {
                session_id: "s".into(),
                duration_ms: 1,
                files: Default::default(),
            },
        )
        .unwrap();
        let input = minutes_input(&cfg, &store, &m).unwrap();
        assert_eq!(input.notes, "我关心预算");
        assert_eq!(input.attendees, vec!["赵磊"]);
        assert_eq!(input.transcript[1].speaker, "远程 · 说话人 2");

        let minutes = Minutes {
            title: "周会".into(),
            summary: "预算上调。".into(),
            decisions: vec!["预算上调".into()],
            action_items: vec![],
            open_questions: vec![],
            topics: vec![],
        };
        apply_minutes(&cfg, &mut store, &mut m, &minutes).unwrap();
        assert_eq!(
            store.meeting(m.id).unwrap().unwrap().status,
            MeetingStatus::Done
        );
        let md = std::fs::read_to_string(&note).unwrap();
        assert!(md.contains("### 决定\n\n- 预算上调"));
        assert!(md.contains("我关心预算"), "user notes survive");
        assert!(store.minutes(m.id).unwrap().is_some());
        assert!(
            !store.search("我关心预算", None, 5).unwrap().is_empty(),
            "note was indexed"
        );
    }

    #[test]
    fn file_transcripts_merge_with_distinct_remote_clusters() {
        let m = MeetingId::new();
        let ft = |segs: Vec<(u64, &str)>| FileTranscript {
            language: Some("zh".into()),
            segments: segs
                .into_iter()
                .map(|(s, t)| FileSegment {
                    start_ms: s,
                    end_ms: s + 100,
                    text: t.into(),
                    speaker: SpeakerLabel::Cluster { index: 1 },
                    origin: None,
                })
                .collect(),
        };
        let merged = merge_file_transcripts(
            m,
            &[
                (AudioOrigin::Microphone, ft(vec![(500, "b"), (0, " ")])),
                (AudioOrigin::System, ft(vec![(100, "a")])),
            ],
        );
        assert_eq!(
            merged.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(merged[0].speaker, SpeakerLabel::Cluster { index: 101 });
        assert_eq!(merged[0].origin, AudioOrigin::System);
    }

    #[test]
    fn notes_section_skips_frontmatter_blocks_and_other_sections() {
        let md = "---\ntitle: x\n---\n# T\n\n## 笔记 Notes\n\n要点 ![[s.png]]\n<!-- mm:x:start -->\n生成\n<!-- mm:x:end -->\n\n## 纪要 Minutes\nm\n";
        assert_eq!(notes_section(md), "要点 ![[s.png]]");
        assert_eq!(notes_section("plain"), "plain");
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("附件")).unwrap();
        std::fs::write(dir.path().join("附件/s.png"), b"png").unwrap();
        assert_eq!(
            note_images(dir.path(), "要点 ![[s.png]] ![x](/nope.png)").len(),
            1
        );
    }
}
