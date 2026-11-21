//! Spreadsheets in the preview: a grid with row numbers and column letters,
//! and a tab for each sheet.

use std::sync::Arc;

use ferge_core::document::sheet::{COLUMN_LIMIT, ROW_LIMIT, Sheet};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, Context, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    ScrollHandle, SharedString, StatefulInteractiveElement as _, Styled as _,
    UniformListScrollHandle, div, uniform_list,
};
use gpui_base::SelectableText;
use gpui_component::{ActiveTheme as _, h_flex, v_flex};

use super::Preview;
use crate::format;
use crate::style::{self, ControlSize, Palette as _, TypeScale as _, em};

/// Rows looked at to size the columns.
const SAMPLE_ROWS: usize = 400;
/// Width of the row numbers, in em.
const ROW_HEADER: f32 = 3.5;
/// Height of a row.
const ROW_HEIGHT: f32 = 1.75;

/// A sheet ready to show: its cells as shared strings and column widths.
pub(super) struct Grid {
    name: SharedString,
    rows: Arc<[Arc<[SharedString]>]>,
    /// Width of each column, in em.
    widths: Arc<[f32]>,
    truncated: bool,
}

impl Grid {
    /// Prepares `sheet` for showing; run it off the UI thread.
    pub(super) fn new(sheet: Sheet) -> Self {
        let columns = sheet.columns();
        let mut chars = vec![0_usize; columns];
        for row in sheet.rows.iter().take(SAMPLE_ROWS) {
            for (column, cell) in row.iter().enumerate() {
                let width = cell.lines().next().map_or(0, |line| line.chars().count());
                chars[column] = chars[column].max(width);
            }
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "character counts of a cell are small"
        )]
        let widths = chars
            .iter()
            .map(|&count| (count as f32).mul_add(0.55, 1.25).clamp(3.5, 22.0))
            .collect();
        Self {
            name: sheet.name.into(),
            rows: sheet
                .rows
                .into_iter()
                .map(|row| row.into_iter().map(SharedString::from).collect())
                .collect(),
            widths,
            truncated: sheet.truncated,
        }
    }
}

pub(super) struct SheetView {
    grids: Vec<Grid>,
    active: usize,
    scroll: UniformListScrollHandle,
    horizontal: ScrollHandle,
}

impl SheetView {
    pub(super) fn new(grids: Vec<Grid>) -> Self {
        Self {
            grids,
            active: 0,
            scroll: UniformListScrollHandle::new(),
            horizontal: ScrollHandle::new(),
        }
    }

    fn select(&mut self, index: usize) {
        if index != self.active && index < self.grids.len() {
            self.active = index;
            self.scroll = UniformListScrollHandle::new();
            self.horizontal = ScrollHandle::new();
        }
    }

    pub(super) fn render(&self, cx: &mut Context<Preview>) -> AnyElement {
        let Some(grid) = self.grids.get(self.active) else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        let colors = Colors {
            muted: theme.muted_foreground,
            line: theme.border.opacity(0.6),
            header: theme.group_fill(),
        };
        let body = if grid.rows.is_empty() {
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .text_color(colors.muted)
                .child("This sheet is empty.")
                .into_any_element()
        } else {
            render_grid(grid, &self.scroll, &self.horizontal, colors)
        };
        let note = grid.truncated.then(|| {
            div()
                .px(style::BAR_PADDING)
                .py_1()
                .text_small()
                .text_color(colors.muted)
                .child(format!(
                    "Showing the first {} rows and {} columns.",
                    format::count(ROW_LIMIT),
                    format::count(COLUMN_LIMIT)
                ))
        });
        v_flex()
            .flex_1()
            .min_h_0()
            .child(body)
            .children(note)
            .children(self.render_tabs(cx))
            .into_any_element()
    }

    /// A tab for each sheet, when there is more than one.
    fn render_tabs(&self, cx: &mut Context<Preview>) -> Option<impl IntoElement> {
        if self.grids.len() < 2 {
            return None;
        }
        let choices: Vec<(usize, SharedString)> = self
            .grids
            .iter()
            .enumerate()
            .map(|(index, grid)| {
                let name = if grid.name.is_empty() {
                    SharedString::from(format!("Sheet {}", index + 1))
                } else {
                    grid.name.clone()
                };
                (index, name)
            })
            .collect();
        let this = cx.entity().downgrade();
        Some(
            h_flex()
                .id("sheet-tabs")
                .flex_shrink_0()
                .overflow_x_scroll()
                .px(style::BAR_PADDING)
                .py(em(0.375))
                .border_t_1()
                .border_color(cx.theme().border)
                .child(style::segmented(
                    "sheets",
                    ControlSize::Compact,
                    &choices,
                    self.active,
                    move |index, _, cx| {
                        let _ = this.update(cx, |preview, cx| {
                            if let Some(view) = preview.sheets_mut() {
                                view.select(index);
                                cx.notify();
                            }
                        });
                    },
                    cx,
                )),
        )
    }
}

#[derive(Clone, Copy)]
struct Colors {
    muted: Hsla,
    line: Hsla,
    header: Hsla,
}

fn render_grid(
    grid: &Grid,
    scroll: &UniformListScrollHandle,
    horizontal: &ScrollHandle,
    colors: Colors,
) -> AnyElement {
    let widths = Arc::clone(&grid.widths);
    let total = ROW_HEADER + widths.iter().sum::<f32>();
    let header = h_flex()
        .flex_shrink_0()
        .h(em(ROW_HEIGHT))
        .bg(colors.header)
        .border_b_1()
        .border_color(colors.line)
        .text_small()
        .text_color(colors.muted)
        .child(cell(ROW_HEADER, colors.line).into_any_element())
        .children(widths.iter().enumerate().map(|(index, &width)| {
            cell(width, colors.line)
                .justify_center()
                .child(column_name(index))
                .into_any_element()
        }));
    let rows = Arc::clone(&grid.rows);
    let list = uniform_list("sheet-rows", rows.len(), move |range, _, _: &mut App| {
        range
            .map(|index| {
                let cells = &rows[index];
                h_flex()
                    .h(em(ROW_HEIGHT))
                    .border_b_1()
                    .border_color(colors.line)
                    .child(
                        cell(ROW_HEADER, colors.line)
                            .justify_end()
                            .bg(colors.header)
                            .text_small()
                            .text_color(colors.muted)
                            .child((index + 1).to_string()),
                    )
                    .children(widths.iter().enumerate().map(|(column, &width)| {
                        let text = cells.get(column).cloned().unwrap_or_default();
                        let numeric = is_number(&text);
                        cell(width, colors.line)
                            .when(numeric, gpui::Styled::justify_end)
                            .child(
                                div().min_w_0().truncate().child(
                                    SelectableText::new(
                                        ("preview-cell", index * COLUMN_LIMIT + column),
                                        text,
                                    )
                                    .document_order((index * COLUMN_LIMIT + column) as u64),
                                ),
                            )
                    }))
            })
            .collect()
    })
    .track_scroll(scroll)
    .flex_1();
    div()
        .id("sheet-grid")
        .flex_1()
        .min_h_0()
        .overflow_x_scroll()
        .track_scroll(horizontal)
        .child(
            v_flex()
                .w(em(total))
                .h_full()
                .whitespace_nowrap()
                .child(header)
                .child(list),
        )
        .into_any_element()
}

/// A cell `width` em wide with a line on its right.
fn cell(width: f32, line: Hsla) -> gpui::Div {
    h_flex()
        .w(em(width))
        .flex_shrink_0()
        .h_full()
        .px(em(0.375))
        .overflow_hidden()
        .border_r_1()
        .border_color(line)
}

/// The letters of a column: A … Z, AA, AB …
fn column_name(index: usize) -> String {
    let mut name = Vec::new();
    let mut rest = index + 1;
    while rest > 0 {
        let letter = u8::try_from((rest - 1) % 26).unwrap_or(0);
        name.push(char::from(b'A' + letter));
        rest = (rest - 1) / 26;
    }
    name.iter().rev().collect()
}

/// Whether a cell holds a plain number, shown flush right.
fn is_number(text: &str) -> bool {
    !text.is_empty() && text.trim().parse::<f64>().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_columns_like_spreadsheets() {
        assert_eq!(column_name(0), "A");
        assert_eq!(column_name(25), "Z");
        assert_eq!(column_name(26), "AA");
        assert_eq!(column_name(27), "AB");
        assert_eq!(column_name(701), "ZZ");
        assert_eq!(column_name(702), "AAA");
    }

    #[test]
    fn sizes_columns_by_their_content() {
        let grid = Grid::new(Sheet {
            name: "S".to_owned(),
            rows: vec![
                vec!["a".to_owned(), "x".repeat(200)],
                vec![String::new(), "y".to_owned()],
            ],
            truncated: false,
        });
        assert_eq!(grid.widths.len(), 2);
        assert!(grid.widths[0] < grid.widths[1]);
        assert!(grid.widths[1] <= 22.0);
    }
}
