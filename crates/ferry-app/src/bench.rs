//! Scripted benchmark scenarios, driven by ferry-bench.
//!
//! The app reports on stderr. Lines starting with `ferry-bench:` are markers;
//! with `ZED_MEASUREMENTS=1`, GPUI prints `frame duration: …` for every drawn
//! frame. Inputs are dispatched right before a frame is requested and the
//! script waits for that frame, so each `input` marker is followed by the
//! duration of exactly the frame that shows its effect.

use std::time::{Duration, Instant};

use futures::channel::oneshot;
use gpui::{AnyWindowHandle, App, AsyncApp, Entity, Keystroke, WindowHandle};
use gpui_component::Root;

use crate::browser::FileBrowser;
use crate::workspace::Workspace;

/// A benchmark scenario.
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub(crate) enum Scenario {
    /// Report when the first frame is on screen, then quit.
    Startup,
    /// Wait for the listing, then exercise keyboard navigation, scrolling,
    /// type-to-filter, sorting and cached navigation.
    Interaction,
}

/// Upper bound for any wait in a scenario, so a broken setup cannot hang.
const TIMEOUT: Duration = Duration::from_secs(120);

fn marker(text: &str) {
    eprintln!("ferry-bench: {text}");
}

pub(crate) fn start(
    scenario: Scenario,
    window: WindowHandle<Root>,
    workspace: Entity<Workspace>,
    started: Instant,
    cx: &mut App,
) {
    match scenario {
        Scenario::Startup => {
            let _ = window.update(cx, |_, window, _| {
                // The first callback runs right before the first frame is
                // drawn; the second one only after it has been presented.
                window.on_next_frame(move |window, _| {
                    window.on_next_frame(move |_, cx| {
                        marker(&format!("ready {}", started.elapsed().as_micros()));
                        cx.quit();
                    });
                });
            });
        }
        Scenario::Interaction => {
            // `AnyWindowHandle::update` does not lease the root view, so key
            // events can be dispatched through it.
            let window = AnyWindowHandle::from(window);
            cx.spawn(async move |cx| {
                if let Err(error) = interaction(window, workspace, cx).await {
                    marker(&format!("error {error}"));
                }
                marker("done");
                cx.update(|cx| cx.quit());
            })
            .detach();
        }
    }
}

async fn next_frame(window: AnyWindowHandle, cx: &mut AsyncApp) {
    let (drawn, wait) = oneshot::channel();
    let _ = window.update(cx, |_, window, _| {
        window.on_next_frame(move |_, _| {
            let _ = drawn.send(());
        });
    });
    let _ = wait.await;
}

/// Dispatches a key press and waits for the frame that shows it.
async fn key(
    window: AnyWindowHandle,
    cx: &mut AsyncApp,
    keystroke: &'static str,
) -> anyhow::Result<()> {
    let keystroke = Keystroke::parse(keystroke)?;
    window.update(cx, |_, window, cx| {
        marker("input");
        window.dispatch_keystroke(keystroke, cx);
    })?;
    next_frame(window, cx).await;
    Ok(())
}

fn browser(workspace: &Entity<Workspace>, cx: &mut AsyncApp) -> Option<Entity<FileBrowser>> {
    workspace.read_with(cx, |workspace, _| workspace.browser().cloned())
}

/// Waits frame by frame until the browser is idle; returns the elapsed time.
async fn until_idle(
    window: AnyWindowHandle,
    workspace: &Entity<Workspace>,
    cx: &mut AsyncApp,
) -> anyhow::Result<Duration> {
    let started = Instant::now();
    loop {
        if let Some(browser) = browser(workspace, cx)
            && browser.read_with(cx, |browser, _| browser.is_idle())
        {
            return Ok(started.elapsed());
        }
        anyhow::ensure!(
            started.elapsed() < TIMEOUT,
            "timed out waiting for the browser"
        );
        next_frame(window, cx).await;
    }
}

async fn interaction(
    window: AnyWindowHandle,
    workspace: Entity<Workspace>,
    cx: &mut AsyncApp,
) -> anyhow::Result<()> {
    let loaded = until_idle(window, &workspace, cx).await?;
    let browser = browser(&workspace, cx).ok_or_else(|| anyhow::anyhow!("not connected"))?;
    let (first_rows, total) = browser.read_with(cx, |browser, _| {
        (browser.first_rows_after(), browser.total_len())
    });
    marker(&format!(
        "listing entries={total} first_rows_us={} complete_us={}",
        first_rows.map_or(0, |d| d.as_micros()),
        loaded.as_micros()
    ));

    marker("phase keys-down");
    for _ in 0..300 {
        key(window, cx, "down").await?;
    }
    marker("phase keys-pagedown");
    for _ in 0..200 {
        key(window, cx, "pagedown").await?;
    }
    key(window, cx, "home").await?;

    marker("phase scroll");
    for _ in 0..600 {
        window.update(cx, |_, _, cx| {
            marker("input");
            browser.update(cx, |browser, cx| browser.scroll_by(gpui::px(160.), cx));
        })?;
        next_frame(window, cx).await;
    }

    marker("phase filter");
    for character in ["r", "e", "p", "o", "r", "t", "-", "0", "1"] {
        key(window, cx, character).await?;
    }
    for _ in 0..9 {
        key(window, cx, "backspace").await?;
    }

    for (phase, shortcut) in [
        ("sort-size", sort_shortcut(2)),
        ("sort-size-flip", sort_shortcut(2)),
        ("sort-modified", sort_shortcut(3)),
        ("sort-name", sort_shortcut(1)),
    ] {
        marker(&format!("phase {phase}"));
        key(window, cx, shortcut).await?;
        let took = until_idle(window, &workspace, cx).await?;
        marker(&format!("settled {phase} us={}", took.as_micros()));
    }

    marker("phase navigate-cached");
    key(window, cx, parent_shortcut()).await?;
    until_idle(window, &workspace, cx).await?;
    let started = Instant::now();
    key(window, cx, "enter").await?;
    let shown = browser.read_with(cx, |browser, _| browser.total_len());
    marker(&format!(
        "settled navigate-cached us={} entries={shown}",
        started.elapsed().as_micros()
    ));
    until_idle(window, &workspace, cx).await?;
    Ok(())
}

const fn sort_shortcut(column: u8) -> &'static str {
    #[cfg(target_os = "macos")]
    let shortcuts = ["cmd-1", "cmd-2", "cmd-3"];
    #[cfg(not(target_os = "macos"))]
    let shortcuts = ["ctrl-1", "ctrl-2", "ctrl-3"];
    shortcuts[(column - 1) as usize]
}

const fn parent_shortcut() -> &'static str {
    if cfg!(target_os = "macos") {
        "cmd-up"
    } else {
        "alt-up"
    }
}
