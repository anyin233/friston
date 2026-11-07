//! Blocking multi-step jobs run on the background executor: post-meeting processing,
//! minutes generation and provider detection.
//!
//! Each step reports progress through a callback so the UI can show where it is, and a
//! failing step never discards what earlier steps produced (the recording, the live
//! transcript).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use mm_core::{AudioOrigin, CaptureFiles, CaptureStarted, CaptureStopped, FileTranscript, Meeting};
use mm_llm::{LlmError, byok, codex};
use mm_service::engines::{EmbedEngine, MeetingEngine, RefineRequest};
use mm_service::{AppConfig, flow};
use mm_store::Store;

use crate::logic::provider_config;
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
            error: Some(format!("{} account/read：{e}", path.display())),
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

/// Generates minutes with the configured provider and writes them into store and note.
///
/// Params: `ctx`; `meeting` – updated in place (status Done on success).
/// Returns: the provider label used, or an error message with context.
pub fn generate_minutes(ctx: &MinutesCtx, meeting: &mut Meeting) -> Result<String, String> {
    let key_present = match &ctx.cfg.byok {
        Some(p) => byok::read_key(p)
            .map_err(|e| format!("读取 API 密钥失败：{e}"))?
            .is_some(),
        None => false,
    };
    let codex = match &ctx.codex {
        Some(c) if c.signed_in => c.usable_path(),
        _ if ctx.cfg.use_codex => detect_codex().usable_path(),
        _ => None,
    };
    let local = local_model(ctx);
    let choice = mm_llm::select_provider(&provider_config(&ctx.cfg, key_present, codex, local))
        .map_err(|_| "没有可用的纪要生成方式：请在设置中添加 API 密钥、运行 codex login 登录 ChatGPT，或加载本地纪要模型。".to_string())?;
    let label = match &choice {
        mm_llm::ProviderChoice::Byok(p) => format!("API 密钥（{}）", byok::keychain_account(p)),
        mm_llm::ProviderChoice::Codex { .. } => "Codex（ChatGPT）".to_string(),
        mm_llm::ProviderChoice::LocalMlx { .. } => "本地模型".to_string(),
    };
    let input = flow::minutes_input(&ctx.cfg, &*lock(&ctx.store)?, meeting)
        .map_err(|e| format!("整理纪要素材失败：{e}"))?;
    let embed = ctx.embed.clone();
    let run_local = move |_: &str, input: &mm_llm::MinutesInput| match &embed {
        Some(e) => e.local_minutes(input),
        None => Err(LlmError::Local("本地模型尚未配置".into())),
    };
    let minutes = mm_llm::generate_minutes(&choice, &input, &run_local)
        .map_err(|e| format!("{label} 生成纪要失败：{e}"))?;
    flow::apply_minutes(&ctx.cfg, &mut *lock(&ctx.store)?, meeting, &minutes)
        .map_err(|e| format!("写入纪要失败：{e}"))?;
    Ok(label)
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
/// Params: `job`; `progress` – called with a Chinese step description.
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

    progress("正在停止录制…".into());
    let stopped = match engine.as_ref().map(|e| e.capture_stop()) {
        Some(Ok(s)) => s,
        other => {
            let why = match other {
                Some(Err(e)) => e.to_string(),
                _ => "字幕引擎不在运行".into(),
            };
            warnings.push(format!(
                "停止录制时出错（{why}），已保留录音文件夹 {}",
                started.directory.display()
            ));
            CaptureStopped {
                session_id: started.session_id.clone(),
                duration_ms: elapsed_ms,
                files: started.files.clone(),
            }
        }
    };
    flow::mark_stopped(&*lock(&minutes.store)?, &mut meeting, &stopped)
        .map_err(|e| format!("更新会议状态失败：{e}"))?;

    progress("正在写入实时转写…".into());
    writer.flush_blocking();
    if let Err(e) = flow::sync_transcript(&cfg, &mut *lock(&minutes.store)?, &meeting) {
        warnings.push(format!("同步转写到笔记失败：{e}"));
    }

    if cfg.refine_after_meeting {
        match &engine {
            Some(engine) => refine(
                engine,
                &minutes,
                &meeting,
                &merged_files(&stopped.files, &started.files),
                progress,
                &mut warnings,
            ),
            None => warnings.push("字幕引擎不可用，跳过会后精转写".into()),
        }
    }

    let mut minutes_done = false;
    if cfg.auto_minutes {
        progress("正在生成纪要…".into());
        match generate_minutes(&minutes, &mut meeting) {
            Ok(label) => {
                minutes_done = true;
                progress(format!("纪要已由{label}生成并写入笔记"));
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
        Err(e) => warnings.push(format!("读取声纹失败，精转写只按音色聚类：{e}")),
    }
    // Names and note terms steer the decoder toward the right spelling of people and projects.
    match lock(&ctx.store)
        .and_then(|s| flow::refine_context_for(&ctx.cfg, &s, meeting).map_err(|e| e.to_string()))
    {
        Ok(c) => req.context = c,
        Err(e) => warnings.push(format!("读取参会人和笔记失败，精转写不带提示词：{e}")),
    }
    for (origin, path, label) in [
        (AudioOrigin::Microphone, &files.microphone, "麦克风录音"),
        (AudioOrigin::System, &files.system_audio, "系统声音录音"),
    ] {
        let Some(path) = path else { continue };
        progress(format!("正在精转写{label}…"));
        match refine_one(engine, path, &mut req, warnings) {
            Ok(t) => parts.push((origin, t)),
            Err(e) => warnings.push(format!("精转写 {} 失败：{e}", path.display())),
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
        warnings.push(format!("保存精转写失败：{e}"));
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
            warnings.push(format!("精转模型 {model} 不可用，已改用默认模型：{e}"));
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
