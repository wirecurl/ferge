//! ferge desktop application.

mod actions;
mod appearance;
mod assets;
mod bench;
mod browser;
mod editor;
mod file_type;
mod format;
mod inspector;
mod interaction;
mod logging;
mod palette;
mod preview;
mod prompts;
mod reconnect;
mod runtime;
mod server_form;
mod session;
mod settings_window;
mod style;
mod transfers;
mod vault;
mod workspace;

use std::alloc::System;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use cap::Cap;
use ferge_core::document::worker;

use clap::Parser;
use gpui::{
    App, AppContext as _, Bounds, Entity, TitlebarOptions, WindowBounds, WindowDecorations,
    WindowHandle, WindowOptions, px, size,
};
use gpui_component::Root;

use crate::workspace::Workspace;

/// Wayland `app_id`, also used by desktop files and compositor window rules.
pub(crate) const APP_ID: &str = "com.wirecurl.ferge";
const APP_NAME: &str = "ferge";

/// The icon of ferge's windows. Only X11 window managers take it from the
/// window; Wayland compositors find the icon through the app ID and the
/// desktop file, and macOS through the app bundle.
pub(crate) fn window_icon() -> Option<std::sync::Arc<image::RgbaImage>> {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        static ICON: std::sync::OnceLock<Option<std::sync::Arc<image::RgbaImage>>> =
            std::sync::OnceLock::new();
        const PNG: &[u8] = include_bytes!(
            "../../../packaging/linux/icons/hicolor/128x128/apps/com.wirecurl.ferge.png"
        );
        ICON.get_or_init(|| {
            match image::load_from_memory_with_format(PNG, image::ImageFormat::Png) {
                Ok(icon) => Some(std::sync::Arc::new(icon.into_rgba8())),
                Err(error) => {
                    tracing::warn!("could not decode the window icon: {error}");
                    None
                }
            }
        })
        .clone()
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    None
}

/// A fast file transfer client.
#[derive(Debug, Parser)]
#[command(name = "ferge", version, about)]
struct Cli {
    /// Where to connect: `sftp://user@host:port/path`, `user@host[:port]`, an
    /// `ssh_config` alias, or an absolute local path.
    destination: Option<String>,

    /// Use this SSH client configuration instead of `~/.ssh/config`.
    #[arg(short = 'F', long, value_name = "FILE")]
    ssh_config: Option<PathBuf>,

    /// Run a benchmark scenario and exit (used by ferge-bench).
    #[arg(long, hide = true, value_enum)]
    bench: Option<bench::Scenario>,
}

/// The system allocator, counting what is allocated so that the preview
/// worker can be held to a memory limit (see [`worker::run`]). The app
/// itself runs without a limit.
#[global_allocator]
static ALLOCATOR: Cap<System> = Cap::new(System, usize::MAX);

fn main() -> ExitCode {
    // ferge started by itself to read a file for a preview: nothing else
    // is set up in that process.
    if std::env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == worker::ARGUMENT)
    {
        return worker::run(|limit| {
            let used = ALLOCATOR.allocated();
            ALLOCATOR.set_limit(used.saturating_add(limit)).is_ok()
        });
    }
    let started = Instant::now();
    let cli = Cli::parse();
    if let Some(problem) = missing_display(|name| std::env::var_os(name)) {
        // Without a display server, the window would exist but never be
        // drawn: ferge would run without anyone seeing it.
        eprintln!("ferge: {problem}");
        return ExitCode::FAILURE;
    }
    // Benchmarks must neither read nor change the user's files.
    logging::init(cli.bench.is_none());
    match worker::current_program() {
        Ok(program) => worker::set_program(program),
        Err(error) => tracing::warn!(%error, "previews of documents and pictures are unavailable"),
    }

    gpui_platform::application()
        .with_assets(assets::Assets)
        .run(move |cx: &mut App| {
            if let Err(error) = runtime::init(cx) {
                tracing::error!("failed to start the I/O runtime: {error}");
                cx.quit();
                return;
            }
            gpui_component::init(cx);
            actions::bind_keys(cx);
            workspace::bind_keys(cx);
            settings_window::init(cx);
            cx.background_spawn(async { format::load_time_zone() })
                .detach();
            // Benchmarks must neither read nor change the user's data.
            let ephemeral = cli.bench.is_some();
            let startup = appearance::Appearance::init(ephemeral, cx);

            let options = workspace::Options {
                initial: cli.destination,
                ssh_config: cli.ssh_config,
                ephemeral,
            };
            let bench = cli.bench;
            cx.spawn(async move |cx| {
                appearance::Appearance::wait_for_startup(startup, cx).await;
                cx.update(|cx| match open_main_window(options, cx) {
                    Ok((window, workspace)) => {
                        if let Some(scenario) = bench {
                            bench::start(scenario, window, workspace, started, cx);
                        }
                    }
                    Err(error) => {
                        tracing::error!("failed to open the main window: {error:#}");
                        cx.quit();
                    }
                });
            })
            .detach();
        });
    ExitCode::SUCCESS
}

/// Why ferge cannot show a window, if it cannot: on Linux, neither a
/// Wayland nor an X11 display is named, as in a plain SSH session.
fn missing_display(variable: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<&'static str> {
    if !cfg!(any(target_os = "linux", target_os = "freebsd")) {
        return None;
    }
    let named = |name: &str| variable(name).is_some_and(|value| !value.is_empty());
    (!named("WAYLAND_DISPLAY") && !named("DISPLAY")).then_some(
        "there is no display to show the window on: neither WAYLAND_DISPLAY nor DISPLAY is \
         set. In an SSH session, set the one of the desktop to open the window there, such as \
         WAYLAND_DISPLAY=wayland-1.",
    )
}

fn open_main_window(
    options: workspace::Options,
    cx: &mut App,
) -> anyhow::Result<(WindowHandle<Root>, Entity<Workspace>)> {
    let bounds = Bounds::centered(None, size(px(1180.), px(760.)), cx);
    let window_options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some(APP_NAME.into()),
            ..TitlebarOptions::default()
        }),
        app_id: Some(APP_ID.to_owned()),
        icon: window_icon(),
        // Let the compositor decorate (xdg-decoration); niri with
        // `prefer-no-csd` draws no title bar at all, which is what users want.
        window_decorations: Some(WindowDecorations::Server),
        window_min_size: Some(size(px(360.), px(240.))),
        ..WindowOptions::default()
    };
    let mut workspace = None;
    let window = cx.open_window(window_options, |window, cx| {
        tracing::debug!(decorations = ?window.window_decorations(), "main window opened");
        let view = cx.new(|cx| Workspace::new(options, window, cx));
        workspace = Some(view.clone());
        cx.new(|cx| Root::new(view, window, cx))
    })?;
    #[cfg(target_os = "macos")]
    cx.activate(true);
    let workspace =
        workspace.ok_or_else(|| anyhow::anyhow!("window was created without content"))?;
    Ok((window, workspace))
}

#[cfg(test)]
mod tests {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    #[test]
    fn refuses_to_start_without_a_display() {
        let with = |set: &'static [(&'static str, &'static str)]| {
            super::missing_display(move |name| {
                set.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).into())
            })
        };
        assert!(with(&[]).is_some(), "an SSH session");
        assert!(
            with(&[("WAYLAND_DISPLAY", "")]).is_some(),
            "empty counts as unset"
        );
        assert!(with(&[("WAYLAND_DISPLAY", "wayland-1")]).is_none());
        assert!(with(&[("DISPLAY", ":0")]).is_none());
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    #[test]
    fn the_window_icon_decodes() {
        let icon = super::window_icon().expect("the bundled icon decodes");
        assert_eq!(icon.dimensions(), (128, 128));
    }
}
