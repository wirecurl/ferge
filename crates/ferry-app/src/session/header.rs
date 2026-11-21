//! The header of a pane: switch between files and servers, the path bar (or
//! the "go to folder" field) and the pane's menu.

use ferry_core::{Destination, RemotePath};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AppContext as _, ClickEvent, Context, ElementId, Focusable as _, InteractiveElement as _,
    IntoElement, ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
    Window, div,
};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_component::{ActiveTheme as _, Icon, IconName, Selectable as _, h_flex};
use gpui_component_assets::IconName as ShieldIcon;

use super::servers::Tab;
use super::{ConnectRequest, GoTo, LibraryAction, Session, SessionEvent};
use crate::actions;
use crate::style::{self, ControlSize, IconButton, Palette as _, TypeScale as _, em};

impl Session {
    pub(super) fn render_header(&self, cx: &mut Context<Self>) -> gpui::Div {
        let header = h_flex()
            .h(style::HEADER_HEIGHT)
            .px(style::BAR_PADDING)
            .gap_2()
            .flex_shrink_0()
            .text_body();
        if self.shows_servers() {
            return self.render_servers_header(header, cx);
        }
        let switch = IconButton::new("show-servers", IconName::Globe)
            .size(ControlSize::Compact)
            .tooltip("Servers")
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.toggle_servers(window, cx);
            }));
        let middle = match &self.go_to {
            Some(go_to) => div()
                .flex_1()
                .min_w_0()
                .key_context(actions::GO_TO)
                .on_action(cx.listener(|this, _: &actions::CancelGoTo, window, cx| {
                    this.close_go_to(window, cx);
                }))
                .child(style::field(
                    Input::new(&go_to.input),
                    ControlSize::Compact,
                    cx,
                ))
                .into_any_element(),
            None => self.render_path_bar(cx).into_any_element(),
        };
        // Working as root is one click away on every server, and plain to
        // see while it lasts.
        let root = self.can_work_as_root().then(|| {
            let on = self.works_as_root();
            IconButton::new("work-as-root", ShieldIcon::ShieldUser)
                .size(ControlSize::Compact)
                .selected(on)
                .tooltip(if on {
                    "Stop working as root"
                } else {
                    "Work as root"
                })
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.toggle_root(window, cx);
                }))
        });
        header
            .when(self.works_as_root() && !self.opens_root(), |this| {
                this.bg(cx.theme().root_fill())
            })
            .child(switch)
            .child(middle)
            .children(root)
            .child(self.render_pane_menu(cx))
    }

    fn render_servers_header(&self, header: gpui::Div, cx: &mut Context<Self>) -> gpui::Div {
        let back = self.connection.is_some().then(|| {
            IconButton::new("show-files", IconName::Folder)
                .size(ControlSize::Compact)
                .tooltip("Back to the files")
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.toggle_servers(window, cx);
                }))
        });
        let this = cx.entity().downgrade();
        let add = IconButton::new("servers-add", IconName::Plus)
            .size(ControlSize::Compact)
            .tooltip("New favorite or folder")
            .dropdown_menu(move |menu, _, _| {
                let send = |action: LibraryAction| {
                    let this = this.clone();
                    move |_: &ClickEvent, _: &mut Window, cx: &mut gpui::App| {
                        let _ =
                            this.update(cx, |_, cx| cx.emit(SessionEvent::Library(action.clone())));
                    }
                };
                menu.item(PopupMenuItem::new("New favorite…").on_click(send(
                    LibraryAction::NewFavorite {
                        folder: None,
                        destination: None,
                    },
                )))
                .item(
                    PopupMenuItem::new("New folder…")
                        .on_click(send(LibraryAction::NewFolder { parent: None })),
                )
                .separator()
                .item(
                    PopupMenuItem::new("Clear recent connections…")
                        .on_click(send(LibraryAction::ClearHistory)),
                )
            });
        let tab = self.servers.tab();
        let switch = {
            let this = cx.entity().downgrade();
            style::segmented(
                "server-tabs",
                ControlSize::Compact,
                &[
                    (Tab::Servers, "Servers".into()),
                    (Tab::QuickConnect, "Quick Connect".into()),
                ],
                tab,
                move |tab, window, cx| {
                    let _ = this.update(cx, |session, cx| session.servers.show(tab, window, cx));
                },
                cx,
            )
        };
        let search = (tab == Tab::Servers).then(|| {
            div()
                .w(em(14.))
                .min_w(em(6.))
                .flex_shrink_1()
                // Arrow keys move through the list while typing a search.
                .capture_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                    let delta = match event.keystroke.key.as_str() {
                        "up" => -1,
                        "down" => 1,
                        _ => return,
                    };
                    this.move_server_selection(delta, cx);
                    cx.stop_propagation();
                }))
                .child(style::field(
                    Input::new(self.servers.search())
                        .prefix(Icon::new(IconName::Search).size(ControlSize::Compact.icon()))
                        .cleanable(true),
                    ControlSize::Compact,
                    cx,
                ))
        });
        header
            .children(back)
            .child(switch)
            .child(div().flex_1())
            .children(search)
            .child(add)
    }

    /// Breadcrumbs of the current folder; the active pane's folder is
    /// highlighted.
    fn render_path_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, accent, hover, foreground, warning) = (
            theme.muted_foreground,
            theme.blue,
            theme.hover_fill(),
            theme.foreground,
            theme.root_accent(),
        );
        let as_root = self.works_as_root() && !self.opens_root();
        let Some(browser) = self.browser() else {
            return h_flex().id("path-bar").flex_1();
        };
        let browser = browser.read(cx);
        let label: SharedString = if browser.remote().is_local() {
            "This computer".into()
        } else {
            browser.remote_label().to_owned().into()
        };
        let root_icon: Icon = if browser.remote().is_local() {
            IconName::HardDrive.into()
        } else if as_root {
            ShieldIcon::ShieldUser.into()
        } else {
            IconName::Globe.into()
        };
        let ancestors = browser.path().ancestors_from_root();
        let last = ancestors.len().saturating_sub(1);
        let active = self.active;
        // The root is named after the computer or server it belongs to.
        let crumbs = ancestors.into_iter().enumerate().map(|(index, path)| {
            let current = index == last;
            let crumb = h_flex()
                .id(ElementId::NamedInteger("crumb".into(), index as u64))
                .h(ControlSize::Compact.item())
                .px(em(0.4))
                .gap(em(0.4))
                .flex_shrink_0()
                .rounded(style::RADIUS)
                .cursor_pointer()
                .hover(|style| style.bg(hover))
                .map(|this| match (current, active) {
                    (true, true) => this
                        .text_color(accent)
                        .font_weight(gpui::FontWeight::SEMIBOLD),
                    (true, false) => this
                        .text_color(foreground)
                        .font_weight(gpui::FontWeight::SEMIBOLD),
                    (false, _) => this.text_color(muted),
                })
                .on_click(cx.listener({
                    let path = path.clone();
                    move |this, _: &ClickEvent, _, cx| this.go_to_path(path.clone(), cx)
                }))
                .map(|this| match path.file_name() {
                    Some(name) => this.child(SharedString::from(name.to_owned())),
                    None => this
                        .when(as_root, |this| this.text_color(warning))
                        .child(root_icon.clone().size(em(1.)))
                        .child(label.clone()),
                });
            h_flex()
                .flex_shrink_0()
                .when(index > 0, |this| {
                    this.child(
                        Icon::new(IconName::ChevronRight)
                            .size(em(0.875))
                            .text_color(muted.opacity(0.6)),
                    )
                })
                .child(crumb)
        });
        h_flex()
            .id("path-bar")
            .flex_1()
            .min_w_0()
            .overflow_x_hidden()
            .children(crumbs)
            // Double-clicking the bar opens the field to type a location.
            .on_click(cx.listener(|this, event: &ClickEvent, window, cx| {
                if event.click_count() >= 2 {
                    this.open_go_to(window, cx);
                }
            }))
    }

    fn render_pane_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity().downgrade();
        let options = self.options;
        let focus = self.browser().map(|browser| browser.focus_handle(cx));
        let root = self.can_work_as_root().then(|| self.works_as_root());
        IconButton::new("pane-menu", IconName::Ellipsis)
            .size(ControlSize::Compact)
            .tooltip("View options")
            .dropdown_menu(move |menu, _, _| {
                let toggle = |change: fn(&mut crate::browser::ViewOptions)| {
                    let this = this.clone();
                    move |_: &ClickEvent, _: &mut Window, cx: &mut gpui::App| {
                        let _ = this.update(cx, |session, cx| {
                            let mut options = session.options;
                            change(&mut options);
                            session.set_options(options, cx);
                        });
                    }
                };
                let mut menu = menu
                    .item(
                        PopupMenuItem::new("Show hidden files")
                            .checked(options.show_hidden)
                            .on_click(toggle(|o| o.show_hidden = !o.show_hidden)),
                    )
                    .item(
                        PopupMenuItem::new("Show kind")
                            .checked(options.show_kind)
                            .on_click(toggle(|o| o.show_kind = !o.show_kind)),
                    )
                    .item(
                        PopupMenuItem::new("Show permissions")
                            .checked(options.show_permissions)
                            .on_click(toggle(|o| o.show_permissions = !o.show_permissions)),
                    )
                    .separator();
                if let Some(focus) = &focus {
                    menu = menu
                        .action_context(focus.clone())
                        .menu("Go to folder…", Box::new(actions::GoToFolder))
                        .menu("Refresh", Box::new(actions::Refresh))
                        .menu("Copy address", Box::new(actions::CopyAddress));
                }
                let add = {
                    let this = this.clone();
                    move |_: &ClickEvent, _: &mut Window, cx: &mut gpui::App| {
                        let _ = this.update(cx, |session, cx| {
                            let destination = session.location(cx);
                            cx.emit(SessionEvent::Library(LibraryAction::NewFavorite {
                                folder: None,
                                destination,
                            }));
                        });
                    }
                };
                let disconnect = {
                    let this = this.clone();
                    move |_: &ClickEvent, window: &mut Window, cx: &mut gpui::App| {
                        let _ = this.update(cx, |session, cx| session.disconnect(window, cx));
                    }
                };
                menu = menu
                    .item(PopupMenuItem::new("Add to favorites…").on_click(add))
                    .separator();
                if let (Some(on), Some(focus)) = (root, &focus) {
                    menu = menu.action_context(focus.clone()).menu_with_check(
                        "Work as root",
                        on,
                        Box::new(actions::ToggleRoot),
                    );
                }
                menu.item(PopupMenuItem::new("Disconnect").on_click(disconnect))
            })
    }

    fn go_to_path(&mut self, path: RemotePath, cx: &mut Context<Self>) {
        if let Some(browser) = self.browser().cloned() {
            browser.update(cx, |browser, cx| browser.go_to(path, cx));
        }
    }

    pub(super) fn on_go_to_folder(
        &mut self,
        _: &actions::GoToFolder,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.shows_servers() {
            self.servers.focus_quick_connect(window, cx);
            return;
        }
        self.open_go_to(window, cx);
    }

    /// Replaces the path bar with a field holding the current location.
    fn open_go_to(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = self
            .browser()
            .map(|browser| browser.read(cx).path().to_string())
            .unwrap_or_default();
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("A folder, an sftp:// address or an ssh_config alias")
                .default_value(current)
        });
        input.update(cx, |input, cx| {
            input.focus(window, cx);
            input.select_all(window, cx);
        });
        let events = cx.subscribe_in(
            &input,
            window,
            |this, input, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => {
                    let text = input.read(cx).value().trim().to_owned();
                    this.close_go_to(window, cx);
                    this.go_to_location(&text, window, cx);
                }
                InputEvent::Blur => this.close_go_to(window, cx),
                _ => {}
            },
        );
        self.go_to = Some(GoTo {
            input,
            _events: events,
        });
        cx.notify();
    }

    fn close_go_to(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.go_to.take().is_some() {
            self.focus(window, cx);
            cx.notify();
        }
    }

    /// A path stays on this connection; anything else connects anew.
    fn go_to_location(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        if text.is_empty() {
            return;
        }
        if text.starts_with('/')
            && self.connection.is_some()
            && let Ok(path) = RemotePath::parse(text)
        {
            self.go_to_path(path, cx);
            return;
        }
        match Destination::parse(text) {
            Ok(destination) => self.connect(ConnectRequest::to(destination), window, cx),
            Err(error) => cx.emit(SessionEvent::Failed(error.to_string())),
        }
    }
}
