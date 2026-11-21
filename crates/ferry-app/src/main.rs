//! Ferry desktop application.

mod actions;
mod bench;
mod browser;
mod editor;
mod fonts;
mod format;
mod interaction;
mod prompts;
mod runtime;
mod session;
mod vault;
mod workspace;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use clap::Parser;
use gpui::{
    App, AppContext as _, Bounds, Entity, TitlebarOptions, WindowBounds, WindowDecorations,
    WindowHandle, WindowOptions, px, size,
};
use gpui_component::Root;
use tracing_subscriber::EnvFilter;

use crate::workspace::Workspace;

/// Wayland `app_id`, also used by desktop files and compositor window rules.
const APP_ID: &str = "dev.ferry.Ferry";
const APP_NAME: &str = "Ferry";

/// A fast file transfer client.
#[derive(Debug, Parser)]
#[command(name = "ferry", version, about)]
struct Cli {
    /// Where to connect: `sftp://user@host:port/path`, `user@host[:port]`, an
    /// `ssh_config` alias, or an absolute local path.
    destination: Option<String>,

    /// Use this SSH client configuration instead of `~/.ssh/config`.
    #[arg(short = 'F', long, value_name = "FILE")]
    ssh_config: Option<PathBuf>,

    /// Run a benchmark scenario and exit (used by ferry-bench).
    #[arg(long, hide = true, value_enum)]
    bench: Option<bench::Scenario>,
}

fn main() -> ExitCode {
    let started = Instant::now();
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();

    gpui_platform::application()
        .with_assets(gpui_component_assets::Assets)
        .run(move |cx: &mut App| {
            if let Err(error) = runtime::init(cx) {
                tracing::error!("failed to start the I/O runtime: {error}");
                cx.quit();
                return;
            }
            gpui_component::init(cx);
            actions::bind_keys(cx);
            workspace::bind_keys(cx);
            cx.background_spawn(async { format::load_time_zone() })
                .detach();
            fonts::detect(cx);

            let options = workspace::Options {
                initial: cli.destination,
                ssh_config: cli.ssh_config,
                // Benchmarks must neither read nor change the user's data.
                ephemeral: cli.bench.is_some(),
            };
            match open_main_window(options, cx) {
                Ok((window, workspace)) => {
                    if let Some(scenario) = cli.bench {
                        bench::start(scenario, window, workspace, started, cx);
                    }
                }
                Err(error) => {
                    tracing::error!("failed to open the main window: {error:#}");
                    cx.quit();
                }
            }
        });
    ExitCode::SUCCESS
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
