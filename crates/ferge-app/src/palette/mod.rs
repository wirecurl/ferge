//! The command palette (Ctrl+K / ⌘K): one field to run a command, open a
//! server, switch tabs or jump to a file in any open folder.
//!
//! The workspace gathers the items when the palette opens; matching runs in
//! the background, so typing stays smooth with large folders open.

mod fuzzy;

use std::sync::Arc;

use ferge_core::EntryName;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    Action, AppContext as _, Context, Entity, EventEmitter, Hsla, InteractiveElement as _,
    IntoElement, KeyDownEvent, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, UniformListScrollHandle,
    WeakEntity, Window, div, uniform_list,
};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::kbd::Kbd;
use gpui_component::{ActiveTheme as _, Icon, h_flex, v_flex};
use gpui_component_assets::IconName;

use self::fuzzy::Query;
use crate::browser::FileBrowser;
use crate::session::ConnectRequest;
use crate::style::{self, Palette as _, TypeScale as _, em};

/// Results shown at most.
const RESULT_LIMIT: usize = 60;

/// What choosing an item does.
pub(crate) enum Target {
    Action(Box<dyn Action>),
    Connect(ConnectRequest),
    Tab(usize),
    /// An entry of a folder open in a pane: opens a folder, selects a file.
    Entry {
        tab: usize,
        pane: usize,
        browser: WeakEntity<FileBrowser>,
        name: EntryName,
        is_dir: bool,
    },
}

impl Clone for Target {
    fn clone(&self) -> Self {
        match self {
            Self::Action(action) => Self::Action(action.boxed_clone()),
            Self::Connect(request) => Self::Connect(request.clone()),
            Self::Tab(index) => Self::Tab(*index),
            Self::Entry {
                tab,
                pane,
                browser,
                name,
                is_dir,
            } => Self::Entry {
                tab: *tab,
                pane: *pane,
                browser: browser.clone(),
                name: name.clone(),
                is_dir: *is_dir,
            },
        }
    }
}

/// The kinds of items, in the order they are listed without a query.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Group {
    Tab,
    Server,
    Command,
    File,
}

impl Group {
    fn label(self) -> &'static str {
        match self {
            Self::Tab => "Tab",
            Self::Server => "Server",
            Self::Command => "Command",
            Self::File => "File",
        }
    }
}

pub(crate) struct Item {
    pub group: Group,
    pub icon: IconName,
    pub icon_color: Option<Hsla>,
    pub title: SharedString,
    pub detail: SharedString,
    pub shortcut: Option<Kbd>,
    pub target: Target,
}

pub(crate) enum PaletteEvent {
    Run(Target),
    Dismiss,
}

pub(crate) struct Palette {
    input: Entity<InputState>,
    items: Arc<[Item]>,
    /// What each item is matched by, for the background matcher.
    keys: Arc<[SharedString]>,
    /// Indexes into `items`, best first.
    results: Vec<usize>,
    selected: usize,
    scroll: UniformListScrollHandle,
    matching: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PaletteEvent> for Palette {}

impl Palette {
    pub(crate) fn new(items: Vec<Item>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Search commands, servers, tabs and files")
        });
        let events =
            cx.subscribe_in(
                &input,
                window,
                |this, _, event: &InputEvent, _, cx| match event {
                    InputEvent::Change => this.update_results(cx),
                    InputEvent::PressEnter { .. } => this.run_selected(cx),
                    _ => {}
                },
            );
        let keys: Arc<[SharedString]> = items
            .iter()
            .map(|item| match item.group {
                // Servers are found by name and address alike.
                Group::Server => format!("{} {}", item.title, item.detail).into(),
                _ => item.title.clone(),
            })
            .collect();
        let mut palette = Self {
            input,
            items: items.into(),
            keys,
            results: Vec::new(),
            selected: 0,
            scroll: UniformListScrollHandle::new(),
            matching: None,
            _subscriptions: vec![events],
        };
        palette.update_results(cx);
        palette
    }

    pub(crate) fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| input.focus(window, cx));
    }

    fn update_results(&mut self, cx: &mut Context<Self>) {
        let query = Query::new(&self.input.read(cx).value());
        if query.is_empty() {
            // Everything except files, grouped.
            let mut results: Vec<usize> = (0..self.items.len())
                .filter(|&index| self.items[index].group != Group::File)
                .collect();
            results.sort_by_key(|&index| self.items[index].group);
            self.show(results, cx);
            return;
        }
        let keys = Arc::clone(&self.keys);
        let groups: Arc<[Group]> = self.items.iter().map(|item| item.group).collect();
        let matching = cx.background_spawn(async move {
            let mut scored: Vec<(i32, Group, usize)> = keys
                .iter()
                .enumerate()
                .filter_map(|(index, key)| {
                    query.score(key).map(|score| (score, groups[index], index))
                })
                .collect();
            // Best score first; among equals, commands before files.
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            scored.truncate(RESULT_LIMIT);
            scored
                .into_iter()
                .map(|(_, _, index)| index)
                .collect::<Vec<_>>()
        });
        self.matching = Some(cx.spawn(async move |this, cx| {
            let results = matching.await;
            let _ = this.update(cx, |palette, cx| palette.show(results, cx));
        }));
    }

    fn show(&mut self, results: Vec<usize>, cx: &mut Context<Self>) {
        self.results = results;
        self.selected = 0;
        self.scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
        cx.notify();
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.results.is_empty() {
            return;
        }
        let last = self.results.len() - 1;
        self.selected = self.selected.saturating_add_signed(delta).min(last);
        self.scroll
            .scroll_to_item(self.selected, gpui::ScrollStrategy::Nearest);
        cx.notify();
    }

    fn run_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(&index) = self.results.get(self.selected) {
            cx.emit(PaletteEvent::Run(self.items[index].target.clone()));
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "up" => self.move_selection(-1, cx),
            "down" => self.move_selection(1, cx),
            "pageup" => self.move_selection(-8, cx),
            "pagedown" => self.move_selection(8, cx),
            "escape" => cx.emit(PaletteEvent::Dismiss),
            _ => return,
        }
        cx.stop_propagation();
    }
}

#[derive(Clone, Copy)]
struct RowColors {
    muted: Hsla,
    active: Hsla,
    hover: Hsla,
}

/// One result: icon, title, detail and the shortcut or the kind of item.
fn render_row(
    item: &Item,
    row: usize,
    selected: bool,
    colors: RowColors,
    this: WeakEntity<Palette>,
) -> gpui::Stateful<gpui::Div> {
    let RowColors {
        muted,
        active,
        hover,
    } = colors;
    h_flex()
        .id(row)
        .w_full()
        .h(em(2.25))
        .px(em(0.625))
        .gap(em(0.625))
        .rounded(style::RADIUS)
        .cursor_pointer()
        .map(|this| {
            if selected {
                this.bg(active)
            } else {
                this.hover(|style| style.bg(hover))
            }
        })
        .on_click(move |_, _, cx| {
            let _ = this.update(cx, |palette, cx| {
                palette.selected = row;
                palette.run_selected(cx);
            });
        })
        .child(
            Icon::new(item.icon)
                .size(em(1.))
                .flex_shrink_0()
                .text_color(item.icon_color.unwrap_or(muted)),
        )
        .child(
            div()
                .flex_shrink_0()
                .max_w(em(22.))
                .truncate()
                .child(item.title.clone()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_small()
                .text_color(muted)
                .child(item.detail.clone()),
        )
        .map(|this| match &item.shortcut {
            Some(shortcut) => this.child(shortcut.clone()),
            None => this.child(
                div()
                    .flex_shrink_0()
                    .text_small()
                    .text_color(muted)
                    .child(item.group.label()),
            ),
        })
}

impl Render for Palette {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (border, background, muted, active, hover) = (
            theme.border,
            theme.popover,
            theme.muted_foreground,
            theme.list_active,
            theme.hover_fill(),
        );
        let items = Arc::clone(&self.items);
        let results = self.results.clone();
        let selected = self.selected;
        let this = cx.entity().downgrade();
        let list = uniform_list("palette-results", results.len(), move |range, _, _| {
            range
                .map(|row| {
                    let colors = RowColors {
                        muted,
                        active,
                        hover,
                    };
                    render_row(
                        &items[results[row]],
                        row,
                        row == selected,
                        colors,
                        this.clone(),
                    )
                })
                .collect()
        })
        .track_scroll(&self.scroll)
        .flex_1()
        .p(em(0.375));
        let empty = self
            .results
            .is_empty()
            .then(|| div().p(em(1.)).text_color(muted).child("Nothing found."));
        div()
            .id("palette-backdrop")
            .absolute()
            .inset_0()
            // Nothing behind the palette reacts to the mouse while it is open.
            .occlude()
            .flex()
            .flex_col()
            .items_center()
            .pt(em(4.))
            .on_click(cx.listener(|_, _, _, cx| cx.emit(PaletteEvent::Dismiss)))
            .child(
                v_flex()
                    .id("palette")
                    .w(em(40.))
                    .max_w_full()
                    .h(em(26.))
                    .rounded(style::GROUP_RADIUS)
                    .border_1()
                    .border_color(border)
                    .bg(background)
                    .shadow_lg()
                    .overflow_hidden()
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .capture_key_down(cx.listener(Self::on_key_down))
                    .child(
                        h_flex()
                            .h(style::HEADER_HEIGHT)
                            .px(em(0.625))
                            .gap(em(0.5))
                            .flex_shrink_0()
                            .border_b_1()
                            .border_color(border)
                            .child(Icon::new(IconName::Search).size(em(1.)).text_color(muted))
                            .child(
                                div()
                                    .flex_1()
                                    .child(Input::new(&self.input).appearance(false)),
                            ),
                    )
                    .child(list)
                    .children(empty)
                    .child(
                        h_flex()
                            .px(em(0.75))
                            .py(em(0.375))
                            .gap_3()
                            .flex_shrink_0()
                            .border_t_1()
                            .border_color(border)
                            .text_small()
                            .text_color(muted)
                            .child("↑↓ choose")
                            .child("Enter run")
                            .child("Esc close"),
                    ),
            )
    }
}
