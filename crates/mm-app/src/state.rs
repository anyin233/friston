//! Application state shared by every page: store, settings, engines, indexer and the
//! live session.
//!
//! Threading rule: nothing here blocks the UI thread. Store access, engine calls, Keychain
//! and Codex probes run through [`bg`] on the background executor and their results are
//! applied back on the foreground; engine notification streams are drained by foreground
//! tasks awaiting `async_channel::Receiver::recv`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{Local, Utc};
use gpui_kit::*;
use mm_core::{
    AudioInput, AudioLevels, CalendarEvent, CaptureSources, CaptureStarted, CaptureState,
    ClipRequest, LevelSource, Meeting, MeetingId, Person, PersonId, Voiceprint, VoiceprintId,
    VoiceprintSource,
};
use mm_engine::EngineEvent;
use mm_service::engines::{
    EMBEDD, EmbedEngine, EmbedStatus, LlmStatus, MEETINGD, MeetingEngine, MeetingNotice,
    decode_notice, locate_engine,
};
use mm_service::hub::{self, HubRepo};
use mm_service::indexer::{Indexer, IndexerEvent, IndexerMsg, current_stamp};
use mm_service::live::{CaptionBoard, SpeakerDirectory, person_hue};
use mm_service::{AppConfig, ServiceError, data_dir, flow, models_dir};
use mm_store::{IndexStats, Store};

use crate::logic::{self, ActionRow};
use crate::pages::Page;
use crate::pipeline::{self, CodexStatus, FinishJob, MinutesCtx};
use crate::writer::{LiveWriter, WriteMsg, lock};

/// Runs `work` on the background executor and applies `done` to the view afterwards.
///
/// Params: `cx` – the view's context; `work` – blocking closure; `done` – foreground
/// continuation (skipped if the view is gone).
pub fn bg<V: 'static, T: Send + 'static>(
    cx: &mut Context<V>,
    work: impl FnOnce() -> T + Send + 'static,
    done: impl FnOnce(&mut V, T, &mut Context<V>) + 'static,
) {
    let task = cx.background_spawn(async move { work() });
    cx.spawn(async move |this, cx| {
        let out = task.await;
        let _ = this.update(cx, |v, cx| done(v, out, cx));
    })
    .detach();
}

/// A running Hugging Face download.
#[derive(Debug, Clone)]
pub struct ModelDownload {
    /// `owner/name` being fetched.
    pub repo: String,
    pub progress: hub::Progress,
    /// Set to stop the download; partial files stay for the next attempt.
    pub cancel: Arc<AtomicBool>,
}

/// Messages from the download thread to the UI.
enum DownloadMsg {
    Progress(hub::Progress),
    Done(Result<PathBuf, hub::HubError>),
}

/// Something loaded in the background.
#[derive(Debug, Clone, Default)]
pub enum Loadable<T> {
    #[default]
    Idle,
    Loading,
    Ready(T),
    Failed(LoadError),
}

impl<T> Loadable<T> {
    /// The value if ready.
    ///
    /// Returns: a reference, or `None`.
    pub fn ready(&self) -> Option<&T> {
        match self {
            Loadable::Ready(v) => Some(v),
            _ => None,
        }
    }
}

/// A failed load, with permission errors flagged so the UI can link to System Settings.
#[derive(Debug, Clone)]
pub struct LoadError {
    pub message: String,
    pub permission: bool,
}

impl From<ServiceError> for LoadError {
    /// Classifies a service error.
    fn from(e: ServiceError) -> Self {
        Self {
            permission: matches!(e, ServiceError::Permission(_)),
            message: e.to_string(),
        }
    }
}

impl From<String> for LoadError {
    /// Wraps a plain message (engine missing etc.).
    fn from(message: String) -> Self {
        Self {
            message,
            permission: false,
        }
    }
}

/// Lifecycle of a helper engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineState {
    NotStarted,
    Starting,
    Ready,
    /// The executable was not found (dev builds without `make engines`).
    Missing,
    Failed(String),
}

impl EngineState {
    /// User-facing description for status lines.
    ///
    /// Params: `name` – engine description, e.g. "字幕引擎".
    /// Returns: the text.
    pub fn describe(&self, name: &str) -> String {
        match self {
            EngineState::NotStarted => format!("{name}未启动"),
            EngineState::Starting => format!("{name}正在启动"),
            EngineState::Ready => format!("{name}就绪"),
            EngineState::Missing => format!("找不到{name}，请先运行 make engines"),
            EngineState::Failed(e) => format!("{name}不可用：{e}"),
        }
    }
}

/// Severity of a toast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    Info,
    Success,
    Warning,
    Error,
}

/// Events the workbench reacts to (toasts and navigation need the window).
#[derive(Debug, Clone)]
pub enum AppEvent {
    Notify(NoticeKind, String),
    Navigate(Page),
    /// Open a meeting in 纪要库, optionally at an offset.
    OpenMeeting(MeetingId, Option<u64>),
    /// A meeting's minutes/transcript/speakers changed; detail views reload.
    MeetingChanged(MeetingId),
    /// A live session started; the notes editor loads this text.
    LiveStarted {
        notes: String,
    },
}

/// A person with their voiceprints.
#[derive(Debug, Clone)]
pub struct PersonRow {
    pub person: Person,
    pub hue: u16,
    pub voiceprints: Vec<Voiceprint>,
}

/// The running capture.
pub struct LiveSession {
    pub meeting: Meeting,
    pub started: CaptureStarted,
    pub started_at: Instant,
    pub board: CaptionBoard,
    pub directory: SpeakerDirectory,
    pub capture_state: CaptureState,
    pub capture_error: Option<String>,
    pub levels: HashMap<LevelSource, AudioLevels>,
    /// Running left-right position per speaker, from `spatial/cue` (empty without a stereo mic).
    pub positions: logic::SpeakerPositions,
    pub note_path: Option<PathBuf>,
    pub window_id: Option<u32>,
    last_sync: Instant,
}

impl LiveSession {
    /// Milliseconds since capture started.
    ///
    /// Returns: elapsed time.
    pub fn elapsed_ms(&self) -> u64 {
        self.started_at.elapsed().as_millis() as u64
    }
}

/// Progress of the post-meeting job.
#[derive(Debug, Clone)]
pub struct PostMeeting {
    pub meeting_id: MeetingId,
    pub title: String,
    pub steps: Vec<String>,
    pub warnings: Vec<String>,
    pub running: bool,
}

/// How a speaker gets named.
#[derive(Debug, Clone)]
pub enum PersonChoice {
    Existing(PersonId),
    New(String),
}

/// How long transcript syncs are spaced during a meeting.
const SYNC_EVERY: Duration = Duration::from_secs(10);
/// Notes autosave debounce.
const NOTES_DEBOUNCE: Duration = Duration::from_millis(1000);
/// When to explain a slow `capture/start`.
const START_HINT_AFTER: Duration = Duration::from_secs(15);
/// Voiceprint enrolment length.
pub const ENROLL_SECONDS: u32 = 15;

/// Shared application state (one entity per process).
pub struct AppState {
    pub data_dir: PathBuf,
    pub store: Arc<Mutex<Store>>,
    pub cfg: AppConfig,
    pub indexer: Indexer,
    writer: LiveWriter,

    meeting_engine: Option<MeetingEngine>,
    pub meeting_state: EngineState,
    meeting_restarts: u32,
    embed_engine: Option<EmbedEngine>,
    pub embed_state: EngineState,
    embed_restarts: u32,

    pub live: Option<LiveSession>,
    pub live_starting: bool,
    pub post: Option<PostMeeting>,
    /// Window chosen on the 会议中 page; used by every start path.
    pub selected_window: Option<u32>,

    pub meetings: Loadable<Vec<Meeting>>,
    pub actions: Vec<ActionRow>,
    pub people: Loadable<Vec<PersonRow>>,
    pub events: Loadable<Vec<CalendarEvent>>,
    pub inputs: Loadable<Vec<AudioInput>>,
    pub sources: Loadable<CaptureSources>,
    /// macOS privacy grants (screen & system audio, microphone, calendar).
    pub permissions: Loadable<mm_core::PermissionStatus>,
    pub codex: Loadable<CodexStatus>,
    pub embed_status: Loadable<EmbedStatus>,
    pub llm_status: Loadable<LlmStatus>,
    pub index_stats: Option<IndexStats>,
    pub byok_key_present: bool,
    /// A Hugging Face token is stored in the Keychain.
    pub hf_token_present: bool,
    /// Model download in progress, if any.
    pub model_download: Option<ModelDownload>,
    /// Person currently enrolling and when it started (countdown).
    pub enrolling: Option<(PersonId, Instant)>,

    notes_pending: Option<String>,
    notes_task: Option<Task<()>>,
    floating: Option<AnyWindowHandle>,
    /// Notices raised before the workbench subscribed.
    startup_notices: Vec<(NoticeKind, String)>,
}

impl EventEmitter<AppEvent> for AppState {}

impl AppState {
    /// Opens the store and settings, starts the indexer and kicks off background probes.
    ///
    /// Params: `cx` – entity context.
    /// Returns: the state; failures become startup notices instead of panics.
    pub fn new(cx: &mut Context<Self>) -> Self {
        let dir = data_dir();
        let mut notices = Vec::new();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            notices.push((
                NoticeKind::Error,
                format!("无法创建数据文件夹 {}：{e}", dir.display()),
            ));
        }
        let db = dir.join("meeting-master.db");
        let store = match Store::open(&db) {
            Ok(s) => s,
            Err(e) => {
                notices.push((
                    NoticeKind::Error,
                    format!("打开数据库 {} 失败，本次使用临时数据库：{e}", db.display()),
                ));
                Store::open_in_memory().expect("in-memory store always opens")
            }
        };
        let first_run = !dir.join("config.json").exists();
        let mut cfg = AppConfig::load(&dir).unwrap_or_else(|e| {
            notices.push((
                NoticeKind::Error,
                format!("读取设置失败，已使用默认设置：{e}"),
            ));
            AppConfig::default()
        });
        if first_run {
            if cfg.adopt_obsidian_vault()
                && let Some(v) = &cfg.vault_path
            {
                notices.push((
                    NoticeKind::Info,
                    format!("已自动使用 Obsidian 库：{}", v.display()),
                ));
            }
            if let Err(e) = cfg.save(&dir) {
                notices.push((NoticeKind::Error, e.to_string()));
            }
        }
        let store = Arc::new(Mutex::new(store));
        let (indexer, index_events) = Indexer::start(store.clone(), cfg.vault_path.clone());
        let (writer, write_errors) = LiveWriter::start(store.clone(), indexer.clone());

        cx.spawn(async move |this, cx| {
            while let Ok(ev) = index_events.recv().await {
                let alive = this.update(cx, |s, cx| match ev {
                    IndexerEvent::Updated { .. } => s.refresh_stats(cx),
                    IndexerEvent::Error(e) => tracing::warn!("indexer: {e}"),
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
        cx.spawn(async move |this, cx| {
            while let Ok(e) = write_errors.recv().await {
                if this
                    .update(cx, |s, cx| s.notify(NoticeKind::Error, e, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        let mut state = Self {
            data_dir: dir,
            store,
            cfg,
            indexer,
            writer,
            meeting_engine: None,
            meeting_state: EngineState::NotStarted,
            meeting_restarts: 0,
            embed_engine: None,
            embed_state: EngineState::NotStarted,
            embed_restarts: 0,
            live: None,
            live_starting: false,
            post: None,
            selected_window: None,
            meetings: Loadable::Loading,
            actions: vec![],
            people: Loadable::Loading,
            events: Loadable::Loading,
            inputs: Loadable::Idle,
            sources: Loadable::Idle,
            permissions: Loadable::Idle,
            codex: Loadable::Loading,
            embed_status: Loadable::Idle,
            llm_status: Loadable::Idle,
            index_stats: None,
            byok_key_present: false,
            hf_token_present: false,
            model_download: None,
            enrolling: None,
            notes_pending: None,
            notes_task: None,
            floating: None,
            startup_notices: notices,
        };
        state.ensure_meeting_engine(cx);
        state.refresh_meetings(cx);
        state.refresh_people(cx);
        state.refresh_stats(cx);
        state.refresh_codex(cx);
        state.refresh_key_presence(cx);
        state.refresh_hf_token(cx);
        if let Some(dir) = state.cfg.embedding_model_dir.clone() {
            state.load_embedding(dir, cx);
        }
        state
    }

    /// Hands notices raised during construction to the subscriber.
    ///
    /// Returns: the notices (emptied).
    pub fn take_startup_notices(&mut self) -> Vec<(NoticeKind, String)> {
        std::mem::take(&mut self.startup_notices)
    }

    /// Raises a toast.
    ///
    /// Params: `kind`; `msg`; `cx`.
    pub fn notify(&mut self, kind: NoticeKind, msg: impl Into<String>, cx: &mut Context<Self>) {
        let msg = msg.into();
        if kind == NoticeKind::Error {
            tracing::warn!("{msg}");
        }
        cx.emit(AppEvent::Notify(kind, msg));
    }

    /// Asks the workbench to show a page.
    ///
    /// Params: `page`; `cx`.
    pub fn navigate(&mut self, page: Page, cx: &mut Context<Self>) {
        cx.emit(AppEvent::Navigate(page));
    }

    // ---- engines -----------------------------------------------------------------------

    /// The meeting engine, or a Chinese explanation of why it is unavailable.
    ///
    /// Returns: a clone of the engine handle.
    pub fn meeting_engine(&self) -> Result<MeetingEngine, String> {
        self.meeting_engine
            .clone()
            .ok_or_else(|| self.meeting_state.describe("字幕引擎 mm-meetingd "))
    }

    /// The embed engine when a model is loaded (search queries, local minutes).
    ///
    /// Returns: the engine or `None`.
    pub fn embed_engine(&self) -> Option<EmbedEngine> {
        self.embed_engine.clone()
    }

    /// Whether an embedding model is loaded.
    ///
    /// Returns: `true` when query vectors can be computed.
    pub fn embedding_ready(&self) -> bool {
        self.embed_status.ready().is_some_and(|s| s.loaded)
    }

    /// Starts `mm-meetingd` if it is not running yet (no-op while starting or ready).
    ///
    /// Params: `cx`.
    pub fn ensure_meeting_engine(&mut self, cx: &mut Context<Self>) {
        if matches!(
            self.meeting_state,
            EngineState::Starting | EngineState::Ready
        ) {
            return;
        }
        self.meeting_state = EngineState::Starting;
        self.events = Loadable::Loading;
        cx.notify();
        bg(
            cx,
            || -> Result<_, EngineState> {
                let path = locate_engine(MEETINGD).ok_or(EngineState::Missing)?;
                let (engine, rx) =
                    MeetingEngine::spawn(&path).map_err(|e| EngineState::Failed(e.to_string()))?;
                engine
                    .initialize()
                    .map_err(|e| EngineState::Failed(format!("initialize：{e}")))?;
                Ok((engine, rx))
            },
            |this, res, cx| {
                match res {
                    Ok((engine, rx)) => {
                        this.meeting_engine = Some(engine);
                        this.meeting_state = EngineState::Ready;
                        this.drain_meeting_events(rx, cx);
                        this.refresh_calendar(cx);
                        this.refresh_inputs(cx);
                        this.refresh_permissions(cx);
                    }
                    Err(state) => {
                        let msg = state.describe("字幕引擎 mm-meetingd ");
                        this.meeting_state = state;
                        this.events = Loadable::Failed(msg.clone().into());
                        this.inputs = Loadable::Failed(msg.clone().into());
                        this.sources = Loadable::Failed(msg.clone().into());
                        this.notify(NoticeKind::Warning, msg, cx);
                    }
                }
                cx.notify();
            },
        );
    }

    /// Drains `mm-meetingd` notifications on the foreground.
    ///
    /// Params: `rx` – event stream; `cx`.
    fn drain_meeting_events(
        &mut self,
        rx: async_channel::Receiver<EngineEvent>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            while let Ok(ev) = rx.recv().await {
                let closed = ev == EngineEvent::Closed;
                if this.update(cx, |s, cx| s.on_meeting_event(ev, cx)).is_err() || closed {
                    break;
                }
            }
        })
        .detach();
    }

    /// Applies one engine event.
    ///
    /// Params: `ev`; `cx`.
    fn on_meeting_event(&mut self, ev: EngineEvent, cx: &mut Context<Self>) {
        match ev {
            EngineEvent::Notification { method, params } => match decode_notice(&method, params) {
                Ok(n) => self.on_notice(n, cx),
                Err(e) => tracing::warn!("{e}"),
            },
            EngineEvent::Request { method, .. } => {
                tracing::warn!("mm-meetingd sent an unexpected request {method}; ignored")
            }
            EngineEvent::ProtocolError(e) => tracing::warn!("mm-meetingd protocol error: {e}"),
            EngineEvent::Closed => {
                self.meeting_engine = None;
                if self.live.is_some() {
                    self.notify(NoticeKind::Error, "字幕引擎在会议中退出。已保存的字幕和录音不会丢失，请点击“结束记录”完成整理。", cx);
                }
                if self.meeting_restarts == 0 {
                    self.meeting_restarts += 1;
                    self.meeting_state = EngineState::Failed("已退出".into());
                    self.notify(NoticeKind::Warning, "字幕引擎意外退出，正在重新启动。", cx);
                    self.ensure_meeting_engine(cx);
                } else {
                    self.meeting_state = EngineState::Failed("再次退出，请在设置中手动重启".into());
                    self.notify(
                        NoticeKind::Error,
                        "字幕引擎再次退出，请在设置中手动重启。",
                        cx,
                    );
                }
            }
        }
        cx.notify();
    }

    /// Applies a decoded meeting notice to the live session.
    ///
    /// Params: `n`; `cx`.
    fn on_notice(&mut self, n: MeetingNotice, cx: &mut Context<Self>) {
        let Some(live) = self.live.as_mut() else {
            return;
        };
        match n {
            MeetingNotice::Partial(p) => {
                live.board.apply_partial(&p);
            }
            MeetingNotice::Final(f) => {
                if let Some(seg) = live.board.apply_final(&f, live.meeting.id) {
                    self.writer.send(WriteMsg::Segment(seg, f.utterance_id));
                    if live.last_sync.elapsed() >= SYNC_EVERY {
                        live.last_sync = Instant::now();
                        self.writer.send(WriteMsg::Sync(
                            Box::new(self.cfg.clone()),
                            Box::new(live.meeting.clone()),
                        ));
                    }
                }
            }
            MeetingNotice::Relabel(r) => {
                if r.session_id == live.started.session_id {
                    live.positions.relabel(&r.from, &r.to);
                }
                if live.board.apply_relabel(&r) > 0 || r.session_id == live.started.session_id {
                    self.writer
                        .send(WriteMsg::Relabel(live.meeting.id, r.from, r.to));
                }
            }
            MeetingNotice::Levels(l) => {
                if l.session_id == live.started.session_id {
                    live.levels.insert(l.source, l);
                }
            }
            MeetingNotice::State(s) => {
                if s.session_id == live.started.session_id {
                    live.capture_state = s.state;
                    live.capture_error = s.error.clone();
                    if s.state == CaptureState::Failed {
                        let msg =
                            format!("录制失败：{}", s.error.unwrap_or_else(|| "未知原因".into()));
                        self.notify(NoticeKind::Error, msg, cx);
                    }
                }
            }
            MeetingNotice::Spatial(c) => {
                if c.session_id == live.started.session_id
                    && let Some(x) = c.lateral()
                {
                    live.positions.apply(&c.speaker, x, c.confidence);
                }
            }
            MeetingNotice::Other { .. } => {}
        }
    }

    /// Restarts the meeting engine on user request (resets the auto-restart budget).
    ///
    /// Params: `cx`.
    pub fn restart_meeting_engine(&mut self, cx: &mut Context<Self>) {
        if let Some(e) = self.meeting_engine.take() {
            cx.background_spawn(async move { e.shutdown() }).detach();
        }
        self.meeting_restarts = 0;
        self.meeting_state = EngineState::NotStarted;
        self.ensure_meeting_engine(cx);
    }

    /// Starts `mm-embedd` if needed and loads an embedding checkpoint.
    ///
    /// Params: `dir` – checkpoint folder; `cx`.
    pub fn load_embedding(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        self.embed_status = Loadable::Loading;
        cx.notify();
        let existing = self.embed_engine.clone();
        bg(
            cx,
            move || -> Result<_, LoadError> {
                let (engine, rx) = match existing {
                    Some(e) => (e, None),
                    None => {
                        let path = locate_engine(EMBEDD).ok_or_else(|| {
                            LoadError::from(
                                "找不到嵌入引擎 mm-embedd，请先运行 make engines".to_string(),
                            )
                        })?;
                        let (e, rx) = EmbedEngine::spawn(&path)?;
                        (e, Some(rx))
                    }
                };
                let status = engine.load(&dir)?;
                Ok((engine, rx, status, dir))
            },
            |this, res, cx| {
                match res {
                    Ok((engine, rx, status, dir)) => {
                        if let Some(rx) = rx {
                            this.drain_embed_events(rx, cx);
                        }
                        this.embed_engine = Some(engine.clone());
                        this.embed_state = EngineState::Ready;
                        this.indexer
                            .send(IndexerMsg::SetEmbedder(Some(Arc::new(engine))));
                        this.embed_status = Loadable::Ready(status);
                        if this.cfg.embedding_model_dir.as_ref() != Some(&dir) {
                            this.update_config(cx, |c| c.embedding_model_dir = Some(dir));
                        }
                    }
                    Err(e) => {
                        let msg = format!("加载嵌入模型失败：{}", e.message);
                        this.embed_status = Loadable::Failed(e);
                        this.notify(NoticeKind::Error, msg, cx);
                    }
                }
                cx.notify();
            },
        );
    }

    /// Unloads the embedding model and stops background embedding.
    ///
    /// Params: `cx`.
    pub fn unload_embedding(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.embed_engine.clone() else {
            return;
        };
        self.indexer.send(IndexerMsg::SetEmbedder(None));
        bg(
            cx,
            move || engine.unload(),
            |this, res, cx| {
                match res {
                    Ok(()) => this.embed_status = Loadable::Ready(EmbedStatus::default()),
                    Err(e) => this.notify(NoticeKind::Error, format!("卸载嵌入模型失败：{e}"), cx),
                }
                cx.notify();
            },
        );
    }

    /// Loads the local minutes LLM (starting `mm-embedd` if needed).
    ///
    /// Params: `dir` – model folder; `cx`.
    pub fn load_local_llm(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        self.llm_status = Loadable::Loading;
        cx.notify();
        let existing = self.embed_engine.clone();
        bg(
            cx,
            move || -> Result<_, LoadError> {
                let (engine, rx) = match existing {
                    Some(e) => (e, None),
                    None => {
                        let path = locate_engine(EMBEDD).ok_or_else(|| {
                            LoadError::from("找不到 mm-embedd，请先运行 make engines".to_string())
                        })?;
                        let (e, rx) = EmbedEngine::spawn(&path)?;
                        (e, Some(rx))
                    }
                };
                let status = engine.llm_load(&dir)?;
                Ok((engine, rx, status, dir))
            },
            |this, res, cx| {
                match res {
                    Ok((engine, rx, status, dir)) => {
                        if let Some(rx) = rx {
                            this.drain_embed_events(rx, cx);
                        }
                        this.embed_engine = Some(engine);
                        this.embed_state = EngineState::Ready;
                        this.llm_status = Loadable::Ready(status);
                        if this.cfg.local_llm_dir.as_ref() != Some(&dir) {
                            this.update_config(cx, |c| c.local_llm_dir = Some(dir));
                        }
                    }
                    Err(e) => {
                        let msg = format!("加载本地纪要模型失败：{}", e.message);
                        this.llm_status = Loadable::Failed(e);
                        this.notify(NoticeKind::Error, msg, cx);
                    }
                }
                cx.notify();
            },
        );
    }

    /// Unloads the local minutes LLM.
    ///
    /// Params: `cx`.
    pub fn unload_local_llm(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.embed_engine.clone() else {
            return;
        };
        bg(
            cx,
            move || engine.llm_unload(),
            |this, res, cx| {
                match res {
                    Ok(()) => this.llm_status = Loadable::Ready(LlmStatus::default()),
                    Err(e) => {
                        this.notify(NoticeKind::Error, format!("卸载本地纪要模型失败：{e}"), cx)
                    }
                }
                cx.notify();
            },
        );
    }

    /// Watches `mm-embedd` for exit and restarts it once.
    ///
    /// Params: `rx`; `cx`.
    fn drain_embed_events(
        &mut self,
        rx: async_channel::Receiver<EngineEvent>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            while let Ok(ev) = rx.recv().await {
                if ev != EngineEvent::Closed {
                    continue;
                }
                let _ = this.update(cx, |s, cx| {
                    s.embed_engine = None;
                    s.indexer.send(IndexerMsg::SetEmbedder(None));
                    s.embed_status = Loadable::Idle;
                    s.llm_status = Loadable::Idle;
                    if s.embed_restarts == 0
                        && let Some(dir) = s.cfg.embedding_model_dir.clone()
                    {
                        s.embed_restarts += 1;
                        s.embed_state = EngineState::Starting;
                        s.notify(NoticeKind::Warning, "嵌入引擎意外退出，正在重新启动。", cx);
                        s.load_embedding(dir, cx);
                    } else {
                        s.embed_state = EngineState::Failed("已退出".into());
                        s.notify(
                            NoticeKind::Error,
                            "嵌入引擎已退出，搜索暂时只用关键词。",
                            cx,
                        );
                    }
                    cx.notify();
                });
                break;
            }
        })
        .detach();
    }

    // ---- background refreshes ----------------------------------------------------------

    /// Reloads recent meetings and the action items of their minutes.
    ///
    /// Params: `cx`.
    pub fn refresh_meetings(&mut self, cx: &mut Context<Self>) {
        let store = self.store.clone();
        bg(
            cx,
            move || -> Result<_, String> {
                let s = lock(&store)?;
                let meetings = s
                    .recent_meetings(100)
                    .map_err(|e| format!("读取会议列表失败：{e}"))?;
                let mut with_minutes = Vec::new();
                for m in meetings.iter().take(15) {
                    if let Ok(Some(v)) = s.minutes(m.id)
                        && let Some(min) = logic::parse_minutes(&v)
                    {
                        with_minutes.push((m.title.clone(), min));
                    }
                }
                Ok((meetings, logic::action_rows(&with_minutes, 12)))
            },
            |this, res, cx| {
                match res {
                    Ok((m, a)) => {
                        this.meetings = Loadable::Ready(m);
                        this.actions = a;
                    }
                    Err(e) => this.meetings = Loadable::Failed(e.into()),
                }
                cx.notify();
            },
        );
    }

    /// Reloads people and voiceprints.
    ///
    /// Params: `cx`.
    pub fn refresh_people(&mut self, cx: &mut Context<Self>) {
        let store = self.store.clone();
        bg(
            cx,
            move || -> Result<Vec<PersonRow>, String> {
                let s = lock(&store)?;
                let people = s.people().map_err(|e| format!("读取人物失败：{e}"))?;
                let prints = s
                    .voiceprints(None)
                    .map_err(|e| format!("读取声纹失败：{e}"))?;
                Ok(people
                    .into_iter()
                    .map(|(p, hue)| PersonRow {
                        hue: hue.unwrap_or_else(|| person_hue(p.id)),
                        voiceprints: prints
                            .iter()
                            .filter(|v| v.person_id == p.id)
                            .cloned()
                            .collect(),
                        person: p,
                    })
                    .collect())
            },
            |this, res, cx| {
                this.people = match res {
                    Ok(p) => Loadable::Ready(p),
                    Err(e) => Loadable::Failed(e.into()),
                };
                cx.notify();
            },
        );
    }

    /// Reloads index counters.
    ///
    /// Params: `cx`.
    pub fn refresh_stats(&mut self, cx: &mut Context<Self>) {
        let store = self.store.clone();
        bg(
            cx,
            move || {
                lock(&store)
                    .and_then(|s| s.index_stats(&current_stamp()).map_err(|e| e.to_string()))
            },
            |this, res, cx| {
                match res {
                    Ok(s) => this.index_stats = Some(s),
                    Err(e) => tracing::warn!("index stats: {e}"),
                }
                cx.notify();
            },
        );
    }

    /// Loads calendar events for the next 7 days (today included).
    ///
    /// Params: `cx`.
    pub fn refresh_calendar(&mut self, cx: &mut Context<Self>) {
        let engine = match self.meeting_engine() {
            Ok(e) => e,
            Err(msg) => {
                if self.meeting_state != EngineState::Starting {
                    self.events = Loadable::Failed(msg.into());
                }
                cx.notify();
                return;
            }
        };
        self.events = Loadable::Loading;
        let (from, to) = logic::local_days(Local::now().date_naive(), 7);
        bg(
            cx,
            move || engine.calendar_events(from, to),
            |this, res, cx| {
                this.events = match res {
                    Ok(ev) => Loadable::Ready(ev),
                    Err(e) => Loadable::Failed(e.into()),
                };
                cx.notify();
            },
        );
    }

    /// Reads the app's privacy grants from the engine (no prompts).
    ///
    /// Params: `cx`.
    pub fn refresh_permissions(&mut self, cx: &mut Context<Self>) {
        let engine = match self.meeting_engine() {
            Ok(e) => e,
            Err(msg) => {
                self.permissions = Loadable::Failed(msg.into());
                cx.notify();
                return;
            }
        };
        self.permissions = Loadable::Loading;
        bg(
            cx,
            move || engine.permissions(),
            |this, res, cx| {
                this.permissions = match res {
                    Ok(p) => Loadable::Ready(p),
                    Err(e) => Loadable::Failed(e.into()),
                };
                cx.notify();
            },
        );
    }

    /// Asks macOS for one permission, then refreshes the window list and calendar that depend on it.
    ///
    /// Params: `kind` – `screenCapture`, `microphone` or `calendar`; `cx`.
    pub fn request_permission(&mut self, kind: &'static str, cx: &mut Context<Self>) {
        let engine = match self.meeting_engine() {
            Ok(e) => e,
            Err(msg) => {
                self.notify(NoticeKind::Error, msg, cx);
                return;
            }
        };
        bg(
            cx,
            move || engine.request_permission(kind),
            move |this, res, cx| {
                match res {
                    Ok(p) => {
                        this.permissions = Loadable::Ready(p);
                        this.refresh_sources(cx);
                        this.refresh_calendar(cx);
                    }
                    Err(e) => this.notify(NoticeKind::Error, e.to_string(), cx),
                }
                cx.notify();
            },
        );
    }

    /// Lists microphones.
    ///
    /// Params: `cx`.
    pub fn refresh_inputs(&mut self, cx: &mut Context<Self>) {
        let engine = match self.meeting_engine() {
            Ok(e) => e,
            Err(msg) => {
                self.inputs = Loadable::Failed(msg.into());
                cx.notify();
                return;
            }
        };
        self.inputs = Loadable::Loading;
        bg(
            cx,
            move || engine.probe_inputs(),
            |this, res, cx| {
                this.inputs = match res {
                    Ok(v) => Loadable::Ready(v),
                    Err(e) => Loadable::Failed(e.into()),
                };
                cx.notify();
            },
        );
    }

    /// Lists recordable windows (needs Screen Recording permission).
    ///
    /// Params: `cx`.
    pub fn refresh_sources(&mut self, cx: &mut Context<Self>) {
        let engine = match self.meeting_engine() {
            Ok(e) => e,
            Err(msg) => {
                self.sources = Loadable::Failed(msg.into());
                cx.notify();
                return;
            }
        };
        self.sources = Loadable::Loading;
        bg(
            cx,
            move || engine.list_sources(),
            |this, res, cx| {
                this.sources = match res {
                    Ok(v) => Loadable::Ready(v),
                    Err(e) => Loadable::Failed(e.into()),
                };
                cx.notify();
            },
        );
    }

    /// Probes Codex (installed, signed in).
    ///
    /// Params: `cx`.
    pub fn refresh_codex(&mut self, cx: &mut Context<Self>) {
        self.codex = Loadable::Loading;
        bg(cx, pipeline::detect_codex, |this, status, cx| {
            this.codex = Loadable::Ready(status);
            cx.notify();
        });
    }

    /// Checks whether the configured BYOK provider has a key (never reads it into the UI).
    ///
    /// Params: `cx`.
    pub fn refresh_key_presence(&mut self, cx: &mut Context<Self>) {
        let Some(p) = self.cfg.byok.clone() else {
            self.byok_key_present = false;
            return;
        };
        bg(
            cx,
            move || mm_llm::byok::read_key(&p).map(|k| k.is_some()),
            |this, res, cx| {
                match res {
                    Ok(present) => this.byok_key_present = present,
                    Err(e) => this.notify(NoticeKind::Error, format!("读取钥匙串失败：{e}"), cx),
                }
                cx.notify();
            },
        );
    }

    /// Checks whether a Hugging Face token is stored in the Keychain.
    ///
    /// Params: `cx`.
    pub fn refresh_hf_token(&mut self, cx: &mut Context<Self>) {
        bg(
            cx,
            || hub::read_stored_token().map(|t| t.is_some_and(|t| !t.trim().is_empty())),
            |this, res, cx| {
                match res {
                    Ok(present) => this.hf_token_present = present,
                    Err(e) => this.notify(NoticeKind::Error, format!("读取钥匙串失败：{e}"), cx),
                }
                cx.notify();
            },
        );
    }

    /// Stores or deletes the Hugging Face token in the Keychain.
    ///
    /// Params: `token` – `None` deletes; `cx`.
    pub fn write_hf_token(&mut self, token: Option<String>, cx: &mut Context<Self>) {
        bg(
            cx,
            move || hub::write_stored_token(token.as_deref()).map(|_| token.is_some()),
            |this, res, cx| {
                match res {
                    Ok(saved) => {
                        this.hf_token_present = saved;
                        let msg = if saved {
                            "Hugging Face 令牌已保存到钥匙串。"
                        } else {
                            "Hugging Face 令牌已删除。"
                        };
                        this.notify(NoticeKind::Success, msg, cx);
                    }
                    Err(e) => this.notify(NoticeKind::Error, format!("写入钥匙串失败：{e}"), cx),
                }
                cx.notify();
            },
        );
    }

    /// Downloads the embedding model from Hugging Face, then loads it.
    ///
    /// Params: `url` – model page URL or `owner/name`; `cx`.
    pub fn download_embedding(&mut self, url: String, cx: &mut Context<Self>) {
        if self.model_download.is_some() {
            self.notify(NoticeKind::Warning, "已有模型正在下载。", cx);
            return;
        }
        let repo = match HubRepo::parse(&url) {
            Ok(r) => r,
            Err(e) => {
                self.notify(NoticeKind::Warning, e.to_string(), cx);
                return;
            }
        };
        if self.cfg.embedding_model_url != url.trim() {
            let u = url.trim().to_string();
            self.update_config(cx, |c| c.embedding_model_url = u);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.model_download = Some(ModelDownload {
            repo: repo.id.clone(),
            progress: hub::Progress::default(),
            cancel: cancel.clone(),
        });
        cx.notify();
        let root = models_dir(&self.data_dir);
        let (tx, rx) = async_channel::unbounded::<DownloadMsg>();
        cx.background_spawn(async move {
            let res = hub::resolve_token().and_then(|token| {
                hub::download(&repo, &root, token.as_deref(), &cancel, &mut |p| {
                    let _ = tx.try_send(DownloadMsg::Progress(p.clone()));
                })
            });
            let _ = tx.try_send(DownloadMsg::Done(res));
        })
        .detach();
        cx.spawn(async move |this, cx| {
            while let Ok(msg) = rx.recv().await {
                let done = matches!(msg, DownloadMsg::Done(_));
                let _ = this.update(cx, |s, cx| {
                    s.on_download_msg(msg, cx);
                    cx.notify();
                });
                if done {
                    break;
                }
            }
        })
        .detach();
    }

    /// Applies one message from the download thread.
    ///
    /// Params: `msg`; `cx`.
    fn on_download_msg(&mut self, msg: DownloadMsg, cx: &mut Context<Self>) {
        match msg {
            DownloadMsg::Progress(p) => {
                if let Some(d) = &mut self.model_download {
                    d.progress = p;
                }
            }
            DownloadMsg::Done(res) => {
                self.model_download = None;
                match res {
                    Ok(dir) => {
                        self.notify(NoticeKind::Success, "嵌入模型已下载，正在加载。", cx);
                        self.load_embedding(dir, cx);
                    }
                    Err(hub::HubError::Cancelled) => {
                        self.notify(NoticeKind::Info, "已取消下载，下次会从断点继续。", cx)
                    }
                    Err(e) => self.notify(NoticeKind::Error, e.to_string(), cx),
                }
            }
        }
    }

    /// Stops the running download (partial files stay for resuming).
    ///
    /// Params: `cx`.
    pub fn cancel_download(&mut self, cx: &mut Context<Self>) {
        if let Some(d) = &self.model_download {
            d.cancel.store(true, Ordering::Relaxed);
        }
        cx.notify();
    }

    // ---- settings ----------------------------------------------------------------------

    /// Applies a settings change and saves `config.json` in the background.
    ///
    /// Params: `cx`; `f` – mutation.
    pub fn update_config(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut AppConfig)) {
        let before = self.cfg.clone();
        f(&mut self.cfg);
        if self.cfg == before {
            return;
        }
        if self.cfg.vault_path != before.vault_path {
            self.indexer
                .send(IndexerMsg::SetVault(self.cfg.vault_path.clone()));
        }
        if self.cfg.byok != before.byok {
            self.refresh_key_presence(cx);
        }
        let cfg = self.cfg.clone();
        let dir = self.data_dir.clone();
        bg(
            cx,
            move || cfg.save(&dir),
            |this, res, cx| {
                if let Err(e) = res {
                    this.notify(NoticeKind::Error, e.to_string(), cx);
                }
            },
        );
        cx.notify();
    }

    /// Stores or deletes the BYOK key in the Keychain.
    ///
    /// Params: `key` – `None` deletes; `cx`.
    pub fn write_byok_key(&mut self, key: Option<String>, cx: &mut Context<Self>) {
        let Some(p) = self.cfg.byok.clone() else {
            self.notify(NoticeKind::Warning, "请先选择提供方并填写模型。", cx);
            return;
        };
        bg(
            cx,
            move || mm_llm::byok::write_key(&p, key.as_deref()).map(|_| key.is_some()),
            |this, res, cx| {
                match res {
                    Ok(saved) => {
                        this.byok_key_present = saved;
                        let msg = if saved {
                            "API 密钥已保存到钥匙串。"
                        } else {
                            "API 密钥已删除。"
                        };
                        this.notify(NoticeKind::Success, msg, cx);
                    }
                    Err(e) => this.notify(NoticeKind::Error, format!("写入钥匙串失败：{e}"), cx),
                }
                cx.notify();
            },
        );
    }

    // ---- live session ------------------------------------------------------------------

    /// Starts an ad-hoc meeting now ("开始记录").
    ///
    /// Params: `title` – may be empty; `cx`.
    pub fn quick_record(&mut self, title: String, cx: &mut Context<Self>) {
        if self.guard_not_recording(cx) {
            return;
        }
        let store = self.store.clone();
        bg(
            cx,
            move || {
                lock(&store).and_then(|s| {
                    flow::quick_meeting(&s, &title).map_err(|e| format!("创建会议失败：{e}"))
                })
            },
            |this, res, cx| match res {
                Ok(m) => this.start_recording(m, vec![], cx),
                Err(e) => this.notify(NoticeKind::Error, e, cx),
            },
        );
    }

    /// Starts recording a calendar event.
    ///
    /// Params: `ev`; `cx`.
    pub fn record_event(&mut self, ev: CalendarEvent, cx: &mut Context<Self>) {
        if self.guard_not_recording(cx) {
            return;
        }
        let store = self.store.clone();
        let attendees = ev.attendees.clone();
        bg(
            cx,
            move || {
                lock(&store).and_then(|s| {
                    flow::meeting_for_event(&s, &ev)
                        .map_err(|e| format!("创建会议失败（{}）：{e}", ev.title))
                })
            },
            move |this, res, cx| match res {
                Ok(m) => this.start_recording(m, attendees, cx),
                Err(e) => this.notify(NoticeKind::Error, e, cx),
            },
        );
    }

    /// Returns `true` (and tells the user) when a session is running or starting.
    fn guard_not_recording(&mut self, cx: &mut Context<Self>) -> bool {
        if self.live.is_some() || self.live_starting {
            self.notify(NoticeKind::Info, "已经在记录中。", cx);
            self.navigate(Page::Live, cx);
            return true;
        }
        if self.post.as_ref().is_some_and(|p| p.running) {
            self.notify(
                NoticeKind::Info,
                "上一场会议还在整理，完成后再开始新的记录。",
                cx,
            );
            return true;
        }
        false
    }

    /// note → preset → capture/start → mark live.
    ///
    /// Params: `meeting`; `attendees` – names for the note; `cx`.
    pub fn start_recording(
        &mut self,
        meeting: Meeting,
        attendees: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let engine = match self.meeting_engine() {
            Ok(e) => e,
            Err(msg) => {
                self.notify(NoticeKind::Error, format!("无法开始记录：{msg}"), cx);
                return;
            }
        };
        self.live_starting = true;
        // capture/start blocks until macOS answers permission prompts; tell the user what
        // they may be waiting for instead of leaving a silent spinner.
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(START_HINT_AFTER).await;
            let _ = this.update(cx, |s, cx| {
                if s.live_starting {
                    s.notify(
                        NoticeKind::Warning,
                        "开始录制还没有完成。如果系统弹出了麦克风或屏幕录制权限请求，请先允许；也可以在“系统设置 › 隐私与安全性”里检查。",
                        cx,
                    );
                }
            });
        })
        .detach();
        self.navigate(Page::Live, cx);
        cx.notify();
        let cfg = self.cfg.clone();
        let store = self.store.clone();
        let window_id = self.selected_window;
        // The engine needs an explicit microphone UID; "system default" (None) is resolved
        // here from the probed inputs (service-layer gap, see logic::default_microphone).
        let fallback_mic = self
            .inputs
            .ready()
            .and_then(|ins| logic::default_microphone(ins));
        bg(
            cx,
            move || -> Result<_, String> {
                let mut meeting = meeting;
                let (note, preset, directory) = {
                    let s = lock(&store)?;
                    let note = flow::ensure_note(&cfg, &s, &mut meeting, &attendees)
                        .map_err(|e| format!("创建会议笔记失败：{e}"))?;
                    let mut preset = flow::build_preset(&cfg, &s, &meeting, window_id)
                        .map_err(|e| format!("准备录制参数失败：{e}"))?;
                    if preset.microphone_uid.is_none() {
                        preset.microphone_uid = fallback_mic;
                    }
                    let dir = flow::directory_for(&s, meeting.id).map_err(|e| e.to_string())?;
                    (note, preset, dir)
                };
                // Without Screen Recording permission, keep the meeting going on the microphone
                // alone rather than failing the whole start.
                let (started, degraded) = match engine.capture_start(&preset) {
                    Ok(s) => (s, None),
                    Err(mm_service::ServiceError::Permission(why)) => {
                        match flow::without_screen_capture(&preset) {
                            Some(reduced) => (
                                engine
                                    .capture_start(&reduced)
                                    .map_err(|e| format!("开始录制失败：{e}"))?,
                                Some(format!(
                                    "没有屏幕录制权限，只录麦克风（不录窗口和系统声音）。在系统设置 › 隐私与安全性 › 屏幕与系统录音中允许后即可恢复。（{why}）"
                                )),
                            ),
                            None => return Err(format!("开始录制失败：{why}")),
                        }
                    }
                    Err(e) => return Err(format!("开始录制失败：{e}")),
                };
                flow::mark_live(&*lock(&store)?, &mut meeting, &started)
                    .map_err(|e| format!("更新会议状态失败：{e}"))?;
                let notes = note
                    .as_ref()
                    .and_then(|p| std::fs::read_to_string(p).ok())
                    .map(|md| flow::notes_section(&md))
                    .unwrap_or_default();
                Ok((meeting, note, started, directory, notes, degraded))
            },
            move |this, res, cx| {
                this.live_starting = false;
                match res {
                    Ok((meeting, note_path, started, directory, notes, degraded)) => {
                        if let Some(warning) = degraded {
                            this.notify(NoticeKind::Warning, warning, cx);
                        }
                        let mut board = CaptionBoard::default();
                        board.begin(&started.session_id);
                        if note_path.is_none() {
                            this.notify(
                                NoticeKind::Warning,
                                "还没有设置 Obsidian 库，笔记不会写入文件。可以在设置中选择。",
                                cx,
                            );
                        }
                        this.notify(
                            NoticeKind::Success,
                            format!("开始记录：{}", meeting.title),
                            cx,
                        );
                        this.live = Some(LiveSession {
                            meeting,
                            started,
                            started_at: Instant::now(),
                            board,
                            directory,
                            capture_state: CaptureState::Recording,
                            capture_error: None,
                            levels: HashMap::new(),
                            positions: logic::SpeakerPositions::default(),
                            note_path,
                            window_id,
                            last_sync: Instant::now(),
                        });
                        this.post = None;
                        this.indexer.send(IndexerMsg::SetLive(true));
                        cx.emit(AppEvent::LiveStarted { notes });
                        this.start_ticker(cx);
                        if this.cfg.floating_captions {
                            this.open_floating(cx);
                        }
                        this.refresh_meetings(cx);
                    }
                    Err(e) => this.notify(NoticeKind::Error, e, cx),
                }
                cx.notify();
            },
        );
    }

    /// Re-renders once per second while recording (timer, countdowns).
    ///
    /// Params: `cx`.
    fn start_ticker(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let keep = this.update(cx, |s, cx| {
                    cx.notify();
                    s.live.is_some() || s.enrolling.is_some()
                });
                if !matches!(keep, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
    }

    /// Queues the notes text for a debounced write into the note's 笔记 section.
    ///
    /// Params: `text` – full editor text; `cx`.
    pub fn notes_changed(&mut self, text: String, cx: &mut Context<Self>) {
        self.notes_pending = Some(text);
        self.notes_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(NOTES_DEBOUNCE).await;
            let _ = this.update(cx, |s, cx| s.flush_notes(cx));
        }));
    }

    /// Writes pending notes now (background).
    ///
    /// Params: `cx`.
    pub fn flush_notes(&mut self, cx: &mut Context<Self>) {
        let Some(text) = self.notes_pending.take() else {
            return;
        };
        let Some(path) = self.live.as_ref().and_then(|l| l.note_path.clone()) else {
            return;
        };
        let indexer = self.indexer.clone();
        bg(
            cx,
            move || write_notes(&path, &text, &indexer),
            |this, res, cx| {
                if let Err(e) = res {
                    this.notify(NoticeKind::Error, e, cx);
                }
            },
        );
    }

    /// Takes a screenshot of the recorded window (or the screen) into the vault.
    ///
    /// Params: `cx`; `done` – receives the Obsidian embed text to insert.
    pub fn screenshot(
        &mut self,
        cx: &mut Context<Self>,
        done: impl FnOnce(String, &mut App) + 'static,
    ) {
        let Some(vault) = self.cfg.vault_path.clone() else {
            self.notify(
                NoticeKind::Warning,
                "请先在设置中选择 Obsidian 库，截图会保存到库的附件文件夹。",
                cx,
            );
            return;
        };
        let window_id = self.live.as_ref().and_then(|l| l.window_id);
        let dir = logic::attachments_dir(&vault, &self.cfg.meetings_folder);
        bg(
            cx,
            move || -> Result<String, String> {
                std::fs::create_dir_all(&dir)
                    .map_err(|e| format!("创建 {} 失败：{e}", dir.display()))?;
                let name = logic::screenshot_name(Local::now());
                let out = dir.join(&name);
                let status = std::process::Command::new("/usr/sbin/screencapture")
                    .args(logic::screencapture_args(window_id, &out))
                    .status()
                    .map_err(|e| format!("运行 screencapture 失败：{e}"))?;
                if !status.success() || !out.exists() {
                    return Err(format!(
                        "截图失败（screencapture 退出码 {status}）。请检查屏幕录制权限。"
                    ));
                }
                Ok(format!("![[{name}]]"))
            },
            |this, res, cx| match res {
                Ok(embed) => done(embed, cx),
                Err(e) => this.notify(NoticeKind::Error, e, cx),
            },
        );
    }

    /// capture/stop → mark stopped → refine → transcript → minutes, with progress.
    ///
    /// Params: `cx`.
    pub fn stop_recording(&mut self, cx: &mut Context<Self>) {
        let Some(live) = self.live.take() else { return };
        self.notes_task = None;
        let notes = self.notes_pending.take().zip(live.note_path.clone());
        self.indexer.send(IndexerMsg::SetLive(false));
        self.close_floating(cx);
        self.post = Some(PostMeeting {
            meeting_id: live.meeting.id,
            title: live.meeting.title.clone(),
            steps: vec![],
            warnings: vec![],
            running: true,
        });
        cx.notify();

        let (ptx, prx) = async_channel::unbounded::<String>();
        cx.spawn(async move |this, cx| {
            while let Ok(step) = prx.recv().await {
                let _ = this.update(cx, |s, cx| {
                    if let Some(p) = s.post.as_mut() {
                        p.steps.push(step);
                    }
                    cx.notify();
                });
            }
        })
        .detach();
        let job = FinishJob {
            minutes: self.minutes_ctx(),
            engine: self.meeting_engine.clone(),
            writer: self.writer.clone(),
            elapsed_ms: live.elapsed_ms(),
            meeting: live.meeting,
            started: live.started,
        };
        let indexer = self.indexer.clone();
        bg(
            cx,
            move || {
                if let Some((text, path)) = notes
                    && let Err(e) = write_notes(&path, &text, &indexer)
                {
                    let _ = ptx.send_blocking(e);
                }
                pipeline::finish_meeting(job, &|s| {
                    let _ = ptx.send_blocking(s);
                })
            },
            |this, res, cx| {
                match res {
                    Ok(report) => {
                        if let Some(p) = this.post.as_mut() {
                            p.running = false;
                            p.warnings = report.warnings.clone();
                        }
                        let msg = if report.minutes_done {
                            format!("{}：纪要已生成", report.meeting.title)
                        } else {
                            format!("{}：录制已结束，转写已保存", report.meeting.title)
                        };
                        this.notify(NoticeKind::Success, msg, cx);
                        for w in report.warnings {
                            this.notify(NoticeKind::Warning, w, cx);
                        }
                        cx.emit(AppEvent::MeetingChanged(report.meeting.id));
                    }
                    Err(e) => {
                        if let Some(p) = this.post.as_mut() {
                            p.running = false;
                            p.warnings.push(e.clone());
                        }
                        this.notify(
                            NoticeKind::Error,
                            format!("会后整理失败，录音已保留：{e}"),
                            cx,
                        );
                    }
                }
                this.refresh_meetings(cx);
                this.refresh_stats(cx);
                cx.notify();
            },
        );
    }

    /// Material for minutes generation.
    ///
    /// Returns: a context movable to a background thread.
    fn minutes_ctx(&self) -> MinutesCtx {
        MinutesCtx {
            cfg: self.cfg.clone(),
            store: self.store.clone(),
            codex: self.codex.ready().cloned(),
            embed: self.embed_engine.clone(),
        }
    }

    /// Regenerates minutes for a stored meeting.
    ///
    /// Params: `id`; `cx`.
    pub fn regenerate_minutes(&mut self, id: MeetingId, cx: &mut Context<Self>) {
        let ctx = self.minutes_ctx();
        self.notify(NoticeKind::Info, "正在生成纪要，可能需要几分钟…", cx);
        bg(
            cx,
            move || -> Result<(Meeting, String), String> {
                let mut m = lock(&ctx.store)?
                    .meeting(id)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("找不到会议 {id}"))?;
                let label = pipeline::generate_minutes(&ctx, &mut m)?;
                Ok((m, label))
            },
            move |this, res, cx| {
                match res {
                    Ok((m, label)) => {
                        this.notify(
                            NoticeKind::Success,
                            format!("{}：纪要已由{label}生成", m.title),
                            cx,
                        );
                        cx.emit(AppEvent::MeetingChanged(id));
                    }
                    Err(e) => this.notify(NoticeKind::Error, e, cx),
                }
                this.refresh_meetings(cx);
            },
        );
    }

    /// Names a speaker cluster of a meeting, creating the person if new.
    ///
    /// Params: `meeting`; `cluster`; `choice`; `cx`.
    pub fn assign_speaker(
        &mut self,
        meeting: MeetingId,
        cluster: u32,
        choice: PersonChoice,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.clone();
        bg(
            cx,
            move || -> Result<(SpeakerDirectory, String), String> {
                let s = lock(&store)?;
                let (pid, name) = match choice {
                    PersonChoice::Existing(id) => {
                        let name = s
                            .people()
                            .map_err(|e| e.to_string())?
                            .into_iter()
                            .find(|(p, _)| p.id == id)
                            .map(|(p, _)| p.display_name)
                            .unwrap_or_default();
                        (id, name)
                    }
                    PersonChoice::New(name) => {
                        let p = new_person(&name);
                        s.upsert_person(&p, Some(person_hue(p.id)))
                            .map_err(|e| format!("创建人物 {name} 失败：{e}"))?;
                        (p.id, name)
                    }
                };
                s.name_cluster(meeting, cluster, Some(pid))
                    .map_err(|e| format!("命名说话人失败：{e}"))?;
                Ok((
                    flow::directory_for(&s, meeting).map_err(|e| e.to_string())?,
                    name,
                ))
            },
            move |this, res, cx| {
                match res {
                    Ok((dir, name)) => {
                        if let Some(live) = this.live.as_mut().filter(|l| l.meeting.id == meeting) {
                            live.directory = dir;
                        }
                        this.notify(
                            NoticeKind::Success,
                            format!("说话人 {cluster} 已命名为 {name}，之前的发言也一起改名。"),
                            cx,
                        );
                        cx.emit(AppEvent::MeetingChanged(meeting));
                        this.refresh_people(cx);
                    }
                    Err(e) => this.notify(NoticeKind::Error, e, cx),
                }
                cx.notify();
            },
        );
    }

    // ---- people ------------------------------------------------------------------------

    /// Adds a person.
    ///
    /// Params: `name`; `cx`.
    pub fn add_person(&mut self, name: String, cx: &mut Context<Self>) {
        let name = name.trim().to_string();
        if name.is_empty() {
            self.notify(NoticeKind::Warning, "请输入名字。", cx);
            return;
        }
        let store = self.store.clone();
        bg(
            cx,
            move || {
                let p = new_person(&name);
                lock(&store)?
                    .upsert_person(&p, Some(person_hue(p.id)))
                    .map_err(|e| format!("添加 {name} 失败：{e}"))
            },
            |this, res, cx| {
                if let Err(e) = res {
                    this.notify(NoticeKind::Error, e, cx);
                }
                this.refresh_people(cx);
            },
        );
    }

    /// Deletes a person (voiceprints cascade; meeting assignments are cleared).
    ///
    /// Params: `id`; `cx`.
    pub fn delete_person(&mut self, id: PersonId, cx: &mut Context<Self>) {
        let store = self.store.clone();
        bg(
            cx,
            move || {
                lock(&store)?
                    .delete_person(id)
                    .map_err(|e| format!("删除人物失败：{e}"))
            },
            |this, res, cx| {
                if let Err(e) = res {
                    this.notify(NoticeKind::Error, e, cx);
                }
                this.refresh_people(cx);
            },
        );
    }

    /// Deletes one voiceprint sample.
    ///
    /// Params: `id`; `cx`.
    pub fn delete_voiceprint(&mut self, id: VoiceprintId, cx: &mut Context<Self>) {
        let store = self.store.clone();
        bg(
            cx,
            move || {
                lock(&store)?
                    .delete_voiceprint(id)
                    .map_err(|e| format!("删除声纹失败：{e}"))
            },
            |this, res, cx| {
                if let Err(e) = res {
                    this.notify(NoticeKind::Error, e, cx);
                }
                this.refresh_people(cx);
            },
        );
    }

    /// Records [`ENROLL_SECONDS`] from the microphone and stores the voiceprint.
    ///
    /// Params: `person`; `cx`.
    pub fn enroll_voiceprint(&mut self, person: PersonId, cx: &mut Context<Self>) {
        if self.live.is_some() {
            self.notify(
                NoticeKind::Warning,
                "会议进行中不能录入声纹，请结束记录后再试。",
                cx,
            );
            return;
        }
        let engine = match self.meeting_engine() {
            Ok(e) => e,
            Err(msg) => {
                self.notify(NoticeKind::Error, format!("无法录入声纹：{msg}"), cx);
                return;
            }
        };
        self.enrolling = Some((person, Instant::now()));
        self.start_ticker(cx);
        let mic = self.cfg.microphone_uid.clone();
        let store = self.store.clone();
        bg(
            cx,
            move || -> Result<(), String> {
                let sample = engine
                    .voiceprint_enroll(mic.as_deref(), ENROLL_SECONDS)
                    .map_err(|e| format!("录入声纹失败：{e}"))?;
                store_sample(&store, person, sample, VoiceprintSource::Enrollment)
            },
            |this, res, cx| {
                this.enrolling = None;
                match res {
                    Ok(()) => this.notify(NoticeKind::Success, "声纹已录入。", cx),
                    Err(e) => this.notify(NoticeKind::Error, e, cx),
                }
                this.refresh_people(cx);
            },
        );
    }

    /// Extracts a voiceprint from a slice of a recording.
    ///
    /// Params: `person`; `path`; `start_ms`/`end_ms` – optional slice; `cx`.
    pub fn voiceprint_from_clip(
        &mut self,
        person: PersonId,
        path: PathBuf,
        start_ms: Option<u64>,
        end_ms: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        let engine = match self.meeting_engine() {
            Ok(e) => e,
            Err(msg) => {
                self.notify(NoticeKind::Error, format!("无法提取声纹：{msg}"), cx);
                return;
            }
        };
        let store = self.store.clone();
        bg(
            cx,
            move || -> Result<(), String> {
                if !path.is_file() {
                    return Err(format!("找不到录音文件 {}", path.display()));
                }
                let req = ClipRequest {
                    path: path.clone(),
                    start_ms,
                    end_ms,
                };
                let sample = engine
                    .voiceprint_from_clip(&req)
                    .map_err(|e| format!("从 {} 提取声纹失败：{e}", path.display()))?;
                let source = VoiceprintSource::Clip {
                    asset_path: path.display().to_string(),
                    start_ms: start_ms.unwrap_or(0),
                    end_ms: end_ms.unwrap_or(0),
                };
                store_sample(&store, person, sample, source)
            },
            |this, res, cx| {
                match res {
                    Ok(()) => this.notify(NoticeKind::Success, "已从录音片段提取声纹。", cx),
                    Err(e) => this.notify(NoticeKind::Error, e, cx),
                }
                this.refresh_people(cx);
            },
        );
    }

    // ---- floating captions -------------------------------------------------------------

    /// Opens the always-on-top caption window.
    ///
    /// Params: `cx`.
    pub fn open_floating(&mut self, cx: &mut Context<Self>) {
        if self.floating.is_some() {
            return;
        }
        cx.spawn(async move |this, cx| {
            let Some(entity) = this.upgrade() else { return };
            let handle = cx.update(|cx| crate::floating::open(entity, cx));
            match handle {
                Ok(h) => {
                    let _ = this.update(cx, |s, cx| {
                        s.floating = Some(h);
                        cx.notify();
                    });
                }
                Err(e) => {
                    let _ = this.update(cx, |s, cx| {
                        s.notify(NoticeKind::Error, format!("打开浮窗字幕失败：{e}"), cx)
                    });
                }
            }
        })
        .detach();
    }

    /// Closes the caption window if open.
    ///
    /// Params: `cx`.
    pub fn close_floating(&mut self, cx: &mut Context<Self>) {
        let Some(handle) = self.floating.take() else {
            return;
        };
        cx.spawn(async move |_, cx| {
            let _ = cx.update(|cx| handle.update(cx, |_, window, _| window.remove_window()));
        })
        .detach();
        cx.notify();
    }

    /// Whether the caption window is open.
    ///
    /// Returns: `true` if open.
    pub fn floating_open(&self) -> bool {
        self.floating.is_some()
    }

    /// Records that the user closed the caption window themselves.
    pub fn floating_closed(&mut self) {
        self.floating = None;
    }

    // ---- links ---------------------------------------------------------------------------

    /// Handles a `meeting-master://open?meeting=…&t=…` URL.
    ///
    /// Params: `url`; `cx`.
    pub fn open_url(&mut self, url: &str, cx: &mut Context<Self>) {
        match flow::parse_seek_url(url) {
            Some((id, t)) => cx.emit(AppEvent::OpenMeeting(id, Some(t))),
            None => self.notify(NoticeKind::Warning, format!("无法识别的链接：{url}"), cx),
        }
    }

    /// Opens a meeting note in Obsidian.
    ///
    /// Params: `meeting`; `cx`.
    pub fn open_in_obsidian(&mut self, meeting: &Meeting, cx: &mut Context<Self>) {
        let (Some(vault), Some(note)) = (&self.cfg.vault_path, &meeting.note_path) else {
            self.notify(NoticeKind::Warning, "这场会议还没有 Obsidian 笔记。", cx);
            return;
        };
        let name = vault
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        cx.open_url(&mm_vault::open_note_uri(&name, note));
    }

    /// Opens a meeting's recording folder in Finder.
    ///
    /// Params: `meeting`; `cx`.
    pub fn open_recording(&mut self, meeting: &Meeting, cx: &mut Context<Self>) {
        match meeting.recording_dir.as_deref().map(Path::new) {
            Some(dir) if dir.exists() => cx.open_with_system(dir),
            Some(dir) => self.notify(
                NoticeKind::Warning,
                format!("录音文件夹不存在：{}", dir.display()),
                cx,
            ),
            None => self.notify(NoticeKind::Warning, "这场会议没有录音。", cx),
        }
    }
}

/// Builds a new person record.
///
/// Params: `name`.
/// Returns: the person.
fn new_person(name: &str) -> Person {
    Person {
        id: PersonId::new(),
        display_name: name.trim().to_string(),
        email: None,
        note_path: None,
    }
}

/// Stores a voiceprint sample for a person.
///
/// Params: `store`; `person`; `sample`; `source`.
/// Returns: an error message on failure.
fn store_sample(
    store: &Arc<Mutex<Store>>,
    person: PersonId,
    sample: mm_core::VoiceprintSample,
    source: VoiceprintSource,
) -> Result<(), String> {
    let v = Voiceprint {
        id: VoiceprintId::new(),
        person_id: person,
        model: sample.model,
        vector: sample.vector,
        source,
        speech_ms: sample.speech_ms,
    };
    lock(store)?
        .add_voiceprint(&v)
        .map_err(|e| format!("保存声纹失败：{e}"))
}

/// Writes the notes text into the note's 笔记 section and asks the indexer to refresh it.
///
/// Params: `path` – note; `text` – notes; `indexer`.
/// Returns: an error message with the path on failure.
pub fn write_notes(path: &Path, text: &str, indexer: &Indexer) -> Result<(), String> {
    let md = std::fs::read_to_string(path)
        .map_err(|e| format!("读取笔记 {} 失败：{e}", path.display()))?;
    let updated = logic::replace_notes_section(&md, text).ok_or_else(|| {
        format!(
            "笔记 {} 里没有“## 笔记 Notes”标题，未写入。",
            path.display()
        )
    })?;
    if updated != md {
        mm_vault::write_atomic(path, &updated)
            .map_err(|e| format!("写入笔记 {} 失败：{e}", path.display()))?;
        indexer.send(IndexerMsg::Touch(
            path.to_path_buf(),
            mm_index::Priority::Interactive,
        ));
    }
    Ok(())
}

/// Current UTC time (kept here so pages do not import chrono just for this).
///
/// Returns: now.
pub fn now() -> chrono::DateTime<Utc> {
    Utc::now()
}

#[cfg(test)]
mod tests {
    use super::{
        Arc, EngineState, Indexer, IndexerMsg, LoadError, Mutex, ServiceError, Store, write_notes,
    };

    #[test]
    fn engine_state_descriptions() {
        assert!(
            EngineState::Missing
                .describe("字幕引擎")
                .contains("make engines")
        );
        assert_eq!(EngineState::Ready.describe("字幕引擎"), "字幕引擎就绪");
    }

    #[test]
    fn permission_errors_are_flagged() {
        let e: LoadError = ServiceError::Permission("日历权限：denied".into()).into();
        assert!(e.permission);
        let e: LoadError = ServiceError::Engine("x".into()).into();
        assert!(!e.permission);
    }

    #[test]
    fn write_notes_rewrites_only_the_section() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("n.md");
        std::fs::write(&p, "# T\n\n## 笔记 Notes\n\n\n## 纪要 Minutes\nkeep\n").unwrap();
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let (ix, _ev) = Indexer::start(store, None);
        write_notes(&p, "[00:05] 要点", &ix).unwrap();
        let md = std::fs::read_to_string(&p).unwrap();
        assert!(md.contains("## 笔记 Notes\n\n[00:05] 要点\n\n## 纪要 Minutes\nkeep"));
        std::fs::write(&p, "no heading").unwrap();
        assert!(write_notes(&p, "x", &ix).unwrap_err().contains("n.md"));
        ix.send(IndexerMsg::Stop);
    }
}
