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
use mm_i18n::{tr, trf};
use mm_llm::ModelOption;
use mm_service::config::{data_dir_overridden, default_recordings_dir};
use mm_service::engines::{
    EMBEDD, EmbedEngine, EmbedStatus, LlmStatus, MEETINGD, MeetingEngine, MeetingNotice,
    SpeechModelsStatus, decode_notice, locate_engine,
};
use mm_service::hub::{self, HubRepo};
use mm_service::indexer::{Indexer, IndexerEvent, IndexerMsg, current_stamp};
use mm_service::legacy::{self, FolderMove};
use mm_service::live::{CaptionBoard, SpeakerDirectory, person_hue};
use mm_service::{AppConfig, ServiceError, data_dir, flow, models_dir};
use mm_store::{IndexStats, Store};

use crate::logic::{self, ActionRow};
use crate::pages::Page;
use crate::pipeline::{self, CodexStatus, MinutesCtx, StopJob, StoppedMeeting};
use crate::post_queue::PostQueue;
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
    /// Params: `name` – engine description in the current language, e.g. "字幕引擎 " or
    /// "Caption engine".
    /// Returns: the text.
    pub fn describe(&self, name: &str) -> String {
        // Chinese callers pass a trailing space as the separator; English needs a word gap
        // of its own, so trim there to avoid doubled spaces.
        let en = name.trim();
        match self {
            EngineState::NotStarted => trf!("{name}未启动", "{en} isn’t running"),
            EngineState::Starting => trf!("{name}正在启动", "{en} is starting"),
            EngineState::Ready => trf!("{name}就绪", "{en} is ready"),
            EngineState::Missing => trf!(
                "找不到{name}，请先运行 make engines",
                "{en} not found. Run make engines first.",
            ),
            EngineState::Failed(e) => trf!("{name}不可用：{e}", "{en} unavailable: {e}"),
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
    /// `initialize`'s `capabilities.models` (role → model id), known before they load.
    pub meeting_models: std::collections::BTreeMap<String, String>,
    /// Live caption models preloaded in `mm-meetingd` (`models/preload`).
    pub speech_models: Loadable<SpeechModelsStatus>,
    embed_engine: Option<EmbedEngine>,
    pub embed_state: EngineState,
    embed_restarts: u32,
    /// Load the configured local minutes model once the embedding load settles. They share
    /// `mm-embedd`, and starting both loads at once would spawn two engine processes.
    autoload_llm: bool,

    pub live: Option<LiveSession>,
    pub live_starting: bool,
    /// Post-meeting jobs: one refine/minutes run at a time, later meetings wait in line.
    pub post: PostQueue<StoppedMeeting>,
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
    /// Models the signed-in Codex offers (`model/list`); loaded when settings open.
    pub codex_models: Loadable<Vec<ModelOption>>,
    /// Models the signed-in Antigravity CLI offers (`agy models`); loaded when settings open.
    pub agy_models: Loadable<Vec<ModelOption>>,
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
        // The rename to Friston moved the data folder; tests that redirect it never had one.
        let legacy_data = legacy::legacy_sibling(&dir);
        let data_moved = !data_dir_overridden()
            && match legacy::move_folder(&legacy_data, &dir) {
                Ok(FolderMove::Moved) => true,
                Ok(FolderMove::BothExist) => {
                    notices.push((
                        NoticeKind::Warning,
                        trf!(
                            "旧数据文件夹 {} 还在，没有合并到 {}。确认不需要后可以删掉它。",
                            "The old data folder {} is still there and wasn’t merged into {}. You can delete it once you’re sure you don’t need it.",
                            legacy_data.display(),
                            dir.display(),
                        ),
                    ));
                    false
                }
                Ok(FolderMove::NothingToMove) => false,
                Err(e) => {
                    notices.push((
                        NoticeKind::Error,
                        trf!(
                            "迁移数据文件夹失败：{e}",
                            "Couldn’t move the data folder: {e}"
                        ),
                    ));
                    false
                }
            };
        if let Err(e) = std::fs::create_dir_all(&dir) {
            notices.push((
                NoticeKind::Error,
                trf!(
                    "无法创建数据文件夹 {}：{e}",
                    "Couldn’t create the data folder {}: {e}",
                    dir.display(),
                ),
            ));
        }
        let db = dir.join("meeting-master.db");
        let store = match Store::open(&db) {
            Ok(s) => s,
            Err(e) => {
                notices.push((
                    NoticeKind::Error,
                    trf!(
                        "打开数据库 {} 失败，本次使用临时数据库：{e}",
                        "Couldn’t open the database {}; using a temporary database for this session: {e}",
                        db.display(),
                    ),
                ));
                Store::open_in_memory().expect("in-memory store always opens")
            }
        };
        let first_run = !dir.join("config.json").exists();
        let mut cfg = AppConfig::load(&dir).unwrap_or_else(|e| {
            notices.push((
                NoticeKind::Error,
                trf!(
                    "读取设置失败，已使用默认设置：{e}",
                    "Couldn’t read settings; using defaults: {e}",
                ),
            ));
            AppConfig::default()
        });
        apply_ui_language(cfg.effective_ui_language());
        if first_run {
            if cfg.adopt_obsidian_vault()
                && let Some(v) = &cfg.vault_path
            {
                notices.push((
                    NoticeKind::Info,
                    trf!(
                        "已自动使用 Obsidian 库：{}",
                        "Using Obsidian vault: {}",
                        v.display()
                    ),
                ));
            }
            if let Err(e) = cfg.save(&dir) {
                notices.push((NoticeKind::Error, e.to_string()));
            }
        }
        if data_moved
            && cfg.rebase_paths(&legacy_data, &dir)
            && let Err(e) = cfg.save(&dir)
        {
            notices.push((NoticeKind::Error, e.to_string()));
        }
        if !data_dir_overridden() {
            let new_rec = default_recordings_dir();
            let old_rec = legacy::legacy_sibling(&new_rec);
            match legacy::migrate_recordings(&store, &cfg, &old_rec, &new_rec) {
                Ok(FolderMove::Moved) => notices.push((
                    NoticeKind::Info,
                    trf!("录音已移到 {}。", "Recordings moved to {}.", new_rec.display()),
                )),
                Ok(FolderMove::BothExist) => notices.push((
                    NoticeKind::Warning,
                    trf!(
                        "旧录音文件夹 {} 还在，没有合并到 {}，旧会议仍从原位置读取。",
                        "The old recordings folder {} is still there and wasn’t merged into {}. Older meetings still read from it.",
                        old_rec.display(),
                        new_rec.display(),
                    ),
                )),
                Ok(FolderMove::NothingToMove) => {}
                Err(e) => notices.push((
                    NoticeKind::Error,
                    trf!(
                        "迁移录音文件夹失败：{e}",
                        "Couldn’t move the recordings folder: {e}"
                    ),
                )),
            }
        }
        if data_moved {
            notices.push((
                NoticeKind::Info,
                trf!(
                    "Meeting Master 已更名为 Friston，数据已移到 {}。",
                    "Meeting Master is now Friston. Your data moved to {}.",
                    dir.display(),
                ),
            ));
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

        let cfg_has_llm = cfg.local_llm_dir.is_some();
        let mut state = Self {
            data_dir: dir,
            store,
            cfg,
            indexer,
            writer,
            meeting_engine: None,
            meeting_state: EngineState::NotStarted,
            meeting_restarts: 0,
            meeting_models: Default::default(),
            speech_models: Loadable::Idle,
            embed_engine: None,
            embed_state: EngineState::NotStarted,
            embed_restarts: 0,
            autoload_llm: cfg_has_llm,
            live: None,
            live_starting: false,
            post: PostQueue::default(),
            selected_window: None,
            meetings: Loadable::Loading,
            actions: vec![],
            people: Loadable::Loading,
            events: Loadable::Loading,
            inputs: Loadable::Idle,
            sources: Loadable::Idle,
            permissions: Loadable::Idle,
            codex: Loadable::Loading,
            codex_models: Loadable::Idle,
            agy_models: Loadable::Idle,
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
        // Load every configured model at launch, so the first search, meeting or local
        // minutes run does not wait. The minutes model follows the embedding load
        // (`autoload_llm`) because both live in the same `mm-embedd` process.
        if let Some(dir) = state.cfg.embedding_model_dir.clone() {
            state.load_embedding(dir, cx);
        } else if state.cfg.auto_download_embedding {
            state.load_default_embedding(cx);
        } else {
            state.autoload_local_llm(cx);
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
        self.meeting_engine.clone().ok_or_else(|| {
            self.meeting_state
                .describe(tr!("字幕引擎 mm-meetingd ", "Caption engine mm-meetingd"))
        })
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
                let caps = engine
                    .initialize()
                    .map_err(|e| EngineState::Failed(trf!("initialize：{e}", "initialize: {e}")))?;
                Ok((engine, rx, caps))
            },
            |this, res, cx| {
                match res {
                    Ok((engine, rx, caps)) => {
                        this.meeting_models =
                            serde_json::from_value(caps["capabilities"]["models"].clone())
                                .unwrap_or_default();
                        this.meeting_engine = Some(engine);
                        this.meeting_state = EngineState::Ready;
                        this.preload_speech_models(cx);
                        this.drain_meeting_events(rx, cx);
                        this.refresh_calendar(cx);
                        this.refresh_inputs(cx);
                        this.refresh_permissions(cx);
                    }
                    Err(state) => {
                        let msg = state
                            .describe(tr!("字幕引擎 mm-meetingd ", "Caption engine mm-meetingd"));
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
                // The models died with the process; a restart preloads them again.
                self.speech_models = Loadable::Idle;
                if self.live.is_some() {
                    self.notify(
                        NoticeKind::Error,
                        tr!(
                            "字幕引擎在会议中退出。已保存的字幕和录音不会丢失，请点击“结束记录”完成整理。",
                            "The caption engine quit during the meeting. Saved captions and audio are safe; click “Stop recording” to finish up.",
                        ),
                        cx,
                    );
                }
                if self.meeting_restarts == 0 {
                    self.meeting_restarts += 1;
                    self.meeting_state = EngineState::Failed(tr!("已退出", "quit").into());
                    self.notify(
                        NoticeKind::Warning,
                        tr!(
                            "字幕引擎意外退出，正在重新启动。",
                            "The caption engine quit unexpectedly. Restarting it.",
                        ),
                        cx,
                    );
                    self.ensure_meeting_engine(cx);
                } else {
                    self.meeting_state = EngineState::Failed(
                        tr!(
                            "再次退出，请在设置中手动重启",
                            "quit again; restart it in Settings"
                        )
                        .into(),
                    );
                    self.notify(
                        NoticeKind::Error,
                        tr!(
                            "字幕引擎再次退出，请在设置中手动重启。",
                            "The caption engine quit again. Restart it in Settings.",
                        ),
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
                        let msg = trf!(
                            "录制失败：{}",
                            "Recording failed: {}",
                            s.error
                                .unwrap_or_else(|| tr!("未知原因", "unknown reason").into()),
                        );
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
        self.speech_models = Loadable::Idle;
        self.ensure_meeting_engine(cx);
    }

    /// Loads the live caption models in `mm-meetingd` now rather than on the first recording.
    ///
    /// A failure is a warning, not fatal: `capture/start` loads the models again (the engine
    /// retries a failed load), and recording works without captions.
    ///
    /// Params: `cx`.
    fn preload_speech_models(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.meeting_engine.clone() else {
            return;
        };
        self.speech_models = Loadable::Loading;
        cx.notify();
        let language = self.cfg.caption_language.clone();
        bg(
            cx,
            move || engine.preload_models(language.as_deref()),
            |this, res, cx| {
                // The engine exited meanwhile (state reset to Idle): its models are gone, so
                // this result must not mark them ready.
                if !matches!(this.speech_models, Loadable::Loading) {
                    return;
                }
                match res {
                    Ok(status) => this.speech_models = Loadable::Ready(status),
                    Err(e) => {
                        let msg = trf!(
                            "字幕模型加载失败：{e}",
                            "Couldn’t load the caption model: {e}",
                        );
                        this.speech_models = Loadable::Failed(LoadError::from(e));
                        this.notify(NoticeKind::Warning, msg, cx);
                    }
                }
                cx.notify();
            },
        );
    }

    /// Starts the one-time launch load of the configured local minutes model, if pending.
    ///
    /// Params: `cx`.
    fn autoload_local_llm(&mut self, cx: &mut Context<Self>) {
        if !std::mem::take(&mut self.autoload_llm) {
            return;
        }
        if let Some(dir) = self.cfg.local_llm_dir.clone() {
            self.load_local_llm(dir, cx);
        }
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
                                tr!(
                                    "找不到嵌入引擎 mm-embedd，请先运行 make engines",
                                    "Embedding engine mm-embedd not found. Run make engines first.",
                                )
                                .to_string(),
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
                        let msg = trf!(
                            "加载嵌入模型失败：{}",
                            "Couldn’t load the embedding model: {}",
                            e.message,
                        );
                        this.embed_status = Loadable::Failed(e);
                        this.notify(NoticeKind::Error, msg, cx);
                    }
                }
                this.autoload_local_llm(cx);
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
                    Err(e) => this.notify(
                        NoticeKind::Error,
                        trf!(
                            "卸载嵌入模型失败：{e}",
                            "Couldn’t unload the embedding model: {e}"
                        ),
                        cx,
                    ),
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
                            LoadError::from(
                                tr!(
                                    "找不到 mm-embedd，请先运行 make engines",
                                    "mm-embedd not found. Run make engines first.",
                                )
                                .to_string(),
                            )
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
                        let msg = trf!(
                            "加载本地纪要模型失败：{}",
                            "Couldn’t load the local minutes model: {}",
                            e.message,
                        );
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
                    Err(e) => this.notify(
                        NoticeKind::Error,
                        trf!(
                            "卸载本地纪要模型失败：{e}",
                            "Couldn’t unload the local minutes model: {e}",
                        ),
                        cx,
                    ),
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
                        s.notify(
                            NoticeKind::Warning,
                            tr!(
                                "嵌入引擎意外退出，正在重新启动。",
                                "The embedding engine quit unexpectedly. Restarting it.",
                            ),
                            cx,
                        );
                        s.load_embedding(dir, cx);
                    } else {
                        s.embed_state = EngineState::Failed(tr!("已退出", "quit").into());
                        s.notify(
                            NoticeKind::Error,
                            tr!(
                                "嵌入引擎已退出，搜索暂时只用关键词。",
                                "The embedding engine quit. Search uses keywords only for now.",
                            ),
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
                let meetings = s.recent_meetings(100).map_err(|e| {
                    trf!(
                        "读取会议列表失败：{e}",
                        "Couldn’t read the meeting list: {e}"
                    )
                })?;
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
                let people = s
                    .people()
                    .map_err(|e| trf!("读取人物失败：{e}", "Couldn’t read people: {e}"))?;
                let prints = s
                    .voiceprints(None)
                    .map_err(|e| trf!("读取声纹失败：{e}", "Couldn’t read voiceprints: {e}"))?;
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

    /// Lists the models Codex offers, finding Codex again when it was not probed yet.
    ///
    /// Params: `cx`.
    pub fn refresh_codex_models(&mut self, cx: &mut Context<Self>) {
        let known = self.codex.ready().and_then(|c| c.path.clone());
        self.codex_models = Loadable::Loading;
        cx.notify();
        bg(
            cx,
            move || -> Result<Vec<ModelOption>, String> {
                let path = known
                    .or_else(mm_llm::codex::find_codex)
                    .ok_or_else(|| tr!("没有找到 Codex CLI", "Codex CLI not found").to_string())?;
                mm_llm::codex::list_models(&path).map_err(|e| e.to_string())
            },
            |this, res, cx| {
                this.codex_models = match res {
                    Ok(m) => Loadable::Ready(m),
                    Err(e) => Loadable::Failed(e.into()),
                };
                cx.notify();
            },
        );
    }

    /// Lists the models the Antigravity CLI offers.
    ///
    /// Params: `cx`.
    pub fn refresh_agy_models(&mut self, cx: &mut Context<Self>) {
        self.agy_models = Loadable::Loading;
        cx.notify();
        bg(
            cx,
            || -> Result<Vec<ModelOption>, String> {
                let agy = mm_llm::antigravity::find_agy()
                    .ok_or_else(|| tr!("没有找到 agy", "agy not found").to_string())?;
                mm_llm::antigravity::list_models(&agy).map_err(|e| e.to_string())
            },
            |this, res, cx| {
                this.agy_models = match res {
                    Ok(m) => Loadable::Ready(m),
                    Err(e) => Loadable::Failed(e.into()),
                };
                cx.notify();
            },
        );
    }

    /// Loads both model lists once; later loads only happen through the refresh buttons.
    ///
    /// Params: `cx`.
    pub fn ensure_model_lists(&mut self, cx: &mut Context<Self>) {
        if matches!(self.codex_models, Loadable::Idle) {
            self.refresh_codex_models(cx);
        }
        if matches!(self.agy_models, Loadable::Idle) {
            self.refresh_agy_models(cx);
        }
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
                    Err(e) => this.notify(
                        NoticeKind::Error,
                        trf!("读取钥匙串失败：{e}", "Couldn’t read the Keychain: {e}"),
                        cx,
                    ),
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
                    Err(e) => this.notify(
                        NoticeKind::Error,
                        trf!("读取钥匙串失败：{e}", "Couldn’t read the Keychain: {e}"),
                        cx,
                    ),
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
                            tr!(
                                "Hugging Face 令牌已保存到钥匙串。",
                                "Hugging Face token saved to Keychain.",
                            )
                        } else {
                            tr!("Hugging Face 令牌已删除。", "Hugging Face token removed.")
                        };
                        this.notify(NoticeKind::Success, msg, cx);
                    }
                    Err(e) => this.notify(
                        NoticeKind::Error,
                        trf!("写入钥匙串失败：{e}", "Couldn’t write to the Keychain: {e}"),
                        cx,
                    ),
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
            self.notify(
                NoticeKind::Warning,
                tr!("已有模型正在下载。", "A model is already downloading."),
                cx,
            );
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

    /// Loads the configured default embedding model when no folder is set: a copy already
    /// on disk (app models folder or Hugging Face cache) loads at once, otherwise it is
    /// downloaded and then loaded.
    ///
    /// Params: `cx`.
    fn load_default_embedding(&mut self, cx: &mut Context<Self>) {
        let url = self.cfg.embedding_model_url.clone();
        let repo = match HubRepo::parse(&url) {
            Ok(r) => r,
            Err(e) => {
                self.notify(
                    NoticeKind::Warning,
                    trf!("默认嵌入模型：{e}", "Default embedding model: {e}"),
                    cx,
                );
                self.autoload_local_llm(cx);
                return;
            }
        };
        let root = models_dir(&self.data_dir);
        if let Some(dir) = hub::find_local_snapshot(&repo, &root, hub::hf_cache_dir().as_deref()) {
            tracing::info!(
                "default embedding model {} found at {}",
                repo.id,
                dir.display()
            );
            self.load_embedding(dir, cx);
            return;
        }
        // A 1.5 GB download should not hold back the minutes model.
        self.autoload_local_llm(cx);
        self.notify(
            NoticeKind::Info,
            trf!(
                "正在从 Hugging Face 下载嵌入模型 {}（约 1.5 GB），完成后自动加载。可以在设置里取消。",
                "Downloading embedding model {} from Hugging Face (about 1.5 GB). It loads automatically when done; you can cancel in Settings.",
                repo.id,
            ),
            cx,
        );
        self.download_embedding(url, cx);
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
                        self.notify(
                            NoticeKind::Success,
                            tr!(
                                "嵌入模型已下载，正在加载。",
                                "Embedding model downloaded. Loading it now.",
                            ),
                            cx,
                        );
                        self.load_embedding(dir, cx);
                    }
                    Err(hub::HubError::Cancelled) => self.notify(
                        NoticeKind::Info,
                        tr!(
                            "已取消下载，下次会从断点继续。",
                            "Download canceled. It resumes where it left off next time.",
                        ),
                        cx,
                    ),
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
        if self.cfg.ui_language != before.ui_language {
            apply_ui_language(self.cfg.effective_ui_language());
            // Every window (including the floating captions) re-renders in the new language.
            cx.refresh_windows();
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
            self.notify(
                NoticeKind::Warning,
                tr!(
                    "请先选择提供方并填写模型。",
                    "Choose a provider and enter a model first."
                ),
                cx,
            );
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
                            tr!("API 密钥已保存到钥匙串。", "API key saved to Keychain.")
                        } else {
                            tr!("API 密钥已删除。", "API key removed.")
                        };
                        this.notify(NoticeKind::Success, msg, cx);
                    }
                    Err(e) => this.notify(
                        NoticeKind::Error,
                        trf!("写入钥匙串失败：{e}", "Couldn’t write to the Keychain: {e}"),
                        cx,
                    ),
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
                    flow::quick_meeting(&s, &title)
                        .map_err(|e| trf!("创建会议失败：{e}", "Couldn’t create the meeting: {e}"))
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
                    flow::meeting_for_event(&s, &ev).map_err(|e| {
                        trf!(
                            "创建会议失败（{}）：{e}",
                            "Couldn’t create the meeting ({}): {e}",
                            ev.title,
                        )
                    })
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
            self.notify(
                NoticeKind::Info,
                tr!("已经在记录中。", "Already recording."),
                cx,
            );
            self.navigate(Page::Live, cx);
            return true;
        }
        // Only the short stop phase holds the recorder; refine and minutes of earlier
        // meetings keep running in the post-meeting queue while the next one records.
        if self.post.stopping() {
            self.notify(
                NoticeKind::Info,
                tr!(
                    "上一场录制正在停止，几秒后再开始新的记录。",
                    "The previous recording is still stopping. Start a new one in a few seconds.",
                ),
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
                self.notify(
                    NoticeKind::Error,
                    trf!("无法开始记录：{msg}", "Couldn’t start recording: {msg}"),
                    cx,
                );
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
                        tr!(
                            "开始录制还没有完成。如果系统弹出了麦克风或屏幕录制权限请求，请先允许；也可以在“系统设置 › 隐私与安全性”里检查。",
                            "Recording hasn’t started yet. If macOS asks for microphone or screen recording permission, allow it; you can also check System Settings › Privacy & Security.",
                        ),
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
                    let note =
                        flow::ensure_note(&cfg, &s, &mut meeting, &attendees).map_err(|e| {
                            trf!(
                                "创建会议笔记失败：{e}",
                                "Couldn’t create the meeting note: {e}"
                            )
                        })?;
                    let mut preset =
                        flow::build_preset(&cfg, &s, &meeting, window_id).map_err(|e| {
                            trf!(
                                "准备录制参数失败：{e}",
                                "Couldn’t prepare recording settings: {e}",
                            )
                        })?;
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
                                engine.capture_start(&reduced).map_err(|e| {
                                    trf!("开始录制失败：{e}", "Couldn’t start recording: {e}")
                                })?,
                                Some(trf!(
                                    "没有屏幕录制权限，只录麦克风（不录窗口和系统声音）。在系统设置 › 隐私与安全性 › 屏幕与系统录音中允许后即可恢复。（{why}）",
                                    "No screen recording permission, so only the microphone is recorded (no window or system audio). Allow it in System Settings › Privacy & Security › Screen & System Audio Recording to restore it. ({why})",
                                )),
                            ),
                            None => {
                                return Err(trf!(
                                    "开始录制失败：{why}",
                                    "Couldn’t start recording: {why}",
                                ));
                            }
                        }
                    }
                    Err(e) => {
                        return Err(trf!("开始录制失败：{e}", "Couldn’t start recording: {e}"));
                    }
                };
                flow::mark_live(&*lock(&store)?, &mut meeting, &started).map_err(|e| {
                    trf!(
                        "更新会议状态失败：{e}",
                        "Couldn’t update the meeting status: {e}"
                    )
                })?;
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
                                tr!(
                                    "还没有设置 Obsidian 库，笔记不会写入文件。可以在设置中选择。",
                                    "No Obsidian vault is set, so notes won’t be written to files. You can choose one in Settings.",
                                ),
                                cx,
                            );
                        }
                        this.notify(
                            NoticeKind::Success,
                            trf!("开始记录：{}", "Recording started: {}", meeting.title),
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
                        this.post.clear_finished();
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
                tr!(
                    "请先在设置中选择 Obsidian 库，截图会保存到库的附件文件夹。",
                    "Choose an Obsidian vault in Settings first. Screenshots are saved to its attachments folder.",
                ),
                cx,
            );
            return;
        };
        let window_id = self.live.as_ref().and_then(|l| l.window_id);
        let dir = logic::attachments_dir(&vault, &self.cfg.meetings_folder);
        bg(
            cx,
            move || -> Result<String, String> {
                std::fs::create_dir_all(&dir).map_err(|e| {
                    trf!(
                        "创建 {} 失败：{e}",
                        "Couldn’t create {}: {e}",
                        dir.display()
                    )
                })?;
                let name = logic::screenshot_name(Local::now());
                let out = dir.join(&name);
                let status = std::process::Command::new("/usr/sbin/screencapture")
                    .args(logic::screencapture_args(window_id, &out))
                    .status()
                    .map_err(|e| {
                        trf!(
                            "运行 screencapture 失败：{e}",
                            "Couldn’t run screencapture: {e}"
                        )
                    })?;
                if !status.success() || !out.exists() {
                    return Err(trf!(
                        "截图失败（screencapture 退出码 {status}）。请检查屏幕录制权限。",
                        "Screenshot failed (screencapture exit code {status}). Check screen recording permission.",
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

    /// Stops the recording (short, holds the recorder), then queues refine → minutes.
    ///
    /// Params: `cx`.
    pub fn stop_recording(&mut self, cx: &mut Context<Self>) {
        let Some(live) = self.live.take() else { return };
        self.notes_task = None;
        let notes = self.notes_pending.take().zip(live.note_path.clone());
        self.indexer.send(IndexerMsg::SetLive(false));
        self.close_floating(cx);
        let id = live.meeting.id;
        self.post.push_stopping(id, live.meeting.title.clone());
        cx.notify();

        let job = StopJob {
            store: self.store.clone(),
            cfg: self.cfg.clone(),
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
                // Stop-phase progress is collected and shown once it ends (a few seconds).
                let steps = Mutex::new(Vec::new());
                let mut warnings = Vec::new();
                if let Some((text, path)) = notes
                    && let Err(e) = write_notes(&path, &text, &indexer)
                {
                    warnings.push(e);
                }
                let push = |s: String| {
                    if let Ok(mut v) = steps.lock() {
                        v.push(s);
                    }
                };
                let res = pipeline::stop_meeting(job, &push);
                let steps = steps.into_inner().unwrap_or_default();
                res.map(|mut stopped| {
                    warnings.append(&mut stopped.warnings);
                    (stopped, steps, warnings)
                })
            },
            move |this, res, cx| {
                match res {
                    Ok((stopped, steps, warnings)) => {
                        this.post.stopped(id, stopped, steps, warnings);
                        if let Some(n) = this.post.position(id)
                            && this.post.counts().0 > 0
                        {
                            this.notify(
                                NoticeKind::Info,
                                trf!(
                                    "录制已停止，精转写排在第 {n} 位",
                                    "Recording stopped. Refine is number {n} in the queue",
                                ),
                                cx,
                            );
                        }
                    }
                    Err(e) => {
                        this.post.fail(id, e.clone());
                        this.notify(
                            NoticeKind::Error,
                            trf!(
                                "会后整理失败，录音已保留：{e}",
                                "Post-meeting processing failed; the recording was kept: {e}",
                            ),
                            cx,
                        );
                    }
                }
                this.run_next_post(cx);
                this.refresh_meetings(cx);
                cx.notify();
            },
        );
    }

    /// Starts the next queued refine → minutes job when none is running.
    ///
    /// Settings and engines are read now, not when the meeting stopped, so a job that waited
    /// uses the current configuration and a restarted engine.
    ///
    /// Params: `cx`.
    fn run_next_post(&mut self, cx: &mut Context<Self>) {
        let Some((id, stopped)) = self.post.start_next() else {
            return;
        };
        cx.notify();
        let (ptx, prx) = async_channel::unbounded::<String>();
        cx.spawn(async move |this, cx| {
            while let Ok(step) = prx.recv().await {
                let _ = this.update(cx, |s, cx| {
                    s.post.step(id, step);
                    cx.notify();
                });
            }
        })
        .detach();
        let minutes = self.minutes_ctx();
        let engine = self.meeting_engine.clone();
        bg(
            cx,
            move || {
                pipeline::process_meeting(&minutes, engine.as_ref(), stopped, &|s| {
                    let _ = ptx.send_blocking(s);
                })
            },
            move |this, res, cx| {
                match res {
                    Ok(report) => {
                        this.post.finish(id, report.warnings.clone());
                        let msg = if report.minutes_done {
                            trf!("{}：纪要已生成", "{}: minutes ready", report.meeting.title)
                        } else {
                            trf!(
                                "{}：录制已结束，转写已保存",
                                "{}: recording stopped, transcript saved",
                                report.meeting.title,
                            )
                        };
                        this.notify(NoticeKind::Success, msg, cx);
                        for w in report.warnings {
                            this.notify(NoticeKind::Warning, w, cx);
                        }
                        cx.emit(AppEvent::MeetingChanged(report.meeting.id));
                    }
                    Err(e) => {
                        this.post.fail(id, e.clone());
                        this.notify(
                            NoticeKind::Error,
                            trf!(
                                "会后整理失败，录音已保留：{e}",
                                "Post-meeting processing failed; the recording was kept: {e}",
                            ),
                            cx,
                        );
                    }
                }
                this.run_next_post(cx);
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
        self.notify(
            NoticeKind::Info,
            tr!(
                "正在生成纪要，可能需要几分钟…",
                "Writing minutes. This may take a few minutes…"
            ),
            cx,
        );
        bg(
            cx,
            move || -> Result<(Meeting, pipeline::MinutesWritten), String> {
                let mut m = lock(&ctx.store)?
                    .meeting(id)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| trf!("找不到会议 {id}", "Meeting {id} not found"))?;
                let written = pipeline::generate_minutes(&ctx, &mut m)?;
                Ok((m, written))
            },
            move |this, res, cx| {
                match res {
                    Ok((m, pipeline::MinutesWritten { label, warnings })) => {
                        for w in warnings {
                            this.notify(NoticeKind::Warning, w, cx);
                        }
                        this.notify(
                            NoticeKind::Success,
                            trf!(
                                "{}：纪要已由{label}生成",
                                "{}: minutes written by {label}",
                                m.title,
                            ),
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
                        s.upsert_person(&p, Some(person_hue(p.id))).map_err(|e| {
                            trf!(
                                "创建人物 {name} 失败：{e}",
                                "Couldn’t create person {name}: {e}"
                            )
                        })?;
                        (p.id, name)
                    }
                };
                s.name_cluster(meeting, cluster, Some(pid))
                    .map_err(|e| trf!("命名说话人失败：{e}", "Couldn’t name the speaker: {e}"))?;
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
                            trf!(
                                "说话人 {cluster} 已命名为 {name}，之前的发言也一起改名。",
                                "Speaker {cluster} is now {name}. Earlier lines were renamed too.",
                            ),
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
            self.notify(
                NoticeKind::Warning,
                tr!("请输入名字。", "Enter a name."),
                cx,
            );
            return;
        }
        let store = self.store.clone();
        bg(
            cx,
            move || {
                let p = new_person(&name);
                lock(&store)?
                    .upsert_person(&p, Some(person_hue(p.id)))
                    .map_err(|e| trf!("添加 {name} 失败：{e}", "Couldn’t add {name}: {e}"))
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
                    .map_err(|e| trf!("删除人物失败：{e}", "Couldn’t delete the person: {e}"))
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
                    .map_err(|e| trf!("删除声纹失败：{e}", "Couldn’t delete the voiceprint: {e}"))
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
                tr!(
                    "会议进行中不能录入声纹，请结束记录后再试。",
                    "Can’t enroll a voiceprint during a meeting. Stop recording and try again.",
                ),
                cx,
            );
            return;
        }
        let engine = match self.meeting_engine() {
            Ok(e) => e,
            Err(msg) => {
                self.notify(
                    NoticeKind::Error,
                    trf!("无法录入声纹：{msg}", "Couldn’t enroll voiceprint: {msg}"),
                    cx,
                );
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
                    .map_err(|e| trf!("录入声纹失败：{e}", "Couldn’t enroll voiceprint: {e}"))?;
                store_sample(&store, person, sample, VoiceprintSource::Enrollment)
            },
            |this, res, cx| {
                this.enrolling = None;
                match res {
                    Ok(()) => this.notify(
                        NoticeKind::Success,
                        tr!("声纹已录入。", "Voiceprint enrolled."),
                        cx,
                    ),
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
                self.notify(
                    NoticeKind::Error,
                    trf!("无法提取声纹：{msg}", "Couldn’t extract voiceprint: {msg}"),
                    cx,
                );
                return;
            }
        };
        let store = self.store.clone();
        bg(
            cx,
            move || -> Result<(), String> {
                if !path.is_file() {
                    return Err(trf!(
                        "找不到录音文件 {}",
                        "Recording file {} not found",
                        path.display(),
                    ));
                }
                let req = ClipRequest {
                    path: path.clone(),
                    start_ms,
                    end_ms,
                };
                let sample = engine.voiceprint_from_clip(&req).map_err(|e| {
                    trf!(
                        "从 {} 提取声纹失败：{e}",
                        "Couldn’t extract a voiceprint from {}: {e}",
                        path.display(),
                    )
                })?;
                let source = VoiceprintSource::Clip {
                    asset_path: path.display().to_string(),
                    start_ms: start_ms.unwrap_or(0),
                    end_ms: end_ms.unwrap_or(0),
                };
                store_sample(&store, person, sample, source)
            },
            |this, res, cx| {
                match res {
                    Ok(()) => this.notify(
                        NoticeKind::Success,
                        tr!(
                            "已从录音片段提取声纹。",
                            "Voiceprint extracted from the recording clip.",
                        ),
                        cx,
                    ),
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
                        s.notify(
                            NoticeKind::Error,
                            trf!(
                                "打开浮窗字幕失败：{e}",
                                "Couldn’t open floating captions: {e}"
                            ),
                            cx,
                        )
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

    /// Handles a `friston://open?meeting=…&t=…` URL.
    ///
    /// Params: `url`; `cx`.
    pub fn open_url(&mut self, url: &str, cx: &mut Context<Self>) {
        match flow::parse_seek_url(url) {
            Some((id, t)) => cx.emit(AppEvent::OpenMeeting(id, Some(t))),
            None => self.notify(
                NoticeKind::Warning,
                trf!("无法识别的链接：{url}", "Unrecognized link: {url}"),
                cx,
            ),
        }
    }

    /// Opens a meeting note in Obsidian.
    ///
    /// Params: `meeting`; `cx`.
    pub fn open_in_obsidian(&mut self, meeting: &Meeting, cx: &mut Context<Self>) {
        let (Some(vault), Some(note)) = (&self.cfg.vault_path, &meeting.note_path) else {
            self.notify(
                NoticeKind::Warning,
                tr!(
                    "这场会议还没有 Obsidian 笔记。",
                    "This meeting has no Obsidian note yet."
                ),
                cx,
            );
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
                trf!(
                    "录音文件夹不存在：{}",
                    "Recordings folder doesn’t exist: {}",
                    dir.display()
                ),
                cx,
            ),
            None => self.notify(
                NoticeKind::Warning,
                tr!("这场会议没有录音。", "This meeting has no recording."),
                cx,
            ),
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
        .map_err(|e| trf!("保存声纹失败：{e}", "Couldn’t save the voiceprint: {e}"))
}

/// Writes the notes text into the note's 笔记 section and asks the indexer to refresh it.
///
/// Params: `path` – note; `text` – notes; `indexer`.
/// Returns: an error message with the path on failure.
pub fn write_notes(path: &Path, text: &str, indexer: &Indexer) -> Result<(), String> {
    let md = std::fs::read_to_string(path).map_err(|e| {
        trf!(
            "读取笔记 {} 失败：{e}",
            "Couldn’t read note {}: {e}",
            path.display()
        )
    })?;
    let updated = logic::replace_notes_section(&md, text).ok_or_else(|| {
        trf!(
            "笔记 {} 里没有“## 笔记”标题，未写入。",
            "Note {} has no “## Notes” heading, so nothing was written.",
            path.display(),
        )
    })?;
    if updated != md {
        mm_vault::write_atomic(path, &updated).map_err(|e| {
            trf!(
                "写入笔记 {} 失败：{e}",
                "Couldn’t write note {}: {e}",
                path.display()
            )
        })?;
        indexer.send(IndexerMsg::Touch(
            path.to_path_buf(),
            mm_index::Priority::Interactive,
        ));
    }
    Ok(())
}

/// Makes `lang` the language of our own strings and of gpui-component's built-in ones
/// (dialog buttons, pickers), which use their own `rust-i18n` locale.
///
/// Params: `lang` – interface language.
pub fn apply_ui_language(lang: mm_i18n::Lang) {
    mm_i18n::set_current(lang);
    gpui_kit::component::set_locale(lang.component_locale());
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
    fn engine_state_descriptions_in_english() {
        mm_i18n::with_lang(mm_i18n::Lang::En, || {
            // The trailing separator space Chinese callers use must not double up.
            assert_eq!(
                EngineState::Ready.describe("Caption engine "),
                "Caption engine is ready"
            );
            assert_eq!(
                EngineState::Failed("quit".into()).describe("Caption engine"),
                "Caption engine unavailable: quit"
            );
        });
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
