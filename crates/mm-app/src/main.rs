//! Meeting Master desktop entry point.

mod assign;
mod floating;
mod logic;
mod pages;
mod pipeline;
mod state;
mod ui;
mod workbench;
mod writer;

use gpui_kit::component::TitleBar;
use gpui_kit::*;

use crate::state::AppState;
use crate::workbench::Workbench;

/// Starts the GPUI application and opens the workbench window.
fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    // URLs (meeting-master://open?…) arrive from macOS before or after launch; queue them
    // until the state exists. Command-line arguments with the scheme are treated the same,
    // which also makes the handler testable without registering the scheme.
    let (url_tx, url_rx) = async_channel::unbounded::<String>();
    for arg in std::env::args().skip(1) {
        if arg.starts_with(&format!("{}://", mm_service::flow::URL_SCHEME)) {
            let _ = url_tx.send_blocking(arg);
        }
    }
    let app = gpui_kit::application().with_assets(gpui_kit::assets::Assets);
    app.on_open_urls(move |urls| {
        for u in urls {
            let _ = url_tx.send_blocking(u);
        }
    });
    app.run(move |cx| {
        gpui_kit::init(cx);
        let state = cx.new(AppState::new);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1440.), px(900.)), cx)),
            window_min_size: Some(size(px(1100.), px(700.))),
            ..TitleBar::window_options()
        };
        let s = state.clone();
        gpui_kit::open_window(options, cx, move |window, cx| {
            cx.new(|cx| Workbench::new(s, window, cx))
        })
        .expect("failed to open the Meeting Master window");
        cx.spawn(async move |cx| {
            while let Ok(url) = url_rx.recv().await {
                state.update(cx, |s, cx| s.open_url(&url, cx));
            }
        })
        .detach();
        cx.activate(true);
    });
}
