//! The main window: tabs, the sidebar with favorites, and dialogs.

mod dialogs;
mod sidebar;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use ferry_core::Destination;
use ferry_core::library::{
    BookmarkDraft, BookmarkId, FolderId, Library, LibraryError, LibrarySnapshot,
};
use ferry_core::remote::sftp::{SshConfigSource, configured_hosts};
use ferry_core::transfer::{TransferEvent, TransferId};
use futures::StreamExt as _;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, AppContext as _, ClickEvent, Context, Entity, FocusHandle, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, Styled as _, Subscription, Task, Window,
    div, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::tab::{Tab, TabBar};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Selectable as _, Sizable as _, Theme, h_flex, v_flex,
};

use self::dialogs::{ActivePrompt, Dialog};
use crate::actions;
use crate::browser::FileBrowser;
use crate::prompts::{self, PromptRequest, SessionId, TaggedPrompt};
use crate::runtime;
use crate::session::{ConnectRequest, Session, SessionContext, SessionEvent};
use crate::transfers::{TransferPanel, Transfers};
use crate::vault::Vault;

/// Key context of the whole window.
const WORKSPACE_CONTEXT: &str = "Workspace";

/// Below this window width the sidebar hides itself.
const SIDEBAR_MIN_WINDOW_WIDTH: gpui::Pixels = px(720.);

/// Height of the transfer panel.
const TRANSFER_PANEL_HEIGHT: gpui::Pixels = px(220.);

/// A tab: one pane, or two side by side.
struct Panes {
    panes: Vec<Entity<Session>>,
    /// The pane with the keyboard focus.
    focused: usize,
}

impl Panes {
    fn active(&self) -> &Entity<Session> {
        &self.panes[self.focused.min(self.panes.len() - 1)]
    }

    fn other(&self) -> Option<&Entity<Session>> {
        (self.panes.len() == 2).then(|| &self.panes[1 - self.focused.min(1)])
    }
}

gpui::actions!(
    workspace,
    [
        FocusAddress,
        NewTab,
        CloseTab,
        NextTab,
        PreviousTab,
        ToggleSidebar,
        SaveFavorite,
        /// Shows a second pane next to the current one, or closes it.
        ToggleSplit,
        ToggleTransfers,
    ]
);

/// Registers the key bindings of this module.
pub(crate) fn bind_keys(cx: &mut App) {
    /// `cmd-…` on macOS, `ctrl-…` elsewhere.
    macro_rules! primary {
        ($key:literal) => {
            if cfg!(target_os = "macos") {
                concat!("cmd-", $key)
            } else {
                concat!("ctrl-", $key)
            }
        };
    }
    let context = Some(WORKSPACE_CONTEXT);
    cx.bind_keys([
        gpui::KeyBinding::new(primary!("l"), FocusAddress, context),
        gpui::KeyBinding::new(primary!("t"), NewTab, context),
        gpui::KeyBinding::new(primary!("w"), CloseTab, context),
        gpui::KeyBinding::new("ctrl-tab", NextTab, context),
        gpui::KeyBinding::new("ctrl-shift-tab", PreviousTab, context),
        gpui::KeyBinding::new(primary!("b"), ToggleSidebar, context),
        gpui::KeyBinding::new(primary!("d"), SaveFavorite, context),
        gpui::KeyBinding::new(primary!("\\"), ToggleSplit, context),
        gpui::KeyBinding::new(primary!("j"), ToggleTransfers, context),
    ]);
    #[cfg(target_os = "macos")]
    cx.bind_keys([
        gpui::KeyBinding::new("cmd-shift-]", NextTab, context),
        gpui::KeyBinding::new("cmd-shift-[", PreviousTab, context),
    ]);
    #[cfg(not(target_os = "macos"))]
    cx.bind_keys([
        gpui::KeyBinding::new("ctrl-pagedown", NextTab, context),
        gpui::KeyBinding::new("ctrl-pageup", PreviousTab, context),
    ]);
    dialogs::bind_keys(cx);
}

/// How the window was started.
#[derive(Debug, Default)]
pub(crate) struct Options {
    /// Connect the first tab to this address.
    pub initial: Option<String>,
    /// `-F` from the command line.
    pub ssh_config: Option<PathBuf>,
    /// Keep favorites, history and passwords in memory only (benchmarks).
    pub ephemeral: bool,
}

pub(crate) struct Workspace {
    context: SessionContext,
    tabs: Vec<Panes>,
    session_events: HashMap<SessionId, Subscription>,
    active: usize,
    next_session: u64,
    library: Option<Arc<Library>>,
    snapshot: Arc<LibrarySnapshot>,
    /// Host aliases from the SSH configuration.
    ssh_hosts: Vec<SharedString>,
    collapsed: HashSet<FolderId>,
    sidebar_open: bool,
    queued_prompts: VecDeque<TaggedPrompt>,
    prompt: Option<ActivePrompt>,
    dialog: Option<Dialog>,
    prompt_focus: FocusHandle,
    transfer_panel: Entity<TransferPanel>,
    transfers_open: bool,
    /// Transfers already seen, so only new ones open the panel.
    seen_transfers: HashSet<TransferId>,
    /// Background work owned by the window; dropped with it.
    tasks: Vec<Task<()>>,
    _appearance: Subscription,
}

impl Workspace {
    pub(crate) fn new(options: Options, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Theme::sync_system_appearance(Some(window), cx);
        let appearance = cx.observe_window_appearance(window, |_, window, cx| {
            Theme::sync_system_appearance(Some(window), cx);
        });

        let (sender, mut receiver) = prompts::channel();
        let prompt_listener = cx.spawn_in(window, async move |this, cx| {
            while let Some(prompt) = receiver.next().await {
                let delivered = this.update_in(cx, |workspace, window, cx| {
                    workspace.receive_prompt(prompt, window, cx);
                });
                if delivered.is_err() {
                    break;
                }
            }
        });
        let mut transfer_events = Transfers::install(sender.clone(), cx);
        let transfer_listener = cx.spawn_in(window, async move |this, cx| {
            while let Some(event) = transfer_events.next().await {
                let delivered = this.update_in(cx, |workspace, window, cx| {
                    workspace.on_transfer_event(event, window, cx);
                });
                if delivered.is_err() {
                    break;
                }
            }
        });
        let vault = if options.ephemeral {
            Vault::ephemeral()
        } else {
            Vault::new()
        };
        // Open the password store early and off the UI thread, so the first
        // saved password is not delayed by D-Bus.
        let warm_up = Arc::clone(&vault);
        runtime::spawn(cx, async move {
            warm_up.store().await;
        });

        let mut workspace = Self {
            context: SessionContext {
                prompts: sender,
                vault,
                ssh_config: options.ssh_config,
            },
            tabs: Vec::new(),
            session_events: HashMap::new(),
            active: 0,
            next_session: 0,
            library: None,
            snapshot: Arc::default(),
            ssh_hosts: Vec::new(),
            collapsed: HashSet::new(),
            sidebar_open: true,
            queued_prompts: VecDeque::new(),
            prompt: None,
            dialog: None,
            prompt_focus: cx.focus_handle(),
            transfer_panel: cx.new(TransferPanel::new),
            transfers_open: false,
            seen_transfers: HashSet::new(),
            tasks: Vec::new(),
            _appearance: appearance,
        };
        workspace.tasks = vec![
            prompt_listener,
            transfer_listener,
            workspace.load_ssh_hosts(cx),
            Self::load_library(options.ephemeral, cx),
        ];

        let session = workspace.add_tab(window, cx);
        session.update(cx, |session, cx| match options.initial {
            Some(address) => match Destination::parse(&address) {
                Ok(destination) => session.connect(ConnectRequest::to(destination), window, cx),
                Err(error) => {
                    tracing::error!(%address, %error, "invalid destination");
                    session.focus(window, cx);
                }
            },
            None => session.focus(window, cx),
        });
        workspace
    }

    /// The browser of the active tab, if it is connected (benchmarks).
    pub(crate) fn active_browser(&self, cx: &App) -> Option<Entity<FileBrowser>> {
        self.active_session()?.read(cx).browser().cloned()
    }

    fn active_session(&self) -> Option<&Entity<Session>> {
        self.tabs.get(self.active).map(Panes::active)
    }

    /// Every pane of every tab.
    fn all_sessions(&self) -> impl Iterator<Item = &Entity<Session>> {
        self.tabs.iter().flat_map(|tab| tab.panes.iter())
    }

    /// The tab and pane index of a session.
    fn locate(&self, id: SessionId, cx: &App) -> Option<(usize, usize)> {
        self.tabs.iter().enumerate().find_map(|(tab_index, tab)| {
            tab.panes
                .iter()
                .position(|pane| pane.read(cx).id() == id)
                .map(|pane_index| (tab_index, pane_index))
        })
    }

    // ---------------------------------------------------------------------
    // Background loading

    fn load_ssh_hosts(&self, cx: &mut Context<Self>) -> Task<()> {
        let source = self
            .context
            .ssh_config
            .clone()
            .map_or(SshConfigSource::User, SshConfigSource::File);
        let lookup = runtime::spawn(cx, async move {
            tokio::task::spawn_blocking(move || configured_hosts(&source)).await
        });
        cx.spawn(async move |this, cx| {
            let hosts = match lookup.await {
                Ok(Ok(Ok(hosts))) => hosts,
                Ok(Ok(Err(error))) => {
                    tracing::warn!(%error, "could not read the SSH configuration");
                    Vec::new()
                }
                _ => Vec::new(),
            };
            let _ = this.update(cx, |workspace, cx| {
                workspace.ssh_hosts = hosts.into_iter().map(SharedString::from).collect();
                cx.notify();
            });
        })
    }

    fn load_library(ephemeral: bool, cx: &mut Context<Self>) -> Task<()> {
        let opening = runtime::spawn(cx, async move {
            tokio::task::spawn_blocking(move || {
                let opened = match Library::default_path() {
                    Some(path) if !ephemeral => Library::open(&path),
                    _ => Library::open_in_memory(),
                };
                let (library, warning) = match opened {
                    Ok(library) => (library, None),
                    // Keep working without persistence rather than not at all.
                    Err(error) => (Library::open_in_memory()?, Some(error.to_string())),
                };
                let snapshot = library.snapshot()?;
                Ok::<_, LibraryError>((Arc::new(library), snapshot, warning))
            })
            .await
        });
        cx.spawn(async move |this, cx| {
            let opened = match opening.await {
                Ok(Ok(result)) => result,
                Ok(Err(error)) | Err(error) => Err(LibraryError::Invalid(error.to_string())),
            };
            let _ = this.update(cx, |workspace, cx| match opened {
                Ok((library, snapshot, warning)) => {
                    workspace.library = Some(library);
                    workspace.set_snapshot(snapshot, cx);
                    if let Some(warning) = warning {
                        workspace.notice(format!(
                            "Favorites could not be opened and will not be saved: {warning}"
                        ));
                    }
                }
                Err(error) => tracing::error!(%error, "the library is unavailable"),
            });
        })
    }

    /// Runs `change` against the library on the runtime, then reloads it.
    fn change_library(
        &mut self,
        change: impl FnOnce(&Library) -> Result<(), LibraryError> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(library) = self.library.clone() else {
            self.notice("Favorites are still loading. Try again in a moment.".into());
            return;
        };
        let changing = runtime::spawn(cx, async move {
            tokio::task::spawn_blocking(move || {
                let result = change(&library).map_err(|error| error.to_string());
                (result, library.snapshot())
            })
            .await
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok((result, snapshot))) = changing.await else {
                return;
            };
            let _ = this.update(cx, |workspace, cx| {
                match snapshot {
                    Ok(snapshot) => workspace.set_snapshot(snapshot, cx),
                    Err(error) => tracing::warn!(%error, "reloading the library failed"),
                }
                if let Err(error) = result {
                    workspace.notice(error);
                }
            });
        })
        .detach();
    }

    fn set_snapshot(&mut self, snapshot: LibrarySnapshot, cx: &mut Context<Self>) {
        self.snapshot = Arc::new(snapshot);
        let sessions: Vec<Entity<Session>> = self.all_sessions().cloned().collect();
        for session in sessions {
            let snapshot = Arc::clone(&self.snapshot);
            session.update(cx, |session, cx| session.set_library(snapshot, cx));
        }
        cx.notify();
    }

    /// Shows `message` as a notification. Goes through the prompt channel
    /// because showing it needs the window, which is not at hand everywhere.
    fn notice(&self, message: String) {
        let _ = self.context.prompts.unbounded_send(TaggedPrompt {
            session: None,
            request: PromptRequest::Notice(message),
        });
    }

    // ---------------------------------------------------------------------
    // Tabs

    /// Creates a pane; the caller places it in a tab.
    fn new_session(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<Session> {
        let id = SessionId(self.next_session);
        self.next_session += 1;
        let context = self.context.clone();
        let snapshot = Arc::clone(&self.snapshot);
        let session = cx.new(|cx| Session::new(id, context, snapshot, window, cx));
        let events = cx.subscribe_in(&session, window, Self::on_session_event);
        self.session_events.insert(id, events);
        session
    }

    /// Forgets a pane that was removed from its tab.
    fn discard_session(
        &mut self,
        session: &Entity<Session>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = session.read(cx).id();
        drop(self.session_events.remove(&id));
        self.drop_prompts_of(id, window, cx);
    }

    /// Opens a new tab with one pane and makes it active.
    fn add_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<Session> {
        let session = self.new_session(window, cx);
        self.tabs.push(Panes {
            panes: vec![session.clone()],
            focused: 0,
        });
        self.active = self.tabs.len() - 1;
        self.update_title(window, cx);
        cx.notify();
        session
    }

    fn activate(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        self.active = index;
        let session = self.tabs[index].active().clone();
        session.update(cx, |session, cx| session.focus(window, cx));
        self.update_title(window, cx);
        cx.notify();
    }

    fn close_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        let closed = self.tabs.remove(index);
        for pane in &closed.panes {
            self.discard_session(pane, window, cx);
        }
        if self.tabs.is_empty() {
            self.add_tab(window, cx)
                .update(cx, |session, cx| session.focus(window, cx));
            return;
        }
        if self.active > index || self.active == self.tabs.len() {
            self.active = self.active.saturating_sub(1);
        }
        let active = self.active;
        self.activate(active, window, cx);
    }

    /// Gives the keyboard focus to pane `pane` of the active tab.
    fn focus_pane(&mut self, pane: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        if pane >= tab.panes.len() {
            return;
        }
        tab.focused = pane;
        let session = tab.panes[pane].clone();
        session.update(cx, |session, cx| session.focus(window, cx));
        self.update_title(window, cx);
        cx.notify();
    }

    fn update_title(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.active_session() {
            window.set_window_title(&session.read(cx).window_title(cx));
        }
    }

    fn on_session_event(
        &mut self,
        session: &Entity<Session>,
        event: &SessionEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            SessionEvent::Changed => {
                if self.active_session() == Some(session) {
                    self.update_title(window, cx);
                }
                cx.notify();
            }
            SessionEvent::Connected {
                destination,
                bookmark,
            } => {
                let (destination, bookmark) = (destination.clone(), *bookmark);
                self.change_library(
                    move |library| library.record_connection(&destination, bookmark),
                    cx,
                );
            }
            SessionEvent::Abandoned => {
                let id = session.read(cx).id();
                self.drop_prompts_of(id, window, cx);
            }
            SessionEvent::SaveAsFavorite(destination) => {
                let draft = BookmarkDraft {
                    folder: None,
                    name: String::new(),
                    destination: destination.clone(),
                    identity_file: None,
                };
                self.open_editor(None, &draft, window, cx);
            }
        }
    }

    /// Opens `request` in the active tab, or in a new one.
    fn open(
        &mut self,
        request: ConnectRequest,
        new_tab: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let session = match self.active_session() {
            Some(session) if !new_tab => session.clone(),
            _ => self.add_tab(window, cx),
        };
        session.update(cx, |session, cx| session.connect(request, window, cx));
        cx.notify();
    }

    fn open_bookmark(
        &mut self,
        id: BookmarkId,
        new_tab: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(bookmark) = self.snapshot.bookmark(id) else {
            return;
        };
        let request = ConnectRequest {
            destination: bookmark.destination.clone(),
            bookmark: Some(id),
            identity_file: bookmark.identity_file.clone(),
        };
        self.open(request, new_tab, window, cx);
    }

    // ---------------------------------------------------------------------
    // Actions

    fn on_focus_address(&mut self, _: &FocusAddress, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.active_session() {
            session.update(cx, |session, cx| session.focus_address(window, cx));
        }
    }

    fn on_new_tab(&mut self, _: &NewTab, window: &mut Window, cx: &mut Context<Self>) {
        self.add_tab(window, cx)
            .update(cx, |session, cx| session.focus(window, cx));
    }

    fn on_close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        let active = self.active;
        self.close_tab(active, window, cx);
    }

    fn on_next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        let next = (self.active + 1) % self.tabs.len().max(1);
        self.activate(next, window, cx);
    }

    fn on_previous_tab(&mut self, _: &PreviousTab, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.tabs.len().max(1);
        let previous = (self.active + count - 1) % count;
        self.activate(previous, window, cx);
    }

    fn on_toggle_sidebar(&mut self, _: &ToggleSidebar, _: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_open = !self.sidebar_open;
        cx.notify();
    }

    fn on_toggle_split(&mut self, _: &ToggleSplit, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        if tab.panes.len() == 2 {
            // Close the other pane and stay where the focus is.
            let other = 1 - tab.focused.min(1);
            let closed = self.tabs[self.active].panes.remove(other);
            self.tabs[self.active].focused = 0;
            self.discard_session(&closed, window, cx);
            self.focus_pane(0, window, cx);
            return;
        }
        // The classic layout: a server on one side, this computer on the other.
        let remote_shown = tab
            .active()
            .read(cx)
            .browser()
            .is_some_and(|browser| !browser.read(cx).remote().is_local());
        let pane = self.new_session(window, cx);
        let tab = &mut self.tabs[self.active];
        tab.panes.push(pane.clone());
        tab.focused = 1;
        pane.update(cx, |pane, cx| {
            if remote_shown {
                pane.connect(
                    ConnectRequest::to(Destination::Local { path: None }),
                    window,
                    cx,
                );
            } else {
                pane.focus(window, cx);
            }
        });
        cx.notify();
    }

    fn on_focus_other_pane(
        &mut self,
        _: &actions::FocusOtherPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(tab) = self.tabs.get(self.active)
            && tab.panes.len() == 2
        {
            let other = 1 - tab.focused.min(1);
            self.focus_pane(other, window, cx);
        }
    }

    /// F5: copies the selection of the focused pane into the other pane's
    /// folder. Without a second pane it refreshes, as F5 traditionally does.
    fn on_copy_to_other_pane(
        &mut self,
        _: &actions::CopyToOtherPane,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let Some(source) = tab.active().read(cx).browser().cloned() else {
            return;
        };
        let target = tab
            .other()
            .and_then(|other| other.read(cx).browser().cloned());
        let Some(target) = target else {
            if tab.other().is_some() {
                self.notice("Open a folder in the other pane first.".into());
            } else {
                source.update(cx, FileBrowser::reload);
            }
            return;
        };
        let (remote, items) = {
            let source = source.read(cx);
            (Arc::clone(source.remote()), source.selected_paths())
        };
        target.update(cx, |target, cx| {
            let dir = target.path().clone();
            target.receive(remote, items, dir, cx);
        });
    }

    fn on_toggle_transfers(&mut self, _: &ToggleTransfers, _: &mut Window, cx: &mut Context<Self>) {
        self.transfers_open = !self.transfers_open;
        cx.notify();
    }

    fn on_transfer_event(
        &mut self,
        event: TransferEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            TransferEvent::Changed(id) => {
                // A new transfer shows the panel; later changes do not reopen it.
                if self.seen_transfers.insert(id) {
                    self.transfers_open = true;
                }
            }
            TransferEvent::Settled {
                id,
                target,
                target_dir,
            } => {
                // Questions of a transfer that stopped can no longer matter.
                self.drop_conflicts_of(id, window, cx);
                let browsers: Vec<Entity<FileBrowser>> = self
                    .all_sessions()
                    .filter_map(|session| session.read(cx).browser().cloned())
                    .collect();
                for browser in browsers {
                    let shows_target = {
                        let browser = browser.read(cx);
                        let same_remote = Arc::ptr_eq(browser.remote(), &target)
                            || (browser.remote().is_local() && target.is_local());
                        same_remote && browser.path() == &target_dir
                    };
                    if shows_target {
                        browser.update(cx, FileBrowser::reload);
                    }
                }
            }
        }
        self.transfer_panel.update(cx, TransferPanel::refresh);
        cx.notify();
    }

    fn on_save_favorite(&mut self, _: &SaveFavorite, window: &mut Window, cx: &mut Context<Self>) {
        let Some(location) = self
            .active_session()
            .and_then(|session| session.read(cx).location(cx))
        else {
            return;
        };
        let draft = BookmarkDraft {
            folder: None,
            name: String::new(),
            destination: location,
            identity_file: None,
        };
        self.open_editor(None, &draft, window, cx);
    }

    // ---------------------------------------------------------------------
    // Rendering

    fn render_tabs(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity().downgrade();
        let tabs: Vec<Tab> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                let session = tab.active().read(cx);
                let close = {
                    let this = this.clone();
                    Button::new(("close-tab", index))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .on_click(move |_: &ClickEvent, window, cx| {
                            cx.stop_propagation();
                            let _ = this.update(cx, |workspace, cx| {
                                workspace.close_tab(index, window, cx);
                            });
                        })
                };
                let icon = if session.is_connecting() {
                    IconName::LoaderCircle
                } else if session.browser().is_some() {
                    IconName::Globe
                } else {
                    IconName::Plus
                };
                Tab::new()
                    .prefix(div().pl_2().child(Icon::new(icon).size_3p5()))
                    .label(session.title(cx))
                    .suffix(close)
            })
            .collect();
        let new_tab = Button::new("new-tab")
            .ghost()
            .small()
            .icon(IconName::Plus)
            .tooltip("New tab")
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.on_new_tab(&NewTab, window, cx);
            }));
        let sidebar_toggle = Button::new("toggle-sidebar")
            .ghost()
            .small()
            .icon(Icon::new(gpui_component_assets::IconName::PanelLeft))
            .tooltip("Toggle sidebar")
            .selected(self.sidebar_visible(window))
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.on_toggle_sidebar(&ToggleSidebar, window, cx);
            }));
        let split = Button::new("toggle-split")
            .ghost()
            .small()
            .icon(Icon::new(gpui_component_assets::IconName::PanelRight))
            .tooltip("Split view")
            .selected(
                self.tabs
                    .get(self.active)
                    .is_some_and(|tab| tab.panes.len() == 2),
            )
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.on_toggle_split(&ToggleSplit, window, cx);
            }));
        let transfers = self.render_transfers_toggle(cx);
        TabBar::new("tabs")
            .small()
            .prefix(h_flex().px_1().child(sidebar_toggle))
            .suffix(
                h_flex()
                    .px_1()
                    .gap_1()
                    .child(new_tab)
                    .child(split)
                    .child(transfers),
            )
            .selected_index(self.active)
            .on_click(move |index: &usize, window, cx| {
                let index = *index;
                let _ = this.update(cx, |workspace, cx| workspace.activate(index, window, cx));
            })
            .children(tabs)
    }

    /// The tab bar button that shows the transfers, with overall progress.
    fn render_transfers_toggle(&self, cx: &mut Context<Self>) -> Button {
        let panel = self.transfer_panel.read(cx);
        let label: Option<SharedString> = match panel.overall_progress() {
            Some(progress) if panel.active_count() > 0 => {
                #[expect(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "a percentage between 0 and 100"
                )]
                let percent = (progress * 100.0).round() as u32;
                Some(format!("{percent}%").into())
            }
            _ => None,
        };
        Button::new("toggle-transfers")
            .ghost()
            .small()
            .icon(IconName::ArrowDown)
            .when_some(label, Button::label)
            .tooltip("Transfers")
            .selected(self.transfers_open)
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.on_toggle_transfers(&ToggleTransfers, window, cx);
            }))
    }

    /// The panes of the active tab, side by side.
    fn render_panes(&self, cx: &mut Context<Self>) -> gpui::Div {
        let theme = cx.theme();
        let (border, accent) = (theme.border, theme.primary);
        let Some(tab) = self.tabs.get(self.active) else {
            return h_flex().flex_1().min_w_0();
        };
        let split = tab.panes.len() == 2;
        let focused = tab.focused;
        h_flex()
            .flex_1()
            .min_w_0()
            .items_stretch()
            .children(tab.panes.iter().enumerate().map(|(index, pane)| {
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .when(index > 0, |this| this.border_l_1().border_color(border))
                    // In a split, a thin line marks where the keyboard goes.
                    .when(split, |this| {
                        this.child(
                            div()
                                .h(px(2.))
                                .flex_shrink_0()
                                .when(index == focused, |line| line.bg(accent)),
                        )
                    })
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, _: &gpui::MouseDownEvent, window, cx| {
                            if let Some(tab) = this.tabs.get_mut(this.active)
                                && tab.focused != index
                            {
                                tab.focused = index;
                                this.update_title(window, cx);
                                cx.notify();
                            }
                        }),
                    )
                    .child(div().flex_1().min_h_0().child(pane.clone()))
            }))
    }

    fn sidebar_visible(&self, window: &Window) -> bool {
        self.sidebar_open && window.viewport_size().width >= SIDEBAR_MIN_WINDOW_WIDTH
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (background, foreground, border) = (theme.background, theme.foreground, theme.border);
        let sidebar = self
            .sidebar_visible(window)
            .then(|| self.render_sidebar(cx));
        let content = self.render_panes(cx);
        v_flex()
            .relative()
            .size_full()
            .key_context(WORKSPACE_CONTEXT)
            .on_action(cx.listener(Self::on_focus_address))
            .on_action(cx.listener(Self::on_new_tab))
            .on_action(cx.listener(Self::on_close_tab))
            .on_action(cx.listener(Self::on_next_tab))
            .on_action(cx.listener(Self::on_previous_tab))
            .on_action(cx.listener(Self::on_toggle_sidebar))
            .on_action(cx.listener(Self::on_save_favorite))
            .on_action(cx.listener(Self::on_toggle_split))
            .on_action(cx.listener(Self::on_toggle_transfers))
            .on_action(cx.listener(Self::on_focus_other_pane))
            .on_action(cx.listener(Self::on_copy_to_other_pane))
            .bg(background)
            .text_color(foreground)
            .child(self.render_tabs(window, cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .when_some(sidebar, |this, sidebar| {
                        this.child(sidebar.border_r_1().border_color(border))
                    })
                    .child(content),
            )
            .when(self.transfers_open, |this| {
                this.child(
                    div()
                        .h(TRANSFER_PANEL_HEIGHT)
                        .flex_shrink_0()
                        .border_t_1()
                        .border_color(border)
                        .child(self.transfer_panel.clone()),
                )
            })
            .children(self.render_dialog(cx))
            .children(self.render_prompt(cx))
    }
}
