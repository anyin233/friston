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
use mm_llm::{LlmError, MinutesBackend, ProviderChoice, antigravity, byok, codex};
use mm_service::engines::{EmbedEngine, MeetingEngine, RefineRequest};
use mm_service::{AppConfig, RefineBackend, external, flow};
use mm_store::Store;

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
        MinutesBackend::Antigravity => tr!("没有找到 agy", "agy wasn’t found"),
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
    let agy = (backend == MinutesBackend::Antigravity)
        .then(antigravity::find_agy)
        .flatten()
        .map(|p| p.display().to_string());
    let local = local_model(ctx);
    let plan = mm_llm::plan_providers(&provider_config(&ctx.cfg, key_present, codex, agy, local))
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

/// Inputs of the post-meeting job.
pub struct FinishJob {
    pub minutes: MinutesCtx,
    pub engine: Option<MeetingEngine>,
    pub writer: LiveWriter,
    pub meeting: Meeting,
    pub started: CaptureStarted,
    /// Elapsed time, used when the engine cannot report the duration.
    pub elapsed_ms: u64,
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

/// Stops capture and runs refine → transcript sync → minutes.
///
/// Params: `job`; `progress` – called with a step description in the UI language.
/// Returns: the report; only a missing store makes it fail.
pub fn finish_meeting(job: FinishJob, progress: &dyn Fn(String)) -> Result<FinishReport, String> {
    let FinishJob {
        minutes,
        engine,
        writer,
        mut meeting,
        started,
        elapsed_ms,
    } = job;
    let cfg = minutes.cfg.clone();
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
    flow::mark_stopped(&*lock(&minutes.store)?, &mut meeting, &stopped).map_err(|e| {
        trf!(
            "更新会议状态失败：{e}",
            "Couldn’t update the meeting status: {e}"
        )
    })?;

    progress(tr!("正在写入实时转写…", "Saving the live transcript…").into());
    writer.flush_blocking();
    if let Err(e) = flow::sync_transcript(&cfg, &mut *lock(&minutes.store)?, &meeting) {
        warnings.push(trf!(
            "同步转写到笔记失败：{e}",
            "Couldn’t sync the transcript to the note: {e}"
        ));
    }

    let files = merged_files(&stopped.files, &started.files);
    let mut minutes_done = false;
    // Antigravity first when chosen; any failure there falls through to the local pass.
    let mut refined = false;
    if cfg.refine_after_meeting && cfg.refine_backend == RefineBackend::Antigravity {
        match antigravity_pass(&minutes, &mut meeting, &files, progress, &mut warnings) {
            Ok(wrote_minutes) => {
                refined = true;
                minutes_done = wrote_minutes;
            }
            Err(e) => warnings.push(trf!(
                "Antigravity 精转写失败，改用本地精转写：{e}",
                "Antigravity refine failed, so the local refine ran instead: {e}"
            )),
        }
    }
    if cfg.refine_after_meeting && !refined {
        match &engine {
            Some(engine) => refine(engine, &minutes, &meeting, &files, progress, &mut warnings),
            None => warnings.push(
                tr!(
                    "字幕引擎不可用，跳过会后精转写",
                    "Caption engine unavailable; skipped the post-meeting refine"
                )
                .into(),
            ),
        }
    }

    if cfg.auto_minutes && !minutes_done {
        progress(tr!("正在生成纪要…", "Writing minutes…").into());
        match generate_minutes(&minutes, &mut meeting) {
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

/// Removes a scratch folder when dropped, so every exit path cleans up the converted audio.
struct ScratchDir(PathBuf);

impl Drop for ScratchDir {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.0)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!("removing {}: {e}", self.0.display());
        }
    }
}

/// Longest wait for one agy run (one audio part, or the minutes); agy retries 500s itself.
const AGY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20 * 60);

/// Whether the Antigravity refine should also write the minutes in the same agy run.
///
/// That happens only when the minutes would go to the same CLI and model anyway, and the
/// recording fits in one part: longer recordings are heard part by part, and minutes need
/// the whole meeting, so they are written afterwards from the joined transcript.
///
/// Params: `cfg`; `parts` – number of audio parts the recording was cut into.
/// Returns: `true` for one combined run.
pub fn merges_minutes_into_refine(cfg: &AppConfig, parts: usize) -> bool {
    cfg.auto_minutes
        && cfg.minutes_backend == MinutesBackend::Antigravity
        && cfg.antigravity_minutes_model == cfg.antigravity_model
        && parts == 1
}

/// Post-meeting pass through the Antigravity CLI: Gemini hears the recordings and the
/// result replaces the transcript. When [`merges_minutes_into_refine`] holds, the same run
/// also writes the minutes; otherwise the caller writes them with the configured provider.
///
/// Params: `ctx`; `meeting` – status updated when minutes are written; `files`; `progress`;
/// `warnings` – gets non-fatal problems (unusable combined minutes keep the new transcript).
/// Returns: whether minutes were written, or why the transcript step failed (the caller then
/// runs the local pass).
fn antigravity_pass(
    ctx: &MinutesCtx,
    meeting: &mut Meeting,
    files: &CaptureFiles,
    progress: &dyn Fn(String),
    warnings: &mut Vec<String>,
) -> Result<bool, String> {
    let agy = antigravity::find_agy()
        .ok_or(tr!(
            "没有找到 Antigravity CLI（agy）。安装 Antigravity 后在终端运行一次 agy 并登录。",
            "Antigravity CLI (agy) not found. Install Antigravity, then run agy once in Terminal and sign in."
        ))?;
    let model = ctx.cfg.antigravity_model.as_str();
    let label = antigravity::model_label(model);
    let recordings: Vec<(AudioOrigin, PathBuf)> = [
        (AudioOrigin::Microphone, &files.microphone),
        (AudioOrigin::System, &files.system_audio),
    ]
    .into_iter()
    .filter_map(|(o, p)| p.clone().map(|p| (o, p)))
    .collect();
    if recordings.is_empty() {
        return Err(tr!("这场会议没有录音文件", "This meeting has no recordings").into());
    }
    progress(tr!("正在准备录音…", "Preparing recordings…").into());
    let work = ScratchDir(std::env::temp_dir().join(format!("friston-agy-{}", meeting.id)));
    let chunks = external::prepare_recordings(&recordings, &work.0)
        .map_err(|e| trf!("准备录音失败：{e}", "Couldn’t prepare the recordings: {e}"))?;
    if chunks.is_empty() {
        return Err(tr!("录音里没有声音", "The recordings are silent").into());
    }
    let context = match lock(&ctx.store)
        .and_then(|s| flow::refine_context_for(&ctx.cfg, &s, meeting).map_err(|e| e.to_string()))
    {
        Ok(c) => c,
        Err(e) => {
            warnings.push(trf!(
                "读取参会人和笔记失败，识别时不带提示词：{e}",
                "Couldn’t read attendees and notes, so recognition runs without hints: {e}"
            ));
            None
        }
    };
    // Title, attendees and notes for a combined run; without them it only transcribes.
    let minutes_input = if merges_minutes_into_refine(&ctx.cfg, chunks.len()) {
        match lock(&ctx.store)
            .and_then(|s| flow::minutes_input(&ctx.cfg, &s, meeting).map_err(|e| e.to_string()))
        {
            Ok(input) => Some(input),
            Err(e) => {
                warnings.push(trf!(
                    "整理纪要素材失败，纪要改为在转写后单独生成：{e}",
                    "Couldn’t gather material for the minutes, so they’ll be written after the transcript: {e}"
                ));
                None
            }
        }
    } else {
        None
    };
    let live = lock(&ctx.store)?.transcript(meeting.id).map_err(|e| {
        trf!(
            "读取实时转写失败：{e}",
            "Couldn’t read the live transcript: {e}"
        )
    })?;
    let language = ctx.cfg.caption_language.clone();
    let mut segments = Vec::new();
    let mut combined_minutes = None;
    for (i, chunk) in chunks.iter().enumerate() {
        progress(match (&minutes_input, chunks.len()) {
            (Some(_), _) => trf!(
                "正在用 {label} 识别录音并生成纪要…",
                "Transcribing and writing minutes with {label}…"
            ),
            (None, 1) => trf!("正在用 {label} 识别录音…", "Transcribing with {label}…"),
            (None, n) => trf!(
                "正在用 {label} 识别录音（第 {}/{} 段）…",
                "Transcribing with {label} (part {} of {})…",
                i + 1,
                n
            ),
        });
        let heard = match &minutes_input {
            Some(input) => antigravity::transcribe_with_minutes(
                &agy,
                &chunk.dir,
                &chunk.tracks,
                context.as_deref(),
                language.as_deref(),
                input,
                model,
                AGY_TIMEOUT,
            )
            .map(|(heard, minutes)| {
                combined_minutes = Some(minutes);
                heard
            }),
            None => antigravity::transcribe(
                &agy,
                &chunk.dir,
                &chunk.tracks,
                context.as_deref(),
                language.as_deref(),
                model,
                AGY_TIMEOUT,
            ),
        }
        .map_err(|e| trf!("第 {} 段：{e}", "Part {}: {e}", i + 1))?;
        if heard.interrupted.is_some() {
            warnings.push(trf!(
                "{label} 识别第 {} 段时被中断，这一段的转写可能不完整",
                "{label} was interrupted on part {}; its transcript may be incomplete",
                i + 1
            ));
        }
        segments.extend(external::heard_to_segments(
            meeting.id,
            chunk.offset_ms,
            &heard.lines,
            &live,
            language.clone(),
        ));
    }
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
    let Some(minutes) = combined_minutes else {
        return Ok(false);
    };
    let written = minutes.map_err(|e| e.to_string()).and_then(|minutes| {
        flow::apply_minutes(&ctx.cfg, &mut *lock(&ctx.store)?, meeting, &minutes)
            .map_err(|e| trf!("写入纪要失败：{e}", "Couldn’t save the minutes: {e}"))
    });
    match written {
        Ok(()) => {
            progress(trf!(
                "纪要已由{label}（Antigravity）生成并写入笔记",
                "Minutes written by {label} (Antigravity) and saved to the note"
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
    fn refine_writes_minutes_only_for_same_model_and_one_part() {
        let mut cfg = AppConfig {
            refine_backend: RefineBackend::Antigravity,
            minutes_backend: MinutesBackend::Antigravity,
            ..Default::default()
        };
        assert_eq!(cfg.antigravity_minutes_model, cfg.antigravity_model);
        assert!(merges_minutes_into_refine(&cfg, 1));
        assert!(!merges_minutes_into_refine(&cfg, 2), "long meetings split");
        cfg.antigravity_minutes_model = "claude-opus-5-5-high".into();
        assert!(
            !merges_minutes_into_refine(&cfg, 1),
            "another model writes them later"
        );
        cfg.antigravity_minutes_model = cfg.antigravity_model.clone();
        cfg.minutes_backend = MinutesBackend::Codex;
        assert!(
            !merges_minutes_into_refine(&cfg, 1),
            "another provider writes them later"
        );
        cfg.minutes_backend = MinutesBackend::Antigravity;
        cfg.auto_minutes = false;
        assert!(!merges_minutes_into_refine(&cfg, 1));
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
}
