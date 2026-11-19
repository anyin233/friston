//! Meeting Master desktop entry point.

mod assign;
#[cfg(test)]
mod e2e;
mod floating;
#[cfg(test)]
mod gui_tests;
mod logic;
mod pages;
mod pipeline;
mod post_queue;
mod shortcuts;
mod state;
mod status;
mod ui;
mod workbench;
mod writer;

use gpui_kit::component::TitleBar;
use gpui_kit::*;

use crate::state::AppState;
use crate::workbench::{TRAFFIC_LIGHT_POSITION, Workbench};

/// Starts the GPUI application and opens the workbench window.
fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    // Startup notices (data-folder moves, database errors) are created before AppState reads
    // config.json, so apply the saved language first. An unreadable or not-yet-migrated config
    // falls back to the system language; AppState applies the loaded choice again.
    let lang = mm_service::AppConfig::load(&mm_service::data_dir())
        .map(|c| c.effective_ui_language())
        .unwrap_or_else(|_| mm_i18n::system_lang());
    crate::state::apply_ui_language(lang);

    // URLs (friston://open?…, or meeting-master:// from older notes) arrive from macOS before or after launch; queue them
    // until the state exists. Command-line arguments with the scheme are treated the same,
    // which also makes the handler testable without registering the scheme.
    let (url_tx, url_rx) = async_channel::unbounded::<String>();
    for arg in std::env::args().skip(1) {
        if mm_service::flow::is_app_url(&arg) {
            let _ = url_tx.send_blocking(arg);
        }
    }
    let app = gpui_kit::application().with_assets(gpui_kit::assets::Assets);
    app.on_open_urls(move |urls| {
        for u in urls {
            let _ = url_tx.send_blocking(u);
        }
    });
    // Clicking the Dock icon brings back the main window after ⌘W hid it.
    app.on_reopen(|cx| {
        shortcuts::show_main_window(cx);
    });
    app.run(move |cx| {
        gpui_kit::init(cx);
        let state = cx.new(AppState::new);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1440.), px(900.)), cx)),
            window_min_size: Some(size(px(1100.), px(700.))),
            // No title bar: the content fills the window and the traffic lights sit above the
            // sidebar logo. Keep `app_owns_titlebar_drag` so the sidebar strip moves the window.
            titlebar: Some(TitlebarOptions {
                title: None,
                appears_transparent: true,
                traffic_light_position: Some(TRAFFIC_LIGHT_POSITION),
            }),
            ..TitleBar::window_options()
        };
        let s = state.clone();
        let (main_window, _) = gpui_kit::open_window(options, cx, move |window, cx| {
            cx.new(|cx| Workbench::new(s, window, cx))
        })
        .expect("failed to open the Friston window");
        shortcuts::init(state.clone(), main_window, cx);
        cx.spawn(async move |cx| {
            while let Ok(url) = url_rx.recv().await {
                state.update(cx, |s, cx| s.open_url(&url, cx));
            }
        })
        .detach();
        cx.activate(true);
    });
}
