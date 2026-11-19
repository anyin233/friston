//! Floating caption window: a borderless, always-on-top panel with the last lines.
//!
//! On macOS the panel follows the user onto every Space, including other apps' full-screen
//! Spaces, and never takes focus from the app they are working in.

use gpui_kit::base::TestTextExt as _;
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
        // `PopUp` is the only kind GPUI creates as a non-activating `NSPanel` that joins all
        // Spaces as a full-screen auxiliary window; `Floating` stays on the Space it was
        // opened on and disappears behind full-screen apps. The style mask must be set at
        // creation: adding `NonactivatingPanel` later does not reach the window server.
        kind: WindowKind::PopUp,
        focus: false,
        is_movable: true,
        is_resizable: true,
        is_minimizable: false,
        window_background: WindowBackgroundAppearance::Transparent,
        ..Default::default()
    };
    let (handle, _) = gpui_kit::open_window(options, cx, |window, cx| {
        #[cfg(target_os = "macos")]
        if let Err(e) = overlay::pin_above_everything(window) {
            // Captions still work on the current Space; only the cross-Space pinning failed.
            tracing::warn!("floating captions: couldn't pin the panel above other windows: {e}");
        }
        #[cfg(not(target_os = "macos"))]
        let _ = window;
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
                    .child(
                        div()
                            .flex_1()
                            .child(SharedString::from(title.clone()).test_text(title)),
                    )
                    .child(
                        div()
                            .id("float-close")
                            .test_support()
                            .cursor_pointer()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
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
                let waiting: SharedString =
                    tr!("等待有人发言…", "Waiting for someone to speak…").into();
                el.child(
                    div()
                        .text_color(dim)
                        .child(waiting.clone().test_text(waiting)),
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
                                    .child(SharedString::from(name.clone()).test_text(name)),
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
                                    .child(crate::ui::caption_text(&text, stable, dim))
                                    .test_text(text),
                            )
                    }),
            )
    }
}

/// AppKit tweaks GPUI does not expose for the caption panel.
#[cfg(target_os = "macos")]
mod overlay {
    use gpui_kit::Window;
    use objc2::msg_send;
    use objc2::runtime::{AnyObject, Bool};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    // `NSWindowCollectionBehavior` bits (AppKit, NSWindow.h).
    const CAN_JOIN_ALL_SPACES: usize = 1 << 0;
    const MOVE_TO_ACTIVE_SPACE: usize = 1 << 1;
    const MANAGED: usize = 1 << 2;
    const STATIONARY: usize = 1 << 4;
    const PARTICIPATES_IN_CYCLE: usize = 1 << 5;
    const IGNORES_CYCLE: usize = 1 << 6;
    const FULL_SCREEN_PRIMARY: usize = 1 << 7;
    const FULL_SCREEN_AUXILIARY: usize = 1 << 8;

    /// Returns the collection behavior for an overlay that is visible on every Space.
    ///
    /// Params: `current` – the behavior GPUI set at creation.
    /// Returns: `current` with the overlay bits set and the conflicting ones cleared.
    pub(super) fn overlay_behavior(current: usize) -> usize {
        // AppKit raises if `CanJoinAllSpaces` is combined with `MoveToActiveSpace`, or
        // `FullScreenAuxiliary` with `FullScreenPrimary`; `Managed` would let Mission
        // Control move the panel, and `ParticipatesInCycle` would put it in Cmd-`.
        let cleared = current
            & !(MOVE_TO_ACTIVE_SPACE | MANAGED | PARTICIPATES_IN_CYCLE | FULL_SCREEN_PRIMARY);
        cleared | CAN_JOIN_ALL_SPACES | FULL_SCREEN_AUXILIARY | STATIONARY | IGNORES_CYCLE
    }

    /// Keeps the caption panel above every window, on every Space and over full-screen apps.
    ///
    /// Params: `window` – the GPUI window that hosts the captions (a `PopUp` `NSPanel`).
    /// Returns: an error when the native window can't be reached.
    pub(super) fn pin_above_everything(window: &Window) -> anyhow::Result<()> {
        // `Window` has an inherent `window_handle()` (the GPUI handle), so name the trait.
        let handle = HasWindowHandle::window_handle(window)
            .map_err(|e| anyhow::anyhow!("no native handle for the caption window: {e}"))?;
        let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
            anyhow::bail!("caption window handle is not AppKit: {:?}", handle.as_raw());
        };
        let view: *mut AnyObject = appkit.ns_view.as_ptr().cast();
        // SAFETY: GPUI calls the build closure on the main thread with a live content view,
        // and every selector below exists on `NSView` / `NSWindow` / `NSPanel`.
        unsafe {
            let ns_window: *mut AnyObject = msg_send![view, window];
            if ns_window.is_null() {
                anyhow::bail!("caption view {view:p} is not in a window yet");
            }
            let current: usize = msg_send![ns_window, collectionBehavior];
            let _: () = msg_send![ns_window, setCollectionBehavior: overlay_behavior(current)];
            // NSPanel hides itself whenever this app deactivates, which is exactly when the
            // user switches to the meeting app.
            let _: () = msg_send![ns_window, setHidesOnDeactivate: Bool::NO];
            // Clicking the close button or dragging must not take key focus away from the
            // app the user is typing in.
            let _: () = msg_send![ns_window, setBecomesKeyOnlyIfNeeded: Bool::YES];
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The overlay joins every Space as a full-screen auxiliary and stays out of Cmd-`.
        #[test]
        fn overlay_behavior_sets_all_spaces_and_full_screen_bits() {
            let b = overlay_behavior(0);
            assert_eq!(b & CAN_JOIN_ALL_SPACES, CAN_JOIN_ALL_SPACES);
            assert_eq!(b & FULL_SCREEN_AUXILIARY, FULL_SCREEN_AUXILIARY);
            assert_eq!(b & STATIONARY, STATIONARY);
            assert_eq!(b & IGNORES_CYCLE, IGNORES_CYCLE);
        }

        /// Bits AppKit rejects alongside the overlay bits are cleared; unrelated ones survive.
        #[test]
        fn overlay_behavior_clears_conflicting_bits() {
            let unrelated = 1 << 11; // NSWindowCollectionBehaviorAllowsTiling-style extra bit
            let b = overlay_behavior(
                MOVE_TO_ACTIVE_SPACE
                    | MANAGED
                    | PARTICIPATES_IN_CYCLE
                    | FULL_SCREEN_PRIMARY
                    | unrelated,
            );
            assert_eq!(
                b & (MOVE_TO_ACTIVE_SPACE | MANAGED | PARTICIPATES_IN_CYCLE | FULL_SCREEN_PRIMARY),
                0
            );
            assert_eq!(b & unrelated, unrelated);
        }
    }
}
