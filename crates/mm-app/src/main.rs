//! Meeting Master desktop entry point.

mod pages;
mod workbench;

use gpui_kit::component::TitleBar;
use gpui_kit::*;

use crate::workbench::Workbench;

/// Starts the GPUI application and opens the workbench window.
fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(|cx| {
            gpui_kit::init(cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::centered(size(px(1440.), px(900.)), cx)),
                window_min_size: Some(size(px(1100.), px(700.))),
                ..TitleBar::window_options()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| Workbench::new(window, cx))
            })
            .expect("failed to open the Meeting Master window");
            cx.activate(true);
        });
}
