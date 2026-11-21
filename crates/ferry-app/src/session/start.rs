//! The start page of a tab: Quick Connect, local files and recent connections.

use std::time::SystemTime;

use ferry_core::{Destination, RemotePath, SftpTarget};
use gpui::{
    AppContext as _, ClickEvent, Context, ElementId, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription,
    Window, div, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{ActiveTheme as _, Icon, IconName, h_flex, v_flex};

use super::{ConnectRequest, Session, SessionEvent};

/// Recent connections listed on the start page.
const RECENT_SHOWN: usize = 6;

/// The Quick Connect form.
pub(super) struct QuickConnect {
    host: Entity<InputState>,
    port: Entity<InputState>,
    user: Entity<InputState>,
    path: Entity<InputState>,
    error: Option<SharedString>,
    _submits: Vec<Subscription>,
}

impl QuickConnect {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Session>) -> Self {
        let field = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Session>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let host = field("Host, address or ssh_config alias", window, cx);
        let port = field("22", window, cx);
        let user = field("From ssh_config, or your login name", window, cx);
        let path = field("Login directory", window, cx);
        let submits = [&host, &port, &user, &path]
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
            path,
            error: None,
            _submits: submits,
        }
    }

    pub(super) fn focus(&self, window: &mut Window, cx: &mut Context<Session>) {
        self.host.update(cx, |input, cx| input.focus(window, cx));
    }

    /// The destination described by the form.
    fn destination(&self, cx: &Context<Session>) -> Result<Destination, String> {
        let text = |input: &Entity<InputState>| input.read(cx).value().trim().to_owned();
        let host = text(&self.host);
        if host.is_empty() {
            return Err("Enter a host to connect to.".into());
        }
        let mut target =
            SftpTarget::new(&host).map_err(|_| format!("“{host}” is not a valid host."))?;
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
                    .map_err(|_| "The directory must be an absolute path like /srv/www.")?,
            );
        }
        Ok(Destination::Sftp(target))
    }
}

impl Session {
    fn quick_connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.quick.destination(cx) {
            Ok(destination) => {
                self.quick.error = None;
                self.connect(ConnectRequest::to(destination), window, cx);
            }
            Err(error) => {
                self.quick.error = Some(error.into());
                cx.notify();
            }
        }
    }

    fn quick_save(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        match self.quick.destination(cx) {
            Ok(destination) => {
                self.quick.error = None;
                cx.emit(SessionEvent::SaveAsFavorite(destination));
            }
            Err(error) => self.quick.error = Some(error.into()),
        }
        cx.notify();
    }

    fn render_quick_connect(&self, cx: &mut Context<Self>) -> gpui::Div {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let label = |text: &'static str| div().text_xs().text_color(muted).child(text);
        v_flex()
            .w_full()
            .p_4()
            .gap_3()
            .rounded_lg()
            .border_1()
            .border_color(theme.border)
            .bg(theme.secondary)
            .child(
                h_flex()
                    .gap_2()
                    .child(Icon::new(IconName::Globe).size_4().text_color(muted))
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("Quick Connect"),
                    )
                    .child(div().text_xs().text_color(muted).child("SFTP")),
            )
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        v_flex()
                            .flex_1()
                            .gap_1()
                            .child(label("Host"))
                            .child(Input::new(&self.quick.host)),
                    )
                    .child(
                        v_flex()
                            .w(px(88.))
                            .gap_1()
                            .child(label("Port"))
                            .child(Input::new(&self.quick.port)),
                    ),
            )
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        v_flex()
                            .flex_1()
                            .gap_1()
                            .child(label("User"))
                            .child(Input::new(&self.quick.user)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .gap_1()
                            .child(label("Directory"))
                            .child(Input::new(&self.quick.path)),
                    ),
            )
            .children(
                self.quick
                    .error
                    .clone()
                    .map(|error| div().text_sm().text_color(theme.danger).child(error)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(muted)
                            .child("Passwords are asked for when needed."),
                    )
                    .child(
                        Button::new("quick-save")
                            .label("Save as favorite…")
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

    pub(super) fn render_start(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let form = self.render_quick_connect(cx);
        let muted = cx.theme().muted_foreground;

        let recent = self.render_recent(cx);
        let shortcuts = if cfg!(target_os = "macos") {
            "⌘T new tab · ⌘\\ split · ⌘L address · ⌘D save favorite · type to filter"
        } else {
            "Ctrl+T new tab · Ctrl+\\ split · Ctrl+L address · Ctrl+D save favorite · type to filter"
        };
        div()
            .id("start-page")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(
                v_flex().w_full().items_center().py_10().px_6().child(
                    v_flex()
                        .w_full()
                        .max_w(px(560.))
                        .gap_5()
                        .child(
                            v_flex()
                                .gap_1()
                                .child(
                                    div()
                                        .text_xl()
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .child("Where to?"),
                                )
                                .child(div().text_sm().text_color(muted).child(
                                    "Connect to a server, pick a favorite in the sidebar, \
                                         or browse this computer.",
                                )),
                        )
                        .child(form)
                        .child(
                            h_flex().child(
                                Button::new("open-local")
                                    .icon(IconName::HardDrive)
                                    .label("Browse local files")
                                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                        this.connect(
                                            ConnectRequest::to(Destination::Local { path: None }),
                                            window,
                                            cx,
                                        );
                                    })),
                            ),
                        )
                        .children(recent)
                        .child(div().text_xs().text_color(muted).child(shortcuts)),
                ),
            )
    }

    fn render_recent(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        if self.library.history.is_empty() {
            return None;
        }
        let theme = cx.theme();
        let (muted, hover, border) = (theme.muted_foreground, theme.list_hover, theme.border);
        let rows = self
            .library
            .history
            .iter()
            .take(RECENT_SHOWN)
            .enumerate()
            .map(|(index, recent)| {
                let bookmark = recent.bookmark.and_then(|id| self.library.bookmark(id));
                let title: SharedString = bookmark
                    .map_or_else(
                        || recent.destination.title(),
                        |bookmark| bookmark.name.clone(),
                    )
                    .into();
                let request = ConnectRequest {
                    destination: recent.destination.clone(),
                    bookmark: bookmark.map(|bookmark| bookmark.id),
                    identity_file: bookmark.and_then(|bookmark| bookmark.identity_file.clone()),
                };
                let icon = match recent.destination {
                    Destination::Local { .. } => IconName::HardDrive,
                    Destination::Sftp(_) => IconName::Globe,
                };
                h_flex()
                    .id(ElementId::NamedInteger("recent".into(), index as u64))
                    .px_3()
                    .py_2()
                    .gap_3()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|style| style.bg(hover))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.connect(request.clone(), window, cx);
                    }))
                    .child(Icon::new(icon).size_4().text_color(muted))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_sm().truncate().child(title))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .truncate()
                                    .child(recent.destination.address()),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(ago(recent.connected_at)),
                    )
            });
        Some(
            v_flex()
                .gap_1()
                .child(
                    div()
                        .pb_1()
                        .text_xs()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(muted)
                        .border_b_1()
                        .border_color(border)
                        .child("RECENT"),
                )
                .children(rows),
        )
    }
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
