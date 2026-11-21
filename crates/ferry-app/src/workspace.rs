//! The main window: tabs, the sidebar with favorites, and dialogs.

mod dialogs;
mod sidebar;

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use ferry_core::Destination;
use ferry_core::library::{
    BookmarkDraft, BookmarkId, FolderId, Library, LibraryError, LibrarySnapshot,
};
use ferry_core::remote::sftp::{SshConfigSource, configured_hosts};
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
use crate::browser::FileBrowser;
use crate::prompts::{self, PromptRequest, SessionId, TaggedPrompt};
use crate::runtime;
use crate::session::{ConnectRequest, Session, SessionContext, SessionEvent};
use crate::vault::Vault;

/// Key context of the whole window.
const WORKSPACE_CONTEXT: &str = "Workspace";

/// Below this window width the sidebar hides itself.
const SIDEBAR_MIN_WINDOW_WIDTH: gpui::Pixels = px(720.);

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
    sessions: Vec<Entity<Session>>,
    session_events: Vec<Subscription>,
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
            sessions: Vec::new(),
            session_events: Vec::new(),
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
            tasks: Vec::new(),
            _appearance: appearance,
        };
        workspace.tasks = vec![
            prompt_listener,
            workspace.load_ssh_hosts(cx),
            Self::load_library(options.ephemeral, cx),
        ];

        let session = workspace.add_session(window, cx);
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
        self.sessions.get(self.active)
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
        for session in &self.sessions {
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

    fn add_session(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<Session> {
        let id = SessionId(self.next_session);
        self.next_session += 1;
        let context = self.context.clone();
        let snapshot = Arc::clone(&self.snapshot);
        let session = cx.new(|cx| Session::new(id, context, snapshot, window, cx));
        let events = cx.subscribe_in(&session, window, Self::on_session_event);
        self.sessions.push(session.clone());
        self.session_events.push(events);
        self.active = self.sessions.len() - 1;
        self.update_title(window, cx);
        cx.notify();
        session
    }

    fn activate(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.sessions.len() {
            return;
        }
        self.active = index;
        self.sessions[index].update(cx, |session, cx| session.focus(window, cx));
        self.update_title(window, cx);
        cx.notify();
    }

    fn close_session(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.sessions.len() {
            return;
        }
        let closed = self.sessions.remove(index).read(cx).id();
        drop(self.session_events.remove(index));
        self.drop_prompts_of(closed, window, cx);
        if self.sessions.is_empty() {
            self.add_session(window, cx)
                .update(cx, |session, cx| session.focus(window, cx));
            return;
        }
        if self.active > index || self.active == self.sessions.len() {
            self.active = self.active.saturating_sub(1);
        }
        let active = self.active;
        self.activate(active, window, cx);
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
            _ => self.add_session(window, cx),
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
        self.add_session(window, cx)
            .update(cx, |session, cx| session.focus(window, cx));
    }

    fn on_close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        let active = self.active;
        self.close_session(active, window, cx);
    }

    fn on_next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        let next = (self.active + 1) % self.sessions.len().max(1);
        self.activate(next, window, cx);
    }

    fn on_previous_tab(&mut self, _: &PreviousTab, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.sessions.len().max(1);
        let previous = (self.active + count - 1) % count;
        self.activate(previous, window, cx);
    }

    fn on_toggle_sidebar(&mut self, _: &ToggleSidebar, _: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_open = !self.sidebar_open;
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
            .sessions
            .iter()
            .enumerate()
            .map(|(index, session)| {
                let session = session.read(cx);
                let close = {
                    let this = this.clone();
                    Button::new(("close-tab", index))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .on_click(move |_: &ClickEvent, window, cx| {
                            cx.stop_propagation();
                            let _ = this.update(cx, |workspace, cx| {
                                workspace.close_session(index, window, cx);
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
        TabBar::new("tabs")
            .small()
            .prefix(h_flex().px_1().child(sidebar_toggle))
            .suffix(h_flex().px_1().child(new_tab))
            .selected_index(self.active)
            .on_click(move |index: &usize, window, cx| {
                let index = *index;
                let _ = this.update(cx, |workspace, cx| workspace.activate(index, window, cx));
            })
            .children(tabs)
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
        let content = self
            .active_session()
            .map(|session| div().flex_1().min_w_0().child(session.clone()));
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
                    .children(content),
            )
            .children(self.render_dialog(cx))
            .children(self.render_prompt(cx))
    }
}
