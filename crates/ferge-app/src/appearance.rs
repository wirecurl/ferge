//! How ferge looks: the appearance settings, the desktop's fonts and the
//! installed font families, applied to the theme and kept current while the
//! settings file or the desktop change.
//!
//! The settings file is read and written on the I/O runtime. Changes made in
//! the settings window apply at once and are written in order by a single
//! writer; the file watcher then reports them back like any other edit.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ferge_core::desktop::{self, DesktopFont, DesktopFonts};
use ferge_core::settings::{
    Change, FONT_SIZE_RANGE, Loaded, Settings, SettingsError, SettingsFile, ThemePreference,
};
use futures::channel::{mpsc, oneshot};
use futures::{FutureExt as _, StreamExt as _};
use gpui::{App, AppContext as _, BorrowAppContext as _, Global, Pixels, SharedString, Window, px};
use gpui_component::{ActiveTheme as _, Theme, ThemeMode};

use crate::runtime;

/// The family GPUI resolves to the platform's interface font.
const SYSTEM_UI_FONT: &str = ".SystemUIFont";

/// Interface font size when neither the settings nor the desktop name one.
const DEFAULT_FONT_SIZE: f32 = 13.0;

/// How long the window waits for the settings before it opens anyway. Reading
/// them takes a few milliseconds; the wait spares a visible change of fonts
/// right after start.
pub(crate) const STARTUP_WAIT: Duration = Duration::from_millis(60);

/// Something about the settings the user should hear about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Issue {
    /// Increases with every new issue, so each one is shown once.
    pub serial: u64,
    pub message: String,
}

/// The fonts and theme mode in effect, to skip work when nothing changed.
#[derive(Clone, Debug, PartialEq)]
struct Applied {
    font: SharedString,
    /// Body text size.
    font_size: Pixels,
    mono_font: SharedString,
}

/// What arrives from the runtime.
enum Update {
    Settings(Result<Loaded, SettingsError>),
    Desktop(DesktopFonts),
    Saved(Result<(), SettingsError>),
}

pub(crate) struct Appearance {
    /// `None` keeps everything in memory (benchmarks).
    file: Option<SettingsFile>,
    settings: Settings,
    desktop: DesktopFonts,
    /// Installed font families, sorted; empty until they are known.
    installed: Arc<[SharedString]>,
    /// gpui-component's fixed-width default, which follows the platform.
    default_mono: SharedString,
    applied: Option<Applied>,
    /// Problems with the settings file, as last reported.
    file_problem: Option<String>,
    issue: Option<Issue>,
    /// Writes changes to the file one after another.
    writer: Option<tokio::sync::mpsc::UnboundedSender<Change>>,
    updates: mpsc::UnboundedSender<Update>,
}

impl Global for Appearance {}

impl Appearance {
    /// Reads the settings and the desktop's fonts and starts following both.
    /// The returned receiver completes once the first reading is applied.
    pub(crate) fn init(ephemeral: bool, cx: &mut App) -> oneshot::Receiver<()> {
        let file = if ephemeral {
            None
        } else {
            SettingsFile::default_path().map(SettingsFile::new)
        };
        let (updates, mut receiver) = mpsc::unbounded();
        let writer = file
            .clone()
            .map(|file| Self::start_writer(file, updates.clone(), cx));
        cx.set_global(Self {
            file: file.clone(),
            settings: Settings::default(),
            desktop: DesktopFonts::default(),
            installed: Arc::from([]),
            default_mono: cx.theme().mono_font_family.clone(),
            applied: None,
            file_problem: None,
            issue: None,
            writer,
            updates: updates.clone(),
        });
        Self::load_installed_fonts(cx);
        Self::follow_desktop(updates.clone(), cx);
        if let Some(file) = &file {
            Self::follow_file(file.clone(), updates.clone(), cx);
        }

        let first = runtime::spawn(cx, async move {
            let settings = match file {
                Some(file) => tokio::task::spawn_blocking(move || file.load())
                    .await
                    .unwrap_or_else(|error| {
                        Err(SettingsError::Read {
                            path: PathBuf::new(),
                            source: std::io::Error::other(error),
                        })
                    }),
                None => Ok(Loaded::default()),
            };
            (settings, desktop::fonts().await)
        });
        let (ready, startup) = oneshot::channel();
        cx.spawn(async move |cx| {
            if let Ok((settings, fonts)) = first.await {
                cx.update(|cx| {
                    Self::receive(Update::Desktop(fonts), cx);
                    Self::receive(Update::Settings(settings), cx);
                });
            }
            let _ = ready.send(());
            while let Some(update) = receiver.next().await {
                cx.update(|cx| Self::receive(update, cx));
            }
        })
        .detach();
        startup
    }

    /// Waits until [`Self::init`] applied the settings, but no longer than
    /// [`STARTUP_WAIT`].
    pub(crate) async fn wait_for_startup(startup: oneshot::Receiver<()>, cx: &gpui::AsyncApp) {
        let timeout = cx.background_executor().timer(STARTUP_WAIT);
        futures::select_biased! {
            _ = startup.fuse() => {}
            () = timeout.fuse() => tracing::debug!("opening the window before the settings were read"),
        }
    }

    fn start_writer(
        file: SettingsFile,
        updates: mpsc::UnboundedSender<Update>,
        cx: &App,
    ) -> tokio::sync::mpsc::UnboundedSender<Change> {
        let (sender, mut changes) = tokio::sync::mpsc::unbounded_channel::<Change>();
        runtime::spawn(cx, async move {
            while let Some(change) = changes.recv().await {
                let file = file.clone();
                let saved = tokio::task::spawn_blocking(move || file.apply(&change).map(drop))
                    .await
                    .unwrap_or_else(|error| {
                        Err(SettingsError::Write {
                            path: PathBuf::new(),
                            source: std::io::Error::other(error),
                        })
                    });
                if updates.unbounded_send(Update::Saved(saved)).is_err() {
                    break;
                }
            }
        });
        sender
    }

    fn follow_file(file: SettingsFile, updates: mpsc::UnboundedSender<Update>, cx: &App) {
        runtime::spawn(cx, async move {
            let mut watcher = match file.watch() {
                Ok(watcher) => watcher,
                Err(error) => {
                    tracing::warn!(%error, "settings changes made outside ferge will need a restart");
                    return;
                }
            };
            while watcher.changed().await {
                let file = file.clone();
                let Ok(loaded) = tokio::task::spawn_blocking(move || file.load()).await else {
                    break;
                };
                if updates.unbounded_send(Update::Settings(loaded)).is_err() {
                    break;
                }
            }
        });
    }

    fn follow_desktop(updates: mpsc::UnboundedSender<Update>, cx: &App) {
        runtime::spawn(cx, async move {
            let mut changes = desktop::font_changes().await;
            while changes.next().await.is_some() {
                if updates
                    .unbounded_send(Update::Desktop(desktop::fonts().await))
                    .is_err()
                {
                    break;
                }
            }
        });
    }

    /// Asking the text system for every family walks the font database,
    /// which takes a while on systems with many fonts.
    fn load_installed_fonts(cx: &mut App) {
        let text_system = cx.text_system().clone();
        let loading = cx.background_spawn(async move {
            text_system
                .all_font_names()
                .into_iter()
                .filter(|name| !name.starts_with('.'))
                .map(SharedString::from)
                .collect::<Arc<[SharedString]>>()
        });
        cx.spawn(async move |cx| {
            let installed = loading.await;
            cx.update(|cx| {
                cx.update_global::<Self, _>(|this, _| this.installed = installed);
                Self::check_fonts(cx);
                Self::apply(cx);
            });
        })
        .detach();
    }

    fn receive(update: Update, cx: &mut App) {
        match update {
            Update::Settings(Ok(loaded)) => {
                let problem = (!loaded.problems.is_empty()).then(|| {
                    let list: Vec<String> =
                        loaded.problems.iter().map(ToString::to_string).collect();
                    format!("Some settings were ignored: {}", list.join("; "))
                });
                cx.update_global::<Self, _>(|this, _| this.settings = loaded.settings);
                Self::report_file_problem(problem, cx);
                Self::check_fonts(cx);
            }
            // Keep what is in effect until the file can be read again.
            Update::Settings(Err(error)) => Self::report_file_problem(Some(error.to_string()), cx),
            Update::Desktop(fonts) => {
                cx.update_global::<Self, _>(|this, _| this.desktop = fonts);
            }
            Update::Saved(Ok(())) => {}
            Update::Saved(Err(error)) => {
                Self::report(format!("Settings were not saved: {error}"), cx);
            }
        }
        Self::apply(cx);
    }

    /// Reports a problem with the file once, not again on every reload.
    fn report_file_problem(problem: Option<String>, cx: &mut App) {
        if cx.global::<Self>().file_problem == problem {
            return;
        }
        cx.update_global::<Self, _>(|this, _| this.file_problem.clone_from(&problem));
        if let Some(problem) = problem {
            tracing::warn!("{problem}");
            Self::report(problem, cx);
        }
    }

    fn report(message: String, cx: &mut App) {
        cx.update_global::<Self, _>(|this, _| {
            let serial = this.issue.as_ref().map_or(1, |issue| issue.serial + 1);
            this.issue = Some(Issue { serial, message });
        });
    }

    /// Fonts named in the settings that are not installed draw with a
    /// fallback; say so instead of leaving people guessing.
    fn check_fonts(cx: &mut App) {
        let this = cx.global::<Self>();
        if this.installed.is_empty() {
            return;
        }
        let missing: Vec<String> = [
            &this.settings.appearance.font,
            &this.settings.appearance.mono_font,
        ]
        .into_iter()
        .flatten()
        .filter(|family| !this.is_installed(family))
        .cloned()
        .collect();
        if !missing.is_empty() {
            Self::report(
                format!(
                    "Font not installed: {}. A fallback is used.",
                    missing.join(", ")
                ),
                cx,
            );
        }
    }

    // ---------------------------------------------------------------------
    // Applying

    /// Puts the fonts and the theme mode in effect.
    fn apply(cx: &mut App) {
        Self::apply_mode(None, cx);
        Self::apply_fonts(cx);
    }

    /// Called when a window's light or dark appearance changes.
    pub(crate) fn follow_window(window: &mut Window, cx: &mut App) {
        Self::apply_mode(Some(window), cx);
        Self::apply_fonts(cx);
    }

    fn apply_mode(window: Option<&mut Window>, cx: &mut App) {
        let preference = cx
            .try_global::<Self>()
            .map(|this| this.settings.appearance.theme)
            .unwrap_or_default();
        let mode = match preference {
            ThemePreference::Light => ThemeMode::Light,
            ThemePreference::Dark => ThemeMode::Dark,
            ThemePreference::System => match window {
                // The window knows best; the application-wide value can be
                // stale on Linux.
                Some(window) => window.appearance().into(),
                None => cx.window_appearance().into(),
            },
        };
        if cx.theme().mode != mode {
            Theme::change(mode, None, cx);
        }
    }

    fn apply_fonts(cx: &mut App) {
        let Some(this) = cx.try_global::<Self>() else {
            return;
        };
        let wanted = Applied {
            font: this.font_family(),
            font_size: px(this.font_size()),
            mono_font: this.mono_font_family(),
        };
        if this.applied.as_ref() == Some(&wanted) {
            return;
        }
        tracing::debug!(?wanted, "applying fonts");
        Theme::update(cx, |theme| {
            theme.font_family = wanted.font.clone();
            // Body text is a little smaller than one rem; see `style`.
            theme.font_size = px(crate::style::rem_for_body(f32::from(wanted.font_size)));
            theme.mono_font_family = wanted.mono_font.clone();
            theme.mono_font_size = wanted.font_size;
        });
        cx.update_global::<Self, _>(|this, _| this.applied = Some(wanted));
    }

    fn is_installed(&self, family: &str) -> bool {
        self.installed.is_empty()
            || self
                .installed
                .binary_search_by(|name| name.as_ref().cmp(family))
                .is_ok()
    }

    /// The desktop's font, if it can be drawn.
    fn usable(&self, font: Option<&DesktopFont>) -> Option<SharedString> {
        font.map(|font| font.family.as_str())
            .filter(|family| self.is_installed(family))
            .map(|family| SharedString::from(family.to_owned()))
    }

    fn font_family(&self) -> SharedString {
        self.settings
            .appearance
            .font
            .clone()
            .map(SharedString::from)
            .or_else(|| self.usable(self.desktop.interface.as_ref()))
            .unwrap_or_else(|| SYSTEM_UI_FONT.into())
    }

    fn mono_font_family(&self) -> SharedString {
        self.settings
            .appearance
            .mono_font
            .clone()
            .map(SharedString::from)
            .or_else(|| self.usable(self.desktop.monospace.as_ref()))
            .unwrap_or_else(|| self.default_mono.clone())
    }

    // ---------------------------------------------------------------------
    // For the settings window

    pub(crate) fn settings(&self) -> &Settings {
        &self.settings
    }

    /// The interface font size in effect, in pixels.
    pub(crate) fn font_size(&self) -> f32 {
        self.settings
            .appearance
            .font_size
            .or_else(|| self.desktop.interface.as_ref().map(|font| font.size))
            .unwrap_or(DEFAULT_FONT_SIZE)
            .clamp(*FONT_SIZE_RANGE.start(), *FONT_SIZE_RANGE.end())
    }

    /// What "the desktop's font" means here, for labels.
    pub(crate) fn desktop_font_name(&self) -> Option<&str> {
        self.desktop
            .interface
            .as_ref()
            .map(|font| font.family.as_str())
    }

    pub(crate) fn desktop_mono_font_name(&self) -> Option<&str> {
        self.desktop
            .monospace
            .as_ref()
            .map(|font| font.family.as_str())
    }

    pub(crate) fn installed_fonts(&self) -> Arc<[SharedString]> {
        Arc::clone(&self.installed)
    }

    pub(crate) fn file_path(&self) -> Option<&std::path::Path> {
        self.file.as_ref().map(SettingsFile::path)
    }

    pub(crate) fn issue(&self) -> Option<&Issue> {
        self.issue.as_ref()
    }

    /// Applies `change` at once and writes it to the settings file.
    pub(crate) fn change(change: Change, cx: &mut App) {
        cx.update_global::<Self, _>(|this, _| {
            let appearance = &mut this.settings.appearance;
            match &change {
                Change::Theme(theme) => appearance.theme = *theme,
                Change::Font(family) => appearance.font.clone_from(family),
                Change::FontSize(size) => appearance.font_size = *size,
                Change::MonoFont(family) => appearance.mono_font.clone_from(family),
            }
        });
        Self::apply(cx);
        let this = cx.global::<Self>();
        if let Some(writer) = &this.writer
            && writer.send(change).is_err()
        {
            let updates = this.updates.clone();
            let _ = updates.unbounded_send(Update::Saved(Err(SettingsError::Write {
                path: this.file_path().map(ToOwned::to_owned).unwrap_or_default(),
                source: std::io::Error::other("the settings writer stopped"),
            })));
        }
    }
}
