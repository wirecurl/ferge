//! Rendering of the file browser. Only visible rows are built.

use std::ops::Range;

use ferry_core::EntryKind;
use ferry_core::listing::SortColumn;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, ClickEvent, Context, ElementId, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Window,
    div, px, uniform_list,
};
use gpui_component::{ActiveTheme as _, Icon, IconName, h_flex, v_flex};

use super::{FileBrowser, LoadState, ROW_HEIGHT};
use crate::{actions, format};

const SIZE_WIDTH: gpui::Pixels = px(96.);
const DATE_WIDTH: gpui::Pixels = px(136.);

impl Render for FileBrowser {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.flush_pending();
        v_flex()
            .size_full()
            .key_context(actions::FILE_LIST)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::on_select_previous))
            .on_action(cx.listener(Self::on_select_next))
            .on_action(cx.listener(Self::on_page_up))
            .on_action(cx.listener(Self::on_page_down))
            .on_action(cx.listener(Self::on_select_first))
            .on_action(cx.listener(Self::on_select_last))
            .on_action(cx.listener(Self::on_open))
            .on_action(cx.listener(Self::on_go_to_parent))
            .on_action(cx.listener(Self::on_go_back))
            .on_action(cx.listener(Self::on_go_forward))
            .on_action(cx.listener(Self::on_backspace))
            .on_action(cx.listener(Self::on_clear_filter))
            .on_action(cx.listener(Self::on_sort_by_name))
            .on_action(cx.listener(Self::on_sort_by_size))
            .on_action(cx.listener(Self::on_sort_by_modified))
            .on_action(cx.listener(Self::on_refresh))
            .on_key_down(cx.listener(Self::on_key_down))
            .child(self.render_path_bar(cx))
            .child(self.render_header(cx))
            .child(self.render_list(cx))
            .child(self.render_status(cx))
    }
}

impl FileBrowser {
    fn render_path_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let ancestors = self.path.ancestors_from_root();
        let last = ancestors.len().saturating_sub(1);
        let crumbs = ancestors.into_iter().enumerate().map(|(index, path)| {
            let label: SharedString = path
                .file_name()
                .map_or_else(|| "/".into(), |name| name.to_owned().into());
            let is_current = index == last;
            h_flex()
                .gap_1()
                .when(index > 1, |this| {
                    this.child(div().text_color(muted).child("/"))
                })
                .child(
                    div()
                        .id(ElementId::NamedInteger("crumb".into(), index as u64))
                        .px_1()
                        .rounded_sm()
                        .when(is_current, |this| {
                            this.font_weight(gpui::FontWeight::SEMIBOLD)
                        })
                        .when(!is_current, |this| {
                            this.cursor_pointer()
                                .text_color(muted)
                                .hover(|style| style.bg(cx.theme().list_hover))
                        })
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.go_to(path.clone(), cx);
                        }))
                        .child(label),
                )
        });
        h_flex()
            .h(px(32.))
            .px_3()
            .gap_1()
            .flex_shrink_0()
            .overflow_x_hidden()
            .text_sm()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .text_color(muted)
                    .pr_2()
                    .child(SharedString::from(self.remote.label().to_owned())),
            )
            .children(crumbs)
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let sort = self.view.sort_order();
        let column = |id: &'static str, label: &'static str, sort_column: SortColumn| {
            let arrow = (sort.column == sort_column).then(|| {
                Icon::new(if sort.descending {
                    IconName::ArrowDown
                } else {
                    IconName::ArrowUp
                })
                .size_3()
            });
            h_flex()
                .id(id)
                .gap_1()
                .cursor_pointer()
                .hover(|style| style.text_color(cx.theme().foreground))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.sort_by(sort_column, cx);
                }))
                .child(label)
                .children(arrow)
        };
        h_flex()
            .h(px(28.))
            .px_3()
            .gap_2()
            .flex_shrink_0()
            .text_xs()
            .text_color(theme.muted_foreground)
            .bg(theme.list_head)
            .border_b_1()
            .border_color(theme.border)
            .child(div().w(px(16.)))
            .child(
                div()
                    .flex_1()
                    .child(column("sort-name", "Name", SortColumn::Name)),
            )
            .child(h_flex().w(SIZE_WIDTH).justify_end().child(column(
                "sort-size",
                "Size",
                SortColumn::Size,
            )))
            .child(div().w(DATE_WIDTH).child(column(
                "sort-modified",
                "Modified",
                SortColumn::Modified,
            )))
    }

    fn render_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        if self.view.is_empty() {
            let message: SharedString = match &self.state {
                LoadState::Loading | LoadState::Revalidating => "Loading…".into(),
                LoadState::Failed(error) => error.clone().into(),
                LoadState::Complete if self.filter.is_empty() => "This folder is empty".into(),
                LoadState::Complete => format!("Nothing matches “{}”", self.filter).into(),
            };
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(message)
                .into_any_element();
        }
        div()
            .flex_1()
            .min_h_0()
            .child(
                uniform_list(
                    "entries",
                    self.view.len(),
                    cx.processor(|this, range: Range<usize>, _window, cx| {
                        this.render_rows(range, cx)
                    }),
                )
                .track_scroll(&self.scroll)
                .size_full(),
            )
            .into_any_element()
    }

    fn render_rows(&self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let theme = cx.theme();
        let (active, even, hover) = (theme.list_active, theme.list_even, theme.list_hover);
        let (muted, border) = (theme.muted_foreground, theme.list_active_border);
        range
            .filter_map(|index| {
                let entry = self.view.get(index)?;
                let selected = self.selected == Some(index);
                let (icon, size) = match entry.kind() {
                    EntryKind::Directory => (IconName::Folder, String::new()),
                    EntryKind::Symlink => (IconName::ExternalLink, String::new()),
                    EntryKind::File => (IconName::File, format::size(entry.metadata.size)),
                    EntryKind::Other => (IconName::FileText, String::new()),
                };
                let row = h_flex()
                    .id(index)
                    .w_full()
                    .h(ROW_HEIGHT)
                    .px_3()
                    .gap_2()
                    .text_sm()
                    // Every row reserves the selection border so content never shifts.
                    .border_l_2()
                    .border_color(gpui::transparent_black())
                    .when(selected, |this| this.bg(active).border_color(border))
                    .when(!selected && index % 2 == 1, |this| this.bg(even))
                    .when(!selected, |this| this.hover(|style| style.bg(hover)))
                    .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                        this.select(index, cx);
                        if event.click_count() >= 2 {
                            this.open_index(index, cx);
                        }
                    }))
                    .child(Icon::new(icon).size_4().text_color(muted))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_x_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(SharedString::from(entry.name.as_str().to_owned())),
                    )
                    .child(
                        div()
                            .w(SIZE_WIDTH)
                            .text_right()
                            .text_color(muted)
                            .child(size),
                    )
                    .child(
                        div()
                            .w(DATE_WIDTH)
                            .text_color(muted)
                            .child(format::date(entry.metadata.modified)),
                    );
                Some(row.into_any_element())
            })
            .collect()
    }

    fn render_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let total = self.view.total_len();
        let count = if self.filter.is_empty() {
            format!("{total} items")
        } else {
            format!("{} of {total} items", self.view.len())
        };
        let state: Option<SharedString> = match &self.state {
            LoadState::Loading => Some("Loading…".into()),
            LoadState::Revalidating => Some("Updating…".into()),
            LoadState::Complete => None,
            LoadState::Failed(error) => Some(error.clone().into()),
        };
        let failed = matches!(self.state, LoadState::Failed(_));
        h_flex()
            .h(px(26.))
            .px_3()
            .gap_3()
            .flex_shrink_0()
            .text_xs()
            .text_color(theme.muted_foreground)
            .border_t_1()
            .border_color(theme.border)
            .child(count)
            .when(!self.filter.is_empty(), |this| {
                this.child(
                    h_flex()
                        .gap_1()
                        .px_2()
                        .rounded_sm()
                        .bg(theme.accent)
                        .text_color(theme.accent_foreground)
                        .child(Icon::new(IconName::Search).size_3())
                        .child(SharedString::from(self.filter.clone())),
                )
                .child("Esc clears")
            })
            .child(div().flex_1())
            .children(state.map(|state| {
                div()
                    .when(failed, |this| this.text_color(theme.danger))
                    .child(state)
            }))
    }
}
