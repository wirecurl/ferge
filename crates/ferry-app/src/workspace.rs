//! The main window: a toolbar, tabs of two file browsers each (this computer
//! and a server, side by side), the inspector, the transfers and dialogs.

mod dialogs;
mod library;
mod toolbar;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use ferry_core::library::{Library, LibrarySnapshot};
use ferry_core::transfer::{TransferEvent, TransferId};
use ferry_core::{Destination, Remote, RemotePath};
use futures::StreamExt as _;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, AppContext as _, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Subscription, Task, Window, div, px,
};
use gpui_component::input::{InputEvent, InputState};
use gpui_component::{ActiveTheme as _, h_flex, v_flex};

use self::dialogs::{ActivePrompt, ConfirmAction, Dialog};
use crate::actions;
use crate::appearance::Appearance;
use crate::browser::FileBrowser;
use crate::inspector::{INSPECTOR_WIDTH, Inspector};
use crate::prompts::{self, PromptRequest, SessionId, TaggedPrompt};
use crate::runtime;
use crate::session::{ConnectRequest, Session, SessionContext, SessionEvent};
use crate::transfers::{TransferPanel, Transfers};
use crate::vault::Vault;

/// Key context of the whole window.
const WORKSPACE_CONTEXT: &str = "Workspace";

/// Below this width two panes do not fit side by side; one is shown.
const NARROW_WINDOW_WIDTH: gpui::Pixels = px(860.);

/// Height of the transfer panel.
const TRANSFER_PANEL_HEIGHT: gpui::Pixels = px(220.);

gpui::actions!(
    workspace,
    [
        NewTab,
        CloseTab,
        NextTab,
        PreviousTab,
        /// Shows a second file browser next to the current one, or closes it.
        ToggleSplit,
        ToggleTransfers,
        /// Shows or hides the details of the selection.
        ToggleInspector,
        /// Moves the keyboard to the search field.
        FocusSearch,
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
        gpui::KeyBinding::new(primary!("t"), NewTab, context),
        gpui::KeyBinding::new(primary!("w"), CloseTab, context),
        gpui::KeyBinding::new("ctrl-tab", NextTab, context),
        gpui::KeyBinding::new("ctrl-shift-tab", PreviousTab, context),
        gpui::KeyBinding::new(primary!("\\"), ToggleSplit, context),
        gpui::KeyBinding::new(primary!("j"), ToggleTransfers, context),
        gpui::KeyBinding::new(primary!("i"), ToggleInspector, context),
        gpui::KeyBinding::new(primary!("f"), FocusSearch, context),
    ]);
    #[cfg(target_os = "macos")]
    cx.bind_keys([
        gpui::KeyBinding::new("cmd-shift-]", NextTab, context),
        gpui::KeyBinding::new("cmd-shift-[", PreviousTab, context),
        gpui::KeyBinding::new("cmd-alt-left", actions::FocusLeftPane, context),
        gpui::KeyBinding::new("cmd-alt-right", actions::FocusRightPane, context),
    ]);
    #[cfg(not(target_os = "macos"))]
    cx.bind_keys([
        gpui::KeyBinding::new("ctrl-pagedown", NextTab, context),
        gpui::KeyBinding::new("ctrl-pageup", PreviousTab, context),
        gpui::KeyBinding::new("ctrl-alt-left", actions::FocusLeftPane, context),
        gpui::KeyBinding::new("ctrl-alt-right", actions::FocusRightPane, context),
    ]);
    dialogs::bind_keys(cx);
}

/// How the window was started.
#[derive(Debug, Default)]
pub(crate) struct Options {
    /// Connect the server pane of the first tab to this address.
    pub initial: Option<String>,
    /// `-F` from the command line.
    pub ssh_config: Option<PathBuf>,
    /// Keep favorites, history and passwords in memory only (benchmarks).
    pub ephemeral: bool,
}

/// A tab: one pane, or two side by side.
struct Panes {
    panes: Vec<Entity<Session>>,
    /// The pane with the keyboard.
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

pub(crate) struct Workspace {
    context: SessionContext,
    tabs: Vec<Panes>,
    session_events: HashMap<SessionId, Subscription>,
    active: usize,
    next_session: u64,
    library: Option<Arc<Library>>,
    snapshot: Arc<LibrarySnapshot>,
    /// Host aliases from the SSH configuration.
    ssh_hosts: Arc<[SharedString]>,
    queued_prompts: VecDeque<TaggedPrompt>,
    prompt: Option<ActivePrompt>,
    dialog: Option<Dialog>,
    prompt_focus: FocusHandle,
    transfer_panel: Entity<TransferPanel>,
    transfers_open: bool,
    /// Transfers already seen, so only new ones open the panel.
    seen_transfers: HashSet<TransferId>,
    inspector: Entity<Inspector>,
    inspector_open: bool,
    /// The toolbar's search field; it filters the active file browser.
    filter: Entity<InputState>,
    /// The last problem with the settings that was shown.
    shown_settings_issue: u64,
    /// Background work and subscriptions owned by the window.
    tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub(crate) fn new(options: Options, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Appearance::follow_window(window, cx);
        let appearance = cx.observe_window_appearance(window, |_, window, cx| {
            Appearance::follow_window(window, cx);
        });
        // Problems with the settings are shown here, once each.
        let settings_issues = cx.observe_global::<Appearance>(|this: &mut Self, cx| {
            let Some(issue) = cx.global::<Appearance>().issue() else {
                return;
            };
            if issue.serial > this.shown_settings_issue {
                this.shown_settings_issue = issue.serial;
                this.notice(issue.message.clone());
            }
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

        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter this folder"));
        let filter_events =
            cx.subscribe_in(&filter, window, |this, input, event: &InputEvent, _, cx| {
                if let InputEvent::Change = event {
                    let text = input.read(cx).value().to_string();
                    if let Some(browser) = this.active_browser(cx) {
                        browser.update(cx, |browser, cx| browser.set_filter(text, cx));
                    }
                }
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
            ssh_hosts: Arc::from([]),
            queued_prompts: VecDeque::new(),
            prompt: None,
            dialog: None,
            prompt_focus: cx.focus_handle(),
            transfer_panel: cx.new(TransferPanel::new),
            transfers_open: false,
            seen_transfers: HashSet::new(),
            inspector: cx.new(|_| Inspector::new()),
            inspector_open: false,
            filter,
            tasks: Vec::new(),
            shown_settings_issue: 0,
            _subscriptions: vec![appearance, settings_issues, filter_events],
        };
        workspace.tasks = vec![
            prompt_listener,
            transfer_listener,
            workspace.load_ssh_hosts(cx),
            Self::load_library(options.ephemeral, cx),
        ];

        let initial = options.initial.and_then(|address| {
            Destination::parse(&address)
                .inspect_err(|error| tracing::error!(%address, %error, "invalid destination"))
                .ok()
        });
        workspace.add_tab(initial.map(ConnectRequest::to), window, cx);
        workspace
    }

    /// The browser of the active pane, if it shows one.
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

    /// Shows `message` as a notification. Goes through the prompt channel
    /// because showing it needs the window, which is not at hand everywhere.
    fn notice(&self, message: String) {
        let _ = self.context.prompts.unbounded_send(TaggedPrompt {
            session: None,
            request: PromptRequest::Notice(message),
        });
    }

    fn is_narrow(window: &Window) -> bool {
        window.viewport_size().width < NARROW_WINDOW_WIDTH
    }

    // ---------------------------------------------------------------------
    // Tabs and panes

    /// Creates a pane; the caller places it in a tab.
    fn new_session(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<Session> {
        let id = SessionId(self.next_session);
        self.next_session += 1;
        let context = self.context.clone();
        let snapshot = Arc::clone(&self.snapshot);
        let hosts = Arc::clone(&self.ssh_hosts);
        let session = cx.new(|cx| Session::new(id, context, snapshot, hosts, window, cx));
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

    /// Opens a tab with this computer on the left and the servers (or
    /// `server`) on the right, and makes it active.
    fn add_tab(
        &mut self,
        server: Option<ConnectRequest>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<Session> {
        let local = self.new_session(window, cx);
        let remote = self.new_session(window, cx);
        local.update(cx, |pane, cx| {
            pane.connect(
                ConnectRequest::to(Destination::Local { path: None }),
                window,
                cx,
            );
        });
        if let Some(request) = server {
            remote.update(cx, |pane, cx| pane.connect(request, window, cx));
        }
        self.tabs.push(Panes {
            panes: vec![local, remote.clone()],
            focused: 1,
        });
        self.active = self.tabs.len() - 1;
        self.focus_pane(1, window, cx);
        remote
    }

    fn activate(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        self.active = index;
        let focused = self.tabs[index].focused;
        self.focus_pane(focused, window, cx);
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
            self.add_tab(None, window, cx);
            return;
        }
        if self.active > index || self.active == self.tabs.len() {
            self.active = self.active.saturating_sub(1);
        }
        let active = self.active;
        self.activate(active, window, cx);
    }

    /// Gives the keyboard to pane `pane` of the active tab.
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
        self.sync_active(window, cx);
    }

    /// Marks the active pane, follows it with the inspector, the search field
    /// and the window title.
    fn sync_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let active = self.active_session().cloned();
        let sessions: Vec<Entity<Session>> = self.all_sessions().cloned().collect();
        for session in sessions {
            let is_active = active.as_ref() == Some(&session);
            session.update(cx, |session, cx| session.set_active(is_active, cx));
        }
        let browser = self.active_browser(cx);
        self.inspector.update(cx, |inspector, cx| {
            inspector.set_browser(browser.as_ref(), cx);
        });
        let filter = browser
            .map(|browser| browser.read(cx).filter().to_owned())
            .unwrap_or_default();
        if self.filter.read(cx).value().as_ref() != filter {
            self.filter
                .update(cx, |input, cx| input.set_value(filter, window, cx));
        }
        if let Some(session) = self.active_session() {
            window.set_window_title(&session.read(cx).window_title(cx));
        }
        cx.notify();
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
                    self.sync_active(window, cx);
                }
                cx.notify();
            }
            SessionEvent::Connected {
                destination,
                bookmark,
            } => {
                // This computer is not a connection worth remembering.
                if !matches!(destination, Destination::Local { .. }) {
                    let (destination, bookmark) = (destination.clone(), *bookmark);
                    self.change_library(
                        move |library| library.record_connection(&destination, bookmark),
                        cx,
                    );
                }
            }
            SessionEvent::Abandoned => {
                let id = session.read(cx).id();
                self.drop_prompts_of(id, window, cx);
            }
            SessionEvent::Library(action) => self.on_library_action(action.clone(), window, cx),
            SessionEvent::OpenInNewTab(request) => {
                self.add_tab(Some(request.clone()), window, cx);
            }
            SessionEvent::ConfirmDelete { browser, items } => {
                self.confirm(
                    ConfirmAction::DeleteFiles {
                        browser: browser.clone(),
                        items: items.clone(),
                    },
                    window,
                    cx,
                );
            }
            SessionEvent::Failed(message) => self.notice(message.clone()),
            SessionEvent::Focused => {
                let id = session.read(cx).id();
                if let Some((tab, pane)) = self.locate(id, cx)
                    && tab == self.active
                {
                    self.tabs[tab].focused = pane;
                    self.sync_active(window, cx);
                }
            }
        }
    }

    // ---------------------------------------------------------------------
    // Actions

    fn on_new_tab(&mut self, _: &NewTab, window: &mut Window, cx: &mut Context<Self>) {
        self.add_tab(None, window, cx);
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

    fn on_toggle_split(&mut self, _: &ToggleSplit, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        if tab.panes.len() == 2 {
            // Keep the pane with the keyboard; close the other.
            let other = 1 - tab.focused.min(1);
            let closed = self.tabs[self.active].panes.remove(other);
            self.tabs[self.active].focused = 0;
            self.discard_session(&closed, window, cx);
            self.focus_pane(0, window, cx);
            return;
        }
        // The new side shows the servers next to this computer, and this
        // computer next to a server; this computer goes left.
        let local_shown = tab
            .active()
            .read(cx)
            .browser()
            .is_some_and(|browser| browser.read(cx).remote().is_local());
        let pane = self.new_session(window, cx);
        if local_shown {
            self.tabs[self.active].panes.push(pane);
        } else {
            pane.update(cx, |pane, cx| {
                pane.connect(
                    ConnectRequest::to(Destination::Local { path: None }),
                    window,
                    cx,
                );
            });
            self.tabs[self.active].panes.insert(0, pane);
        }
        self.focus_pane(1, window, cx);
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

    fn on_focus_left(
        &mut self,
        _: &actions::FocusLeftPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_pane(0, window, cx);
    }

    fn on_focus_right(
        &mut self,
        _: &actions::FocusRightPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_pane(1, window, cx);
    }

    /// Copies the selection of the active pane into the other pane's folder.
    /// Without a second pane, F5 refreshes as it traditionally does.
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
                self.notice("Connect the other side to a folder first.".into());
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

    fn on_toggle_inspector(
        &mut self,
        _: &ToggleInspector,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.inspector_open = !self.inspector_open;
        self.sync_active(window, cx);
    }

    fn on_focus_search(&mut self, _: &FocusSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.filter.update(cx, |input, cx| input.focus(window, cx));
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
            TransferEvent::Writing {
                target, target_dir, ..
            } => self.refresh_listings_of(&target, &target_dir, cx),
            TransferEvent::Settled {
                id,
                target,
                target_dir,
            } => {
                // Questions of a transfer that stopped can no longer matter.
                self.drop_conflicts_of(id, window, cx);
                self.refresh_listings_of(&target, &target_dir, cx);
            }
        }
        self.transfer_panel.update(cx, TransferPanel::refresh);
        cx.notify();
    }

    /// Reloads every pane that shows `dir` on `remote`.
    fn refresh_listings_of(
        &mut self,
        remote: &Arc<dyn Remote>,
        dir: &RemotePath,
        cx: &mut Context<Self>,
    ) {
        let browsers: Vec<Entity<FileBrowser>> = self
            .all_sessions()
            .filter_map(|session| session.read(cx).browser().cloned())
            .collect();
        for browser in browsers {
            let shows_dir = {
                let browser = browser.read(cx);
                let same_remote = Arc::ptr_eq(browser.remote(), remote)
                    || (browser.remote().is_local() && remote.is_local());
                same_remote && browser.path() == dir
            };
            if shows_dir {
                browser.update(cx, FileBrowser::reload);
            }
        }
    }

    // ---------------------------------------------------------------------
    // Rendering

    /// The panes of the active tab: side by side, or only the active one in
    /// a narrow window.
    fn render_panes(&self, window: &Window, cx: &mut Context<Self>) -> gpui::Div {
        let border = cx.theme().border;
        let Some(tab) = self.tabs.get(self.active) else {
            return h_flex().flex_1().min_w_0();
        };
        let narrow = Self::is_narrow(window);
        let focused = tab.focused;
        h_flex().flex_1().min_w_0().items_stretch().children(
            tab.panes
                .iter()
                .enumerate()
                .filter(|(index, _)| !narrow || *index == focused)
                .map(|(index, pane)| {
                    div()
                        .flex_1()
                        .min_w_0()
                        .when(index > 0 && !narrow, |this| {
                            this.border_l_1().border_color(border)
                        })
                        .child(pane.clone())
                }),
        )
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (background, foreground, border) = (theme.background, theme.foreground, theme.border);
        let content = self.render_panes(window, cx);
        v_flex()
            .relative()
            .size_full()
            .key_context(WORKSPACE_CONTEXT)
            .on_action(cx.listener(Self::on_new_tab))
            .on_action(cx.listener(Self::on_close_tab))
            .on_action(cx.listener(Self::on_next_tab))
            .on_action(cx.listener(Self::on_previous_tab))
            .on_action(cx.listener(Self::on_toggle_split))
            .on_action(cx.listener(Self::on_toggle_transfers))
            .on_action(cx.listener(Self::on_toggle_inspector))
            .on_action(cx.listener(Self::on_focus_search))
            .on_action(cx.listener(Self::on_focus_other_pane))
            .on_action(cx.listener(Self::on_focus_left))
            .on_action(cx.listener(Self::on_focus_right))
            .on_action(cx.listener(Self::on_copy_to_other_pane))
            .bg(background)
            .text_color(foreground)
            .child(self.render_toolbar(window, cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .child(content)
                    .when(self.inspector_open, |this| {
                        this.child(
                            div()
                                .w(INSPECTOR_WIDTH)
                                .flex_shrink_0()
                                .border_l_1()
                                .border_color(border)
                                .child(self.inspector.clone()),
                        )
                    }),
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
