//! One pane of a tab: the files of a connection, or the server panel to pick
//! one. Panes come in pairs like in classic dual-pane file transfer clients:
//! this computer on one side, a server on the other.

mod header;
mod servers;

use std::path::PathBuf;
use std::sync::Arc;

use ferry_core::interaction::Account;
use ferry_core::library::{BookmarkId, LibrarySnapshot};
use ferry_core::remote::local::LocalRemote;
use ferry_core::remote::sftp::{SftpRemote, SshConfigSource};
use ferry_core::{Destination, EntryKind, Remote, RemotePath};
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
use secrecy::SecretString;

pub(crate) use self::servers::LibraryAction;
use crate::actions;
use crate::browser::{BrowserEvent, FileBrowser, Navigated, ViewOptions};
use crate::interaction::UiInteraction;
use crate::prompts::{Prompter, SessionId, TaggedPrompt};
use crate::runtime;
use crate::style::{self, Palette as _, TypeScale as _};
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

/// A connection being established: the remote, its start directory and the
/// account it logged in with (SFTP).
type PendingConnection =
    tokio::task::JoinHandle<ferry_core::Result<(Arc<dyn Remote>, RemotePath, Option<Account>)>>;

enum ConnectState {
    Idle,
    Connecting(String),
    Failed(String),
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
    _events: [Subscription; 2],
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
                format!("{folder} — {}", connection.destination.title()).into()
            }
            None => "Servers".into(),
        }
    }

    /// Title for the window while this pane is active.
    pub(crate) fn window_title(&self, cx: &App) -> String {
        match &self.connection {
            Some(connection) => {
                let browser = connection.browser.read(cx);
                format!("{} – {} – Ferry", browser.path(), browser.remote_label())
            }
            None => "Ferry".to_owned(),
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
                    Ok((remote, path, account)) => {
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
                        session.show_browser(&request, remote, path, window, cx);
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
                    let account = sftp.account().clone();
                    let remote: Arc<dyn Remote> = Arc::new(sftp);
                    let start = match start {
                        Some(path) => path,
                        None => remote.home_dir().await?,
                    };
                    Ok((remote, start, Some(account)))
                })
            }
        }
    }

    fn show_browser(
        &mut self,
        request: &ConnectRequest,
        remote: Arc<dyn Remote>,
        path: RemotePath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let options = self.options;
        let address = &request.destination;
        let browser = cx.new(|cx| FileBrowser::new(remote, address, path, options, cx));
        let navigated = cx.subscribe_in(&browser, window, |_, _, _: &Navigated, _, cx| {
            cx.emit(SessionEvent::Changed);
        });
        let requests = cx.subscribe_in(
            &browser,
            window,
            |_, browser, event: &BrowserEvent, _, cx| match event {
                BrowserEvent::Delete(items) => cx.emit(SessionEvent::ConfirmDelete {
                    browser: browser.downgrade(),
                    items: items.clone(),
                }),
                BrowserEvent::Failed(message) => cx.emit(SessionEvent::Failed(message.clone())),
            },
        );
        cx.emit(SessionEvent::Connected {
            destination: request.destination.clone(),
            bookmark: request.bookmark,
        });
        self.connection = Some(Connection {
            destination: request.destination.with_path(None),
            browser,
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

    fn render_banner(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let theme = cx.theme();
        let (icon, text, failed): (IconName, SharedString, bool) = match &self.state {
            ConnectState::Connecting(host) => (
                IconName::LoaderCircle,
                format!("Connecting to {host}…").into(),
                false,
            ),
            ConnectState::Failed(error) => (IconName::TriangleAlert, error.clone().into(), true),
            ConnectState::Idle => return None,
        };
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
                .map(|this| {
                    if failed {
                        this.text_color(theme.danger)
                            .border_color(theme.danger.opacity(0.4))
                            .bg(theme.danger.opacity(0.08))
                    } else {
                        this.border_color(theme.border).bg(theme.group_fill())
                    }
                })
                .child(Icon::new(icon).size_4())
                .child(div().flex_1().min_w_0().child(text))
                .map(|this| {
                    if failed {
                        this.child(
                            Button::new("dismiss-error")
                                .ghost()
                                .xsmall()
                                .icon(IconName::Close)
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.state = ConnectState::Idle;
                                    cx.notify();
                                })),
                        )
                    } else {
                        this.child(
                            Button::new("cancel-connect")
                                .ghost()
                                .xsmall()
                                .label("Cancel")
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.abandon_attempt(cx);
                                    cx.emit(SessionEvent::Changed);
                                    cx.notify();
                                })),
                        )
                    }
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
