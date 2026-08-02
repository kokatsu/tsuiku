//! Filesystem watching: event classification and debounced aggregation.
//!
//! The backend watcher (added with the watch runtime) translates raw
//! filesystem notifications into [`WatchEvent`] values; everything downstream
//! works only with that classified form. Events are never dropped inside a
//! debounce window — they are *aggregated*: paths union, category flags OR,
//! and only the timer moves. Keeping the last event alone would lose the
//! earlier ones (e.g. "selected file changed, then another file changed"
//! within one window must still reload the selected file).

pub mod debounce;

use std::collections::HashSet;

use crate::path::GitPath;

/// One classified filesystem observation.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum WatchEvent {
    /// A path inside the worktree changed (created, modified, removed, or
    /// one side of a rename). The path is worktree-relative and non-empty:
    /// an event on the worktree root itself must be classified as
    /// [`WatchEvent::Unknown`] (it may affect everything). Should an empty
    /// path slip through anyway, it counts as an ancestor of every path, so
    /// the conservative side still wins.
    Worktree { path: GitPath },
    /// Git metadata changed: HEAD, index, a watched ref, packed-refs, or
    /// config. Which paths are affected cannot be derived from the event.
    GitMetadata,
    /// An ignore source changed: a `.gitignore`, `info/exclude`, or the
    /// resolved `core.excludesFile`. Requires a matcher rebuild.
    IgnoreSource,
    /// The backend could not say which path changed (coarse or unknown
    /// granularity). Treated as potentially affecting everything.
    Unknown,
    /// Events may have been lost: backend queue overflow, an explicit rescan
    /// request, a watched directory disappearing, or channel disconnect.
    /// Requires full recovery (matcher rebuild + rediscover + reload).
    Overflow,
}

/// Everything observed within one debounce window, aggregated.
///
/// Paths are a set: a checkout can flood one window with tens of thousands
/// of events, and aggregation runs per event on the terminal thread, so
/// dedup must not be linear per insertion.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct EventBatch {
    /// Union of the worktree paths seen in the window.
    pub paths: HashSet<GitPath>,
    pub git_metadata: bool,
    pub ignore_source: bool,
    pub unknown: bool,
    pub overflow: bool,
}

impl EventBatch {
    fn absorb(&mut self, event: WatchEvent) {
        match event {
            WatchEvent::Worktree { path } => {
                self.paths.insert(path);
            }
            WatchEvent::GitMetadata => self.git_metadata = true,
            WatchEvent::IgnoreSource => self.ignore_source = true,
            WatchEvent::Unknown => self.unknown = true,
            WatchEvent::Overflow => self.overflow = true,
        }
    }

    /// Whether events may have been lost, voiding any carry-over shortcut.
    pub fn lossy(&self) -> bool {
        self.overflow
    }

    /// Conservative check: may this batch have touched the content shown for
    /// a selection with these old/new paths? `true` forces a re-read; only a
    /// batch of clearly unrelated plain path events allows carrying the
    /// currently displayed content over to the next snapshot.
    ///
    /// `ignore_source` counts as affecting: an ignore-source event may *be*
    /// the selected file (someone viewing a `.gitignore` edit), and the
    /// classified event does not carry its path.
    pub fn affects_selection(&self, old: Option<&GitPath>, new: Option<&GitPath>) -> bool {
        if self.git_metadata || self.ignore_source || self.unknown || self.overflow {
            return true;
        }
        let sides = [old, new];
        let selected = sides.iter().flatten();
        selected.into_iter().any(|selected| {
            self.paths.contains(*selected)
                || self.paths.iter().any(|path| path.is_ancestor_of(selected))
        })
    }
}
