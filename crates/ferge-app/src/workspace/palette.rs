//! Opening the command palette and carrying out what is chosen in it.

use ferge_core::{Destination, EntryKind};
use gpui::{Action, AppContext as _, Context, SharedString, Window};
use gpui_component::ActiveTheme as _;
use gpui_component::kbd::Kbd;
use gpui_component_assets::IconName;

use super::{
    CloseTab, NewTab, NextTab, PreviousTab, ToggleInspector, ToggleSplit, ToggleTransfers,
    WORKSPACE_CONTEXT, Workspace,
};
use crate::actions;
use crate::file_type::FileType;
use crate::palette::{Group, Item, Palette, PaletteEvent, Target};
use crate::session::ConnectRequest;
use crate::settings_window::OpenSettings;

/// Entries of open folders offered at most, so opening the palette stays
/// instant next to folders with hundreds of thousands of entries.
const ENTRY_LIMIT: usize = 200_000;

/// A command: its title, action, the key context its shortcut is bound in,
/// and icon.
pub(crate) type Command = (&'static str, Box<dyn Action>, &'static str, IconName);

#[expect(
    clippy::too_many_lines,
    reason = "a flat table of the palette's commands reads best in one piece"
)]
pub(crate) fn commands() -> Vec<Command> {
    let list = actions::FILE_LIST;
    let pane = actions::PANE;
    let window = WORKSPACE_CONTEXT;
    vec![
        ("New tab", Box::new(NewTab), window, IconName::Plus),
        ("Close tab", Box::new(CloseTab), window, IconName::X),
        ("Next tab", Box::new(NextTab), window, IconName::ArrowRight),
        (
            "Previous tab",
            Box::new(PreviousTab),
            window,
            IconName::ArrowLeft,
        ),
        (
            "Two file browsers",
            Box::new(ToggleSplit),
            window,
            IconName::PanelRight,
        ),
        (
            "Show or hide transfers",
            Box::new(ToggleTransfers),
            window,
            IconName::ArrowDown,
        ),
        (
            "Show or hide info",
            Box::new(ToggleInspector),
            window,
            IconName::Info,
        ),
        ("Settings", Box::new(OpenSettings), "", IconName::Settings),
        (
            "Go to folder…",
            Box::new(actions::GoToFolder),
            pane,
            IconName::FolderOpen,
        ),
        (
            "Servers",
            Box::new(actions::ToggleServers),
            pane,
            IconName::Globe,
        ),
        (
            "Disconnect",
            Box::new(actions::Disconnect),
            pane,
            IconName::Unplug,
        ),
        (
            "Work as root",
            Box::new(actions::ToggleRoot),
            pane,
            IconName::ShieldUser,
        ),
        (
            "Quick look",
            Box::new(actions::Preview),
            list,
            IconName::Eye,
        ),
        (
            "New folder",
            Box::new(actions::NewFolder),
            list,
            IconName::FolderPlus,
        ),
        (
            "New file",
            Box::new(actions::NewFile),
            list,
            IconName::FilePlus,
        ),
        ("Rename", Box::new(actions::Rename), list, IconName::Pencil),
        (
            "Duplicate",
            Box::new(actions::Duplicate),
            list,
            IconName::Copy,
        ),
        ("Delete…", Box::new(actions::Delete), list, IconName::Trash),
        ("Copy", Box::new(actions::Copy), list, IconName::Copy),
        (
            "Paste",
            Box::new(actions::Paste),
            list,
            IconName::ClipboardPaste,
        ),
        (
            "Copy to the other side",
            Box::new(actions::CopyToOtherPane),
            list,
            IconName::ArrowRight,
        ),
        (
            "Copy address",
            Box::new(actions::CopyAddress),
            list,
            IconName::Link,
        ),
        (
            "Select all",
            Box::new(actions::SelectAll),
            list,
            IconName::ListChecks,
        ),
        (
            "Refresh",
            Box::new(actions::Refresh),
            list,
            IconName::RefreshCw,
        ),
        (
            "Show or hide hidden files",
            Box::new(actions::ToggleHiddenFiles),
            list,
            IconName::Eye,
        ),
        (
            "Sort by name",
            Box::new(actions::SortByName),
            list,
            IconName::ArrowUpDown,
        ),
        (
            "Sort by size",
            Box::new(actions::SortBySize),
            list,
            IconName::ArrowUpDown,
        ),
        (
            "Sort by date",
            Box::new(actions::SortByModified),
            list,
            IconName::ArrowUpDown,
        ),
        ("Back", Box::new(actions::GoBack), list, IconName::ArrowLeft),
        (
            "Forward",
            Box::new(actions::GoForward),
            list,
            IconName::ArrowRight,
        ),
        (
            "Parent folder",
            Box::new(actions::GoToParent),
            list,
            IconName::ArrowUp,
        ),
    ]
}

impl Workspace {
    pub(super) fn on_toggle_palette(
        &mut self,
        _: &super::TogglePalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.palette.is_some() {
            self.close_palette(window, cx);
            return;
        }
        let items = self.palette_items(window, cx);
        let palette = cx.new(|cx| Palette::new(items, window, cx));
        let events = cx.subscribe_in(&palette, window, |this, _, event, window, cx| match event {
            PaletteEvent::Run(target) => {
                let target = target.clone();
                this.close_palette(window, cx);
                this.run_palette_target(target, window, cx);
            }
            PaletteEvent::Dismiss => this.close_palette(window, cx),
        });
        palette.update(cx, |palette, cx| palette.focus(window, cx));
        self.palette = Some((palette, events));
        cx.notify();
    }

    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.take().is_some() {
            if let Some(tab) = self.tabs.get(self.active) {
                let focused = tab.focused;
                self.focus_pane(focused, window, cx);
            }
            cx.notify();
        }
    }

    fn palette_items(&self, window: &Window, cx: &Context<Self>) -> Vec<Item> {
        let mut items = self.tab_items(cx);
        items.extend(self.server_items());
        items.extend(command_items(window, cx));
        items.extend(self.entry_items(cx));
        items
    }

    fn tab_items(&self, cx: &Context<Self>) -> Vec<Item> {
        self.tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| Item {
                group: Group::Tab,
                icon: IconName::PanelsTopLeft,
                icon_color: None,
                title: tab.active().read(cx).title(cx),
                detail: if index == self.active {
                    "current tab".into()
                } else {
                    format!("tab {}", index + 1).into()
                },
                shortcut: None,
                target: Target::Tab(index),
            })
            .collect()
    }

    /// Favorites, `ssh_config` hosts, recent connections and this computer.
    fn server_items(&self) -> Vec<Item> {
        let library = &self.snapshot;
        let mut items = Vec::new();
        for bookmark in &library.bookmarks {
            let request = ConnectRequest {
                bookmark: Some(bookmark.id),
                identity_file: bookmark.identity_file.clone(),
                ..ConnectRequest::to(bookmark.destination.clone())
            };
            items.push(server_item(
                bookmark.name.clone(),
                bookmark.destination.address(),
                IconName::Star,
                request,
            ));
        }
        for host in self.ssh_hosts.iter() {
            if let Ok(destination) = Destination::parse(host) {
                items.push(server_item(
                    host.to_string(),
                    "from ~/.ssh/config".to_owned(),
                    IconName::SquareTerminal,
                    ConnectRequest::to(destination),
                ));
            }
        }
        for recent in &library.history {
            if matches!(recent.destination, Destination::Local { .. }) {
                continue;
            }
            let bookmark = recent.bookmark.and_then(|id| library.bookmark(id));
            let request = ConnectRequest {
                bookmark: bookmark.map(|bookmark| bookmark.id),
                identity_file: bookmark.and_then(|bookmark| bookmark.identity_file.clone()),
                ..ConnectRequest::to(recent.destination.clone())
            };
            items.push(server_item(
                recent.destination.title(),
                format!("{} · recent", recent.destination.address()),
                IconName::Clock,
                request,
            ));
        }
        items.push(server_item(
            "This computer".to_owned(),
            "your home folder".to_owned(),
            IconName::HardDrive,
            ConnectRequest::to(Destination::Local { path: None }),
        ));
        items
    }

    /// The entries of every folder open in a pane.
    fn entry_items(&self, cx: &Context<Self>) -> Vec<Item> {
        let mut items = Vec::new();
        for (tab_index, tab) in self.tabs.iter().enumerate() {
            for (pane_index, pane) in tab.panes.iter().enumerate() {
                let Some(browser) = pane.read(cx).browser().cloned() else {
                    continue;
                };
                let weak = browser.downgrade();
                let browser = browser.read(cx);
                let location: SharedString = browser.address_of(browser.path()).into();
                for entry in browser.entries() {
                    if items.len() == ENTRY_LIMIT {
                        return items;
                    }
                    let file_type = FileType::of(entry.name.as_str(), entry.kind());
                    items.push(Item {
                        group: Group::File,
                        icon: file_type.icon(),
                        icon_color: Some(file_type.color(cx.theme())),
                        title: crate::format::shown(entry.name.as_str()).into(),
                        detail: location.clone(),
                        shortcut: None,
                        target: Target::Entry {
                            tab: tab_index,
                            pane: pane_index,
                            browser: weak.clone(),
                            name: entry.name.clone(),
                            is_dir: entry.kind() == EntryKind::Directory,
                        },
                    });
                }
            }
        }
        items
    }

    fn run_palette_target(&mut self, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        match target {
            // The pane has the keyboard again, so its commands reach it.
            Target::Action(action) => window.dispatch_action(action, cx),
            Target::Tab(index) => self.activate(index, window, cx),
            Target::Connect(request) => {
                // A pane that shows the servers connects itself; otherwise
                // the server opens next to what is open, in a new tab.
                let idle = self
                    .active_session()
                    .filter(|session| session.read(cx).browser().is_none())
                    .cloned();
                match idle {
                    Some(session) => {
                        session.update(cx, |session, cx| session.connect(request, window, cx));
                    }
                    None => {
                        self.add_tab(Some(request), window, cx);
                    }
                }
            }
            Target::Entry {
                tab,
                pane,
                browser,
                name,
                is_dir,
            } => {
                let Some(browser) = browser.upgrade() else {
                    return;
                };
                self.activate(tab, window, cx);
                self.focus_pane(pane, window, cx);
                browser.update(cx, |browser, cx| {
                    if is_dir {
                        let path = browser.path().join(&name);
                        browser.go_to(path, cx);
                    } else {
                        browser.reveal(&name, cx);
                    }
                });
            }
        }
    }
}

/// Every command, with the shortcut that runs it.
fn command_items(window: &Window, cx: &Context<Workspace>) -> Vec<Item> {
    let muted = cx.theme().muted_foreground;
    commands()
        .into_iter()
        .map(|(title, action, context, icon)| {
            let shortcut = if context.is_empty() {
                Kbd::global_binding_for_action(action.as_ref(), window)
            } else {
                Kbd::binding_for_action(action.as_ref(), Some(context), window)
            };
            Item {
                group: Group::Command,
                icon,
                icon_color: Some(muted),
                title: title.into(),
                detail: SharedString::default(),
                shortcut,
                target: Target::Action(action),
            }
        })
        .collect()
}

fn server_item(title: String, detail: String, icon: IconName, request: ConnectRequest) -> Item {
    Item {
        group: Group::Server,
        icon,
        icon_color: None,
        title: title.into(),
        detail: detail.into(),
        shortcut: None,
        target: Target::Connect(request),
    }
}
