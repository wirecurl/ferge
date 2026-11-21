//! One tab: the address bar and either the start page or a connected file
//! browser.

mod start;

use std::path::PathBuf;
use std::sync::Arc;

use ferry_core::library::{BookmarkId, LibrarySnapshot};
use ferry_core::remote::local::LocalRemote;
use ferry_core::remote::sftp::{SftpRemote, SshConfigSource};
use ferry_core::{Destination, Remote, RemotePath};
use futures::channel::mpsc;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, AppContext as _, ClickEvent, Context, Entity, EventEmitter, Focusable as _, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Subscription, Task, Window, div,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};

use crate::browser::{FileBrowser, Navigated};
use crate::interaction::UiInteraction;
use crate::prompts::{Prompter, SessionId, TaggedPrompt};
use crate::runtime;
use crate::vault::Vault;

/// What to connect to, and where it came from.
#[derive(Clone, Debug)]
pub(crate) struct ConnectRequest {
    pub destination: Destination,
    /// The favorite it was opened from, recorded in the history.
    pub bookmark: Option<BookmarkId>,
    /// A key to try before the configured ones.
    pub identity_file: Option<PathBuf>,
}

impl ConnectRequest {
    pub(crate) fn to(destination: Destination) -> Self {
        Self {
            destination,
            bookmark: None,
            identity_file: None,
        }
    }
}

/// Shared by all tabs of a window.
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
    /// The title or the location changed.
    Changed,
    /// A connection was established.
    Connected {
        destination: Destination,
        bookmark: Option<BookmarkId>,
    },
    /// A connection attempt was abandoned; its open questions are void.
    Abandoned,
    /// The user wants to save `destination` as a favorite.
    SaveAsFavorite(Destination),
}

impl EventEmitter<SessionEvent> for Session {}

/// A connection being established: the remote and the start directory.
type PendingConnection = tokio::task::JoinHandle<ferry_core::Result<(Arc<dyn Remote>, RemotePath)>>;

enum ConnectState {
    Idle,
    Connecting(String),
    Failed(String),
}

struct Connection {
    /// Where the connection went, without the current directory.
    destination: Destination,
    bookmark: Option<BookmarkId>,
    browser: Entity<FileBrowser>,
    _navigated: Subscription,
}

pub(crate) struct Session {
    id: SessionId,
    context: SessionContext,
    address: Entity<InputState>,
    quick: start::QuickConnect,
    state: ConnectState,
    connection: Option<Connection>,
    connect_task: Option<Task<()>>,
    /// Aborts the attempt on the runtime; dropping the UI task alone would
    /// leave it running (and possibly asking questions).
    connect_abort: Option<tokio::task::AbortHandle>,
    library: Arc<LibrarySnapshot>,
    _address_submit: Subscription,
}

impl Session {
    pub(crate) fn new(
        id: SessionId,
        context: SessionContext,
        library: Arc<LibrarySnapshot>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let address = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("sftp://user@host:port/path, an ssh_config alias, or a local path")
        });
        let submit = cx.subscribe_in(
            &address,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.connect_to_address(window, cx);
                }
            },
        );
        Self {
            id,
            context,
            address,
            quick: start::QuickConnect::new(window, cx),
            state: ConnectState::Idle,
            connection: None,
            connect_task: None,
            connect_abort: None,
            library,
            _address_submit: submit,
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

    /// The favorite this tab was opened from.
    pub(crate) fn bookmark(&self) -> Option<BookmarkId> {
        self.connection
            .as_ref()
            .and_then(|connection| connection.bookmark)
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

    /// Short title for the tab.
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
            None => "New tab".into(),
        }
    }

    /// Title for the window while this tab is active.
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

    /// Moves the keyboard focus into this tab.
    pub(crate) fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.connection {
            Some(connection) => window.focus(&connection.browser.focus_handle(cx), cx),
            None => self.quick.focus(window, cx),
        }
    }

    pub(crate) fn focus_address(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.address.update(cx, |input, cx| input.focus(window, cx));
    }

    fn connect_to_address(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.address.read(cx).value().to_string();
        match Destination::parse(&text) {
            Ok(destination) => self.connect(ConnectRequest::to(destination), window, cx),
            Err(error) => {
                self.state = ConnectState::Failed(error.to_string());
                cx.notify();
            }
        }
    }

    /// Connects this tab to `request`, replacing the current connection once
    /// the new one is established.
    pub(crate) fn connect(
        &mut self,
        request: ConnectRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.abandon_attempt(cx);
        let address = request.destination.address();
        self.address
            .update(cx, |input, cx| input.set_value(address, window, cx));
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
                    Ok((remote, path)) => {
                        // Remembered secrets are saved only now that they worked.
                        runtime::spawn(cx, async move { interaction.commit().await });
                        session.show_browser(&request, remote, path, window, cx);
                    }
                    Err(message) => {
                        session.state = ConnectState::Failed(message);
                        if session.connection.is_none() {
                            session.quick.focus(window, cx);
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
                Ok((remote, start))
            }),
            Destination::Sftp(target) => {
                let mut options = target.options();
                if let Some(file) = &self.context.ssh_config {
                    options.ssh_config = SshConfigSource::File(file.clone());
                }
                options.identity_files.extend(request.identity_file.clone());
                let interaction = Arc::clone(interaction);
                runtime::spawn(cx, async move {
                    let sftp = SftpRemote::connect(options, interaction).await?;
                    let remote: Arc<dyn Remote> = Arc::new(sftp);
                    let start = match start {
                        Some(path) => path,
                        None => remote.home_dir().await?,
                    };
                    Ok((remote, start))
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
        let browser = cx.new(|cx| FileBrowser::new(remote, path, window, cx));
        let navigated = cx.subscribe_in(&browser, window, |this, _, _: &Navigated, window, cx| {
            this.sync_address(window, cx);
            cx.emit(SessionEvent::Changed);
        });
        cx.emit(SessionEvent::Connected {
            destination: request.destination.clone(),
            bookmark: request.bookmark,
        });
        self.connection = Some(Connection {
            destination: request.destination.with_path(None),
            bookmark: request.bookmark,
            browser,
            _navigated: navigated,
        });
        self.state = ConnectState::Idle;
        self.sync_address(window, cx);
    }

    /// Shows the current location in the address bar, unless the user is
    /// typing there.
    fn sync_address(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(location) = self.location(cx) else {
            return;
        };
        if self.address.focus_handle(cx).is_focused(window) {
            return;
        }
        self.address.update(cx, |input, cx| {
            input.set_value(location.address(), window, cx);
        });
    }

    fn save_as_favorite(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(location) = self.location(cx) {
            cx.emit(SessionEvent::SaveAsFavorite(location));
        }
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .p_2()
            .gap_2()
            .flex_shrink_0()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(div().flex_1().child(Input::new(&self.address)))
            .when(self.connection.is_some(), |this| {
                this.child(
                    Button::new("save-favorite")
                        .ghost()
                        .icon(Icon::new(gpui_component_assets::IconName::Star))
                        .tooltip("Save as favorite")
                        .on_click(cx.listener(Self::save_as_favorite)),
                )
            })
            .child(
                Button::new("connect")
                    .primary()
                    .label("Connect")
                    .loading(self.is_connecting())
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.connect_to_address(window, cx);
                    })),
            )
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
                .px_3()
                .py_2()
                .gap_2()
                .text_sm()
                .flex_shrink_0()
                .border_b_1()
                .border_color(theme.border)
                .when(failed, |this| this.text_color(theme.danger))
                .child(Icon::new(icon).size_4())
                .child(text)
                .when(failed, |this| {
                    this.child(div().flex_1()).child(
                        Button::new("dismiss-error")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.state = ConnectState::Idle;
                                cx.notify();
                            })),
                    )
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
        let content = match &self.connection {
            Some(connection) => div()
                .flex_1()
                .min_h_0()
                .child(connection.browser.clone())
                .into_any_element(),
            None => self.render_start(window, cx).into_any_element(),
        };
        v_flex()
            .size_full()
            .child(self.render_toolbar(cx))
            .children(self.render_banner(cx))
            .child(content)
    }
}
