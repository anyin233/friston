//! Pure helpers behind the pages: formatting, filtering and small state transitions.
//!
//! Nothing here touches GPUI, the store or an engine, so every rule the UI relies on
//! is covered by unit tests.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, TimeZone, Utc};
use mm_core::{CalendarEvent, CloudLiveMode, MeetingStatus, SpeakerLabel};
use mm_i18n::{tr, trf};
use mm_llm::{
    AsrMode, AsrProvider, ByokProvider, DEFAULT_GEMINI_LIVE_MODEL, DEFAULT_GEMINI_REFINE_MODEL,
    DEFAULT_OPENAI_LIVE_MODEL, DEFAULT_OPENAI_REFINE_MODEL, Minutes, MinutesBackend, ModelOption,
    ProviderChoice, ProviderConfig, asr_pricing,
};
use mm_service::live::CaptionLine;
use mm_service::{AppConfig, LiveAsrBackend, RefineBackend};

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

/// Label for a meeting status (tags in lists), in the interface language.
///
/// Params: `s` – status.
/// Returns: the label.
pub fn status_label(s: MeetingStatus) -> &'static str {
    match s {
        MeetingStatus::Scheduled => tr!("未开始", "Not started"),
        MeetingStatus::Live => tr!("记录中", "Recording"),
        MeetingStatus::Processing => tr!("整理中", "Processing"),
        MeetingStatus::Done => tr!("纪要已生成", "Minutes ready"),
        MeetingStatus::Cancelled => tr!("已取消", "Cancelled"),
    }
}

/// English count with the noun in the right number, e.g. `1 meeting`, `3 meetings`.
///
/// Chinese needs no plural, so callers use this only in the English arm of `trf!`.
/// Params: `n` – the count; `one` – singular noun; `many` – plural noun.
/// Returns: e.g. `3 meetings`.
pub fn count_en(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
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

/// Day heading such as `11月5日 周四` / `Thu, Nov 5`, with 今天/明天 (Today/Tomorrow)
/// when applicable.
///
/// Params: `day` – the day; `today` – the current local day.
/// Returns: the heading.
pub fn day_heading(day: NaiveDate, today: NaiveDate) -> String {
    const WEEK: [&str; 7] = ["周一", "周二", "周三", "周四", "周五", "周六", "周日"];
    let base = match mm_i18n::current() {
        mm_i18n::Lang::Zh => format!(
            "{}月{}日 {}",
            day.month(),
            day.day(),
            WEEK[day.weekday().num_days_from_monday() as usize]
        ),
        // chrono's `%a`/`%b` are always English without its locale feature, which is
        // exactly what this arm needs.
        mm_i18n::Lang::En => day.format("%a, %b %-d").to_string(),
    };
    match (day - today).num_days() {
        0 => trf!("今天 · {base}", "Today · {base}"),
        1 => trf!("明天 · {base}", "Tomorrow · {base}"),
        _ => base,
    }
}

/// Relative description of an event's timing, e.g. `8 分钟后` / `In 8 min`, `进行中`,
/// `已结束`.
///
/// Params: `now`; `start`; `end`.
/// Returns: the text.
pub fn relative_start(now: DateTime<Utc>, start: DateTime<Utc>, end: DateTime<Utc>) -> String {
    if now >= end {
        return tr!("已结束", "Ended").into();
    }
    if now >= start {
        return tr!("进行中", "In progress").into();
    }
    let mins = (start - now).num_minutes();
    match mins {
        0 => tr!("马上开始", "Starting now").into(),
        1..=59 => trf!("{mins} 分钟后", "In {mins} min"),
        _ => trf!("{} 小时 {} 分钟后", "In {} h {} min", mins / 60, mins % 60),
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

/// Short local date-time for meeting lists (`11月5日 14:00` / `Nov 5, 14:00`).
///
/// Params: `t` – time.
/// Returns: the text.
pub fn short_datetime(t: DateTime<Utc>) -> String {
    let l = t.with_timezone(&Local);
    match mm_i18n::current() {
        mm_i18n::Lang::Zh => format!("{}月{}日 {}", l.month(), l.day(), l.format("%H:%M")),
        mm_i18n::Lang::En => l.format("%b %-d, %H:%M").to_string(),
    }
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

/// How early the sidebar offers to record a calendar meeting before it starts.
pub const RECORD_EVENT_LEAD: Duration = Duration::minutes(10);

/// The calendar meeting the sidebar offers to record: one that is running now or starts
/// within [`RECORD_EVENT_LEAD`].
///
/// All-day and multi-day entries (24 h or longer) are skipped, since they are not meetings
/// you sit in. With back-to-back meetings the one whose start is closest to `now` wins, so
/// a meeting about to begin beats one that is ending.
///
/// Params: `events`; `now`.
/// Returns: the event to offer, if any.
pub fn recordable_event(events: &[CalendarEvent], now: DateTime<Utc>) -> Option<CalendarEvent> {
    events
        .iter()
        .filter(|e| e.end > now && e.start <= now + RECORD_EVENT_LEAD)
        .filter(|e| e.end - e.start < Duration::hours(24))
        .min_by_key(|e| (e.start - now).num_seconds().abs())
        .cloned()
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

/// Label for a search modality, in the interface language.
///
/// Params: `m` – `text`, `image`, `audio` or `video`.
/// Returns: the label.
pub fn modality_label(m: &str) -> &'static str {
    match m {
        "image" => tr!("图片", "Image"),
        "audio" => tr!("音频", "Audio"),
        "video" => tr!("视频", "Video"),
        _ => tr!("文字", "Text"),
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
        backend: cfg.minutes_backend,
        byok: cfg.byok.clone(),
        byok_key_present: cfg.byok.is_some() && key_present,
        codex_signed_in: codex,
        codex_in_auto: cfg.use_codex,
        codex_model: cfg.codex_model.clone(),
        local_model,
    }
}

/// Names a minutes provider for progress messages and notices.
///
/// Params: `choice`.
/// Returns: e.g. "Codex（ChatGPT · gpt-6.1-terra）", in the current UI language.
pub fn provider_label(choice: &ProviderChoice) -> String {
    match choice {
        ProviderChoice::Byok(p) => {
            trf!(
                "API 密钥（{}）",
                "API key ({})",
                mm_llm::byok::keychain_account(p)
            )
        }
        ProviderChoice::Codex { model: None, .. } => {
            tr!("Codex（ChatGPT）", "Codex (ChatGPT)").to_string()
        }
        ProviderChoice::Codex { model: Some(m), .. } => {
            trf!("Codex（ChatGPT · {m}）", "Codex (ChatGPT · {m})")
        }
        ProviderChoice::LocalMlx { .. } => tr!("本地模型", "Local model").to_string(),
    }
}

/// Options for a model picker.
///
/// Params: `listed` – what the CLI reported, or `None` before it answered; `fallback` – shown
/// while `listed` is `None`; `keep` – which ids to offer; `saved` – the configured id, kept
/// even when the CLI no longer lists it so the picker never shows a blank selection;
/// `default_entry` – label of an extra "use the CLI's default" entry with the value `""`.
/// Returns: `(id, label)` pairs in display order.
pub fn model_choices(
    listed: Option<&[ModelOption]>,
    fallback: &[(&str, &str)],
    keep: fn(&str) -> bool,
    saved: &str,
    default_entry: Option<&str>,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = default_entry
        .map(|l| (String::new(), l.to_string()))
        .into_iter()
        .collect();
    match listed {
        Some(models) => out.extend(
            models
                .iter()
                .filter(|m| keep(&m.id))
                .map(|m| (m.id.clone(), m.label.clone())),
        ),
        None => out.extend(
            fallback
                .iter()
                .filter(|(id, _)| keep(id))
                .map(|(id, l)| (id.to_string(), l.to_string())),
        ),
    }
    if !saved.is_empty() && !out.iter().any(|(id, _)| id == saved) {
        out.push((saved.to_string(), saved.to_string()));
    }
    out
}

/// Names a minutes backend in settings and notices.
///
/// Params: `b`.
/// Returns: the label in the current UI language.
pub fn minutes_backend_label(b: MinutesBackend) -> &'static str {
    match b {
        MinutesBackend::Auto => tr!("自动", "Automatic"),
        MinutesBackend::ApiKey => tr!("API 密钥", "API key"),
        MinutesBackend::Codex => "Codex",
    }
}

/// Names a live caption ASR backend in settings and status lights.
///
/// Params: `b`.
/// Returns: localized label.
pub fn live_asr_backend_label(b: LiveAsrBackend) -> &'static str {
    match b {
        LiveAsrBackend::Local => tr!("本地 Qwen3-ASR", "Local Qwen3-ASR"),
        LiveAsrBackend::OpenAi => "OpenAI",
        LiveAsrBackend::Gemini => "Google Gemini",
    }
}

/// Names a cloud live upload strategy in settings and status lights.
///
/// Params: `mode`.
/// Returns: localized label.
pub fn cloud_live_mode_label(mode: CloudLiveMode) -> &'static str {
    match mode {
        CloudLiveMode::Streaming => tr!("实时流式（VAD 按需）", "Realtime stream (VAD-gated)"),
        CloudLiveMode::Utterance => tr!("按句上传（省流低成本）", "Per-utterance (low cost)"),
    }
}

/// Names a post-meeting refine backend in settings and notices.
///
/// Params: `b`.
/// Returns: localized label.
pub fn refine_backend_label(b: RefineBackend) -> &'static str {
    match b {
        RefineBackend::Local => tr!("本地 Qwen3-ASR", "Local Qwen3-ASR"),
        RefineBackend::Codex => tr!("Codex 一体化", "Codex (transcribe + minutes)"),
        RefineBackend::OpenAi => tr!("OpenAI 转写", "OpenAI Audio"),
        RefineBackend::Gemini => "Google Gemini",
    }
}

/// Effective live ASR model identifier for cloud backends (falling back to the mode-matched default).
///
/// Params: `cfg`.
/// Returns: model id string (empty for `Local`).
pub fn effective_live_model(cfg: &AppConfig) -> String {
    match (cfg.live_asr_backend, cfg.live_cloud_mode) {
        (LiveAsrBackend::Local, _) => String::new(),
        (LiveAsrBackend::OpenAi, CloudLiveMode::Streaming) => cfg
            .live_openai_model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_OPENAI_LIVE_MODEL)
            .to_string(),
        (LiveAsrBackend::OpenAi, CloudLiveMode::Utterance) => cfg
            .live_openai_utterance_model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_OPENAI_REFINE_MODEL)
            .to_string(),
        (LiveAsrBackend::Gemini, CloudLiveMode::Streaming) => cfg
            .live_gemini_model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_GEMINI_LIVE_MODEL)
            .to_string(),
        (LiveAsrBackend::Gemini, CloudLiveMode::Utterance) => cfg
            .live_gemini_utterance_model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_GEMINI_REFINE_MODEL)
            .to_string(),
    }
}

/// Effective post-meeting cloud refine model identifier (falling back to the default).
///
/// Params: `cfg`.
/// Returns: model id string (empty for `Local` / `Codex`).
pub fn effective_refine_model(cfg: &AppConfig) -> String {
    match cfg.refine_backend {
        RefineBackend::OpenAi => cfg
            .refine_openai_model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_OPENAI_REFINE_MODEL)
            .to_string(),
        RefineBackend::Gemini => cfg
            .refine_gemini_model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_GEMINI_REFINE_MODEL)
            .to_string(),
        RefineBackend::Local | RefineBackend::Codex => String::new(),
    }
}

/// Start of `now`'s local calendar month converted to UTC.
///
/// Params: `now` – current local timestamp.
/// Returns: UTC instant corresponding to local `YYYY-MM-01 00:00:00`.
pub fn local_month_start_utc(now: DateTime<Local>) -> DateTime<Utc> {
    let first_day = NaiveDate::from_ymd_opt(now.year(), now.month(), 1).expect("day 1 exists");
    let naive = first_day
        .and_hms_opt(0, 0, 0)
        .expect("midnight exists on day 1");
    Local
        .from_local_datetime(&naive)
        .earliest()
        .unwrap_or(now)
        .with_timezone(&Utc)
}

/// Formats a USD amount (`$0.0045` for sub-cent unit prices, `$1.23` otherwise).
///
/// Params: `usd`.
/// Returns: formatted currency string.
pub fn format_usd(usd: f64) -> String {
    if !usd.is_finite() || usd <= 0.0 {
        return "$0.00".to_string();
    }
    if usd < 0.05 {
        format!("${usd:.4}")
    } else {
        format!("${usd:.2}")
    }
}

/// Formats processed audio duration in minutes / hours in the UI language.
///
/// Params: `audio_ms`.
/// Returns: e.g. `12 分钟` / `2 小时 15 分钟` or `12 min` / `2 h 15 min`.
pub fn format_duration_minutes(audio_ms: u64) -> String {
    let total_secs = audio_ms / 1000;
    if total_secs == 0 {
        return tr!("0 分钟", "0 min").to_string();
    }
    if total_secs < 60 {
        return trf!("< 1 分钟", "< 1 min");
    }
    let total_mins = (audio_ms + 30_000) / 60_000;
    if total_mins < 60 {
        trf!("{total_mins} 分钟", "{total_mins} min")
    } else {
        let h = total_mins / 60;
        let m = total_mins % 60;
        trf!("{h} 小时 {m} 分钟", "{h} h {m} min")
    }
}

/// Localized pricing reference string (`$0.0045/分钟 · 每 $100 约可转写 370 小时`).
///
/// Params: `provider`; `mode`; `model`.
/// Returns: formatted rate hint.
pub fn model_rate_hint(provider: AsrProvider, mode: AsrMode, model: &str) -> String {
    let clean = model.trim();
    if clean.is_empty() || asr_pricing::known_model_spec(provider, mode, clean).is_some() {
        let spec = asr_pricing::model_spec(provider, mode, clean);
        let rate = format_usd(spec.usd_per_minute);
        let hours_per_100 =
            (asr_pricing::minutes_for_usd(provider, mode, clean, 100.0) / 60.0).round() as u64;
        trf!(
            "{rate}/分钟 · 每 $100 约可转写 {hours_per_100} 小时",
            "{rate}/min · ~{hours_per_100} h per $100"
        )
    } else {
        let rate = format_usd(asr_pricing::model_spec(provider, mode, clean).usd_per_minute);
        trf!(
            "未收录该模型单价（估算按默认参考价 {rate}/分钟）",
            "Model pricing not listed (estimated at default {rate}/min)"
        )
    }
}

/// BYOK provider kinds offered in settings (`""` = none). A function rather than a
/// `const` because the labels follow the interface language.
///
/// Returns: `(kind, label)` pairs in display order.
pub fn byok_kinds() -> [(&'static str, &'static str); 5] {
    [
        ("", tr!("不使用", "None")),
        ("openai", "OpenAI"),
        ("anthropic", "Anthropic"),
        ("gemini", "Gemini"),
        ("openai_compatible", tr!("OpenAI 兼容", "OpenAI compatible")),
    ]
}

/// Builds a provider from the settings form.
///
/// Params: `kind` – one of [`byok_kinds`]; `model`; `base_url` – compatible endpoints only.
/// Returns: the provider, or `None` for "不使用" (none) or an empty model.
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
/// Returns: `偏左`, `居中` or `偏右` (`Left`, `Center`, `Right` in English).
pub fn direction_label(x: f32) -> &'static str {
    if x < -DIRECTION_CENTRE_BAND {
        tr!("偏左", "Left")
    } else if x > DIRECTION_CENTRE_BAND {
        tr!("偏右", "Right")
    } else {
        tr!("居中", "Center")
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
/// Returns: `Ok(None)` for blank, `Ok(Some(cm))` for 1–500 cm, `Err` with a message in the
/// interface language otherwise.
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
        _ => Err(trf!(
            "麦克风间距要填 1 到 500 之间的厘米数，收到的是“{}”",
            "Mic spacing must be 1 to 500 cm; got “{}”",
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
/// Returns: e.g. `正在下载 o/m：620 MB / 1.5 GB（41%），第 9/15 个文件` /
/// `Downloading o/m: 620 MB / 1.5 GB (41%), file 9 of 15`.
pub fn download_line(repo: &str, p: &mm_service::hub::Progress) -> String {
    if p.files_total == 0 {
        return trf!(
            "正在获取 {repo} 的文件列表…",
            "Getting the file list for {repo}…"
        );
    }
    let percent = (p.done_bytes * 100)
        .checked_div(p.total_bytes)
        .map(|pct| trf!("（{pct}%）", " ({pct}%)"))
        .unwrap_or_default();
    trf!(
        "正在下载 {repo}：{} / {}{percent}，第 {}/{} 个文件",
        "Downloading {repo}: {} / {}{percent}, file {} of {}",
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

/// Caption language choices. A function rather than a `const` because the labels follow
/// the interface language.
///
/// Returns: `(config value, label)` pairs; `None` = auto.
pub fn caption_languages() -> [(Option<&'static str>, &'static str); 4] {
    [
        (Some("zh"), tr!("中文", "Chinese")),
        (Some("en"), "English"),
        (Some("ja"), tr!("日本語", "Japanese")),
        (None, tr!("自动识别", "Auto-detect")),
    ]
}

/// Minutes language choices, labelled in the interface language.
///
/// Returns: `(config value, label)` pairs.
pub fn minutes_languages() -> [(&'static str, &'static str); 3] {
    [
        ("zh", tr!("中文", "Chinese")),
        ("en", "English"),
        ("ja", tr!("日本語", "Japanese")),
    ]
}

/// System Settings deep link for a permission error message.
///
/// Params: `message` – error text from the service layer, in either interface language.
/// Returns: the `x-apple.systempreferences:` URL.
pub fn privacy_settings_url(message: &str) -> &'static str {
    // The service may word the error in Chinese or English, so match both.
    let lower = message.to_lowercase();
    if message.contains("日历") || lower.contains("calendar") {
        "x-apple.systempreferences:com.apple.preference.security?Privacy_Calendars"
    } else if message.contains("麦克风") || lower.contains("microphone") {
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
    fn recordable_event_picks_running_or_imminent_meeting() {
        let now = Utc::now();
        let m = Duration::minutes;
        let running = event("running", now - m(20), 60);
        let soon = event("soon", now + m(8), 30);
        let later = event("later", now + m(11), 30);
        let ended = event("ended", now - m(60), 30);
        let all_day = event("all-day", now - m(60), 24 * 60);
        assert_eq!(recordable_event(&[], now), None);
        assert_eq!(recordable_event(&[later.clone(), ended.clone()], now), None);
        assert_eq!(recordable_event(std::slice::from_ref(&all_day), now), None);
        let pick = |evs: &[CalendarEvent]| recordable_event(evs, now).map(|e| e.id);
        assert_eq!(pick(&[running.clone(), all_day]), Some("running".into()));
        assert_eq!(pick(&[later.clone(), soon.clone()]), Some("soon".into()));
        // Back to back: the meeting about to start beats the one that is winding down.
        assert_eq!(pick(&[running, soon, later]), Some("soon".into()));
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
        assert!(p.codex_in_auto);
        cfg.use_codex = false;
        cfg.byok = byok_from_form("anthropic", "claude-opus-5-5", "");
        cfg.minutes_backend = MinutesBackend::Codex;
        cfg.codex_model = Some("gpt-6.2-sol".into());
        let p = provider_config(&cfg, true, Some("/bin/codex".into()), Some("/m".into()));
        assert!(p.byok_key_present);
        // The switch only keeps Codex out of the automatic chain; an explicit pick still works.
        assert!(!p.codex_in_auto);
        assert_eq!(p.codex_signed_in.as_deref(), Some("/bin/codex"));
        assert_eq!(p.local_model.as_deref(), Some("/m"));
        assert!(matches!(
            mm_llm::select_provider(&p),
            Ok(ProviderChoice::Codex {
                model: Some(ref m),
                ..
            }) if m == "gpt-6.2-sol"
        ));
    }

    #[test]
    fn model_choices_keep_the_saved_model_and_filter() {
        let listed = vec![
            ModelOption {
                id: "gpt-6.2-sol".into(),
                label: "GPT-6.2-Sol".into(),
                is_default: true,
            },
            ModelOption {
                id: "gpt-6.1-terra".into(),
                label: "GPT-6.1 Terra".into(),
                is_default: false,
            },
        ];
        let all = |_: &str| true;
        let only_6_2 = |id: &str| id.starts_with("gpt-6.2");
        let ids = |v: Vec<(String, String)>| v.into_iter().map(|(id, _)| id).collect::<Vec<_>>();
        assert_eq!(
            ids(model_choices(
                Some(&listed),
                &[],
                all,
                "gone-model",
                Some("Default")
            )),
            ["", "gpt-6.2-sol", "gpt-6.1-terra", "gone-model"]
        );
        assert_eq!(
            ids(model_choices(
                Some(&listed),
                &[],
                only_6_2,
                "gpt-6.2-sol",
                None
            )),
            ["gpt-6.2-sol"]
        );
        // Before the CLI answers, the built-in list stands in.
        assert_eq!(
            ids(model_choices(None, &[("a", "A")], all, "", None)),
            ["a"]
        );
    }

    #[test]
    fn provider_labels_name_the_model() {
        mm_i18n::with_lang(mm_i18n::Lang::En, || {
            let codex = |m: Option<&str>| ProviderChoice::Codex {
                codex_path: "/c".into(),
                model: m.map(String::from),
            };
            assert_eq!(provider_label(&codex(None)), "Codex (ChatGPT)");
            assert_eq!(
                provider_label(&codex(Some("gpt-6-luna"))),
                "Codex (ChatGPT · gpt-6-luna)"
            );
            assert_eq!(minutes_backend_label(MinutesBackend::Auto), "Automatic");
        });
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
        for (kind, _) in byok_kinds().iter().skip(1) {
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
            stable_chars: 1,
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

#[cfg(test)]
mod english_tests {
    use super::*;
    use mm_i18n::{Lang, with_lang};
    use mm_service::hub::Progress;

    #[test]
    fn day_headings_and_dates_read_naturally_in_english() {
        let day = NaiveDate::from_ymd_opt(2026, 11, 5).unwrap();
        with_lang(Lang::En, || {
            assert_eq!(day_heading(day, day), "Today · Thu, Nov 5");
            assert_eq!(
                day_heading(day, day - Duration::days(1)),
                "Tomorrow · Thu, Nov 5"
            );
            assert_eq!(day_heading(day, day - Duration::days(3)), "Thu, Nov 5");
            let t = Local.with_ymd_and_hms(2026, 11, 8, 9, 5, 0).unwrap();
            assert_eq!(short_datetime(t.with_timezone(&Utc)), "Nov 8, 09:05");
        });
        // The Chinese wording is untouched.
        assert_eq!(day_heading(day, day - Duration::days(3)), "11月5日 周四");
    }

    #[test]
    fn relative_times_and_counts_in_english() {
        let now = Utc::now();
        let at = |m: i64| now + Duration::minutes(m);
        with_lang(Lang::En, || {
            assert_eq!(relative_start(now, at(8), at(60)), "In 8 min");
            assert_eq!(relative_start(now, at(125), at(130)), "In 2 h 5 min");
            assert_eq!(relative_start(now, at(-5), at(60)), "In progress");
            assert_eq!(relative_start(now, at(-60), at(-5)), "Ended");
            assert_eq!(status_label(MeetingStatus::Done), "Minutes ready");
            assert_eq!(direction_label(0.0), "Center");
            assert_eq!(byok_kinds()[0].1, "None");
            assert_eq!(caption_languages()[3], (None, "Auto-detect"));
        });
        assert_eq!(count_en(1, "meeting", "meetings"), "1 meeting");
        assert_eq!(count_en(3, "meeting", "meetings"), "3 meetings");
        assert_eq!(count_en(0, "meeting", "meetings"), "0 meetings");
    }

    #[test]
    fn download_progress_and_errors_in_english() {
        let p = Progress {
            file: "model.safetensors".into(),
            done_bytes: 620_000_000,
            total_bytes: 1_500_000_000,
            files_done: 8,
            files_total: 15,
        };
        with_lang(Lang::En, || {
            assert_eq!(
                download_line("o/m", &Progress::default()),
                "Getting the file list for o/m…"
            );
            assert_eq!(
                download_line("o/m", &p),
                "Downloading o/m: 620 MB / 1.5 GB (41%), file 9 of 15"
            );
            assert_eq!(
                parse_spacing_cm("0").unwrap_err(),
                "Mic spacing must be 1 to 500 cm; got “0”"
            );
        });
        assert!(privacy_settings_url("Calendar access: denied").ends_with("Privacy_Calendars"));
        assert!(privacy_settings_url("Microphone permission: x").ends_with("Privacy_Microphone"));
    }

    #[test]
    fn spend_formatting_month_start_and_rate_hints_work_in_both_languages() {
        let now = Local.with_ymd_and_hms(2026, 11, 19, 16, 30, 0).unwrap();
        let start_utc = local_month_start_utc(now);
        assert_eq!(start_utc.with_timezone(&Local).day(), 1);
        assert_eq!(start_utc.with_timezone(&Local).month(), 11);

        assert_eq!(format_usd(0.0), "$0.00");
        assert_eq!(format_usd(0.0045), "$0.0045");
        assert_eq!(format_usd(12.345), "$12.35");

        assert_eq!(format_duration_minutes(0), "0 分钟");
        assert_eq!(format_duration_minutes(15_000), "< 1 分钟");
        assert_eq!(format_duration_minutes(12 * 60_000), "12 分钟");
        assert_eq!(format_duration_minutes(135 * 60_000), "2 小时 15 分钟");
        let hint_zh = model_rate_hint(AsrProvider::OpenAi, AsrMode::Refine, "gpt-transcribe");
        assert!(
            hint_zh.contains("$0.0045/分钟") && hint_zh.contains("370 小时"),
            "{hint_zh}"
        );
        let snap_zh = model_rate_hint(
            AsrProvider::OpenAi,
            AsrMode::Refine,
            "gpt-4o-mini-transcribe-2025-12-15",
        );
        assert!(snap_zh.contains("$0.0030/分钟"), "{snap_zh}");
        let custom_zh = model_rate_hint(AsrProvider::OpenAi, AsrMode::Refine, "gpt-9-transcribe");
        assert!(custom_zh.contains("未收录该模型单价"), "{custom_zh}");

        with_lang(Lang::En, || {
            assert_eq!(format_duration_minutes(135 * 60_000), "2 h 15 min");
            let hint_en = model_rate_hint(
                AsrProvider::Gemini,
                AsrMode::Live,
                "gemini-3.5-transcribe-live",
            );
            assert!(
                hint_en.contains("$0.0090/min") && hint_en.contains("185 h per $100"),
                "{hint_en}"
            );
            let custom_en =
                model_rate_hint(AsrProvider::OpenAi, AsrMode::Refine, "gpt-9-transcribe");
            assert!(
                custom_en.contains("Model pricing not listed"),
                "{custom_en}"
            );
            assert_eq!(live_asr_backend_label(LiveAsrBackend::OpenAi), "OpenAI");
            assert_eq!(
                cloud_live_mode_label(CloudLiveMode::Streaming),
                "Realtime stream (VAD-gated)"
            );
            assert_eq!(
                cloud_live_mode_label(CloudLiveMode::Utterance),
                "Per-utterance (low cost)"
            );
            assert_eq!(refine_backend_label(RefineBackend::Gemini), "Google Gemini");
        });

        let mut cfg = AppConfig {
            live_asr_backend: LiveAsrBackend::OpenAi,
            live_cloud_mode: CloudLiveMode::Streaming,
            ..Default::default()
        };
        assert_eq!(effective_live_model(&cfg), DEFAULT_OPENAI_LIVE_MODEL);
        cfg.live_cloud_mode = CloudLiveMode::Utterance;
        assert_eq!(effective_live_model(&cfg), DEFAULT_OPENAI_REFINE_MODEL);
        cfg.live_openai_utterance_model = Some("gpt-4o-mini-transcribe".into());
        assert_eq!(effective_live_model(&cfg), "gpt-4o-mini-transcribe");
    }
}
