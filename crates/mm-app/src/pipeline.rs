//! Blocking multi-step jobs run on the background executor: post-meeting processing,
//! minutes generation and provider detection.
//!
//! Each step reports progress through a callback so the UI can show where it is, and a
//! failing step never discards what earlier steps produced (the recording, the live
//! transcript).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use mm_core::{AudioOrigin, CaptureFiles, CaptureStarted, CaptureStopped, FileTranscript, Meeting};
use mm_i18n::{tr, trf};
use mm_llm::{
    AsrMode, AsrProvider, AudioTrack, DEFAULT_GEMINI_REFINE_MODEL, DEFAULT_OPENAI_REFINE_MODEL,
    GeminiTranscribeOptions, LlmError, MinutesBackend, OpenAiTranscribeOptions, ProviderChoice,
    asr_pricing, byok, codex, gemini_asr, openai_asr,
};
use mm_service::engines::{
    EmbedEngine, MEETINGD, MeetingEngine, RefineRequest, RefineWorker, locate_engine,
};
use mm_service::{AppConfig, RefineBackend, external, flow};
use mm_store::{AsrUsageRecord, Store};

use crate::logic::{self, provider_config, provider_label};
use crate::writer::{LiveWriter, lock};

/// Result of probing the local Codex CLI.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CodexStatus {
    /// Executable, if found.
    pub path: Option<PathBuf>,
    pub signed_in: bool,
    /// Account label (email/plan) when signed in.
    pub label: Option<String>,
    /// Why the probe failed, if it did.
    pub error: Option<String>,
}

impl CodexStatus {
    /// Path usable for minutes (only when signed in).
    ///
    /// Returns: the path as a string.
    pub fn usable_path(&self) -> Option<String> {
        self.signed_in
            .then(|| self.path.as_ref().map(|p| p.display().to_string()))
            .flatten()
    }
}

/// Finds Codex and asks whether it is signed in. Blocks for a few seconds.
///
/// Returns: the status (never fails; errors are recorded in it).
pub fn detect_codex() -> CodexStatus {
    let Some(path) = codex::find_codex() else {
        return CodexStatus::default();
    };
    match codex::read_account(&path) {
        Ok(acc) => CodexStatus {
            path: Some(path),
            signed_in: acc.signed_in,
            label: acc.label,
            error: None,
        },
        Err(e) => CodexStatus {
            error: Some(trf!(
                "{} account/read：{e}",
                "{} account/read: {e}",
                path.display()
            )),
            path: Some(path),
            signed_in: false,
            label: None,
        },
    }
}

/// Everything minutes generation needs, cloned so it can move to a background thread.
#[derive(Clone)]
pub struct MinutesCtx {
    pub cfg: AppConfig,
    pub store: Arc<Mutex<Store>>,
    /// Cached Codex probe; `None` probes again.
    pub codex: Option<CodexStatus>,
    /// Embed engine for the optional on-device LLM.
    pub embed: Option<EmbedEngine>,
}

/// Loads the local LLM if configured and returns its folder when ready.
///
/// Params: `ctx`.
/// Returns: the loaded model folder, or `None` (logged) when unavailable.
fn local_model(ctx: &MinutesCtx) -> Option<String> {
    let embed = ctx.embed.as_ref()?;
    if let Ok(s) = embed.llm_status()
        && s.loaded
    {
        return s.directory.or_else(|| {
            ctx.cfg
                .local_llm_dir
                .as_ref()
                .map(|d| d.display().to_string())
        });
    }
    let dir = ctx.cfg.local_llm_dir.as_ref()?;
    match embed.llm_load(dir) {
        Ok(s) if s.loaded => Some(dir.display().to_string()),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!("loading local LLM {}: {e}", dir.display());
            None
        }
    }
}

/// Minutes that were written, and by whom.
#[derive(Debug, Clone, PartialEq)]
pub struct MinutesWritten {
    /// Provider that wrote them, as [`provider_label`] names it.
    pub label: String,
    /// The picked provider could not run or failed before the fallback wrote the minutes.
    pub warnings: Vec<String>,
}

/// Why an explicitly picked minutes backend could not run at all.
///
/// Params: `b` – the pick.
/// Returns: a short reason in the UI language.
fn unavailable_reason(b: MinutesBackend) -> &'static str {
    match b {
        MinutesBackend::Auto => "",
        MinutesBackend::ApiKey => tr!("没有保存 API 密钥", "no API key is saved"),
        MinutesBackend::Codex => tr!("Codex 未安装或未登录", "Codex isn’t installed or signed in"),
    }
}

/// Generates minutes with the configured provider and writes them into store and note.
///
/// An explicitly picked provider that fails hands over to the automatic chain once; the
/// handover is reported in [`MinutesWritten::warnings`].
///
/// Params: `ctx`; `meeting` – updated in place (status Done on success).
/// Returns: who wrote the minutes, or an error message with context.
pub fn generate_minutes(ctx: &MinutesCtx, meeting: &mut Meeting) -> Result<MinutesWritten, String> {
    let key_present = match &ctx.cfg.byok {
        Some(p) => byok::read_key(p)
            .map_err(|e| trf!("读取 API 密钥失败：{e}", "Couldn’t read the API key: {e}"))?
            .is_some(),
        None => false,
    };
    let backend = ctx.cfg.minutes_backend;
    let codex = match &ctx.codex {
        Some(c) if c.signed_in => c.usable_path(),
        _ if ctx.cfg.use_codex || backend == MinutesBackend::Codex => detect_codex().usable_path(),
        _ => None,
    };
    let local = local_model(ctx);
    let plan = mm_llm::plan_providers(&provider_config(&ctx.cfg, key_present, codex, local))
        .map_err(|_| {
            tr!(
                "没有可用的纪要生成方式：请在设置中添加 API 密钥、运行 codex login 登录 ChatGPT，或加载本地纪要模型。",
                "No way to write minutes: add an API key in Settings, run codex login to sign in to ChatGPT, or load a local minutes model."
            )
            .to_string()
        })?;
    let mut warnings = Vec::new();
    if let Some(b) = plan.unavailable {
        warnings.push(trf!(
            "纪要方式“{}”用不了（{}），改用{}",
            "Couldn’t use {} for minutes ({}), so {} wrote them",
            logic::minutes_backend_label(b),
            unavailable_reason(b),
            provider_label(&plan.primary)
        ));
    }
    let input = flow::minutes_input(&ctx.cfg, &*lock(&ctx.store)?, meeting).map_err(|e| {
        trf!(
            "整理纪要素材失败：{e}",
            "Couldn’t gather material for the minutes: {e}"
        )
    })?;
    let embed = ctx.embed.clone();
    let run_local = move |_: &str, input: &mm_llm::MinutesInput| match &embed {
        Some(e) => e.local_minutes(input),
        None => Err(LlmError::Local(
            tr!("本地模型尚未配置", "No local model is set up").into(),
        )),
    };
    let run = |choice: &ProviderChoice| {
        let label = provider_label(choice);
        mm_llm::generate_minutes(choice, &input, &run_local)
            .map(|m| (label.clone(), m))
            .map_err(|e| {
                trf!(
                    "{label} 生成纪要失败：{e}",
                    "Couldn’t write minutes with {label}: {e}"
                )
            })
    };
    let (label, minutes) = match (run(&plan.primary), &plan.fallback) {
        (Ok(done), _) => done,
        (Err(e), Some(fallback)) => {
            warnings.push(trf!(
                "{e}，改用{}",
                "{e}. Used {} instead",
                provider_label(fallback)
            ));
            run(fallback)?
        }
        (Err(e), None) => return Err(e),
    };
    flow::apply_minutes(&ctx.cfg, &mut *lock(&ctx.store)?, meeting, &minutes)
        .map_err(|e| trf!("写入纪要失败：{e}", "Couldn’t save the minutes: {e}"))?;
    Ok(MinutesWritten { label, warnings })
}

/// Inputs of the whole post-meeting job in one go (tests; the app queues the two phases).
#[cfg(test)]
pub struct FinishJob {
    pub minutes: MinutesCtx,
    pub engine: Option<MeetingEngine>,
    pub writer: LiveWriter,
    pub meeting: Meeting,
    pub started: CaptureStarted,
    /// Elapsed time, used when the engine cannot report the duration.
    pub elapsed_ms: u64,
}

/// Inputs of the stop phase: everything that must finish before the next recording starts.
pub struct StopJob {
    pub store: Arc<Mutex<Store>>,
    pub cfg: AppConfig,
    pub engine: Option<MeetingEngine>,
    pub writer: LiveWriter,
    pub meeting: Meeting,
    pub started: CaptureStarted,
    /// Elapsed time, used when the engine cannot report the duration.
    pub elapsed_ms: u64,
}

/// A stopped meeting waiting for refine and minutes.
#[derive(Debug, Clone)]
pub struct StoppedMeeting {
    pub meeting: Meeting,
    /// Recorded files (stop-time report, falling back to those announced at start).
    pub files: CaptureFiles,
    /// Non-fatal problems of the stop phase, in order.
    pub warnings: Vec<String>,
}

/// What the post-meeting job achieved.
#[derive(Debug, Clone)]
pub struct FinishReport {
    pub meeting: Meeting,
    /// Non-fatal problems, in order.
    pub warnings: Vec<String>,
    /// Whether minutes were written.
    pub minutes_done: bool,
}

/// Stops capture and runs refine → transcript sync → minutes in one go. The app runs the two
/// phases separately ([`stop_meeting`], then [`process_meeting`] from the queue).
///
/// Params: `job`; `progress` – called with a step description in the UI language.
/// Returns: the report; only a missing store makes it fail.
#[cfg(test)]
pub fn finish_meeting(job: FinishJob, progress: &dyn Fn(String)) -> Result<FinishReport, String> {
    let FinishJob {
        minutes,
        engine,
        writer,
        meeting,
        started,
        elapsed_ms,
    } = job;
    let stopped = stop_meeting(
        StopJob {
            store: minutes.store.clone(),
            cfg: minutes.cfg.clone(),
            engine: engine.clone(),
            writer,
            meeting,
            started,
            elapsed_ms,
        },
        progress,
    )?;
    // Like the app: the refine gets its own process; without a recording engine (unit tests)
    // there is none to start either.
    let refine_engine = || match engine {
        Some(_) => spawn_refine_worker(),
        None => Err(tr!("字幕引擎不在运行", "the caption engine isn’t running").into()),
    };
    process_meeting(&minutes, refine_engine, stopped, progress)
}

/// Starts the dedicated refine process (see [`RefineWorker`]).
///
/// Returns: the worker, or why it could not start (UI language).
pub fn spawn_refine_worker() -> Result<RefineWorker, String> {
    let path = locate_engine(MEETINGD)
        .ok_or_else(|| trf!("找不到 {MEETINGD}", "{MEETINGD} wasn’t found"))?;
    RefineWorker::spawn(&path).map_err(|e| e.to_string())
}

/// Stop phase: capture/stop → mark stopped → save the live transcript. Short, and the only
/// part that has to finish before the recorder can start the next meeting.
///
/// Params: `job`; `progress` – called with a step description in the UI language.
/// Returns: the stopped meeting for [`process_meeting`]; only a store failure is an error.
pub fn stop_meeting(job: StopJob, progress: &dyn Fn(String)) -> Result<StoppedMeeting, String> {
    let StopJob {
        store,
        cfg,
        engine,
        writer,
        mut meeting,
        started,
        elapsed_ms,
    } = job;
    let mut warnings = Vec::new();

    progress(tr!("正在停止录制…", "Stopping recording…").into());
    let stopped = match engine.as_ref().map(|e| e.capture_stop()) {
        Some(Ok(s)) => s,
        other => {
            let why = match other {
                Some(Err(e)) => e.to_string(),
                _ => tr!("字幕引擎不在运行", "the caption engine isn’t running").into(),
            };
            warnings.push(trf!(
                "停止录制时出错（{why}），已保留录音文件夹 {}",
                "Couldn’t stop recording cleanly ({why}); kept the recording folder {}",
                started.directory.display()
            ));
            CaptureStopped {
                session_id: started.session_id.clone(),
                duration_ms: elapsed_ms,
                files: started.files.clone(),
            }
        }
    };
    flow::mark_stopped(&*lock(&store)?, &mut meeting, &stopped).map_err(|e| {
        trf!(
            "更新会议状态失败：{e}",
            "Couldn’t update the meeting status: {e}"
        )
    })?;

    progress(tr!("正在写入实时转写…", "Saving the live transcript…").into());
    writer.flush_blocking();
    if let Err(e) = flow::sync_transcript(&cfg, &mut *lock(&store)?, &meeting) {
        warnings.push(trf!(
            "同步转写到笔记失败：{e}",
            "Couldn’t sync the transcript to the note: {e}"
        ));
    }
    Ok(StoppedMeeting {
        meeting,
        files: merged_files(&stopped.files, &started.files),
        warnings,
    })
}

/// Processing phase: refine → transcript sync → minutes. Runs from the post-meeting queue,
/// possibly while the next meeting is recording.
///
/// The local refine runs in its own engine process, started by `refine_engine` only when
/// that pass runs, so live captions of a meeting recording meanwhile never wait for it.
///
/// Params: `minutes` – settings, store and minutes providers (read when the job starts, so
/// settings changed while it waited apply); `refine_engine` – starts the refine process
/// (normally [`spawn_refine_worker`]); `stopped` – from [`stop_meeting`]; `progress` – step
/// descriptions in the UI language.
/// Returns: the report (stop-phase warnings first); never fails today, kept fallible for
/// symmetry with the stop phase.
pub fn process_meeting(
    minutes: &MinutesCtx,
    refine_engine: impl FnOnce() -> Result<RefineWorker, String>,
    stopped: StoppedMeeting,
    progress: &dyn Fn(String),
) -> Result<FinishReport, String> {
    let StoppedMeeting {
        mut meeting,
        files,
        mut warnings,
    } = stopped;
    let cfg = &minutes.cfg;
    let mut minutes_done = false;
    // Unified Codex or cloud ASR refine pass when chosen; any failure falls through to local refine.
    let mut refined = false;
    if cfg.refine_after_meeting {
        match cfg.refine_backend {
            RefineBackend::Local => {}
            RefineBackend::Codex => {
                match codex_pass(minutes, &mut meeting, &files, progress, &mut warnings) {
                    Ok(wrote_minutes) => {
                        refined = true;
                        minutes_done = wrote_minutes;
                    }
                    Err(e) => warnings.push(trf!(
                        "Codex 一体化精转写失败，改用本地精转写：{e}",
                        "Codex refine and minutes failed, so the local refine ran instead: {e}"
                    )),
                }
            }
            RefineBackend::OpenAi | RefineBackend::Gemini => {
                match cloud_refine_pass(
                    minutes,
                    &meeting,
                    &files,
                    cfg.refine_backend,
                    progress,
                    &mut warnings,
                ) {
                    Ok(()) => refined = true,
                    Err(e) => warnings.push(trf!(
                        "{} 精转写失败，改用本地精转写：{e}",
                        "{} refine failed, so the local refine ran instead: {e}",
                        logic::refine_backend_label(cfg.refine_backend),
                    )),
                }
            }
        }
    }
    if cfg.refine_after_meeting && !refined {
        progress(tr!("正在启动精转写引擎…", "Starting the refine engine…").into());
        match refine_engine() {
            // The worker shuts its process down when it drops at the end of this arm.
            Ok(worker) => refine(
                worker.engine(),
                minutes,
                &meeting,
                &files,
                progress,
                &mut warnings,
            ),
            Err(e) => warnings.push(trf!(
                "精转写引擎启动失败，跳过会后精转写：{e}",
                "Couldn’t start the refine engine, so the post-meeting refine was skipped: {e}"
            )),
        }
    }

    if cfg.auto_minutes && !minutes_done {
        progress(tr!("正在生成纪要…", "Writing minutes…").into());
        match generate_minutes(minutes, &mut meeting) {
            Ok(MinutesWritten {
                label,
                warnings: handover,
            }) => {
                minutes_done = true;
                warnings.extend(handover);
                progress(trf!(
                    "纪要已由{label}生成并写入笔记",
                    "Minutes saved to the note (written by {label})"
                ));
            }
            Err(e) => warnings.push(e),
        }
    }
    Ok(FinishReport {
        meeting,
        warnings,
        minutes_done,
    })
}

/// Prefers the files reported at stop time, falling back to those announced at start.
///
/// Params: `stopped`, `started`.
/// Returns: merged file set.
fn merged_files(stopped: &CaptureFiles, started: &CaptureFiles) -> CaptureFiles {
    CaptureFiles {
        screen: stopped.screen.clone().or_else(|| started.screen.clone()),
        microphone: stopped
            .microphone
            .clone()
            .or_else(|| started.microphone.clone()),
        system_audio: stopped
            .system_audio
            .clone()
            .or_else(|| started.system_audio.clone()),
    }
}

/// Re-transcribes the recorded files and stores the result as the final pass.
///
/// Params: `engine`; `ctx`; `meeting`; `files`; `progress`; `warnings` – appended to.
fn refine(
    engine: &MeetingEngine,
    ctx: &MinutesCtx,
    meeting: &Meeting,
    files: &CaptureFiles,
    progress: &dyn Fn(String),
    warnings: &mut Vec<String>,
) {
    let mut parts = Vec::new();
    let mut req = RefineRequest {
        language: ctx.cfg.caption_language.clone(),
        model: ctx.cfg.refine_asr_model.clone(),
        ..Default::default()
    };
    // Same voiceprints as the live preset, so enrolled people come back as `known`.
    match lock(&ctx.store)
        .and_then(|s| flow::build_preset(&ctx.cfg, &s, meeting, None).map_err(|e| e.to_string()))
    {
        Ok(p) => req.voiceprints = p.voiceprints,
        Err(e) => warnings.push(trf!(
            "读取声纹失败，精转写只按音色聚类：{e}",
            "Couldn’t read voiceprints, so refine groups speakers by voice only: {e}"
        )),
    }
    // Names and note terms steer the decoder toward the right spelling of people and projects.
    match lock(&ctx.store)
        .and_then(|s| flow::refine_context_for(&ctx.cfg, &s, meeting).map_err(|e| e.to_string()))
    {
        Ok(c) => req.context = c,
        Err(e) => warnings.push(trf!(
            "读取参会人和笔记失败，精转写不带提示词：{e}",
            "Couldn’t read attendees and notes, so refine runs without hints: {e}"
        )),
    }
    for (origin, path, label) in [
        (
            AudioOrigin::Microphone,
            &files.microphone,
            tr!("麦克风录音", "the microphone recording"),
        ),
        (
            AudioOrigin::System,
            &files.system_audio,
            tr!("系统声音录音", "the system audio recording"),
        ),
    ] {
        let Some(path) = path else { continue };
        progress(trf!("正在精转写{label}…", "Refining {label}…"));
        match refine_one(engine, path, &mut req, warnings) {
            Ok(t) => parts.push((origin, t)),
            Err(e) => warnings.push(trf!(
                "精转写 {} 失败：{e}",
                "Couldn’t refine {}: {e}",
                path.display()
            )),
        }
    }
    if parts.is_empty() {
        return;
    }
    let segments = flow::merge_file_transcripts(meeting.id, &parts);
    let result = lock(&ctx.store).and_then(|mut s| {
        flow::store_final_transcript(&mut s, meeting.id, &segments)
            .and_then(|_| flow::sync_transcript(&ctx.cfg, &mut s, meeting))
            .map_err(|e| e.to_string())
    });
    if let Err(e) = result {
        warnings.push(trf!(
            "保存精转写失败：{e}",
            "Couldn’t save the refined transcript: {e}"
        ));
    }
}

/// Longest wait for one unified Codex refine + minutes turn.
const CODEX_REFINE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Whether post-meeting processing writes minutes inside the unified Codex refine pass.
///
/// Params: `cfg`.
/// Returns: `true` when post-meeting refine uses Codex and automatic minutes are enabled.
pub fn uses_codex_unified_pass(cfg: &AppConfig) -> bool {
    cfg.refine_after_meeting && cfg.refine_backend == RefineBackend::Codex && cfg.auto_minutes
}

/// Unified post-meeting pass through the local Codex CLI: Codex receives the audio file
/// paths directly in a read-only thread, transcribes and separates speakers, and (when
/// `auto_minutes` is enabled) writes structured meeting minutes in the same turn.
///
/// Params: `ctx`; `meeting` – status updated when minutes are written; `files`; `progress`;
/// `warnings` – receives non-fatal issues (unusable minutes still keep the refined transcript).
/// Returns: whether minutes were written, or why the pass failed (caller falls back to local).
fn codex_pass(
    ctx: &MinutesCtx,
    meeting: &mut Meeting,
    files: &CaptureFiles,
    progress: &dyn Fn(String),
    warnings: &mut Vec<String>,
) -> Result<bool, String> {
    let codex_path = ctx
        .codex
        .as_ref()
        .and_then(CodexStatus::usable_path)
        .or_else(|| detect_codex().usable_path())
        .ok_or_else(|| {
            tr!(
                "没有找到已登录的 Codex CLI。请先安装 Codex 并在终端运行 codex login。",
                "Signed-in Codex CLI not found. Install Codex and run codex login in Terminal."
            )
            .to_string()
        })?;

    let mut tracks = Vec::new();
    for (origin, path) in [
        (AudioOrigin::Microphone, &files.microphone),
        (AudioOrigin::System, &files.system_audio),
    ] {
        let Some(p) = path else { continue };
        if p.is_file() {
            tracks.push(AudioTrack {
                origin,
                path: p.clone(),
            });
        } else {
            warnings.push(trf!(
                "找不到录音文件：{}",
                "Recording file not found: {}",
                p.display()
            ));
        }
    }
    if tracks.is_empty() {
        return Err(tr!("这场会议没有录音文件", "This meeting has no recordings").into());
    }

    let context = match lock(&ctx.store)
        .and_then(|s| flow::refine_context_for(&ctx.cfg, &s, meeting).map_err(|e| e.to_string()))
    {
        Ok(c) => c,
        Err(e) => {
            warnings.push(trf!(
                "读取参会人和笔记失败，转写时不带提示词：{e}",
                "Couldn’t read attendees and notes, so transcription runs without hints: {e}"
            ));
            None
        }
    };

    let minutes_input = match lock(&ctx.store)
        .and_then(|s| flow::minutes_input(&ctx.cfg, &s, meeting).map_err(|e| e.to_string()))
    {
        Ok(input) => input,
        Err(e) => {
            warnings.push(trf!(
                "整理纪要素材失败，仅使用基础会议信息：{e}",
                "Couldn’t gather full minutes material; using basic meeting metadata: {e}"
            ));
            mm_llm::MinutesInput {
                title: meeting.title.clone(),
                when: meeting
                    .starts_at
                    .with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string(),
                language: Some(ctx.cfg.minutes_language.clone()),
                ..Default::default()
            }
        }
    };

    let live = lock(&ctx.store)?.transcript(meeting.id).map_err(|e| {
        trf!(
            "读取实时转写失败：{e}",
            "Couldn’t read the live transcript: {e}"
        )
    })?;

    let model = ctx.cfg.codex_refine_model.as_deref();
    let label = match model.map(str::trim).filter(|m| !m.is_empty()) {
        Some(m) => format!("Codex · {m}"),
        None => "Codex".to_string(),
    };
    progress(trf!(
        "正在用 {label} 转写录音并生成纪要…",
        "Transcribing and writing minutes with {label}…"
    ));

    let language = ctx.cfg.caption_language.clone();
    let (heard, combined_minutes) = codex::transcribe_and_summarize(
        Path::new(&codex_path),
        &tracks,
        context.as_deref(),
        language.as_deref(),
        &minutes_input,
        model,
        CODEX_REFINE_TIMEOUT,
    )
    .map_err(|e| e.to_string())?;

    let mut segments = external::heard_to_segments(meeting.id, 0, &heard, &live, language);
    if segments.is_empty() {
        return Err(trf!(
            "{label} 没有识别出任何语句",
            "{label} didn’t recognize any speech"
        ));
    }
    segments.sort_by_key(|s| (s.start_ms, s.end_ms));

    {
        let mut store = lock(&ctx.store)?;
        flow::store_final_transcript(&mut store, meeting.id, &segments)
            .and_then(|_| flow::sync_transcript(&ctx.cfg, &mut store, meeting))
            .map_err(|e| trf!("保存识别结果失败：{e}", "Couldn’t save the transcript: {e}"))?;
    }

    if !ctx.cfg.auto_minutes {
        return Ok(false);
    }

    let written = combined_minutes
        .map_err(|e| e.to_string())
        .and_then(|minutes| {
            flow::apply_minutes(&ctx.cfg, &mut *lock(&ctx.store)?, meeting, &minutes)
                .map_err(|e| trf!("写入纪要失败：{e}", "Couldn’t save the minutes: {e}"))
        });
    match written {
        Ok(()) => {
            progress(trf!(
                "纪要已由 {label} 一并生成并写入笔记",
                "Minutes written by {label} and saved to the note"
            ));
            Ok(true)
        }
        Err(e) => {
            warnings.push(trf!(
                "{label} 一并生成的纪要不可用，改为单独生成：{e}",
                "The minutes {label} wrote along with the transcript were unusable, so they’ll be written separately: {e}"
            ));
            Ok(false)
        }
    }
}

/// Transcribes recorded files through OpenAI or Google Gemini speech APIs, records local
/// usage/spend in SQLite, maps speakers onto the live pass, and writes the final transcript.
///
/// Params: `ctx`; `meeting`; `files`; `backend` – `OpenAi` or `Gemini`; `progress`;
/// `warnings` – appended to on non-fatal issues.
/// Returns: `Ok(())` once the refined transcript is saved (or recordings had only silence).
fn cloud_refine_pass(
    ctx: &MinutesCtx,
    meeting: &Meeting,
    files: &CaptureFiles,
    backend: RefineBackend,
    progress: &dyn Fn(String),
    warnings: &mut Vec<String>,
) -> Result<(), String> {
    let (provider, account, configured_model, default_model) = match backend {
        RefineBackend::OpenAi => (
            AsrProvider::OpenAi,
            byok::OPENAI_KEY_ACCOUNT,
            ctx.cfg.refine_openai_model.as_deref(),
            DEFAULT_OPENAI_REFINE_MODEL,
        ),
        RefineBackend::Gemini => (
            AsrProvider::Gemini,
            byok::GEMINI_KEY_ACCOUNT,
            ctx.cfg.refine_gemini_model.as_deref(),
            DEFAULT_GEMINI_REFINE_MODEL,
        ),
        RefineBackend::Local | RefineBackend::Codex => {
            return Err("not a cloud refine backend".into());
        }
    };
    let model = configured_model
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .unwrap_or(default_model)
        .to_string();
    let label = format!("{} · {model}", logic::refine_backend_label(backend));

    let api_key = byok::read_account_key(account)
        .map_err(|e| trf!("读取 API 密钥失败：{e}", "Couldn’t read the API key: {e}"))?
        .filter(|k| !k.trim().is_empty())
        .ok_or_else(|| match provider {
            AsrProvider::OpenAi => tr!(
                "尚未保存 OpenAI API 密钥，请在设置里填写。",
                "No OpenAI API key is saved. Add one in Settings."
            )
            .to_string(),
            AsrProvider::Gemini => tr!(
                "尚未保存 Gemini API 密钥，请在设置里填写。",
                "No Gemini API key is saved. Add one in Settings."
            )
            .to_string(),
        })?;

    let mut tracks = Vec::new();
    for (origin, path) in [
        (AudioOrigin::Microphone, &files.microphone),
        (AudioOrigin::System, &files.system_audio),
    ] {
        let Some(p) = path else { continue };
        if p.is_file() {
            tracks.push((origin, p.clone()));
        } else {
            warnings.push(trf!(
                "找不到录音文件：{}",
                "Recording file not found: {}",
                p.display()
            ));
        }
    }
    if tracks.is_empty() {
        return Err(tr!("这场会议没有录音文件", "This meeting has no recordings").into());
    }

    let context = match lock(&ctx.store)
        .and_then(|s| flow::refine_context_for(&ctx.cfg, &s, meeting).map_err(|e| e.to_string()))
    {
        Ok(c) => c,
        Err(e) => {
            warnings.push(trf!(
                "读取参会人和笔记失败，转写时不带提示词：{e}",
                "Couldn’t read attendees and notes, so transcription runs without hints: {e}"
            ));
            None
        }
    };
    let live = lock(&ctx.store)?.transcript(meeting.id).map_err(|e| {
        trf!(
            "读取实时转写失败：{e}",
            "Couldn’t read the live transcript: {e}"
        )
    })?;

    progress(trf!(
        "正在预处理 {label} 音频分片…",
        "Preparing audio chunks for {label}…"
    ));
    let work = tempfile::tempdir().map_err(|e| format!("creating temp dir: {e}"))?;
    let chunks =
        if provider == AsrProvider::OpenAi && !asr_pricing::openai_model_has_timestamps(&model) {
            external::prepare_utterance_chunks(&tracks, work.path(), external::DEFAULT_UTTERANCE_MS)
        } else {
            external::prepare_track_chunks(&tracks, work.path(), external::CHUNK_MS)
        }
        .map_err(|e| e.to_string())?;

    if chunks.is_empty() {
        warnings.push(
            tr!(
                "录音里没有检测到有效语音，已保留实时转写。",
                "No active speech found in the recordings; keeping the live transcript."
            )
            .into(),
        );
        return Ok(());
    }

    let language = ctx.cfg.caption_language.clone();
    let mut all_lines = Vec::new();
    let mut total_audio_ms = 0_u64;
    let mut total_in_tokens = 0_u64;
    let mut total_out_tokens = 0_u64;
    let mut chunk_err: Option<String> = None;

    for (idx, chunk) in chunks.iter().enumerate() {
        progress(trf!(
            "正在用 {label} 精转写（第 {}/{} 段）…",
            "Refining with {label} (chunk {}/{})…",
            idx + 1,
            chunks.len()
        ));
        let res = match provider {
            AsrProvider::OpenAi => openai_asr::transcribe_file(
                &api_key,
                chunk.origin,
                &chunk.path,
                chunk.offset_ms,
                chunk.duration_ms,
                &OpenAiTranscribeOptions {
                    model: model.clone(),
                    language: language.clone(),
                    prompt: context.clone(),
                },
            ),
            AsrProvider::Gemini => gemini_asr::transcribe_file(
                &api_key,
                chunk.origin,
                &chunk.path,
                chunk.offset_ms,
                chunk.duration_ms,
                &GeminiTranscribeOptions {
                    model: model.clone(),
                    language: language.clone(),
                    context: context.clone(),
                },
            ),
        };
        match res {
            Ok(out) => {
                total_audio_ms = total_audio_ms.saturating_add(out.audio_ms);
                total_in_tokens = total_in_tokens.saturating_add(out.input_tokens);
                total_out_tokens = total_out_tokens.saturating_add(out.output_tokens);
                all_lines.extend(out.lines);
            }
            Err(e) => {
                chunk_err = Some(e.to_string());
                break;
            }
        }
    }

    if total_audio_ms > 0 || total_in_tokens > 0 || total_out_tokens > 0 {
        let cost_usd = asr_pricing::estimate_cost_usd(
            provider,
            AsrMode::Refine,
            &model,
            total_audio_ms,
            total_in_tokens,
            total_out_tokens,
        );
        let rec = AsrUsageRecord {
            recorded_at: chrono::Utc::now(),
            meeting_id: Some(meeting.id),
            provider: provider.as_str().to_string(),
            mode: AsrMode::Refine.as_str().to_string(),
            model: model.clone(),
            audio_ms: total_audio_ms,
            input_tokens: total_in_tokens,
            output_tokens: total_out_tokens,
            cost_usd,
        };
        if let Err(e) =
            lock(&ctx.store).and_then(|s| s.record_asr_usage(&rec).map_err(|e| e.to_string()))
        {
            warnings.push(trf!(
                "记录云端转写用量失败：{e}",
                "Couldn’t record cloud ASR usage: {e}"
            ));
        }
    }

    if let Some(err) = chunk_err {
        return Err(err);
    }

    let mut segments = external::heard_to_segments(meeting.id, 0, &all_lines, &live, language);
    if segments.is_empty() {
        return Err(trf!(
            "{label} 没有识别出任何语句",
            "{label} didn’t recognize any speech"
        ));
    }
    segments.sort_by_key(|s| (s.start_ms, s.end_ms));

    let mut store = lock(&ctx.store)?;
    flow::store_final_transcript(&mut store, meeting.id, &segments)
        .and_then(|_| flow::sync_transcript(&ctx.cfg, &mut store, meeting))
        .map_err(|e| trf!("保存识别结果失败：{e}", "Couldn’t save the transcript: {e}"))?;
    Ok(())
}

/// Transcribes one file; when a custom refine model fails, retries with the engine default
/// and keeps using the default for the remaining files.
///
/// Params: `engine`; `path`; `req` – its `model` is cleared after a fallback; `warnings`.
/// Returns: the transcript, or the error of the last attempt.
fn refine_one(
    engine: &MeetingEngine,
    path: &Path,
    req: &mut RefineRequest,
    warnings: &mut Vec<String>,
) -> Result<FileTranscript, mm_service::ServiceError> {
    match engine.refine_file(path, req) {
        Err(e) if req.model.is_some() => {
            let model = req.model.take().unwrap_or_default();
            warnings.push(trf!(
                "精转模型 {model} 不可用，已改用默认模型：{e}",
                "Refine model {model} is unavailable, so the default model ran instead: {e}"
            ));
            engine.refine_file(path, req)
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_path_requires_sign_in() {
        let mut s = CodexStatus {
            path: Some("/bin/codex".into()),
            ..Default::default()
        };
        assert_eq!(s.usable_path(), None);
        s.signed_in = true;
        assert_eq!(s.usable_path().as_deref(), Some("/bin/codex"));
    }

    #[test]
    fn codex_unified_pass_requires_refine_codex_and_auto_minutes() {
        let mut cfg = AppConfig {
            refine_backend: RefineBackend::Codex,
            refine_after_meeting: true,
            auto_minutes: true,
            ..Default::default()
        };
        assert!(uses_codex_unified_pass(&cfg));

        cfg.auto_minutes = false;
        assert!(!uses_codex_unified_pass(&cfg));

        cfg.auto_minutes = true;
        cfg.refine_after_meeting = false;
        assert!(!uses_codex_unified_pass(&cfg));

        cfg.refine_after_meeting = true;
        cfg.refine_backend = RefineBackend::Local;
        assert!(!uses_codex_unified_pass(&cfg));
    }

    #[test]
    fn stop_files_fall_back_to_start_files() {
        let started = CaptureFiles {
            microphone: Some("/r/mic.caf".into()),
            system_audio: Some("/r/sys.caf".into()),
            screen: None,
        };
        let stopped = CaptureFiles {
            microphone: Some("/r/mic2.caf".into()),
            ..Default::default()
        };
        let m = merged_files(&stopped, &started);
        assert_eq!(m.microphone.unwrap(), PathBuf::from("/r/mic2.caf"));
        assert_eq!(m.system_audio.unwrap(), PathBuf::from("/r/sys.caf"));
    }

    #[test]
    fn finish_without_engine_keeps_meeting_and_reports() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let (indexer, _ev) = mm_service::indexer::Indexer::start(store.clone(), None);
        let (writer, _err) = LiveWriter::start(store.clone(), indexer);
        let cfg = AppConfig {
            vault_path: Some(dir.path().into()),
            auto_minutes: false,
            ..Default::default()
        };
        let mut m = flow::quick_meeting(&store.lock().unwrap(), "测试").unwrap();
        flow::ensure_note(&cfg, &store.lock().unwrap(), &mut m, &[]).unwrap();
        let started = CaptureStarted {
            session_id: "s".into(),
            directory: dir.path().join("rec"),
            files: Default::default(),
            microphone_channels: Some(1),
            started_at: chrono::Utc::now(),
        };
        flow::mark_live(&store.lock().unwrap(), &mut m, &started).unwrap();
        let steps = Mutex::new(Vec::new());
        let report = finish_meeting(
            FinishJob {
                minutes: MinutesCtx {
                    cfg,
                    store: store.clone(),
                    codex: None,
                    embed: None,
                },
                engine: None,
                writer,
                meeting: m.clone(),
                started,
                elapsed_ms: 1000,
            },
            &|s| steps.lock().unwrap().push(s),
        )
        .unwrap();
        assert_eq!(report.meeting.status, mm_core::MeetingStatus::Processing);
        assert!(report.warnings.iter().any(|w| w.contains("停止录制")));
        assert!(report.warnings.iter().any(|w| w.contains("精转写")));
        assert!(!steps.lock().unwrap().is_empty());
    }

    #[test]
    fn stop_phase_hands_its_warnings_to_the_queued_phase() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let (indexer, _ev) = mm_service::indexer::Indexer::start(store.clone(), None);
        let (writer, _err) = LiveWriter::start(store.clone(), indexer);
        let cfg = AppConfig {
            vault_path: Some(dir.path().into()),
            auto_minutes: false,
            refine_after_meeting: false,
            ..Default::default()
        };
        let mut m = flow::quick_meeting(&store.lock().unwrap(), "排队").unwrap();
        let started = CaptureStarted {
            session_id: "s".into(),
            directory: dir.path().join("rec"),
            files: CaptureFiles {
                microphone: Some(dir.path().join("rec/mic.caf")),
                ..Default::default()
            },
            microphone_channels: Some(1),
            started_at: chrono::Utc::now(),
        };
        flow::mark_live(&store.lock().unwrap(), &mut m, &started).unwrap();
        let stopped = stop_meeting(
            StopJob {
                store: store.clone(),
                cfg: cfg.clone(),
                engine: None,
                writer,
                meeting: m,
                started,
                elapsed_ms: 1000,
            },
            &|_| {},
        )
        .unwrap();
        // The recorder is free from here on; the files travel with the queued job.
        assert_eq!(stopped.meeting.status, mm_core::MeetingStatus::Processing);
        assert!(stopped.files.microphone.is_some());
        assert_eq!(stopped.warnings.len(), 1, "{:?}", stopped.warnings);

        let ctx = MinutesCtx {
            cfg,
            store,
            codex: None,
            embed: None,
        };
        let report = process_meeting(
            &ctx,
            || panic!("refine is off, so no refine engine starts"),
            stopped,
            &|_| {},
        )
        .unwrap();
        assert!(!report.minutes_done);
        assert_eq!(report.warnings.len(), 1);
        assert!(report.warnings[0].contains("停止录制"));
    }
}
