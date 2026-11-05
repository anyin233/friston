//! 今天: next meeting, today's schedule, recent meetings and action items.

use chrono::{Datelike, Local};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use mm_core::CalendarEvent;

use crate::logic;
use crate::pages::Page;
use crate::state::{AppEvent, AppState, Loadable, now};
use crate::ui;

/// The 今天 page.
pub struct TodayPage {
    app: Entity<AppState>,
}

impl TodayPage {
    /// Creates the page and re-renders when the app state changes.
    ///
    /// Params: `app`; `cx`.
    /// Returns: the page.
    pub fn new(app: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        cx.observe(&app, |_, _, cx| cx.notify()).detach();
        Self { app }
    }

    /// Today's events from the 7-day window.
    ///
    /// Params: `cx`.
    /// Returns: events starting today, in order.
    fn today_events(&self, cx: &App) -> Option<Vec<CalendarEvent>> {
        let today = Local::now().date_naive();
        self.app.read(cx).events.ready().map(|evs| {
            logic::group_by_day(evs)
                .into_iter()
                .find(|(d, _)| *d == today)
                .map(|(_, e)| e)
                .unwrap_or_default()
        })
    }

    /// Card for the next meeting.
    fn next_card(&self, ev: &CalendarEvent, cx: &mut Context<Self>) -> impl IntoElement {
        let app = self.app.clone();
        let rec = ev.clone();
        let join = ev.join_url.clone();
        ui::card(cx)
            .child(
                h_flex()
                    .gap_3()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        div()
                            .text_color(cx.theme().primary)
                            .child(logic::relative_start(now(), ev.start, ev.end)),
                    )
                    .child(logic::time_span(ev.start, ev.end))
                    .when(!ev.calendar.is_empty(), |el| el.child(ev.calendar.clone())),
            )
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(ev.title.clone()),
            )
            .when(!ev.attendees.is_empty(), |el| {
                el.child(ui::muted(
                    format!("参会者：{}", ev.attendees.join("、")),
                    cx,
                ))
            })
            .child(
                h_flex()
                    .gap_2()
                    .when_some(join, |el, url| {
                        el.child(
                            Button::new("next-join")
                                .outline()
                                .icon(IconName::ExternalLink)
                                .label("加入会议")
                                .on_click(move |_, _, cx| cx.open_url(&url)),
                        )
                    })
                    .child(
                        Button::new("next-record")
                            .primary()
                            .icon(IconName::Mic)
                            .label("开始记录")
                            .on_click(move |_, _, cx| {
                                app.update(cx, |a, cx| a.record_event(rec.clone(), cx));
                            }),
                    ),
            )
    }

    /// Today's schedule rows.
    fn day_list(&self, events: &[CalendarEvent], cx: &mut Context<Self>) -> impl IntoElement {
        let now = now();
        v_flex()
            .gap_1()
            .children(events.iter().enumerate().map(|(i, ev)| {
                let app = self.app.clone();
                let rec = ev.clone();
                let ended = ev.end <= now;
                h_flex()
                    .gap_3()
                    .py_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .items_center()
                    .child(
                        div()
                            .w(px(48.))
                            .text_sm()
                            .child(ev.start.with_timezone(&Local).format("%H:%M").to_string()),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .child(
                                div()
                                    .text_color(if ended {
                                        cx.theme().muted_foreground
                                    } else {
                                        cx.theme().foreground
                                    })
                                    .child(ev.title.clone()),
                            )
                            .child(ui::muted(
                                [
                                    ev.location.clone().unwrap_or_default(),
                                    logic::relative_start(now, ev.start, ev.end),
                                ]
                                .into_iter()
                                .filter(|s| !s.is_empty())
                                .collect::<Vec<_>>()
                                .join(" · "),
                                cx,
                            )),
                    )
                    .when(!ended, |el| {
                        el.child(
                            Button::new(("today-rec", i))
                                .small()
                                .outline()
                                .label("开始记录")
                                .on_click(move |_, _, cx| {
                                    app.update(cx, |a, cx| a.record_event(rec.clone(), cx))
                                }),
                        )
                    })
            }))
    }

    /// Right rail: recent meetings and action items.
    fn rail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.app.read(cx);
        let meetings = match &state.meetings {
            Loadable::Ready(m) => {
                let rows: Vec<_> = m.iter().take(8).cloned().collect();
                if rows.is_empty() {
                    ui::muted("还没有会议记录。", cx).into_any_element()
                } else {
                    v_flex()
                        .gap_1()
                        .children(rows.into_iter().enumerate().map(|(i, m)| {
                            let app = self.app.clone();
                            let id = m.id;
                            h_flex()
                                .id(("recent", i))
                                .gap_2()
                                .p_2()
                                .rounded_md()
                                .cursor_pointer()
                                .hover(|s| s.bg(cx.theme().muted))
                                .on_click(move |_, _, cx| {
                                    app.update(cx, |_, cx| cx.emit(AppEvent::OpenMeeting(id, None)))
                                })
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .child(div().text_sm().truncate().child(m.title.clone()))
                                        .child(ui::muted(logic::short_datetime(m.starts_at), cx)),
                                )
                                .child(ui::status_tag(m.status))
                        }))
                        .into_any_element()
                }
            }
            Loadable::Failed(e) => ui::load_error("meet-err", e, cx).into_any_element(),
            _ => ui::loading("正在读取会议…", cx).into_any_element(),
        };
        let actions = if state.actions.is_empty() {
            ui::muted("纪要里的待办会出现在这里。", cx).into_any_element()
        } else {
            v_flex()
                .gap_2()
                .children(state.actions.iter().map(|a| {
                    let src = [
                        Some(a.meeting_title.clone()),
                        a.owner.clone(),
                        a.due.clone(),
                    ]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" · ");
                    v_flex()
                        .child(div().text_sm().child(a.task.clone()))
                        .child(ui::muted(src, cx))
                }))
                .into_any_element()
        };
        let stats = state
            .index_stats
            .map(|s| format!("已索引 {} 个片段，{} 个已嵌入", s.chunks, s.embedded))
            .unwrap_or_default();
        v_flex()
            .w(px(320.))
            .flex_none()
            .gap_5()
            .child(
                v_flex()
                    .gap_2()
                    .child(ui::section_title("最近的会议"))
                    .child(meetings),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(ui::section_title("我的待办"))
                    .child(actions),
            )
            .child(ui::muted(stats, cx))
    }
}

impl Render for TodayPage {
    /// Renders the page.
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let today = Local::now();
        const WEEK: [&str; 7] = [
            "星期一",
            "星期二",
            "星期三",
            "星期四",
            "星期五",
            "星期六",
            "星期日",
        ];
        let title = format!(
            "{}月{}日，{}",
            today.month(),
            today.day(),
            WEEK[today.weekday().num_days_from_monday() as usize]
        );
        let events = self.today_events(cx);
        let upcoming = events
            .as_deref()
            .map(|e| logic::upcoming(e, now()))
            .unwrap_or_default();
        let sub = match &events {
            Some(e) if e.is_empty() => "今天没有日历上的会议。随时可以点“开始记录”。".to_string(),
            Some(e) => format!("今天有 {} 场会议，还剩 {} 场。", e.len(), upcoming.len()),
            None => "正在读取日历…".to_string(),
        };
        let app = self.app.clone();
        let calendar = match (&self.app.read(cx).events, &events) {
            (Loadable::Failed(e), _) => ui::load_error("cal-err-today", e, cx).into_any_element(),
            (_, Some(evs)) if !evs.is_empty() => self.day_list(evs, cx).into_any_element(),
            (_, Some(_)) => ui::muted("没有安排。", cx).into_any_element(),
            _ => ui::loading("正在读取日历…", cx).into_any_element(),
        };
        let main = v_flex()
            .flex_1()
            .min_w_0()
            .gap_5()
            .child(
                h_flex()
                    .justify_between()
                    .items_start()
                    .child(ui::page_head(title, sub, cx))
                    .child(
                        Button::new("today-quick")
                            .primary()
                            .icon(IconName::Mic)
                            .label("开始记录")
                            .on_click(move |_, _, cx| {
                                app.update(cx, |a, cx| a.quick_record(String::new(), cx))
                            }),
                    ),
            )
            .when_some(upcoming.first(), |el, ev| el.child(self.next_card(ev, cx)))
            .child(
                v_flex().gap_2().child(
                    h_flex()
                        .justify_between()
                        .child(ui::section_title("今天的日程"))
                        .child({
                            let app = self.app.clone();
                            Button::new("today-schedule")
                                .ghost()
                                .small()
                                .label("查看一周")
                                .on_click(move |_, _, cx| {
                                    app.update(cx, |a, cx| a.navigate(Page::Schedule, cx))
                                })
                        }),
                ),
            )
            .child(calendar);
        h_flex()
            .id("today-scroll")
            .size_full()
            .items_start()
            .gap_8()
            .p_6()
            .overflow_y_scrollbar()
            .child(main)
            .child(self.rail(cx))
    }
}
