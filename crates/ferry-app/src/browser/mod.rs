//! The file browser pane: one remote, one directory at a time.
//!
//! All I/O happens in ferry-core on tokio. This entity only consumes
//! [`ListingEvent`]s, keeps a [`DirectoryView`] and moves the selection. Work
//! that is linear in the size of the listing but too slow for a frame (full
//! sorts, rebuilding from a cached listing) runs on GPUI's background executor.

mod ops;
mod render;

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use ferry_core::listing::{self, DirectoryView, ListingCache, ListingEvent, SortColumn, SortOrder};
use ferry_core::transfer::TransferRequest;
use ferry_core::{Destination, Entry, EntryKind, EntryName, Remote, RemotePath};
use futures::{FutureExt as _, StreamExt as _};
use gpui::{
    App, AppContext as _, Context, FocusHandle, Focusable, Global, KeyDownEvent, ScrollStrategy,
    SharedString, Task, UniformListScrollHandle, WeakEntity, Window,
};

use crate::actions;
use crate::runtime;
use crate::transfers::Transfers;

/// Recently shown directories kept with their sorted views, so going back or
/// up is instant without re-sorting.
const VIEW_CACHE_CAPACITY: usize = 6;

/// Listing events handled per UI update; the rest waits for the next turn so
/// a fast server cannot starve rendering.
const EVENTS_PER_UPDATE: usize = 64;

/// Where the current listing stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LoadState {
    /// Nothing cached: rows appear as they arrive.
    Loading,
    /// Showing a cached listing while a fresh one is fetched.
    Revalidating,
    Complete,
    Failed(String),
}

/// Emitted when the browser's directory or status changes, for window titles
/// and the like.
#[derive(Clone, Debug)]
pub(crate) struct Navigated;

impl gpui::EventEmitter<Navigated> for FileBrowser {}

/// Requests the browser cannot handle on its own.
#[derive(Clone, Debug)]
pub(crate) enum BrowserEvent {
    /// The user wants these deleted; ask before calling [`FileBrowser::delete`].
    Delete(Vec<(RemotePath, EntryKind)>),
    /// An operation failed; show `message` to the user.
    Failed(String),
}

impl gpui::EventEmitter<BrowserEvent> for FileBrowser {}

/// What the list shows besides names, sizes and dates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ViewOptions {
    /// Entries whose names start with a dot.
    pub show_hidden: bool,
    pub show_kind: bool,
    pub show_permissions: bool,
}

/// Items copied with Ctrl+C, waiting to be pasted. Shared by all tabs.
pub(crate) struct FileClipboard {
    source: Arc<dyn Remote>,
    items: Vec<RemotePath>,
}

impl Global for FileClipboard {}

/// What is carried while rows are dragged: the browser they come from and
/// the row that was grabbed. The items are looked up when they are dropped.
#[derive(Clone)]
pub(crate) struct DraggedEntries {
    pub source: WeakEntity<FileBrowser>,
    pub grabbed: EntryName,
    /// Shown in the drag preview.
    pub count: usize,
}

struct CachedView {
    path: RemotePath,
    view: DirectoryView,
    selected: Option<EntryName>,
}

pub(crate) struct FileBrowser {
    remote: Arc<dyn Remote>,
    /// Where the browser is connected, for addresses of its files.
    address: Destination,
    options: ViewOptions,
    renaming: Option<ops::Renaming>,
    /// An entry that was just created and should be named once it is listed.
    rename_when_listed: Option<EntryName>,
    cache: Arc<ListingCache>,
    path: RemotePath,
    back: Vec<RemotePath>,
    forward: Vec<RemotePath>,
    view: DirectoryView,
    views: VecDeque<CachedView>,
    sort: SortOrder,
    filter: String,
    /// Visible index of the selected row.
    selected: Option<usize>,
    /// Name of the selected entry, so the selection survives re-sorting,
    /// filtering and refreshes.
    selected_name: Option<EntryName>,
    /// Entries marked for a multiple selection (Shift, Ctrl/Cmd, select all).
    /// Empty means the selection is just the cursor row.
    marked: HashSet<EntryName>,
    /// Where Shift-selections start.
    anchor: Option<usize>,
    /// A short message for the status bar, e.g. after copying.
    notice: Option<SharedString>,
    state: LoadState,
    /// Incremented on every navigation; results of older work are dropped.
    generation: u64,
    /// Chunks received since the last frame, merged once per frame.
    pending: Vec<Entry>,
    listing_task: Option<Task<()>>,
    build_task: Option<Task<()>>,
    sort_task: Option<Task<()>>,
    scroll: UniformListScrollHandle,
    focus: FocusHandle,
    /// When the current navigation started, for the status line.
    navigated_at: Instant,
    first_rows_after: Option<std::time::Duration>,
}

impl FileBrowser {
    pub(crate) fn new(
        remote: Arc<dyn Remote>,
        address: &Destination,
        path: RemotePath,
        options: ViewOptions,
        cx: &mut Context<Self>,
    ) -> Self {
        // The pane decides whether the new list takes the keyboard.
        let focus = cx.focus_handle();
        let sort = SortOrder::default();
        let mut view = DirectoryView::new(sort);
        view.set_show_hidden(options.show_hidden);
        let mut browser = Self {
            remote,
            address: address.with_path(None),
            options,
            renaming: None,
            rename_when_listed: None,
            cache: Arc::new(ListingCache::default()),
            path,
            back: Vec::new(),
            forward: Vec::new(),
            view,
            views: VecDeque::new(),
            sort,
            filter: String::new(),
            selected: None,
            selected_name: None,
            marked: HashSet::new(),
            anchor: None,
            notice: None,
            state: LoadState::Loading,
            generation: 0,
            pending: Vec::new(),
            listing_task: None,
            build_task: None,
            sort_task: None,
            scroll: UniformListScrollHandle::new(),
            focus,
            navigated_at: Instant::now(),
            first_rows_after: None,
        };
        browser.load(cx);
        browser
    }

    pub(crate) fn path(&self) -> &RemotePath {
        &self.path
    }

    pub(crate) fn remote_label(&self) -> &str {
        self.remote.label()
    }

    pub(crate) fn remote(&self) -> &Arc<dyn Remote> {
        &self.remote
    }

    pub(crate) fn set_options(&mut self, options: ViewOptions, cx: &mut Context<Self>) {
        self.options = options;
        self.view.set_show_hidden(options.show_hidden);
        self.reselect();
        self.scroll_to_selection();
        cx.notify();
    }

    /// Number of entries the list shows, and how many hidden ones it leaves out.
    pub(crate) fn counts(&self) -> (usize, usize) {
        let shown = self.view.len();
        let hidden = if self.filter.is_empty() {
            self.view.total_len().saturating_sub(shown)
        } else {
            0
        };
        (shown, hidden)
    }

    /// The selected entries with their metadata, for the inspector.
    pub(crate) fn selected_entries(&self) -> Vec<Entry> {
        if self.marked.is_empty() {
            return self
                .selected
                .and_then(|row| self.view.get(row))
                .cloned()
                .into_iter()
                .collect();
        }
        self.marked
            .iter()
            .filter_map(|name| self.view.position(name.as_str()))
            .filter_map(|row| self.view.get(row).cloned())
            .collect()
    }

    /// Whether nothing is loading, building or sorting.
    pub(crate) fn is_idle(&self) -> bool {
        self.state == LoadState::Complete
            && self.pending.is_empty()
            && self.build_task.is_none()
            && !self.view.is_sorting()
    }

    pub(crate) fn total_len(&self) -> usize {
        self.view.total_len()
    }

    /// Time from navigation until the first rows were shown, if they streamed in.
    pub(crate) fn first_rows_after(&self) -> Option<std::time::Duration> {
        self.first_rows_after
    }

    /// Scrolls the list by `delta` like a mouse wheel would (benchmarks).
    pub(crate) fn scroll_by(&mut self, delta: gpui::Pixels, cx: &mut Context<Self>) {
        let handle = &self.scroll.0.borrow().base_handle;
        let mut offset = handle.offset();
        offset.y -= delta;
        handle.set_offset(offset);
        cx.notify();
    }

    // ---------------------------------------------------------------------
    // Navigation

    /// Shows `path`, remembering the current directory for "back".
    fn navigate(&mut self, path: RemotePath, select: Option<EntryName>, cx: &mut Context<Self>) {
        if path == self.path {
            return;
        }
        let previous = std::mem::replace(&mut self.path, path);
        self.back.push(previous.clone());
        self.forward.clear();
        self.switch_from(previous, select, cx);
    }

    fn switch_from(
        &mut self,
        previous: RemotePath,
        select: Option<EntryName>,
        cx: &mut Context<Self>,
    ) {
        // Park the current view; it is sorted already and cheap to bring back.
        let parked = std::mem::replace(&mut self.view, DirectoryView::new(self.sort));
        if self.state != LoadState::Loading || parked.total_len() > 0 {
            self.park_view(previous, parked);
        }
        self.filter.clear();
        self.selected = None;
        self.selected_name = select;
        self.marked.clear();
        self.anchor = None;
        self.notice = None;
        self.view.set_show_hidden(self.options.show_hidden);
        if let Some(cached) = self.take_parked_view(&self.path.clone()) {
            self.view = cached.view;
            self.view.set_filter("");
            self.view.set_show_hidden(self.options.show_hidden);
            if self.selected_name.is_none() {
                self.selected_name = cached.selected;
            }
            self.apply_sort_to_view(cx);
            self.reselect();
            self.state = LoadState::Revalidating;
        } else {
            self.state = LoadState::Loading;
        }
        self.scroll_to_selection();
        self.load(cx);
        cx.emit(Navigated);
        cx.notify();
    }

    fn park_view(&mut self, path: RemotePath, view: DirectoryView) {
        self.views.retain(|cached| cached.path != path);
        if self.views.len() == VIEW_CACHE_CAPACITY {
            self.views.pop_back();
        }
        self.views.push_front(CachedView {
            path,
            view,
            selected: self.selected_name.clone(),
        });
    }

    fn take_parked_view(&mut self, path: &RemotePath) -> Option<CachedView> {
        let position = self.views.iter().position(|cached| &cached.path == path)?;
        self.views.remove(position)
    }

    /// Starts listing the current path; replaces any listing in flight.
    fn load(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.pending.clear();
        self.build_task = None;
        self.navigated_at = Instant::now();
        self.first_rows_after = None;
        let stream = {
            let _runtime = runtime::handle(cx).enter();
            listing::browse(&self.remote, &self.cache, &self.path)
        };
        self.listing_task = Some(cx.spawn(async move |this, cx| {
            let mut stream = stream;
            let mut ended = false;
            while !ended {
                let Some(first) = stream.next().await else {
                    break;
                };
                // Hand over everything that is already available in one update.
                let mut batch = vec![first];
                while batch.len() < EVENTS_PER_UPDATE {
                    match stream.next().now_or_never() {
                        Some(Some(event)) => batch.push(event),
                        Some(None) => {
                            ended = true;
                            break;
                        }
                        None => break,
                    }
                }
                let alive = this.update(cx, |browser, cx| {
                    for event in batch {
                        browser.on_listing_event(generation, event, cx);
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        }));
    }

    fn on_listing_event(&mut self, generation: u64, event: ListingEvent, cx: &mut Context<Self>) {
        if generation != self.generation {
            return;
        }
        match event {
            ListingEvent::Cached(entries) => {
                // A parked view already shows this directory; otherwise build
                // one from the cached listing off the UI thread.
                if self.state == LoadState::Loading {
                    self.state = LoadState::Revalidating;
                    self.rebuild_from(entries, cx);
                }
            }
            ListingEvent::Chunk(entries) => {
                if self.state == LoadState::Loading {
                    self.pending.extend(entries);
                    cx.notify();
                }
            }
            ListingEvent::Complete(entries) => {
                if self.state == LoadState::Revalidating {
                    self.revalidate_with(entries, cx);
                } else {
                    self.flush_pending();
                }
                self.state = LoadState::Complete;
                cx.notify();
            }
            ListingEvent::Failed(error) => {
                self.flush_pending();
                tracing::warn!(path = %self.path, %error, "listing failed");
                self.state = LoadState::Failed(error.to_string());
                cx.notify();
            }
        }
    }

    /// Merges chunks that arrived since the last frame. Called from `render`,
    /// so a burst of chunks costs one merge per frame.
    fn flush_pending(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        if self.first_rows_after.is_none() {
            self.first_rows_after = Some(self.navigated_at.elapsed());
        }
        let entries = std::mem::take(&mut self.pending);
        self.view.extend(entries);
        self.reselect();
    }

    /// Replaces the view with one built from `entries` on a background thread.
    fn rebuild_from(&mut self, entries: Arc<[Entry]>, cx: &mut Context<Self>) {
        let generation = self.generation;
        let sort = self.sort;
        let filter = self.filter.clone();
        let show_hidden = self.options.show_hidden;
        self.build_task = Some(cx.spawn(async move |this, cx| {
            let view = cx
                .background_spawn(async move {
                    let mut view = DirectoryView::with_entries(sort, &filter, entries.to_vec());
                    view.set_show_hidden(show_hidden);
                    view
                })
                .await;
            let _ = this.update(cx, |browser, cx| {
                if browser.generation == generation {
                    browser.install_view(view, cx);
                }
            });
        }));
    }

    /// Swaps in a revalidated listing if it differs from what is shown.
    fn revalidate_with(&mut self, entries: Arc<[Entry]>, cx: &mut Context<Self>) {
        if self.build_task.is_some() {
            // Still building from the cache; build from the fresh data instead.
            self.rebuild_from(entries, cx);
            return;
        }
        let generation = self.generation;
        let snapshot = self.view.snapshot();
        let sort = self.sort;
        let filter = self.filter.clone();
        let show_hidden = self.options.show_hidden;
        self.build_task = Some(cx.spawn(async move |this, cx| {
            let view = cx
                .background_spawn(async move {
                    (!snapshot.matches(&entries)).then(|| {
                        let mut view = DirectoryView::with_entries(sort, &filter, entries.to_vec());
                        view.set_show_hidden(show_hidden);
                        view
                    })
                })
                .await;
            let _ = this.update(cx, |browser, cx| {
                if browser.generation != generation {
                    return;
                }
                browser.build_task = None;
                if let Some(view) = view {
                    browser.install_view(view, cx);
                }
            });
        }));
    }

    fn install_view(&mut self, view: DirectoryView, cx: &mut Context<Self>) {
        self.build_task = None;
        self.view = view;
        // Filter, options or sort may have changed while the view was built.
        self.view.set_filter(&self.filter);
        self.view.set_show_hidden(self.options.show_hidden);
        self.apply_sort_to_view(cx);
        self.reselect();
        self.scroll_to_selection();
        cx.notify();
    }

    pub(crate) fn go_to_parent(&mut self, cx: &mut Context<Self>) {
        let Some(parent) = self.path.parent() else {
            return;
        };
        let child = self
            .path
            .file_name()
            .and_then(|name| EntryName::new(name).ok());
        self.navigate(parent, child, cx);
    }

    pub(crate) fn go_back(&mut self, cx: &mut Context<Self>) {
        let Some(previous) = self.back.pop() else {
            return;
        };
        let current = std::mem::replace(&mut self.path, previous);
        self.forward.push(current.clone());
        self.switch_from(current, None, cx);
    }

    pub(crate) fn go_forward(&mut self, cx: &mut Context<Self>) {
        let Some(next) = self.forward.pop() else {
            return;
        };
        let current = std::mem::replace(&mut self.path, next);
        self.back.push(current.clone());
        self.switch_from(current, None, cx);
    }

    /// Navigates to an ancestor shown in the path bar.
    pub(crate) fn go_to(&mut self, path: RemotePath, cx: &mut Context<Self>) {
        let select = self
            .path
            .ancestors_from_root()
            .into_iter()
            .find(|ancestor| ancestor.parent().as_ref() == Some(&path))
            .and_then(|child| child.file_name().and_then(|name| EntryName::new(name).ok()));
        self.navigate(path, select, cx);
    }

    /// Opens the entry at visible `index`: directories are entered, symlinks
    /// are entered when they point to a directory.
    fn open_index(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.view.get(index) else {
            return;
        };
        let target = self.path.join(&entry.name);
        match entry.kind() {
            EntryKind::Directory => self.navigate(target, None, cx),
            EntryKind::Symlink => {
                let remote = Arc::clone(&self.remote);
                let probe = target.clone();
                let lookup = runtime::spawn(cx, async move { remote.stat(&probe).await });
                cx.spawn(async move |this, cx| {
                    let Ok(Ok(metadata)) = lookup.await else {
                        return;
                    };
                    if metadata.is_dir() {
                        let _ = this.update(cx, |browser, cx| browser.navigate(target, None, cx));
                    }
                })
                .detach();
            }
            EntryKind::File | EntryKind::Other => {}
        }
    }

    // ---------------------------------------------------------------------
    // Sorting and filtering

    pub(crate) fn sort_by(&mut self, column: SortColumn, cx: &mut Context<Self>) {
        self.sort = self.sort.toggled(column);
        self.apply_sort_to_view(cx);
        cx.notify();
    }

    /// Brings the view to `self.sort`: flips at once, full sorts in the background.
    fn apply_sort_to_view(&mut self, cx: &mut Context<Self>) {
        let Some(job) = self.view.begin_sort(self.sort) else {
            self.reselect();
            self.scroll_to_selection();
            return;
        };
        let generation = self.generation;
        self.sort_task = Some(cx.spawn(async move |this, cx| {
            let sorted = cx.background_spawn(async move { job.run() }).await;
            let _ = this.update(cx, |browser, cx| {
                if browser.generation == generation {
                    browser.view.finish_sort(sorted);
                    browser.reselect();
                    browser.scroll_to_selection();
                    cx.notify();
                }
            });
        }));
    }

    /// The type-to-filter text.
    pub(crate) fn filter(&self) -> &str {
        &self.filter
    }

    pub(crate) fn can_go_back(&self) -> bool {
        !self.back.is_empty()
    }

    pub(crate) fn can_go_forward(&self) -> bool {
        !self.forward.is_empty()
    }

    pub(crate) fn set_filter(&mut self, filter: String, cx: &mut Context<Self>) {
        self.filter = filter;
        // Marks on rows the filter hides would be copied unseen.
        self.marked.clear();
        self.view.set_filter(&self.filter);
        // Typing narrows the list; jump to the best (first) match.
        self.selected = (!self.view.is_empty()).then_some(0);
        self.selected_name = self
            .selected
            .and_then(|i| self.view.get(i))
            .map(|e| e.name.clone());
        self.scroll_to_selection();
        cx.notify();
    }

    // ---------------------------------------------------------------------
    // Selection

    /// Re-resolves the selected index from the selected name after the visible
    /// order changed.
    fn reselect(&mut self) {
        self.selected = match &self.selected_name {
            Some(name) => self.view.position(name.as_str()),
            None => None,
        };
        if self.selected.is_none() && !self.view.is_empty() {
            self.selected = Some(0);
        }
    }

    /// The entries shown, in their order.
    pub(crate) fn entries(&self) -> impl Iterator<Item = &Entry> {
        (0..self.view.len()).filter_map(|index| self.view.get(index))
    }

    /// Selects the entry `name` and scrolls it into view, clearing a filter
    /// that hides it.
    pub(crate) fn reveal(&mut self, name: &EntryName, cx: &mut Context<Self>) {
        if self.view.position(name.as_str()).is_none() && !self.filter.is_empty() {
            self.set_filter(String::new(), cx);
        }
        if let Some(index) = self.view.position(name.as_str()) {
            self.select(index, cx);
        }
    }

    pub(crate) fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        self.marked.clear();
        self.anchor = Some(index);
        self.move_cursor(index, cx);
    }

    /// Leaves no row selected, as after a click beside the rows.
    pub(crate) fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.marked.clear();
        self.anchor = None;
        self.selected = None;
        self.selected_name = None;
        cx.notify();
    }

    fn move_cursor(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.view.get(index) else {
            return;
        };
        self.selected = Some(index);
        self.selected_name = Some(entry.name.clone());
        self.notice = None;
        self.scroll_to_selection();
        cx.notify();
    }

    /// Marks the rows from the anchor to `index` (Shift).
    fn select_range(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.view.len() {
            return;
        }
        let anchor = self.anchor.or(self.selected).unwrap_or(index);
        let (from, to) = (anchor.min(index), anchor.max(index));
        self.marked = (from..=to)
            .filter_map(|row| self.view.get(row).map(|entry| entry.name.clone()))
            .collect();
        self.anchor = Some(anchor);
        self.move_cursor(index, cx);
    }

    /// Adds or removes one row from the selection (Ctrl/Cmd-click).
    fn toggle_mark(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(name) = self.view.get(index).map(|entry| entry.name.clone()) else {
            return;
        };
        if self.marked.is_empty()
            && let Some(current) = self.selected_name.clone()
        {
            self.marked.insert(current);
        }
        if !self.marked.remove(&name) {
            self.marked.insert(name);
        }
        self.anchor = Some(index);
        self.move_cursor(index, cx);
    }

    pub(crate) fn is_marked(&self, name: &EntryName) -> bool {
        self.marked.contains(name)
    }

    /// The selected items: the marked rows, or the cursor row.
    pub(crate) fn selected_paths(&self) -> Vec<RemotePath> {
        if self.marked.is_empty() {
            return self
                .selected
                .and_then(|index| self.view.get(index))
                .map(|entry| vec![self.path.join(&entry.name)])
                .unwrap_or_default();
        }
        let mut names: Vec<&EntryName> = self.marked.iter().collect();
        names.sort();
        names.into_iter().map(|name| self.path.join(name)).collect()
    }

    /// What dragging `grabbed` carries: the whole selection if the row is
    /// part of it, otherwise just that row.
    pub(crate) fn drag_items(&self, grabbed: &EntryName) -> Vec<RemotePath> {
        if self.marked.contains(grabbed) {
            self.selected_paths()
        } else {
            vec![self.path.join(grabbed)]
        }
    }

    /// For dragging local files out of the window into other applications.
    /// Remote files do not exist locally yet, so they stay inside Ferry.
    pub(crate) fn drag_out_paths(&self, grabbed: &EntryName) -> Option<gpui::FileDragPaths> {
        if !self.remote.is_local() {
            return None;
        }
        let entries = self.drag_items(grabbed).into_iter().map(|path| {
            let is_dir = path
                .file_name()
                .and_then(|name| self.view.position(name))
                .and_then(|row| self.view.get(row))
                .is_some_and(|entry| entry.kind() == EntryKind::Directory);
            (std::path::PathBuf::from(path.as_str()), is_dir)
        });
        Some(gpui::FileDragPaths::new(entries))
    }

    /// Number of rows a drag of `grabbed` would carry.
    pub(crate) fn drag_count(&self, grabbed: &EntryName) -> usize {
        if self.marked.contains(grabbed) {
            self.marked.len()
        } else {
            1
        }
    }

    /// Copies `items` from `source` into `target_dir` of this browser's remote.
    pub(crate) fn receive(
        &mut self,
        source: Arc<dyn Remote>,
        items: Vec<RemotePath>,
        target_dir: RemotePath,
        cx: &mut Context<Self>,
    ) {
        if items.is_empty() {
            return;
        }
        Transfers::start(
            TransferRequest {
                source,
                items,
                target: Arc::clone(&self.remote),
                target_dir,
            },
            cx,
        );
    }

    /// Copies what was dragged from another (or this) browser into `target_dir`.
    pub(crate) fn receive_drag(
        &mut self,
        dragged: &DraggedEntries,
        target_dir: RemotePath,
        cx: &mut Context<Self>,
    ) {
        let Some(source) = dragged.source.upgrade() else {
            return;
        };
        // Rows dropped onto a folder of their own browser come from `self`,
        // which is already borrowed for this update and cannot be read again.
        let source = if source.entity_id() == cx.entity_id() {
            &*self
        } else {
            source.read(cx)
        };
        let remote = Arc::clone(&source.remote);
        let items = source.drag_items(&dragged.grabbed);
        // Dropping rows onto their own folder would only duplicate them.
        if Arc::ptr_eq(&remote, &self.remote)
            && items
                .iter()
                .all(|item| item.parent().as_ref() == Some(&target_dir))
        {
            return;
        }
        self.receive(remote, items, target_dir, cx);
    }

    /// Uploads files dropped from another application.
    pub(crate) fn receive_external(
        &mut self,
        paths: &gpui::ExternalPaths,
        target_dir: RemotePath,
        cx: &mut Context<Self>,
    ) {
        let items: Vec<RemotePath> = paths
            .paths()
            .iter()
            .filter_map(|path| path.to_str().and_then(|path| RemotePath::parse(path).ok()))
            .collect();
        let source: Arc<dyn Remote> = Arc::new(ferry_core::remote::local::LocalRemote::new());
        self.receive(source, items, target_dir, cx);
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(target) = self.row_after(delta) {
            self.select(target, cx);
        }
    }

    fn extend_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(target) = self.row_after(delta) {
            self.select_range(target, cx);
        }
    }

    fn row_after(&self, delta: isize) -> Option<usize> {
        let len = self.view.len();
        if len == 0 {
            return None;
        }
        // Without a cursor, moving down starts at the top and up at the bottom.
        let Some(current) = self.selected else {
            return Some(if delta < 0 { len - 1 } else { 0 });
        };
        Some(current.saturating_add_signed(delta).min(len - 1))
    }

    /// Rows that fit into the list's viewport.
    fn page_rows(&self, window: &Window) -> isize {
        let height = self.scroll.0.borrow().base_handle.bounds().size.height;
        let row_height = crate::style::ROW_HEIGHT.to_pixels(window.rem_size());
        let rows = (height / row_height).floor();
        // Float to int: the value is a small, non-negative row count.
        #[expect(
            clippy::cast_possible_truncation,
            reason = "bounded by the window height"
        )]
        let rows = rows as isize;
        (rows - 1).max(1)
    }

    fn scroll_to_selection(&self) {
        if let Some(index) = self.selected {
            self.scroll.scroll_to_item(index, ScrollStrategy::Nearest);
        }
    }

    // ---------------------------------------------------------------------
    // Actions

    fn on_select_previous(
        &mut self,
        _: &actions::SelectPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_selection(-1, cx);
    }

    fn on_select_next(&mut self, _: &actions::SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(1, cx);
    }

    fn on_select_previous_extend(
        &mut self,
        _: &actions::SelectPreviousExtend,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.extend_selection(-1, cx);
    }

    fn on_select_next_extend(
        &mut self,
        _: &actions::SelectNextExtend,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.extend_selection(1, cx);
    }

    fn on_select_all(&mut self, _: &actions::SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.marked = (0..self.view.len())
            .filter_map(|row| self.view.get(row).map(|entry| entry.name.clone()))
            .collect();
        cx.notify();
    }

    fn on_copy(&mut self, _: &actions::Copy, _: &mut Window, cx: &mut Context<Self>) {
        let items = self.selected_paths();
        if items.is_empty() {
            return;
        }
        self.notice = Some(
            match items.len() {
                1 => "Copied 1 item. Paste it into another folder.".to_owned(),
                n => format!("Copied {n} items. Paste them into another folder."),
            }
            .into(),
        );
        cx.set_global(FileClipboard {
            source: Arc::clone(&self.remote),
            items,
        });
        cx.notify();
    }

    fn on_paste(&mut self, _: &actions::Paste, _: &mut Window, cx: &mut Context<Self>) {
        let Some(clipboard) = cx.try_global::<FileClipboard>() else {
            return;
        };
        let (source, items) = (Arc::clone(&clipboard.source), clipboard.items.clone());
        let target_dir = self.path.clone();
        self.receive(source, items, target_dir, cx);
    }

    fn on_page_up(
        &mut self,
        _: &actions::SelectPageUp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_selection(-self.page_rows(window), cx);
    }

    fn on_page_down(
        &mut self,
        _: &actions::SelectPageDown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_selection(self.page_rows(window), cx);
    }

    fn on_select_first(
        &mut self,
        _: &actions::SelectFirst,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select(0, cx);
    }

    fn on_select_last(&mut self, _: &actions::SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(last) = self.view.len().checked_sub(1) {
            self.select(last, cx);
        }
    }

    fn on_open(&mut self, _: &actions::Open, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.selected {
            self.open_index(index, cx);
        }
    }

    fn on_go_to_parent(&mut self, _: &actions::GoToParent, _: &mut Window, cx: &mut Context<Self>) {
        self.go_to_parent(cx);
    }

    fn on_go_back(&mut self, _: &actions::GoBack, _: &mut Window, cx: &mut Context<Self>) {
        self.go_back(cx);
    }

    fn on_go_forward(&mut self, _: &actions::GoForward, _: &mut Window, cx: &mut Context<Self>) {
        self.go_forward(cx);
    }

    fn on_backspace(&mut self, _: &actions::Backspace, _: &mut Window, cx: &mut Context<Self>) {
        if self.filter.is_empty() {
            self.go_to_parent(cx);
        } else {
            let mut filter = self.filter.clone();
            filter.pop();
            self.set_filter(filter, cx);
        }
    }

    fn on_clear_filter(
        &mut self,
        _: &actions::ClearFilter,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.filter.is_empty() {
            // Nothing to clear here; Escape may close something around.
            cx.propagate();
            return;
        }
        {
            // Keep the entry that was found selected and in view.
            let found = self.selected_name.clone();
            self.set_filter(String::new(), cx);
            if found.is_some() {
                self.selected_name = found;
            }
            self.reselect();
            self.scroll_to_selection();
        }
    }

    fn on_sort_by_name(&mut self, _: &actions::SortByName, _: &mut Window, cx: &mut Context<Self>) {
        self.sort_by(SortColumn::Name, cx);
    }

    fn on_sort_by_size(&mut self, _: &actions::SortBySize, _: &mut Window, cx: &mut Context<Self>) {
        self.sort_by(SortColumn::Size, cx);
    }

    fn on_sort_by_modified(
        &mut self,
        _: &actions::SortByModified,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sort_by(SortColumn::Modified, cx);
    }

    fn on_refresh(&mut self, _: &actions::Refresh, _: &mut Window, cx: &mut Context<Self>) {
        self.reload(cx);
    }

    /// Lists the current folder again, keeping what is shown until the new
    /// listing arrives.
    pub(crate) fn reload(&mut self, cx: &mut Context<Self>) {
        self.state = if self.view.total_len() > 0 {
            LoadState::Revalidating
        } else {
            LoadState::Loading
        };
        self.load(cx);
        cx.notify();
    }

    /// Type-to-filter: printable characters that no binding claimed extend the
    /// filter.
    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.renaming.is_some() {
            return;
        }
        let modifiers = event.keystroke.modifiers;
        if modifiers.control || modifiers.alt || modifiers.platform || modifiers.function {
            return;
        }
        let Some(text) = event.keystroke.key_char.as_deref() else {
            return;
        };
        if text.is_empty() || text.chars().any(char::is_control) {
            return;
        }
        // Space previews; while a filter is typed it is part of the filter,
        // as names contain spaces.
        if text == " " && self.filter.is_empty() {
            window.dispatch_action(Box::new(actions::Preview), cx);
            cx.stop_propagation();
            return;
        }
        let mut filter = self.filter.clone();
        filter.push_str(text);
        self.set_filter(filter, cx);
        cx.stop_propagation();
    }
}

impl Focusable for FileBrowser {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}
