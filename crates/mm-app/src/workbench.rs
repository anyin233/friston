//! Root view: title bar, sidebar navigation and the active page.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::sidebar::{
    Sidebar, SidebarGroup, SidebarHeader, SidebarMenu, SidebarMenuItem,
};
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, TitleBar, h_flex, v_flex,
};
use gpui_kit::*;

use crate::pages::Page;

/// Top-level workbench state.
pub struct Workbench {
    page: Page,
}

impl Workbench {
    /// Creates the workbench on the Today page.
    ///
    /// Params: `_window`, `_cx` – GPUI handles (unused until engines are wired in).
    /// Returns: the root view state.
    pub fn new(_window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self { page: Page::Today }
    }

    /// Builds the navigation sidebar.
    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let items = Page::ALL.map(|page| {
            SidebarMenuItem::new(page.label())
                .icon(page.icon())
                .active(self.page == page)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.page = page;
                    cx.notify();
                }))
        });
        Sidebar::new("main-sidebar")
            .header(
                SidebarHeader::new().child(
                    h_flex()
                        .gap_2()
                        .child(Icon::new(IconName::Mic).small())
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("Meeting Master"),
                        ),
                ),
            )
            .child(SidebarGroup::new("Workspace").child(SidebarMenu::new().children(items)))
    }

    /// Placeholder body for pages that are designed but not implemented yet.
    fn page_body(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .flex_1()
            .p_6()
            .gap_2()
            .child(
                div()
                    .text_xl()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(self.page.label()),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(self.page.summary()),
            )
    }
}

impl Render for Workbench {
    /// Renders title bar + sidebar + page.
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .child(
                TitleBar::new().child(
                    h_flex()
                        .w_full()
                        .pr_3()
                        .justify_between()
                        .child("Meeting Master")
                        .child(
                            Button::new("quick-record")
                                .small()
                                .danger()
                                .icon(IconName::Mic)
                                .label("开始记录"),
                        ),
                ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.sidebar(cx))
                    .child(self.page_body(cx)),
            )
    }
}
