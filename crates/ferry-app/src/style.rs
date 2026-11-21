//! The design language of the window, in one place.
//!
//! Surfaces are flat; what belongs together sits in a rounded group: a
//! segmented track for controls, a framed group for lists of items. Sizes are
//! in `rem`, which the theme sets to the interface font size, so the whole
//! window follows the font size setting. Radii are fixed pixels: they should
//! not grow into pills on large text.

use gpui::{Div, Hsla, ParentElement as _, Pixels, Rems, Styled, px, rems};
use gpui_component::{ActiveTheme as _, Theme, h_flex};

/// Radius of single elements: rows, buttons, fields.
pub(crate) const RADIUS: Pixels = px(6.);
/// Radius of a segmented track around controls.
pub(crate) const TRACK_RADIUS: Pixels = px(8.);
/// Radius of a group of list items.
pub(crate) const GROUP_RADIUS: Pixels = px(10.);

/// Height of one row in the file list.
pub(crate) const ROW_HEIGHT: Rems = rems(2.);
/// Height of the toolbar.
pub(crate) const TOOLBAR_HEIGHT: Rems = rems(3.);
/// Height of a pane header; both panes line up.
pub(crate) const HEADER_HEIGHT: Rems = rems(2.5);

/// The text sizes. Body text is the theme's font size itself.
pub(crate) trait TypeScale: Styled + Sized {
    /// Names, values, everything people read first.
    fn text_body(self) -> Self {
        self.text_size(rems(1.))
    }

    /// Details next to body text: sizes, dates, addresses.
    fn text_small(self) -> Self {
        self.text_size(rems(0.92))
    }

    /// Column headers and section titles.
    fn text_caption(self) -> Self {
        self.text_size(rems(0.85))
    }
}

impl<E: Styled> TypeScale for E {}

/// Colors that the theme does not name directly.
pub(crate) trait Palette {
    /// Behind a row under the pointer.
    fn hover_fill(&self) -> Hsla;
    /// Inside a group of items, set off from the window background.
    fn group_fill(&self) -> Hsla;
    /// The track of a segmented control.
    fn track_fill(&self) -> Hsla;
    /// Selected rows in a pane that does not have the keyboard.
    fn inactive_selection(&self) -> Hsla;
    /// The chosen segment of a track, lifted above it.
    fn raised_fill(&self) -> Hsla;
}

impl Palette for Theme {
    fn hover_fill(&self) -> Hsla {
        self.foreground.opacity(0.05)
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
            .opacity(if self.is_dark() { 0.06 } else { 0.05 })
    }

    fn inactive_selection(&self) -> Hsla {
        self.foreground.opacity(0.08)
    }

    fn raised_fill(&self) -> Hsla {
        if self.is_dark() {
            self.foreground.opacity(0.12)
        } else {
            self.background
        }
    }
}

/// A segmented track that holds related controls, such as back and forward.
pub(crate) fn track(cx: &gpui::App) -> Div {
    h_flex()
        .flex_shrink_0()
        .p(px(2.))
        .gap(px(2.))
        .rounded(TRACK_RADIUS)
        .bg(cx.theme().track_fill())
}

/// A framed group of items. Items separate themselves with
/// [`divided`]; the group clips them to its rounded corners.
pub(crate) fn group(cx: &gpui::App) -> Div {
    let theme = cx.theme();
    gpui::div()
        .flex()
        .flex_col()
        .rounded(GROUP_RADIUS)
        .border_1()
        .border_color(theme.border)
        .bg(theme.group_fill())
        .overflow_hidden()
}

/// Draws the hairline between an item and the one before it in a group.
pub(crate) fn divided<E: Styled>(item: E, index: usize, cx: &gpui::App) -> E {
    if index == 0 {
        item
    } else {
        item.border_t_1().border_color(cx.theme().border)
    }
}

/// A section title above a group.
pub(crate) fn section_title(title: &'static str, cx: &gpui::App) -> Div {
    gpui::div()
        .px_1()
        .pb_1()
        .text_caption()
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(cx.theme().muted_foreground)
        .child(title)
}
