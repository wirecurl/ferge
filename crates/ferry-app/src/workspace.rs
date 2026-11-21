//! The main window: address bar, file browser and connection dialogs.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

use ferry_core::interaction::HostKeyDecision;
use ferry_core::remote::local::LocalRemote;
use ferry_core::remote::sftp::{SftpRemote, SshConfigSource, configured_hosts};
use ferry_core::{Destination, Remote, RemotePath};
use futures::StreamExt as _;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AppContext as _, ClickEvent, Context, ElementId, Entity, FocusHandle, Focusable as _,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString, Styled as _,
    Subscription, Task, Window, div, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, Theme, h_flex, v_flex};
use secrecy::SecretString;

use crate::browser::{FileBrowser, Navigated};
use crate::interaction::{self, PromptRequest, SecretRequest, UiInteraction};
use crate::runtime;

/// A connection being established, resolving to the remote and its start directory.
type PendingConnection = tokio::task::JoinHandle<ferry_core::Result<(Arc<dyn Remote>, RemotePath)>>;

/// Key context of the dialog overlay.
const PROMPT_CONTEXT: &str = "FerryPrompt";

/// Key context of the whole window.
const WORKSPACE_CONTEXT: &str = "Workspace";

gpui::actions!(ferry_prompt, [CancelPrompt]);
gpui::actions!(workspace, [FocusAddress]);

/// Registers the key bindings of this module.
pub(crate) fn bind_keys(cx: &mut gpui::App) {
    let focus_address = if cfg!(target_os = "macos") {
        "cmd-l"
    } else {
        "ctrl-l"
    };
    cx.bind_keys([
        gpui::KeyBinding::new("escape", CancelPrompt, Some(PROMPT_CONTEXT)),
        gpui::KeyBinding::new(focus_address, FocusAddress, Some(WORKSPACE_CONTEXT)),
    ]);
}

enum ConnectState {
    Idle,
    Connecting(String),
    Failed(String),
}

/// A dialog that is currently shown.
enum ActivePrompt {
    HostKey {
        prompt: ferry_core::interaction::HostKeyPrompt,
        reply: futures::channel::oneshot::Sender<HostKeyDecision>,
    },
    Secret {
        request: SecretRequest,
        input: Entity<InputState>,
        _submit: Subscription,
    },
}

pub(crate) struct Workspace {
    address: Entity<InputState>,
    browser: Option<Entity<FileBrowser>>,
    connect: ConnectState,
    connect_task: Option<Task<()>>,
    /// Aborts the connection attempt on the runtime; dropping the UI task alone
    /// would leave it running (and possibly prompting).
    connect_abort: Option<tokio::task::AbortHandle>,
    interaction: UiInteraction,
    /// `-F` from the command line; `None` means `~/.ssh/config`.
    ssh_config: Option<PathBuf>,
    queued_prompts: VecDeque<PromptRequest>,
    prompt: Option<ActivePrompt>,
    prompt_focus: FocusHandle,
    _prompt_listener: Task<()>,
    _address_submit: Subscription,
    /// Host aliases from the SSH configuration, shown on the start screen.
    hosts: Vec<SharedString>,
    _hosts_lookup: Task<()>,
    /// Follows the system's light/dark preference.
    _appearance: Subscription,
    /// Keeps the window title in sync with the browser.
    browser_title: Option<Subscription>,
}

impl Workspace {
    pub(crate) fn new(
        initial: Option<String>,
        ssh_config: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let autoconnect = initial.is_some();
        let address = cx.new(|cx| {
            let state = InputState::new(window, cx)
                .placeholder("sftp://user@host:port/path, an ssh_config alias, or a local path");
            match initial {
                Some(text) => state.default_value(text),
                None => state,
            }
        });
        let submit = cx.subscribe_in(
            &address,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.connect(window, cx);
                }
            },
        );

        Theme::sync_system_appearance(Some(window), cx);
        let appearance = cx.observe_window_appearance(window, |_, window, cx| {
            Theme::sync_system_appearance(Some(window), cx);
        });

        let source = ssh_config
            .clone()
            .map_or(SshConfigSource::User, SshConfigSource::File);
        let lookup = runtime::spawn(cx, async move {
            tokio::task::spawn_blocking(move || configured_hosts(&source)).await
        });
        let hosts_lookup = cx.spawn(async move |this, cx| {
            let hosts = match lookup.await {
                Ok(Ok(Ok(hosts))) => hosts,
                Ok(Ok(Err(error))) => {
                    tracing::warn!(%error, "could not read the SSH configuration");
                    Vec::new()
                }
                _ => Vec::new(),
            };
            let _ = this.update(cx, |workspace, cx| {
                workspace.hosts = hosts.into_iter().map(SharedString::from).collect();
                cx.notify();
            });
        });

        let (interaction, mut requests) = interaction::channel();
        let prompt_listener = cx.spawn_in(window, async move |this, cx| {
            while let Some(request) = requests.next().await {
                let delivered = this.update_in(cx, |workspace, window, cx| {
                    workspace.queued_prompts.push_back(request);
                    workspace.show_next_prompt(window, cx);
                });
                if delivered.is_err() {
                    break;
                }
            }
        });

        let mut workspace = Self {
            address,
            browser: None,
            connect: ConnectState::Idle,
            connect_task: None,
            connect_abort: None,
            interaction,
            ssh_config,
            queued_prompts: VecDeque::new(),
            prompt: None,
            prompt_focus: cx.focus_handle(),
            _prompt_listener: prompt_listener,
            _address_submit: submit,
            hosts: Vec::new(),
            _hosts_lookup: hosts_lookup,
            _appearance: appearance,
            browser_title: None,
        };
        if autoconnect {
            workspace.connect(window, cx);
        } else {
            workspace
                .address
                .update(cx, |input, cx| input.focus(window, cx));
        }
        workspace
    }

    /// The browser, if connected. Used by the benchmark driver.
    pub(crate) fn browser(&self) -> Option<&Entity<FileBrowser>> {
        self.browser.as_ref()
    }

    fn connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.address.read(cx).value().to_string();
        let destination = match Destination::parse(&text) {
            Ok(destination) => destination,
            Err(error) => {
                self.connect = ConnectState::Failed(error.to_string());
                cx.notify();
                return;
            }
        };
        if let Some(previous) = self.connect_abort.take() {
            previous.abort();
            // Its questions can no longer be answered; dropping them rejects.
            self.prompt = None;
            self.queued_prompts.clear();
        }
        let (label, connecting) = self.start_connection(destination, cx);
        self.connect_abort = Some(connecting.abort_handle());
        self.connect = ConnectState::Connecting(label);
        self.connect_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = match connecting.await {
                Ok(result) => result.map_err(|error| error.to_string()),
                Err(error) => Err(format!("connection task failed: {error}")),
            };
            let _ = this.update_in(cx, |workspace, window, cx| {
                workspace.connect_task = None;
                workspace.connect_abort = None;
                match result {
                    Ok((remote, path)) => workspace.show_browser(remote, path, window, cx),
                    Err(message) => {
                        workspace.connect = ConnectState::Failed(message);
                        workspace
                            .address
                            .update(cx, |input, cx| input.focus(window, cx));
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Starts connecting on the runtime and returns a label for the UI plus the
    /// pending connection with its start directory.
    fn start_connection(
        &self,
        destination: Destination,
        cx: &mut Context<Self>,
    ) -> (String, PendingConnection) {
        match destination {
            Destination::Local { path } => (
                "local files".into(),
                runtime::spawn(cx, async move {
                    let remote: Arc<dyn Remote> = Arc::new(LocalRemote::new());
                    let start = match path {
                        Some(path) => path,
                        None => remote.home_dir().await?,
                    };
                    Ok((remote, start))
                }),
            ),
            Destination::Sftp(target) => {
                let mut options = target.options();
                let path = target.path;
                if let Some(file) = &self.ssh_config {
                    options.ssh_config = SshConfigSource::File(file.clone());
                }
                let label = options.host.clone();
                let interaction = Arc::new(self.interaction.clone());
                (
                    label,
                    runtime::spawn(cx, async move {
                        let sftp = SftpRemote::connect(options, interaction).await?;
                        let remote: Arc<dyn Remote> = Arc::new(sftp);
                        let start = match path {
                            Some(path) => path,
                            None => remote.home_dir().await?,
                        };
                        Ok((remote, start))
                    }),
                )
            }
        }
    }

    fn show_browser(
        &mut self,
        remote: Arc<dyn Remote>,
        path: RemotePath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let browser = cx.new(|cx| FileBrowser::new(remote, path, window, cx));
        let titles = cx.subscribe_in(&browser, window, |_, browser, _: &Navigated, window, cx| {
            let browser = browser.read(cx);
            window.set_window_title(&format!(
                "{} – {} – Ferry",
                browser.path(),
                browser.remote_label()
            ));
        });
        {
            let browser = browser.read(cx);
            window.set_window_title(&format!(
                "{} – {} – Ferry",
                browser.path(),
                browser.remote_label()
            ));
        }
        self.browser_title = Some(titles);
        self.browser = Some(browser);
        self.connect = ConnectState::Idle;
    }

    // ---------------------------------------------------------------------
    // Prompts

    fn show_next_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.prompt.is_some() {
            return;
        }
        let Some(request) = self.queued_prompts.pop_front() else {
            self.restore_focus(window, cx);
            return;
        };
        self.prompt = Some(match request {
            PromptRequest::HostKey { prompt, reply } => {
                window.focus(&self.prompt_focus, cx);
                ActivePrompt::HostKey { prompt, reply }
            }
            PromptRequest::Secret(request) => {
                let echo = request.echo;
                let input = cx.new(|cx| InputState::new(window, cx).masked(!echo));
                input.update(cx, |input, cx| input.focus(window, cx));
                let submit =
                    cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                        if let InputEvent::PressEnter { .. } = event {
                            this.answer_secret(true, window, cx);
                        }
                    });
                ActivePrompt::Secret {
                    request,
                    input,
                    _submit: submit,
                }
            }
        });
        cx.notify();
    }

    fn answer_host_key(
        &mut self,
        decision: HostKeyDecision,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(ActivePrompt::HostKey { reply, .. }) = self.prompt.take() {
            let _ = reply.send(decision);
        }
        self.show_next_prompt(window, cx);
    }

    fn answer_secret(&mut self, submit: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ActivePrompt::Secret { request, input, .. }) = self.prompt.take() {
            let answer = submit.then(|| SecretString::from(input.read(cx).value().to_string()));
            // Do not keep the secret around in the input's buffer.
            input.update(cx, |input, cx| input.set_value("", window, cx));
            let _ = request.reply.send(answer);
        }
        self.show_next_prompt(window, cx);
    }

    fn cancel_prompt(&mut self, _: &CancelPrompt, window: &mut Window, cx: &mut Context<Self>) {
        match self.prompt {
            Some(ActivePrompt::HostKey { .. }) => {
                self.answer_host_key(HostKeyDecision::Reject, window, cx);
            }
            Some(ActivePrompt::Secret { .. }) => self.answer_secret(false, window, cx),
            None => {}
        }
    }

    fn restore_focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.browser {
            Some(browser) => window.focus(&browser.focus_handle(cx), cx),
            None => self.address.update(cx, |input, cx| input.focus(window, cx)),
        }
    }

    // ---------------------------------------------------------------------
    // Rendering

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let connecting = matches!(self.connect, ConnectState::Connecting(_));
        h_flex()
            .p_2()
            .gap_2()
            .flex_shrink_0()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(div().flex_1().child(Input::new(&self.address)))
            .child(
                Button::new("connect")
                    .primary()
                    .label("Connect")
                    .loading(connecting)
                    .on_click(
                        cx.listener(|this, _: &ClickEvent, window, cx| this.connect(window, cx)),
                    ),
            )
    }

    fn render_body(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let message: Option<(IconName, SharedString, bool)> = match &self.connect {
            ConnectState::Connecting(host) => Some((
                IconName::LoaderCircle,
                format!("Connecting to {host}…").into(),
                false,
            )),
            ConnectState::Failed(error) => {
                Some((IconName::TriangleAlert, error.clone().into(), true))
            }
            ConnectState::Idle => None,
        };
        let banner = message.map(|(icon, text, failed)| {
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
        });
        let content = match &self.browser {
            Some(browser) => div().flex_1().min_h_0().child(browser.clone()),
            None => self.render_start(cx),
        };
        v_flex().flex_1().min_h_0().children(banner).child(content)
    }

    /// Fills the address field with `destination` and connects.
    fn connect_to(
        &mut self,
        destination: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.address
            .update(cx, |input, cx| input.set_value(destination, window, cx));
        self.connect(window, cx);
    }

    fn render_start(&self, cx: &mut Context<Self>) -> gpui::Div {
        /// Quick-connect buttons shown at most; long configs stay usable.
        const MAX_HOSTS: usize = 24;
        let theme = cx.theme();
        let hosts = self
            .hosts
            .iter()
            .take(MAX_HOSTS)
            .enumerate()
            .map(|(index, host)| {
                let destination = host.clone();
                Button::new(ElementId::NamedInteger("ssh-host".into(), index as u64))
                    .outline()
                    .small()
                    .icon(IconName::Globe)
                    .label(host.clone())
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.connect_to(destination.clone(), window, cx);
                    }))
            });
        let shortcuts = if cfg!(target_os = "macos") {
            "⌘L address · ↩ open · ⌘↑ parent · ⌘[ back · type to filter · ⌘1–3 sort"
        } else {
            "Ctrl+L address · Enter open · Alt+↑ parent · Alt+← back · type to filter · Ctrl+1–3 sort"
        };
        div()
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .p_6()
            .child(
                v_flex()
                    .max_w(px(640.))
                    .items_center()
                    .gap_4()
                    .child(
                        Icon::new(IconName::HardDrive)
                            .size_10()
                            .text_color(theme.muted_foreground),
                    )
                    .child(
                        v_flex()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .text_xl()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child("Where to?"),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child("Type an address above, or pick a destination."),
                            ),
                    )
                    .child(
                        Button::new("open-local")
                            .icon(IconName::Folder)
                            .label("Local files")
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.connect_to("local".into(), window, cx);
                            })),
                    )
                    .when(!self.hosts.is_empty(), |this| {
                        this.child(
                            v_flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child("SSH hosts"),
                                )
                                .child(
                                    h_flex()
                                        .flex_wrap()
                                        .justify_center()
                                        .gap_2()
                                        .children(hosts),
                                ),
                        )
                    })
                    .child(
                        div()
                            .pt_4()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(shortcuts),
                    ),
            )
    }

    fn render_prompt(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let theme = cx.theme();
        let card = v_flex()
            .w(px(460.))
            .p_4()
            .gap_3()
            .rounded_lg()
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .text_color(theme.popover_foreground)
            .shadow_lg();
        let card = match self.prompt.as_ref()? {
            ActivePrompt::HostKey { prompt, .. } => self.render_host_key_prompt(card, prompt, cx),
            ActivePrompt::Secret { request, input, .. } => {
                Self::render_secret_prompt(card, request, input, cx)
            }
        };
        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::black().opacity(0.35))
                .key_context(PROMPT_CONTEXT)
                .on_action(cx.listener(Self::cancel_prompt))
                .child(card),
        )
    }

    fn render_host_key_prompt(
        &self,
        card: gpui::Div,
        prompt: &ferry_core::interaction::HostKeyPrompt,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let theme = cx.theme();
        let decide = |decision: HostKeyDecision| {
            cx.listener(move |this: &mut Self, _: &ClickEvent, window, cx| {
                this.answer_host_key(decision, window, cx);
            })
        };
        card.track_focus(&self.prompt_focus)
            .child(
                div()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child("Unknown host key"),
            )
            .child(div().text_sm().child(format!(
                "The authenticity of {} cannot be established. Verify the fingerprint with the \
                 server's administrator before you trust it.",
                prompt.host
            )))
            .child(
                v_flex()
                    .p_2()
                    .rounded_md()
                    .bg(theme.muted)
                    .text_xs()
                    .font_family("monospace")
                    .child(prompt.algorithm.clone())
                    .child(prompt.fingerprint.clone()),
            )
            .when(prompt.other_types_known, |this| {
                this.child(div().text_sm().text_color(theme.danger).child(
                    "Keys of other types are already known for this host. The server may have \
                     been reconfigured, or someone may be intercepting the connection.",
                ))
            })
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("host-key-reject")
                            .label("Cancel")
                            .on_click(decide(HostKeyDecision::Reject)),
                    )
                    .child(
                        Button::new("host-key-once")
                            .label("Connect once")
                            .on_click(decide(HostKeyDecision::AcceptOnce)),
                    )
                    .child(
                        Button::new("host-key-trust")
                            .primary()
                            .label("Trust and connect")
                            .on_click(decide(HostKeyDecision::AcceptAndRemember)),
                    ),
            )
    }

    fn render_secret_prompt(
        card: gpui::Div,
        request: &SecretRequest,
        input: &Entity<InputState>,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let answer = |submit: bool| {
            cx.listener(move |this: &mut Self, _: &ClickEvent, window, cx| {
                this.answer_secret(submit, window, cx);
            })
        };
        card.child(
            div()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(request.title.clone()),
        )
        .child(div().text_sm().child(request.message.clone()))
        .child(Input::new(input))
        .child(
            h_flex()
                .gap_2()
                .justify_end()
                .child(
                    Button::new("secret-cancel")
                        .label("Cancel")
                        .on_click(answer(false)),
                )
                .child(
                    Button::new("secret-ok")
                        .primary()
                        .label("OK")
                        .on_click(answer(true)),
                ),
        )
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .relative()
            .size_full()
            .key_context(WORKSPACE_CONTEXT)
            .on_action(cx.listener(|this, _: &FocusAddress, window, cx| {
                this.address.update(cx, |input, cx| input.focus(window, cx));
            }))
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(self.render_toolbar(cx))
            .child(self.render_body(cx))
            .children(self.render_prompt(cx))
    }
}
