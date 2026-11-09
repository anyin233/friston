//! Floating caption window: a borderless, always-on-top panel with the last lines.

use gpui_kit::component::{IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use mm_i18n::tr;

use crate::state::AppState;
use crate::ui;

/// Lines shown in the floating window.
const TAIL: usize = 3;
/// Window size.
const WIDTH: f32 = 860.;
const HEIGHT: f32 = 160.;

/// Opens the floating caption window near the bottom of the primary display.
///
/// Params: `app` – shared state; `cx`.
/// Returns: the window handle.
pub fn open(app: Entity<AppState>, cx: &mut App) -> anyhow::Result<AnyWindowHandle> {
    let sz = size(px(WIDTH), px(HEIGHT));
    let bounds = match cx.primary_display() {
        Some(d) => {
            let b = d.bounds();
            Bounds::new(
                point(
                    b.origin.x + (b.size.width - sz.width) / 2.,
                    b.origin.y + b.size.height - sz.height - px(80.),
                ),
                sz,
            )
        }
        None => Bounds::centered(None, sz, cx),
    };
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: None,
        kind: WindowKind::Floating,
        focus: false,
        is_movable: true,
        is_resizable: true,
        is_minimizable: false,
        window_background: WindowBackgroundAppearance::Transparent,
        ..Default::default()
    };
    let (handle, _) = gpui_kit::open_window(options, cx, |_, cx| {
        cx.new(|cx| FloatingCaptions::new(app, cx))
    })?;
    Ok(handle)
}

/// The floating caption view.
pub struct FloatingCaptions {
    app: Entity<AppState>,
}

impl FloatingCaptions {
    /// Creates the view and follows the app state.
    ///
    /// Params: `app`; `cx`.
    /// Returns: the view.
    pub fn new(app: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        cx.observe(&app, |_, _, cx| cx.notify()).detach();
        Self { app }
    }
}

impl Render for FloatingCaptions {
    /// Renders the last caption lines with speaker hues.
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let app = self.app.read(cx);
        let ink: Hsla = rgb(0xECEEFF).into();
        let dim: Hsla = rgb(0x9DA3D6).into();
        let (title, lines) = match &app.live {
            Some(live) => (
                format!(
                    "{} · {}",
                    live.meeting.title,
                    crate::logic::clock(live.elapsed_ms())
                ),
                live.board
                    .tail(TAIL)
                    .iter()
                    .map(|l| {
                        (
                            live.directory.name(&l.speaker),
                            live.directory.hue(&l.speaker),
                            l.text.clone(),
                            l.stable_byte_len(),
                        )
                    })
                    .collect::<Vec<_>>(),
            ),
            None => (
                tr!("没有正在进行的记录", "No recording in progress").to_string(),
                vec![],
            ),
        };
        let count = lines.len();
        let app_close = self.app.clone();
        v_flex()
            .size_full()
            .rounded_xl()
            .bg(hsla(236. / 360., 0.44, 0.14, 0.92))
            .px_5()
            .py_3()
            .gap_1()
            .child(
                h_flex()
                    .id("float-drag")
                    .gap_2()
                    .text_xs()
                    .text_color(dim)
                    .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
                    .child(div().flex_1().child(title))
                    .child(
                        div()
                            .id("float-close")
                            .cursor_pointer()
                            .child(
                                gpui_kit::component::Icon::new(IconName::Close)
                                    .small()
                                    .text_color(dim),
                            )
                            .on_click(move |_, window, cx| {
                                app_close.update(cx, |a, cx| {
                                    a.floating_closed();
                                    cx.notify();
                                });
                                window.remove_window();
                            }),
                    ),
            )
            .when(lines.is_empty(), |el| {
                el.child(
                    div()
                        .text_color(dim)
                        .child(tr!("等待有人发言…", "Waiting for someone to speak…")),
                )
            })
            .children(
                lines
                    .into_iter()
                    .enumerate()
                    .map(|(i, (name, hue, text, stable))| {
                        let last = i + 1 == count;
                        h_flex()
                            .gap_3()
                            .items_baseline()
                            .child(
                                div()
                                    .flex_none()
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(ui::hue_color(hue))
                                    .child(name),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    // Older lines recede; on the newest one only the
                                    // volatile tail is dimmed, so new words grow in place.
                                    .text_color(if last { ink } else { dim })
                                    .when(last, |el| el.text_xl())
                                    .child(crate::ui::caption_text(&text, stable, dim)),
                            )
                    }),
            )
    }
}
