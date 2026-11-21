//! The design system of the window. Every size, radius and text size of
//! Ferry's own views comes from here; `design_rules` in the tests rejects
//! views that bypass it.
//!
//! # Scale
//!
//! Sizes are multiples of the body text size ([`em`]). gpui-component draws
//! its menus, fields and buttons with `text_sm` (0.875 rem), so the theme's
//! rem is set to body size / 0.875 ([`rem_for_body`]); that way library
//! controls and Ferry's own text are exactly the same size, and everything
//! grows and shrinks with the font size setting.
//!
//! # Shapes
//!
//! Surfaces are flat. Related controls sit in a [`track`]; lists of items
//! sit in a [`group`]. A track's corner radius is the radius of the items in
//! it plus its padding, so the corners run parallel.

use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, ClickEvent, Div, ElementId, Hsla, InteractiveElement, Interactivity, IntoElement,
    ParentElement as _, Pixels, Rems, RenderOnce, SharedString, Stateful,
    StatefulInteractiveElement as _, StyleRefinement, Styled, Window, div, px, rems,
};
use gpui_component::menu::DropdownMenu;
use gpui_component::tooltip::Tooltip;
use gpui_component::{ActiveTheme as _, Icon, Selectable, Theme, h_flex};

/// Body text in rem, as gpui-component's `text_sm`.
const BODY_REMS: f32 = 0.875;

/// The theme font size (one rem) that makes body text `body` pixels.
pub(crate) fn rem_for_body(body: f32) -> f32 {
    body / BODY_REMS
}

/// `x` times the body text size.
pub(crate) const fn em(x: f32) -> Rems {
    rems(x * BODY_REMS)
}

/// Radius of single elements: rows, buttons, fields, segments.
pub(crate) const RADIUS: Pixels = px(6.);
/// Radius of a group of list items and of banners.
pub(crate) const GROUP_RADIUS: Pixels = px(10.);

/// Height of one row in the file list.
pub(crate) const ROW_HEIGHT: Rems = em(2.);
/// Height of the window toolbar.
pub(crate) const TOOLBAR_HEIGHT: Rems = em(3.25);
/// Height of a pane header; both panes line up.
pub(crate) const HEADER_HEIGHT: Rems = em(2.75);
/// Horizontal padding of bars (toolbar, headers, status lines) and panels.
pub(crate) const BAR_PADDING: Rems = em(0.625);

/// The two sizes of controls.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ControlSize {
    /// In the window toolbar.
    #[default]
    Regular,
    /// In pane headers, rows and panels.
    Compact,
}

impl ControlSize {
    /// Height of a control on its own, and of a track around controls.
    pub(crate) const fn height(self) -> Rems {
        match self {
            Self::Regular => em(2.25),
            Self::Compact => em(1.875),
        }
    }

    /// Height of one item inside a track.
    pub(crate) const fn item(self) -> Rems {
        match self {
            Self::Regular => em(1.8),
            Self::Compact => em(1.5),
        }
    }

    /// Padding of a track: (height − item) / 2.
    const fn padding(self) -> Rems {
        match self {
            Self::Regular => em(0.225),
            Self::Compact => em(0.1875),
        }
    }

    pub(crate) const fn icon(self) -> Rems {
        match self {
            Self::Regular => em(1.125),
            Self::Compact => em(1.),
        }
    }

    /// A track's radius continues its items' radius around the padding.
    pub(crate) const fn track_radius(self) -> Pixels {
        match self {
            Self::Regular => px(9.),
            Self::Compact => px(8.5),
        }
    }
}

/// The text sizes. Body text matches gpui-component's controls.
pub(crate) trait TypeScale: Styled + Sized {
    /// Names, values, everything people read first.
    fn text_body(self) -> Self {
        self.text_size(em(1.))
    }

    /// Details next to body text (sizes, dates, addresses), column headers
    /// and section titles; gpui-component's `text_xs`.
    fn text_small(self) -> Self {
        self.text_size(rems(0.75))
    }

    /// Titles of panels and dialogs.
    fn text_title(self) -> Self {
        self.text_size(em(1.075))
            .font_weight(gpui::FontWeight::SEMIBOLD)
    }

    /// Headings inside a previewed document, level 1 the largest. Only
    /// content uses these; the interface itself has no headings above
    /// [`Self::text_title`].
    fn text_heading(self, level: u8) -> Self {
        let size = match level {
            0 | 1 => 1.5,
            2 => 1.25,
            3 => 1.1,
            _ => 1.,
        };
        self.text_size(em(size))
            .line_height(em(size * 1.3))
            .font_weight(if level <= 2 {
                gpui::FontWeight::BOLD
            } else {
                gpui::FontWeight::SEMIBOLD
            })
    }
}

impl<E: Styled> TypeScale for E {}

/// Colors that the theme does not name directly.
pub(crate) trait Palette {
    /// Behind an item under the pointer.
    fn hover_fill(&self) -> Hsla;
    /// Inside a group of items, set off from the window background.
    fn group_fill(&self) -> Hsla;
    /// The track of a segmented control, and fields in bars.
    fn track_fill(&self) -> Hsla;
    /// Selected rows in a pane that does not have the keyboard.
    fn inactive_selection(&self) -> Hsla;
    /// The chosen item of a track, lifted above it.
    fn raised_fill(&self) -> Hsla;
    /// Text and icons that say a pane works as root.
    fn root_accent(&self) -> Hsla;
    /// Behind a pane header that works as root, and notes that offer it.
    fn root_fill(&self) -> Hsla;
}

impl Palette for Theme {
    fn hover_fill(&self) -> Hsla {
        self.foreground
            .opacity(if self.is_dark() { 0.07 } else { 0.05 })
    }

    fn group_fill(&self) -> Hsla {
        if self.is_dark() {
            self.foreground.opacity(0.03)
        } else {
            self.background
        }
    }

    fn track_fill(&self) -> Hsla {
        self.foreground
            .opacity(if self.is_dark() { 0.07 } else { 0.05 })
    }

    fn inactive_selection(&self) -> Hsla {
        self.foreground.opacity(0.08)
    }

    fn raised_fill(&self) -> Hsla {
        if self.is_dark() {
            self.foreground.opacity(0.14)
        } else {
            self.background
        }
    }

    fn root_accent(&self) -> Hsla {
        if self.is_dark() {
            self.warning
        } else {
            // Amber text on a light background needs to be darker to read.
            Hsla {
                l: self.warning.l * 0.55,
                ..self.warning
            }
        }
    }

    fn root_fill(&self) -> Hsla {
        self.warning
            .opacity(if self.is_dark() { 0.1 } else { 0.14 })
    }
}

/// A track that holds related controls, such as back and forward.
pub(crate) fn track(size: ControlSize, cx: &App) -> Div {
    h_flex()
        .flex_shrink_0()
        .h(size.height())
        .p(size.padding())
        .gap(px(2.))
        .rounded(size.track_radius())
        .bg(cx.theme().track_fill())
}

/// Makes a gpui-component field fit a bar: the height of a track, its
/// corners and its fill.
pub(crate) fn field<E: Styled>(field: E, size: ControlSize, cx: &App) -> E {
    field
        .h(size.height())
        .rounded(size.track_radius())
        .bg(cx.theme().track_fill())
        .border_color(gpui::transparent_black())
}

/// A framed group of items. Items separate themselves with [`divided`]; the
/// group clips them to its rounded corners.
pub(crate) fn group(cx: &App) -> Div {
    let theme = cx.theme();
    div()
        .flex()
        .flex_col()
        .rounded(GROUP_RADIUS)
        .border_1()
        .border_color(theme.border)
        .bg(theme.group_fill())
        .overflow_hidden()
}

/// Draws the hairline between an item and the one before it in a group.
pub(crate) fn divided<E: Styled>(item: E, index: usize, cx: &App) -> E {
    if index == 0 {
        item
    } else {
        item.border_t_1().border_color(cx.theme().border)
    }
}

/// A section title above a group.
pub(crate) fn section_title(title: &'static str, cx: &App) -> Div {
    div()
        .px_1()
        .pb_1p5()
        .text_small()
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(cx.theme().muted_foreground)
        .child(title)
}

type ClickHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

/// A square button with an icon, in tracks, bars and rows. Every icon-only
/// button of the window is one of these.
#[derive(IntoElement)]
pub(crate) struct IconButton {
    base: Stateful<Div>,
    icon: Icon,
    size: ControlSize,
    /// Inside a track: as tall as its items.
    in_track: bool,
    selected: bool,
    disabled: bool,
    tooltip: Option<SharedString>,
    on_click: Option<ClickHandler>,
}

impl IconButton {
    pub(crate) fn new(id: impl Into<ElementId>, icon: impl Into<Icon>) -> Self {
        Self {
            base: div().id(id),
            icon: icon.into(),
            size: ControlSize::Regular,
            in_track: false,
            selected: false,
            disabled: false,
            tooltip: None,
            on_click: None,
        }
    }

    pub(crate) fn size(mut self, size: ControlSize) -> Self {
        self.size = size;
        self
    }

    /// The button sits in a [`track`] of its size.
    pub(crate) fn in_track(mut self) -> Self {
        self.in_track = true;
        self
    }

    pub(crate) fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub(crate) fn tooltip(mut self, tooltip: impl Into<SharedString>) -> Self {
        self.tooltip = Some(tooltip.into());
        self
    }

    pub(crate) fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Rc::new(handler));
        self
    }
}

impl Selectable for IconButton {
    fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    fn is_selected(&self) -> bool {
        self.selected
    }
}

impl Styled for IconButton {
    fn style(&mut self) -> &mut StyleRefinement {
        self.base.style()
    }
}

impl InteractiveElement for IconButton {
    fn interactivity(&mut self) -> &mut Interactivity {
        self.base.interactivity()
    }
}

impl DropdownMenu for IconButton {}

impl RenderOnce for IconButton {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let (foreground, muted, hover, raised) = (
            theme.foreground,
            theme.muted_foreground,
            theme.hover_fill(),
            theme.raised_fill(),
        );
        let side = if self.in_track {
            self.size.item()
        } else {
            self.size.height()
        };
        let color = if self.selected { foreground } else { muted };
        let disabled = self.disabled;
        self.base
            .size(side)
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .rounded(RADIUS)
            .when(self.selected, |this| this.bg(raised).shadow_xs())
            .when(disabled, |this| this.opacity(0.4))
            .when(!disabled && !self.selected, |this| {
                this.cursor_pointer().hover(|style| style.bg(hover))
            })
            .when_some(self.tooltip, |this, tooltip| {
                this.tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
            })
            .when_some(self.on_click.filter(|_| !disabled), |this, handler| {
                this.cursor_pointer()
                    .on_click(move |event, window, cx| handler(event, window, cx))
            })
            .child(self.icon.size(self.size.icon()).text_color(color))
    }
}

/// A thin progress bar, `value` in percent.
pub(crate) fn progress(
    id: impl Into<ElementId>,
    value: f32,
    color: Hsla,
) -> gpui_component::progress::Progress {
    use gpui_component::Sizable as _;
    gpui_component::progress::Progress::new(id)
        .with_size(gpui_component::Size::XSmall)
        .color(color)
        .value(value)
}

/// A segmented control: one of a few choices, side by side in a track.
pub(crate) fn segmented<T: Copy + PartialEq + 'static>(
    id: &'static str,
    size: ControlSize,
    choices: &[(T, SharedString)],
    selected: T,
    on_select: impl Fn(T, &mut Window, &mut App) + 'static,
    cx: &App,
) -> Div {
    let theme = cx.theme();
    let (foreground, muted, raised) = (
        theme.foreground,
        theme.muted_foreground,
        theme.raised_fill(),
    );
    let on_select = Rc::new(on_select);
    track(size, cx).children(choices.iter().enumerate().map(|(index, (value, label))| {
        let (value, chosen) = (*value, *value == selected);
        let on_select = Rc::clone(&on_select);
        div()
            .id((id, index))
            .h(size.item())
            .px(em(0.75))
            .flex()
            .items_center()
            .justify_center()
            .rounded(RADIUS)
            .text_body()
            .whitespace_nowrap()
            .cursor_pointer()
            .map(|this| {
                if chosen {
                    this.bg(raised).shadow_xs().text_color(foreground)
                } else {
                    this.text_color(muted)
                        .hover(|style| style.text_color(foreground))
                }
            })
            .on_click(move |_, window, cx| on_select(value, window, cx))
            .child(label.clone())
    }))
}

#[cfg(test)]
mod design_rules {
    //! Views build on the design system instead of picking sizes ad hoc.
    //! Everything rejected here has an equivalent in `style.rs`.

    use std::path::{Path, PathBuf};

    /// Patterns that bypass the system, with what to use instead.
    const FORBIDDEN: &[(&str, &str)] = &[
        (".text_xs()", "text_small()"),
        (".text_sm()", "text_body()"),
        (".text_base()", "text_body()"),
        (".text_lg()", "text_title()"),
        ("rems(", "style::em(…)"),
        (".xsmall()", "the default size or .small()"),
        (".icon(IconName::", "style::IconButton for icon buttons"),
        (".icon(Icon::", "style::IconButton for icon buttons"),
        ("ButtonGroup", "style::segmented or style::track"),
        (".rounded_sm()", "style::RADIUS"),
        (".rounded_md()", "style::RADIUS"),
        (".rounded_lg()", "style::GROUP_RADIUS"),
        ("list_hover", "Palette::hover_fill"),
        ("list_even", "flat rows without stripes"),
    ];

    /// Exceptions, each with its reason.
    fn allowed(file: &str, pattern: &str) -> bool {
        // A settings page's icon is part of the page, not a button.
        file == "settings_window.rs" && pattern == ".icon(IconName::"
    }

    fn sources(dir: &Path, files: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                sources(&path, files);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                files.push(path);
            }
        }
    }

    #[test]
    fn views_use_the_design_system() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        sources(&root, &mut files);
        let mut violations = Vec::new();
        for file in files {
            let name = file.strip_prefix(&root).unwrap().display().to_string();
            if name == "style.rs" {
                continue;
            }
            let text = std::fs::read_to_string(&file).unwrap();
            for (number, line) in text.lines().enumerate() {
                for (pattern, instead) in FORBIDDEN {
                    if line.contains(pattern) && !allowed(&name, pattern) {
                        violations
                            .push(format!("{name}:{}: `{pattern}`, use {instead}", number + 1));
                    }
                }
            }
        }
        assert!(violations.is_empty(), "\n{}", violations.join("\n"));
    }
}
