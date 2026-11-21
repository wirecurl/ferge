//! Font choices that depend on what is installed.

use std::sync::OnceLock;

use gpui::{App, AppContext as _, SharedString};

/// Monospace families in order of preference. Generic names such as
/// `monospace` are not resolved by every text system, so a concrete family
/// is picked from the installed ones.
const MONOSPACE_PREFERENCES: &[&str] = &[
    "SF Mono",
    "Menlo",
    "JetBrains Mono",
    "Cascadia Mono",
    "Fira Mono",
    "Source Code Pro",
    "Noto Sans Mono",
    "DejaVu Sans Mono",
    "Liberation Mono",
    "Ubuntu Mono",
];

static MONOSPACE: OnceLock<SharedString> = OnceLock::new();

/// Looks up the installed fonts in the background.
pub(crate) fn detect(cx: &App) {
    let text_system = cx.text_system().clone();
    cx.background_spawn(async move {
        let installed = text_system.all_font_names();
        let found = MONOSPACE_PREFERENCES
            .iter()
            .find(|family| installed.iter().any(|name| name == *family));
        if let Some(family) = found {
            let _ = MONOSPACE.set(SharedString::new_static(family));
        }
    })
    .detach();
}

/// The monospace family to use, once detected.
pub(crate) fn monospace() -> SharedString {
    MONOSPACE.get().cloned().unwrap_or_else(|| {
        SharedString::new_static(if cfg!(target_os = "macos") {
            "Menlo"
        } else {
            "monospace"
        })
    })
}
