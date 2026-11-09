//! 会议中: capture setup, live captions with speakers, levels and the notes editor.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{ActiveTheme as _, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use mm_core::{AudioOrigin, CaptureState, LevelSource, SpeakerLabel};
use mm_i18n::{tr, trf};

use crate::assign;
use crate::logic;
use crate::pages::Page;
use crate::state::{AppEvent, AppState, LiveSession};
use crate::ui;

/// Dark caption stage colours (from the prototype: `--stage`, `--stage-ink`, `--stage-dim`).
fn stage_bg() -> Hsla {
    rgb(0x141634).into()
}
fn stage_ink() -> Hsla {
    rgb(0xECEEFF).into()
}
fn stage_dim() -> Hsla {
    rgb(0x9DA3D6).into()
}

/// Channel count of an input, e.g. `2 声道` / `2 channels`.
///
/// Params: `n` – number of channels.
/// Returns: the localized count (English needs a singular for mono inputs).
fn channel_count(n: u32) -> String {
    let en = logic::count_en(u64::from(n), "channel", "channels");
    trf!("{n} 声道", "{en}")
}

/// The 会议中 page.
pub struct LivePage {
    app: Entity<AppState>,
    /// Input placeholders, rewritten when the interface language changes.
    placeholders: ui::Placeholders,
    notes: Entity<TextareaState>,
    new_person: Entity<InputState>,
    captions_scroll: ScrollHandle,
    caption_count: usize,
}

impl LivePage {
    /// Creates the page and wires the notes editor and selects.
    ///
    /// Params: `app`; `window`; `cx`.
    /// Returns: the page.
    pub fn new(app: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let notes = cx.new(|cx| TextareaState::new(window, cx));
        let new_person = cx.new(|cx| InputState::new(window, cx));
        cx.observe_in(&app, window, |this, _, _, cx| this.follow_captions(cx))
            .detach();
        cx.subscribe_in(&app, window, |this, _, ev: &AppEvent, window, cx| {
            if let AppEvent::LiveStarted { notes } = ev {
                this.notes
                    .update(cx, |s, cx| s.set_value(notes.clone(), window, cx));
                this.caption_count = 0;
            }
        })
        .detach();
        cx.subscribe_in(
            &notes,
            window,
            |this, state, ev: &InputEvent, window, cx| match ev {
                InputEvent::Change => {
                    let text = state.read(cx).value().to_string();
                    this.app.update(cx, |a, cx| a.notes_changed(text, cx));
                }
                InputEvent::PressEnter {
                    secondary: true, ..
                } => {
                    let elapsed = this.app.read(cx).live.as_ref().map(LiveSession::elapsed_ms);
                    if let Some(ms) = elapsed {
                        state.update(cx, |s, cx| s.insert(logic::timestamp_token(ms), window, cx));
                    }
                }
                _ => {}
            },
        )
        .detach();
        let mut placeholders = ui::Placeholders::default();
        placeholders.textarea(&notes, || tr!(
                "在这里记笔记。⌘↩ 插入当前时间戳，笔记会自动保存到 Obsidian 笔记的“笔记”部分。",
                "Take notes here. ⌘↩ inserts the current timestamp; notes save automatically to the Notes section of the Obsidian note."
            ));
        placeholders.input(&new_person, || tr!("新人物的名字", "New person’s name"));
        placeholders.sync(window, cx);
        Self {
            app,
            placeholders,
            notes,
            new_person,
            captions_scroll: ScrollHandle::new(),
            caption_count: 0,
        }
    }

    /// Keeps the caption list scrolled to the newest line.
    ///
    /// Params: `cx` – the page context.
    fn follow_captions(&mut self, cx: &mut Context<Self>) {
        let n = self
            .app
            .read(cx)
            .live
            .as_ref()
            .map(|l| l.board.lines().len())
            .unwrap_or(0);
        if n != self.caption_count {
            self.caption_count = n;
            self.captions_scroll.scroll_to_bottom();
        }
        cx.notify();
    }

    /// Shown when nothing is recording: where to start, where the devices live, and
    /// whether the caption engine is ready.
    ///
    /// Params: `cx` – the page context.
    /// Returns: the card.
    fn idle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let engine_line = self
            .app
            .read(cx)
            .meeting_state
            .describe(tr!("字幕引擎 ", "Caption engine"));
        let app = self.app.clone();
        ui::card(cx)
            .max_w(px(720.))
            .gap_3()
            .child(ui::section_title(tr!("还没有开始记录", "Not recording")))
            .child(ui::muted(
                tr!(
                    "点侧边栏的“开始记录”开始。日历上有正在进行或 10 分钟内开始的会议时，可以直接记录这场会议。",
                    "Click Start recording in the sidebar. When a calendar meeting is running or starts within 10 minutes, you can record that meeting directly."
                ),
                cx,
            ))
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(
                        Button::new("live-open-settings")
                            .small()
                            .outline()
                            .icon(IconName::Settings)
                            .label(tr!("录制设置", "Recording settings"))
                            .on_click(move |_, _, cx| {
                                app.update(cx, |a, cx| a.navigate(Page::Settings, cx))
                            }),
                    )
                    .child(ui::muted(engine_line, cx)),
            )
    }

    /// Post-meeting progress card.
    fn post_card(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let post = self.app.read(cx).post.clone()?;
        let app = self.app.clone();
        let id = post.meeting_id;
        Some(
            ui::card(cx)
                .max_w(px(720.))
                .child(
                    h_flex()
                        .justify_between()
                        .child(ui::section_title(trf!(
                            "会后整理：{}",
                            "After the meeting: {}",
                            post.title
                        )))
                        .child(if post.running {
                            Tag::warning()
                                .small()
                                .child(tr!("进行中", "In progress"))
                                .into_any_element()
                        } else {
                            Button::new("post-open")
                                .small()
                                .outline()
                                .label(tr!("在纪要库中查看", "View in library"))
                                .on_click(move |_, _, cx| {
                                    app.update(cx, |_, cx| cx.emit(AppEvent::OpenMeeting(id, None)))
                                })
                                .into_any_element()
                        }),
                )
                .children(
                    post.steps
                        .iter()
                        .map(|s| div().text_sm().child(format!("· {s}"))),
                )
                .when(post.running, |el| {
                    el.child(ui::loading(
                        tr!(
                            "请稍候，录音已经安全保存。",
                            "Please wait. The recording is saved safely."
                        ),
                        cx,
                    ))
                })
                .children(post.warnings.iter().map(|w| {
                    div()
                        .text_sm()
                        .text_color(cx.theme().warning)
                        .child(format!("⚠ {w}"))
                })),
        )
    }

    /// Capture bar: state, timer, meters, actions.
    fn capbar(&self, live: &LiveSession, cx: &App) -> impl IntoElement {
        let state_tag = match live.capture_state {
            CaptureState::Recording => Tag::danger().small().child(tr!("录制中", "Recording")),
            CaptureState::Failed => Tag::danger()
                .small()
                .child(tr!("录制失败", "Recording failed")),
            CaptureState::Starting => Tag::warning().small().child(tr!("正在开始", "Starting")),
            CaptureState::Stopping | CaptureState::Stopped => {
                Tag::secondary().small().child(tr!("已停止", "Stopped"))
            }
            CaptureState::Idle => Tag::secondary().small().child(tr!("空闲", "Idle")),
        };
        let meter_of = |src: LevelSource| -> Vec<f32> {
            live.levels
                .get(&src)
                .map(|l| {
                    l.peak_db
                        .iter()
                        .map(|d| logic::level_fraction(*d))
                        .collect()
                })
                .unwrap_or_else(|| vec![0.0])
        };
        let mic = meter_of(LevelSource::Microphone);
        let sys = meter_of(LevelSource::System);
        let channels = live.started.microphone_channels.unwrap_or(1);
        let floating = self.app.read(cx).floating_open();
        let (a1, a2, a3) = (self.app.clone(), self.app.clone(), self.app.clone());
        let notes = self.notes.clone();
        h_flex()
            .h(px(52.))
            .flex_none()
            .px_4()
            .gap_3()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(state_tag)
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(live.meeting.title.clone()),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(logic::clock(live.elapsed_ms())),
            )
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .text_xs()
                    .child(trf!(
                        "麦克风 {}",
                        "Microphone, {}",
                        channel_count(channels)
                    ))
                    .child(ui::meter(&mic, cx.theme().success, cx)),
            )
            .when(self.app.read(cx).cfg.system_audio, |el| {
                el.child(
                    h_flex()
                        .gap_1()
                        .items_center()
                        .text_xs()
                        .child(tr!("系统声音", "System audio"))
                        .child(ui::meter(&sys, cx.theme().primary, cx)),
                )
            })
            .when_some(live.capture_error.clone(), |el, e| {
                el.child(div().text_xs().text_color(cx.theme().danger).child(e))
            })
            .child(div().flex_1())
            .child(
                Button::new("shot")
                    .small()
                    .outline()
                    .icon(IconName::Frame)
                    .label(tr!("截图", "Screenshot"))
                    .tooltip(tr!(
                        "截取录制窗口（未选窗口时截整个屏幕），插入笔记",
                        "Capture the recorded window (or the whole screen if none is selected) into the notes"
                    ))
                    .on_click(move |_, window, cx| {
                        let notes = notes.clone();
                        let handle = window.window_handle();
                        a1.update(cx, |a, cx| {
                            a.screenshot(cx, move |embed, cx| {
                                let _ = handle.update(cx, |_, window, cx| {
                                    notes.update(cx, |s, cx| {
                                        s.insert(format!("\n{embed}\n"), window, cx)
                                    });
                                });
                            })
                        });
                    }),
            )
            .child(
                Button::new("float")
                    .small()
                    .outline()
                    .toggled(floating)
                    .label(tr!("浮窗字幕", "Floating captions"))
                    .on_click(move |_, _, cx| {
                        a2.update(cx, |a, cx| {
                            if a.floating_open() {
                                a.close_floating(cx)
                            } else {
                                a.open_floating(cx)
                            }
                        })
                    }),
            )
            .child(
                Button::new("live-stop")
                    .small()
                    .danger()
                    .icon(IconName::Square)
                    .label(if self.app.read(cx).cfg.auto_minutes {
                        tr!("结束并生成纪要", "End and write minutes")
                    } else {
                        tr!("结束记录", "End recording")
                    })
                    .on_click(move |_, _, cx| a3.update(cx, |a, cx| a.stop_recording(cx))),
            )
    }

    /// Caption stage.
    fn captions(&self, live: &LiveSession, _cx: &App) -> impl IntoElement {
        let meeting = live.meeting.id;
        let lines: Vec<_> = live
            .board
            .lines()
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let name = live.directory.name(&l.speaker);
                let hue = live.directory.hue(&l.speaker);
                let color = ui::hue_color(hue);
                let cluster = match (&l.speaker, live.directory.person(&l.speaker)) {
                    (SpeakerLabel::Cluster { index }, None) => Some(*index),
                    _ => None,
                };
                let app = self.app.clone();
                let input = self.new_person.clone();
                h_flex()
                    .gap_3()
                    .px_4()
                    .py_2()
                    .items_start()
                    .child(div().w(px(3.)).h(px(40.)).rounded_sm().bg(color))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .text_xs()
                                    .text_color(stage_dim())
                                    .child(
                                        div()
                                            .id(("spk", i))
                                            .text_sm()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(color)
                                            .when(cluster.is_some(), |el| {
                                                el.cursor_pointer().underline()
                                            })
                                            .child(name)
                                            .when_some(cluster, |el, c| {
                                                el.on_click(move |_, window, cx| {
                                                    assign::open(
                                                        app.clone(),
                                                        meeting,
                                                        c,
                                                        input.clone(),
                                                        window,
                                                        cx,
                                                    )
                                                })
                                            }),
                                    )
                                    .when(l.origin == AudioOrigin::System, |el| {
                                        el.child(tr!("远程", "Remote"))
                                    })
                                    .when_some(
                                        match &l.speaker {
                                            SpeakerLabel::Known { score, .. } => Some(trf!(
                                                "声纹 {score:.2}",
                                                "Voiceprint {score:.2}"
                                            )),
                                            _ => None,
                                        },
                                        |el, s| el.child(s),
                                    )
                                    .when(!l.is_final, |el| el.child(tr!("正在说", "Speaking")))
                                    .child(logic::clock(l.start_ms)),
                            )
                            .child(div().text_lg().text_color(stage_ink()).child(
                                ui::caption_text(&l.text, l.stable_byte_len(), stage_dim()),
                            )),
                    )
            })
            .collect();
        let empty = lines.is_empty();
        v_flex()
            .w(px(500.))
            .flex_none()
            .bg(stage_bg())
            .child(
                h_flex()
                    .px_4()
                    .py_3()
                    .gap_2()
                    .text_sm()
                    .text_color(stage_dim())
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(stage_ink())
                            .child(tr!("实时字幕", "Live captions")),
                    )
                    .child(match live.started.microphone_channels {
                        Some(n) if n >= 2 => {
                            tr!("说话人：音色 + 方位", "Speakers: voice + direction")
                        }
                        _ => tr!("说话人：音色", "Speakers: voice"),
                    }),
            )
            .child(
                div()
                    .id("captions")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.captions_scroll)
                    .when(empty, |el| {
                        el.child(
                            div()
                                .p_4()
                                .text_sm()
                                .text_color(stage_dim())
                                .child(tr!("等待有人发言…", "Waiting for someone to speak…")),
                        )
                    })
                    .children(lines),
            )
            .child(
                div()
                    .px_4()
                    .py_2()
                    .text_xs()
                    .text_color(stage_dim())
                    .child(tr!(
                        "点击“说话人 N”可以把它命名为某个人。",
                        "Click “Speaker N” to name it as a person."
                    )),
            )
    }

    /// Notes editor pane.
    fn notes_pane(&self, live: &LiveSession, cx: &App) -> impl IntoElement {
        let elapsed = live.elapsed_ms();
        let notes = self.notes.clone();
        let meeting = live.meeting.clone();
        let app = self.app.clone();
        let footer = match &live.note_path {
            Some(p) => trf!("自动保存到 {}", "Saving automatically to {}", p.display()),
            None => tr!(
                "未设置 Obsidian 库，笔记不会写入文件",
                "No Obsidian vault set, so notes aren’t saved to a file"
            )
            .into(),
        };
        v_flex()
            .flex_1()
            .min_w_0()
            .child(
                h_flex()
                    .h(px(40.))
                    .px_3()
                    .gap_2()
                    .items_center()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        Button::new("ts")
                            .small()
                            .ghost()
                            .label(tr!("插入时间戳 ⌘↩", "Insert timestamp ⌘↩"))
                            .on_click(move |_, window, cx| {
                                notes.update(cx, |s, cx| {
                                    s.insert(logic::timestamp_token(elapsed), window, cx)
                                });
                            }),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("open-obsidian")
                            .small()
                            .ghost()
                            .icon(IconName::ExternalLink)
                            .label(tr!("在 Obsidian 中打开", "Open in Obsidian"))
                            .on_click(move |_, _, cx| {
                                app.update(cx, |a, cx| a.open_in_obsidian(&meeting, cx))
                            }),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .p_3()
                    .child(Textarea::new(&self.notes).h_full()),
            )
            .child(
                div()
                    .px_4()
                    .py_2()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .truncate()
                    .child(footer),
            )
    }

    /// Right pane: speaking time per speaker.
    fn speakers_pane(&self, live: &LiveSession, cx: &App) -> impl IntoElement {
        let talk = logic::talk_time(live.board.lines());
        let total: u64 = talk.iter().map(|(_, t)| t).sum::<u64>().max(1);
        v_flex()
            .w(px(260.))
            .flex_none()
            .p_4()
            .gap_3()
            .border_l_1()
            .border_color(cx.theme().border)
            .child(ui::section_title(tr!("发言时长", "Speaking time")))
            .when(talk.is_empty(), |el| {
                el.child(ui::muted(
                    tr!("还没有完成的发言。", "No finished turns yet."),
                    cx,
                ))
            })
            .children(talk.into_iter().map(|(label, ms)| {
                let hue = live.directory.hue(&label);
                let frac = ms as f32 / total as f32;
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .justify_between()
                            .child(ui::speaker_name(live.directory.name(&label), hue))
                            .child(ui::muted(format!("{:.0}%", frac * 100.0), cx)),
                    )
                    .child(
                        div()
                            .h(px(6.))
                            .w_full()
                            .rounded_sm()
                            .bg(cx.theme().muted)
                            .child(
                                div()
                                    .h_full()
                                    .rounded_sm()
                                    .bg(ui::hue_color(hue))
                                    .w(relative(frac)),
                            ),
                    )
            }))
            .children(self.directions(live, cx))
            .child(ui::section_title(tr!("录音位置", "Recording location")))
            .child(ui::muted(live.started.directory.display().to_string(), cx))
    }

    /// Speaker directions: a front half-circle with one dot per speaker, then 偏左/居中/偏右 rows.
    ///
    /// Shown only for a microphone with at least two channels whose engine has sent direction
    /// cues; mono, beamformed and virtual inputs never produce any, so nothing is drawn there.
    /// Params: `live` – the running session; `cx`.
    /// Returns: the section, or `None` without direction data.
    fn directions(&self, live: &LiveSession, cx: &App) -> Option<impl IntoElement> {
        if live.started.microphone_channels.unwrap_or(1) < 2 || live.positions.is_empty() {
            return None;
        }
        // Radius fits the 260 px pane minus padding; dots sit on the arc.
        const R: f32 = 96.;
        const DOT: f32 = 12.;
        let entries = live.positions.entries();
        let dots = entries.iter().map(|(label, x)| {
            let (dx, dy) = logic::seat_point(*x);
            div()
                .absolute()
                .left(px(DOT / 2. + R + dx * R - DOT / 2.))
                .top(px(DOT / 2. + R - dy * R - DOT / 2.))
                .size(px(DOT))
                .rounded_full()
                .border_2()
                .border_color(cx.theme().background)
                .bg(ui::hue_color(live.directory.hue(label)))
        });
        let seats = div()
            .relative()
            .w(px(2. * R + DOT))
            .h(px(R + DOT))
            .child(
                // The arc: top half of a circle, open towards the microphone at the bottom.
                div()
                    .absolute()
                    .left(px(DOT / 2.))
                    .top(px(DOT / 2.))
                    .w(px(2. * R))
                    .h(px(R))
                    .border_t_1()
                    .border_l_1()
                    .border_r_1()
                    .border_color(cx.theme().border)
                    .rounded_tl(px(R))
                    .rounded_tr(px(R)),
            )
            .child(
                div()
                    .absolute()
                    .left(px(DOT / 2. + R - 9.))
                    .top(px(R - 4.))
                    .w(px(18.))
                    .h(px(8.))
                    .rounded_sm()
                    .bg(cx.theme().muted_foreground),
            )
            .children(dots);
        Some(
            v_flex()
                .gap_2()
                .child(ui::section_title(tr!("方位", "Directions")))
                .child(seats)
                .children(entries.into_iter().map(|(label, x)| {
                    h_flex()
                        .justify_between()
                        .child(ui::speaker_name(
                            live.directory.name(&label),
                            live.directory.hue(&label),
                        ))
                        .child(ui::muted(logic::direction_label(x), cx))
                }))
                .child(ui::muted(
                    tr!(
                        "相对本场其他人的位置，下方是麦克风。",
                        "Positions relative to others in this meeting; the microphone is at the bottom."
                    ),
                    cx,
                ).text_xs()),
        )
    }
}

impl Render for LivePage {
    /// Renders setup or the running session.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.placeholders.sync(window, cx);
        let app = self.app.clone();
        let cx_ref: &App = cx;
        if let Some(live) = app.read(cx_ref).live.as_ref() {
            return v_flex()
                .size_full()
                .child(self.capbar(live, cx_ref))
                .child(
                    // `h_flex` centers items on the cross axis; stretch them so every
                    // pane fills the window height instead of shrinking to its content.
                    h_flex()
                        .flex_1()
                        .min_h_0()
                        .items_stretch()
                        .child(self.captions(live, cx_ref))
                        .child(self.notes_pane(live, cx_ref))
                        .child(self.speakers_pane(live, cx_ref)),
                )
                .into_any_element();
        }
        v_flex()
            .id("live-setup")
            .size_full()
            .p_6()
            .gap_5()
            .overflow_y_scrollbar()
            .child(ui::page_head(
                tr!("会议中", "Live"),
                tr!(
                    "字幕、说话人和笔记会在记录时实时显示在这里。",
                    "Captions, speakers and notes appear here live while recording."
                ),
                cx,
            ))
            .children(self.post_card(cx))
            .child(self.idle(cx))
            .into_any_element()
    }
}
