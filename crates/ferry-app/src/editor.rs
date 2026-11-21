//! The dialog that creates or edits a favorite.

use std::path::PathBuf;

use crate::style::{self, ControlSize, TypeScale as _};
use ferry_core::library::{BookmarkDraft, BookmarkId, FolderId, LibrarySnapshot};
use ferry_core::{Destination, RemotePath, SftpTarget};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AppContext as _, ClickEvent, Context, Entity, EventEmitter, IntoElement, ParentElement as _,
    PathPromptOptions, Render, SharedString, Styled as _, Subscription, Window, div, px,
};
use gpui_component::button::{Button, ButtonVariants as _, DropdownButton};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::PopupMenuItem;
use gpui_component::{ActiveTheme as _, h_flex, v_flex};

/// What the editor produced.
#[derive(Clone, Debug)]
pub(crate) enum EditorEvent {
    Save {
        id: Option<BookmarkId>,
        draft: BookmarkDraft,
    },
    Cancel,
}

impl EventEmitter<EditorEvent> for BookmarkEditor {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Sftp,
    Local,
}

pub(crate) struct BookmarkEditor {
    id: Option<BookmarkId>,
    kind: Kind,
    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    user: Entity<InputState>,
    path: Entity<InputState>,
    key: Entity<InputState>,
    folder: Option<FolderId>,
    /// Every folder with its full path, for the folder menu.
    folders: Vec<(FolderId, SharedString)>,
    error: Option<SharedString>,
    _submits: Vec<Subscription>,
}

impl BookmarkEditor {
    /// Opens the editor for `draft`; `id` is set when editing an existing favorite.
    pub(crate) fn new(
        id: Option<BookmarkId>,
        draft: &BookmarkDraft,
        library: &LibrarySnapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let field = |placeholder: &'static str,
                     value: String,
                     window: &mut Window,
                     cx: &mut Context<Self>| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(value)
            })
        };
        let (kind, host, port, user, path) = match &draft.destination {
            Destination::Sftp(target) => (
                Kind::Sftp,
                target.host.clone(),
                target.port.map(|port| port.to_string()).unwrap_or_default(),
                target.user.clone().unwrap_or_default(),
                target
                    .path
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default(),
            ),
            Destination::Local { path } => (
                Kind::Local,
                String::new(),
                String::new(),
                String::new(),
                path.as_ref().map(ToString::to_string).unwrap_or_default(),
            ),
        };
        let name = field("Defaults to the host", draft.name.clone(), window, cx);
        let host = field("Host, address or ssh_config alias", host, window, cx);
        let port = field("22", port, window, cx);
        let user = field("From ssh_config", user, window, cx);
        let path = field("Login or home directory", path, window, cx);
        let key = field(
            "From ssh_config and the SSH agent",
            draft
                .identity_file
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            window,
            cx,
        );
        let submits = [&name, &host, &port, &user, &path, &key]
            .into_iter()
            .map(|input| {
                cx.subscribe_in(input, window, |this, _, event: &InputEvent, _, cx| {
                    if let InputEvent::PressEnter { .. } = event {
                        this.save(cx);
                    }
                })
            })
            .collect();
        let folders = library
            .folders
            .iter()
            .map(|folder| (folder.id, library.folder_path(folder.id).into()))
            .collect::<Vec<(FolderId, SharedString)>>();
        let mut folders = folders;
        folders.sort_by_key(|(_, path)| path.to_lowercase());
        let first = if kind == Kind::Sftp && draft.name.is_empty() {
            &host
        } else {
            &name
        };
        first.update(cx, |input, cx| input.focus(window, cx));
        Self {
            id,
            kind,
            name,
            host,
            port,
            user,
            path,
            key,
            folder: draft.folder,
            folders,
            error: None,
            _submits: submits,
        }
    }

    pub(crate) fn cancel(cx: &mut Context<Self>) {
        cx.emit(EditorEvent::Cancel);
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        match self.draft(cx) {
            Ok(draft) => cx.emit(EditorEvent::Save { id: self.id, draft }),
            Err(error) => {
                self.error = Some(error.into());
                cx.notify();
            }
        }
    }

    fn draft(&self, cx: &Context<Self>) -> Result<BookmarkDraft, String> {
        let text = |input: &Entity<InputState>| input.read(cx).value().trim().to_owned();
        let path = text(&self.path);
        let path = if path.is_empty() {
            None
        } else {
            Some(
                RemotePath::parse(&path)
                    .map_err(|_| "The directory must be an absolute path like /srv/www.")?,
            )
        };
        let (destination, identity_file) = match self.kind {
            Kind::Local => (Destination::Local { path }, None),
            Kind::Sftp => {
                let host = text(&self.host);
                if host.is_empty() {
                    return Err("Enter the server's host name or address.".into());
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
                target.path = path;
                let key = text(&self.key);
                let key = (!key.is_empty()).then(|| expand_home(&key));
                if let Some(key) = &key
                    && !key.is_absolute()
                {
                    return Err("The key file must be an absolute path.".into());
                }
                (Destination::Sftp(target), key)
            }
        };
        let mut name = text(&self.name);
        if name.is_empty() {
            name = match &destination {
                Destination::Sftp(target) => target.host.clone(),
                Destination::Local { path } => path
                    .as_ref()
                    .and_then(|path| path.file_name().map(str::to_owned))
                    .unwrap_or_else(|| "Local files".to_owned()),
            };
        }
        Ok(BookmarkDraft {
            folder: self.folder,
            name,
            destination,
            identity_file,
        })
    }

    fn browse_key(window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose a private key".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update_in(cx, |editor, window, cx| {
                editor.key.update(cx, |input, cx| {
                    input.set_value(path.display().to_string(), window, cx);
                });
            });
        })
        .detach();
    }

    fn folder_label(&self) -> SharedString {
        self.folder
            .and_then(|id| self.folders.iter().find(|(folder, _)| *folder == id))
            .map_or_else(|| "No folder".into(), |(_, path)| path.clone())
    }

    fn render_folder_menu(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let this = cx.entity().downgrade();
        let folders = self.folders.clone();
        let current = self.folder;
        DropdownButton::new("folder")
            .button(Button::new("folder-label").label(self.folder_label()))
            .dropdown_menu(move |menu, _, _| {
                let pick = |folder: Option<FolderId>| {
                    let this = this.clone();
                    move |_: &ClickEvent, _: &mut Window, cx: &mut gpui::App| {
                        let _ = this.update(cx, |editor: &mut Self, cx| {
                            editor.folder = folder;
                            cx.notify();
                        });
                    }
                };
                let mut menu = menu.item(
                    PopupMenuItem::new("No folder")
                        .checked(current.is_none())
                        .on_click(pick(None)),
                );
                for (id, path) in &folders {
                    menu = menu.item(
                        PopupMenuItem::new(path.clone())
                            .checked(current == Some(*id))
                            .on_click(pick(Some(*id))),
                    );
                }
                menu
            })
    }
}

impl Render for BookmarkEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let folder_menu = self.render_folder_menu(cx);
        let theme = cx.theme();
        let (muted, danger) = (theme.muted_foreground, theme.danger);
        let label = |text: &'static str| div().text_small().text_color(muted).child(text);
        let field = |text: &'static str, input: &Entity<InputState>| {
            v_flex()
                .flex_1()
                .gap_1()
                .child(label(text))
                .child(Input::new(input))
        };
        let kind = self.kind;
        let sftp_fields =
            v_flex()
                .gap_3()
                .child(
                    h_flex()
                        .gap_3()
                        .child(field("Host", &self.host))
                        .child(div().w(px(88.)).child(field("Port", &self.port))),
                )
                .child(field("User", &self.user))
                .child(
                    v_flex().gap_1().child(label("Private key")).child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().child(Input::new(&self.key)))
                            .child(Button::new("browse-key").label("Browse…").on_click(
                                cx.listener(|_, _: &ClickEvent, window, cx| {
                                    Self::browse_key(window, cx);
                                }),
                            )),
                    ),
                );
        v_flex()
            .gap_3()
            .child(div().text_title().child(if self.id.is_some() {
                "Edit favorite"
            } else {
                "New favorite"
            }))
            .child(style::segmented(
                "kind",
                ControlSize::Regular,
                &[(Kind::Sftp, "SFTP".into()), (Kind::Local, "Local".into())],
                kind,
                {
                    let this = cx.entity().downgrade();
                    move |kind, _, cx| {
                        let _ = this.update(cx, |editor: &mut Self, cx| {
                            editor.kind = kind;
                            cx.notify();
                        });
                    }
                },
                cx,
            ))
            .child(field("Name", &self.name))
            .when(kind == Kind::Sftp, |this| this.child(sftp_fields))
            .child(field("Directory", &self.path))
            .child(
                v_flex()
                    .gap_1()
                    .child(label("Folder"))
                    .child(h_flex().child(folder_menu)),
            )
            .children(
                self.error
                    .clone()
                    .map(|error| div().text_body().text_color(danger).child(error)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("editor-cancel")
                            .label("Cancel")
                            .on_click(cx.listener(|_, _: &ClickEvent, _, cx| Self::cancel(cx))),
                    )
                    .child(
                        Button::new("editor-save")
                            .primary()
                            .label("Save")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.save(cx))),
                    ),
            )
    }
}

/// Expands a leading `~/` to the home directory.
fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(path),
    }
}
