//! 纪要库: hybrid search, meeting list and a meeting's minutes and transcript.

use std::time::Duration;

use gpui_kit::base::TestTextExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{ActiveTheme as _, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use mm_core::{AudioOrigin, Meeting, MeetingId, SpeakerLabel, TranscriptSegment};
use mm_i18n::{tr, trf};
use mm_llm::Minutes;
use mm_service::flow;
use mm_service::live::SpeakerDirectory;
use mm_store::{SearchHit, SourceKind};

use crate::assign;
use crate::logic;
use crate::state::{AppEvent, AppState, Loadable, bg};
use crate::ui;
use crate::writer::lock;

/// Search debounce.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(350);

/// A loaded meeting with everything the detail pane shows.
#[derive(Clone)]
pub struct Detail {
    pub meeting: Meeting,
    pub minutes: Option<Minutes>,
    pub transcript: Vec<TranscriptSegment>,
    pub directory: SpeakerDirectory,
}

/// The 纪要库 page.
pub struct LibraryPage {
    app: Entity<AppState>,
    /// Input placeholders, rewritten when the interface language changes.
    placeholders: ui::Placeholders,
    search: Entity<InputState>,
    new_person: Entity<InputState>,
    query: String,
    hits: Loadable<Vec<SearchHit>>,
    search_task: Option<Task<()>>,
    selected: Option<MeetingId>,
    detail: Loadable<Detail>,
    seek_ms: Option<u64>,
}

impl LibraryPage {
    /// Creates the page.
    ///
    /// Params: `app`; `window`; `cx`.
    /// Returns: the page.
    pub fn new(app: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx));
        let new_person = cx.new(|cx| InputState::new(window, cx));
        cx.observe(&app, |_, _, cx| cx.notify()).detach();
        cx.subscribe(&search, |this, state, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change | InputEvent::PressEnter { .. }) {
                let q = state.read(cx).value().trim().to_string();
                this.schedule_search(q, matches!(ev, InputEvent::PressEnter { .. }), cx);
            }
        })
        .detach();
        cx.subscribe(&app, |this, _, ev: &AppEvent, cx| {
            if let AppEvent::MeetingChanged(id) = ev
                && this.selected == Some(*id)
            {
                this.load_detail(*id, cx);
            }
        })
        .detach();
        let mut placeholders = ui::Placeholders::default();
        placeholders.input(&search, || {
            tr!(
                "搜索纪要、转写、笔记和截图，例如“预算”",
                "Search minutes, transcripts, notes and screenshots, e.g. “budget”"
            )
        });
        placeholders.input(&new_person, || tr!("新人物的名字", "New person’s name"));
        placeholders.sync(window, cx);
        Self {
            app,
            placeholders,
            search,
            new_person,
            query: String::new(),
            hits: Loadable::Idle,
            search_task: None,
            selected: None,
            detail: Loadable::Idle,
            seek_ms: None,
        }
    }

    /// Debounces a search.
    ///
    /// Params: `q` – query; `now` – skip the debounce (Enter); `cx`.
    fn schedule_search(&mut self, q: String, now: bool, cx: &mut Context<Self>) {
        if q == self.query && !now {
            return;
        }
        self.query = q.clone();
        if q.is_empty() {
            self.hits = Loadable::Idle;
            self.search_task = None;
            cx.notify();
            return;
        }
        let delay = if now { Duration::ZERO } else { SEARCH_DEBOUNCE };
        self.search_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |p, cx| p.run_search(q, cx));
        }));
    }

    /// Runs the hybrid search in the background.
    ///
    /// Params: `q`; `cx`.
    fn run_search(&mut self, q: String, cx: &mut Context<Self>) {
        let app = self.app.read(cx);
        let store = app.store.clone();
        let embed = app.embedding_ready().then(|| app.embed_engine()).flatten();
        self.hits = Loadable::Loading;
        cx.notify();
        let query = q.clone();
        bg(
            cx,
            move || -> Result<Vec<SearchHit>, String> {
                let vector = match &embed {
                    Some(e) => match e.embed_query(&query) {
                        Ok(v) => Some(v),
                        Err(err) => {
                            tracing::warn!("query embedding failed, keyword only: {err}");
                            None
                        }
                    },
                    None => None,
                };
                lock(&store)?
                    .search(&query, vector.as_deref(), 30)
                    .map_err(|e| {
                        trf!(
                            "搜索“{query}”失败：{e}",
                            "Couldn’t search for “{query}”: {e}"
                        )
                    })
            },
            move |this, res, cx| {
                // A newer query may have started meanwhile; drop stale results.
                if this.query != q {
                    return;
                }
                this.hits = match res {
                    Ok(h) => Loadable::Ready(h),
                    Err(e) => Loadable::Failed(e.into()),
                };
                cx.notify();
            },
        );
    }

    /// Puts the cursor in the search field (⌘F), selecting what is there so typing replaces it.
    ///
    /// Params: `window`; `cx`.
    pub fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search.update(cx, |s, cx| s.focus(window, cx));
    }

    /// Opens a meeting, optionally at an offset (seek links).
    ///
    /// Params: `id`; `seek_ms`; `cx`.
    pub fn open(&mut self, id: MeetingId, seek_ms: Option<u64>, cx: &mut Context<Self>) {
        self.seek_ms = seek_ms;
        self.load_detail(id, cx);
    }

    /// Loads minutes, transcript and speakers of a meeting.
    ///
    /// Params: `id`; `cx`.
    fn load_detail(&mut self, id: MeetingId, cx: &mut Context<Self>) {
        self.selected = Some(id);
        if !matches!(self.detail, Loadable::Ready(ref d) if d.meeting.id == id) {
            self.detail = Loadable::Loading;
        }
        cx.notify();
        let store = self.app.read(cx).store.clone();
        bg(
            cx,
            move || -> Result<Detail, String> {
                let s = lock(&store)?;
                let meeting = s
                    .meeting(id)
                    .map_err(|e| {
                        trf!("读取会议 {id} 失败：{e}", "Couldn’t read meeting {id}: {e}")
                    })?
                    .ok_or_else(|| trf!("找不到会议 {id}", "Meeting {id} not found"))?;
                let minutes = s
                    .minutes(id)
                    .map_err(|e| e.to_string())?
                    .and_then(|v| logic::parse_minutes(&v));
                let transcript = s
                    .transcript(id)
                    .map_err(|e| trf!("读取转写失败：{e}", "Couldn’t read the transcript: {e}"))?;
                let directory = flow::directory_for(&s, id).map_err(|e| e.to_string())?;
                Ok(Detail {
                    meeting,
                    minutes,
                    transcript,
                    directory,
                })
            },
            move |this, res, cx| {
                if this.selected != Some(id) {
                    return;
                }
                this.detail = match res {
                    Ok(d) => Loadable::Ready(d),
                    Err(e) => Loadable::Failed(e.into()),
                };
                cx.notify();
            },
        );
    }

    /// Opens a search hit: its meeting, or the note in Obsidian.
    ///
    /// Params: `hit`; `cx`.
    fn open_hit(&mut self, hit: &SearchHit, cx: &mut Context<Self>) {
        let ms = logic::locator_ms(&hit.locator);
        if let Some(id) = hit
            .meeting_id
            .as_deref()
            .and_then(|s| s.parse::<uuid::Uuid>().ok())
        {
            self.open(MeetingId(id), ms, cx);
            return;
        }
        if hit.source_kind == SourceKind::Note
            && let Some(vault) = self.app.read(cx).cfg.vault_path.clone()
        {
            let name = vault
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            cx.open_url(&mm_vault::open_note_uri(&name, &hit.source_uri));
        } else if std::path::Path::new(&hit.source_uri).exists() {
            cx.open_with_system(std::path::Path::new(&hit.source_uri));
        }
    }

    /// Search results list.
    fn results(&self, cx: &mut Context<Self>) -> AnyElement {
        match &self.hits {
            Loadable::Idle => self.meeting_list(cx),
            Loadable::Loading => ui::loading(tr!("正在搜索…", "Searching…"), cx).into_any_element(),
            Loadable::Failed(e) => ui::load_error("search-err", e, cx).into_any_element(),
            Loadable::Ready(h) if h.is_empty() => ui::muted(
                trf!("没有找到“{}”。", "No results for “{}”.", self.query),
                cx,
            )
            .into_any_element(),
            Loadable::Ready(hits) => {
                let mode = if self.app.read(cx).embedding_ready() {
                    tr!("关键词 + 语义", "Keyword + semantic")
                } else {
                    tr!(
                        "关键词（加载嵌入模型后可语义搜索）",
                        "Keyword (load the embedding model for semantic search)"
                    )
                };
                let found = hits.len();
                let found_en = logic::count_en(found as u64, "result", "results");
                v_flex()
                    .gap_1()
                    .child(ui::muted(
                        trf!("找到 {found} 条 · {mode}", "{found_en} · {mode}"),
                        cx,
                    ))
                    .children(hits.iter().enumerate().map(|(i, h)| {
                        let hit = h.clone();
                        let title = h
                            .meeting_title
                            .clone()
                            .or_else(|| {
                                h.locator
                                    .get("title")
                                    .and_then(|t| t.as_str())
                                    .map(String::from)
                            })
                            .unwrap_or_else(|| h.source_uri.clone());
                        let time = logic::locator_ms(&h.locator).map(logic::clock);
                        v_flex()
                            .id(("hit", i))
                            .test_support()
                            .p_2()
                            .gap_1()
                            .rounded_md()
                            .cursor_pointer()
                            .hover(|s| s.bg(cx.theme().muted))
                            .on_click(cx.listener(move |this, _, _, cx| this.open_hit(&hit, cx)))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(
                                        Tag::secondary()
                                            .small()
                                            .child(logic::modality_label(&h.modality)),
                                    )
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .truncate()
                                            .child(
                                                SharedString::from(title.clone()).test_text(title),
                                            ),
                                    )
                                    .when_some(time, |el, t| el.child(ui::muted(t, cx)))
                                    .when(h.keyword_match, |el| {
                                        el.child(
                                            Tag::info().small().child(tr!("关键词", "Keyword")),
                                        )
                                    }),
                            )
                            .when_some(h.text.clone(), |el, t| {
                                el.child(ui::muted(logic::snippet(&t, 120), cx))
                            })
                    }))
                    .into_any_element()
            }
        }
    }

    /// Meetings list (shown when not searching).
    fn meeting_list(&self, cx: &mut Context<Self>) -> AnyElement {
        match &self.app.read(cx).meetings {
            Loadable::Ready(m) if m.is_empty() => ui::muted(
                tr!(
                    "还没有会议。开始一场记录后会出现在这里。",
                    "No meetings yet. They appear here once you start recording one."
                ),
                cx,
            )
            .into_any_element(),
            Loadable::Ready(meetings) => v_flex()
                .gap_1()
                .children(meetings.iter().enumerate().map(|(i, m)| {
                    let id = m.id;
                    let active = self.selected == Some(id);
                    h_flex()
                        .id(("meeting", i))
                        .test_support()
                        .p_2()
                        .gap_2()
                        .rounded_md()
                        .cursor_pointer()
                        .when(active, |el| el.bg(cx.theme().accent))
                        .hover(|s| s.bg(cx.theme().muted))
                        .on_click(cx.listener(move |this, _, _, cx| this.open(id, None, cx)))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(div().text_sm().truncate().child(
                                    SharedString::from(m.title.clone()).test_text(m.title.clone()),
                                ))
                                .child(ui::muted(logic::short_datetime(m.starts_at), cx)),
                        )
                        .child(ui::status_tag(m.status))
                }))
                .into_any_element(),
            Loadable::Failed(e) => ui::load_error("ml-err", e, cx).into_any_element(),
            _ => ui::loading(tr!("正在读取会议…", "Loading meetings…"), cx).into_any_element(),
        }
    }

    /// Minutes section.
    fn minutes_view(m: &Minutes, cx: &App) -> impl IntoElement {
        let list = |title: &str, items: &[String]| {
            v_flex()
                .gap_1()
                .child(ui::section_title(title.to_string()))
                .children(items.iter().map(|s| {
                    let line: SharedString = format!("· {s}").into();
                    div().text_sm().child(line.clone().test_text(line))
                }))
        };
        v_flex()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .child(SharedString::from(m.summary.clone()).test_text(m.summary.clone())),
            )
            .when(!m.decisions.is_empty(), |el| {
                el.child(list(tr!("决定", "Decisions"), &m.decisions))
            })
            .when(!m.action_items.is_empty(), |el| {
                el.child(
                    v_flex()
                        .gap_1()
                        .child(ui::section_title(tr!("待办", "Action items")))
                        .children(m.action_items.iter().map(|a| {
                            let meta = [
                                a.owner.clone(),
                                a.due.clone(),
                                a.evidence_ms.map(logic::clock),
                            ]
                            .into_iter()
                            .flatten()
                            .collect::<Vec<_>>()
                            .join(" · ");
                            let task_line: SharedString = format!("· {}", a.task).into();
                            h_flex()
                                .gap_2()
                                .child(
                                    div()
                                        .text_sm()
                                        .child(task_line.clone().test_text(task_line)),
                                )
                                .child(ui::muted(meta, cx))
                        })),
                )
            })
            .when(!m.open_questions.is_empty(), |el| {
                el.child(list(
                    tr!("待解决的问题", "Open questions"),
                    &m.open_questions,
                ))
            })
            .when(!m.topics.is_empty(), |el| {
                el.child(list(tr!("话题", "Topics"), &m.topics))
            })
    }

    /// Detail pane for the selected meeting.
    fn detail(&self, cx: &mut Context<Self>) -> AnyElement {
        let d = match &self.detail {
            Loadable::Idle => {
                return ui::muted(
                    tr!(
                        "选择左侧的会议或搜索结果查看纪要和转写。",
                        "Select a meeting or search result on the left to see its minutes and transcript."
                    ),
                    cx,
                )
                    .into_any_element();
            }
            Loadable::Loading => {
                return ui::loading(tr!("正在读取…", "Loading…"), cx).into_any_element();
            }
            Loadable::Failed(e) => return ui::load_error("detail-err", e, cx).into_any_element(),
            Loadable::Ready(d) => d.clone(),
        };
        let m = d.meeting.clone();
        let (a1, a2, a3) = (self.app.clone(), self.app.clone(), self.app.clone());
        let (m1, m2) = (m.clone(), m.clone());
        let id = m.id;
        let seek = self.seek_ms;
        let actions = h_flex()
            .gap_2()
            .flex_wrap()
            .child(
                Button::new("regen")
                    .small()
                    .primary()
                    .label(if d.minutes.is_some() {
                        tr!("重新生成纪要", "Regenerate minutes")
                    } else {
                        tr!("生成纪要", "Generate minutes")
                    })
                    .on_click(move |_, _, cx| a1.update(cx, |a, cx| a.regenerate_minutes(id, cx))),
            )
            .child(
                Button::new("obsidian")
                    .small()
                    .outline()
                    .icon(IconName::ExternalLink)
                    .label(tr!("在 Obsidian 中打开", "Open in Obsidian"))
                    .on_click(move |_, _, cx| a2.update(cx, |a, cx| a.open_in_obsidian(&m1, cx))),
            )
            .child(
                Button::new("rec-folder")
                    .small()
                    .outline()
                    .icon(IconName::FolderOpen)
                    .label(tr!("打开录音文件夹", "Open recording folder"))
                    .on_click(move |_, _, cx| a3.update(cx, |a, cx| a.open_recording(&m2, cx))),
            );
        let minutes = match &d.minutes {
            Some(min) => Self::minutes_view(min, cx).into_any_element(),
            None => ui::muted(tr!("还没有纪要。", "No minutes yet."), cx).into_any_element(),
        };
        let transcript = if d.transcript.is_empty() {
            ui::muted(tr!("没有转写。", "No transcript."), cx).into_any_element()
        } else {
            let mut rows = Vec::with_capacity(d.transcript.len());
            for (i, s) in d.transcript.iter().enumerate() {
                rows.push(self.segment_row(i, s, &d, seek, cx));
            }
            v_flex().gap_2().children(rows).into_any_element()
        };
        v_flex()
            .gap_4()
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .text_xl()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(SharedString::from(m.title.clone()).test_text(m.title.clone())),
                            )
                            .child(ui::status_tag(m.status)),
                    )
                    .child(ui::muted(logic::short_datetime(m.starts_at), cx)),
            )
            .child(actions)
            .when_some(seek, |el, t| {
                el.child(ui::muted(
                    trf!(
                        "从链接定位到 {}，下方高亮对应的发言。录音在“打开录音文件夹”里。",
                        "Opened from a link at {}; the matching line is highlighted below. The recording is in “Open recording folder”.",
                        logic::clock(t)
                    ),
                    cx,
                ))
            })
            .child(ui::section_title(tr!("纪要", "Minutes")))
            .child(minutes)
            .child(ui::section_title({
                let n = d.transcript.len();
                let n_en = logic::count_en(n as u64, "segment", "segments");
                trf!("转写（{n} 段）", "Transcript ({n_en})")
            }))
            .child(transcript)
            .into_any_element()
    }

    /// One transcript row; anonymous cluster names are clickable for naming.
    fn segment_row(
        &self,
        i: usize,
        s: &TranscriptSegment,
        d: &Detail,
        seek: Option<u64>,
        cx: &App,
    ) -> Div {
        let hue = d.directory.hue(&s.speaker);
        let mut name = d.directory.name(&s.speaker);
        if s.origin == AudioOrigin::System && d.directory.person(&s.speaker).is_none() {
            name = trf!("远程 · {name}", "Remote · {name}");
        }
        let cluster = match (&s.speaker, d.directory.person(&s.speaker)) {
            (SpeakerLabel::Cluster { index }, None) => Some(*index),
            _ => None,
        };
        let hit = seek.is_some_and(|t| t >= s.start_ms && t < s.end_ms.max(s.start_ms + 1000));
        let app = self.app.clone();
        let input = self.new_person.clone();
        let meeting = d.meeting.id;
        h_flex()
            .gap_3()
            .items_start()
            .p_1()
            .rounded_md()
            .when(hit, |el| el.bg(cx.theme().accent))
            .child(
                div()
                    .w(px(56.))
                    .flex_none()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(logic::clock(s.start_ms)),
            )
            .child(
                div()
                    .id(("seg-spk", i))
                    .test_support()
                    .w(px(120.))
                    .flex_none()
                    .when(cluster.is_some(), |el| el.cursor_pointer())
                    .child(ui::speaker_name(name, hue))
                    .when_some(cluster, |el, c| {
                        el.on_click(move |_, window, cx| {
                            assign::open(app.clone(), meeting, c, input.clone(), window, cx)
                        })
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .text_sm()
                    .child(SharedString::from(s.text.clone()).test_text(s.text.clone())),
            )
    }
}

impl Render for LibraryPage {
    /// Renders search + list | detail.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.placeholders.sync(window, cx);
        let stats = self
            .app
            .read(cx)
            .index_stats
            .map(|s| {
                let (sources, chunks, embedded) = (s.sources, s.chunks, s.embedded);
                let sources_en = logic::count_en(sources, "source", "sources");
                let chunks_en = logic::count_en(chunks, "chunk", "chunks");
                trf!(
                    "索引：{sources} 个来源，{chunks} 个片段，{embedded} 个已嵌入",
                    "Index: {sources_en}, {chunks_en}, {embedded} embedded"
                )
            })
            .unwrap_or_default();
        h_flex()
            .size_full()
            .child(
                v_flex()
                    .w(px(420.))
                    .flex_none()
                    .h_full()
                    .border_r_1()
                    .border_color(cx.theme().border)
                    .child(
                        v_flex()
                            .p_4()
                            .gap_2()
                            .child(ui::section_title(tr!("纪要库", "Library")))
                            .child(Input::new(&self.search).id("lib-search").cleanable(true))
                            .child(ui::muted(stats, cx)),
                    )
                    .child(
                        div()
                            .id("lib-list")
                            .flex_1()
                            .min_h_0()
                            .px_2()
                            .pb_4()
                            .overflow_y_scrollbar()
                            .child(self.results(cx)),
                    ),
            )
            .child(
                div()
                    .id("lib-detail")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .p_6()
                    .overflow_y_scrollbar()
                    .child(self.detail(cx)),
            )
    }
}
