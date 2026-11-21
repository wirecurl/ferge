//! File operations of the browser: new folder or file, rename in place,
//! delete, duplicate, copy address. The work runs on the I/O runtime; the
//! listing is reloaded when it is done.

use std::sync::Arc;

use ferry_core::transfer::TransferRequest;
use ferry_core::{EntryKind, EntryName, RemotePath, ops};
use gpui::{AppContext as _, ClipboardItem, Context, Entity, Subscription, Window};
use gpui_component::input::{InputEvent, InputState};

use super::{BrowserEvent, FileBrowser};
use crate::actions;
use crate::runtime;
use crate::transfers::Transfers;

/// Name editing in place of a row's label.
pub(super) struct Renaming {
    /// The folder the entry is in; navigating away does not change it.
    dir: RemotePath,
    pub name: EntryName,
    pub input: Entity<InputState>,
    _events: Subscription,
}

impl FileBrowser {
    /// The address of `path` on this browser's server, e.g. for the clipboard.
    pub(crate) fn address_of(&self, path: &RemotePath) -> String {
        self.address.with_path(Some(path.clone())).address()
    }

    pub(super) fn renaming(&self) -> Option<&Renaming> {
        self.renaming.as_ref()
    }

    /// Starts editing the name of `name` in its row.
    pub(crate) fn start_rename(
        &mut self,
        name: EntryName,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(row) = self.view.position(name.as_str()) {
            self.select(row, cx);
        }
        let text = name.as_str().to_owned();
        // Like file managers do: select the name without its extension.
        let stem = match text.rfind('.') {
            Some(dot) if dot > 0 => dot,
            _ => text.len(),
        };
        let input = cx.new(|cx| InputState::new(window, cx).default_value(text));
        input.update(cx, |input, cx| {
            input.focus(window, cx);
            input.set_selected_range(0..stem, cx);
        });
        let events =
            cx.subscribe_in(
                &input,
                window,
                |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { .. } | InputEvent::Blur => {
                        this.commit_rename(window, cx);
                    }
                    _ => {}
                },
            );
        self.renaming = Some(Renaming {
            dir: self.path.clone(),
            name,
            input,
            _events: events,
        });
        cx.notify();
    }

    fn commit_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(renaming) = self.renaming.take() else {
            return;
        };
        window.focus(&self.focus, cx);
        cx.notify();
        let text = renaming.input.read(cx).value().trim().to_owned();
        if text.is_empty() || text == renaming.name.as_str() {
            return;
        }
        let Ok(new_name) = EntryName::new(&text) else {
            cx.emit(BrowserEvent::Failed(format!(
                "“{text}” is not a valid name: names cannot contain “/”."
            )));
            return;
        };
        let from = renaming.dir.join(&renaming.name);
        let to = renaming.dir.join(&new_name);
        let remote = Arc::clone(&self.remote);
        let renaming_task = runtime::spawn(cx, async move { remote.rename(&from, &to).await });
        cx.spawn(async move |this, cx| {
            let result = renaming_task.await;
            let _ = this.update(cx, |browser, cx| match result {
                Ok(Ok(())) => {
                    browser.selected_name = Some(new_name);
                    browser.reload(cx);
                }
                Ok(Err(error)) => cx.emit(BrowserEvent::Failed(error.to_string())),
                Err(error) => cx.emit(BrowserEvent::Failed(error.to_string())),
            });
        })
        .detach();
    }

    pub(super) fn on_cancel_rename(
        &mut self,
        _: &actions::CancelRename,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Dropping the state first keeps the blur from committing.
        self.renaming = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    pub(super) fn on_commit_rename(
        &mut self,
        _: &actions::CommitRename,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.commit_rename(window, cx);
    }

    pub(super) fn on_rename(
        &mut self,
        _: &actions::Rename,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = self
            .selected
            .and_then(|row| self.view.get(row))
            .map(|entry| entry.name.clone());
        if let Some(name) = name {
            self.start_rename(name, window, cx);
        }
    }

    /// Creates a folder or an empty file with a free name, then lets the user
    /// name it.
    fn create(&mut self, folder: bool, cx: &mut Context<Self>) {
        let remote = Arc::clone(&self.remote);
        let dir = self.path.clone();
        let creating = runtime::spawn(cx, async move {
            let base = if folder {
                "untitled folder"
            } else {
                "untitled.txt"
            };
            let name = ops::free_name(remote.as_ref(), &dir, base).await?;
            let path = dir.join(&name);
            if folder {
                remote.mkdir(&path).await?;
            } else {
                ops::create_file(remote.as_ref(), &path).await?;
            }
            Ok::<_, ferry_core::Error>(name)
        });
        cx.spawn(async move |this, cx| {
            let result = creating.await;
            let _ = this.update(cx, |browser, cx| match result {
                Ok(Ok(name)) => {
                    browser.rename_when_listed = Some(name);
                    browser.reload(cx);
                }
                Ok(Err(error)) => cx.emit(BrowserEvent::Failed(error.to_string())),
                Err(error) => cx.emit(BrowserEvent::Failed(error.to_string())),
            });
        })
        .detach();
    }

    pub(super) fn on_new_folder(
        &mut self,
        _: &actions::NewFolder,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.create(true, cx);
    }

    pub(super) fn on_new_file(
        &mut self,
        _: &actions::NewFile,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.create(false, cx);
    }

    /// Starts renaming an entry created a moment ago once it is listed.
    pub(super) fn rename_new_entry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.rename_when_listed.clone() else {
            return;
        };
        if self.view.position(name.as_str()).is_some() {
            self.rename_when_listed = None;
            self.start_rename(name, window, cx);
        }
    }

    /// The selected items with their kinds, for deleting.
    fn selected_items(&self) -> Vec<(RemotePath, EntryKind)> {
        self.selected_paths()
            .into_iter()
            .map(|path| {
                let kind = path
                    .file_name()
                    .and_then(|name| self.view.position(name))
                    .and_then(|row| self.view.get(row))
                    .map_or(EntryKind::File, ferry_core::Entry::kind);
                (path, kind)
            })
            .collect()
    }

    pub(super) fn on_delete(
        &mut self,
        _: &actions::Delete,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let items = self.selected_items();
        if !items.is_empty() {
            cx.emit(BrowserEvent::Delete(items));
        }
    }

    /// Deletes `items` (after the user confirmed) and reloads.
    pub(crate) fn delete(&mut self, items: Vec<(RemotePath, EntryKind)>, cx: &mut Context<Self>) {
        let total = items.len();
        self.notice = Some(match total {
            1 => "Deleting…".into(),
            n => format!("Deleting {n} items…").into(),
        });
        cx.notify();
        let remote = Arc::clone(&self.remote);
        let deleting = runtime::spawn(cx, async move {
            ops::delete(remote.as_ref(), &items, &|_| {}).await
        });
        cx.spawn(async move |this, cx| {
            let result = deleting.await;
            let _ = this.update(cx, |browser, cx| {
                browser.notice = None;
                match result {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => cx.emit(BrowserEvent::Failed(error.to_string())),
                    Err(error) => cx.emit(BrowserEvent::Failed(error.to_string())),
                }
                // Reload either way: some items may be gone.
                browser.marked.clear();
                browser.reload(cx);
            });
        })
        .detach();
    }

    pub(super) fn on_duplicate(
        &mut self,
        _: &actions::Duplicate,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let items = self.selected_paths();
        if items.is_empty() {
            return;
        }
        // Copying onto the same folder keeps both copies: "name (2)".
        Transfers::start(
            TransferRequest {
                source: Arc::clone(&self.remote),
                items,
                target: Arc::clone(&self.remote),
                target_dir: self.path.clone(),
            },
            cx,
        );
    }

    pub(super) fn on_copy_address(
        &mut self,
        _: &actions::CopyAddress,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut items = self.selected_paths();
        if items.is_empty() {
            items.push(self.path.clone());
        }
        let text = items
            .iter()
            .map(|path| self.address_of(path))
            .collect::<Vec<_>>()
            .join("\n");
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.notice = Some(match items.len() {
            1 => "Address copied.".into(),
            n => format!("{n} addresses copied.").into(),
        });
        cx.notify();
    }
}
