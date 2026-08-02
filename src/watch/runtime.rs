//! The watch runtime: one worker thread owning the filesystem watcher, the
//! classifier, the tracked-first filter, the debouncer, and rediscovery.
//!
//! Everything heavier than a channel receive stays on this thread — the
//! terminal thread only polls for completed [`WatchUpdate`]s and applies
//! them. Because one thread owns the repository, the index, the ignore
//! matcher, and the discovery walk, a rebuilt matcher and the snapshot it
//! produced always arrive together in a single update: there is no window
//! where one is applied without the other.
//!
//! Events arriving while a rediscover runs simply queue in the channel and
//! aggregate into the next batch — the accumulator contract without a
//! second queue. Possible event loss (backend overflow, rescan requests,
//! watcher errors) degrades to full recovery: matcher rebuild, watch
//! re-registration, rediscover, and a batch marked lossy so the owner
//! re-reads displayed content instead of carrying it over.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use notify::event::{AccessKind, AccessMode, ModifyKind};
use notify::{ErrorKind, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use super::debounce::Debouncer;
use super::filter::IgnoreFilter;
use super::targets::WatchTargets;
use super::{EventBatch, WatchEvent};
use crate::change::{ChangeDiscoverer, ChangeQuery, ChangeSet, DiffTarget};
use crate::discover::GixDiscoverer;

/// How often the worker wakes to notice a stop request even when the
/// filesystem is quiet.
const IDLE_TICK: Duration = Duration::from_millis(500);

/// One completed refresh, or the reason watching ended.
pub enum WatchUpdate {
    /// A fresh discovery snapshot plus the aggregated batch that caused it.
    Refresh {
        changes: ChangeSet,
        batch: EventBatch,
    },
    /// Watching never started or could not be recovered; the viewer keeps
    /// working without it. Shown once in the status bar.
    Degraded { reason: String },
}

/// Handle owned by the terminal thread.
pub struct WatchCoordinator {
    updates: mpsc::Receiver<WatchUpdate>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl WatchCoordinator {
    /// Start watching the repository that contains `path`. Failures are
    /// reported through the first [`WatchUpdate::Degraded`] poll rather
    /// than an error here, so startup never blocks on the filesystem.
    pub fn start(path: PathBuf) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let (tx, updates) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("tsuiku-watch".into())
            .spawn(move || {
                if let Err(reason) = worker(&path, &tx, &worker_stop) {
                    let _ = tx.send(WatchUpdate::Degraded { reason });
                }
            })
            .expect("watch worker thread must start");
        Self {
            updates,
            stop,
            thread: Some(thread),
        }
    }

    /// One completed update, without waiting.
    pub fn poll(&self) -> Option<WatchUpdate> {
        self.updates.try_recv().ok()
    }
}

impl Drop for WatchCoordinator {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // The worker notices the flag within one idle tick; quitting must
        // not wait for a rediscover that is already running.
        self.thread.take();
    }
}

/// Wraps a raw notify event stream result for the worker loop.
type RawEvent = Result<notify::Event, notify::Error>;

struct WatchState {
    watcher: RecommendedWatcher,
    targets: WatchTargets,
    watched_dirs: Vec<PathBuf>,
    /// Target directories that did not exist at registration time, with the
    /// event their targets represent. Their nearest existing ancestor is
    /// watched instead; once anything on the missing chain is created a
    /// rearm re-resolves and the watch moves closer (or lands).
    pending_dirs: Vec<(PathBuf, WatchEvent)>,
}

impl WatchState {
    fn arm(watcher: RecommendedWatcher, targets: WatchTargets) -> Result<Self, String> {
        let mut state = Self {
            watcher,
            targets,
            watched_dirs: Vec::new(),
            pending_dirs: Vec::new(),
        };
        state.register()?;
        Ok(state)
    }

    /// Watch the worktree recursively and each metadata parent directory
    /// non-recursively. A directory that does not exist yet is deferred:
    /// its nearest existing ancestor is watched so its creation is seen
    /// (silently skipping it would mean e.g. a configured-but-not-yet-
    /// created excludes file is never picked up). Any other failure means
    /// metadata changes would be silently missed, which must surface.
    fn register(&mut self) -> Result<(), String> {
        let root = self.targets.worktree_root().to_path_buf();
        self.watcher
            .watch(&root, RecursiveMode::Recursive)
            .map_err(|error| format!("cannot watch {}: {error}", root.display()))?;
        self.watched_dirs.push(root);
        // Metadata parents inside the worktree are also covered by the
        // recursive watch; registering them again only duplicates events,
        // which the batch set-union absorbs.
        for (dir, event) in self.targets.metadata_dirs_with_events() {
            match self.watcher.watch(dir, RecursiveMode::NonRecursive) {
                Ok(()) => self.watched_dirs.push(dir.to_path_buf()),
                Err(error) if is_missing_path(&error) => {
                    // A pending dir without a live ancestor watch would
                    // never see its own creation, so registration retries
                    // up the (finite) parent chain until a watch sticks;
                    // running out of ancestors is a hard failure.
                    let mut ancestor = nearest_existing_ancestor(dir);
                    loop {
                        match self.watcher.watch(&ancestor, RecursiveMode::NonRecursive) {
                            Ok(()) => {
                                self.watched_dirs.push(ancestor);
                                break;
                            }
                            Err(error) if is_missing_path(&error) => {
                                let retry = nearest_existing_ancestor(&ancestor);
                                if retry == ancestor {
                                    return Err(format!(
                                        "cannot watch {}: {error}",
                                        ancestor.display()
                                    ));
                                }
                                ancestor = retry;
                            }
                            Err(error) => {
                                return Err(format!(
                                    "cannot watch {}: {error}",
                                    ancestor.display()
                                ));
                            }
                        }
                    }
                    self.pending_dirs.push((dir.to_path_buf(), event));
                }
                Err(error) => {
                    return Err(format!("cannot watch {}: {error}", dir.display()));
                }
            }
        }
        Ok(())
    }

    /// Re-resolve targets (the HEAD ref may point elsewhere now) and swap
    /// the watch set accordingly.
    fn rearm(&mut self, repo: &gix::Repository) -> Result<(), String> {
        let targets = WatchTargets::resolve(repo).map_err(|error| error.to_string())?;
        for dir in self.watched_dirs.drain(..) {
            let _ = self.watcher.unwatch(&dir);
        }
        self.pending_dirs.clear();
        self.targets = targets;
        self.register()
    }
}

/// The deepest existing ancestor of a missing directory. Only a definite
/// `NotFound` walks upward: an undecidable probe (e.g. permission denied)
/// stops there so the subsequent watch attempt surfaces the real error
/// instead of silently drifting to a higher directory.
fn nearest_existing_ancestor(dir: &Path) -> PathBuf {
    let mut current = dir;
    loop {
        match std::fs::metadata(current) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => match current.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => current = parent,
                _ => break,
            },
            Err(_) => break,
        }
    }
    current.to_path_buf()
}

/// `PathNotFound` (or the equivalent io error) for optional directories
/// like `info/` that may not exist yet.
fn is_missing_path(error: &notify::Error) -> bool {
    match &error.kind {
        ErrorKind::PathNotFound => true,
        ErrorKind::Io(io) => io.kind() == std::io::ErrorKind::NotFound,
        _ => false,
    }
}

fn worker(path: &Path, tx: &mpsc::Sender<WatchUpdate>, stop: &AtomicBool) -> Result<(), String> {
    let mut discoverer =
        GixDiscoverer::open(path).map_err(|error| format!("cannot open repository: {error}"))?;
    let targets = WatchTargets::resolve(discoverer.repository())
        .map_err(|error| format!("cannot resolve watch targets: {error}"))?;

    let (event_tx, events) = mpsc::channel::<RawEvent>();
    let watcher = notify::recommended_watcher(move |event: RawEvent| {
        let _ = event_tx.send(event);
    })
    .map_err(|error| format!("cannot create filesystem watcher: {error}"))?;
    let mut state = WatchState::arm(watcher, targets)?;

    // A filter that cannot be built keeps everything (only costs extra
    // refreshes); a broken one must never silently drop updates.
    let mut filter = IgnoreFilter::build(discoverer.repository()).ok();
    let mut filter_dirty = false;
    let mut debouncer = Debouncer::default();

    // The initial refresh closes the startup gap: edits landing between the
    // owner's own discovery and the watcher arming above produce no events,
    // so the first post-arming snapshot must come from a fresh walk. It also
    // tells consumers the watcher is live. The batch is marked unknown —
    // anything may have changed in the gap, so displayed content must be
    // re-read, never carried over.
    let initial = discoverer
        .discover(&ChangeQuery::new(DiffTarget::WorktreeVsHead))
        .map_err(|error| format!("initial rediscovery failed: {error}"))?;
    if tx
        .send(WatchUpdate::Refresh {
            changes: initial,
            batch: EventBatch {
                unknown: true,
                ..EventBatch::default()
            },
        })
        .is_err()
    {
        return Ok(());
    }

    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let now = Instant::now();
        let timeout = debouncer
            .deadline()
            .map(|deadline| deadline.saturating_duration_since(now))
            .unwrap_or(IDLE_TICK)
            .min(IDLE_TICK);
        match events.recv_timeout(timeout) {
            Ok(raw) => {
                let now = Instant::now();
                for event in classify_raw(raw, &state) {
                    let keep = match (&event, filter_dirty, filter.as_mut()) {
                        // While an ignore-source change is pending, dropping
                        // by the old matcher could lose updates forever.
                        (WatchEvent::Worktree { path }, false, Some(filter)) => filter.keep(path),
                        _ => true,
                    };
                    if matches!(
                        event,
                        WatchEvent::IgnoreSource | WatchEvent::Overflow | WatchEvent::Unknown
                    ) {
                        filter_dirty = true;
                    }
                    if keep {
                        debouncer.observe(event, now);
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("filesystem watcher stopped delivering events".into());
            }
        }

        let Some(batch) = debouncer.due(Instant::now()) else {
            continue;
        };
        // Metadata changes may have moved HEAD or rewritten config; loss may
        // have invalidated anything. gix snapshots configuration when the
        // repository is opened, so the repository is reopened before targets
        // and matcher are rebuilt — reusing the old one would resurrect a
        // stale core.excludesFile or ref layout.
        if batch.git_metadata || batch.ignore_source || batch.lossy() {
            discoverer = GixDiscoverer::open(path)
                .map_err(|error| format!("cannot reopen repository: {error}"))?;
            state.rearm(discoverer.repository())?;
        }
        let changes = discoverer
            .discover(&ChangeQuery::new(DiffTarget::WorktreeVsHead))
            .map_err(|error| format!("rediscovery failed: {error}"))?;
        // The new matcher and the snapshot built beside it apply together;
        // events queued during the walk aggregate into the next batch.
        filter = IgnoreFilter::build(discoverer.repository()).ok();
        filter_dirty = false;
        if tx.send(WatchUpdate::Refresh { changes, batch }).is_err() {
            return Ok(());
        }
    }
}

/// Map one raw notify delivery onto classified events.
fn classify_raw(raw: RawEvent, state: &WatchState) -> Vec<WatchEvent> {
    let event = match raw {
        Ok(event) => event,
        // A backend error may mean lost events: full recovery.
        Err(_) => return vec![WatchEvent::Overflow],
    };
    if event.need_rescan() {
        return vec![WatchEvent::Overflow];
    }
    // Read-side access events must never schedule a refresh: inotify
    // delivers OPEN/CLOSE_NOWRITE by default, and rediscovery itself opens
    // .git/index — classifying those would loop refresh → open → refresh.
    // Only the write-completion access carries information.
    if let EventKind::Access(access) = &event.kind
        && !matches!(access, AccessKind::Close(AccessMode::Write))
    {
        return Vec::new();
    }
    // A watched directory being removed or renamed loses its watch: every
    // later change under it would be missed, so recover fully. Plain path
    // classification would file these under git-internal noise.
    let displaced = matches!(
        event.kind,
        EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))
    );
    if displaced
        && event
            .paths
            .iter()
            .any(|path| state.watched_dirs.iter().any(|dir| dir == path))
    {
        return vec![WatchEvent::Overflow];
    }
    // Creation along the chain of a still-missing target directory: rearm
    // so the watch moves closer to (or lands on) the target.
    if let Some((_, pending_event)) = state.pending_dirs.iter().find(|(dir, _)| {
        event
            .paths
            .iter()
            .any(|path| dir == path || dir.starts_with(path))
    }) {
        return vec![pending_event.clone()];
    }
    if event.paths.is_empty() {
        return vec![WatchEvent::Unknown];
    }
    event
        .paths
        .iter()
        .filter_map(|path| state.targets.classify(path))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_existing_ancestor_walks_only_missing_links() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let existing = dir.path().to_path_buf();
        let missing = existing.join("a/b/c");
        assert_eq!(nearest_existing_ancestor(&missing), existing);
        assert_eq!(nearest_existing_ancestor(&existing), existing);
    }

    #[test]
    fn nearest_existing_ancestor_bottoms_out_at_the_root() {
        assert_eq!(
            nearest_existing_ancestor(Path::new("/nonexistent-tsuiku-test/x/y")),
            Path::new("/")
        );
    }
}
