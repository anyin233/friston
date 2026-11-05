//! 会议中: capture setup, live captions with speakers, levels and the notes editor.

use gpui_kit::component::Disableable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{ActiveTheme as _, IconName, IndexPath, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use mm_core::{AudioOrigin, CaptureState, LevelSource, SpeakerLabel};

use crate::assign;
use crate::logic;
use crate::state::{AppEvent, AppState, EngineState, LiveSession, Loadable};
use crate::ui::{self, Choice};

/// Value of the "no window" choice.
const NO_WINDOW: &str = "";
/// Value of the "system default microphone" choice.
const DEFAULT_MIC: &str = "";

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

/// The 会议中 page.
pub struct LivePage {
    app: Entity<AppState>,
    title: Entity<InputState>,
    notes: Entity<TextareaState>,
    windows: Entity<SelectState<Vec<Choice>>>,
    mics: Entity<SelectState<Vec<Choice>>>,
    new_person: Entity<InputState>,
    captions_scroll: ScrollHandle,
    /// Signatures of the select items, so selects are rebuilt only when lists change.
    window_sig: Vec<String>,
    mic_sig: Vec<String>,
    caption_count: usize,
}

impl LivePage {
    /// Creates the page and wires the notes editor and selects.
    ///
    /// Params: `app`; `window`; `cx`.
    /// Returns: the page.
    pub fn new(app: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let title = cx.new(|cx| InputState::new(window, cx).placeholder("会议标题（可留空）"));
        let notes = cx.new(|cx| {
            TextareaState::new(window, cx).placeholder(
                "在这里记笔记。⌘↩ 插入当前时间戳，笔记会自动保存到 Obsidian 笔记的“笔记”部分。",
            )
        });
        let new_person = cx.new(|cx| InputState::new(window, cx).placeholder("新人物的名字"));
        let windows = cx.new(|cx| {
            SelectState::new(
                vec![Choice::new(NO_WINDOW, "不录屏，只录声音")],
                Some(IndexPath::new(0)),
                window,
                cx,
            )
        });
        let mics = cx.new(|cx| {
            SelectState::new(
                vec![Choice::new(DEFAULT_MIC, "系统默认麦克风")],
                Some(IndexPath::new(0)),
                window,
                cx,
            )
        });

        cx.observe_in(&app, window, |this, _, window, cx| {
            this.sync_selects(window, cx)
        })
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
        cx.subscribe_in(
            &windows,
            window,
            |this, _, ev: &SelectEvent<Vec<Choice>>, _, cx| {
                let SelectEvent::Confirm(v) = ev;
                let id = v.as_deref().and_then(|s| s.parse::<u32>().ok());
                this.app.update(cx, |a, cx| {
                    a.selected_window = id;
                    cx.notify();
                });
            },
        )
        .detach();
        cx.subscribe_in(
            &mics,
            window,
            |this, _, ev: &SelectEvent<Vec<Choice>>, _, cx| {
                let SelectEvent::Confirm(v) = ev;
                let uid = v.clone().filter(|s| !s.is_empty());
                this.app
                    .update(cx, |a, cx| a.update_config(cx, |c| c.microphone_uid = uid));
            },
        )
        .detach();

        Self {
            app,
            title,
            notes,
            windows,
            mics,
            new_person,
            captions_scroll: ScrollHandle::new(),
            window_sig: vec![],
            mic_sig: vec![],
            caption_count: 0,
        }
    }

    /// Rebuilds select items when the app's window/microphone lists change and keeps the
    /// caption list scrolled to the newest line.
    fn sync_selects(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let app = self.app.read(cx);
        if let Loadable::Ready(src) = &app.sources {
            let mut items = vec![Choice::new(NO_WINDOW, "不录屏，只录声音")];
            items.extend(src.windows.iter().map(|w| {
                let title = if w.title.is_empty() {
                    w.app_name.clone()
                } else {
                    format!("{} — {}", w.app_name, w.title)
                };
                Choice::new(w.window_id.to_string(), title)
            }));
            let sig: Vec<String> = items.iter().map(|c| c.value.clone()).collect();
            if sig != self.window_sig {
                self.window_sig = sig;
                let selected = app
                    .selected_window
                    .map(|w| w.to_string())
                    .unwrap_or_default();
                self.windows.update(cx, |s, cx| {
                    s.set_items(items, window, cx);
                    s.set_selected_value(&selected, window, cx);
                });
            }
        }
        let app = self.app.read(cx);
        if let Loadable::Ready(inputs) = &app.inputs {
            let mut items = vec![Choice::new(DEFAULT_MIC, "系统默认麦克风")];
            items.extend(inputs.iter().map(|i| {
                let spatial = if i.supports_spatial_cues() {
                    "，可判断方位"
                } else {
                    ""
                };
                Choice::new(
                    i.uid.clone(),
                    format!("{}（{} 声道{spatial}）", i.name, i.channels),
                )
            }));
            let sig: Vec<String> = items.iter().map(|c| c.value.clone()).collect();
            if sig != self.mic_sig {
                self.mic_sig = sig;
                let selected = app.cfg.microphone_uid.clone().unwrap_or_default();
                self.mics.update(cx, |s, cx| {
                    s.set_items(items, window, cx);
                    s.set_selected_value(&selected, window, cx);
                });
            }
        }
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

    /// Capture setup shown when nothing is recording.
    fn setup(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let app = self.app.read(cx);
        let engine_ready = app.meeting_state == EngineState::Ready;
        let starting = app.live_starting;
        let cfg = app.cfg.clone();
        let spatial_hint = app
            .inputs
            .ready()
            .and_then(|ins| {
                ins.iter()
                    .find(|i| Some(&i.uid) == cfg.microphone_uid.as_ref())
            })
            .map(|i| {
                if i.supports_spatial_cues() {
                    format!(
                        "{} 有 {} 个声道，可以根据方位区分说话人。",
                        i.name, i.channels
                    )
                } else {
                    format!("{} 是单声道或虚拟设备，只按音色区分说话人。", i.name)
                }
            });
        let sources_err = match &app.sources {
            Loadable::Failed(e) => Some(e.clone()),
            _ => None,
        };
        let inputs_err = match &app.inputs {
            Loadable::Failed(e) => Some(e.clone()),
            _ => None,
        };
        let engine_line = app.meeting_state.describe("字幕引擎 ");
        let a1 = self.app.clone();
        let a2 = self.app.clone();
        let a3 = self.app.clone();
        let a4 = self.app.clone();
        let a5 = self.app.clone();
        let title = self.title.clone();
        ui::card(cx)
            .max_w(px(720.))
            .gap_4()
            .child(ui::section_title("开始一场记录"))
            .child(Input::new(&self.title))
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().w(px(90.)).text_sm().child("录制窗口"))
                            .child(div().flex_1().child(Select::new(&self.windows).small()))
                            .child(
                                Button::new("refresh-windows")
                                    .small()
                                    .outline()
                                    .icon(IconName::RefreshCw)
                                    .label("读取窗口")
                                    .on_click(move |_, _, cx| {
                                        a1.update(cx, |a, cx| a.refresh_sources(cx))
                                    }),
                            ),
                    )
                    .when_some(sources_err, |el, e| {
                        el.child(ui::load_error("src-err", &e, cx))
                    }),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().w(px(90.)).text_sm().child("麦克风"))
                            .child(div().flex_1().child(Select::new(&self.mics).small()))
                            .child(
                                Button::new("refresh-mics")
                                    .small()
                                    .outline()
                                    .icon(IconName::RefreshCw)
                                    .on_click(move |_, _, cx| {
                                        a2.update(cx, |a, cx| a.refresh_inputs(cx))
                                    }),
                            ),
                    )
                    .when_some(spatial_hint, |el, h| el.child(ui::muted(h, cx)))
                    .when_some(inputs_err, |el, e| {
                        el.child(ui::load_error("mic-err", &e, cx))
                    }),
            )
            .child(
                h_flex()
                    .gap_6()
                    .child(
                        Switch::new("sys-audio")
                            .checked(cfg.system_audio)
                            .label("录系统声音（远程参会者）")
                            .on_click(move |v, _, cx| {
                                let v = *v;
                                a3.update(cx, |a, cx| a.update_config(cx, |c| c.system_audio = v));
                            }),
                    )
                    .child(
                        Switch::new("rec-screen")
                            .checked(cfg.record_screen)
                            .label("录制所选窗口画面")
                            .on_click(move |v, _, cx| {
                                let v = *v;
                                a4.update(cx, |a, cx| a.update_config(cx, |c| c.record_screen = v));
                            }),
                    ),
            )
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(
                        Button::new("live-start")
                            .primary()
                            .icon(IconName::Mic)
                            .label(if starting {
                                "正在开始…"
                            } else {
                                "开始记录"
                            })
                            .loading(starting)
                            .disabled(!engine_ready || starting)
                            .on_click(move |_, _, cx| {
                                let t = title.read(cx).value().to_string();
                                a5.update(cx, |a, cx| a.quick_record(t, cx));
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
                        .child(ui::section_title(format!("会后整理：{}", post.title)))
                        .child(if post.running {
                            Tag::warning().small().child("进行中").into_any_element()
                        } else {
                            Button::new("post-open")
                                .small()
                                .outline()
                                .label("在纪要库中查看")
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
                    el.child(ui::loading("请稍候，录音已经安全保存。", cx))
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
            CaptureState::Recording => Tag::danger().small().child("录制中"),
            CaptureState::Failed => Tag::danger().small().child("录制失败"),
            CaptureState::Starting => Tag::warning().small().child("正在开始"),
            CaptureState::Stopping | CaptureState::Stopped => {
                Tag::secondary().small().child("已停止")
            }
            CaptureState::Idle => Tag::secondary().small().child("空闲"),
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
                    .child(format!("麦克风 {channels} 声道"))
                    .child(ui::meter(&mic, cx.theme().success, cx)),
            )
            .when(self.app.read(cx).cfg.system_audio, |el| {
                el.child(
                    h_flex()
                        .gap_1()
                        .items_center()
                        .text_xs()
                        .child("系统声音")
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
                    .label("截图")
                    .tooltip("截取录制窗口（未选窗口时截整个屏幕），插入笔记")
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
                    .label("浮窗字幕")
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
                        "结束并生成纪要"
                    } else {
                        "结束记录"
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
                                    .when(l.origin == AudioOrigin::System, |el| el.child("远程"))
                                    .when_some(
                                        match &l.speaker {
                                            SpeakerLabel::Known { score, .. } => {
                                                Some(format!("声纹 {score:.2}"))
                                            }
                                            _ => None,
                                        },
                                        |el, s| el.child(s),
                                    )
                                    .when(!l.is_final, |el| el.child("正在说"))
                                    .child(logic::clock(l.start_ms)),
                            )
                            .child(
                                div()
                                    .text_lg()
                                    .text_color(if l.is_final { stage_ink() } else { stage_dim() })
                                    .child(l.text.clone()),
                            ),
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
                            .child("实时字幕"),
                    )
                    .child(match live.started.microphone_channels {
                        Some(n) if n >= 2 => "说话人：音色 + 方位",
                        _ => "说话人：音色",
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
                                .child("等待有人发言…"),
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
                    .child("点击“说话人 N”可以把它命名为某个人。"),
            )
    }

    /// Notes editor pane.
    fn notes_pane(&self, live: &LiveSession, cx: &App) -> impl IntoElement {
        let elapsed = live.elapsed_ms();
        let notes = self.notes.clone();
        let meeting = live.meeting.clone();
        let app = self.app.clone();
        let footer = match &live.note_path {
            Some(p) => format!("自动保存到 {}", p.display()),
            None => "未设置 Obsidian 库，笔记不会写入文件".into(),
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
                            .label("插入时间戳 ⌘↩")
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
                            .label("在 Obsidian 中打开")
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
            .child(ui::section_title("发言时长"))
            .when(talk.is_empty(), |el| {
                el.child(ui::muted("还没有完成的发言。", cx))
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
            .child(ui::section_title("录音位置"))
            .child(ui::muted(live.started.directory.display().to_string(), cx))
    }
}

impl Render for LivePage {
    /// Renders setup or the running session.
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let app = self.app.clone();
        let cx_ref: &App = cx;
        if let Some(live) = app.read(cx_ref).live.as_ref() {
            return v_flex()
                .size_full()
                .child(self.capbar(live, cx_ref))
                .child(
                    h_flex()
                        .flex_1()
                        .min_h_0()
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
                "会议中",
                "选择录制方式后开始记录。字幕、说话人和笔记会实时显示在这里。",
                cx,
            ))
            .children(self.post_card(cx))
            .child(self.setup(cx))
            .into_any_element()
    }
}
