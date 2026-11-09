//! Keyboard shortcuts and the macOS menu bar.
//!
//! Every shortcut is also a menu item, so the menu bar is where users discover them. The
//! actions are handled at the app level (not on a view) so they work from any page, with
//! focus in a text field, and from the menu bar while the main window is hidden.
//!
//! Closing the main window (⌘W or the red traffic light) only hides it: a recording keeps
//! running behind it, and the Dock icon or ⌘0 brings the same window back with the notes
//! editor intact. ⌘Q asks first while something would be lost.

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::ButtonVariant;
use gpui_kit::component::input;
use gpui_kit::*;
use mm_i18n::{tr, trf};

use crate::pages::Page;
use crate::state::{AppState, NoticeKind};

actions!(
    friston,
    [
        /// Quit the app (asks first while recording or processing).
        Quit,
        /// Close the active window; the main window is hidden, not destroyed.
        CloseWindow,
        /// Minimize the active window.
        Minimize,
        /// Zoom the active window.
        Zoom,
        /// Hide the app.
        HideApp,
        /// Hide every other app.
        HideOthers,
        /// Show every app.
        ShowAll,
        /// Bring the main window back.
        ShowMainWindow,
        /// Open the Settings page.
        OpenSettings,
        /// Record the calendar meeting that is on now, or an ad-hoc meeting.
        StartRecording,
        /// Always start an ad-hoc meeting.
        AdhocRecording,
        /// Stop the running recording.
        StopRecording,
        /// Show or hide the floating captions.
        ToggleFloatingCaptions,
        /// Go to the Today page.
        GoToday,
        /// Go to the Schedule page.
        GoSchedule,
        /// Go to the Live page.
        GoLive,
        /// Go to the Library page.
        GoLibrary,
        /// Go to the Speakers page.
        GoPeople,
        /// Go to the Library page and focus its search field.
        SearchLibrary,
    ]
);

/// The main (workbench) window, for actions that must reach it while it is hidden.
struct MainWindow(AnyWindowHandle);

impl Global for MainWindow {}

/// Every app shortcut. None has a context, so each works on every page, also with focus in a
/// text field; the text field's own bindings (⌘C, ⌘Z, ⌘F, ⌘↩ …) are left alone.
///
/// Returns: the key bindings, in menu order.
pub fn key_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("cmd-,", OpenSettings, None),
        KeyBinding::new("cmd-h", HideApp, None),
        KeyBinding::new("alt-cmd-h", HideOthers, None),
        KeyBinding::new("cmd-q", Quit, None),
        // ⇧⌘ keeps the recording keys away from the single-modifier ones people hit while
        // typing notes, so a stray key never starts or ends a meeting.
        KeyBinding::new("shift-cmd-r", StartRecording, None),
        KeyBinding::new("alt-shift-cmd-r", AdhocRecording, None),
        KeyBinding::new("shift-cmd-s", StopRecording, None),
        KeyBinding::new("shift-cmd-f", ToggleFloatingCaptions, None),
        KeyBinding::new("cmd-1", GoToday, None),
        KeyBinding::new("cmd-2", GoSchedule, None),
        KeyBinding::new("cmd-3", GoLive, None),
        KeyBinding::new("cmd-4", GoLibrary, None),
        KeyBinding::new("cmd-5", GoPeople, None),
        KeyBinding::new("cmd-f", SearchLibrary, None),
        KeyBinding::new("cmd-m", Minimize, None),
        KeyBinding::new("cmd-w", CloseWindow, None),
        KeyBinding::new("cmd-0", ShowMainWindow, None),
    ]
}

/// The menu bar in the current interface language. macOS shows each item's shortcut from
/// [`key_bindings`] (and the text fields' own bindings for the Edit menu).
///
/// Returns: the menus, app menu first.
pub fn menus() -> Vec<Menu> {
    let app = crate::workbench::APP_NAME;
    vec![
        Menu::new(app).items([
            MenuItem::action(tr!("设置…", "Settings…"), OpenSettings),
            MenuItem::separator(),
            MenuItem::os_submenu(tr!("服务", "Services"), SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action(trf!("隐藏 {app}", "Hide {app}"), HideApp),
            MenuItem::action(tr!("隐藏其他", "Hide Others"), HideOthers),
            MenuItem::action(tr!("全部显示", "Show All"), ShowAll),
            MenuItem::separator(),
            MenuItem::action(trf!("退出 {app}", "Quit {app}"), Quit),
        ]),
        Menu::new(tr!("编辑", "Edit")).items([
            MenuItem::os_action(tr!("撤销", "Undo"), input::Undo, OsAction::Undo),
            MenuItem::os_action(tr!("重做", "Redo"), input::Redo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::os_action(tr!("剪切", "Cut"), input::Cut, OsAction::Cut),
            MenuItem::os_action(tr!("拷贝", "Copy"), input::Copy, OsAction::Copy),
            MenuItem::os_action(tr!("粘贴", "Paste"), input::Paste, OsAction::Paste),
            MenuItem::os_action(
                tr!("全选", "Select All"),
                input::SelectAll,
                OsAction::SelectAll,
            ),
        ]),
        Menu::new(tr!("记录", "Recording")).items([
            MenuItem::action(tr!("开始记录", "Start recording"), StartRecording),
            MenuItem::action(tr!("临时记录", "Ad-hoc recording"), AdhocRecording),
            MenuItem::action(tr!("结束记录", "Stop recording"), StopRecording),
            MenuItem::separator(),
            MenuItem::action(
                tr!("显示或隐藏浮窗字幕", "Show or hide floating captions"),
                ToggleFloatingCaptions,
            ),
        ]),
        Menu::new(tr!("前往", "Go")).items([
            MenuItem::action(Page::Today.label(), GoToday),
            MenuItem::action(Page::Schedule.label(), GoSchedule),
            MenuItem::action(Page::Live.label(), GoLive),
            MenuItem::action(Page::Library.label(), GoLibrary),
            MenuItem::action(Page::People.label(), GoPeople),
            MenuItem::separator(),
            MenuItem::action(tr!("搜索纪要", "Search minutes"), SearchLibrary),
        ]),
        Menu::new(tr!("窗口", "Window")).items([
            MenuItem::action(tr!("最小化", "Minimize"), Minimize),
            MenuItem::action(tr!("缩放", "Zoom"), Zoom),
            MenuItem::separator(),
            MenuItem::action(tr!("关闭窗口", "Close window"), CloseWindow),
            MenuItem::separator(),
            MenuItem::action(trf!("{app} 主窗口", "{app} main window"), ShowMainWindow),
        ]),
    ]
}

/// Installs the menu bar in the current language; call again after a language switch.
///
/// Params: `cx` – the app.
pub fn set_menus(cx: &App) {
    cx.set_menus(menus());
}

/// What quitting now would interrupt, if anything.
///
/// Params: `recording` – title of the running recording (empty for an untitled one);
/// `starting` – a recording is starting; `processing` – post-meeting jobs stopping, queued or
/// running.
/// Returns: the warning for the quit dialog, or `None` to quit right away.
pub fn quit_warning(recording: Option<&str>, starting: bool, processing: usize) -> Option<String> {
    let mut lines = Vec::new();
    match recording {
        Some(t) if !t.trim().is_empty() => lines.push(trf!(
            "“{t}”正在记录。现在退出会立即中断录制，也不会做会后整理。",
            "“{t}” is still recording. Quitting now cuts the recording short and skips post-meeting processing."
        )),
        Some(_) => lines.push(
            tr!(
                "正在记录。现在退出会立即中断录制，也不会做会后整理。",
                "A recording is running. Quitting now cuts it short and skips post-meeting processing."
            )
            .to_string(),
        ),
        None if starting => lines.push(
            tr!(
                "正在开始记录。现在退出会中断它。",
                "A recording is starting. Quitting now interrupts it."
            )
            .to_string(),
        ),
        None => {}
    }
    match processing {
        0 => {}
        1 => lines.push(
            tr!(
                "还有 1 场会议在会后整理，退出后它会停在“整理中”。",
                "1 meeting is still being processed. If you quit, it stays at “Processing”."
            )
            .to_string(),
        ),
        n => lines.push(trf!(
            "还有 {n} 场会议在会后整理，退出后它们会停在“整理中”。",
            "{n} meetings are still being processed. If you quit, they stay at “Processing”."
        )),
    }
    (!lines.is_empty()).then(|| lines.join("\n\n"))
}

/// Registers the shortcuts, the menu bar and their handlers.
///
/// Params: `state` – shared app state; `main` – the workbench window; `cx`.
pub fn init(state: Entity<AppState>, main: AnyWindowHandle, cx: &mut App) {
    cx.set_global(MainWindow(main));
    cx.bind_keys(key_bindings());
    set_menus(cx);

    let s = state.clone();
    on_window_action::<Quit>(cx, move |cx| quit(&s, cx));
    on_window_action::<CloseWindow>(cx, close_active_window);
    on_window_action::<Minimize>(cx, |cx| {
        with_active_window(cx, |w| w.minimize_window());
    });
    on_window_action::<Zoom>(cx, |cx| {
        with_active_window(cx, |w| w.zoom_window());
    });
    cx.on_action(|_: &HideApp, cx| cx.hide());
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
    on_window_action::<ShowMainWindow>(cx, |cx| {
        show_main_window(cx);
    });

    for (page, register) in [
        (
            Page::Today,
            register_go::<GoToday> as fn(Entity<AppState>, Page, &mut App),
        ),
        (Page::Schedule, register_go::<GoSchedule>),
        (Page::Live, register_go::<GoLive>),
        (Page::Library, register_go::<GoLibrary>),
        (Page::People, register_go::<GoPeople>),
        (Page::Settings, register_go::<OpenSettings>),
    ] {
        register(state.clone(), page, cx);
    }
    let s = state.clone();
    on_window_action::<SearchLibrary>(cx, move |cx| {
        show_main_window(cx);
        s.update(cx, |a, cx| a.focus_library_search(cx));
    });

    let s = state.clone();
    cx.on_action(move |_: &StartRecording, cx| s.update(cx, |a, cx| a.record_now(cx)));
    let s = state.clone();
    cx.on_action(move |_: &AdhocRecording, cx| {
        s.update(cx, |a, cx| a.quick_record(String::new(), cx))
    });
    let s = state.clone();
    cx.on_action(move |_: &StopRecording, cx| s.update(cx, stop_recording));
    cx.on_action(move |_: &ToggleFloatingCaptions, cx| state.update(cx, toggle_floating));
}

/// Registers a navigation action that shows the main window on `page`.
///
/// Params: `state`; `page`; `cx`.
fn register_go<A: Action>(state: Entity<AppState>, page: Page, cx: &mut App) {
    on_window_action::<A>(cx, move |cx| {
        show_main_window(cx);
        state.update(cx, |a, cx| a.navigate(page, cx));
    });
}

/// Registers an app-level action whose handler touches a window, run after the current
/// dispatch. A shortcut pressed in the main window is dispatched while GPUI has that window
/// checked out, so updating it from the handler fails with "window not found".
///
/// Params: `cx`; `f` – the handler.
fn on_window_action<A: Action>(cx: &mut App, f: impl Fn(&mut App) + 'static) {
    let f = std::rc::Rc::new(f);
    cx.on_action(move |_: &A, cx| {
        let f = f.clone();
        cx.defer(move |cx| f(cx));
    });
}

/// Stops the recording, or explains why there is nothing to stop.
///
/// Params: `app`; `cx`.
fn stop_recording(app: &mut AppState, cx: &mut Context<AppState>) {
    if app.live.is_some() {
        app.stop_recording(cx);
    } else if app.live_starting {
        app.notify(
            NoticeKind::Info,
            tr!(
                "记录还在开始，开始后再结束。",
                "The recording is still starting. Stop it once it has started."
            ),
            cx,
        );
    } else {
        app.notify(
            NoticeKind::Info,
            tr!("当前没有在记录。", "Nothing is recording."),
            cx,
        );
    }
}

/// Shows or hides the floating captions; they exist only while recording.
///
/// Params: `app`; `cx`.
fn toggle_floating(app: &mut AppState, cx: &mut Context<AppState>) {
    if app.floating_open() {
        app.close_floating(cx);
    } else if app.live.is_some() {
        app.open_floating(cx);
    } else {
        app.notify(
            NoticeKind::Info,
            tr!(
                "开始记录后才能打开浮窗字幕。",
                "Floating captions are available while recording."
            ),
            cx,
        );
    }
}

/// Quits, after a confirmation when a recording or post-meeting work would be lost.
///
/// Params: `state`; `cx`.
fn quit(state: &Entity<AppState>, cx: &mut App) {
    let warning = {
        let a = state.read(cx);
        let (running, waiting) = a.post.counts();
        quit_warning(
            a.live.as_ref().map(|l| l.meeting.title.as_str()),
            a.live_starting,
            running + waiting,
        )
    };
    let Some(warning) = warning else {
        cx.quit();
        return;
    };
    let Some(main) = show_main_window(cx) else {
        // No window to ask in; quitting silently would be worse than not quitting.
        tracing::warn!("quit: main window is gone, can't confirm; quitting");
        cx.quit();
        return;
    };
    let _ = main.update(cx, |_, window, cx| {
        if window.has_active_dialog(cx) {
            // A second ⌘Q while the question is open means "yes, quit".
            cx.quit();
            return;
        }
        window.open_alert_dialog(cx, move |alert, _, _| {
            alert
                .confirm()
                .title(tr!("要退出吗？", "Quit now?"))
                .description(warning.clone())
                .ok_text(tr!("退出", "Quit"))
                .ok_variant(ButtonVariant::Danger)
                .cancel_text(tr!("取消", "Cancel"))
                .on_ok(|_, _, cx| {
                    cx.quit();
                    true
                })
        });
    });
}

/// The main window handle, if [`init`] ran.
///
/// Params: `cx`.
/// Returns: the handle.
fn main_window(cx: &App) -> Option<AnyWindowHandle> {
    cx.try_global::<MainWindow>().map(|m| m.0)
}

/// Brings the main window back (it may be hidden) and activates the app.
///
/// Params: `cx`.
/// Returns: the main window handle, or `None` if it no longer exists.
pub fn show_main_window(cx: &mut App) -> Option<AnyWindowHandle> {
    let main = main_window(cx)?;
    if let Err(e) = main.update(cx, |_, window, _| window.activate_window()) {
        tracing::warn!("couldn't show the main window: {e}");
        return None;
    }
    cx.activate(true);
    Some(main)
}

/// The window a window command acts on: the key window, else the main window. The app can be
/// frontmost with no key window (after activation from the Dock or another app's floating
/// panel took key status), and ⌘W must still close the window the user sees.
///
/// Params: `cx`.
/// Returns: the target window, if any.
fn target_window(cx: &App) -> Option<AnyWindowHandle> {
    cx.active_window().or_else(|| main_window(cx))
}

/// Runs `f` on [`target_window`], if any.
///
/// Params: `cx`; `f` – what to do with the window.
fn with_active_window(cx: &mut App, f: impl FnOnce(&mut Window)) {
    if let Some(w) = target_window(cx)
        && let Err(e) = w.update(cx, |_, window, _| f(window))
    {
        tracing::warn!("window command: window {w:?} is gone: {e}");
    }
}

/// ⌘W: hides the main window; other windows (none can be key today) are closed.
///
/// Params: `cx`.
fn close_active_window(cx: &mut App) {
    let Some(target) = target_window(cx) else {
        return;
    };
    let is_main = main_window(cx) == Some(target);
    if let Err(e) = target.update(cx, |_, window, _| {
        if is_main {
            hide_window(window);
        } else {
            window.remove_window();
        }
    }) {
        tracing::warn!("close window: window {target:?} is gone: {e}");
    }
}

/// Hides `window` without destroying it, so its views (and an unsaved notes buffer) survive.
/// The red traffic light uses this too, see [`hide_on_close`].
///
/// Params: `window`.
pub fn hide_window(window: &mut Window) {
    #[cfg(target_os = "macos")]
    if let Err(e) = native::order_out(window) {
        // Minimizing still gets the window out of the way.
        tracing::warn!("couldn't hide the main window, minimizing instead: {e}");
        window.minimize_window();
    }
    #[cfg(not(target_os = "macos"))]
    window.minimize_window();
}

/// Makes the window's close button hide it instead of closing it.
///
/// Params: `window`; `cx`.
pub fn hide_on_close(window: &mut Window, cx: &App) {
    window.on_window_should_close(cx, |window, _| {
        hide_window(window);
        false
    });
}

/// AppKit calls GPUI does not expose.
#[cfg(target_os = "macos")]
mod native {
    use gpui_kit::Window;
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    /// Removes the window from the screen (`orderOut:`) while keeping it alive.
    ///
    /// Params: `window` – a GPUI window on macOS.
    /// Returns: an error when the native window can't be reached.
    pub(super) fn order_out(window: &Window) -> anyhow::Result<()> {
        // `Window` has an inherent `window_handle()` (the GPUI handle), so name the trait.
        let handle = HasWindowHandle::window_handle(window)
            .map_err(|e| anyhow::anyhow!("no native handle for the window: {e}"))?;
        let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
            anyhow::bail!("window handle is not AppKit: {:?}", handle.as_raw());
        };
        let view: *mut AnyObject = appkit.ns_view.as_ptr().cast();
        // SAFETY: called on the main thread (GPUI actions and window callbacks run there)
        // with the live content view; `window` and `orderOut:` exist on NSView / NSWindow.
        unsafe {
            let ns_window: *mut AnyObject = msg_send![view, window];
            if ns_window.is_null() {
                anyhow::bail!("view {view:p} is not in a window");
            }
            let nil: *mut AnyObject = std::ptr::null_mut();
            let _: () = msg_send![ns_window, orderOut: nil];
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: that would pull in GPUI's `test` macro and shadow `#[test]`.
    use super::{
        CloseWindow, Quit, StartRecording, StopRecording, key_bindings, menus, quit_warning,
    };
    use gpui_kit::{Action as _, KeyBinding, MenuItem};
    use mm_i18n::{Lang, with_lang};
    use std::collections::HashSet;

    /// Keystrokes of a binding, for comparing bindings.
    fn keys(b: &KeyBinding) -> String {
        b.keystrokes()
            .iter()
            .map(|k| format!("{k:?}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Names of every action reachable from a menu, submenus included.
    fn menu_actions(items: &[MenuItem], out: &mut HashSet<&'static str>) {
        for item in items {
            match item {
                MenuItem::Action { action, .. } => {
                    out.insert(action.name());
                }
                MenuItem::Submenu(m) => menu_actions(&m.items, out),
                _ => {}
            }
        }
    }

    #[test]
    fn no_two_shortcuts_share_keys() {
        let mut seen = HashSet::new();
        for b in key_bindings() {
            assert!(seen.insert(keys(&b)), "duplicate shortcut {}", keys(&b));
        }
    }

    #[test]
    fn every_shortcut_is_in_the_menu_bar() {
        let mut in_menus = HashSet::new();
        for m in menus() {
            menu_actions(&m.items, &mut in_menus);
        }
        for b in key_bindings() {
            assert!(
                in_menus.contains(b.action().name()),
                "{} has a shortcut but no menu item",
                b.action().name()
            );
        }
    }

    #[test]
    fn core_shortcuts_are_bound() {
        let bound: HashSet<_> = key_bindings().iter().map(|b| b.action().name()).collect();
        for name in [
            Quit.name(),
            CloseWindow.name(),
            StartRecording.name(),
            StopRecording.name(),
        ] {
            assert!(bound.contains(name), "{name} is not bound");
        }
    }

    #[test]
    fn menus_follow_the_interface_language() {
        let zh = with_lang(Lang::Zh, menus);
        let en = with_lang(Lang::En, menus);
        assert_eq!(zh[2].name.as_ref(), "记录");
        assert_eq!(en[2].name.as_ref(), "Recording");
        assert_eq!(zh.len(), en.len());
    }

    #[test]
    fn quitting_idle_needs_no_confirmation() {
        assert_eq!(quit_warning(None, false, 0), None);
    }

    #[test]
    fn quitting_while_recording_names_the_meeting() {
        let w = with_lang(Lang::Zh, || quit_warning(Some("周会"), false, 0)).unwrap();
        assert!(w.contains("“周会”正在记录"), "{w}");
        let w = with_lang(Lang::En, || quit_warning(Some(" "), false, 0)).unwrap();
        assert!(w.starts_with("A recording is running."), "{w}");
    }

    #[test]
    fn quitting_while_starting_or_processing_warns() {
        let w = with_lang(Lang::Zh, || quit_warning(None, true, 0)).unwrap();
        assert!(w.contains("正在开始记录"), "{w}");
        let w = with_lang(Lang::En, || quit_warning(Some("Standup"), false, 2)).unwrap();
        assert!(w.contains("“Standup” is still recording"), "{w}");
        assert!(w.contains("2 meetings are still being processed"), "{w}");
    }
}
