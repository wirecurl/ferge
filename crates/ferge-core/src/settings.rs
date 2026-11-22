//! User settings, kept in a TOML file that people may also edit by hand.
//!
//! The file is the single source of truth. The application changes it through
//! [`SettingsFile::apply`], which edits the document in place, so comments,
//! ordering and unknown keys written by hand survive. Every write replaces the
//! file atomically. [`SettingsWatcher`] reports changes made by anyone, which
//! is how edits in a text editor take effect while ferge runs.
//!
//! A setting that is left out takes its default; for fonts that means
//! following the desktop. A value that cannot be used is reported as a
//! [`Problem`] and replaced by its default, so one typo never resets the rest.
//!
//! All functions block on disk I/O and must not run on a UI thread.

use std::fmt;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use notify::{EventKind, RecursiveMode, Watcher as _};
use tokio::sync::mpsc;
use toml_edit::{DocumentMut, Item, Table, value};

/// The smallest and largest font size accepted, in pixels.
pub const FONT_SIZE_RANGE: std::ops::RangeInclusive<f32> = 6.0..=32.0;

/// Changes that arrive within this window are reported once: editors often
/// write a file in several steps.
const SETTLE_TIME: Duration = Duration::from_millis(75);

/// Written when the application stores the first setting, so that people who
/// open the file find out what they can change.
/// Bytes read of the settings file at most.
const READ_LIMIT: u64 = 1024 * 1024;

const TEMPLATE: &str = r#"# ferge settings. Changes take effect as soon as the file is saved.
# A setting that is left out uses its default.
#
# [appearance]
# theme       "system", "light" or "dark"
# font        interface font, such as "Inter"; default: the desktop's
# font-size   interface font size in pixels, such as 13; default: the desktop's
# mono-font   font for fixed-width text, such as "JetBrains Mono"

[appearance]
"#;

/// Errors of the [`SettingsFile`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SettingsError {
    #[error("could not read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not save {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The file is not valid TOML. Nothing is written over it.
    #[error("{path} is not valid TOML (line {line}): {message}")]
    Syntax {
        path: PathBuf,
        line: usize,
        message: String,
    },
    #[error("could not watch {path} for changes: {source}")]
    Watch {
        path: PathBuf,
        #[source]
        source: notify::Error,
    },
}

/// Light or dark colors.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThemePreference {
    /// Follow the desktop.
    #[default]
    System,
    Light,
    Dark,
}

impl ThemePreference {
    pub const ALL: [Self; 3] = [Self::System, Self::Light, Self::Dark];

    /// How the preference is written in the file.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }
}

impl FromStr for ThemePreference {
    type Err = ();

    fn from_str(text: &str) -> Result<Self, ()> {
        Self::ALL
            .into_iter()
            .find(|theme| theme.as_str() == text)
            .ok_or(())
    }
}

impl fmt::Display for ThemePreference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How ferge looks.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Appearance {
    pub theme: ThemePreference,
    /// Interface font family; `None` follows the desktop.
    pub font: Option<String>,
    /// Interface font size in pixels; `None` follows the desktop.
    pub font_size: Option<f32>,
    /// Fixed-width font family; `None` follows the desktop.
    pub mono_font: Option<String>,
}

/// Everything the settings file holds.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings {
    pub appearance: Appearance,
}

/// A value in the file that was ignored, with the reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    /// Dotted key, such as `appearance.font-size`.
    pub key: String,
    pub message: String,
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.key, self.message)
    }
}

/// Settings read from a file, and what in it could not be used.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Loaded {
    pub settings: Settings,
    pub problems: Vec<Problem>,
}

/// One change the application makes to the file. `None` removes the key, so
/// the default applies again.
#[derive(Clone, Debug, PartialEq)]
pub enum Change {
    Theme(ThemePreference),
    Font(Option<String>),
    FontSize(Option<f32>),
    MonoFont(Option<String>),
}

/// Keys of the `[appearance]` table.
mod key {
    pub(super) const APPEARANCE: &str = "appearance";
    pub(super) const THEME: &str = "theme";
    pub(super) const FONT: &str = "font";
    pub(super) const FONT_SIZE: &str = "font-size";
    pub(super) const MONO_FONT: &str = "mono-font";
}

/// The settings file on disk.
#[derive(Clone, Debug)]
pub struct SettingsFile {
    path: PathBuf,
}

impl SettingsFile {
    /// `ferge/settings.toml` in the platform's configuration directory
    /// (`$XDG_CONFIG_HOME` or `~/.config` on Linux,
    /// `~/Library/Application Support` on macOS).
    pub fn default_path() -> Option<PathBuf> {
        dirs::config_dir().map(|dir| dir.join("ferge").join("settings.toml"))
    }

    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads the file. A missing file means every setting has its default.
    pub fn load(&self) -> Result<Loaded, SettingsError> {
        let text = self.read()?.unwrap_or_default();
        let document = self.parse(&text)?;
        Ok(read_settings(&document))
    }

    /// Writes `change` into the file, keeping everything else in it as it is.
    /// Creates the file from a commented template if it does not exist yet.
    pub fn apply(&self, change: &Change) -> Result<Loaded, SettingsError> {
        let text = self.read()?.unwrap_or_else(|| TEMPLATE.to_owned());
        let mut document = self.parse(&text)?;
        write_change(&mut document, change);
        let updated = document.to_string();
        if updated != text {
            self.write(&updated)?;
        }
        Ok(read_settings(&document))
    }

    /// Writes the commented template if the file does not exist yet, so it
    /// can be opened in an editor.
    pub fn create_if_missing(&self) -> Result<(), SettingsError> {
        if self.read()?.is_none() {
            self.write(TEMPLATE)?;
        }
        Ok(())
    }

    fn read(&self) -> Result<Option<String>, SettingsError> {
        use std::io::Read as _;
        let error = |source| SettingsError::Read {
            path: self.path.clone(),
            source,
        };
        let file = match std::fs::File::open(&self.path) {
            Ok(file) => file,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(error(source)),
        };
        // A settings file is a few hundred bytes; whatever the path leads
        // to (a link to /dev/zero, say) is not read without end.
        let mut bytes = Vec::new();
        file.take(READ_LIMIT + 1)
            .read_to_end(&mut bytes)
            .map_err(error)?;
        if bytes.len() as u64 > READ_LIMIT {
            return Err(error(std::io::Error::other(
                "the file is too large for settings",
            )));
        }
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| error(std::io::Error::other("the file is not UTF-8 text")))
    }

    fn parse(&self, text: &str) -> Result<DocumentMut, SettingsError> {
        text.parse::<DocumentMut>()
            .map_err(|error| SettingsError::Syntax {
                path: self.path.clone(),
                line: error.span().map_or(1, |span| line_of(text, span.start)),
                message: error.message().to_owned(),
            })
    }

    /// Replaces the file atomically: a reader sees the old or the new file,
    /// never a half-written one. A symbolic link (as dotfile managers use)
    /// stays in place and its target is replaced, but only in a folder no
    /// one else may write to: elsewhere, a link someone else placed could
    /// make ferge overwrite any file of the user's, so the link itself is
    /// replaced instead.
    fn write(&self, text: &str) -> Result<(), SettingsError> {
        let error = |source| SettingsError::Write {
            path: self.path.clone(),
            source,
        };
        let target = if self.follows_link() {
            match std::fs::canonicalize(&self.path) {
                Ok(target) => target,
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => self.path.clone(),
                Err(source) => return Err(error(source)),
            }
        } else {
            self.path.clone()
        };
        let dir = target
            .parent()
            .ok_or_else(|| error(std::io::Error::other("the path has no parent directory")))?;
        std::fs::create_dir_all(dir).map_err(error)?;
        let mut file = tempfile::Builder::new()
            .prefix(".settings.")
            .suffix(".toml")
            .tempfile_in(dir)
            .map_err(error)?;
        file.write_all(text.as_bytes()).map_err(error)?;
        file.as_file().sync_all().map_err(error)?;
        file.persist(&target)
            .map_err(|failed| error(failed.error))?;
        Ok(())
    }

    /// Whether writing may go through a symbolic link at the path: only if
    /// the folder holding the link cannot be written by others.
    fn follows_link(&self) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let Some(dir) = self.path.parent() else {
                return false;
            };
            std::fs::metadata(dir).is_ok_and(|meta| meta.permissions().mode() & 0o022 == 0)
        }
        #[cfg(not(unix))]
        {
            true
        }
    }

    /// Starts watching the file for changes by anyone, including this
    /// process. The directory is created if necessary, so a file that does
    /// not exist yet is noticed once it is written.
    pub fn watch(&self) -> Result<SettingsWatcher, SettingsError> {
        let error = |source| SettingsError::Watch {
            path: self.path.clone(),
            source,
        };
        let dir = self
            .path
            .parent()
            .ok_or_else(|| error(notify::Error::generic("the path has no parent directory")))?;
        std::fs::create_dir_all(dir).map_err(|source| SettingsError::Write {
            path: dir.to_path_buf(),
            source,
        })?;
        let name = self.path.file_name().map(ToOwned::to_owned);
        let (sender, receiver) = mpsc::channel(1);
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                let Ok(event) = event else {
                    return;
                };
                let relevant = !matches!(event.kind, EventKind::Access(_))
                    && event
                        .paths
                        .iter()
                        .any(|path| path.file_name() == name.as_deref());
                if relevant {
                    // A full channel already holds a pending notification.
                    let _ = sender.try_send(());
                }
            })
            .map_err(error)?;
        // Editors and dotfile managers often replace the file instead of
        // writing into it, which a watch on the file itself would miss.
        watcher
            .watch(dir, RecursiveMode::NonRecursive)
            .map_err(error)?;
        Ok(SettingsWatcher {
            _watcher: watcher,
            receiver,
        })
    }
}

/// Reports changes to the settings file; see [`SettingsFile::watch`].
pub struct SettingsWatcher {
    _watcher: notify::RecommendedWatcher,
    receiver: mpsc::Receiver<()>,
}

impl fmt::Debug for SettingsWatcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SettingsWatcher").finish_non_exhaustive()
    }
}

impl SettingsWatcher {
    /// Waits until the file changed and settled. Returns `false` once the
    /// watch has ended.
    pub async fn changed(&mut self) -> bool {
        if self.receiver.recv().await.is_none() {
            return false;
        }
        tokio::time::sleep(SETTLE_TIME).await;
        while self.receiver.try_recv().is_ok() {}
        true
    }
}

/// 1-based line of the byte `offset` in `text`.
fn line_of(text: &str, offset: usize) -> usize {
    text.as_bytes()
        .iter()
        .take(offset)
        .filter(|&&byte| byte == b'\n')
        .count()
        + 1
}

fn read_settings(document: &DocumentMut) -> Loaded {
    let mut problems = Vec::new();
    let mut settings = Settings::default();
    let Some(appearance) = document.get(key::APPEARANCE) else {
        return Loaded { settings, problems };
    };
    let Some(appearance) = appearance.as_table_like() else {
        problems.push(Problem {
            key: key::APPEARANCE.to_owned(),
            message: "must be a table".to_owned(),
        });
        return Loaded { settings, problems };
    };
    let mut problem = |key: &str, message: String| {
        problems.push(Problem {
            key: format!("{}.{key}", key::APPEARANCE),
            message,
        });
    };
    let mut appearance_settings = Appearance::default();
    for (name, item) in appearance.iter() {
        match name {
            key::THEME => match item.as_str().map(str::parse) {
                Some(Ok(theme)) => appearance_settings.theme = theme,
                _ => problem(name, r#"must be "system", "light" or "dark""#.to_owned()),
            },
            key::FONT | key::MONO_FONT => match item.as_str().map(str::trim) {
                Some(family) if !family.is_empty() => {
                    let family = Some(family.to_owned());
                    if name == key::FONT {
                        appearance_settings.font = family;
                    } else {
                        appearance_settings.mono_font = family;
                    }
                }
                _ => problem(name, "must be the name of a font family".to_owned()),
            },
            key::FONT_SIZE => match number(item) {
                Some(size) if FONT_SIZE_RANGE.contains(&size) => {
                    appearance_settings.font_size = Some(size);
                }
                _ => problem(
                    name,
                    format!(
                        "must be a number from {} to {}",
                        FONT_SIZE_RANGE.start(),
                        FONT_SIZE_RANGE.end()
                    ),
                ),
            },
            _ => problem(name, "is not a setting".to_owned()),
        }
    }
    settings.appearance = appearance_settings;
    Loaded { settings, problems }
}

/// An integer or float as `f32`.
fn number(item: &Item) -> Option<f32> {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        reason = "font sizes are small; out-of-range values are rejected by the caller"
    )]
    item.as_float()
        .map(|float| float as f32)
        .or_else(|| item.as_integer().map(|integer| integer as f32))
        .filter(|size| size.is_finite())
}

fn write_change(document: &mut DocumentMut, change: &Change) {
    let root = document.as_table_mut();
    if !root.get(key::APPEARANCE).is_some_and(Item::is_table_like) {
        root.insert(key::APPEARANCE, Item::Table(Table::new()));
    }
    let Some(appearance) = root
        .get_mut(key::APPEARANCE)
        .and_then(Item::as_table_like_mut)
    else {
        return;
    };
    let (name, new_value) = match change {
        Change::Theme(ThemePreference::System) => (key::THEME, None),
        Change::Theme(theme) => (key::THEME, Some(value(theme.as_str()))),
        Change::Font(family) => (key::FONT, family.as_deref().map(value)),
        Change::MonoFont(family) => (key::MONO_FONT, family.as_deref().map(value)),
        Change::FontSize(size) => (key::FONT_SIZE, size.map(font_size_value)),
    };
    match new_value {
        Some(new_value) => {
            appearance.insert(name, new_value);
        }
        None => {
            appearance.remove(name);
        }
    }
}

/// Whole sizes are written as integers (`13`, not `13.0`).
fn font_size_value(size: f32) -> Item {
    let size = size.clamp(*FONT_SIZE_RANGE.start(), *FONT_SIZE_RANGE.end());
    if size.fract() == 0.0 {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "clamped to a small whole number"
        )]
        value(i64::from(size as i32))
    } else {
        value(f64::from(size))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Loaded {
        read_settings(&text.parse().unwrap())
    }

    fn file(dir: &tempfile::TempDir) -> SettingsFile {
        SettingsFile::new(dir.path().join("ferge").join("settings.toml"))
    }

    #[test]
    fn empty_file_has_defaults() {
        assert_eq!(parse(""), Loaded::default());
        assert_eq!(parse(TEMPLATE), Loaded::default());
    }

    #[test]
    fn reads_every_setting() {
        let loaded = parse(
            r#"
            [appearance]
            theme = "dark"
            font = "Fira Sans"
            font-size = 13.5
            mono-font = "JetBrains Mono"
            "#,
        );
        assert_eq!(loaded.problems, []);
        assert_eq!(
            loaded.settings.appearance,
            Appearance {
                theme: ThemePreference::Dark,
                font: Some("Fira Sans".into()),
                font_size: Some(13.5),
                mono_font: Some("JetBrains Mono".into()),
            }
        );
        assert_eq!(
            parse("[appearance]\nfont-size = 14\n")
                .settings
                .appearance
                .font_size,
            Some(14.0)
        );
    }

    #[test]
    fn bad_values_fall_back_and_are_reported() {
        let loaded = parse(
            r#"
            [appearance]
            theme = "purple"
            font = ""
            font-size = 400
            mono-font = "Iosevka"
            colour = "red"
            "#,
        );
        assert_eq!(loaded.settings.appearance.theme, ThemePreference::System);
        assert_eq!(loaded.settings.appearance.font, None);
        assert_eq!(loaded.settings.appearance.font_size, None);
        // A good value next to bad ones is still used.
        assert_eq!(
            loaded.settings.appearance.mono_font.as_deref(),
            Some("Iosevka")
        );
        let keys: Vec<&str> = loaded.problems.iter().map(|p| p.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "appearance.theme",
                "appearance.font",
                "appearance.font-size",
                "appearance.colour"
            ]
        );
    }

    #[test]
    fn appearance_must_be_a_table() {
        let loaded = parse("appearance = 3\n");
        assert_eq!(loaded.settings, Settings::default());
        assert_eq!(loaded.problems.len(), 1);
    }

    #[test]
    fn first_change_writes_the_template() {
        let dir = tempfile::tempdir().unwrap();
        let file = file(&dir);
        let loaded = file.apply(&Change::Theme(ThemePreference::Dark)).unwrap();
        assert_eq!(loaded.settings.appearance.theme, ThemePreference::Dark);
        let text = std::fs::read_to_string(file.path()).unwrap();
        assert!(text.starts_with("# ferge settings."));
        assert!(text.contains("theme = \"dark\""));
        assert_eq!(file.load().unwrap(), loaded);
    }

    #[test]
    fn changes_keep_comments_and_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        let file = file(&dir);
        let original =
            "# mine\n[appearance]\nfont = \"Inter\" # nice\nextra = 1\n\n[later]\nx = 2\n";
        std::fs::create_dir_all(file.path().parent().unwrap()).unwrap();
        std::fs::write(file.path(), original).unwrap();

        file.apply(&Change::FontSize(Some(14.0))).unwrap();
        let text = std::fs::read_to_string(file.path()).unwrap();
        assert!(text.starts_with("# mine\n[appearance]\nfont = \"Inter\" # nice\nextra = 1\n"));
        assert!(text.contains("font-size = 14\n"));
        assert!(text.contains("[later]\nx = 2"));

        file.apply(&Change::FontSize(Some(13.5))).unwrap();
        assert!(
            std::fs::read_to_string(file.path())
                .unwrap()
                .contains("font-size = 13.5\n")
        );
    }

    #[test]
    fn none_removes_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let file = file(&dir);
        file.apply(&Change::Font(Some("Inter".into()))).unwrap();
        file.apply(&Change::Theme(ThemePreference::Light)).unwrap();
        let loaded = file.apply(&Change::Font(None)).unwrap();
        assert_eq!(loaded.settings.appearance.font, None);
        let loaded = file.apply(&Change::Theme(ThemePreference::System)).unwrap();
        assert_eq!(loaded.settings, Settings::default());
        let text = std::fs::read_to_string(file.path()).unwrap();
        assert!(!text.lines().any(|line| line.starts_with("font =")));
        assert!(!text.lines().any(|line| line.starts_with("theme =")));
    }

    #[test]
    fn invalid_toml_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let file = file(&dir);
        std::fs::create_dir_all(file.path().parent().unwrap()).unwrap();
        let broken = "[appearance]\n\ntheme = \"dark\n";
        std::fs::write(file.path(), broken).unwrap();
        let error = file.apply(&Change::Font(None)).unwrap_err();
        assert!(
            matches!(error, SettingsError::Syntax { line: 3, .. }),
            "{error}"
        );
        assert!(matches!(file.load(), Err(SettingsError::Syntax { .. })));
        assert_eq!(std::fs::read_to_string(file.path()).unwrap(), broken);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_files_stay_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let file = file(&dir);
        let real = dir.path().join("dotfiles-settings.toml");
        std::fs::write(&real, "[appearance]\n").unwrap();
        std::fs::create_dir_all(file.path().parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&real, file.path()).unwrap();

        file.apply(&Change::Theme(ThemePreference::Dark)).unwrap();
        assert!(file.path().symlink_metadata().unwrap().is_symlink());
        assert!(std::fs::read_to_string(&real).unwrap().contains("dark"));
    }

    #[cfg(unix)]
    #[test]
    fn links_in_shared_folders_are_not_written_through() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let file = file(&dir);
        let shared = file.path().parent().unwrap();
        std::fs::create_dir_all(shared).unwrap();
        std::fs::set_permissions(shared, std::fs::Permissions::from_mode(0o777)).unwrap();
        // Someone else links the settings to a file of the user's.
        let victim = dir.path().join("victim.toml");
        std::fs::write(&victim, "# mine\n").unwrap();
        std::os::unix::fs::symlink(&victim, file.path()).unwrap();

        file.apply(&Change::Theme(ThemePreference::Dark)).unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "# mine\n");
        assert!(!file.path().symlink_metadata().unwrap().is_symlink());
        assert!(
            std::fs::read_to_string(file.path())
                .unwrap()
                .contains("dark")
        );
    }

    #[cfg(unix)]
    #[test]
    fn reads_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let file = file(&dir);
        std::fs::create_dir_all(file.path().parent().unwrap()).unwrap();
        std::os::unix::fs::symlink("/dev/zero", file.path()).unwrap();
        assert!(matches!(file.load(), Err(SettingsError::Read { .. })));
    }

    #[tokio::test]
    async fn watcher_sees_writes() {
        let dir = tempfile::tempdir().unwrap();
        let file = file(&dir);
        let mut watcher = file.watch().unwrap();
        file.apply(&Change::Theme(ThemePreference::Dark)).unwrap();
        let changed = tokio::time::timeout(Duration::from_secs(5), watcher.changed()).await;
        assert_eq!(changed, Ok(true));
    }

    #[test]
    fn creates_the_template_only_once() {
        let dir = tempfile::tempdir().unwrap();
        let file = file(&dir);
        file.create_if_missing().unwrap();
        assert_eq!(std::fs::read_to_string(file.path()).unwrap(), TEMPLATE);
        file.apply(&Change::Theme(ThemePreference::Dark)).unwrap();
        file.create_if_missing().unwrap();
        assert!(
            std::fs::read_to_string(file.path())
                .unwrap()
                .contains("dark")
        );
    }

    #[test]
    fn whole_sizes_are_integers() {
        assert_eq!(font_size_value(13.0).to_string(), "13");
        assert_eq!(font_size_value(12.5).to_string(), "12.5");
        assert_eq!(font_size_value(100.0).to_string(), "32");
    }
}
