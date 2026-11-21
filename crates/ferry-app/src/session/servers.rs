//! The server panel of a pane: saved servers in folders, the hosts from
//! `~/.ssh/config`, recent connections and this computer, plus a Quick
//! Connect form. Double-clicking a server connects this pane.

use std::collections::HashSet;
use std::time::SystemTime;

use ferry_core::library::{BookmarkId, FolderId};
use ferry_core::{Destination, RemotePath, SftpTarget};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, ElementId, Entity,
    InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, px, rems,
};
use gpui_component::button::{Button, ButtonGroup, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Selectable as _, Sizable as _, h_flex, v_flex,
};
use secrecy::SecretString;

use super::{ConnectRequest, Session, SessionEvent};
use crate::style::{self, Palette as _, TypeScale as _};

/// Something in the server panel that needs one of the window's dialogs.
#[derive(Clone, Debug)]
pub(crate) enum LibraryAction {
    /// Create a favorite, prefilled with `destination` if given.
    NewFavorite {
        folder: Option<FolderId>,
        destination: Option<Destination>,
    },
    EditFavorite(BookmarkId),
    DuplicateFavorite(BookmarkId),
    DeleteFavorite(BookmarkId),
    NewFolder {
        parent: Option<FolderId>,
    },
    RenameFolder(FolderId),
    DeleteFolder(FolderId),
    ClearHistory,
    ForgetRecent(Destination),
}

/// Which part of the panel is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tab {
    Servers,
    QuickConnect,
}

/// A row that can be selected in the server list.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Row {
    Favorite(BookmarkId),
    Host(SharedString),
    Recent(usize, Destination),
    Local,
}

/// Indentation per folder level, in rem.
const INDENT: f32 = 1.25;

/// Recent connections listed.
const RECENT_SHOWN: usize = 8;

pub(super) struct ServerPanel {
    tab: Tab,
    search: Entity<InputState>,
    quick: QuickConnect,
    collapsed: HashSet<FolderId>,
    selected: Option<Row>,
    _search_events: Subscription,
}

impl ServerPanel {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Session>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search servers"));
        let search_events = cx.subscribe_in(
            &search,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => cx.notify(),
                InputEvent::PressEnter { secondary, .. } => {
                    if let Some(row) = this.highlighted_row(cx) {
                        this.open_row(&row, *secondary, window, cx);
                    }
                }
                _ => {}
            },
        );
        Self {
            tab: Tab::Servers,
            search,
            quick: QuickConnect::new(window, cx),
            collapsed: HashSet::new(),
            selected: None,
            _search_events: search_events,
        }
    }

    pub(super) fn search(&self) -> &Entity<InputState> {
        &self.search
    }

    pub(super) fn focus(&self, window: &mut Window, cx: &mut Context<Session>) {
        match self.tab {
            Tab::Servers => self.search.update(cx, |input, cx| input.focus(window, cx)),
            Tab::QuickConnect => self.quick.focus(window, cx),
        }
    }

    pub(super) fn focus_quick_connect(&mut self, window: &mut Window, cx: &mut Context<Session>) {
        self.tab = Tab::QuickConnect;
        self.quick.focus(window, cx);
        cx.notify();
    }

    fn query(&self, cx: &App) -> String {
        self.search.read(cx).value().trim().to_lowercase()
    }
}

/// The Quick Connect form.
struct QuickConnect {
    host: Entity<InputState>,
    port: Entity<InputState>,
    user: Entity<InputState>,
    password: Entity<InputState>,
    path: Entity<InputState>,
    remember: bool,
    error: Option<SharedString>,
    _submits: Vec<Subscription>,
}

impl QuickConnect {
    fn new(window: &mut Window, cx: &mut Context<Session>) -> Self {
        let field = |placeholder: &'static str,
                     masked: bool,
                     window: &mut Window,
                     cx: &mut Context<Session>| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .masked(masked)
            })
        };
        let host = field(
            "example.org, an address or an ssh_config alias",
            false,
            window,
            cx,
        );
        let port = field("22", false, window, cx);
        let user = field("From ssh_config, or your login name", false, window, cx);
        let password = field("Asked for when needed", true, window, cx);
        let path = field("The login folder", false, window, cx);
        let submits = [&host, &port, &user, &password, &path]
            .into_iter()
            .map(|input| {
                cx.subscribe_in(input, window, |this, _, event: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = event {
                        this.quick_connect(window, cx);
                    }
                })
            })
            .collect();
        Self {
            host,
            port,
            user,
            password,
            path,
            remember: false,
            error: None,
            _submits: submits,
        }
    }

    fn focus(&self, window: &mut Window, cx: &mut Context<Session>) {
        self.host.update(cx, |input, cx| input.focus(window, cx));
    }

    /// The destination described by the form.
    fn destination(&self, cx: &App) -> Result<Destination, String> {
        let text = |input: &Entity<InputState>| input.read(cx).value().trim().to_owned();
        let host = text(&self.host);
        if host.is_empty() {
            return Err("Enter a server to connect to.".into());
        }
        let mut target =
            SftpTarget::new(&host).map_err(|_| format!("“{host}” is not a valid server name."))?;
        let port = text(&self.port);
        if !port.is_empty() {
            target.port = Some(
                port.parse::<u16>()
                    .ok()
                    .filter(|port| *port != 0)
                    .ok_or_else(|| format!("“{port}” is not a valid port."))?,
            );
        }
        let user = text(&self.user);
        if !user.is_empty() {
            if user
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || c == '@')
            {
                return Err(format!("“{user}” is not a valid user name."));
            }
            target.user = Some(user);
        }
        let path = text(&self.path);
        if !path.is_empty() {
            target.path = Some(
                RemotePath::parse(&path)
                    .map_err(|_| "The folder must be an absolute path like /srv/www.")?,
            );
        }
        Ok(Destination::Sftp(target))
    }

    fn password(&self, cx: &App) -> Option<SecretString> {
        let password = self.password.read(cx).value();
        (!password.is_empty()).then(|| SecretString::from(password.to_string()))
    }
}

impl Session {
    fn quick_connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let quick = &mut self.servers.quick;
        match quick.destination(cx) {
            Ok(destination) => {
                quick.error = None;
                let request = ConnectRequest {
                    password: quick.password(cx),
                    remember_password: quick.remember,
                    ..ConnectRequest::to(destination)
                };
                // The password lives on in the request only.
                quick
                    .password
                    .update(cx, |input, cx| input.set_value("", window, cx));
                self.connect(request, window, cx);
            }
            Err(error) => {
                quick.error = Some(error.into());
                cx.notify();
            }
        }
    }

    fn quick_save(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        match self.servers.quick.destination(cx) {
            Ok(destination) => {
                self.servers.quick.error = None;
                cx.emit(SessionEvent::Library(LibraryAction::NewFavorite {
                    folder: None,
                    destination: Some(destination),
                }));
            }
            Err(error) => self.servers.quick.error = Some(error.into()),
        }
        cx.notify();
    }

    /// Connects this pane to the row, or opens it in a new tab.
    fn open_row(&mut self, row: &Row, new_tab: bool, window: &mut Window, cx: &mut Context<Self>) {
        let request = match row {
            Row::Favorite(id) => {
                let Some(bookmark) = self.library.bookmark(*id) else {
                    return;
                };
                ConnectRequest {
                    bookmark: Some(*id),
                    identity_file: bookmark.identity_file.clone(),
                    ..ConnectRequest::to(bookmark.destination.clone())
                }
            }
            Row::Host(alias) => match Destination::parse(alias) {
                Ok(destination) => ConnectRequest::to(destination),
                Err(_) => return,
            },
            Row::Recent(index, _) => {
                let Some(recent) = self.library.history.get(*index) else {
                    return;
                };
                let bookmark = recent.bookmark.and_then(|id| self.library.bookmark(id));
                ConnectRequest {
                    bookmark: bookmark.map(|bookmark| bookmark.id),
                    identity_file: bookmark.and_then(|bookmark| bookmark.identity_file.clone()),
                    ..ConnectRequest::to(recent.destination.clone())
                }
            }
            Row::Local => ConnectRequest::to(Destination::Local { path: None }),
        };
        if new_tab {
            cx.emit(SessionEvent::OpenInNewTab(request));
        } else {
            self.connect(request, window, cx);
        }
    }

    /// The rows of the server list in display order, for the keyboard.
    fn visible_rows(&self, cx: &App) -> Vec<Row> {
        let query = self.servers.query(cx);
        let matches = |name: &str, address: &str| {
            query.is_empty()
                || name.to_lowercase().contains(&query)
                || address.to_lowercase().contains(&query)
        };
        let mut rows = Vec::new();
        self.collect_favorites(None, &query, &mut rows);
        rows.extend(
            self.ssh_hosts
                .iter()
                .filter(|host| matches(host, host))
                .map(|host| Row::Host(host.clone())),
        );
        rows.extend(
            self.library
                .history
                .iter()
                .enumerate()
                .filter(|(_, recent)| !matches!(recent.destination, Destination::Local { .. }))
                .take(RECENT_SHOWN)
                .filter(|(_, recent)| {
                    let name = recent
                        .bookmark
                        .and_then(|id| self.library.bookmark(id))
                        .map_or_else(|| recent.destination.title(), |b| b.name.clone());
                    matches(&name, &recent.destination.address())
                })
                .map(|(index, recent)| Row::Recent(index, recent.destination.clone())),
        );
        if matches("this computer local files", "") {
            rows.push(Row::Local);
        }
        rows
    }

    fn collect_favorites(&self, folder: Option<FolderId>, query: &str, rows: &mut Vec<Row>) {
        let subfolders: Vec<FolderId> = self.library.subfolders(folder).map(|f| f.id).collect();
        for id in subfolders {
            if !query.is_empty() || !self.servers.collapsed.contains(&id) {
                self.collect_favorites(Some(id), query, rows);
            }
        }
        rows.extend(
            self.library
                .bookmarks_in(folder)
                .filter(|bookmark| {
                    query.is_empty()
                        || bookmark.name.to_lowercase().contains(query)
                        || bookmark
                            .destination
                            .address()
                            .to_lowercase()
                            .contains(query)
                })
                .map(|bookmark| Row::Favorite(bookmark.id)),
        );
    }

    /// The selected row if it is visible; while searching, the first match.
    fn highlighted_row(&self, cx: &App) -> Option<Row> {
        let rows = self.visible_rows(cx);
        match &self.servers.selected {
            Some(selected) if rows.contains(selected) => Some(selected.clone()),
            _ if !self.servers.query(cx).is_empty() => rows.into_iter().next(),
            _ => None,
        }
    }

    /// Moves the selection in the server list by `delta` rows.
    pub(super) fn move_server_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let rows = self.visible_rows(cx);
        if rows.is_empty() {
            return;
        }
        let current = self
            .highlighted_row(cx)
            .and_then(|row| rows.iter().position(|candidate| *candidate == row));
        let next = match current {
            Some(index) => index.saturating_add_signed(delta).min(rows.len() - 1),
            None if delta < 0 => rows.len() - 1,
            None => 0,
        };
        self.servers.selected = Some(rows[next].clone());
        cx.notify();
    }

    pub(super) fn render_servers(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let tab = self.servers.tab;
        let tabs = ButtonGroup::new("server-tabs")
            .small()
            .ghost()
            .child(
                Button::new("tab-servers")
                    .label("Servers")
                    .selected(tab == Tab::Servers),
            )
            .child(
                Button::new("tab-quick")
                    .label("Quick Connect")
                    .selected(tab == Tab::QuickConnect),
            )
            .on_click(cx.listener(|this, selected: &Vec<usize>, window, cx| {
                this.servers.tab = if selected.contains(&1) {
                    Tab::QuickConnect
                } else {
                    Tab::Servers
                };
                this.servers.focus(window, cx);
                cx.notify();
            }));
        let content = match tab {
            Tab::Servers => self.render_server_list(cx).into_any_element(),
            Tab::QuickConnect => self.render_quick_connect(cx).into_any_element(),
        };
        div()
            .id("server-panel")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(
                v_flex()
                    .p_3()
                    .gap_3()
                    .child(
                        h_flex()
                            .justify_center()
                            .child(style::track(cx).child(tabs)),
                    )
                    .child(content),
            )
    }

    fn render_quick_connect(&self, cx: &mut Context<Self>) -> gpui::Div {
        let theme = cx.theme();
        let (muted, danger) = (theme.muted_foreground, theme.danger);
        let quick = &self.servers.quick;
        let field = |label: &'static str, input: &Entity<InputState>| {
            v_flex()
                .flex_1()
                .gap_1()
                .child(div().text_small().text_color(muted).child(label))
                .child(Input::new(input))
        };
        v_flex()
            .w_full()
            .max_w(px(520.))
            .mx_auto()
            .gap_3()
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        v_flex()
                            .w(px(96.))
                            .gap_1()
                            .child(div().text_small().text_color(muted).child("Protocol"))
                            .child(
                                div()
                                    .h(px(32.))
                                    .px_2()
                                    .flex()
                                    .items_center()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(theme.border)
                                    .child("SFTP"),
                            ),
                    )
                    .child(field("Server", &quick.host))
                    .child(div().w(px(80.)).child(field("Port", &quick.port))),
            )
            .child(
                h_flex()
                    .gap_3()
                    .child(field("User", &quick.user))
                    .child(field("Password", &quick.password)),
            )
            .child(field("Initial folder", &quick.path))
            .children(
                quick
                    .error
                    .clone()
                    .map(|error| div().text_body().text_color(danger).child(error)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Checkbox::new("quick-remember")
                            .label("Remember password")
                            .checked(quick.remember)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.servers.quick.remember = *checked;
                                cx.notify();
                            })),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("quick-save")
                            .label("Add to favorites…")
                            .on_click(cx.listener(Self::quick_save)),
                    )
                    .child(
                        Button::new("quick-connect")
                            .primary()
                            .label("Connect")
                            .loading(self.is_connecting())
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.quick_connect(window, cx);
                            })),
                    ),
            )
    }

    fn render_server_list(&self, cx: &mut Context<Self>) -> gpui::Div {
        let query = self.servers.query(cx);
        let muted = cx.theme().muted_foreground;
        let matches = |name: &str, address: &str| {
            query.is_empty()
                || name.to_lowercase().contains(&query)
                || address.to_lowercase().contains(&query)
        };

        let highlight = self.highlighted_row(cx);
        let mut favorites = Vec::new();
        self.render_folder(None, 0, &query, highlight.as_ref(), cx, &mut favorites);
        let library_empty = self.library.bookmarks.is_empty() && self.library.folders.is_empty();

        let hosts: Vec<AnyElement> = self
            .ssh_hosts
            .iter()
            .filter(|host| matches(host, host))
            .enumerate()
            .map(|(index, host)| {
                let row = Row::Host(host.clone());
                Self::render_row(
                    ElementId::NamedInteger("host".into(), index as u64),
                    highlight.as_ref() == Some(&row),
                    row,
                    IconName::SquareTerminal,
                    host.clone(),
                    "from ~/.ssh/config".into(),
                    0,
                    cx,
                )
            })
            .collect();

        let recent = self.render_recent(&query, highlight.as_ref(), cx);

        let local = matches("this computer local files", "").then(|| {
            Self::render_row(
                "local".into(),
                highlight.as_ref() == Some(&Row::Local),
                Row::Local,
                IconName::HardDrive,
                "This computer".into(),
                "Your home folder".into(),
                0,
                cx,
            )
        });

        let hint = (library_empty && query.is_empty()).then(|| {
            div().px_1().text_small().text_color(muted).child(
                "Save servers you use often with + above, or with “Add to favorites” in a \
                 connected pane.",
            )
        });
        v_flex()
            .w_full()
            .max_w(rems(40.))
            .mx_auto()
            .gap_4()
            .children(match hint {
                Some(hint) => Some(
                    v_flex()
                        .child(style::section_title("Favorites", cx))
                        .child(hint),
                ),
                None => section("Favorites", favorites, cx),
            })
            .children(section("SSH config", hosts, cx))
            .children(section("Recent", recent, cx))
            .children(section("This computer", local.into_iter().collect(), cx))
    }

    /// Recent connections to servers matching `query`.
    fn render_recent(
        &self,
        query: &str,
        highlight: Option<&Row>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        self.library
            .history
            .iter()
            .enumerate()
            .filter(|(_, recent)| !matches!(recent.destination, Destination::Local { .. }))
            .take(RECENT_SHOWN)
            .filter_map(|(index, recent)| {
                let bookmark = recent.bookmark.and_then(|id| self.library.bookmark(id));
                let name = bookmark.map_or_else(
                    || recent.destination.title(),
                    |bookmark| bookmark.name.clone(),
                );
                let address = recent.destination.address();
                let matches = query.is_empty()
                    || name.to_lowercase().contains(query)
                    || address.to_lowercase().contains(query);
                matches.then(|| {
                    let row = Row::Recent(index, recent.destination.clone());
                    Self::render_row(
                        ElementId::NamedInteger("recent".into(), index as u64),
                        highlight == Some(&row),
                        row,
                        IconName::Globe,
                        name.into(),
                        format!("{address} · {}", ago(recent.connected_at)).into(),
                        0,
                        cx,
                    )
                })
            })
            .collect()
    }

    /// Appends the rows of `folder` (`None`: the top level) to `rows`.
    fn render_folder(
        &self,
        folder: Option<FolderId>,
        depth: usize,
        query: &str,
        highlight: Option<&Row>,
        cx: &mut Context<Self>,
        rows: &mut Vec<AnyElement>,
    ) {
        let subfolders: Vec<(FolderId, String)> = self
            .library
            .subfolders(folder)
            .map(|folder| (folder.id, folder.name.clone()))
            .collect();
        for (id, name) in subfolders {
            // While searching, folders are open so matches are visible.
            let open = !query.is_empty() || !self.servers.collapsed.contains(&id);
            rows.push(Self::render_folder_row(id, name, open, depth, cx));
            if open {
                self.render_folder(Some(id), depth + 1, query, highlight, cx, rows);
            }
        }
        let bookmarks: Vec<_> = self
            .library
            .bookmarks_in(folder)
            .filter(|bookmark| {
                query.is_empty()
                    || bookmark.name.to_lowercase().contains(query)
                    || bookmark
                        .destination
                        .address()
                        .to_lowercase()
                        .contains(query)
            })
            .map(|bookmark| {
                (
                    bookmark.id,
                    bookmark.name.clone(),
                    bookmark.destination.clone(),
                )
            })
            .collect();
        for (id, name, destination) in bookmarks {
            let icon = match destination {
                Destination::Local { .. } => IconName::HardDrive,
                Destination::Sftp(_) => IconName::Globe,
            };
            rows.push(Self::render_row(
                ElementId::Name(format!("favorite-{id:?}").into()),
                highlight == Some(&Row::Favorite(id)),
                Row::Favorite(id),
                icon,
                name.into(),
                destination.address().into(),
                depth,
                cx,
            ));
        }
    }

    fn render_folder_row(
        id: FolderId,
        name: String,
        open: bool,
        depth: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (muted, hover, folder) = (theme.muted_foreground, theme.hover_fill(), theme.blue);
        let this = cx.entity().downgrade();
        h_flex()
            .id(ElementId::Name(format!("folder-{id:?}").into()))
            .h(rems(2.25))
            .pl(indent(depth))
            .pr_2()
            .gap_2()
            .cursor_pointer()
            .hover(|style| style.bg(hover))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                if !this.servers.collapsed.remove(&id) {
                    this.servers.collapsed.insert(id);
                }
                cx.notify();
            }))
            .child(
                Icon::new(if open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .size_3()
                .text_color(muted),
            )
            .child(
                Icon::new(if open {
                    IconName::FolderOpen
                } else {
                    IconName::Folder
                })
                .text_color(folder),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(name),
            )
            .context_menu(move |menu, _, _| {
                let send = |action: LibraryAction| {
                    let this = this.clone();
                    move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                        let _ =
                            this.update(cx, |_, cx| cx.emit(SessionEvent::Library(action.clone())));
                    }
                };
                menu.item(PopupMenuItem::new("New favorite here…").on_click(send(
                    LibraryAction::NewFavorite {
                        folder: Some(id),
                        destination: None,
                    },
                )))
                .item(
                    PopupMenuItem::new("New folder inside…")
                        .on_click(send(LibraryAction::NewFolder { parent: Some(id) })),
                )
                .separator()
                .item(PopupMenuItem::new("Rename…").on_click(send(LibraryAction::RenameFolder(id))))
                .item(PopupMenuItem::new("Delete…").on_click(send(LibraryAction::DeleteFolder(id))))
            })
            .into_any_element()
    }

    /// A two-line row: name and address. Click selects, double-click connects.
    #[expect(
        clippy::too_many_arguments,
        reason = "the parts of one row; a struct would only rename them"
    )]
    fn render_row(
        id: ElementId,
        selected: bool,
        row: Row,
        icon: IconName,
        name: SharedString,
        detail: SharedString,
        depth: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (muted, hover, active, badge) = (
            theme.muted_foreground,
            theme.hover_fill(),
            theme.list_active,
            theme.track_fill(),
        );
        let this = cx.entity().downgrade();
        let clicked = row.clone();
        h_flex()
            .id(id)
            .h(rems(3.25))
            .pl(indent(depth))
            .pr_2()
            .gap_3()
            .cursor_pointer()
            .when(selected, |this| this.bg(active))
            .when(!selected, |this| this.hover(|style| style.bg(hover)))
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.servers.selected = Some(clicked.clone());
                if event.click_count() >= 2 {
                    let new_tab = event.modifiers().secondary(cx);
                    this.open_row(&clicked, new_tab, window, cx);
                }
                cx.notify();
            }))
            .child(
                div()
                    .size(rems(2.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(style::RADIUS)
                    .bg(badge)
                    .child(Icon::new(icon).size_4().text_color(muted)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_body().truncate().child(name))
                    .child(
                        div()
                            .text_small()
                            .text_color(muted)
                            .truncate()
                            .child(detail),
                    ),
            )
            .context_menu(move |menu, _, _| row_menu(menu, &this, &row))
            .into_any_element()
    }
}

/// The context menu of a server row.
fn row_menu(
    menu: gpui_component::menu::PopupMenu,
    this: &gpui::WeakEntity<Session>,
    row: &Row,
) -> gpui_component::menu::PopupMenu {
    let open = |new_tab: bool| {
        let (this, row) = (this.clone(), row.clone());
        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
            let _ = this.update(cx, |session, cx| {
                session.open_row(&row, new_tab, window, cx);
            });
        }
    };
    let send = |action: LibraryAction| {
        let this = this.clone();
        move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
            let _ = this.update(cx, |_, cx| cx.emit(SessionEvent::Library(action.clone())));
        }
    };
    let menu = menu
        .item(PopupMenuItem::new("Connect").on_click(open(false)))
        .item(PopupMenuItem::new("Connect in new tab").on_click(open(true)))
        .separator();
    match row {
        Row::Favorite(id) => menu
            .item(PopupMenuItem::new("Edit…").on_click(send(LibraryAction::EditFavorite(*id))))
            .item(
                PopupMenuItem::new("Duplicate")
                    .on_click(send(LibraryAction::DuplicateFavorite(*id))),
            )
            .item(PopupMenuItem::new("Delete…").on_click(send(LibraryAction::DeleteFavorite(*id)))),
        Row::Host(alias) => match Destination::parse(alias) {
            Ok(destination) => menu.item(PopupMenuItem::new("Add to favorites…").on_click(send(
                LibraryAction::NewFavorite {
                    folder: None,
                    destination: Some(destination),
                },
            ))),
            Err(_) => menu,
        },
        Row::Recent(_, destination) => menu
            .item(PopupMenuItem::new("Add to favorites…").on_click(send(
                LibraryAction::NewFavorite {
                    folder: None,
                    destination: Some(destination.clone()),
                },
            )))
            .item(
                PopupMenuItem::new("Remove from recent")
                    .on_click(send(LibraryAction::ForgetRecent(destination.clone()))),
            ),
        Row::Local => menu,
    }
}

fn indent(depth: usize) -> gpui::Rems {
    #[expect(clippy::cast_precision_loss, reason = "folder depth is small")]
    let depth = depth.min(8) as f32;
    rems(0.625 + INDENT * depth)
}

/// A titled group of rows; nothing at all without rows.
fn section(title: &'static str, rows: Vec<AnyElement>, cx: &App) -> Option<gpui::Div> {
    if rows.is_empty() {
        return None;
    }
    let group = style::group(cx).children(
        rows.into_iter()
            .enumerate()
            .map(|(index, row)| style::divided(div().child(row), index, cx)),
    );
    Some(v_flex().child(style::section_title(title, cx)).child(group))
}

/// "just now", "5 min ago", "3 h ago", "2 days ago".
fn ago(time: SystemTime) -> String {
    let seconds = SystemTime::now()
        .duration_since(time)
        .map_or(0, |elapsed| elapsed.as_secs());
    match seconds {
        0..60 => "just now".to_owned(),
        60..3_600 => format!("{} min ago", seconds / 60),
        3_600..86_400 => format!("{} h ago", seconds / 3_600),
        86_400..172_800 => "yesterday".to_owned(),
        _ => format!("{} days ago", seconds / 86_400),
    }
}
