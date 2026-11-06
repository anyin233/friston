//! 说话人: people, their hues and voiceprints (enrolment and clips).

use gpui_kit::component::Disableable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use mm_core::{PersonId, VoiceprintSource};

use crate::logic;
use crate::state::{AppState, ENROLL_SECONDS, Loadable, PersonRow};
use crate::ui;

/// The 说话人 page.
pub struct PeoplePage {
    app: Entity<AppState>,
    name: Entity<InputState>,
    clip_path: Entity<InputState>,
    clip_start: Entity<InputState>,
    clip_end: Entity<InputState>,
    selected: Option<PersonId>,
}

impl PeoplePage {
    /// Creates the page.
    ///
    /// Params: `app`; `window`; `cx`.
    /// Returns: the page.
    pub fn new(app: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("名字，例如 王伟"));
        let clip_path = cx.new(|cx| {
            InputState::new(window, cx).placeholder("录音文件路径，例如 ~/Movies/Friston/…/mic.caf")
        });
        let clip_start = cx.new(|cx| InputState::new(window, cx).placeholder("开始（秒或 分:秒）"));
        let clip_end = cx.new(|cx| InputState::new(window, cx).placeholder("结束（秒或 分:秒）"));
        cx.observe(&app, |_, _, cx| cx.notify()).detach();
        cx.subscribe_in(&name, window, |this, _, ev: &InputEvent, window, cx| {
            if matches!(ev, InputEvent::PressEnter { .. }) {
                this.add(window, cx);
            }
        })
        .detach();
        Self {
            app,
            name,
            clip_path,
            clip_start,
            clip_end,
            selected: None,
        }
    }

    /// Adds the person typed in the name field.
    fn add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.name.read(cx).value().to_string();
        self.app.update(cx, |a, cx| a.add_person(name, cx));
        self.name.update(cx, |s, cx| s.set_value("", window, cx));
    }

    /// Extracts a voiceprint from the clip form for `person`.
    fn extract_clip(&mut self, person: PersonId, cx: &mut Context<Self>) {
        let raw = self.clip_path.read(cx).value().trim().to_string();
        if raw.is_empty() {
            self.app.update(cx, |a, cx| {
                a.notify(
                    crate::state::NoticeKind::Warning,
                    "请填写录音文件路径。",
                    cx,
                )
            });
            return;
        }
        let path = match raw.strip_prefix("~/") {
            Some(rest) => std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_default()
                .join(rest),
            None => std::path::PathBuf::from(&raw),
        };
        let start = logic::parse_seconds(&self.clip_start.read(cx).value());
        let end = logic::parse_seconds(&self.clip_end.read(cx).value());
        self.app.update(cx, |a, cx| {
            a.voiceprint_from_clip(person, path, start, end, cx)
        });
    }

    /// Detail of the selected person.
    fn detail(&self, row: &PersonRow, cx: &mut Context<Self>) -> impl IntoElement {
        let app = self.app.read(cx);
        let enrolling = app.enrolling;
        let id = row.person.id;
        let countdown = enrolling
            .filter(|(p, _)| *p == id)
            .map(|(_, t)| ENROLL_SECONDS.saturating_sub(t.elapsed().as_secs() as u32));
        let busy = enrolling.is_some();
        let (a1, a2) = (self.app.clone(), self.app.clone());
        v_flex()
            .gap_4()
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(div().size(px(28.)).rounded_full().bg(ui::hue_color(Some(row.hue))))
                    .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).child(row.person.display_name.clone()))
                    .child(ui::muted(format!("色相 {}° · 在所有页面都用这个颜色", row.hue), cx)),
            )
            .child(
                ui::card(cx)
                    .child(ui::section_title("录入声纹"))
                    .child(ui::muted(format!("点“开始录入”后请本人对着麦克风自然说话 {ENROLL_SECONDS} 秒。多录几段（不同场合）识别更稳。"), cx))
                    .child(
                        h_flex()
                            .gap_3()
                            .items_center()
                            .child(
                                Button::new("enroll")
                                    .primary()
                                    .icon(IconName::Mic)
                                    .label(match countdown {
                                        Some(s) if s > 0 => format!("正在录音，还剩 {s} 秒"),
                                        Some(_) => "正在提取声纹…".into(),
                                        None => "开始录入".into(),
                                    })
                                    .loading(countdown.is_some())
                                    .disabled(busy)
                                    .on_click(move |_, _, cx| a1.update(cx, |a, cx| a.enroll_voiceprint(id, cx))),
                            )
                            .child(ui::muted(app.meeting_state.describe("字幕引擎 "), cx)),
                    ),
            )
            .child(
                ui::card(cx)
                    .child(ui::section_title("从录音截取"))
                    .child(ui::muted("已有这个人单独说话的录音时，填写文件和时间段即可提取声纹。时间留空表示整段。", cx))
                    .child(Input::new(&self.clip_path))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().child(Input::new(&self.clip_start)))
                            .child(div().flex_1().child(Input::new(&self.clip_end)))
                            .child(Button::new("clip").outline().label("提取声纹").on_click(cx.listener(move |this, _, _, cx| this.extract_clip(id, cx)))),
                    ),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(ui::section_title(format!("声纹样本（{}）", row.voiceprints.len())))
                    .when(row.voiceprints.is_empty(), |el| el.child(ui::muted("还没有声纹。字幕里会先显示“说话人 N”。", cx)))
                    .children(row.voiceprints.iter().enumerate().map(|(i, v)| {
                        let app = self.app.clone();
                        let vid = v.id;
                        let source = match &v.source {
                            VoiceprintSource::Enrollment => "现场录入".to_string(),
                            VoiceprintSource::Clip { asset_path, start_ms, end_ms } => {
                                format!("录音片段 {} {}–{}", asset_path, logic::clock(*start_ms), logic::clock(*end_ms))
                            }
                            VoiceprintSource::Confirmed { .. } => "会议中确认".to_string(),
                        };
                        h_flex()
                            .gap_3()
                            .items_center()
                            .p_2()
                            .border_1()
                            .border_color(cx.theme().border)
                            .rounded_md()
                            .child(v_flex().flex_1().min_w_0().child(div().text_sm().truncate().child(source)).child(ui::muted(
                                format!("有效语音 {:.1} 秒 · 模型 {}", v.speech_ms as f32 / 1000.0, v.model),
                                cx,
                            )))
                            .child(
                                Button::new(("del-vp", i))
                                    .small()
                                    .ghost()
                                    .icon(IconName::Delete)
                                    .label("删除")
                                    .on_click(move |_, _, cx| app.update(cx, |a, cx| a.delete_voiceprint(vid, cx))),
                            )
                    })),
            )
            .child(
                Button::new("del-person")
                    .small()
                    .danger()
                    .outline()
                    .label("删除这个人")
                    .on_click(move |_, _, cx| a2.update(cx, |a, cx| a.delete_person(id, cx))),
            )
    }
}

impl Render for PeoplePage {
    /// Renders the people list and the selected person.
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let people = self.app.read(cx).people.clone();
        let list =
            match &people {
                Loadable::Ready(rows) if rows.is_empty() => {
                    ui::muted("还没有人物。先在上面添加。", cx).into_any_element()
                }
                Loadable::Ready(rows) => v_flex()
                    .gap_1()
                    .children(rows.iter().enumerate().map(|(i, r)| {
                        let id = r.person.id;
                        let active = self.selected == Some(id);
                        h_flex()
                            .id(("person", i))
                            .p_2()
                            .gap_2()
                            .rounded_md()
                            .cursor_pointer()
                            .when(active, |el| el.bg(cx.theme().accent))
                            .hover(|s| s.bg(cx.theme().muted))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.selected = Some(id);
                                cx.notify();
                            }))
                            .child(div().flex_1().child(ui::speaker_name(
                                r.person.display_name.clone(),
                                Some(r.hue),
                            )))
                            .child(ui::muted(
                                if r.voiceprints.is_empty() {
                                    "未录入声纹".to_string()
                                } else {
                                    format!("声纹 {} 段", r.voiceprints.len())
                                },
                                cx,
                            ))
                    }))
                    .into_any_element(),
                Loadable::Failed(e) => ui::load_error("people-err", e, cx).into_any_element(),
                _ => ui::loading("正在读取…", cx).into_any_element(),
            };
        let selected = people.ready().and_then(|rows| {
            rows.iter()
                .find(|r| Some(r.person.id) == self.selected)
                .cloned()
        });
        let detail = match selected {
            Some(row) => self.detail(&row, cx).into_any_element(),
            None => ui::muted("选择左侧的人物，录入或管理声纹。", cx).into_any_element(),
        };
        h_flex()
            .size_full()
            .child(
                v_flex()
                    .w(px(340.))
                    .flex_none()
                    .h_full()
                    .p_4()
                    .gap_3()
                    .border_r_1()
                    .border_color(cx.theme().border)
                    .child(ui::page_head(
                        "说话人",
                        "录入声纹后，字幕直接显示名字。",
                        cx,
                    ))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().child(Input::new(&self.name)))
                            .child(
                                Button::new("add-person")
                                    .outline()
                                    .icon(IconName::Plus)
                                    .label("添加")
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.add(window, cx)),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .id("people-list")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scrollbar()
                            .child(list),
                    ),
            )
            .child(
                div()
                    .id("people-detail")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .p_6()
                    .overflow_y_scrollbar()
                    .child(detail),
            )
    }
}
