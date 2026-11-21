//! Modal dialogs: questions from connections (host keys, passwords, the
//! master password) and the window's own dialogs (favorite editor, folder
//! names, confirmations).
//!
//! Questions from connections take precedence and are shown on top; they
//! belong to a tab and disappear with it.

use ferry_core::interaction::{HostKeyDecision, HostKeyPrompt};
use ferry_core::library::{BookmarkDraft, BookmarkId, FolderId};
use ferry_core::transfer::{Conflict, ConflictAction, ConflictDecision, TransferId};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AppContext as _, ClickEvent, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, Styled as _, Subscription, Window, div, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::notification::Notification;
use gpui_component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};
use secrecy::{ExposeSecret as _, SecretString};

use super::Workspace;
use crate::editor::{BookmarkEditor, EditorEvent};
use crate::prompts::{
    MasterPasswordRequest, PromptRequest, SecretRequest, SessionId, TaggedPrompt,
};

/// Key context of the question overlay.
const PROMPT_CONTEXT: &str = "FerryPrompt";
/// Key context of the dialog overlay.
const DIALOG_CONTEXT: &str = "FerryDialog";

/// Shortest master password accepted for a new password file.
const MIN_MASTER_PASSWORD_LEN: usize = 8;

gpui::actions!(ferry_dialog, [CancelPrompt, CancelDialog]);

pub(super) fn bind_keys(cx: &mut gpui::App) {
    cx.bind_keys([
        gpui::KeyBinding::new("escape", CancelPrompt, Some(PROMPT_CONTEXT)),
        gpui::KeyBinding::new("escape", CancelDialog, Some(DIALOG_CONTEXT)),
    ]);
}

/// A question from a connection that is currently shown.
pub(super) enum ActivePrompt {
    HostKey {
        session: Option<SessionId>,
        prompt: HostKeyPrompt,
        reply: futures::channel::oneshot::Sender<HostKeyDecision>,
    },
    Secret {
        session: Option<SessionId>,
        request: SecretRequest,
        input: Entity<InputState>,
        remember: bool,
        _submit: Subscription,
    },
    Master {
        session: Option<SessionId>,
        request: MasterPasswordRequest,
        input: Entity<InputState>,
        /// Second field when a new password is chosen.
        confirm: Option<Entity<InputState>>,
        error: Option<SharedString>,
        _submits: Vec<Subscription>,
    },
    Conflict {
        conflict: Conflict,
        reply: futures::channel::oneshot::Sender<ConflictDecision>,
        apply_to_all: bool,
    },
}

impl ActivePrompt {
    fn session(&self) -> Option<SessionId> {
        match self {
            Self::HostKey { session, .. }
            | Self::Secret { session, .. }
            | Self::Master { session, .. } => *session,
            Self::Conflict { .. } => None,
        }
    }
}

/// A dialog of the window itself.
pub(super) enum Dialog {
    Editor {
        editor: Entity<BookmarkEditor>,
        _events: Subscription,
    },
    FolderName {
        input: Entity<InputState>,
        purpose: FolderPurpose,
        error: Option<SharedString>,
        _submit: Subscription,
    },
    Confirm {
        title: SharedString,
        message: SharedString,
        confirm: SharedString,
        action: ConfirmAction,
    },
}

#[derive(Clone, Copy, Debug)]
pub(super) enum FolderPurpose {
    Create { parent: Option<FolderId> },
    Rename(FolderId),
}

#[derive(Clone, Copy, Debug)]
pub(super) enum ConfirmAction {
    DeleteBookmark(BookmarkId),
    DeleteFolder(FolderId),
    ClearHistory,
}

impl Workspace {
    // ---------------------------------------------------------------------
    // Questions from connections

    pub(super) fn receive_prompt(
        &mut self,
        prompt: TaggedPrompt,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let PromptRequest::Notice(message) = prompt.request {
            window.push_notification(Notification::error(message), cx);
            return;
        }
        self.queued_prompts.push_back(prompt);
        self.show_next_prompt(window, cx);
    }

    /// Withdraws the questions of a tab that was closed or gave up.
    pub(super) fn drop_prompts_of(
        &mut self,
        session: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.queued_prompts
            .retain(|prompt| prompt.session != Some(session));
        if self
            .prompt
            .as_ref()
            .is_some_and(|prompt| prompt.session() == Some(session))
        {
            // Dropping the reply sender answers "declined".
            self.prompt = None;
            self.show_next_prompt(window, cx);
        }
        cx.notify();
    }

    /// Withdraws the conflict questions of a transfer that settled.
    pub(super) fn drop_conflicts_of(
        &mut self,
        transfer: TransferId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let of_transfer = |request: &PromptRequest| matches!(request, PromptRequest::Conflict { conflict, .. } if conflict.transfer == transfer);
        self.queued_prompts
            .retain(|prompt| !of_transfer(&prompt.request));
        if matches!(
            &self.prompt,
            Some(ActivePrompt::Conflict { conflict, .. }) if conflict.transfer == transfer
        ) {
            self.prompt = None;
            self.show_next_prompt(window, cx);
        }
        cx.notify();
    }

    fn show_next_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.prompt.is_some() {
            return;
        }
        let Some(TaggedPrompt { session, request }) = self.queued_prompts.pop_front() else {
            self.restore_focus(window, cx);
            return;
        };
        // Show the tab and pane that is asking.
        if let Some((tab, pane)) = session.and_then(|id| self.locate(id, cx)) {
            self.active = tab;
            self.tabs[tab].focused = pane;
            self.update_title(window, cx);
        }
        self.prompt = match request {
            PromptRequest::HostKey { prompt, reply } => {
                window.focus(&self.prompt_focus, cx);
                Some(ActivePrompt::HostKey {
                    session,
                    prompt,
                    reply,
                })
            }
            PromptRequest::Secret(request) => {
                let input = cx.new(|cx| InputState::new(window, cx).masked(!request.echo));
                input.update(cx, |input, cx| input.focus(window, cx));
                let submit =
                    cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                        if let InputEvent::PressEnter { .. } = event {
                            this.answer_secret(true, window, cx);
                        }
                    });
                Some(ActivePrompt::Secret {
                    session,
                    remember: request.remember.unwrap_or(false),
                    request,
                    input,
                    _submit: submit,
                })
            }
            PromptRequest::MasterPassword(request) => {
                let input = cx.new(|cx| InputState::new(window, cx).masked(true));
                let confirm = request
                    .create
                    .then(|| cx.new(|cx| InputState::new(window, cx).masked(true)));
                input.update(cx, |input, cx| input.focus(window, cx));
                let submits = std::iter::once(&input)
                    .chain(confirm.as_ref())
                    .map(|field| {
                        cx.subscribe_in(field, window, |this, _, event: &InputEvent, window, cx| {
                            if let InputEvent::PressEnter { .. } = event {
                                this.answer_master(true, window, cx);
                            }
                        })
                    })
                    .collect();
                Some(ActivePrompt::Master {
                    session,
                    request,
                    input,
                    confirm,
                    error: None,
                    _submits: submits,
                })
            }
            PromptRequest::Conflict { conflict, reply } => {
                window.focus(&self.prompt_focus, cx);
                Some(ActivePrompt::Conflict {
                    conflict,
                    reply,
                    apply_to_all: false,
                })
            }
            PromptRequest::Notice(message) => {
                window.push_notification(Notification::error(message), cx);
                None
            }
        };
        if self.prompt.is_none() {
            self.show_next_prompt(window, cx);
        }
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

    /// `None` dismisses the question, which cancels the transfer.
    fn answer_conflict(
        &mut self,
        action: Option<ConflictAction>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(ActivePrompt::Conflict {
            reply,
            apply_to_all,
            ..
        }) = self.prompt.take()
            && let Some(action) = action
        {
            let _ = reply.send(ConflictDecision {
                action,
                apply_to_all,
            });
        }
        self.show_next_prompt(window, cx);
    }

    fn answer_secret(&mut self, submit: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ActivePrompt::Secret {
            request,
            input,
            remember,
            ..
        }) = self.prompt.take()
        {
            let answer = submit.then(|| crate::prompts::SecretAnswer {
                secret: SecretString::from(input.read(cx).value().to_string()),
                remember,
            });
            // Do not keep the secret around in the input's buffer.
            input.update(cx, |input, cx| input.set_value("", window, cx));
            let _ = request.reply.send(answer);
        }
        self.show_next_prompt(window, cx);
    }

    fn answer_master(&mut self, submit: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ActivePrompt::Master {
            input,
            confirm,
            error,
            ..
        }) = self.prompt.as_mut()
        else {
            return;
        };
        if submit {
            let password = input.read(cx).value().to_string();
            if let Some(confirm) = confirm {
                let problem = if password.chars().count() < MIN_MASTER_PASSWORD_LEN {
                    Some(format!(
                        "Use at least {MIN_MASTER_PASSWORD_LEN} characters."
                    ))
                } else if confirm.read(cx).value().as_ref() != password {
                    Some("The passwords do not match.".to_owned())
                } else {
                    None
                };
                if let Some(problem) = problem {
                    *error = Some(problem.into());
                    cx.notify();
                    return;
                }
            }
        }
        let Some(ActivePrompt::Master {
            request,
            input,
            confirm,
            ..
        }) = self.prompt.take()
        else {
            return;
        };
        let answer = submit.then(|| SecretString::from(input.read(cx).value().to_string()));
        for field in std::iter::once(&input).chain(confirm.as_ref()) {
            field.update(cx, |field, cx| field.set_value("", window, cx));
        }
        let _ = request
            .reply
            .send(answer.filter(|secret| !secret.expose_secret().is_empty()));
        self.show_next_prompt(window, cx);
    }

    fn cancel_prompt(&mut self, _: &CancelPrompt, window: &mut Window, cx: &mut Context<Self>) {
        match self.prompt {
            Some(ActivePrompt::HostKey { .. }) => {
                self.answer_host_key(HostKeyDecision::Reject, window, cx);
            }
            Some(ActivePrompt::Secret { .. }) => self.answer_secret(false, window, cx),
            Some(ActivePrompt::Master { .. }) => self.answer_master(false, window, cx),
            Some(ActivePrompt::Conflict { .. }) => self.answer_conflict(None, window, cx),
            None => {}
        }
    }

    fn restore_focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dialog.is_some() {
            return;
        }
        if let Some(session) = self.active_session() {
            session.update(cx, |session, cx| session.focus(window, cx));
        }
    }

    // ---------------------------------------------------------------------
    // Window dialogs

    pub(super) fn open_editor(
        &mut self,
        id: Option<BookmarkId>,
        draft: &BookmarkDraft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = std::sync::Arc::clone(&self.snapshot);
        let editor = cx.new(|cx| BookmarkEditor::new(id, draft, &snapshot, window, cx));
        let events = cx.subscribe_in(
            &editor,
            window,
            |this, _, event: &EditorEvent, window, cx| {
                if let EditorEvent::Save { id, draft } = event {
                    let (id, draft) = (*id, draft.clone());
                    this.change_library(
                        move |library| match id {
                            Some(id) => library.update_bookmark(id, &draft),
                            None => library.create_bookmark(&draft).map(|_| ()),
                        },
                        cx,
                    );
                }
                this.close_dialog(window, cx);
            },
        );
        self.dialog = Some(Dialog::Editor {
            editor,
            _events: events,
        });
        cx.notify();
    }

    pub(super) fn ask_folder_name(
        &mut self,
        purpose: FolderPurpose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = match purpose {
            FolderPurpose::Rename(id) => self
                .snapshot
                .folder(id)
                .map(|folder| folder.name.clone())
                .unwrap_or_default(),
            FolderPurpose::Create { .. } => String::new(),
        };
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Folder name")
                .default_value(current)
        });
        input.update(cx, |input, cx| input.focus(window, cx));
        let submit = cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.submit_folder_name(window, cx);
            }
        });
        self.dialog = Some(Dialog::FolderName {
            input,
            purpose,
            error: None,
            _submit: submit,
        });
        cx.notify();
    }

    fn submit_folder_name(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Dialog::FolderName {
            input,
            purpose,
            error,
            ..
        }) = &mut self.dialog
        else {
            return;
        };
        let name = input.read(cx).value().trim().to_owned();
        if name.is_empty() {
            *error = Some("Enter a name.".into());
            cx.notify();
            return;
        }
        let purpose = *purpose;
        self.change_library(
            move |library| match purpose {
                FolderPurpose::Create { parent } => {
                    library.create_folder(parent, &name).map(|_| ())
                }
                FolderPurpose::Rename(id) => library.rename_folder(id, &name),
            },
            cx,
        );
        self.close_dialog(window, cx);
    }

    pub(super) fn confirm(
        &mut self,
        action: ConfirmAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (title, message, confirm): (SharedString, SharedString, SharedString) = match action {
            ConfirmAction::DeleteBookmark(id) => {
                let name = self
                    .snapshot
                    .bookmark(id)
                    .map(|bookmark| bookmark.name.clone())
                    .unwrap_or_default();
                (
                    "Delete favorite?".into(),
                    format!("“{name}” will be removed from your favorites.").into(),
                    "Delete".into(),
                )
            }
            ConfirmAction::DeleteFolder(id) => {
                let name = self
                    .snapshot
                    .folder(id)
                    .map(|folder| folder.name.clone())
                    .unwrap_or_default();
                let folders = self.snapshot.folder_with_descendants(id);
                let favorites = self
                    .snapshot
                    .bookmarks
                    .iter()
                    .filter(|bookmark| bookmark.folder.is_some_and(|f| folders.contains(&f)))
                    .count();
                let message = match favorites {
                    0 => format!("The folder “{name}” will be removed."),
                    1 => format!("The folder “{name}” and the favorite in it will be removed."),
                    n => {
                        format!("The folder “{name}” and the {n} favorites in it will be removed.")
                    }
                };
                ("Delete folder?".into(), message.into(), "Delete".into())
            }
            ConfirmAction::ClearHistory => (
                "Clear recent connections?".into(),
                "The list of recent connections will be emptied. Favorites are kept.".into(),
                "Clear".into(),
            ),
        };
        self.dialog = Some(Dialog::Confirm {
            title,
            message,
            confirm,
            action,
        });
        window.focus(&self.prompt_focus, cx);
        cx.notify();
    }

    fn run_confirmed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Dialog::Confirm { action, .. }) = &self.dialog else {
            return;
        };
        let action = *action;
        self.change_library(
            move |library| match action {
                ConfirmAction::DeleteBookmark(id) => library.delete_bookmark(id),
                ConfirmAction::DeleteFolder(id) => library.delete_folder(id),
                ConfirmAction::ClearHistory => library.clear_history(),
            },
            cx,
        );
        self.close_dialog(window, cx);
    }

    fn close_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dialog = None;
        if self.prompt.is_none() {
            self.restore_focus(window, cx);
        }
        cx.notify();
    }

    fn cancel_dialog(&mut self, _: &CancelDialog, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Dialog::Editor { editor, .. }) = &self.dialog {
            editor.update(cx, |_, cx| BookmarkEditor::cancel(cx));
        } else {
            self.close_dialog(window, cx);
        }
    }

    // ---------------------------------------------------------------------
    // Rendering

    fn card(cx: &Context<Self>) -> gpui::Div {
        let theme = cx.theme();
        v_flex()
            .w(px(460.))
            .max_w_full()
            .p_5()
            .gap_3()
            .rounded_lg()
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .text_color(theme.popover_foreground)
            .shadow_lg()
    }

    fn overlay(context: &'static str) -> gpui::Div {
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .p_4()
            .bg(gpui::black().opacity(0.5))
            .key_context(context)
            // Clicks on the backdrop must not reach the window below.
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
    }

    pub(super) fn render_dialog(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let danger = theme.danger;
        let card =
            match self.dialog.as_ref()? {
                Dialog::Editor { editor, .. } => Self::card(cx).w(px(520.)).child(editor.clone()),
                Dialog::FolderName {
                    input,
                    purpose,
                    error,
                    ..
                } => Self::card(cx)
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(match purpose {
                                FolderPurpose::Create { .. } => "New folder",
                                FolderPurpose::Rename(_) => "Rename folder",
                            }),
                    )
                    .child(Input::new(input))
                    .children(
                        error
                            .clone()
                            .map(|error| div().text_sm().text_color(danger).child(error)),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_end()
                            .child(Button::new("folder-cancel").label("Cancel").on_click(
                                cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.close_dialog(window, cx);
                                }),
                            ))
                            .child(
                                Button::new("folder-ok")
                                    .primary()
                                    .label(match purpose {
                                        FolderPurpose::Create { .. } => "Create",
                                        FolderPurpose::Rename(_) => "Rename",
                                    })
                                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                        this.submit_folder_name(window, cx);
                                    })),
                            ),
                    ),
                Dialog::Confirm {
                    title,
                    message,
                    confirm,
                    ..
                } => Self::card(cx)
                    .track_focus(&self.prompt_focus)
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(title.clone()),
                    )
                    .child(div().text_sm().text_color(muted).child(message.clone()))
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_end()
                            .child(Button::new("confirm-cancel").label("Cancel").on_click(
                                cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.close_dialog(window, cx);
                                }),
                            ))
                            .child(
                                Button::new("confirm-ok")
                                    .danger()
                                    .label(confirm.clone())
                                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                        this.run_confirmed(window, cx);
                                    })),
                            ),
                    ),
            };
        Some(
            Self::overlay(DIALOG_CONTEXT)
                .on_action(cx.listener(Self::cancel_dialog))
                .child(card),
        )
    }

    pub(super) fn render_prompt(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let card = match self.prompt.as_ref()? {
            ActivePrompt::HostKey { prompt, .. } => self.render_host_key_prompt(prompt, cx),
            ActivePrompt::Secret {
                request,
                input,
                remember,
                ..
            } => Self::render_secret_prompt(request, input, *remember, cx),
            ActivePrompt::Master {
                request,
                input,
                confirm,
                error,
                ..
            } => Self::render_master_prompt(request, input, confirm.as_ref(), error.as_ref(), cx),
            ActivePrompt::Conflict {
                conflict,
                apply_to_all,
                ..
            } => self.render_conflict_prompt(conflict, *apply_to_all, cx),
        };
        Some(
            Self::overlay(PROMPT_CONTEXT)
                .on_action(cx.listener(Self::cancel_prompt))
                .child(card),
        )
    }

    fn render_host_key_prompt(&self, prompt: &HostKeyPrompt, cx: &mut Context<Self>) -> gpui::Div {
        let theme = cx.theme();
        let (muted, danger, code_bg) = (theme.muted_foreground, theme.danger, theme.muted);
        let decide = |decision: HostKeyDecision| {
            cx.listener(move |this: &mut Self, _: &ClickEvent, window, cx| {
                this.answer_host_key(decision, window, cx);
            })
        };
        Self::card(cx)
            .track_focus(&self.prompt_focus)
            .child(
                div()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child("Unknown host key"),
            )
            .child(div().text_sm().child(format!(
                "Ferry has not connected to {} before. Compare the fingerprint with the one \
                 the server's administrator gave you before you trust it.",
                prompt.host
            )))
            .child(
                v_flex()
                    .p_3()
                    .gap_1()
                    .rounded_md()
                    .bg(code_bg)
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(prompt.algorithm.clone()),
                    )
                    .child(
                        div()
                            .text_sm()
                            .font_family(crate::fonts::monospace())
                            .child(prompt.fingerprint.clone()),
                    ),
            )
            .when(prompt.other_types_known, |this| {
                this.child(div().text_sm().text_color(danger).child(
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
        request: &SecretRequest,
        input: &Entity<InputState>,
        remember: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let answer = |submit: bool| {
            cx.listener(move |this: &mut Self, _: &ClickEvent, window, cx| {
                this.answer_secret(submit, window, cx);
            })
        };
        let remember_box = request.remember.is_some().then(|| {
            Checkbox::new("remember-secret")
                .label("Remember")
                .checked(remember)
                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                    if let Some(ActivePrompt::Secret { remember, .. }) = &mut this.prompt {
                        *remember = *checked;
                        cx.notify();
                    }
                }))
        });
        Self::card(cx)
            .child(
                div()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(request.title.clone()),
            )
            .child(div().text_sm().child(request.message.clone()))
            .child(Input::new(input))
            .child(
                h_flex()
                    .gap_2()
                    .children(remember_box)
                    .child(div().flex_1())
                    .child(
                        Button::new("secret-cancel")
                            .label("Cancel")
                            .on_click(answer(false)),
                    )
                    .child(
                        Button::new("secret-ok")
                            .primary()
                            .label("Log in")
                            .on_click(answer(true)),
                    ),
            )
    }

    fn render_master_prompt(
        request: &MasterPasswordRequest,
        input: &Entity<InputState>,
        confirm: Option<&Entity<InputState>>,
        error: Option<&SharedString>,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let theme = cx.theme();
        let (muted, danger) = (theme.muted_foreground, theme.danger);
        let answer = |submit: bool| {
            cx.listener(move |this: &mut Self, _: &ClickEvent, window, cx| {
                this.answer_master(submit, window, cx);
            })
        };
        let (title, message) = if request.create {
            (
                "Protect saved passwords",
                "No system keyring is available, so Ferry keeps saved passwords in an \
                 encrypted file. Choose a master password for it. It cannot be recovered.",
            )
        } else if request.retry {
            (
                "Unlock saved passwords",
                "That master password is not correct. Try again.",
            )
        } else {
            (
                "Unlock saved passwords",
                "Enter the master password of Ferry's password file.",
            )
        };
        Self::card(cx)
            .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child(title))
            .child(
                div()
                    .text_sm()
                    .when(request.retry, |this| this.text_color(danger))
                    .child(message),
            )
            .child(Input::new(input))
            .children(confirm.map(|confirm| {
                v_flex()
                    .gap_1()
                    .child(div().text_xs().text_color(muted).child("Repeat it"))
                    .child(Input::new(confirm))
            }))
            .children(error.map(|error| div().text_sm().text_color(danger).child(error.clone())))
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("master-cancel")
                            .label(if request.create { "Don't save" } else { "Skip" })
                            .on_click(answer(false)),
                    )
                    .child(
                        Button::new("master-ok")
                            .primary()
                            .label(if request.create { "Create" } else { "Unlock" })
                            .on_click(answer(true)),
                    ),
            )
    }

    fn render_conflict_prompt(
        &self,
        conflict: &Conflict,
        apply_to_all: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let theme = cx.theme();
        let (muted, code_bg) = (theme.muted_foreground, theme.muted);
        let name = conflict.path.file_name().unwrap_or("/").to_owned();
        let folder = conflict
            .path
            .parent()
            .map(|parent| parent.to_string())
            .unwrap_or_default();
        let newer = match (conflict.source_modified, conflict.existing_modified) {
            (Some(new), Some(old)) if new > old => Some(true),
            (Some(new), Some(old)) if new < old => Some(false),
            _ => None,
        };
        let describe = |label: &'static str, size: u64, modified, highlight: bool| {
            h_flex()
                .gap_2()
                .child(div().w(px(72.)).text_color(muted).child(label))
                .child(crate::format::size(size))
                .child(div().text_color(muted).child("·"))
                .child(
                    div()
                        .when(highlight, |this| {
                            this.font_weight(gpui::FontWeight::SEMIBOLD)
                        })
                        .child(crate::format::date(modified)),
                )
                .when(highlight, |this| {
                    this.child(div().text_xs().text_color(muted).child("newer"))
                })
        };
        Self::card(cx)
            .w(px(500.))
            .track_focus(&self.prompt_focus)
            .child(
                div()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(format!("Replace “{name}”?")),
            )
            .child(
                div()
                    .text_sm()
                    .child(format!("A file with this name already exists in {folder}.")),
            )
            .child(
                v_flex()
                    .p_3()
                    .gap_1()
                    .rounded_md()
                    .bg(code_bg)
                    .text_sm()
                    .child(describe(
                        "Existing",
                        conflict.existing_size,
                        conflict.existing_modified,
                        newer == Some(false),
                    ))
                    .child(describe(
                        "New",
                        conflict.source_size,
                        conflict.source_modified,
                        newer == Some(true),
                    )),
            )
            .child(
                Checkbox::new("conflict-apply-all")
                    .label("Do the same for all conflicts in this transfer")
                    .checked(apply_to_all)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        if let Some(ActivePrompt::Conflict { apply_to_all, .. }) = &mut this.prompt
                        {
                            *apply_to_all = *checked;
                            cx.notify();
                        }
                    })),
            )
            .child(Self::conflict_buttons(cx))
    }

    fn conflict_buttons(cx: &mut Context<Self>) -> gpui::Div {
        let decide = |action: ConflictAction| {
            cx.listener(move |this: &mut Self, _: &ClickEvent, window, cx| {
                this.answer_conflict(Some(action), window, cx);
            })
        };
        h_flex()
            .gap_2()
            .child(
                Button::new("conflict-stop")
                    .ghost()
                    .label("Stop transfer")
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.answer_conflict(None, window, cx);
                    })),
            )
            .child(div().flex_1())
            .child(
                Button::new("conflict-skip")
                    .label("Skip")
                    .on_click(decide(ConflictAction::Skip)),
            )
            .child(
                Button::new("conflict-keep")
                    .label("Keep both")
                    .on_click(decide(ConflictAction::KeepBoth)),
            )
            .child(
                Button::new("conflict-replace")
                    .primary()
                    .label("Replace")
                    .on_click(decide(ConflictAction::Overwrite)),
            )
    }
}
