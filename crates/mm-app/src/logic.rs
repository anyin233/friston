//! Pure helpers behind the pages: formatting, filtering and small state transitions.
//!
//! Nothing here touches GPUI, the store or an engine, so every rule the UI relies on
//! is covered by unit tests.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, TimeZone, Utc};
use mm_core::{CalendarEvent, MeetingStatus, SpeakerLabel};
use mm_llm::{ByokProvider, Minutes, ProviderConfig};
use mm_service::AppConfig;
use mm_service::live::CaptionLine;

/// Formats an offset as `mm:ss` (or `h:mm:ss`), matching transcript links.
///
/// Params: `ms` – offset in milliseconds.
/// Returns: the clock text.
pub fn clock(ms: u64) -> String {
    mm_index::pipeline::fmt_ms(ms)
}

/// Text inserted by `⌘↩` in the notes editor.
///
/// Params: `elapsed_ms` – time since capture started.
/// Returns: e.g. `[03:07] `.
pub fn timestamp_token(elapsed_ms: u64) -> String {
    format!("[{}] ", clock(elapsed_ms))
}

/// Chinese label for a meeting status (tags in lists).
///
/// Params: `s` – status.
/// Returns: the label.
pub fn status_label(s: MeetingStatus) -> &'static str {
    match s {
        MeetingStatus::Scheduled => "未开始",
        MeetingStatus::Live => "记录中",
        MeetingStatus::Processing => "整理中",
        MeetingStatus::Done => "纪要已生成",
        MeetingStatus::Cancelled => "已取消",
    }
}

/// Maps a dBFS level to a meter fill fraction; −60 dB and below is silent.
///
/// Params: `db` – level in dBFS.
/// Returns: 0.0–1.0.
pub fn level_fraction(db: f32) -> f32 {
    if db.is_nan() {
        return 0.0;
    }
    ((db + 60.0) / 60.0).clamp(0.0, 1.0)
}

/// Replaces the body of the note's `## 笔记` section, leaving every other byte alone.
///
/// The section runs from its heading to the next `## ` heading (or the end). The body is
/// written with one blank line after the heading and before the next heading so Obsidian
/// renders it the same way as the template.
///
/// Params: `md` – current note; `body` – the user's notes.
/// Returns: the new note, or `None` when the note has no notes heading.
pub fn replace_notes_section(md: &str, body: &str) -> Option<String> {
    let mut offset = 0;
    let mut start = None;
    let mut end = md.len();
    for line in md.split_inclusive('\n') {
        let is_heading = line.starts_with("## ");
        if let Some(_s) = start {
            if is_heading {
                end = offset;
                break;
            }
        } else if is_heading && (line.contains("笔记") || line.to_lowercase().contains("notes")) {
            start = Some(offset + line.len());
        }
        offset += line.len();
    }
    let start = start?;
    let trimmed = body.trim_end();
    let mut out = String::with_capacity(md.len() + body.len());
    out.push_str(&md[..start]);
    if !md[..start].ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
    if !trimmed.is_empty() {
        out.push_str(trimmed);
        out.push('\n');
    }
    if end < md.len() {
        out.push('\n');
    }
    out.push_str(&md[end..]);
    Some(out)
}

/// UTC bounds of `days` local days starting at `day`.
///
/// Params: `day` – first local day; `days` – number of days.
/// Returns: `[from, to)` in UTC.
pub fn local_days(day: NaiveDate, days: i64) -> (DateTime<Utc>, DateTime<Utc>) {
    let start = Local
        .from_local_datetime(&day.and_hms_opt(0, 0, 0).expect("midnight exists"))
        .earliest()
        .unwrap_or_else(Local::now);
    let end = start + Duration::days(days);
    (start.with_timezone(&Utc), end.with_timezone(&Utc))
}

/// Groups events by their local start day, sorted by start time.
///
/// Params: `events` – any order.
/// Returns: `(day, events)` in day order.
pub fn group_by_day(events: &[CalendarEvent]) -> Vec<(NaiveDate, Vec<CalendarEvent>)> {
    let mut map: BTreeMap<NaiveDate, Vec<CalendarEvent>> = BTreeMap::new();
    for ev in events {
        map.entry(ev.start.with_timezone(&Local).date_naive())
            .or_default()
            .push(ev.clone());
    }
    for evs in map.values_mut() {
        evs.sort_by_key(|e| e.start);
    }
    map.into_iter().collect()
}

/// Day heading such as `11月5日 周四`, with 今天/明天 when applicable.
///
/// Params: `day` – the day; `today` – the current local day.
/// Returns: the heading.
pub fn day_heading(day: NaiveDate, today: NaiveDate) -> String {
    const WEEK: [&str; 7] = ["周一", "周二", "周三", "周四", "周五", "周六", "周日"];
    let base = format!(
        "{}月{}日 {}",
        day.month(),
        day.day(),
        WEEK[day.weekday().num_days_from_monday() as usize]
    );
    match (day - today).num_days() {
        0 => format!("今天 · {base}"),
        1 => format!("明天 · {base}"),
        _ => base,
    }
}

/// Relative description of an event's timing, e.g. `8 分钟后`, `进行中`, `已结束`.
///
/// Params: `now`; `start`; `end`.
/// Returns: the text.
pub fn relative_start(now: DateTime<Utc>, start: DateTime<Utc>, end: DateTime<Utc>) -> String {
    if now >= end {
        return "已结束".into();
    }
    if now >= start {
        return "进行中".into();
    }
    let mins = (start - now).num_minutes();
    match mins {
        0 => "马上开始".into(),
        1..=59 => format!("{mins} 分钟后"),
        _ => format!("{} 小时 {} 分钟后", mins / 60, mins % 60),
    }
}

/// Local `HH:MM–HH:MM` span of an event.
///
/// Params: `start`, `end`.
/// Returns: the text.
pub fn time_span(start: DateTime<Utc>, end: DateTime<Utc>) -> String {
    format!(
        "{}–{}",
        start.with_timezone(&Local).format("%H:%M"),
        end.with_timezone(&Local).format("%H:%M")
    )
}

/// Short local date-time for meeting lists (`11月5日 14:00`).
///
/// Params: `t` – time.
/// Returns: the text.
pub fn short_datetime(t: DateTime<Utc>) -> String {
    let l = t.with_timezone(&Local);
    format!("{}月{}日 {}", l.month(), l.day(), l.format("%H:%M"))
}

/// Events that have not ended yet, soonest first.
///
/// Params: `events`; `now`.
/// Returns: upcoming or running events.
pub fn upcoming(events: &[CalendarEvent], now: DateTime<Utc>) -> Vec<CalendarEvent> {
    let mut out: Vec<_> = events.iter().filter(|e| e.end > now).cloned().collect();
    out.sort_by_key(|e| e.start);
    out
}

/// Parses stored minutes JSON.
///
/// Params: `v` – `minutes_json`.
/// Returns: the minutes, or `None` if the shape is unknown.
pub fn parse_minutes(v: &serde_json::Value) -> Option<Minutes> {
    serde_json::from_value(v.clone()).ok()
}

/// One row of the "我的待办" list.
#[derive(Debug, Clone, PartialEq)]
pub struct ActionRow {
    pub meeting_title: String,
    pub task: String,
    pub owner: Option<String>,
    pub due: Option<String>,
}

/// Flattens action items of several meetings, newest meeting first.
///
/// Params: `items` – `(meeting title, minutes)` newest first; `limit` – maximum rows.
/// Returns: rows.
pub fn action_rows(items: &[(String, Minutes)], limit: usize) -> Vec<ActionRow> {
    items
        .iter()
        .flat_map(|(title, m)| {
            m.action_items.iter().map(move |a| ActionRow {
                meeting_title: title.clone(),
                task: a.task.clone(),
                owner: a.owner.clone(),
                due: a.due.clone(),
            })
        })
        .take(limit)
        .collect()
}

/// Shortens text to `max` characters on a char boundary, adding `…`.
///
/// Params: `text`; `max` – characters.
/// Returns: the snippet with whitespace collapsed.
pub fn snippet(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut out: String = flat.chars().take(max).collect();
    out.push('…');
    out
}

/// Start offset carried by a search hit's locator (transcripts and media).
///
/// Params: `locator` – the hit locator.
/// Returns: milliseconds, if any.
pub fn locator_ms(locator: &serde_json::Value) -> Option<u64> {
    locator.get("start_ms").and_then(|v| v.as_u64())
}

/// Chinese label for a search modality.
///
/// Params: `m` – `text`, `image`, `audio` or `video`.
/// Returns: the label.
pub fn modality_label(m: &str) -> &'static str {
    match m {
        "image" => "图片",
        "audio" => "音频",
        "video" => "视频",
        _ => "文字",
    }
}

/// Builds the provider policy input from settings and the detected environment.
///
/// Params: `cfg`; `key_present` – Keychain has the BYOK key; `codex` – signed-in codex path;
/// `local_model` – loaded local LLM folder.
/// Returns: the provider config.
pub fn provider_config(
    cfg: &AppConfig,
    key_present: bool,
    codex: Option<String>,
    local_model: Option<String>,
) -> ProviderConfig {
    ProviderConfig {
        byok: cfg.byok.clone(),
        byok_key_present: cfg.byok.is_some() && key_present,
        codex_signed_in: if cfg.use_codex { codex } else { None },
        local_model,
    }
}

/// BYOK provider kinds offered in settings (`""` = none).
pub const BYOK_KINDS: [(&str, &str); 5] = [
    ("", "不使用"),
    ("openai", "OpenAI"),
    ("anthropic", "Anthropic"),
    ("gemini", "Gemini"),
    ("openai_compatible", "OpenAI 兼容"),
];

/// Builds a provider from the settings form.
///
/// Params: `kind` – one of [`BYOK_KINDS`]; `model`; `base_url` – compatible endpoints only.
/// Returns: the provider, or `None` for "不使用" or an empty model.
pub fn byok_from_form(kind: &str, model: &str, base_url: &str) -> Option<ByokProvider> {
    let model = model.trim().to_string();
    if model.is_empty() {
        return None;
    }
    Some(match kind {
        "openai" => ByokProvider::OpenAi { model },
        "anthropic" => ByokProvider::Anthropic { model },
        "gemini" => ByokProvider::Gemini { model },
        "openai_compatible" => ByokProvider::OpenAiCompatible {
            base_url: base_url.trim().to_string(),
            model,
        },
        _ => return None,
    })
}

/// Splits a provider back into form fields.
///
/// Params: `p` – provider.
/// Returns: `(kind, model, base_url)`.
pub fn byok_parts(p: &ByokProvider) -> (&'static str, String, String) {
    match p {
        ByokProvider::OpenAi { model } => ("openai", model.clone(), String::new()),
        ByokProvider::Anthropic { model } => ("anthropic", model.clone(), String::new()),
        ByokProvider::Gemini { model } => ("gemini", model.clone(), String::new()),
        ByokProvider::OpenAiCompatible { base_url, model } => {
            ("openai_compatible", model.clone(), base_url.clone())
        }
    }
}

/// Folder for screenshots taken during a meeting: `<vault>/<meetings folder>/附件`.
///
/// Params: `vault`; `meetings_folder`.
/// Returns: the folder.
pub fn attachments_dir(vault: &Path, meetings_folder: &str) -> PathBuf {
    vault.join(meetings_folder.trim_matches('/')).join("附件")
}

/// File name of a screenshot (unique per second, readable in Obsidian).
///
/// Params: `now` – local time.
/// Returns: e.g. `截图 2026-11-05 140312.png`.
pub fn screenshot_name(now: DateTime<Local>) -> String {
    format!("截图 {}.png", now.format("%Y-%m-%d %H%M%S"))
}

/// Arguments for `/usr/sbin/screencapture`.
///
/// Params: `window_id` – window to capture, else the whole screen; `out` – PNG path.
/// Returns: argv without the program.
pub fn screencapture_args(window_id: Option<u32>, out: &Path) -> Vec<String> {
    let mut args = vec!["-x".to_string()];
    if let Some(id) = window_id {
        args.push("-l".into());
        args.push(id.to_string());
    }
    args.push(out.display().to_string());
    args
}

/// Parses a seconds field (`12`, `12.5`, `1:05`) to milliseconds.
///
/// Params: `s` – user text.
/// Returns: milliseconds, or `None` when empty/invalid.
pub fn parse_seconds(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Some((m, sec)) = s.split_once(':') {
        let m: u64 = m.trim().parse().ok()?;
        let sec: f64 = sec.trim().parse().ok()?;
        return (sec >= 0.0).then(|| m * 60_000 + (sec * 1000.0) as u64);
    }
    let v: f64 = s.parse().ok()?;
    (v >= 0.0).then_some((v * 1000.0) as u64)
}

/// Speaking time per speaker on the caption board, longest first.
///
/// Params: `lines` – caption lines (only final lines with an end count).
/// Returns: `(label, milliseconds)`.
pub fn talk_time(lines: &[CaptionLine]) -> Vec<(SpeakerLabel, u64)> {
    let mut out: Vec<(SpeakerLabel, u64)> = Vec::new();
    for l in lines {
        let Some(end) = l.end_ms else { continue };
        let ms = end.saturating_sub(l.start_ms);
        let key = speaker_key(&l.speaker);
        match out.iter_mut().find(|(k, _)| *k == key) {
            Some((_, t)) => *t += ms,
            None => out.push((key, ms)),
        }
    }
    out.sort_by_key(|e| std::cmp::Reverse(e.1));
    out
}

/// Groups labels by speaker: known labels carry a per-utterance score, so compare by person.
///
/// Params: `label` – as received.
/// Returns: the label with a zero score for known people.
fn speaker_key(label: &SpeakerLabel) -> SpeakerLabel {
    match label {
        SpeakerLabel::Known { person_id, .. } => SpeakerLabel::Known {
            person_id: *person_id,
            score: 0.0,
        },
        other => other.clone(),
    }
}

/// Left-right coordinates within this band around 0 read as "居中". A third of the observed
/// range: relative positions span about −1…1, so three equal bands split the room evenly.
pub const DIRECTION_CENTRE_BAND: f32 = 0.33;

/// Direction label for a speaker's average left-right coordinate.
///
/// Params: `x` – average `spatial/cue` `position[0]`, about −1 (left) … 1 (right).
/// Returns: `偏左`, `居中` or `偏右`.
pub fn direction_label(x: f32) -> &'static str {
    if x < -DIRECTION_CENTRE_BAND {
        "偏左"
    } else if x > DIRECTION_CENTRE_BAND {
        "偏右"
    } else {
        "居中"
    }
}

/// Widest angle the seat indicator uses; ±90° would put dots on the baseline next to the mic.
pub const SEAT_MAX_DEG: f32 = 75.0;

/// Where a speaker sits on the front half-circle indicator.
///
/// Relative positions carry no true angle, so this is a display mapping, not an azimuth.
/// Params: `x` – average left-right coordinate (clamped to −1…1).
/// Returns: `(dx, dy)` on a unit half-circle: `dx` −1…1 to the right, `dy` 0…1 upwards (front).
pub fn seat_point(x: f32) -> (f32, f32) {
    let x = if x.is_finite() {
        x.clamp(-1.0, 1.0)
    } else {
        0.0
    };
    let a = (x * SEAT_MAX_DEG).to_radians();
    (a.sin(), a.cos())
}

/// Parses the optional "麦克风间距（厘米）" field.
///
/// Params: `s` – field text; blank clears the value.
/// Returns: `Ok(None)` for blank, `Ok(Some(cm))` for 1–500 cm, `Err` with a Chinese message otherwise.
pub fn parse_spacing_cm(s: &str) -> Result<Option<f64>, String> {
    let t = s
        .trim()
        .trim_end_matches("cm")
        .trim_end_matches("厘米")
        .trim();
    if t.is_empty() {
        return Ok(None);
    }
    match t.parse::<f64>() {
        // Narrower than a capsule pair or wider than a room is a typo, not a microphone.
        Ok(cm) if cm.is_finite() && (1.0..=500.0).contains(&cm) => Ok(Some(cm)),
        _ => Err(format!(
            "麦克风间距要填 1 到 500 之间的厘米数，收到的是“{}”",
            s.trim()
        )),
    }
}

/// Shows a stored spacing in the settings field.
///
/// Params: `cm` – stored value.
/// Returns: e.g. `14` or `14.5`; empty when unknown.
pub fn spacing_text(cm: Option<f64>) -> String {
    cm.map(|v| format!("{v}")).unwrap_or_default()
}

/// Formats a byte count for people (decimal units, like Finder).
///
/// Params: `bytes`.
/// Returns: e.g. `620 MB`, `1.5 GB`, `512 B`.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64 / 1000.0;
    let mut unit = 0;
    while v >= 1000.0 && unit + 1 < UNITS.len() {
        v /= 1000.0;
        unit += 1;
    }
    // One decimal only where it carries information (1.5 GB, not 620.3 MB).
    if v < 10.0 {
        format!("{v:.1} {}", UNITS[unit])
    } else {
        format!("{v:.0} {}", UNITS[unit])
    }
}

/// Describes a running model download for the settings page.
///
/// Params: `repo` – `owner/name`; `p` – progress.
/// Returns: e.g. `正在下载 o/m：620 MB / 1.5 GB（41%），第 9/15 个文件`.
pub fn download_line(repo: &str, p: &mm_service::hub::Progress) -> String {
    if p.files_total == 0 {
        return format!("正在获取 {repo} 的文件列表…");
    }
    let percent = (p.done_bytes * 100)
        .checked_div(p.total_bytes)
        .map(|pct| format!("（{pct}%）"))
        .unwrap_or_default();
    format!(
        "正在下载 {repo}：{} / {}{percent}，第 {}/{} 个文件",
        human_bytes(p.done_bytes),
        human_bytes(p.total_bytes),
        (p.files_done + 1).min(p.files_total),
        p.files_total
    )
}

/// Cues below this confidence say little about where someone sits.
const MIN_CUE_CONFIDENCE: f32 = 0.15;
/// Smoothing floor: after five cues the average becomes a moving one, so a speaker who
/// changes seats (or glasses wearer who turns) is followed within a few utterances.
const MIN_SMOOTHING: f32 = 0.2;

/// Running left-right position per speaker, fed by `spatial/cue`.
#[derive(Debug, Default, Clone)]
pub struct SpeakerPositions {
    /// `(speaker, average x, cues seen)` in first-heard order.
    entries: Vec<(SpeakerLabel, f32, u32)>,
}

impl SpeakerPositions {
    /// Folds one cue into its speaker's average.
    ///
    /// Params: `speaker`; `x` – `position[0]`; `confidence` – 0…1.
    /// Returns: `true` when the cue was used (known speaker, finite, confident enough).
    pub fn apply(&mut self, speaker: &SpeakerLabel, x: f32, confidence: f32) -> bool {
        if *speaker == SpeakerLabel::Unknown || !x.is_finite() || confidence < MIN_CUE_CONFIDENCE {
            return false;
        }
        let key = speaker_key(speaker);
        match self.entries.iter_mut().find(|(k, _, _)| *k == key) {
            Some((_, avg, n)) => {
                *n += 1;
                let alpha = (1.0 / *n as f32).max(MIN_SMOOTHING);
                *avg += alpha * (x - *avg);
            }
            None => self.entries.push((key, x, 1)),
        }
        true
    }

    /// Applies `speaker/relabel`: `from`'s history moves to `to`, merged by cue count.
    ///
    /// Params: `from`, `to` – labels from the notice.
    pub fn relabel(&mut self, from: &SpeakerLabel, to: &SpeakerLabel) {
        let (from, to) = (speaker_key(from), speaker_key(to));
        let Some(i) = self.entries.iter().position(|(k, _, _)| *k == from) else {
            return;
        };
        let (_, x, n) = self.entries.remove(i);
        match self.entries.iter_mut().find(|(k, _, _)| *k == to) {
            Some((_, avg, m)) => {
                *avg = (*avg * *m as f32 + x * n as f32) / (*m + n) as f32;
                *m += n;
            }
            None => self.entries.insert(i.min(self.entries.len()), (to, x, n)),
        }
    }

    /// Speakers with a position, in first-heard order.
    ///
    /// Returns: `(speaker, average left-right coordinate)`.
    pub fn entries(&self) -> Vec<(SpeakerLabel, f32)> {
        self.entries
            .iter()
            .map(|(k, x, _)| (k.clone(), *x))
            .collect()
    }

    /// Whether no cue has arrived yet.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Microphone used when the settings say "system default": the first physical input,
/// else the first input of any kind.
///
/// Params: `inputs` – from `audio/probeInputs`.
/// Returns: the device UID, or `None` when there is no input at all.
pub fn default_microphone(inputs: &[mm_core::AudioInput]) -> Option<String> {
    inputs
        .iter()
        .find(|i| !i.is_virtual)
        .or_else(|| inputs.first())
        .map(|i| i.uid.clone())
}

/// Caption language choices: `(config value, label)`; `None` = auto.
pub const CAPTION_LANGUAGES: [(Option<&str>, &str); 4] = [
    (Some("zh"), "中文"),
    (Some("en"), "English"),
    (Some("ja"), "日本語"),
    (None, "自动识别"),
];

/// Minutes language choices.
pub const MINUTES_LANGUAGES: [(&str, &str); 3] =
    [("zh", "中文"), ("en", "English"), ("ja", "日本語")];

/// System Settings deep link for a permission error message.
///
/// Params: `message` – error text from the service layer.
/// Returns: the `x-apple.systempreferences:` URL.
pub fn privacy_settings_url(message: &str) -> &'static str {
    if message.contains("日历") {
        "x-apple.systempreferences:com.apple.preference.security?Privacy_Calendars"
    } else if message.contains("麦克风") {
        "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
    } else {
        "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
    }
}

#[cfg(test)]
mod download_tests {
    use super::*;
    use mm_service::hub::Progress;

    #[test]
    fn bytes_and_download_progress_read_naturally() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(620_300_000), "620 MB");
        assert_eq!(human_bytes(1_521_085_440), "1.5 GB");
        assert_eq!(
            download_line("o/m", &Progress::default()),
            "正在获取 o/m 的文件列表…"
        );
        let p = Progress {
            file: "model.safetensors".into(),
            done_bytes: 620_000_000,
            total_bytes: 1_500_000_000,
            files_done: 8,
            files_total: 15,
        };
        assert_eq!(
            download_line("o/m", &p),
            "正在下载 o/m：620 MB / 1.5 GB（41%），第 9/15 个文件"
        );
        let finished = Progress {
            files_done: 15,
            ..p
        };
        assert!(download_line("o/m", &finished).ends_with("第 15/15 个文件"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mm_core::{AudioOrigin, PersonId};
    use mm_llm::ActionItem;

    #[test]
    fn notes_section_is_replaced_without_touching_other_sections() {
        let md = "---\nx: 1\n---\n# T\n\n## 笔记 Notes\n\n旧\n\n## 纪要 Minutes\n<!-- mm:minutes:start -->\nm\n<!-- mm:minutes:end -->\n";
        let out = replace_notes_section(md, "新的笔记\n- 要点").unwrap();
        assert_eq!(
            out,
            "---\nx: 1\n---\n# T\n\n## 笔记 Notes\n\n新的笔记\n- 要点\n\n## 纪要 Minutes\n<!-- mm:minutes:start -->\nm\n<!-- mm:minutes:end -->\n"
        );
        // Idempotent: writing the same body twice changes nothing.
        assert_eq!(
            replace_notes_section(&out, "新的笔记\n- 要点").unwrap(),
            out
        );
        // Section at the end of the file.
        assert_eq!(
            replace_notes_section("## Notes\nold", "a").unwrap(),
            "## Notes\n\na\n"
        );
        assert!(replace_notes_section("# no section", "a").is_none());
    }

    #[test]
    fn notes_round_trip_with_service_parser() {
        let md = "# T\n\n## 笔记 Notes\n\n\n## 纪要 Minutes\nx\n";
        let out = replace_notes_section(md, "[00:12] 预算 ![[截图.png]]").unwrap();
        assert_eq!(
            mm_service::flow::notes_section(&out),
            "[00:12] 预算 ![[截图.png]]"
        );
    }

    #[test]
    fn formatting_helpers() {
        assert_eq!(timestamp_token(187_000), "[03:07] ");
        assert_eq!(level_fraction(-60.0), 0.0);
        assert_eq!(level_fraction(0.0), 1.0);
        assert_eq!(level_fraction(-30.0), 0.5);
        assert_eq!(level_fraction(f32::NAN), 0.0);
        assert_eq!(snippet("a  b\nc", 10), "a b c");
        assert_eq!(snippet("一二三四五", 3), "一二三…");
        assert_eq!(status_label(MeetingStatus::Processing), "整理中");
        assert_eq!(modality_label("image"), "图片");
        assert_eq!(locator_ms(&serde_json::json!({"start_ms": 5})), Some(5));
        assert_eq!(locator_ms(&serde_json::json!({"title": "x"})), None);
    }

    #[test]
    fn relative_times() {
        let now = Utc::now();
        let ev = |s: i64, e: i64| (now + Duration::minutes(s), now + Duration::minutes(e));
        let (s, e) = ev(8, 60);
        assert_eq!(relative_start(now, s, e), "8 分钟后");
        let (s, e) = ev(-5, 60);
        assert_eq!(relative_start(now, s, e), "进行中");
        let (s, e) = ev(-60, -5);
        assert_eq!(relative_start(now, s, e), "已结束");
        let (s, e) = ev(125, 130);
        assert_eq!(relative_start(now, s, e), "2 小时 5 分钟后");
    }

    fn event(id: &str, start: DateTime<Utc>, mins: i64) -> CalendarEvent {
        CalendarEvent {
            id: id.into(),
            title: id.into(),
            start,
            end: start + Duration::minutes(mins),
            calendar: String::new(),
            location: None,
            join_url: None,
            attendees: vec![],
        }
    }

    #[test]
    fn events_group_by_local_day_and_filter_upcoming() {
        let today = Local::now().date_naive();
        let (from, _) = local_days(today, 1);
        let a = event("a", from + Duration::hours(15), 60);
        let b = event("b", from + Duration::hours(9), 60);
        let c = event("c", from + Duration::hours(24 + 9), 60);
        let groups = group_by_day(&[a.clone(), c.clone(), b.clone()]);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, today);
        assert_eq!(
            groups[0]
                .1
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            ["b", "a"]
        );
        assert!(day_heading(today, today).starts_with("今天"));
        assert!(day_heading(today + Duration::days(1), today).starts_with("明天"));
        let now = from + Duration::hours(12);
        assert_eq!(
            upcoming(&[a, b, c], now)
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "c"]
        );
    }

    #[test]
    fn local_days_span_whole_days() {
        let d = NaiveDate::from_ymd_opt(2026, 11, 5).unwrap();
        let (from, to) = local_days(d, 7);
        assert_eq!((to - from).num_days(), 7);
        assert_eq!(from.with_timezone(&Local).date_naive(), d);
    }

    #[test]
    fn action_rows_flatten_and_limit() {
        let m = |tasks: &[&str]| Minutes {
            title: String::new(),
            summary: String::new(),
            decisions: vec![],
            action_items: tasks
                .iter()
                .map(|t| ActionItem {
                    owner: Some("王伟".into()),
                    task: t.to_string(),
                    due: None,
                    evidence_ms: None,
                })
                .collect(),
            open_questions: vec![],
            topics: vec![],
        };
        let rows = action_rows(
            &[("周会".into(), m(&["a", "b"])), ("评审".into(), m(&["c"]))],
            2,
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].task, "b");
        assert_eq!(rows[0].meeting_title, "周会");
        assert!(parse_minutes(&serde_json::to_value(m(&["x"])).unwrap()).is_some());
        assert!(parse_minutes(&serde_json::json!({"x": 1})).is_none());
    }

    #[test]
    fn provider_config_respects_switches() {
        let mut cfg = AppConfig::default();
        let p = provider_config(&cfg, true, Some("/bin/codex".into()), None);
        assert!(!p.byok_key_present, "no provider configured");
        assert_eq!(p.codex_signed_in.as_deref(), Some("/bin/codex"));
        cfg.use_codex = false;
        cfg.byok = byok_from_form("anthropic", "claude-opus-5-5", "");
        let p = provider_config(&cfg, true, Some("/bin/codex".into()), Some("/m".into()));
        assert!(p.byok_key_present);
        assert!(p.codex_signed_in.is_none());
        assert_eq!(p.local_model.as_deref(), Some("/m"));
    }

    #[test]
    fn byok_form_round_trips() {
        assert!(byok_from_form("", "m", "").is_none());
        assert!(byok_from_form("openai", "  ", "").is_none());
        let p = byok_from_form("openai_compatible", "qwen", " http://x ").unwrap();
        assert_eq!(
            byok_parts(&p),
            ("openai_compatible", "qwen".into(), "http://x".into())
        );
        for (kind, _) in BYOK_KINDS.iter().skip(1) {
            let p = byok_from_form(kind, "m", "u").unwrap();
            assert_eq!(byok_parts(&p).0, *kind);
        }
    }

    #[test]
    fn screenshot_helpers() {
        let p = Path::new("/v/Meetings/附件/a.png");
        assert_eq!(
            screencapture_args(Some(7), p),
            ["-x", "-l", "7", "/v/Meetings/附件/a.png"]
        );
        assert_eq!(
            screencapture_args(None, p),
            ["-x", "/v/Meetings/附件/a.png"]
        );
        assert_eq!(
            attachments_dir(Path::new("/v"), "/Meetings/"),
            PathBuf::from("/v/Meetings/附件")
        );
        let t = Local.with_ymd_and_hms(2026, 11, 5, 14, 3, 12).unwrap();
        assert_eq!(screenshot_name(t), "截图 2026-11-05 140312.png");
    }

    #[test]
    fn default_microphone_prefers_physical_inputs() {
        let input = |uid: &str, is_virtual: bool| mm_core::AudioInput {
            uid: uid.into(),
            name: uid.into(),
            channels: 1,
            sample_rate: 48_000.0,
            is_virtual,
        };
        assert_eq!(
            default_microphone(&[input("v", true), input("m", false)]).as_deref(),
            Some("m")
        );
        assert_eq!(
            default_microphone(&[input("v", true)]).as_deref(),
            Some("v")
        );
        assert_eq!(default_microphone(&[]), None);
    }

    #[test]
    fn seconds_parse() {
        assert_eq!(parse_seconds("12"), Some(12_000));
        assert_eq!(parse_seconds("1.5"), Some(1_500));
        assert_eq!(parse_seconds("1:05"), Some(65_000));
        assert_eq!(parse_seconds(""), None);
        assert_eq!(parse_seconds("-1"), None);
        assert_eq!(parse_seconds("x"), None);
    }

    #[test]
    fn direction_labels_split_the_room_in_thirds() {
        assert_eq!(direction_label(-0.9), "偏左");
        assert_eq!(direction_label(-0.34), "偏左");
        assert_eq!(direction_label(-0.33), "居中");
        assert_eq!(direction_label(0.0), "居中");
        assert_eq!(direction_label(0.33), "居中");
        assert_eq!(direction_label(0.5), "偏右");
        assert_eq!(direction_label(f32::NAN), "居中");
    }

    #[test]
    fn spacing_field_parses_centimetres() {
        assert_eq!(parse_spacing_cm("  "), Ok(None));
        assert_eq!(parse_spacing_cm("14"), Ok(Some(14.0)));
        assert_eq!(parse_spacing_cm("14.5 cm"), Ok(Some(14.5)));
        assert_eq!(parse_spacing_cm("20厘米"), Ok(Some(20.0)));
        assert!(parse_spacing_cm("0").unwrap_err().contains("“0”"));
        assert!(parse_spacing_cm("abc").is_err());
        assert!(parse_spacing_cm("9999").is_err());
        assert_eq!(spacing_text(Some(14.0)), "14");
        assert_eq!(spacing_text(Some(14.5)), "14.5");
        assert_eq!(spacing_text(None), "");
    }

    #[test]
    fn seat_points_stay_on_the_front_half_circle() {
        let (dx, dy) = seat_point(0.0);
        assert!(
            dx.abs() < 1e-6 && (dy - 1.0).abs() < 1e-6,
            "centre is straight ahead"
        );
        let (l, _) = seat_point(-1.0);
        let (r, ry) = seat_point(2.0);
        assert!(l < -0.9 && r > 0.9, "left is left, right is right");
        assert!(ry > 0.2, "the widest seat is still above the baseline");
        assert_eq!(seat_point(f32::INFINITY), seat_point(0.0));
    }

    #[test]
    fn speaker_positions_average_merge_and_skip_noise() {
        let mut p = SpeakerPositions::default();
        let c0 = SpeakerLabel::Cluster { index: 0 };
        let c1 = SpeakerLabel::Cluster { index: 1 };
        assert!(!p.apply(&SpeakerLabel::Unknown, -0.9, 0.9));
        assert!(!p.apply(&c0, -0.9, 0.05), "low confidence ignored");
        assert!(p.is_empty());
        assert!(p.apply(&c0, -0.8, 0.5));
        assert!(p.apply(&c0, -0.6, 0.5));
        assert!(p.apply(&c1, 0.9, 0.5));
        let e = p.entries();
        assert_eq!(e[0].0, c0);
        assert!(
            (e[0].1 + 0.7).abs() < 1e-6,
            "plain mean at first: {}",
            e[0].1
        );
        // Later cues move the average by at least the smoothing floor.
        for _ in 0..20 {
            p.apply(&c0, 0.0, 0.5);
        }
        assert!(direction_label(p.entries()[0].1) == "居中");

        // Known people group across scores; relabel merges by cue count.
        let zhao = PersonId::new();
        let known = |score| SpeakerLabel::Known {
            person_id: zhao,
            score,
        };
        p.apply(&known(0.7), 0.5, 0.5);
        p.apply(&known(0.9), 0.7, 0.5);
        assert_eq!(p.entries().len(), 3);
        p.relabel(&c1, &known(0.8));
        let e = p.entries();
        assert_eq!(e.len(), 2);
        let (_, x) = e.iter().find(|(k, _)| *k == known(0.0)).unwrap();
        assert!((x - (0.6 * 2.0 + 0.9) / 3.0).abs() < 1e-6, "{x}");
        // Relabel into a new label keeps the history.
        p.relabel(&c0, &SpeakerLabel::Cluster { index: 5 });
        assert_eq!(p.entries()[0].0, SpeakerLabel::Cluster { index: 5 });
    }

    #[test]
    fn talk_time_groups_people_across_scores() {
        let p = PersonId::new();
        let line = |speaker: SpeakerLabel, s: u64, e: Option<u64>| CaptionLine {
            utterance_id: s,
            origin: AudioOrigin::Microphone,
            start_ms: s,
            end_ms: e,
            text: "x".into(),
            speaker,
            is_final: e.is_some(),
        };
        let lines = vec![
            line(
                SpeakerLabel::Known {
                    person_id: p,
                    score: 0.8,
                },
                0,
                Some(1000),
            ),
            line(
                SpeakerLabel::Known {
                    person_id: p,
                    score: 0.7,
                },
                2000,
                Some(4000),
            ),
            line(SpeakerLabel::Cluster { index: 1 }, 5000, Some(5500)),
            line(SpeakerLabel::Cluster { index: 1 }, 6000, None),
        ];
        let t = talk_time(&lines);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].1, 3000);
        assert_eq!(t[1], (SpeakerLabel::Cluster { index: 1 }, 500));
    }

    #[test]
    fn privacy_links_follow_the_message() {
        assert!(privacy_settings_url("日历权限：denied").ends_with("Privacy_Calendars"));
        assert!(privacy_settings_url("麦克风权限：x").ends_with("Privacy_Microphone"));
        assert!(privacy_settings_url("屏幕录制权限：x").ends_with("Privacy_ScreenCapture"));
    }
}
