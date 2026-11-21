//! The settings window. It edits the settings file; whatever is changed here
//! can also be changed in the file, and the other way round.

use ferry_core::settings::{Change, FONT_SIZE_RANGE, ThemePreference};
use gpui::{
    App, AppContext as _, Context, Global, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, TitlebarOptions, Window, WindowBounds, WindowDecorations, WindowHandle,
    WindowOptions, px, size,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::group_box::GroupBoxVariant;
use gpui_component::setting::{
    NumberFieldOptions, SettingField, SettingGroup, SettingItem, SettingPage, Settings,
};
use gpui_component::{ActiveTheme as _, IconName, Root, Sizable as _, h_flex, v_flex};

use crate::appearance::Appearance;
use crate::style::TypeScale as _;

gpui::actions!(
    ferry,
    [
        /// Opens the settings window, or brings it to the front.
        OpenSettings
    ]
);

/// Value of the font menus that stands for "follow the desktop".
const DESKTOP_DEFAULT: &str = "";

/// The open settings window, so a second request focuses it.
#[derive(Default)]
struct OpenWindow(Option<WindowHandle<Root>>);

impl Global for OpenWindow {}

/// Registers the action and its shortcut for every window.
pub(crate) fn init(cx: &mut App) {
    cx.set_global(OpenWindow::default());
    cx.on_action(|_: &OpenSettings, cx| open(cx));
    let shortcut = if cfg!(target_os = "macos") {
        "cmd-,"
    } else {
        "ctrl-,"
    };
    cx.bind_keys([gpui::KeyBinding::new(shortcut, OpenSettings, None)]);
}

fn open(cx: &mut App) {
    if let Some(handle) = cx.global::<OpenWindow>().0
        && handle
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    {
        return;
    }
    let bounds = gpui::Bounds::centered(None, size(px(760.), px(560.)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some("Ferry Settings".into()),
            ..TitlebarOptions::default()
        }),
        app_id: Some(crate::APP_ID.to_owned()),
        window_decorations: Some(WindowDecorations::Server),
        window_min_size: Some(size(px(420.), px(320.))),
        ..WindowOptions::default()
    };
    match cx.open_window(options, |window, cx| {
        let view = cx.new(|cx| SettingsView::new(window, cx));
        cx.new(|cx| Root::new(view, window, cx))
    }) {
        Ok(handle) => cx.set_global(OpenWindow(Some(handle))),
        Err(error) => tracing::error!("could not open the settings window: {error:#}"),
    }
}

struct SettingsView {
    _subscriptions: Vec<gpui::Subscription>,
}

impl SettingsView {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let redraw = cx.observe_global::<Appearance>(|_, cx| cx.notify());
        let appearance = cx.observe_window_appearance(window, |_, window, cx| {
            Appearance::follow_window(window, cx);
        });
        Self {
            _subscriptions: vec![redraw, appearance],
        }
    }
}

impl Render for SettingsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .text_body()
            .child(
                Settings::new("settings")
                    .sidebar_width(px(200.))
                    .with_group_variant(GroupBoxVariant::Outline)
                    .page(appearance_page(cx)),
            )
    }
}

fn appearance_page(cx: &App) -> SettingPage {
    SettingPage::new("Appearance")
        .icon(IconName::Palette)
        .default_open(true)
        .resettable(true)
        .groups([
            SettingGroup::new().title("Colors").item(theme_item()),
            SettingGroup::new()
                .title("Fonts")
                .description("By default Ferry uses the fonts of your desktop.")
                .items([font_item(cx), font_size_item(), mono_font_item(cx)]),
            SettingGroup::new()
                .title("Settings file")
                .description(
                    "Everything here is stored in a text file. Changes made to it \
                     take effect as soon as it is saved.",
                )
                .item(file_item()),
        ])
}

fn theme_item() -> SettingItem {
    let options = ThemePreference::ALL
        .into_iter()
        .map(|theme| {
            let label = match theme {
                ThemePreference::System => "Like the system",
                ThemePreference::Light => "Light",
                ThemePreference::Dark => "Dark",
            };
            (theme.as_str().into(), label.into())
        })
        .collect();
    SettingItem::new(
        "Theme",
        SettingField::dropdown(
            options,
            |cx| {
                cx.global::<Appearance>()
                    .settings()
                    .appearance
                    .theme
                    .as_str()
                    .into()
            },
            |value: SharedString, cx| {
                let theme = value.parse().unwrap_or_default();
                Appearance::change(Change::Theme(theme), cx);
            },
        )
        .default_value(ThemePreference::System.as_str()),
    )
    .keywords(["dark", "light", "mode", "colors"])
}

/// A font menu: "the desktop's font" first, then every installed family.
fn font_options(desktop: Option<&str>, cx: &App) -> Vec<(SharedString, SharedString)> {
    let default = match desktop {
        Some(family) => format!("Desktop default ({family})"),
        None => "Desktop default".to_owned(),
    };
    std::iter::once((DESKTOP_DEFAULT.into(), default.into()))
        .chain(
            cx.global::<Appearance>()
                .installed_fonts()
                .iter()
                .map(|family| (family.clone(), family.clone())),
        )
        .collect()
}

fn family_change(value: &SharedString) -> Option<String> {
    (value.as_ref() != DESKTOP_DEFAULT).then(|| value.to_string())
}

fn font_item(cx: &App) -> SettingItem {
    let desktop = cx.global::<Appearance>().desktop_font_name();
    SettingItem::new(
        "Interface font",
        SettingField::scrollable_dropdown(
            font_options(desktop, cx),
            |cx| {
                let font = &cx.global::<Appearance>().settings().appearance.font;
                font.clone().unwrap_or_default().into()
            },
            |value: SharedString, cx| {
                Appearance::change(Change::Font(family_change(&value)), cx);
            },
        )
        .default_value(DESKTOP_DEFAULT),
    )
    .keywords(["font", "typeface", "text"])
}

fn mono_font_item(cx: &App) -> SettingItem {
    let desktop = cx.global::<Appearance>().desktop_mono_font_name();
    SettingItem::new(
        "Fixed-width font",
        SettingField::scrollable_dropdown(
            font_options(desktop, cx),
            |cx| {
                let font = &cx.global::<Appearance>().settings().appearance.mono_font;
                font.clone().unwrap_or_default().into()
            },
            |value: SharedString, cx| {
                Appearance::change(Change::MonoFont(family_change(&value)), cx);
            },
        )
        .default_value(DESKTOP_DEFAULT),
    )
    .description("Used for permissions, addresses and fingerprints.")
    .keywords(["font", "monospace", "code"])
}

fn font_size_item() -> SettingItem {
    let options = NumberFieldOptions {
        min: f64::from(*FONT_SIZE_RANGE.start()),
        max: f64::from(*FONT_SIZE_RANGE.end()),
        step: 1.0,
    };
    SettingItem::new(
        "Font size",
        SettingField::number_input(
            options,
            // Desktop sizes come from points and are rarely whole pixels.
            |cx| (f64::from(cx.global::<Appearance>().font_size()) * 2.0).round() / 2.0,
            |value: f64, cx| {
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "the field limits the value to the font size range"
                )]
                let size = ((value * 2.0).round() / 2.0) as f32;
                Appearance::change(Change::FontSize(Some(size)), cx);
            },
        )
        .on_reset(
            |cx| {
                cx.global::<Appearance>()
                    .settings()
                    .appearance
                    .font_size
                    .is_some()
            },
            |_, cx| Appearance::change(Change::FontSize(None), cx),
        ),
    )
    .description("In pixels. Everything in the window grows and shrinks with it.")
    .keywords(["font", "size", "zoom", "scale"])
}

fn file_item() -> SettingItem {
    SettingItem::render(|_, _, cx| {
        let path = cx.global::<Appearance>().file_path().map(ToOwned::to_owned);
        let theme = cx.theme();
        let (muted, mono) = (theme.muted_foreground, theme.mono_font_family.clone());
        let Some(path) = path else {
            return h_flex()
                .text_color(muted)
                .child("Settings are not saved in this session.")
                .into_any_element();
        };
        let shown: SharedString = path.display().to_string().into();
        let reveal = path.clone();
        h_flex()
            .w_full()
            .gap_2()
            .flex_wrap()
            .child(
                gpui::div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_family(mono)
                    .text_color(muted)
                    .child(shown),
            )
            .child(
                Button::new("open-settings-file")
                    .label("Open")
                    .small()
                    .on_click(|_, _, cx| open_file(cx)),
            )
            .child(
                Button::new("reveal-settings-file")
                    .label("Show in folder")
                    .small()
                    .ghost()
                    .on_click(move |_, _, cx| cx.reveal_path(&reveal)),
            )
            .into_any_element()
    })
    .keywords(["file", "toml", "config"])
}

/// Opens the settings file in the default editor, creating it first if
/// nothing was saved yet.
fn open_file(cx: &mut App) {
    let Some(path) = cx.global::<Appearance>().file_path().map(ToOwned::to_owned) else {
        return;
    };
    let file = ferry_core::settings::SettingsFile::new(path.clone());
    let creating = crate::runtime::spawn(cx, async move {
        tokio::task::spawn_blocking(move || file.create_if_missing()).await
    });
    cx.spawn(async move |cx| match creating.await {
        Ok(Ok(Ok(()))) => cx.update(|cx| cx.open_with_system(&path)),
        Ok(Ok(Err(error))) => tracing::warn!(%error, "could not create the settings file"),
        _ => {}
    })
    .detach();
}
