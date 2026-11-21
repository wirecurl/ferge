//! Sorting and filtering of a directory listing on indices.
//!
//! [`DirectoryView`] owns the entries of one directory and exposes them through
//! a permutation (`order`) and a filtered subsequence of it (`visible`). Entries
//! are never copied or moved for sorting or filtering; only `u32` indices are.
//! Per-entry sort and filter keys are computed once when an entry is added.
//!
//! Entries live in fixed-size, reference-counted segments, so a snapshot for a
//! background [`SortJob`] costs one reference count per 4096 entries. Cheap
//! operations (adding entries, flipping the direction, filtering) are
//! synchronous; a full re-sort runs as a job and is merged back with
//! [`DirectoryView::finish_sort`], including entries added in the meantime.

use std::cmp::Ordering;
use std::sync::Arc;

use crate::entry::{Entry, EntryKind};

/// Column to sort by.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SortColumn {
    #[default]
    Name,
    Size,
    Modified,
}

/// How a listing is ordered. Directories always come first.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SortOrder {
    pub column: SortColumn,
    pub descending: bool,
}

impl SortOrder {
    /// The order after the user picked `column`: the same column flips the
    /// direction, a new column starts ascending for names and descending for
    /// sizes and dates (largest and newest first).
    #[must_use]
    pub fn toggled(self, column: SortColumn) -> Self {
        if self.column == column {
            Self {
                column,
                descending: !self.descending,
            }
        } else {
            Self {
                column,
                descending: column != SortColumn::Name,
            }
        }
    }
}

/// Precomputed keys of one entry.
#[derive(Clone, Debug)]
struct Keys {
    /// Lower-cased name, matched by the filter.
    folded: Box<str>,
    /// Byte string whose plain ordering is a natural, case-insensitive name order.
    natural: Box<[u8]>,
    is_dir: bool,
}

impl Keys {
    fn new(entry: &Entry) -> Self {
        let folded: Box<str> = entry.name.as_str().to_lowercase().into_boxed_str();
        let natural = natural_key(&folded);
        Self {
            folded,
            natural,
            is_dir: entry.kind() == EntryKind::Directory,
        }
    }
}

/// Encodes `text` so that byte-wise comparison sorts digit runs by numeric
/// value: every run of ASCII digits loses its leading zeros and is prefixed with
/// its length, so longer numbers sort after shorter ones and equal lengths
/// compare digit by digit. Lengths below `LONG_RUN` take one byte, longer
/// runs are marked with `LONG_RUN` followed by a 4-byte big-endian length. All
/// length bytes are below `0x20`, so numbers sort before letters and
/// punctuation.
fn natural_key(text: &str) -> Box<[u8]> {
    const LONG_RUN: u8 = 0x1f;
    let bytes = text.as_bytes();
    let mut key = Vec::with_capacity(bytes.len() + 4);
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            key.push(bytes[i]);
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        let mut digits = &bytes[start..i];
        while digits.len() > 1 && digits[0] == b'0' {
            digits = &digits[1..];
        }
        match u8::try_from(digits.len()) {
            Ok(len) if len < LONG_RUN => key.push(len),
            _ => {
                key.push(LONG_RUN);
                let len = u32::try_from(digits.len()).unwrap_or(u32::MAX);
                key.extend_from_slice(&len.to_be_bytes());
            }
        }
        key.extend_from_slice(digits);
    }
    key.into_boxed_slice()
}

/// An entry together with its keys.
#[derive(Clone, Debug)]
struct Item {
    entry: Entry,
    keys: Keys,
}

/// Entries per segment of the [`Store`].
const SEGMENT: usize = 4096;

/// Upper bound of entries per view, so every index fits into a `u32`.
const MAX_ENTRIES: usize = u32::MAX as usize;

/// Converts a length that is known to be at most [`MAX_ENTRIES`].
fn index(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// Append-only item storage in shared, fixed-size segments. Cloning shares all
/// segments; appending copies at most the last, partially filled one, and only
/// if a clone still holds it.
#[derive(Clone, Debug, Default)]
struct Store {
    segments: Vec<Arc<Vec<Item>>>,
    len: usize,
}

impl Store {
    fn get(&self, index: u32) -> &Item {
        let index = index as usize;
        &self.segments[index / SEGMENT][index % SEGMENT]
    }

    fn push(&mut self, item: Item) {
        match self.segments.last_mut() {
            Some(last) if last.len() < SEGMENT => Arc::make_mut(last).push(item),
            _ => {
                let mut segment = Vec::with_capacity(SEGMENT);
                segment.push(item);
                self.segments.push(Arc::new(segment));
            }
        }
        self.len += 1;
    }

    /// Total order: directories first, then the sort column, then the natural
    /// name, the lower-cased name, the raw name and finally the index (so equal
    /// names never tie). Descending is the exact reverse of ascending within
    /// the directory and file groups, which keeps a direction flip a linear
    /// reversal that agrees with this comparator.
    fn compare(&self, sort: SortOrder, a: u32, b: u32) -> Ordering {
        let (ia, ib) = (self.get(a), self.get(b));
        let dirs_first = ib.keys.is_dir.cmp(&ia.keys.is_dir);
        if dirs_first != Ordering::Equal {
            return dirs_first;
        }
        let by_name = || {
            ia.keys
                .natural
                .cmp(&ib.keys.natural)
                .then_with(|| ia.keys.folded.cmp(&ib.keys.folded))
                .then_with(|| ia.entry.name.as_str().cmp(ib.entry.name.as_str()))
        };
        let (ma, mb) = (&ia.entry.metadata, &ib.entry.metadata);
        let primary = match sort.column {
            SortColumn::Name => by_name(),
            SortColumn::Size => ma.size.cmp(&mb.size),
            // Unknown times sort as the oldest.
            SortColumn::Modified => ma.modified.cmp(&mb.modified),
        };
        let ascending = primary.then_with(by_name).then(a.cmp(&b));
        if sort.descending {
            ascending.reverse()
        } else {
            ascending
        }
    }

    /// Indices `range` sorted by `sort`.
    fn sorted(&self, sort: SortOrder, range: std::ops::Range<u32>) -> Vec<u32> {
        let mut order: Vec<u32> = range.collect();
        order.sort_unstable_by(|&a, &b| self.compare(sort, a, b));
        order
    }

    /// Merges two sequences that are each sorted by `sort`.
    fn merge(&self, sort: SortOrder, left: &[u32], right: &[u32]) -> Vec<u32> {
        let mut merged = Vec::with_capacity(left.len() + right.len());
        let (mut l, mut r) = (0, 0);
        while l < left.len() && r < right.len() {
            if self.compare(sort, left[l], right[r]) == Ordering::Greater {
                merged.push(right[r]);
                r += 1;
            } else {
                merged.push(left[l]);
                l += 1;
            }
        }
        merged.extend_from_slice(&left[l..]);
        merged.extend_from_slice(&right[r..]);
        merged
    }
}

/// A full re-sort, detached from the view so it can run on another thread.
#[derive(Debug)]
#[must_use = "a sort job does nothing until it is run and finished"]
pub struct SortJob {
    store: Store,
    sort: SortOrder,
}

impl SortJob {
    /// Sorts the snapshot. CPU-bound; run it off the UI thread.
    pub fn run(self) -> SortedOrder {
        let order = self.store.sorted(self.sort, 0..index(self.store.len));
        SortedOrder {
            sort: self.sort,
            order,
        }
    }
}

/// The result of a [`SortJob`], to be passed to [`DirectoryView::finish_sort`].
#[derive(Debug)]
pub struct SortedOrder {
    sort: SortOrder,
    order: Vec<u32>,
}

/// A cheap, immutable copy of a view's entries in the order they were added.
#[derive(Clone, Debug)]
pub struct EntriesSnapshot {
    store: Store,
}

impl EntriesSnapshot {
    /// Whether the snapshot holds exactly `entries`, in the same order. Linear;
    /// run it off the UI thread for large listings.
    pub fn matches(&self, entries: &[Entry]) -> bool {
        self.store.len == entries.len()
            && self
                .store
                .segments
                .iter()
                .flat_map(|segment| segment.iter())
                .zip(entries)
                .all(|(item, entry)| item.entry == *entry)
    }
}

/// A directory's entries with a current sort order and filter.
#[derive(Debug, Default)]
pub struct DirectoryView {
    store: Store,
    /// All entry indices in `sort` order.
    order: Vec<u32>,
    /// The subsequence of `order` that passes the filter.
    visible: Vec<u32>,
    sort: SortOrder,
    /// A sort requested through [`Self::begin_sort`] that has not finished yet.
    pending_sort: Option<SortOrder>,
    /// Lower-cased filter text; empty means no filter.
    filter: String,
    /// Whether names starting with a dot are left out.
    hide_hidden: bool,
}

impl DirectoryView {
    pub fn new(sort: SortOrder) -> Self {
        Self {
            sort,
            ..Self::default()
        }
    }

    /// Builds a view from complete contents. Sorts synchronously; for large
    /// listings call it off the UI thread.
    pub fn with_entries(sort: SortOrder, filter: &str, entries: Vec<Entry>) -> Self {
        let mut view = Self::new(sort);
        view.filter = filter.to_lowercase();
        view.extend(entries);
        view
    }

    /// Number of entries that pass the filter.
    pub fn len(&self) -> usize {
        self.visible.len()
    }

    pub fn is_empty(&self) -> bool {
        self.visible.is_empty()
    }

    /// Number of entries regardless of the filter.
    pub fn total_len(&self) -> usize {
        self.store.len
    }

    /// The `index`-th visible entry.
    pub fn get(&self, index: usize) -> Option<&Entry> {
        let entry = *self.visible.get(index)?;
        Some(&self.store.get(entry).entry)
    }

    /// Visible position of the entry called `name`.
    pub fn position(&self, name: &str) -> Option<usize> {
        self.visible
            .iter()
            .position(|&index| self.store.get(index).entry.name.as_str() == name)
    }

    /// A snapshot of the entries for comparisons on another thread.
    pub fn snapshot(&self) -> EntriesSnapshot {
        EntriesSnapshot {
            store: self.store.clone(),
        }
    }

    /// The order the view is sorted in, or will be once a pending sort finishes.
    pub fn sort_order(&self) -> SortOrder {
        self.pending_sort.unwrap_or(self.sort)
    }

    /// Whether a [`SortJob`] has been started and not finished.
    pub fn is_sorting(&self) -> bool {
        self.pending_sort.is_some()
    }

    /// The active filter (lower-cased).
    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// Adds entries, keeping the order: the new entries are sorted on their own
    /// and merged into the existing order in linear time.
    ///
    /// Indices are `u32`; entries beyond `u32::MAX` are dropped with an error log.
    pub fn extend(&mut self, mut entries: Vec<Entry>) {
        let room = MAX_ENTRIES - self.store.len;
        if entries.len() > room {
            tracing::error!(
                dropped = entries.len() - room,
                "directory exceeds the entry limit"
            );
            entries.truncate(room);
        }
        if entries.is_empty() {
            return;
        }
        let first = index(self.store.len);
        for entry in entries {
            let keys = Keys::new(&entry);
            self.store.push(Item { entry, keys });
        }
        let mut added = self.store.sorted(self.sort, first..index(self.store.len));
        self.order = self.store.merge(self.sort, &self.order, &added);
        if self.is_filtering() {
            added.retain(|&index| self.matches(index));
            self.visible = self.store.merge(self.sort, &self.visible, &added);
        } else {
            self.visible.clone_from(&self.order);
        }
    }

    /// Requests a new sort order. Flipping the direction of the current column
    /// is applied at once (linear) and returns `None`; any other change returns
    /// a [`SortJob`] whose result must be passed to [`Self::finish_sort`]. The
    /// view keeps its current order until then.
    pub fn begin_sort(&mut self, sort: SortOrder) -> Option<SortJob> {
        if sort == self.sort_order() {
            return None;
        }
        if self.pending_sort.is_none() && sort.column == self.sort.column {
            self.sort = sort;
            // Directories stay first; each group is reversed in place.
            let dirs = self
                .order
                .partition_point(|&index| self.store.get(index).keys.is_dir);
            self.order[..dirs].reverse();
            self.order[dirs..].reverse();
            self.refilter();
            return None;
        }
        self.pending_sort = Some(sort);
        Some(SortJob {
            store: self.store.clone(),
            sort,
        })
    }

    /// Applies a finished [`SortJob`]. Results of superseded jobs are ignored.
    /// Entries added while the job ran are merged in.
    pub fn finish_sort(&mut self, sorted: SortedOrder) {
        if self.pending_sort != Some(sorted.sort) {
            return;
        }
        let covered = index(sorted.order.len());
        let added = self
            .store
            .sorted(sorted.sort, covered..index(self.store.len));
        self.order = if added.is_empty() {
            sorted.order
        } else {
            self.store.merge(sorted.sort, &sorted.order, &added)
        };
        self.sort = sorted.sort;
        self.pending_sort = None;
        self.refilter();
    }

    /// Synchronous [`Self::begin_sort`] + [`SortJob::run`] + [`Self::finish_sort`].
    pub fn set_sort(&mut self, sort: SortOrder) {
        if let Some(job) = self.begin_sort(sort) {
            self.finish_sort(job.run());
        }
    }

    /// Sets the filter (case-insensitive substring match on names).
    pub fn set_filter(&mut self, filter: &str) {
        let folded = filter.to_lowercase();
        if folded == self.filter {
            return;
        }
        let narrowing =
            (!self.filter.is_empty() || self.hide_hidden) && folded.contains(self.filter.as_str());
        self.filter = folded;
        if narrowing {
            // Every match of the longer filter also matched the shorter one.
            let mut visible = std::mem::take(&mut self.visible);
            visible.retain(|&index| self.matches(index));
            self.visible = visible;
        } else {
            self.refilter();
        }
    }

    /// Shows or hides entries whose names start with a dot.
    pub fn set_show_hidden(&mut self, show: bool) {
        if self.hide_hidden != show {
            return;
        }
        self.hide_hidden = !show;
        if self.hide_hidden {
            let mut visible = std::mem::take(&mut self.visible);
            visible.retain(|&index| self.matches(index));
            self.visible = visible;
        } else {
            self.refilter();
        }
    }

    /// Whether entries whose names start with a dot are shown.
    pub fn shows_hidden(&self) -> bool {
        !self.hide_hidden
    }

    fn is_filtering(&self) -> bool {
        !self.filter.is_empty() || self.hide_hidden
    }

    fn refilter(&mut self) {
        if self.is_filtering() {
            self.visible = self
                .order
                .iter()
                .copied()
                .filter(|&index| self.matches(index))
                .collect();
        } else {
            self.visible.clone_from(&self.order);
        }
    }

    fn matches(&self, index: u32) -> bool {
        let folded = &self.store.get(index).keys.folded;
        !(self.hide_hidden && folded.starts_with('.')) && folded.contains(self.filter.as_str())
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::entry::Metadata;
    use crate::path::EntryName;

    fn entry(name: &str, kind: EntryKind, size: u64, mtime: u64) -> Entry {
        Entry {
            name: EntryName::new(name).expect("valid"),
            metadata: Metadata {
                kind,
                size,
                modified: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(mtime)),
                permissions: None,
            },
        }
    }

    fn file(name: &str) -> Entry {
        entry(name, EntryKind::File, 0, 0)
    }

    fn names(view: &DirectoryView) -> Vec<&str> {
        (0..view.len())
            .map(|i| view.get(i).expect("in range").name.as_str())
            .collect()
    }

    #[test]
    fn natural_case_insensitive_name_order_with_dirs_first() {
        let mut view = DirectoryView::new(SortOrder::default());
        view.extend(vec![
            file("file10.txt"),
            file("File2.txt"),
            entry("zeta", EntryKind::Directory, 0, 0),
            file("file1.txt"),
            file("file02.txt"),
            entry("Alpha", EntryKind::Directory, 0, 0),
            file("a.txt"),
        ]);
        assert_eq!(
            names(&view),
            [
                "Alpha",
                "zeta",
                "a.txt",
                "file1.txt",
                "file02.txt",
                "File2.txt",
                "file10.txt"
            ]
        );
    }

    #[test]
    fn long_digit_runs_stay_ordered() {
        let small = natural_key("x9");
        let big = natural_key(&format!("x{}", "9".repeat(40)));
        let bigger = natural_key(&format!("x1{}", "0".repeat(40)));
        assert!(small < big && big < bigger);
    }

    #[test]
    fn incremental_extend_equals_full_sort() {
        let all: Vec<Entry> = (0..500_u64)
            .map(|i| {
                entry(
                    &format!("n{}", (i * 7919) % 1000),
                    EntryKind::File,
                    i % 13,
                    i % 17,
                )
            })
            .collect();
        for sort in [
            SortOrder::default(),
            SortOrder {
                column: SortColumn::Size,
                descending: true,
            },
            SortOrder {
                column: SortColumn::Modified,
                descending: false,
            },
        ] {
            let whole = DirectoryView::with_entries(sort, "", all.clone());
            let mut chunked = DirectoryView::new(sort);
            for chunk in all.chunks(37) {
                chunked.extend(chunk.to_vec());
            }
            assert_eq!(names(&whole), names(&chunked));

            // A direction flip (linear reversal) must agree with a fresh sort, and
            // later merges must keep agreeing.
            let flipped = SortOrder {
                descending: !sort.descending,
                ..sort
            };
            chunked.set_sort(flipped);
            let mut fresh = DirectoryView::with_entries(flipped, "", all.clone());
            assert_eq!(names(&fresh), names(&chunked));
            let more = vec![
                entry("n500", EntryKind::File, 5, 5),
                entry("a", EntryKind::Directory, 0, 0),
            ];
            chunked.extend(more.clone());
            fresh.extend(more);
            assert_eq!(names(&fresh), names(&chunked));
        }
    }

    #[test]
    fn background_sort_merges_entries_added_meanwhile() {
        let first: Vec<Entry> = (0..9000_u64)
            .map(|i| entry(&format!("f{i}"), EntryKind::File, (i * 7) % 101, 0))
            .collect();
        let later: Vec<Entry> = (9000..9500_u64)
            .map(|i| entry(&format!("f{i}"), EntryKind::File, (i * 7) % 101, 0))
            .collect();
        let by_size = SortOrder {
            column: SortColumn::Size,
            descending: true,
        };

        let mut view = DirectoryView::with_entries(SortOrder::default(), "f1", first.clone());
        let job = view.begin_sort(by_size).expect("column change needs a job");
        assert!(view.is_sorting());
        assert_eq!(view.sort_order(), by_size);
        // The snapshot is unaffected by entries added while the job runs.
        let handle = std::thread::spawn(move || job.run());
        view.extend(later.clone());
        view.finish_sort(handle.join().expect("job"));
        assert!(!view.is_sorting());

        let mut all = first;
        all.extend(later);
        let expected = DirectoryView::with_entries(by_size, "f1", all);
        assert_eq!(names(&view), names(&expected));
    }

    #[test]
    fn snapshots_compare_insertion_order() {
        let entries: Vec<Entry> = (0..5000).map(|i| file(&format!("f{i}"))).collect();
        let view = DirectoryView::with_entries(SortOrder::default(), "", entries.clone());
        let snapshot = view.snapshot();
        assert!(snapshot.matches(&entries));
        let mut changed = entries.clone();
        changed[4321] = entry("f4321", EntryKind::File, 1, 0);
        assert!(!snapshot.matches(&changed));
        assert!(!snapshot.matches(&entries[..4999]));
    }

    #[test]
    fn superseded_sort_results_are_ignored() {
        let mut view = DirectoryView::with_entries(
            SortOrder::default(),
            "",
            vec![
                entry("a", EntryKind::File, 1, 9),
                entry("b", EntryKind::File, 2, 8),
            ],
        );
        let by_size = SortOrder::default().toggled(SortColumn::Size);
        let by_date = SortOrder::default().toggled(SortColumn::Modified);
        let stale = view.begin_sort(by_size).expect("job").run();
        let current = view.begin_sort(by_date).expect("job").run();
        view.finish_sort(stale);
        assert!(
            view.is_sorting(),
            "stale result must not finish the newer request"
        );
        view.finish_sort(current);
        assert_eq!(view.sort_order(), by_date);
        assert_eq!(names(&view), ["a", "b"]);
        assert!(view.begin_sort(by_date).is_none(), "no-op");
    }

    #[test]
    fn sort_columns_and_direction() {
        let mut view = DirectoryView::new(SortOrder::default());
        view.extend(vec![
            entry("b", EntryKind::File, 30, 1),
            entry("a", EntryKind::File, 10, 3),
            entry("c", EntryKind::File, 20, 2),
            entry("dir", EntryKind::Directory, 0, 0),
        ]);
        let order = SortOrder::default().toggled(SortColumn::Size);
        assert!(order.descending);
        view.set_sort(order);
        assert_eq!(names(&view), ["dir", "b", "c", "a"]);
        view.set_sort(order.toggled(SortColumn::Size));
        assert_eq!(names(&view), ["dir", "a", "c", "b"]);
        view.set_sort(SortOrder {
            column: SortColumn::Modified,
            descending: false,
        });
        assert_eq!(names(&view), ["dir", "b", "c", "a"]);
    }

    #[test]
    fn filter_narrows_widens_and_survives_extend() {
        let mut view = DirectoryView::new(SortOrder::default());
        view.extend(vec![
            file("Report.pdf"),
            file("photo.jpg"),
            file("report-old.pdf"),
        ]);
        view.set_filter("REP");
        assert_eq!(names(&view), ["report-old.pdf", "Report.pdf"]);
        view.set_filter("repo");
        assert_eq!(names(&view), ["report-old.pdf", "Report.pdf"]);
        view.set_filter("report-");
        assert_eq!(names(&view), ["report-old.pdf"]);
        view.set_filter("p");
        assert_eq!(view.len(), 3);
        view.set_filter("report");
        view.extend(vec![file("another report"), file("nope")]);
        assert_eq!(
            names(&view),
            ["another report", "report-old.pdf", "Report.pdf"]
        );
        assert_eq!(view.total_len(), 5);
        assert_eq!(view.position("Report.pdf"), Some(2));
        view.set_filter("");
        assert_eq!(view.len(), 5);
    }

    #[test]
    fn hides_dot_files_together_with_the_filter() {
        let mut view = DirectoryView::new(SortOrder::default());
        view.extend(vec![file(".env"), file("env.txt"), file(".bashrc")]);
        view.set_show_hidden(false);
        assert_eq!(names(&view), ["env.txt"]);
        view.set_filter("env");
        assert_eq!(names(&view), ["env.txt"]);
        view.extend(vec![file(".env.local"), file("environment")]);
        assert_eq!(names(&view), ["env.txt", "environment"]);
        view.set_show_hidden(true);
        assert_eq!(
            names(&view),
            [".env", ".env.local", "env.txt", "environment"]
        );
        view.set_filter("");
        view.set_show_hidden(false);
        assert_eq!(view.len(), 2);
        assert_eq!(view.total_len(), 5);
        assert!(!view.shows_hidden());
    }

    #[test]
    fn unicode_names_are_folded() {
        let mut view = DirectoryView::new(SortOrder::default());
        view.extend(vec![file("Ärger.txt"), file("über.txt")]);
        view.set_filter("ÄR");
        assert_eq!(names(&view), ["Ärger.txt"]);
        view.set_filter("Ü");
        assert_eq!(names(&view), ["über.txt"]);
    }
}
