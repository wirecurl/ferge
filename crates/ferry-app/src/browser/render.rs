//! Rendering of the file browser. Only visible rows are built.

use std::fmt::Write as _;
use std::ops::Range;

use ferry_core::listing::SortColumn;
use ferry_core::{EntryKind, RemotePath};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, AppContext as _, ClickEvent, Context, ExternalDragPayload, ExternalPaths,
    InteractiveElement as _, IntoElement, ParentElement as _, Rems, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, uniform_list,
};
use gpui_component::input::Input;
use gpui_component::menu::{ContextMenu, ContextMenuExt as _};
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};

use super::{DraggedEntries, FileBrowser, FileClipboard, LoadState};
use crate::file_type::FileType;
use crate::style::{self, Palette as _, TypeScale as _, em};
use crate::{actions, format};

const SIZE_WIDTH: Rems = em(5.5);
const DATE_WIDTH: Rems = em(9.5);
const KIND_WIDTH: Rems = em(8.);
const PERMISSIONS_WIDTH: Rems = em(6.5);
/// Room for the icon in front of the name.
const ICON_WIDTH: Rems = em(1.);
/// Inset of the rows from the edges of the pane; rows are rounded inside it.
const LIST_INSET: Rems = em(0.375);
/// Space between the edge of a row and its content.
const ROW_PADDING: Rems = em(0.5);
/// [`LIST_INSET`] and [`ROW_PADDING`]: where row contents begin.
const HEADER_INSET: Rems = em(0.875);

impl Render for FileBrowser {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.flush_pending();
        if self.rename_when_listed.is_some() {
            self.rename_new_entry(window, cx);
        }
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
            .on_action(cx.listener(Self::on_select_previous_extend))
            .on_action(cx.listener(Self::on_select_next_extend))
            .on_action(cx.listener(Self::on_select_all))
            .on_action(cx.listener(Self::on_copy))
            .on_action(cx.listener(Self::on_paste))
            .on_action(cx.listener(Self::on_new_folder))
            .on_action(cx.listener(Self::on_new_file))
            .on_action(cx.listener(Self::on_rename))
            .on_action(cx.listener(Self::on_commit_rename))
            .on_action(cx.listener(Self::on_cancel_rename))
            .on_action(cx.listener(Self::on_delete))
            .on_action(cx.listener(Self::on_duplicate))
            .on_action(cx.listener(Self::on_copy_address))
            .on_key_down(cx.listener(Self::on_key_down))
            // Clicking anywhere in the browser makes it the keyboard target.
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseDownEvent, window, cx| {
                    window.focus(&this.focus, cx);
                }),
            )
            .child(self.render_header(cx))
            .child(self.render_list(cx))
            .child(self.render_status(cx))
    }
}

impl FileBrowser {
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
                .size(em(0.75))
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
            .h(em(1.875))
            // Lines the titles up with the row contents below.
            .px(HEADER_INSET)
            .gap_2()
            .flex_shrink_0()
            .text_small()
            .text_color(theme.muted_foreground)
            .child(div().w(ICON_WIDTH))
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
            .child(div().w(DATE_WIDTH).pl_3().child(column(
                "sort-modified",
                "Modified",
                SortColumn::Modified,
            )))
            .when(self.options.show_kind, |this| {
                this.child(div().w(KIND_WIDTH).pl_3().child("Kind"))
            })
            .when(self.options.show_permissions, |this| {
                this.child(div().w(PERMISSIONS_WIDTH).pl_3().child("Permissions"))
            })
    }

    fn render_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        if self.view.is_empty() {
            let message: SharedString = match &self.state {
                LoadState::Loading | LoadState::Revalidating => "Loading…".into(),
                LoadState::Failed(error) => error.clone().into(),
                LoadState::Complete if self.filter.is_empty() => "This folder is empty".into(),
                LoadState::Complete => format!("Nothing matches “{}”", self.filter).into(),
            };
            let empty = div()
                .id("empty-drop")
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_body()
                .text_color(cx.theme().muted_foreground);
            let empty = Self::accept_drops(empty, self.path.clone(), cx);
            return self
                .with_folder_menu(empty, cx)
                .child(message)
                .into_any_element();
        }
        let list = div().id("entries-drop").flex_1().min_h_0();
        let list = Self::accept_drops(list, self.path.clone(), cx);
        self.with_folder_menu(list, cx)
            .child(
                uniform_list(
                    "entries",
                    self.view.len(),
                    cx.processor(|this, range: Range<usize>, window, cx| {
                        let focused = this.focus.contains_focused(window, cx);
                        this.render_rows(range, focused, cx)
                    }),
                )
                .track_scroll(&self.scroll)
                .size_full()
                .px(LIST_INSET)
                .pb(LIST_INSET),
            )
            .into_any_element()
    }

    /// Rows `range` of the list. `focused`: the list has the keyboard, so
    /// its selection shows in the accent color.
    fn render_rows(
        &self,
        range: Range<usize>,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = cx.theme();
        let selection = if focused {
            theme.list_active
        } else {
            theme.inactive_selection()
        };
        let hover = theme.hover_fill();
        let (muted, ring) = (theme.muted_foreground, theme.list_active_border);
        let multiple = !self.marked.is_empty();
        let mono = theme.mono_font_family.clone();
        let this = cx.entity().downgrade();
        range
            .filter_map(|index| {
                let entry = self.view.get(index)?;
                let cursor = self.selected == Some(index);
                let marked = self.is_marked(&entry.name);
                let file_type = FileType::of(entry.name.as_str(), entry.kind());
                let icon_color = file_type.color(cx.theme());
                let size = if entry.kind() == EntryKind::File {
                    format::size(entry.metadata.size)
                } else {
                    String::new()
                };
                let dragged = DraggedEntries {
                    source: this.clone(),
                    grabbed: entry.name.clone(),
                    count: self.drag_count(&entry.name),
                };
                let is_dir = entry.kind() == EntryKind::Directory;
                let dir_path = self.path.join(&entry.name);
                // One row is selected by the cursor alone; with marked rows,
                // the cursor is a ring that moves among them.
                let selected = marked || (cursor && !multiple);
                let row = h_flex()
                    .id(index)
                    .w_full()
                    .h(style::ROW_HEIGHT)
                    .px(ROW_PADDING)
                    .gap_2()
                    .text_body()
                    // Columns cut their text short rather than wrap it.
                    .whitespace_nowrap()
                    .rounded(style::RADIUS)
                    // Every row reserves the ring so content never shifts.
                    .border_1()
                    .border_color(gpui::transparent_black())
                    .when(selected, |this| this.bg(selection))
                    .when(cursor && multiple && focused, |this| {
                        this.border_color(ring)
                    })
                    .when(!selected, |this| this.hover(|style| style.bg(hover)))
                    .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                        let modifiers = event.modifiers();
                        if modifiers.shift {
                            this.select_range(index, cx);
                        } else if modifiers.secondary(cx) {
                            this.toggle_mark(index, cx);
                        } else {
                            this.select(index, cx);
                            if event.click_count() >= 2 {
                                this.open_index(index, cx);
                            }
                        }
                    }))
                    .on_drag(dragged, |dragged, _, _, cx| {
                        let label: SharedString = match dragged.count {
                            1 => dragged.grabbed.as_str().to_owned().into(),
                            count => format!("{count} items").into(),
                        };
                        cx.new(|_| DragPreview { label })
                    })
                    .external_drag_payload(|dragged: &DraggedEntries, _, cx| {
                        let source = dragged.source.upgrade()?;
                        let paths = source.read(cx).drag_out_paths(&dragged.grabbed)?;
                        Some(ExternalDragPayload::Files(paths))
                    })
                    .when(is_dir, |this| Self::accept_drops(this, dir_path, cx))
                    .child(
                        Icon::new(file_type.icon())
                            .size(ICON_WIDTH)
                            .flex_shrink_0()
                            .text_color(icon_color),
                    )
                    .child(self.render_name(entry))
                    .map(|row| self.detail_columns(row, entry, size, muted, &mono))
                    // Right-clicking a row outside the selection selects it.
                    .on_mouse_down(
                        gpui::MouseButton::Right,
                        cx.listener(move |this, _: &gpui::MouseDownEvent, _, cx| {
                            this.select_for_menu(index, cx);
                        }),
                    )
                    .context_menu({
                        let focus = self.focus.clone();
                        move |menu, _, _| row_menu(menu, &focus, is_dir)
                    });
                Some(row.into_any_element())
            })
            .collect()
    }

    /// Makes a right-click on `element` that misses every row open the menu
    /// of the current folder. The selection is dropped first, so what the menu
    /// does applies to the folder rather than to rows that are out of sight.
    fn with_folder_menu(
        &self,
        element: gpui::Stateful<gpui::Div>,
        cx: &mut Context<Self>,
    ) -> ContextMenu<gpui::Stateful<gpui::Div>> {
        let focus = self.focus.clone();
        let show_hidden = self.options.show_hidden;
        element
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(|this, _: &gpui::MouseDownEvent, window, cx| {
                    window.focus(&this.focus, cx);
                    this.clear_selection(cx);
                }),
            )
            .context_menu(move |menu, _, cx| {
                let can_paste = cx.has_global::<FileClipboard>();
                folder_menu(menu, &focus, can_paste, show_hidden)
            })
    }

    /// Selects the row a context menu opens on, unless it belongs to the
    /// selection already.
    fn select_for_menu(&mut self, index: usize, cx: &mut Context<Self>) {
        let outside = self
            .view
            .get(index)
            .is_some_and(|entry| !self.is_marked(&entry.name));
        if outside && self.selected != Some(index) {
            self.select(index, cx);
        }
    }

    /// Size and date, then the kind and permission columns if they are
    /// switched on.
    fn detail_columns(
        &self,
        row: gpui::Stateful<gpui::Div>,
        entry: &ferry_core::Entry,
        size: String,
        muted: gpui::Hsla,
        mono: &SharedString,
    ) -> gpui::Stateful<gpui::Div> {
        row.child(
            div()
                .w(SIZE_WIDTH)
                .flex_shrink_0()
                .truncate()
                .text_right()
                .text_small()
                .text_color(muted)
                .child(size),
        )
        .child(
            div()
                .w(DATE_WIDTH)
                .flex_shrink_0()
                .pl_3()
                .truncate()
                .text_small()
                .text_color(muted)
                .child(format::date(entry.metadata.modified)),
        )
        .when(self.options.show_kind, |this| {
            this.child(
                div()
                    .w(KIND_WIDTH)
                    .flex_shrink_0()
                    .pl_3()
                    .text_small()
                    .truncate()
                    .text_color(muted)
                    .child(FileType::of(entry.name.as_str(), entry.kind()).label()),
            )
        })
        .when(self.options.show_permissions, |this| {
            this.child(
                div()
                    .w(PERMISSIONS_WIDTH)
                    .flex_shrink_0()
                    .pl_3()
                    .text_small()
                    .text_color(muted)
                    .font_family(mono.clone())
                    .child(
                        entry
                            .metadata
                            .permissions
                            .map(format::permissions)
                            .unwrap_or_default(),
                    ),
            )
        })
    }

    /// Makes `element` take dragged rows and files from other applications,
    /// copying them into `target_dir`; it lights up while something hovers.
    fn accept_drops(
        element: gpui::Stateful<gpui::Div>,
        target_dir: RemotePath,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let highlight = cx.theme().drop_target;
        let for_files = target_dir.clone();
        element
            .drag_over::<DraggedEntries>(move |style, _, _, _| style.bg(highlight))
            .drag_over::<ExternalPaths>(move |style, _, _, _| style.bg(highlight))
            .on_drop(cx.listener(move |this, dragged: &DraggedEntries, _, cx| {
                this.receive_drag(dragged, target_dir.clone(), cx);
            }))
            .on_drop(cx.listener(move |this, paths: &ExternalPaths, _, cx| {
                this.receive_external(paths, for_files.clone(), cx);
            }))
    }

    fn render_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (shown, hidden) = self.counts();
        let mut count = match (self.filter.is_empty(), shown) {
            (true, 1) => "1 item".to_owned(),
            (true, _) => format!("{} items", format::count(shown)),
            (false, _) => format!(
                "{} of {} items",
                format::count(shown),
                format::count(self.view.total_len())
            ),
        };
        if hidden > 0 {
            let _ = write!(count, " · {} hidden", format::count(hidden));
        }
        let state: Option<SharedString> = match &self.state {
            LoadState::Loading => Some("Loading…".into()),
            LoadState::Revalidating => Some("Updating…".into()),
            LoadState::Complete => None,
            LoadState::Failed(error) => Some(error.clone().into()),
        };
        let failed = matches!(self.state, LoadState::Failed(_));
        h_flex()
            .h(em(2.))
            .px(style::BAR_PADDING)
            .gap_3()
            .flex_shrink_0()
            .text_small()
            .text_color(theme.muted_foreground)
            .border_t_1()
            .border_color(theme.border)
            .child(count)
            .when(self.marked.len() > 1, |this| {
                this.child(format!("{} selected", format::count(self.marked.len())))
            })
            .children(self.notice.clone())
            .when(!self.filter.is_empty(), |this| {
                this.child(
                    h_flex()
                        .gap_1()
                        .px_2()
                        .rounded(style::RADIUS)
                        .bg(theme.accent)
                        .text_color(theme.accent_foreground)
                        .child(Icon::new(IconName::Search).size(em(0.75)))
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

/// The label that follows the pointer while rows are dragged.
struct DragPreview {
    label: SharedString,
}

impl Render for DragPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        h_flex()
            .px_2()
            .py_1()
            .gap_1()
            .rounded(style::RADIUS)
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .text_color(theme.popover_foreground)
            .text_body()
            .shadow_md()
            .child(Icon::new(IconName::File).size_4())
            .child(self.label.clone())
    }
}

/// The context menu of a row; items dispatch the list's actions.
fn row_menu(
    menu: gpui_component::menu::PopupMenu,
    focus: &gpui::FocusHandle,
    is_dir: bool,
) -> gpui_component::menu::PopupMenu {
    let menu = menu.action_context(focus.clone());
    let menu = if is_dir {
        menu.menu("Open", Box::new(actions::Open))
    } else {
        menu
    };
    menu.menu("Copy to other side", Box::new(actions::CopyToOtherPane))
        .menu("Copy", Box::new(actions::Copy))
        .menu("Copy address", Box::new(actions::CopyAddress))
        .separator()
        .menu("Rename", Box::new(actions::Rename))
        .menu("Duplicate", Box::new(actions::Duplicate))
        .menu("Delete…", Box::new(actions::Delete))
        .separator()
        .menu("New folder", Box::new(actions::NewFolder))
        .menu("New file", Box::new(actions::NewFile))
}

/// The context menu of the folder itself, opened beside the rows.
fn folder_menu(
    menu: gpui_component::menu::PopupMenu,
    focus: &gpui::FocusHandle,
    can_paste: bool,
    show_hidden: bool,
) -> gpui_component::menu::PopupMenu {
    menu.action_context(focus.clone())
        .menu("New folder", Box::new(actions::NewFolder))
        .menu("New file", Box::new(actions::NewFile))
        .menu_with_enable("Paste", Box::new(actions::Paste), can_paste)
        .separator()
        .menu("Copy address", Box::new(actions::CopyAddress))
        .menu("Refresh", Box::new(actions::Refresh))
        .menu_with_check(
            "Show hidden files",
            show_hidden,
            Box::new(actions::ToggleHiddenFiles),
        )
}

impl FileBrowser {
    /// The name label, or the name field while the row is renamed.
    fn render_name(&self, entry: &ferry_core::Entry) -> AnyElement {
        if let Some(renaming) = self.renaming()
            && renaming.name == entry.name
        {
            return div()
                .flex_1()
                .min_w_0()
                .key_context(actions::FILE_RENAME)
                // The name stays where it was: the field's text starts at the
                // label's position.
                .ml(-em(0.3))
                .child(Input::new(&renaming.input).small().px(em(0.25)))
                .into_any_element();
        }
        div()
            .flex_1()
            .min_w_0()
            .overflow_x_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .child(SharedString::from(entry.name.as_str().to_owned()))
            .into_any_element()
    }
}
