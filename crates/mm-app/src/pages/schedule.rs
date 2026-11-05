//! 日程: the next seven days from the system calendar.

use chrono::Local;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use mm_core::CalendarEvent;

use crate::logic;
use crate::state::{AppState, Loadable, now};
use crate::ui;

/// The 日程 page.
pub struct SchedulePage {
    app: Entity<AppState>,
}

impl SchedulePage {
    /// Creates the page.
    ///
    /// Params: `app`; `cx`.
    /// Returns: the page.
    pub fn new(app: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        cx.observe(&app, |_, _, cx| cx.notify()).detach();
        Self { app }
    }

    /// One event card.
    ///
    /// Params: `ix` – unique index for element ids; `ev`; `cx`.
    fn event(&self, ix: usize, ev: &CalendarEvent, cx: &mut Context<Self>) -> impl IntoElement {
        let app = self.app.clone();
        let rec = ev.clone();
        let ended = ev.end <= now();
        ui::card(cx).child(
            h_flex()
                .gap_3()
                .items_start()
                .child(
                    v_flex()
                        .w(px(110.))
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(logic::time_span(ev.start, ev.end)),
                        )
                        .child(ui::muted(
                            logic::relative_start(now(), ev.start, ev.end),
                            cx,
                        )),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_1()
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(ev.title.clone()),
                        )
                        .child(ui::muted(
                            [Some(ev.calendar.clone()), ev.location.clone()]
                                .into_iter()
                                .flatten()
                                .filter(|s| !s.is_empty())
                                .collect::<Vec<_>>()
                                .join(" · "),
                            cx,
                        ))
                        .when(!ev.attendees.is_empty(), |el| {
                            el.child(ui::muted(
                                format!(
                                    "参会者（{}）：{}",
                                    ev.attendees.len(),
                                    ev.attendees.join("、")
                                ),
                                cx,
                            ))
                        })
                        .when_some(ev.join_url.clone(), |el, url| {
                            el.child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().link)
                                    .truncate()
                                    .child(url),
                            )
                        }),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .when_some(ev.join_url.clone(), |el, url| {
                            el.child(
                                Button::new(("join", ix))
                                    .small()
                                    .outline()
                                    .icon(IconName::ExternalLink)
                                    .label("加入")
                                    .on_click(move |_, _, cx| cx.open_url(&url)),
                            )
                        })
                        .when(!ended, |el| {
                            el.child(
                                Button::new(("record", ix))
                                    .small()
                                    .primary()
                                    .label("开始记录")
                                    .on_click(move |_, _, cx| {
                                        app.update(cx, |a, cx| a.record_event(rec.clone(), cx))
                                    }),
                            )
                        }),
                ),
        )
    }
}

impl Render for SchedulePage {
    /// Renders the 7-day list.
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let today = Local::now().date_naive();
        let body = match &self.app.read(cx).events {
            Loadable::Ready(evs) if evs.is_empty() => ui::muted("未来 7 天没有日历上的会议。", cx).into_any_element(),
            Loadable::Ready(evs) => {
                let groups = logic::group_by_day(evs);
                let mut ix = 0;
                let mut sections = Vec::new();
                for (day, evs) in groups {
                    let mut section = v_flex().gap_2().child(ui::section_title(logic::day_heading(day, today)));
                    for ev in &evs {
                        ix += 1;
                        section = section.child(self.event(ix, ev, cx));
                    }
                    sections.push(section);
                }
                v_flex().gap_5().children(sections).into_any_element()
            }
            Loadable::Failed(e) => v_flex()
                .gap_2()
                .child(ui::load_error("cal-err", e, cx))
                .when(e.permission, |el| {
                    el.child(ui::muted("在“系统设置 › 隐私与安全性 › 日历”里允许 Meeting Master 访问日历，然后点“刷新”。", cx))
                })
                .into_any_element(),
            _ => ui::loading("正在读取日历…", cx).into_any_element(),
        };
        let app = self.app.clone();
        v_flex()
            .id("schedule-scroll")
            .size_full()
            .p_6()
            .gap_5()
            .overflow_y_scrollbar()
            .child(
                h_flex()
                    .justify_between()
                    .items_start()
                    .child(ui::page_head(
                        "日程",
                        "来自系统日历的未来 7 天会议。点“开始记录”会创建会议笔记并开始录制。",
                        cx,
                    ))
                    .child(
                        Button::new("cal-refresh")
                            .outline()
                            .icon(IconName::RefreshCw)
                            .label("刷新")
                            .on_click(move |_, _, cx| {
                                app.update(cx, |a, cx| a.refresh_calendar(cx))
                            }),
                    ),
            )
            .child(body)
    }
}
