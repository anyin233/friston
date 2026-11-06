//! Root view: title bar, sidebar navigation and the active page.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::sidebar::{
    Sidebar, SidebarFooter, SidebarGroup, SidebarHeader, SidebarMenu, SidebarMenuItem,
};
use gpui_kit::component::{
    ActiveTheme as _, IconName, Sizable as _, TitleBar, WindowExt as _, h_flex, v_flex,
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
use crate::state::{AppEvent, AppState, EngineState, NoticeKind};

/// Product name shown in the title bar and sidebar.
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

/// Top-level workbench state.
pub struct Workbench {
    app: Entity<AppState>,
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
        let pending = app.update(cx, |a, _| a.take_startup_notices());
        for (kind, msg) in pending {
            window.push_notification(toast(kind, msg), cx);
        }
        Self {
            app,
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
                "会议中 ●".to_string()
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
        let app = self.app.read(cx);
        let vault = match &app.cfg.vault_path {
            Some(v) => format!(
                "Obsidian：{}",
                v.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default()
            ),
            None => "Obsidian：未设置".to_string(),
        };
        let engine = match app.meeting_state {
            EngineState::Ready => "字幕引擎就绪".to_string(),
            ref s => s.describe("字幕引擎"),
        };
        let embed = if app.embedding_ready() {
            "嵌入模型已加载"
        } else {
            "嵌入模型未加载"
        };
        let muted = cx.theme().muted_foreground;
        Sidebar::new("main-sidebar")
            .header(
                SidebarHeader::new().child(
                    h_flex()
                        .gap_2()
                        .child(logo())
                        .child(div().font_weight(FontWeight::SEMIBOLD).child(APP_NAME)),
                ),
            )
            .child(SidebarGroup::new("工作台").child(SidebarMenu::new().children(items)))
            .footer(
                SidebarFooter::new().child(
                    v_flex()
                        .gap_1()
                        .text_xs()
                        .text_color(muted)
                        .child(div().truncate().child(vault))
                        .child(div().truncate().child(engine))
                        .child(div().child(embed)),
                ),
            )
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

    /// Title bar: recording timer and the start/stop button.
    fn title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let app = self.app.read(cx);
        let elapsed = app.live.as_ref().map(|l| l.elapsed_ms());
        let busy = app.live_starting;
        let handle = self.app.clone();
        let button = match elapsed {
            Some(_) => Button::new("quick-record")
                .small()
                .danger()
                .icon(IconName::Square)
                .label("结束记录")
                .on_click(move |_, _, cx| handle.update(cx, |a, cx| a.stop_recording(cx))),
            None => Button::new("quick-record")
                .small()
                .danger()
                .icon(IconName::Mic)
                .label(if busy {
                    "正在开始…"
                } else {
                    "开始记录"
                })
                .loading(busy)
                .on_click(move |_, _, cx| {
                    handle.update(cx, |a, cx| a.quick_record(String::new(), cx))
                }),
        };
        TitleBar::new().child(
            h_flex()
                .w_full()
                .pr_3()
                .justify_between()
                .child(APP_NAME)
                .child(
                    h_flex()
                        .gap_3()
                        .items_center()
                        .when_some(elapsed, |el, ms| {
                            el.child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(cx.theme().danger)
                                    .child(format!("● 录制中 {}", logic::clock(ms))),
                            )
                        })
                        .child(button),
                ),
        )
    }
}

impl Render for Workbench {
    /// Renders title bar + sidebar + page.
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .child(self.title_bar(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.sidebar(cx))
                    .child(div().flex_1().min_w_0().h_full().child(self.page_body())),
            )
    }
}
