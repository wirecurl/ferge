//! Modal dialogs: questions from connections (host keys, passwords, the
//! master password) and the window's own dialogs (favorite editor, folder
//! names, confirmations).
//!
//! Questions from connections take precedence and are shown on top; they
//! belong to a tab and disappear with it.

use ferge_core::interaction::{HostKeyDecision, HostKeyPrompt};
use ferge_core::library::{BookmarkDraft, BookmarkId, FolderId, Library};
use ferge_core::transfer::{
    Conflict, ConflictAction, ConflictDecision, ConflictKind, DirectRequest, TransferId,
};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AppContext as _, ClickEvent, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, Styled as _, Subscription, Window, div,
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
    DirectAnswer, MasterPasswordRequest, PromptRequest, SecretRequest, SessionId, TaggedPrompt,
};
use crate::style::{self, TypeScale as _};

/// Key context of the question overlay.
const PROMPT_CONTEXT: &str = "FergePrompt";
/// Key context of the dialog overlay.
const DIALOG_CONTEXT: &str = "FergeDialog";

/// Shortest master password accepted for a new password file.
const MIN_MASTER_PASSWORD_LEN: usize = 8;

gpui::actions!(ferge_dialog, [CancelPrompt, CancelDialog, ConfirmDialog]);

pub(super) fn bind_keys(cx: &mut gpui::App) {
    cx.bind_keys([
        gpui::KeyBinding::new("escape", CancelPrompt, Some(PROMPT_CONTEXT)),
        gpui::KeyBinding::new("escape", CancelDialog, Some(DIALOG_CONTEXT)),
        gpui::KeyBinding::new("enter", ConfirmDialog, Some(DIALOG_CONTEXT)),
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
    Direct {
        request: DirectRequest,
        reply: futures::channel::oneshot::Sender<DirectAnswer>,
        remember: bool,
    },
}

impl ActivePrompt {
    fn session(&self) -> Option<SessionId> {
        match self {
            Self::HostKey { session, .. }
            | Self::Secret { session, .. }
            | Self::Master { session, .. } => *session,
            Self::Conflict { .. } | Self::Direct { .. } => None,
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

#[derive(Clone)]
pub(super) enum ConfirmAction {
    DeleteBookmark(BookmarkId),
    DeleteFolder(FolderId),
    ClearHistory,
    /// Delete files and folders shown in `browser`.
    DeleteFiles {
        browser: gpui::WeakEntity<crate::browser::FileBrowser>,
        items: Vec<(ferge_core::RemotePath, ferge_core::EntryKind)>,
    },
    /// Close the window although these transfers need ferge.
    CloseWhileCopying(Vec<TransferId>),
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
        let of_transfer = |request: &PromptRequest| match request {
            PromptRequest::Conflict { conflict, .. } => conflict.transfer == transfer,
            PromptRequest::Direct { request, .. } => request.transfer == transfer,
            _ => false,
        };
        self.queued_prompts
            .retain(|prompt| !of_transfer(&prompt.request));
        let shown = match &self.prompt {
            Some(ActivePrompt::Conflict { conflict, .. }) => conflict.transfer == transfer,
            Some(ActivePrompt::Direct { request, .. }) => request.transfer == transfer,
            _ => false,
        };
        if shown {
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
            self.sync_active(window, cx);
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
            PromptRequest::Direct { request, reply } => {
                window.focus(&self.prompt_focus, cx);
                Some(ActivePrompt::Direct {
                    request,
                    reply,
                    remember: false,
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

    /// Copy directly, or (`false`, also when dismissed) through this
    /// computer.
    fn answer_direct(&mut self, allow: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ActivePrompt::Direct {
            reply, remember, ..
        }) = self.prompt.take()
        {
            let _ = reply.send(DirectAnswer { allow, remember });
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
            // Held as a secret, so the copy is wiped when it goes.
            let password = SecretString::from(input.read(cx).value().to_string());
            if let Some(confirm) = confirm {
                let problem = if password.expose_secret().chars().count() < MIN_MASTER_PASSWORD_LEN
                {
                    Some(format!(
                        "Use at least {MIN_MASTER_PASSWORD_LEN} characters."
                    ))
                } else if confirm.read(cx).value().as_ref() != password.expose_secret() {
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
            Some(ActivePrompt::Direct { .. }) => self.answer_direct(false, window, cx),
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
        let (title, message, confirm): (SharedString, SharedString, SharedString) = match &action {
            ConfirmAction::DeleteFiles { items, .. } => {
                let folders = items
                    .iter()
                    .filter(|(_, kind)| *kind == ferge_core::EntryKind::Directory)
                    .count();
                let what = match items.as_slice() {
                    [(path, _)] => format!(
                        "“{}”",
                        crate::format::shown(path.file_name().unwrap_or("/"))
                    ),
                    _ => format!("{} items", items.len()),
                };
                let contents = if folders > 0 {
                    " Folders are deleted with everything in them."
                } else {
                    ""
                };
                (
                    format!("Delete {what}?").into(),
                    format!("This cannot be undone.{contents}").into(),
                    "Delete".into(),
                )
            }
            ConfirmAction::DeleteBookmark(id) => {
                let id = *id;
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
                let id = *id;
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
            ConfirmAction::CloseWhileCopying(transfers) => (
                if transfers.len() == 1 {
                    "A transfer is still running".into()
                } else {
                    format!("{} transfers are still running", transfers.len()).into()
                },
                "They go through this computer and stop when ferge closes. They pause and \
                 are back in the list at the next start, ready to resume where they stopped."
                    .into(),
                "Pause and close".into(),
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
        match action.clone() {
            ConfirmAction::DeleteFiles { browser, items } => {
                if let Some(browser) = browser.upgrade() {
                    browser.update(cx, |browser, cx| browser.delete(items, cx));
                }
            }
            ConfirmAction::DeleteBookmark(id) => {
                self.change_library(move |library| library.delete_bookmark(id), cx);
            }
            ConfirmAction::DeleteFolder(id) => {
                self.change_library(move |library| library.delete_folder(id), cx);
            }
            ConfirmAction::ClearHistory => self.change_library(Library::clear_history, cx),
            ConfirmAction::CloseWhileCopying(transfers) => {
                self.close_dialog(window, cx);
                self.close_after_pausing(transfers, window, cx);
                return;
            }
        }
        self.close_dialog(window, cx);
    }

    fn close_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dialog = None;
        if self.prompt.is_none() {
            self.restore_focus(window, cx);
        }
        cx.notify();
    }

    /// Enter confirms a question; other dialogs handle Enter in their fields.
    fn confirm_dialog(&mut self, _: &ConfirmDialog, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.dialog, Some(Dialog::Confirm { .. })) {
            self.run_confirmed(window, cx);
        }
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
            .w(style::em(33.))
            .max_w_full()
            .p_5()
            .gap_3()
            .rounded(style::GROUP_RADIUS)
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
                Dialog::Editor { editor, .. } => {
                    Self::card(cx).w(style::em(37.)).child(editor.clone())
                }
                Dialog::FolderName {
                    input,
                    purpose,
                    error,
                    ..
                } => Self::card(cx)
                    .child(div().text_title().child(match purpose {
                        FolderPurpose::Create { .. } => "New folder",
                        FolderPurpose::Rename(_) => "Rename folder",
                    }))
                    .child(Input::new(input))
                    .children(
                        error
                            .clone()
                            .map(|error| div().text_body().text_color(danger).child(error)),
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
                    .child(div().text_title().child(title.clone()))
                    .child(div().text_body().text_color(muted).child(message.clone()))
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
                .on_action(cx.listener(Self::confirm_dialog))
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
            ActivePrompt::Direct {
                request, remember, ..
            } => self.render_direct_prompt(request, *remember, cx),
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
            .child(div().text_title().child("Unknown host key"))
            .child(div().text_body().child(format!(
                "ferge has not connected to {} before. Compare the fingerprint with the one \
                 the server's administrator gave you before you trust it.",
                prompt.host
            )))
            .child(
                v_flex()
                    .p_3()
                    .gap_1()
                    .rounded(style::RADIUS)
                    .bg(code_bg)
                    .child(
                        div()
                            .text_small()
                            .text_color(muted)
                            .child(prompt.algorithm.clone()),
                    )
                    .child(
                        div()
                            .text_body()
                            .font_family(cx.theme().mono_font_family.clone())
                            .child(prompt.fingerprint.clone()),
                    ),
            )
            .when(prompt.other_types_known, |this| {
                this.child(div().text_body().text_color(danger).child(
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
            .child(div().text_title().child(request.title.clone()))
            .child(div().text_body().child(request.message.clone()))
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
                            .label(request.confirm)
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
                "No system keyring is available, so ferge keeps saved passwords in an \
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
                "Enter the master password of ferge's password file.",
            )
        };
        Self::card(cx)
            .child(div().text_title().child(title))
            .child(
                div()
                    .text_body()
                    .when(request.retry, |this| this.text_color(danger))
                    .child(message),
            )
            .child(Input::new(input))
            .children(confirm.map(|confirm| {
                v_flex()
                    .gap_1()
                    .child(div().text_small().text_color(muted).child("Repeat it"))
                    .child(Input::new(confirm))
            }))
            .children(error.map(|error| div().text_body().text_color(danger).child(error.clone())))
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
        let name = crate::format::shown(conflict.path.file_name().unwrap_or("/"));
        let folder = conflict
            .path
            .parent()
            .map(|parent| crate::format::shown(parent.as_str()))
            .unwrap_or_default();
        let newer = match (conflict.source_modified, conflict.existing_modified) {
            (Some(new), Some(old)) if new > old => Some(true),
            (Some(new), Some(old)) if new < old => Some(false),
            _ => None,
        };
        let describe = |label: &'static str, size: u64, modified, highlight: bool| {
            h_flex()
                .gap_2()
                .child(div().w(style::em(5.5)).text_color(muted).child(label))
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
                    this.child(div().text_small().text_color(muted).child("newer"))
                })
        };
        let card = Self::card(cx)
            .w(style::em(36.))
            .track_focus(&self.prompt_focus);
        let card = if conflict.kind == ConflictKind::Folder {
            card.child(div().text_title().child(format!("Merge into “{name}”?")))
                .child(div().text_body().child(format!(
                    "A folder with this name already exists in {folder}. Merging puts the \
                     copied items into it; files that exist in both are asked about one by one."
                )))
        } else {
            card.child(div().text_title().child(format!("Replace “{name}”?")))
                .child(
                    div()
                        .text_body()
                        .child(format!("A file with this name already exists in {folder}.")),
                )
                .child(
                    v_flex()
                        .p_3()
                        .gap_1()
                        .rounded(style::RADIUS)
                        .bg(code_bg)
                        .text_body()
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
        };
        card.child(
            Checkbox::new("conflict-apply-all")
                .label(if conflict.kind == ConflictKind::Folder {
                    "Do the same for all folders in this transfer"
                } else {
                    "Do the same for all files in this transfer"
                })
                .checked(apply_to_all)
                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                    if let Some(ActivePrompt::Conflict { apply_to_all, .. }) = &mut this.prompt {
                        *apply_to_all = *checked;
                        cx.notify();
                    }
                })),
        )
        .child(Self::conflict_buttons(conflict.kind, cx))
    }

    fn render_direct_prompt(
        &self,
        request: &DirectRequest,
        remember: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let theme = cx.theme();
        let (muted, code_bg) = (theme.muted_foreground, theme.muted);
        let answer = |allow: bool| {
            cx.listener(move |this: &mut Self, _: &ClickEvent, window, cx| {
                this.answer_direct(allow, window, cx);
            })
        };
        Self::card(cx)
            .w(style::em(37.))
            .track_focus(&self.prompt_focus)
            .child(
                div()
                    .text_title()
                    .child("Copy directly between the servers?"),
            )
            .child(div().text_body().child(format!(
                "{} and {} can exchange the files themselves, without going through this \
                 computer. That is usually much faster, and the copy goes on if this \
                 computer sleeps or ferge quits.",
                request.source, request.target
            )))
            .child(
                v_flex()
                    .p_3()
                    .gap_1()
                    .rounded(style::RADIUS)
                    .bg(code_bg)
                    .text_body()
                    .child(
                        "For this, ferge adds a key for this transfer alone to \
                         ~/.ssh/authorized_keys on one of them, and a job folder in \
                         ~/.cache/ferge on both.",
                    )
                    .child(div().text_color(muted).child(
                        "When the copy is done, ferge removes all of it again and checks \
                         that both servers are left as it found them.",
                    )),
            )
            .child(
                Checkbox::new("direct-remember")
                    .label("Give the same answer for these servers until ferge quits")
                    .checked(remember)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        if let Some(ActivePrompt::Direct { remember, .. }) = &mut this.prompt {
                            *remember = *checked;
                            cx.notify();
                        }
                    })),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1())
                    .child(
                        Button::new("direct-relay")
                            .label("Through this computer")
                            .on_click(answer(false)),
                    )
                    .child(
                        Button::new("direct-allow")
                            .primary()
                            .label("Copy directly")
                            .on_click(answer(true)),
                    ),
            )
    }

    fn conflict_buttons(kind: ConflictKind, cx: &mut Context<Self>) -> gpui::Div {
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
                    .label(if kind == ConflictKind::Folder {
                        "Merge"
                    } else {
                        "Replace"
                    })
                    .on_click(decide(ConflictAction::Overwrite)),
            )
    }
}
