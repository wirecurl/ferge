//! Word processor and presentation files in the preview, as readable text:
//! headings, paragraphs, lists, tables and slides.

use std::sync::Arc;

use ferry_core::document::text::{Block, Document};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, IntoElement, ListAlignment, ListState, ParentElement as _, SharedString,
    Styled as _, div, list, px,
};
use gpui_component::{ActiveTheme as _, h_flex, v_flex};

use crate::format;
use crate::style::{self, TypeScale as _, em};

/// How wide text runs at most, for comfortable reading.
const MEASURE: f32 = 42.0;

pub(super) struct DocumentView {
    blocks: Arc<[Block]>,
    truncated: bool,
    list: ListState,
}

impl DocumentView {
    pub(super) fn new(document: Document) -> Self {
        let list = ListState::new(document.blocks.len(), ListAlignment::Top, px(800.));
        Self {
            blocks: document.blocks.into(),
            truncated: document.truncated,
            list,
        }
    }

    pub(super) fn render(&self, cx: &App) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        if self.blocks.is_empty() {
            return v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .text_color(muted)
                .child("This document has no text.")
                .into_any_element();
        }
        let blocks = Arc::clone(&self.blocks);
        let last = blocks.len() - 1;
        let truncated = self.truncated;
        let content = list(self.list.clone(), move |index, _, cx| {
            let block = render_block(&blocks[index], index == 0, cx);
            let end = (index == last && truncated).then(|| {
                div()
                    .pt(em(1.))
                    .text_small()
                    .text_color(cx.theme().muted_foreground)
                    .child("The document goes on; the preview ends here.")
            });
            // A centered column, with room at the ends of the document.
            h_flex()
                .w_full()
                .justify_center()
                .px(style::BAR_PADDING)
                .pt(if index == 0 { em(1.25) } else { em(0.) })
                .pb(if index == last { em(1.5) } else { em(0.) })
                .child(
                    v_flex()
                        .w_full()
                        .max_w(em(MEASURE))
                        .child(block)
                        .children(end),
                )
                .into_any_element()
        })
        .flex_1()
        .min_h_0();
        content.into_any_element()
    }
}

fn render_block(block: &Block, first: bool, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let (muted, border) = (theme.muted_foreground, theme.border);
    match block {
        Block::Heading { level, text } => div()
            .pt(if first { em(0.) } else { em(1.) })
            .pb(em(0.375))
            .text_heading(*level)
            .child(SharedString::from(text.clone()))
            .into_any_element(),
        Block::Paragraph(text) => div()
            .pb(em(0.625))
            .line_height(em(1.5))
            .child(SharedString::from(text.clone()))
            .into_any_element(),
        Block::Item { depth, text } => h_flex()
            .items_start()
            .pb(em(0.25))
            .pl(em(1.25 * f32::from(*depth)))
            .line_height(em(1.5))
            .child(
                div()
                    .w(em(1.25))
                    .flex_shrink_0()
                    .text_color(muted)
                    .child("•"),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(SharedString::from(text.clone())),
            )
            .into_any_element(),
        Block::Table(rows) => {
            let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
            style::group(cx)
                .my(em(0.5))
                .text_small()
                .children(rows.iter().enumerate().map(|(index, row)| {
                    style::divided(h_flex().items_start(), index, cx).children((0..columns).map(
                        |column| {
                            div()
                                .flex_1()
                                .min_w_0()
                                .px(em(0.5))
                                .py(em(0.375))
                                // Cells after the first are set off by a line.
                                .when(column > 0, |cell| cell.border_l_1().border_color(border))
                                .child(SharedString::from(
                                    row.get(column).cloned().unwrap_or_default(),
                                ))
                        },
                    ))
                }))
                .into_any_element()
        }
        Block::Slide(number) => h_flex()
            .pt(if first { em(0.) } else { em(1.5) })
            .pb(em(0.5))
            .gap_2()
            .text_small()
            .text_color(muted)
            .child(format!("Slide {}", format::count(*number)))
            .child(div().flex_1().h(px(1.)).bg(border))
            .into_any_element(),
    }
}
