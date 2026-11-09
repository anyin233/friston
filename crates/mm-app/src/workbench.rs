//! Root view: sidebar navigation (with the window controls on top) and the active page.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::hover_card::HoverCard;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::sidebar::{
    Sidebar, SidebarGroup, SidebarHeader, SidebarMenu, SidebarMenuItem,
};
use gpui_kit::component::{
    ActiveTheme as _, IconName, InteractiveElementExt as _, Sizable as _, WindowExt as _, h_flex,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::logic;
use crate::pages::Page;
use crate::pages::library::LibraryPage;
use crate::pages::live::LivePage;
use crate::pages::people::PeoplePage;
use crate::pages::schedule::SchedulePage;
use crate::pages::settings::SettingsPage;
use crate::pages::today::TodayPage;
use crate::pipeline;
use crate::state::{AppEvent, AppState, Loadable, NoticeKind};
use crate::status::{self, Light, MinutesInput, StatusInput, StatusRow};
use mm_i18n::{tr, trf};
use mm_llm::MinutesBackend;
use mm_service::RefineBackend;

/// Product name shown in the sidebar header.
pub const APP_NAME: &str = "Friston";

/// The app icon rasterised by `scripts/make-icon.sh`, so the sidebar logo matches the Dock icon.
const LOGO_PNG: &[u8] = include_bytes!("../assets/friston-logo-64.png");

/// Builds the full-colour sidebar logo from the embedded PNG.
///
/// Returns: an image element sized for the sidebar header.
fn logo() -> impl IntoElement {
    // `svg()` tints every path one colour and `img()` drew this SVG blank, so use a PNG.
    img(std::sync::Arc::new(Image::from_bytes(
        ImageFormat::Png,
        LOGO_PNG.to_vec(),
    )))
    .size_5()
}

/// Shows notices raised before the window existed (migration, unreadable settings, …).
///
/// Params: `notices` – kind and message pairs; `window`/`cx` – the view being built.
/// Returns: nothing; the toasts appear on the window.
fn show_startup_notices<V: 'static>(
    notices: Vec<(NoticeKind, String)>,
    window: &mut Window,
    cx: &mut Context<V>,
) {
    if notices.is_empty() {
        return;
    }
    // Called from the view's constructor, inside `gpui_kit::open_window`'s build closure. The
    // window's Root, which owns the notification layer, is created only after that closure
    // returns, so pushing now panics ("component window state is missing"). On macOS the panic
    // unwinds through the launch Apple Event and aborts the app. Defer past the build instead.
    cx.defer_in(window, move |_, window, cx| {
        for (kind, msg) in notices {
            window.push_notification(toast(kind, msg), cx);
        }
    });
}

/// Where macOS draws the traffic lights. There is no title bar, so they sit in the sidebar's
/// top strip, left-aligned with the logo below them (sidebar padding 12px + header padding 8px).
pub const TRAFFIC_LIGHT_POSITION: Point<Pixels> = Point {
    x: px(20.),
    y: px(18.),
};

/// Height of the draggable strip above the logo that holds the traffic lights.
const WINDOW_CONTROLS_HEIGHT: Pixels = px(28.);

/// Top-level workbench state.
pub struct Workbench {
    app: Entity<AppState>,
    /// Set on mouse-down in the drag strip; the next mouse move starts a native window drag.
    drag_armed: bool,
    page: Page,
    today: Entity<TodayPage>,
    schedule: Entity<SchedulePage>,
    live: Entity<LivePage>,
    library: Entity<LibraryPage>,
    people: Entity<PeoplePage>,
    settings: Entity<SettingsPage>,
}

/// Builds a toast for a notice.
///
/// Params: `kind`; `msg`.
/// Returns: the notification.
fn toast(kind: NoticeKind, msg: String) -> Notification {
    match kind {
        NoticeKind::Info => Notification::info(msg),
        NoticeKind::Success => Notification::success(msg),
        NoticeKind::Warning => Notification::warning(msg),
        NoticeKind::Error => Notification::error(msg).autohide(false),
    }
}

impl Workbench {
    /// Creates the workbench and its pages on the Today page.
    ///
    /// Params: `app` – shared state; `window`; `cx`.
    /// Returns: the root view state.
    pub fn new(app: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let today = cx.new(|cx| TodayPage::new(app.clone(), cx));
        let schedule = cx.new(|cx| SchedulePage::new(app.clone(), cx));
        let live = cx.new(|cx| LivePage::new(app.clone(), window, cx));
        let library = cx.new(|cx| LibraryPage::new(app.clone(), window, cx));
        let people = cx.new(|cx| PeoplePage::new(app.clone(), window, cx));
        let settings = cx.new(|cx| SettingsPage::new(app.clone(), window, cx));
        cx.observe(&app, |_, _, cx| cx.notify()).detach();
        cx.subscribe_in(
            &app,
            window,
            |this, _, ev: &AppEvent, window, cx| match ev {
                AppEvent::Notify(kind, msg) => {
                    window.push_notification(toast(*kind, msg.clone()), cx)
                }
                AppEvent::Navigate(page) => {
                    this.page = *page;
                    cx.notify();
                }
                AppEvent::OpenMeeting(id, t) => {
                    this.page = Page::Library;
                    this.library.update(cx, |l, cx| l.open(*id, *t, cx));
                    cx.notify();
                }
                AppEvent::MeetingChanged(_) | AppEvent::LiveStarted { .. } => {}
            },
        )
        .detach();
        // The sidebar offers a calendar meeting from 10 minutes before it starts until it
        // ends; nothing else re-renders an idle window, so tick twice a minute.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(30))
                    .await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        })
        .detach();
        let pending = app.update(cx, |a, _| a.take_startup_notices());
        show_startup_notices(pending, window, cx);
        Self {
            app,
            drag_armed: false,
            page: Page::Today,
            today,
            schedule,
            live,
            library,
            people,
            settings,
        }
    }

    /// Builds the navigation sidebar with status lines.
    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let recording = self.app.read(cx).live.is_some();
        let items = Page::ALL.map(|page| {
            let label = if page == Page::Live && recording {
                tr!("会议中 ●", "In meeting ●").to_string()
            } else {
                page.label().to_string()
            };
            SidebarMenuItem::new(label)
                .icon(page.icon())
                .active(self.page == page)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.page = page;
                    cx.notify();
                }))
        });
        Sidebar::new("main-sidebar")
            .header(
                v_flex()
                    .w_full()
                    .gap_2()
                    .child(self.window_controls(cx))
                    .child(
                        SidebarHeader::new().child(
                            h_flex()
                                .gap_2()
                                .child(logo())
                                .child(div().font_weight(FontWeight::SEMIBOLD).child(APP_NAME)),
                        ),
                    )
                    .child(self.record_control(cx)),
            )
            .child(
                SidebarGroup::new(tr!("工作台", "Workspace"))
                    .child(SidebarMenu::new().children(items)),
            )
            // Plain wrapper, not `SidebarFooter`: that one highlights the whole footer width on
            // hover, while only the lights themselves should get a hover background.
            .footer(div().p_2().child(self.status_lights(cx)))
    }

    /// Current status rows of the vault and the models loaded at launch.
    ///
    /// Params: `cx` – the workbench context.
    /// Returns: rows in display order.
    fn status_rows(&self, cx: &App) -> Vec<StatusRow> {
        let app = self.app.read(cx);
        let vault = app.cfg.vault_path.as_deref();
        status::rows(&StatusInput {
            vault,
            // One stat per render; a vault on an unplugged drive must turn the light red.
            vault_exists: vault.is_some_and(|v| v.is_dir()),
            meeting_state: &app.meeting_state,
            meeting_models: &app.meeting_models,
            speech: &app.speech_models,
            embed: &app.embed_status,
            embed_dir: app.cfg.embedding_model_dir.as_deref(),
            minutes: MinutesInput {
                backend: app.cfg.minutes_backend,
                antigravity_model: &app.cfg.antigravity_minutes_model,
                // A stat of a few PATH folders per render, like `vault_exists` above.
                agy_found: app.cfg.minutes_backend == MinutesBackend::Antigravity
                    && mm_llm::antigravity::find_agy().is_some(),
                // Same condition as the post-meeting pipeline (for a one-part recording).
                merged_with_refine: app.cfg.refine_after_meeting
                    && app.cfg.refine_backend == RefineBackend::Antigravity
                    && pipeline::merges_minutes_into_refine(&app.cfg, 1),
                byok: app.cfg.byok.as_ref().filter(|_| app.byok_key_present),
                use_codex: app.cfg.use_codex,
                codex: &app.codex,
                codex_model: app.cfg.codex_model.as_deref(),
                llm: &app.llm_status,
                llm_dir: app.cfg.local_llm_dir.as_deref(),
            },
        })
    }

    /// A bare row of status lights in the sidebar footer; names and details appear only in
    /// the hover card, so the footer stays one dot tall.
    ///
    /// Params: `cx` – the workbench context.
    /// Returns: the hover card element.
    fn status_lights(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.status_rows(cx);
        let hover_bg = cx.theme().tokens.sidebar_accent;
        // The hover background is a rounded rectangle hugging the dots; its padding also
        // enlarges the hover target, since 8px dots alone are hard to land on.
        let trigger = h_flex()
            .id("status-lights")
            .gap_1p5()
            .px_1p5()
            .py_1()
            .rounded(px(6.))
            .hover(move |this| this.bg(hover_bg))
            .children(
                rows.iter()
                    .enumerate()
                    .map(|(i, r)| light_dot(r.light, ("status-light", i), cx)),
            );
        HoverCard::new("status-card")
            .anchor(Anchor::BottomLeft)
            .open_delay(std::time::Duration::from_millis(150))
            .trigger(trigger)
            .content(move |_, _, cx| status_card(&rows, cx))
    }

    /// The active page view.
    fn page_body(&self) -> AnyView {
        match self.page {
            Page::Today => self.today.clone().into(),
            Page::Schedule => self.schedule.clone().into(),
            Page::Live => self.live.clone().into(),
            Page::Library => self.library.clone().into(),
            Page::People => self.people.clone().into(),
            Page::Settings => self.settings.clone().into(),
        }
    }

    /// Empty strip above the logo where macOS draws the traffic lights.
    ///
    /// With the title bar gone this is the window's drag handle. The window is opened with
    /// `app_owns_titlebar_drag`, so AppKit does not move it on its own: arm on mouse-down and
    /// start a native move on the first drag, as gpui-component's `TitleBar` does.
    ///
    /// Params: `cx` – the workbench context.
    /// Returns: the strip element.
    fn window_controls(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("window-controls")
            .w_full()
            .h(WINDOW_CONTROLS_HEIGHT)
            .flex_shrink_0()
            .window_control_area(WindowControlArea::Drag)
            .on_double_click(|_, window, _| window.titlebar_double_click())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.drag_armed = true),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.drag_armed = false),
            )
            .on_mouse_down_out(cx.listener(|this, _, _, _| this.drag_armed = false))
            .on_mouse_move(cx.listener(|this, _, window, _| {
                if this.drag_armed {
                    this.drag_armed = false;
                    window.start_window_move();
                }
            }))
    }

    /// The app's only start-recording control, under the logo, plus the running timer.
    ///
    /// Idle with a calendar meeting running or about to start (see
    /// [`logic::recordable_event`]), it offers two buttons: record that meeting (title and
    /// attendees from the calendar) or an ad-hoc recording. Otherwise one button starts an
    /// ad-hoc recording. Start buttons use the theme's primary colours (filled background,
    /// matching foreground); stop stays red.
    ///
    /// Params: `cx` – the workbench context.
    /// Returns: the control element.
    fn record_control(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let app = self.app.read(cx);
        let elapsed = app.live.as_ref().map(|l| l.elapsed_ms());
        let busy = app.live_starting;
        let now = chrono::Utc::now();
        let event = match (&app.events, elapsed, busy) {
            (Loadable::Ready(evs), None, false) => logic::recordable_event(evs, now),
            _ => None,
        };
        let adhoc = {
            let handle = self.app.clone();
            move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                handle.update(cx, |a, cx| a.quick_record(String::new(), cx))
            }
        };
        let mut col = v_flex().w_full().gap_1();
        col = match (elapsed, event) {
            (Some(_), _) => {
                let handle = self.app.clone();
                col.child(
                    Button::new("quick-record")
                        .small()
                        .danger()
                        .w_full()
                        .icon(IconName::Square)
                        .label(tr!("结束记录", "Stop recording"))
                        .on_click(move |_, _, cx| handle.update(cx, |a, cx| a.stop_recording(cx))),
                )
            }
            (None, Some(ev)) => {
                let handle = self.app.clone();
                let when = logic::relative_start(now, ev.start, ev.end);
                let rec = ev.clone();
                col.child(
                    Button::new("record-event")
                        .small()
                        .primary()
                        .w_full()
                        .icon(IconName::Calendar)
                        .label(tr!("记录当前会议", "Record this meeting"))
                        .on_click(move |_, _, cx| {
                            handle.update(cx, |a, cx| a.record_event(rec.clone(), cx))
                        }),
                )
                .child(
                    div()
                        .w_full()
                        .px_1()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .truncate()
                        .child(format!("{} · {when}", ev.title)),
                )
                .child(
                    Button::new("quick-record")
                        .small()
                        .outline()
                        .w_full()
                        .icon(IconName::Mic)
                        .label(tr!("临时记录", "Ad-hoc recording"))
                        .on_click(adhoc),
                )
            }
            (None, None) => col.child(
                Button::new("quick-record")
                    .small()
                    .primary()
                    .w_full()
                    .icon(IconName::Mic)
                    .label(if busy {
                        tr!("正在开始…", "Starting…")
                    } else {
                        tr!("开始记录", "Start recording")
                    })
                    .loading(busy)
                    .on_click(adhoc),
            ),
        };
        col.when_some(elapsed, |el, ms| {
            el.child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(cx.theme().danger)
                    .child(trf!("● 录制中 {}", "● Recording {}", logic::clock(ms))),
            )
        })
    }
}

/// Colour of a status light: red = not loaded, yellow = loading, green = loaded.
///
/// Params: `light`; `cx`.
/// Returns: the theme colour.
fn light_color(light: Light, cx: &App) -> Hsla {
    match light {
        Light::Red => cx.theme().danger,
        Light::Yellow => cx.theme().warning,
        Light::Green => cx.theme().success,
    }
}

/// A round status light. A yellow (loading) light blinks so it reads as "in progress"
/// rather than as a warning.
///
/// Params: `light`; `id` – animation id, unique among its siblings; `cx`.
/// Returns: the dot element.
fn light_dot(light: Light, id: impl Into<ElementId>, cx: &App) -> AnyElement {
    let dot = div()
        .size(px(8.))
        .flex_shrink_0()
        .rounded_full()
        .bg(light_color(light, cx));
    if light != Light::Yellow {
        return dot.into_any_element();
    }
    dot.with_animation(
        id,
        Animation::new(std::time::Duration::from_millis(1200))
            .repeat()
            .with_easing(bounce(ease_in_out)),
        // Never fully off, so the row keeps its shape while blinking.
        |dot, delta| dot.opacity(1.0 - delta * 0.75),
    )
    .into_any_element()
}

/// Hover card body: one block per resource with its light, name and details.
///
/// Params: `rows`; `cx`.
/// Returns: the card content.
fn status_card(rows: &[StatusRow], cx: &App) -> Div {
    let muted = cx.theme().muted_foreground;
    v_flex()
        .w(px(300.))
        .gap_3()
        .children(rows.iter().enumerate().map(|(i, r)| {
            v_flex()
                .gap_0p5()
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(light_dot(r.light, ("status-card-light", i), cx))
                        .child(div().text_xs().text_color(muted).child(r.title)),
                )
                .child(
                    div()
                        .pl(px(16.))
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .child(r.value.clone()),
                )
                .children(r.details.iter().map(|d| {
                    div()
                        .pl(px(16.))
                        .text_xs()
                        .text_color(muted)
                        .child(d.clone())
                }))
        }))
}

impl Render for Workbench {
    /// Renders sidebar + page; the window has no title bar.
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().size_full().bg(cx.theme().background).child(
            h_flex()
                .flex_1()
                .min_h_0()
                .child(self.sidebar(cx))
                .child(div().flex_1().min_w_0().h_full().child(self.page_body())),
        )
    }
}
