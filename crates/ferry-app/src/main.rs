//! Ferry desktop application.

use gpui::{
    App, AppContext as _, Bounds, Context, Decorations, IntoElement, ParentElement as _, Render,
    Styled as _, TitlebarOptions, Window, WindowBounds, WindowDecorations, WindowOptions, div,
    prelude::FluentBuilder as _, px, size,
};
use gpui_component::{ActiveTheme as _, Root, TitleBar, v_flex};
use tracing_subscriber::EnvFilter;

/// Wayland `app_id`, also used by desktop files and compositor window rules.
const APP_ID: &str = "dev.ferry.Ferry";
const APP_NAME: &str = "Ferry";

struct MainWindow;

impl Render for MainWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Server-side decorations are requested; only when the compositor refuses them
        // (no xdg-decoration, e.g. GNOME) do we draw a minimal title bar ourselves.
        let client_decorated = matches!(window.window_decorations(), Decorations::Client { .. });
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .when(client_decorated, |this| {
                this.child(TitleBar::new().child(APP_NAME))
            })
            .child(
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(APP_NAME),
            )
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    gpui_platform::application()
        .with_assets(gpui_component_assets::Assets)
        .run(|cx: &mut App| {
            gpui_component::init(cx);
            if let Err(error) = open_main_window(cx) {
                tracing::error!("failed to open the main window: {error:#}");
                cx.quit();
            }
        });
}

fn open_main_window(cx: &mut App) -> anyhow::Result<()> {
    let bounds = Bounds::centered(None, size(px(1100.), px(720.)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some(APP_NAME.into()),
            ..TitlebarOptions::default()
        }),
        app_id: Some(APP_ID.to_owned()),
        window_decorations: Some(WindowDecorations::Server),
        ..WindowOptions::default()
    };
    cx.open_window(options, |window, cx| {
        tracing::info!(decorations = ?window.window_decorations(), "main window opened");
        let view = cx.new(|_| MainWindow);
        cx.new(|cx| Root::new(view, window, cx))
    })?;
    // Linux has no notion of activating an application; the compositor focuses new windows.
    #[cfg(target_os = "macos")]
    cx.activate(true);
    Ok(())
}
