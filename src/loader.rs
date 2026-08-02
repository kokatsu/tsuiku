//! Background loading and classification of file contents.
//!
//! Repository discovery initially produces only lightweight change metadata.
//! This worker reads blob or worktree bytes for the selected file and one
//! adjacent prefetch candidate, classifying them as text, binary, or unchanged.
//! It therefore avoids reading every changed file before the first screen can
//! be shown.
//!
//! One job may be running and one newer job may be queued. A new selection
//! replaces the queued job, while a prefetch never displaces queued selected
//! work. Rapid navigation therefore does not build an unbounded backlog.
//! Completed results carry the snapshot generation and `file_id` they were
//! requested under; a file index is only meaningful within its snapshot, so
//! the owner applies a result only when both match its current state.

use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::{self, JoinHandle};

use crate::change::FileChange;
use crate::ids::{ContentPairId, ResolvedContent, SnapshotId};
use crate::resolve::{ContentResolver, GixResolver, ResolveError, ResolvedChange};
use crate::text::{ClassifiedContent, TextContent, classify};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PreparedKind {
    /// Both sides can be displayed and compared as text.
    Text,
    /// At least one side is binary, so no line diff should be computed.
    Binary,
    /// Resolution showed that the candidate has no observable content change.
    NoOp,
}

/// Fully loaded representation of one discovered file change.
#[derive(Clone, Debug)]
pub struct PreparedContent {
    /// Content-derived identity used by the content and line-diff caches.
    pub pair: ContentPairId,
    /// Determines whether the contents should be diffed, skipped, or hidden.
    pub kind: PreparedKind,
    /// Old text, present only when `kind` is [`PreparedKind::Text`].
    pub old: Option<Arc<TextContent>>,
    /// New text, present only when `kind` is [`PreparedKind::Text`].
    pub new: Option<Arc<TextContent>>,
}

impl PreparedContent {
    pub fn estimated_bytes(&self) -> usize {
        fn side(text: &Option<Arc<TextContent>>) -> usize {
            text.as_ref()
                .map(|text| {
                    std::mem::size_of::<TextContent>()
                        + text.bytes.len()
                        + text.lines.len() * std::mem::size_of::<crate::text::LineRecord>()
                })
                .unwrap_or(0)
        }
        std::mem::size_of::<Self>() + side(&self.old) + side(&self.new)
    }
}

#[derive(Clone)]
struct LoadJob {
    snapshot: SnapshotId,
    file_id: usize,
    change: FileChange,
}

impl LoadJob {
    fn identity(&self) -> (SnapshotId, usize) {
        (self.snapshot, self.file_id)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum LoadPriority {
    Selected,
    Prefetch,
}

struct QueuedLoad {
    job: LoadJob,
    priority: LoadPriority,
}

struct Slot {
    queued: Option<QueuedLoad>,
    running: Option<(SnapshotId, usize)>,
    stopped: bool,
}

impl Slot {
    fn enqueue(&mut self, job: LoadJob, priority: LoadPriority) -> bool {
        if self.running == Some(job.identity()) {
            // If the user navigates A -> B -> A while A is still loading, B is
            // no longer useful and must not start after A finishes.
            if priority == LoadPriority::Selected {
                self.queued = None;
            }
            return false;
        }

        if let Some(queued) = &mut self.queued
            && queued.job.identity() == job.identity()
        {
            // Selecting a queued prefetch promotes it so another prefetch
            // cannot replace the now user-visible request.
            if priority == LoadPriority::Selected {
                queued.priority = LoadPriority::Selected;
            }
            return false;
        }

        if priority == LoadPriority::Prefetch
            && self
                .queued
                .as_ref()
                .is_some_and(|queued| queued.priority == LoadPriority::Selected)
        {
            return false;
        }

        self.queued = Some(QueuedLoad { job, priority });
        true
    }
}

/// Result returned by the loading thread.
pub struct LoadResult {
    /// Generation of the snapshot the request belonged to.
    pub snapshot: SnapshotId,
    /// Index into that snapshot's file list.
    pub file_id: usize,
    /// Loaded content or the repository-resolution error.
    pub result: Result<PreparedContent, ResolveError>,
}

/// Coordinates selected-file loading without blocking the terminal event loop.
pub struct ContentLoadCoordinator {
    slot: Arc<(Mutex<Slot>, Condvar)>,
    results: mpsc::Receiver<LoadResult>,
    thread: Option<JoinHandle<()>>,
}

impl ContentLoadCoordinator {
    /// Starts the single background loading thread.
    pub fn new(resolver: GixResolver) -> Self {
        let slot = Arc::new((
            Mutex::new(Slot {
                queued: None,
                running: None,
                stopped: false,
            }),
            Condvar::new(),
        ));
        let worker_slot = Arc::clone(&slot);
        let (tx, results) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("tsuiku-content".into())
            .spawn(move || worker_loop(resolver, worker_slot, tx))
            .expect("content worker thread must start");
        Self {
            slot,
            results,
            thread: Some(thread),
        }
    }

    /// Requests loading for `file_id` in the given snapshot generation.
    ///
    /// Returns `false` when that file is already running or queued. Otherwise,
    /// any older queued request is replaced by this one.
    pub fn request(&self, snapshot: SnapshotId, file_id: usize, change: FileChange) -> bool {
        let (lock, wake) = &*self.slot;
        let mut slot = lock.lock().expect("content queue lock poisoned");
        let queued = slot.enqueue(
            LoadJob {
                snapshot,
                file_id,
                change,
            },
            LoadPriority::Selected,
        );
        if queued {
            wake.notify_one();
        }
        queued
    }

    /// Requests low-priority loading for an adjacent file.
    ///
    /// A prefetch may replace an older queued prefetch, but never selected
    /// work. Returns `false` when the file is already running or queued, or
    /// when a selected request already owns the queue. Resolution itself is
    /// not cancellable: if a prefetch is already running, a later selected
    /// request waits behind that one read.
    pub fn prefetch(&self, snapshot: SnapshotId, file_id: usize, change: FileChange) -> bool {
        let (lock, wake) = &*self.slot;
        let mut slot = lock.lock().expect("content queue lock poisoned");
        let queued = slot.enqueue(
            LoadJob {
                snapshot,
                file_id,
                change,
            },
            LoadPriority::Prefetch,
        );
        if queued {
            wake.notify_one();
        }
        queued
    }

    /// Returns one completed load without waiting, if available.
    pub fn try_recv(&self) -> Option<LoadResult> {
        self.results.try_recv().ok()
    }

    #[cfg(test)]
    pub(crate) fn new_for_test() -> Self {
        let slot = Arc::new((
            Mutex::new(Slot {
                queued: None,
                running: None,
                stopped: false,
            }),
            Condvar::new(),
        ));
        let (_tx, results) = mpsc::channel();
        Self {
            slot,
            results,
            thread: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn queued_for_test(&self) -> Option<(usize, LoadPriority)> {
        let slot = self.slot.0.lock().expect("content queue lock poisoned");
        slot.queued
            .as_ref()
            .map(|queued| (queued.job.file_id, queued.priority))
    }

    #[cfg(test)]
    pub(crate) fn take_queued_for_test(&self) -> Option<(usize, LoadPriority)> {
        let mut slot = self.slot.0.lock().expect("content queue lock poisoned");
        slot.queued
            .take()
            .map(|queued| (queued.job.file_id, queued.priority))
    }
}

impl Drop for ContentLoadCoordinator {
    fn drop(&mut self) {
        let (lock, wake) = &*self.slot;
        if let Ok(mut slot) = lock.lock() {
            slot.stopped = true;
            slot.queued = None;
            wake.notify_one();
        }
        // Dropping the JoinHandle detaches the worker. A read already in
        // progress may finish, but quitting never waits for it.
        self.thread.take();
    }
}

fn worker_loop(
    resolver: GixResolver,
    slot: Arc<(Mutex<Slot>, Condvar)>,
    tx: mpsc::Sender<LoadResult>,
) {
    loop {
        let job = {
            let (lock, wake) = &*slot;
            let mut state = lock.lock().expect("content queue lock poisoned");
            while state.queued.is_none() && !state.stopped {
                state = wake.wait(state).expect("content queue lock poisoned");
            }
            if state.stopped {
                return;
            }
            let job = state.queued.take().expect("checked above").job;
            state.running = Some(job.identity());
            job
        };

        let identity = job.identity();
        let result = prepare(&resolver, job.change);
        let _ = tx.send(LoadResult {
            snapshot: identity.0,
            file_id: identity.1,
            result,
        });
        let (lock, _) = &*slot;
        if let Ok(mut state) = lock.lock()
            && state.running == Some(identity)
        {
            state.running = None;
        }
    }
}

fn prepare(resolver: &GixResolver, change: FileChange) -> Result<PreparedContent, ResolveError> {
    let resolved = ResolvedChange {
        old: resolver.resolve(&change.old)?,
        new: resolver.resolve(&change.new)?,
        change,
    };
    let pair = resolved.pair_id();
    if resolved.is_no_op() {
        return Ok(PreparedContent {
            pair,
            kind: PreparedKind::NoOp,
            old: None,
            new: None,
        });
    }
    let old = classify_side(&resolved.old);
    let new = classify_side(&resolved.new);
    let kind = if old.is_some() && new.is_some() {
        PreparedKind::Text
    } else {
        PreparedKind::Binary
    };
    Ok(PreparedContent {
        pair,
        kind,
        old,
        new,
    })
}

fn classify_side(content: &ResolvedContent) -> Option<Arc<TextContent>> {
    let bytes: Arc<[u8]> = match content {
        ResolvedContent::Absent => Arc::from(&b""[..]),
        ResolvedContent::Present(present) => Arc::clone(&present.bytes),
    };
    match classify(bytes) {
        ClassifiedContent::Text(text) => Some(Arc::new(text)),
        ClassifiedContent::Binary(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ContentSource;

    fn job(file_id: usize) -> LoadJob {
        job_in(SnapshotId(1), file_id)
    }

    fn job_in(snapshot: SnapshotId, file_id: usize) -> LoadJob {
        LoadJob {
            snapshot,
            file_id,
            change: FileChange::classify(
                None,
                Some(crate::path::GitPath::from_bytes(b"x")),
                ContentSource::Absent,
                ContentSource::Submodule {
                    commit: crate::ids::Oid([1; 20]),
                    dirty: false,
                },
                None,
                Some(crate::change::EntryMode::Submodule),
            )
            .expect("change"),
        }
    }

    #[test]
    fn queued_job_is_replaced_by_the_latest() {
        let mut slot = Slot {
            queued: None,
            running: None,
            stopped: false,
        };
        assert!(slot.enqueue(job(1), LoadPriority::Selected));
        assert!(slot.enqueue(job(2), LoadPriority::Selected));
        assert_eq!(
            slot.queued.as_ref().map(|queued| queued.job.file_id),
            Some(2)
        );
    }

    #[test]
    fn returning_to_running_job_does_not_duplicate_it_and_clears_stale_queue() {
        let mut slot = Slot {
            queued: Some(QueuedLoad {
                job: job(2),
                priority: LoadPriority::Selected,
            }),
            running: Some((SnapshotId(1), 1)),
            stopped: false,
        };
        assert!(!slot.enqueue(job(1), LoadPriority::Selected));
        assert!(slot.queued.is_none());
    }

    #[test]
    fn selected_work_replaces_a_queued_prefetch() {
        let mut slot = Slot {
            queued: Some(QueuedLoad {
                job: job(2),
                priority: LoadPriority::Prefetch,
            }),
            running: Some((SnapshotId(1), 1)),
            stopped: false,
        };

        assert!(slot.enqueue(job(3), LoadPriority::Selected));
        let queued = slot.queued.as_ref().expect("selected work queued");
        assert_eq!(queued.job.file_id, 3);
        assert_eq!(queued.priority, LoadPriority::Selected);
    }

    #[test]
    fn latest_prefetch_replaces_the_older_prefetch() {
        let mut slot = Slot {
            queued: Some(QueuedLoad {
                job: job(2),
                priority: LoadPriority::Prefetch,
            }),
            running: Some((SnapshotId(1), 1)),
            stopped: false,
        };

        assert!(slot.enqueue(job(3), LoadPriority::Prefetch));
        let queued = slot.queued.as_ref().expect("latest prefetch queued");
        assert_eq!(queued.job.file_id, 3);
        assert_eq!(queued.priority, LoadPriority::Prefetch);
    }

    #[test]
    fn prefetch_never_displaces_queued_selected_work() {
        let mut slot = Slot {
            queued: Some(QueuedLoad {
                job: job(2),
                priority: LoadPriority::Selected,
            }),
            running: Some((SnapshotId(1), 1)),
            stopped: false,
        };

        assert!(!slot.enqueue(job(3), LoadPriority::Prefetch));
        let queued = slot.queued.as_ref().expect("selected work retained");
        assert_eq!(queued.job.file_id, 2);
        assert_eq!(queued.priority, LoadPriority::Selected);
    }

    #[test]
    fn selecting_a_queued_prefetch_promotes_it() {
        let mut slot = Slot {
            queued: Some(QueuedLoad {
                job: job(2),
                priority: LoadPriority::Prefetch,
            }),
            running: Some((SnapshotId(1), 1)),
            stopped: false,
        };

        assert!(!slot.enqueue(job(2), LoadPriority::Selected));
        assert_eq!(
            slot.queued.as_ref().map(|queued| queued.priority),
            Some(LoadPriority::Selected)
        );
        assert!(!slot.enqueue(job(3), LoadPriority::Prefetch));
        assert_eq!(
            slot.queued.as_ref().map(|queued| queued.job.file_id),
            Some(2)
        );
    }
}
