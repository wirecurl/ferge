//! Favorites, folders and history: loading the library and the SSH hosts,
//! and carrying out what the server panels ask for.

use std::sync::Arc;

use ferry_core::Destination;
use ferry_core::library::{
    BookmarkDraft, BookmarkId, FolderId, Library, LibraryError, LibrarySnapshot,
};
use ferry_core::remote::sftp::{SshConfigSource, configured_hosts};
use gpui::{Context, Entity, SharedString, Task, Window};

use super::Workspace;
use super::dialogs::{ConfirmAction, FolderPurpose};
use crate::runtime;
use crate::session::{LibraryAction, Session};

impl Workspace {
    pub(super) fn load_ssh_hosts(&self, cx: &mut Context<Self>) -> Task<()> {
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
                let sessions: Vec<Entity<Session>> = workspace.all_sessions().cloned().collect();
                for session in sessions {
                    let hosts = Arc::clone(&workspace.ssh_hosts);
                    session.update(cx, |session, cx| session.set_ssh_hosts(hosts, cx));
                }
                cx.notify();
            });
        })
    }

    pub(super) fn load_library(ephemeral: bool, cx: &mut Context<Self>) -> Task<()> {
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
    pub(super) fn change_library(
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

    pub(super) fn set_snapshot(&mut self, snapshot: LibrarySnapshot, cx: &mut Context<Self>) {
        self.snapshot = Arc::new(snapshot);
        let sessions: Vec<Entity<Session>> = self.all_sessions().cloned().collect();
        for session in sessions {
            let snapshot = Arc::clone(&self.snapshot);
            session.update(cx, |session, cx| session.set_library(snapshot, cx));
        }
        cx.notify();
    }

    /// Carries out a request from a server panel.
    pub(super) fn on_library_action(
        &mut self,
        action: LibraryAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            LibraryAction::NewFavorite {
                folder,
                destination,
            } => self.new_favorite(folder, destination, window, cx),
            LibraryAction::EditFavorite(id) => self.edit_favorite(id, window, cx),
            LibraryAction::DuplicateFavorite(id) => self.duplicate_favorite(id, cx),
            LibraryAction::DeleteFavorite(id) => {
                self.confirm(ConfirmAction::DeleteBookmark(id), window, cx);
            }
            LibraryAction::NewFolder { parent } => {
                self.ask_folder_name(FolderPurpose::Create { parent }, window, cx);
            }
            LibraryAction::RenameFolder(id) => {
                self.ask_folder_name(FolderPurpose::Rename(id), window, cx);
            }
            LibraryAction::DeleteFolder(id) => {
                self.confirm(ConfirmAction::DeleteFolder(id), window, cx);
            }
            LibraryAction::ClearHistory => self.confirm(ConfirmAction::ClearHistory, window, cx),
            LibraryAction::ForgetRecent(destination) => {
                self.change_library(move |library| library.forget_connection(&destination), cx);
            }
        }
    }

    /// Opens the favorite editor, prefilled with `destination` or the active
    /// pane's location.
    pub(super) fn new_favorite(
        &mut self,
        folder: Option<FolderId>,
        destination: Option<Destination>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let destination = destination
            .or_else(|| {
                self.active_session()
                    .and_then(|session| session.read(cx).location(cx))
            })
            .unwrap_or(Destination::Local { path: None });
        let draft = BookmarkDraft {
            folder,
            name: String::new(),
            destination,
            identity_file: None,
        };
        self.open_editor(None, &draft, window, cx);
    }

    fn edit_favorite(&mut self, id: BookmarkId, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(bookmark) = self.snapshot.bookmark(id).cloned() {
            self.open_editor(Some(id), &bookmark.into(), window, cx);
        }
    }

    fn duplicate_favorite(&mut self, id: BookmarkId, cx: &mut Context<Self>) {
        let Some(bookmark) = self.snapshot.bookmark(id).cloned() else {
            return;
        };
        let mut draft = BookmarkDraft::from(bookmark);
        draft.name = format!("{} copy", draft.name);
        self.change_library(
            move |library| library.create_bookmark(&draft).map(|_| ()),
            cx,
        );
    }
}
