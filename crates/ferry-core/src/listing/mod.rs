//! Directory listings for browsing: a per-connection cache, a streaming
//! browse operation with background revalidation, and the sorted/filtered view.

mod view;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use futures::StreamExt as _;
use futures::stream::BoxStream;

pub use self::view::{DirectoryView, EntriesSnapshot, SortColumn, SortJob, SortOrder, SortedOrder};
use crate::entry::Entry;
use crate::error::Error;
use crate::path::RemotePath;
use crate::remote::{ListStream, Remote};

/// What a [`browse`] stream reports.
#[derive(Debug)]
pub enum ListingEvent {
    /// The listing from the cache, sent immediately. A fresh listing is being
    /// fetched in the background and arrives as [`ListingEvent::Complete`].
    Cached(Arc<[Entry]>),
    /// Entries as they arrive from the remote. Only sent when nothing was
    /// cached, so the first rows can be shown before the listing is complete.
    Chunk(Vec<Entry>),
    /// The fresh, complete listing. It has been stored in the cache.
    Complete(Arc<[Entry]>),
    /// Listing failed. A cached listing sent before may be stale but stays valid.
    Failed(Error),
}

/// Recently listed directories of one connection, bounded by the total number
/// of entries and evicted least-recently-used first.
#[derive(Debug)]
pub struct ListingCache {
    max_entries: usize,
    state: Mutex<CacheState>,
}

#[derive(Debug, Default)]
struct CacheState {
    listings: HashMap<RemotePath, CachedListing>,
    total_entries: usize,
    clock: u64,
}

#[derive(Debug)]
struct CachedListing {
    entries: Arc<[Entry]>,
    last_used: u64,
}

impl ListingCache {
    /// Default bound: about two million entries (a few hundred MB at most).
    pub const DEFAULT_MAX_ENTRIES: usize = 2_000_000;

    pub fn new(max_entries: usize) -> Self {
        Self {
            max_entries,
            state: Mutex::default(),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, CacheState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The cached listing of `path`, marking it as recently used.
    pub fn get(&self, path: &RemotePath) -> Option<Arc<[Entry]>> {
        let mut state = self.state();
        state.clock += 1;
        let now = state.clock;
        let listing = state.listings.get_mut(path)?;
        listing.last_used = now;
        Some(Arc::clone(&listing.entries))
    }

    /// Stores a listing, evicting old ones to stay within the bound. A single
    /// listing larger than the bound is still kept, alone.
    pub fn insert(&self, path: RemotePath, entries: Arc<[Entry]>) {
        let mut state = self.state();
        state.clock += 1;
        let now = state.clock;
        let replaced = state.listings.get(&path).map_or(0, |old| old.entries.len());
        state.total_entries = state.total_entries - replaced + entries.len();
        while state.total_entries > self.max_entries {
            let Some(oldest) = state
                .listings
                .iter()
                .filter(|(key, _)| **key != path)
                .min_by_key(|(_, listing)| listing.last_used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some(evicted) = state.listings.remove(&oldest) {
                state.total_entries -= evicted.entries.len();
            }
        }
        state.listings.insert(
            path,
            CachedListing {
                entries,
                last_used: now,
            },
        );
    }

    /// Forgets `path`, e.g. after the directory was modified.
    pub fn invalidate(&self, path: &RemotePath) {
        let mut state = self.state();
        if let Some(old) = state.listings.remove(path) {
            state.total_entries -= old.entries.len();
        }
    }
}

impl Default for ListingCache {
    fn default() -> Self {
        Self::new(Self::DEFAULT_MAX_ENTRIES)
    }
}

/// Lists `path`, answering from the cache first and revalidating in the
/// background. Must be called within a tokio runtime; the returned stream can
/// be polled anywhere and ends after [`ListingEvent::Complete`] or
/// [`ListingEvent::Failed`]. Dropping it cancels the listing.
pub fn browse(
    remote: &Arc<dyn Remote>,
    cache: &Arc<ListingCache>,
    path: &RemotePath,
) -> BoxStream<'static, ListingEvent> {
    let cached = cache.get(path);
    let state = Fetch {
        list: remote.list(path),
        collected: Vec::new(),
        streaming: cached.is_none(),
        cache: Arc::clone(cache),
        path: path.clone(),
    };
    let fresh = futures::stream::unfold(Some(state), |state| async move {
        let mut state = state?;
        let event = state.next_event().await;
        let finished = matches!(event, ListingEvent::Complete(_) | ListingEvent::Failed(_));
        Some((event, (!finished).then_some(state)))
    });
    // Fused, so polling after the end is well-defined.
    futures::stream::iter(cached.map(ListingEvent::Cached))
        .chain(fresh)
        .fuse()
        .boxed()
}

struct Fetch {
    list: ListStream,
    collected: Vec<Entry>,
    streaming: bool,
    cache: Arc<ListingCache>,
    path: RemotePath,
}

impl Fetch {
    async fn next_event(&mut self) -> ListingEvent {
        loop {
            match self.list.next().await {
                Some(Ok(chunk)) if self.streaming => {
                    // Entries are cheap to clone (reference-counted names).
                    self.collected.extend_from_slice(&chunk);
                    return ListingEvent::Chunk(chunk);
                }
                Some(Ok(chunk)) => self.collected.extend(chunk),
                Some(Err(error)) => return ListingEvent::Failed(error),
                None => {
                    let entries: Arc<[Entry]> = std::mem::take(&mut self.collected).into();
                    self.cache.insert(self.path.clone(), Arc::clone(&entries));
                    return ListingEvent::Complete(entries);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{EntryKind, Metadata};
    use crate::path::EntryName;
    use crate::remote::local::LocalRemote;

    fn entries(n: usize) -> Arc<[Entry]> {
        (0..n)
            .map(|i| Entry {
                name: EntryName::new(&format!("e{i}")).expect("valid"),
                metadata: Metadata {
                    kind: EntryKind::File,
                    size: 0,
                    modified: None,
                    permissions: None,
                },
            })
            .collect()
    }

    fn path(s: &str) -> RemotePath {
        RemotePath::parse(s).expect("valid")
    }

    #[test]
    fn cache_evicts_least_recently_used() {
        let cache = ListingCache::new(10);
        cache.insert(path("/a"), entries(4));
        cache.insert(path("/b"), entries(4));
        assert!(cache.get(&path("/a")).is_some());
        cache.insert(path("/c"), entries(4));
        assert!(
            cache.get(&path("/b")).is_none(),
            "b was least recently used"
        );
        assert!(cache.get(&path("/a")).is_some());
        assert!(cache.get(&path("/c")).is_some());

        cache.insert(path("/huge"), entries(50));
        assert!(
            cache.get(&path("/huge")).is_some(),
            "an oversized listing is kept alone"
        );
        assert!(cache.get(&path("/a")).is_none());

        cache.invalidate(&path("/huge"));
        assert!(cache.get(&path("/huge")).is_none());
        assert_eq!(cache.state().total_entries, 0);
    }

    #[tokio::test]
    async fn browse_streams_first_then_serves_from_cache() {
        let dir = tempfile::tempdir().expect("tempdir");
        for i in 0..3000 {
            std::fs::write(dir.path().join(format!("f{i}")), b"").expect("write");
        }
        let remote: Arc<dyn Remote> = Arc::new(LocalRemote::new());
        let cache = Arc::new(ListingCache::default());
        let dir_path = path(dir.path().to_str().expect("utf-8"));

        let first: Vec<ListingEvent> = browse(&remote, &cache, &dir_path).collect().await;
        let chunks = first
            .iter()
            .filter(|e| matches!(e, ListingEvent::Chunk(_)))
            .count();
        assert!(chunks >= 2, "uncached listings stream in chunks");
        assert!(matches!(first.last(), Some(ListingEvent::Complete(all)) if all.len() == 3000));

        std::fs::write(dir.path().join("new"), b"").expect("write");
        let second: Vec<ListingEvent> = browse(&remote, &cache, &dir_path).collect().await;
        assert_eq!(
            second.len(),
            2,
            "cached snapshot, then the revalidated listing"
        );
        assert!(matches!(&second[0], ListingEvent::Cached(all) if all.len() == 3000));
        assert!(matches!(&second[1], ListingEvent::Complete(all) if all.len() == 3001));
        assert_eq!(cache.get(&dir_path).map(|all| all.len()), Some(3001));
    }

    #[tokio::test]
    async fn browse_reports_failures() {
        let remote: Arc<dyn Remote> = Arc::new(LocalRemote::new());
        let cache = Arc::new(ListingCache::default());
        let events: Vec<ListingEvent> = browse(&remote, &cache, &path("/definitely/missing/dir"))
            .collect()
            .await;
        assert!(matches!(
            events.as_slice(),
            [ListingEvent::Failed(Error::NotFound { .. })]
        ));
    }
}
