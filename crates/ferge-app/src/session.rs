//! One pane of a tab: the files of a connection, or the server panel to pick
//! one. Panes come in pairs like in classic dual-pane file transfer clients:
//! this computer on one side, a server on the other.

mod header;
mod servers;

use std::path::PathBuf;
use std::sync::Arc;

use ferge_core::library::{BookmarkId, LibrarySnapshot};
use ferge_core::remote::local::LocalRemote;
use ferge_core::remote::sftp::{SftpRemote, SshConfigSource};
use ferge_core::{Destination, EntryKind, Remote, RemotePath};
use futures::channel::mpsc;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, AppContext as _, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable as _,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString, Styled as _,
    Subscription, Task, WeakEntity, Window, div,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::InputState;
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_component_assets::IconName as ShieldIcon;
use secrecy::SecretString;

pub(crate) use self::servers::LibraryAction;
use crate::actions;
use crate::browser::{BrowserEvent, FileBrowser, Navigated, ViewOptions};
use crate::interaction::UiInteraction;
use crate::prompts::{Prompter, SessionId, TaggedPrompt};
use crate::runtime;
use crate::style::{self, ControlSize, IconButton, Palette as _, TypeScale as _, em};
use crate::vault::Vault;

/// What to connect to, and where it came from.
#[derive(Clone, Debug)]
pub(crate) struct ConnectRequest {
    pub destination: Destination,
    /// The favorite it was opened from, recorded in the history.
    pub bookmark: Option<BookmarkId>,
    /// A key to try before the configured ones.
    pub identity_file: Option<PathBuf>,
    /// A password entered up front (Quick Connect).
    pub password: Option<SecretString>,
    /// Save `password` once it worked.
    pub remember_password: bool,
}

impl ConnectRequest {
    pub(crate) fn to(destination: Destination) -> Self {
        Self {
            destination,
            bookmark: None,
            identity_file: None,
            password: None,
            remember_password: false,
        }
    }
}

/// Shared by all panes of a window.
#[derive(Clone, Debug)]
pub(crate) struct SessionContext {
    pub prompts: mpsc::UnboundedSender<TaggedPrompt>,
    pub vault: Arc<Vault>,
    /// `-F` from the command line; `None` means `~/.ssh/config`.
    pub ssh_config: Option<PathBuf>,
}

/// Notifications for the window.
#[derive(Clone, Debug)]
pub(crate) enum SessionEvent {
    /// The title, the location or the selection changed.
    Changed,
    /// A connection was established.
    Connected {
        destination: Destination,
        bookmark: Option<BookmarkId>,
    },
    /// A connection attempt was abandoned; its open questions are void.
    Abandoned,
    /// The server panel needs one of the window's dialogs.
    Library(LibraryAction),
    /// Open this in a new tab.
    OpenInNewTab(ConnectRequest),
    /// Delete these from `browser` once the user confirmed.
    ConfirmDelete {
        browser: WeakEntity<FileBrowser>,
        items: Vec<(RemotePath, EntryKind)>,
    },
    /// Something failed that the user should know about.
    Failed(String),
    /// The pane was clicked and should take the keyboard.
    Focused,
}

impl EventEmitter<SessionEvent> for Session {}

/// A connection being established: the remote, its start directory and, for
/// SFTP, the session it logged in with.
type PendingConnection =
    tokio::task::JoinHandle<ferge_core::Result<(Arc<dyn Remote>, RemotePath, Option<SftpRemote>)>>;

enum ConnectState {
    Idle,
    Connecting(String),
    Failed(String),
    /// The server refused something that working as root could do.
    Denied(String),
}

/// Working as root on the pane's server.
enum Root {
    /// Working as the login user.
    Off,
    /// Opening a session as root. Dropping the task ignores its outcome.
    Opening {
        _task: Task<()>,
        abort: tokio::task::AbortHandle,
    },
    /// The browser works on a session as root.
    On,
}

/// Whether the pane shows its files or the server panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Files,
    Servers,
}

struct Connection {
    /// Where the connection went, without the current directory.
    destination: Destination,
    browser: Entity<FileBrowser>,
    /// The SFTP session of the login, which can open one as root.
    login: Option<SftpRemote>,
    root: Root,
    _events: [Subscription; 2],
}

impl Drop for Connection {
    fn drop(&mut self) {
        // Stop asking for a sudo password on behalf of a connection that is gone.
        if let Root::Opening { abort, .. } = &self.root {
            abort.abort();
        }
    }
}

/// The "go to folder" field that replaces the path bar while it is open.
struct GoTo {
    input: Entity<InputState>,
    _events: Subscription,
}

pub(crate) struct Session {
    id: SessionId,
    context: SessionContext,
    mode: Mode,
    /// Whether this pane has the keyboard within its tab.
    active: bool,
    state: ConnectState,
    connection: Option<Connection>,
    connect_task: Option<Task<()>>,
    /// Aborts the attempt on the runtime; dropping the UI task alone would
    /// leave it running (and possibly asking questions).
    connect_abort: Option<tokio::task::AbortHandle>,
    library: Arc<LibrarySnapshot>,
    ssh_hosts: Arc<[SharedString]>,
    options: ViewOptions,
    servers: servers::ServerPanel,
    go_to: Option<GoTo>,
    /// Where the current connection attempt goes.
    connecting_to: Option<Destination>,
    focus: FocusHandle,
}

impl Session {
    pub(crate) fn new(
        id: SessionId,
        context: SessionContext,
        library: Arc<LibrarySnapshot>,
        ssh_hosts: Arc<[SharedString]>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            id,
            context,
            mode: Mode::Servers,
            active: false,
            state: ConnectState::Idle,
            connection: None,
            connect_task: None,
            connect_abort: None,
            library,
            ssh_hosts,
            options: ViewOptions::default(),
            servers: servers::ServerPanel::new(window, cx),
            go_to: None,
            connecting_to: None,
            focus: cx.focus_handle(),
        }
    }

    pub(crate) fn id(&self) -> SessionId {
        self.id
    }

    pub(crate) fn browser(&self) -> Option<&Entity<FileBrowser>> {
        self.connection
            .as_ref()
            .map(|connection| &connection.browser)
    }

    /// The current location, including the directory shown.
    pub(crate) fn location(&self, cx: &App) -> Option<Destination> {
        let connection = self.connection.as_ref()?;
        let path = connection.browser.read(cx).path().clone();
        Some(connection.destination.with_path(Some(path)))
    }

    pub(crate) fn is_connecting(&self) -> bool {
        matches!(self.state, ConnectState::Connecting(_))
    }

    /// Where this pane is connecting to right now.
    pub(crate) fn connecting_to(&self) -> Option<&Destination> {
        self.connecting_to.as_ref().filter(|_| self.is_connecting())
    }

    fn shows_servers(&self) -> bool {
        self.mode == Mode::Servers || self.connection.is_none()
    }

    /// Short title, e.g. for tabs: `folder — host`.
    pub(crate) fn title(&self, cx: &App) -> SharedString {
        if let ConnectState::Connecting(host) = &self.state {
            return format!("Connecting to {host}…").into();
        }
        match &self.connection {
            Some(connection) => {
                let browser = connection.browser.read(cx);
                let folder = browser.path().file_name().unwrap_or("/");
                format!(
                    "{} — {}",
                    crate::format::name(folder),
                    connection.destination.title()
                )
                .into()
            }
            None => "Servers".into(),
        }
    }

    /// Title for the window while this pane is active.
    pub(crate) fn window_title(&self, cx: &App) -> String {
        match &self.connection {
            Some(connection) => {
                let browser = connection.browser.read(cx);
                format!("{} – {} – ferge", browser.path(), browser.remote_label())
            }
            None => "ferge".to_owned(),
        }
    }

    pub(crate) fn set_library(&mut self, library: Arc<LibrarySnapshot>, cx: &mut Context<Self>) {
        self.library = library;
        cx.notify();
    }

    pub(crate) fn set_ssh_hosts(&mut self, hosts: Arc<[SharedString]>, cx: &mut Context<Self>) {
        self.ssh_hosts = hosts;
        cx.notify();
    }

    /// Marks whether this pane has the keyboard; the active pane's path is
    /// highlighted.
    pub(crate) fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.active != active {
            self.active = active;
            cx.notify();
        }
    }

    /// Moves the keyboard focus into this pane.
    pub(crate) fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.connection {
            Some(connection) if self.mode == Mode::Files => {
                window.focus(&connection.browser.focus_handle(cx), cx);
            }
            _ => self.servers.focus(window, cx),
        }
    }

    /// Connects this pane to `request`, replacing the current connection once
    /// the new one is established.
    pub(crate) fn connect(
        &mut self,
        request: ConnectRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.abandon_attempt(cx);
        self.go_to = None;
        let interaction = Arc::new(UiInteraction::new(
            Prompter::new(self.context.prompts.clone(), Some(self.id)),
            Arc::clone(&self.context.vault),
        ));
        let label = request.destination.title();
        let connecting = self.start_connection(&request, &interaction, cx);
        self.connect_abort = Some(connecting.abort_handle());
        self.state = ConnectState::Connecting(label);
        self.connecting_to = Some(request.destination.clone());
        self.connect_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = match connecting.await {
                Ok(result) => result.map_err(|error| error.to_string()),
                Err(error) if error.is_cancelled() => return,
                Err(error) => Err(format!("the connection attempt failed: {error}")),
            };
            let _ = this.update_in(cx, |session, window, cx| {
                session.connect_task = None;
                session.connect_abort = None;
                match result {
                    Ok((remote, path, login)) => {
                        let account = login.as_ref().map(|login| login.account().clone());
                        // Remembered secrets are saved only now that they worked.
                        let vault = Arc::clone(&session.context.vault);
                        let prompter = Prompter::new(session.context.prompts.clone(), None);
                        let typed = request
                            .remember_password
                            .then(|| request.password.clone())
                            .flatten()
                            .zip(account);
                        runtime::spawn(cx, async move {
                            interaction.commit().await;
                            if let Some((password, account)) = typed {
                                vault.save(account.password_id(), password, &prompter).await;
                            }
                        });
                        session.show_browser(&request, remote, path, login, window, cx);
                    }
                    Err(message) => {
                        session.state = ConnectState::Failed(message);
                        if session.connection.is_none() {
                            session.servers.focus(window, cx);
                        }
                    }
                }
                cx.emit(SessionEvent::Changed);
                cx.notify();
            });
        }));
        cx.emit(SessionEvent::Changed);
        cx.notify();
    }

    /// Cancels a connection attempt in progress.
    fn abandon_attempt(&mut self, cx: &mut Context<Self>) {
        self.connect_task = None;
        if let Some(attempt) = self.connect_abort.take() {
            attempt.abort();
            cx.emit(SessionEvent::Abandoned);
        }
        if self.is_connecting() {
            self.state = ConnectState::Idle;
        }
    }

    fn start_connection(
        &self,
        request: &ConnectRequest,
        interaction: &Arc<UiInteraction>,
        cx: &mut Context<Self>,
    ) -> PendingConnection {
        let start = request.destination.path().cloned();
        match &request.destination {
            Destination::Local { .. } => runtime::spawn(cx, async move {
                let remote: Arc<dyn Remote> = Arc::new(LocalRemote::new());
                let start = match start {
                    Some(path) => path,
                    None => remote.home_dir().await?,
                };
                Ok((remote, start, None))
            }),
            Destination::Sftp(target) => {
                let mut options = target.options();
                if let Some(file) = &self.context.ssh_config {
                    options.ssh_config = SshConfigSource::File(file.clone());
                }
                options.identity_files.extend(request.identity_file.clone());
                options.password.clone_from(&request.password);
                let interaction = Arc::clone(interaction);
                runtime::spawn(cx, async move {
                    let sftp = SftpRemote::connect(options, interaction).await?;
                    let remote: Arc<dyn Remote> = Arc::new(sftp.clone());
                    let start = match start {
                        Some(path) => path,
                        None => remote.home_dir().await?,
                    };
                    Ok((remote, start, Some(sftp)))
                })
            }
        }
    }

    fn show_browser(
        &mut self,
        request: &ConnectRequest,
        remote: Arc<dyn Remote>,
        path: RemotePath,
        login: Option<SftpRemote>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let options = self.options;
        let address = &request.destination;
        let can_elevate = login.is_some();
        let browser = cx.new(|cx| {
            let mut browser = FileBrowser::new(remote, address, path, options, cx);
            browser.set_can_elevate(can_elevate, cx);
            browser
        });
        let navigated = cx.subscribe_in(&browser, window, |_, _, _: &Navigated, _, cx| {
            cx.emit(SessionEvent::Changed);
        });
        let requests = cx.subscribe_in(
            &browser,
            window,
            |this, browser, event: &BrowserEvent, window, cx| match event {
                BrowserEvent::Delete(items) => cx.emit(SessionEvent::ConfirmDelete {
                    browser: browser.downgrade(),
                    items: items.clone(),
                }),
                BrowserEvent::Failed(message) => cx.emit(SessionEvent::Failed(message.clone())),
                BrowserEvent::Denied(message) => {
                    this.state = ConnectState::Denied(message.clone());
                    cx.notify();
                }
                BrowserEvent::WorkAsRoot => this.become_root(window, cx),
            },
        );
        cx.emit(SessionEvent::Connected {
            destination: request.destination.clone(),
            bookmark: request.bookmark,
        });
        if self.opens_root() {
            self.leave_root(cx);
        }
        self.connection = Some(Connection {
            destination: request.destination.with_path(None),
            browser,
            login,
            root: Root::Off,
            _events: [navigated, requests],
        });
        self.state = ConnectState::Idle;
        self.mode = Mode::Files;
        // Only the pane the user works in takes the keyboard; a pane that
        // finishes connecting in the background must not steal it.
        if self.active {
            self.focus(window, cx);
        }
    }

    /// Closes the connection and shows the server panel.
    pub(crate) fn disconnect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.abandon_attempt(cx);
        if self.opens_root() {
            // Withdraws its password question.
            self.leave_root(cx);
        }
        self.connection = None;
        self.go_to = None;
        self.mode = Mode::Servers;
        self.servers.focus(window, cx);
        cx.emit(SessionEvent::Changed);
        cx.notify();
    }

    fn set_options(&mut self, options: ViewOptions, cx: &mut Context<Self>) {
        self.options = options;
        if let Some(browser) = self.browser().cloned() {
            browser.update(cx, |browser, cx| browser.set_options(options, cx));
        }
        cx.notify();
    }

    fn on_disconnect(
        &mut self,
        _: &actions::Disconnect,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.connection.is_some() || self.is_connecting() {
            self.disconnect(window, cx);
        }
    }

    fn on_toggle_servers(
        &mut self,
        _: &actions::ToggleServers,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_servers(window, cx);
    }

    fn toggle_servers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.connection.is_none() {
            return;
        }
        self.mode = match self.mode {
            Mode::Files => Mode::Servers,
            Mode::Servers => Mode::Files,
        };
        self.focus(window, cx);
        cx.emit(SessionEvent::Changed);
        cx.notify();
    }

    // ---------------------------------------------------------------------
    // Working as root

    /// Whether the pane's server can be worked on as root.
    pub(crate) fn can_work_as_root(&self) -> bool {
        self.connection
            .as_ref()
            .is_some_and(|connection| connection.login.is_some())
    }

    /// Whether the pane works as root, or is about to.
    pub(crate) fn works_as_root(&self) -> bool {
        self.connection
            .as_ref()
            .is_some_and(|connection| !matches!(connection.root, Root::Off))
    }

    fn opens_root(&self) -> bool {
        self.connection
            .as_ref()
            .is_some_and(|connection| matches!(connection.root, Root::Opening { .. }))
    }

    pub(crate) fn toggle_root(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.works_as_root() {
            self.leave_root(cx);
        } else {
            self.become_root(window, cx);
        }
    }

    /// Opens a session as root on the same connection and switches the
    /// browser to it, staying in the current folder.
    fn become_root(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let interaction = Arc::new(UiInteraction::new(
            Prompter::new(self.context.prompts.clone(), Some(self.id)),
            Arc::clone(&self.context.vault),
        ));
        let Some(connection) = self.connection.as_mut() else {
            return;
        };
        let Some(login) = connection.login.clone() else {
            return;
        };
        if !matches!(connection.root, Root::Off) {
            return;
        }
        let opening = runtime::spawn(cx, {
            let interaction: Arc<dyn ferge_core::interaction::Interaction> =
                Arc::clone(&interaction) as _;
            async move { login.elevate(interaction).await }
        });
        let abort = opening.abort_handle();
        let task = cx.spawn_in(window, async move |this, cx| {
            let outcome = match opening.await {
                Ok(Ok(root)) => Some(Ok(root)),
                Ok(Err(error)) if error.is_cancelled() => None,
                Ok(Err(error)) => Some(Err(error.to_string())),
                Err(error) if error.is_cancelled() => return,
                Err(error) => Some(Err(format!("working as root failed: {error}"))),
            };
            let _ = this.update(cx, |session, cx| {
                session.root_opened(outcome, interaction, cx);
            });
        });
        connection.root = Root::Opening { _task: task, abort };
        if matches!(
            self.state,
            ConnectState::Denied(_) | ConnectState::Failed(_)
        ) {
            self.state = ConnectState::Idle;
        }
        self.sync_root(cx);
    }

    fn root_opened(
        &mut self,
        outcome: Option<Result<SftpRemote, String>>,
        interaction: Arc<UiInteraction>,
        cx: &mut Context<Self>,
    ) {
        let Some(connection) = self.connection.as_mut() else {
            return;
        };
        connection.root = Root::Off;
        match outcome {
            Some(Ok(root)) => {
                // A sudo password the user asked to remember is saved now that it worked.
                runtime::spawn(cx, async move { interaction.commit().await });
                let remote: Arc<dyn Remote> = Arc::new(root);
                connection
                    .browser
                    .update(cx, |browser, cx| browser.set_remote(remote, cx));
                connection.root = Root::On;
            }
            Some(Err(message)) => self.state = ConnectState::Failed(message),
            None => {}
        }
        self.sync_root(cx);
    }

    /// Goes back to the login user. Transfers started as root finish as root:
    /// the root session closes once nothing uses it any more.
    fn leave_root(&mut self, cx: &mut Context<Self>) {
        let Some(connection) = self.connection.as_mut() else {
            return;
        };
        match std::mem::replace(&mut connection.root, Root::Off) {
            Root::On => {
                if let Some(login) = connection.login.clone() {
                    let remote: Arc<dyn Remote> = Arc::new(login);
                    connection
                        .browser
                        .update(cx, |browser, cx| browser.set_remote(remote, cx));
                }
            }
            Root::Opening { abort, .. } => {
                abort.abort();
                // Its password question is void.
                cx.emit(SessionEvent::Abandoned);
            }
            Root::Off => return,
        }
        self.sync_root(cx);
    }

    /// Lets the browser offer root while the pane works as the login user.
    fn sync_root(&mut self, cx: &mut Context<Self>) {
        if let Some(connection) = &self.connection {
            let can = connection.login.is_some() && matches!(connection.root, Root::Off);
            connection
                .browser
                .update(cx, |browser, cx| browser.set_can_elevate(can, cx));
        }
        cx.emit(SessionEvent::Changed);
        cx.notify();
    }

    fn render_banner(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        /// What the banner is about.
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum Kind {
            Progress,
            Failed,
            Denied,
        }
        let theme = cx.theme();
        let (icon, text, kind): (gpui::AnyElement, SharedString, Kind) = match &self.state {
            ConnectState::Connecting(host) => (
                Icon::new(IconName::LoaderCircle)
                    .size(em(1.))
                    .into_any_element(),
                format!("Connecting to {host}…").into(),
                Kind::Progress,
            ),
            ConnectState::Failed(error) => (
                Icon::new(IconName::TriangleAlert)
                    .size(em(1.))
                    .into_any_element(),
                error.clone().into(),
                Kind::Failed,
            ),
            ConnectState::Denied(error) => (
                Icon::new(IconName::TriangleAlert)
                    .size(em(1.))
                    .into_any_element(),
                error.clone().into(),
                Kind::Denied,
            ),
            ConnectState::Idle if self.opens_root() => (
                Icon::new(ShieldIcon::ShieldUser)
                    .size(em(1.))
                    .into_any_element(),
                "Switching to root…".into(),
                Kind::Progress,
            ),
            ConnectState::Idle => return None,
        };
        let dismiss = IconButton::new("dismiss-error", IconName::Close)
            .size(ControlSize::Compact)
            .tooltip("Dismiss")
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.state = ConnectState::Idle;
                cx.notify();
            }));
        Some(
            h_flex()
                .mx_2()
                .mb_1()
                .px_3()
                .py_1p5()
                .gap_2()
                .text_body()
                .flex_shrink_0()
                .rounded(style::GROUP_RADIUS)
                .border_1()
                .map(|this| match kind {
                    Kind::Failed => this
                        .text_color(theme.danger)
                        .border_color(theme.danger.opacity(0.4))
                        .bg(theme.danger.opacity(0.08)),
                    Kind::Denied => this
                        .border_color(theme.root_accent().opacity(0.4))
                        .bg(theme.root_fill()),
                    Kind::Progress => this.border_color(theme.border).bg(theme.group_fill()),
                })
                .child(icon)
                .child(div().flex_1().min_w_0().child(text))
                .map(|this| match kind {
                    Kind::Failed => this.child(dismiss),
                    // One click repeats nothing by itself, but whatever the
                    // user tries next works.
                    Kind::Denied => this
                        .child(
                            Button::new("work-as-root")
                                .small()
                                .label("Work as root")
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.become_root(window, cx);
                                })),
                        )
                        .child(dismiss),
                    Kind::Progress => this.child(
                        Button::new("cancel-connect")
                            .ghost()
                            .small()
                            .label("Cancel")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                if this.is_connecting() {
                                    this.abandon_attempt(cx);
                                    cx.emit(SessionEvent::Changed);
                                    cx.notify();
                                } else {
                                    this.leave_root(cx);
                                }
                            })),
                    ),
                }),
        )
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(attempt) = self.connect_abort.take() {
            attempt.abort();
        }
    }
}

impl Render for Session {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match &self.connection {
            Some(connection) if self.mode == Mode::Files => div()
                .flex_1()
                .min_h_0()
                .child(connection.browser.clone())
                .into_any_element(),
            _ => self.render_servers(window, cx).into_any_element(),
        };
        v_flex()
            .size_full()
            .key_context(actions::PANE)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::on_disconnect))
            .on_action(cx.listener(Self::on_toggle_servers))
            .on_action(cx.listener(|this, _: &actions::ToggleRoot, window, cx| {
                this.toggle_root(window, cx);
            }))
            .on_action(cx.listener(Self::on_go_to_folder))
            .on_action(cx.listener(|this, _: &actions::ToggleHiddenFiles, _, cx| {
                let mut options = this.options;
                options.show_hidden = !options.show_hidden;
                this.set_options(options, cx);
            }))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseDownEvent, _, cx| {
                    if !this.active {
                        cx.emit(SessionEvent::Focused);
                    }
                }),
            )
            .child(self.render_header(cx))
            .children(self.render_banner(cx))
            .child(body)
    }
}
