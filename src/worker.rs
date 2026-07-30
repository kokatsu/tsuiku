//! Background line-diff computation and result coordination.
//!
//! The terminal UI identifies a line diff by a content-derived cache key. The
//! coordinator keeps one worker job running and at most one replaceable queued
//! job, so rapid file navigation cannot create an unbounded backlog.
//!
//! `current_key` identifies the result the UI currently wants. Every completed
//! job is cached, but only a result whose key still equals `current_key`
//! changes the visible state. A result for a previously selected file is thus
//! reusable later without briefly displaying the wrong file.

use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::{self, JoinHandle};

use crate::asyncstate::{
    AsyncState, LineDiffCacheKey, LineDiffError, LineDiffState, LineDiffUnavailable, RequestId,
};
use crate::cache::WeightedLru;
use crate::linediff::{DiffRow, engine, hunk_starts, line_tokens};
use crate::text::TextContent;

#[derive(Clone)]
struct Job {
    request_id: RequestId,
    key: LineDiffCacheKey,
    old: Arc<TextContent>,
    new: Arc<TextContent>,
}

struct Slot {
    queued: Option<Job>,
    running: Option<(LineDiffCacheKey, RequestId)>,
    stopped: bool,
}

enum Enqueue {
    Queued(RequestId),
    Existing(RequestId),
}

impl Slot {
    fn enqueue(&mut self, job: Job) -> Enqueue {
        if let Some((key, request_id)) = self.running
            && key == job.key
        {
            // If the user navigates A -> B -> A while A is still running, B is
            // no longer selected and A must not be queued a second time.
            self.queued = None;
            return Enqueue::Existing(request_id);
        }
        if let Some(queued) = &self.queued
            && queued.key == job.key
        {
            return Enqueue::Existing(queued.request_id);
        }
        let request_id = job.request_id;
        self.queued = Some(job);
        Enqueue::Queued(request_id)
    }
}

struct ResultMessage {
    key: LineDiffCacheKey,
    rows: Arc<[DiffRow]>,
    hunk_starts: Arc<[usize]>,
}

#[derive(Clone)]
struct CacheEntry {
    rows: Arc<[DiffRow]>,
    hunk_starts: Arc<[usize]>,
}

/// Runs line-diff work off the terminal thread and caches completed row tables.
pub struct LineDiffCoordinator {
    slot: Arc<(Mutex<Slot>, Condvar)>,
    results: mpsc::Receiver<ResultMessage>,
    thread: Option<JoinHandle<()>>,
    cache: WeightedLru<LineDiffCacheKey, CacheEntry>,
    current_key: Option<LineDiffCacheKey>,
    state: LineDiffState,
    current_hunk_starts: Arc<[usize]>,
    next_request_id: u64,
}

impl LineDiffCoordinator {
    /// Starts one worker thread and creates a cache with the given byte budget.
    pub fn new(cache_capacity: usize) -> Self {
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
            .name("tsuiku-linediff".into())
            .spawn(move || worker_loop(worker_slot, tx))
            .expect("line-diff worker thread must start");
        Self {
            slot,
            results,
            thread: Some(thread),
            cache: WeightedLru::new(cache_capacity),
            current_key: None,
            state: AsyncState::NotRequested,
            current_hunk_starts: Arc::from([]),
            next_request_id: 1,
        }
    }

    /// Makes `key` the line diff currently requested by the UI.
    ///
    /// A cached result is applied immediately. Otherwise the computation is
    /// queued, replacing an older queued request when necessary.
    pub fn request(&mut self, key: LineDiffCacheKey, old: Arc<TextContent>, new: Arc<TextContent>) {
        self.poll();
        self.current_key = Some(key);
        if let Some(entry) = self.cache.get_cloned(&key) {
            self.current_hunk_starts = entry.hunk_starts;
            self.state = AsyncState::Ready(entry.rows);
            return;
        }
        self.current_hunk_starts = Arc::from([]);

        let request_id = RequestId(self.next_request_id);
        self.next_request_id += 1;
        let job = Job {
            request_id,
            key,
            old,
            new,
        };
        let (lock, wake) = &*self.slot;
        let mut slot = lock.lock().expect("line-diff queue lock poisoned");
        let effective_id = match slot.enqueue(job) {
            Enqueue::Queued(id) => {
                wake.notify_one();
                id
            }
            Enqueue::Existing(id) => id,
        };
        self.state = AsyncState::Pending {
            request_id: effective_id,
        };
    }

    /// Drains completed work without waiting.
    ///
    /// Every result enters the cache, while only the current cache key changes
    /// the state returned by [`Self::state`].
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(result) = self.results.try_recv() {
            changed |= self.accept_result(result);
        }
        if self.thread.as_ref().is_some_and(JoinHandle::is_finished)
            && matches!(self.state, AsyncState::Pending { .. })
        {
            self.state = AsyncState::Failed(LineDiffError::WorkerGone);
            changed = true;
        }
        changed
    }

    fn accept_result(&mut self, result: ResultMessage) -> bool {
        let weight = std::mem::size_of::<LineDiffCacheKey>()
            + std::mem::size_of::<CacheEntry>()
            + std::mem::size_of_val(result.rows.as_ref())
            + std::mem::size_of_val(result.hunk_starts.as_ref());
        self.cache.insert(
            result.key,
            CacheEntry {
                rows: Arc::clone(&result.rows),
                hunk_starts: Arc::clone(&result.hunk_starts),
            },
            weight,
        );
        if self.current_key == Some(result.key) {
            self.current_hunk_starts = result.hunk_starts;
            self.state = AsyncState::Ready(result.rows);
            true
        } else {
            false
        }
    }

    /// Clears the requested key and records why line diffing is unavailable.
    pub fn skip(&mut self, reason: LineDiffUnavailable) {
        self.poll();
        self.current_key = None;
        self.current_hunk_starts = Arc::from([]);
        if let Ok(mut slot) = self.slot.0.lock() {
            slot.queued = None;
        }
        self.state = AsyncState::Skipped(reason);
    }

    /// Returns to the initial state with no requested line diff.
    pub fn reset(&mut self) {
        self.poll();
        self.current_key = None;
        self.current_hunk_starts = Arc::from([]);
        if let Ok(mut slot) = self.slot.0.lock() {
            slot.queued = None;
        }
        self.state = AsyncState::NotRequested;
    }

    /// Returns the state associated with the currently selected file.
    pub fn state(&self) -> &LineDiffState {
        &self.state
    }

    /// Precomputed navigation targets for the current ready row table.
    pub fn hunk_starts(&self) -> &[usize] {
        &self.current_hunk_starts
    }

    /// Returns whether a completed result is cached for `key`.
    pub fn is_cached(&self, key: &LineDiffCacheKey) -> bool {
        self.cache.contains_key(key)
    }

    /// Returns the estimated bytes occupied by cached line-diff rows.
    pub fn cache_weight(&self) -> usize {
        self.cache.total_weight()
    }
}

impl Default for LineDiffCoordinator {
    fn default() -> Self {
        Self::new(32 * 1024 * 1024)
    }
}

impl Drop for LineDiffCoordinator {
    fn drop(&mut self) {
        let (lock, wake) = &*self.slot;
        if let Ok(mut slot) = lock.lock() {
            slot.stopped = true;
            slot.queued = None;
            wake.notify_one();
        }
        // Dropping the handle detaches the worker. Quitting must not wait for a
        // potentially large line diff that is already running.
        self.thread.take();
    }
}

fn worker_loop(slot: Arc<(Mutex<Slot>, Condvar)>, tx: mpsc::Sender<ResultMessage>) {
    loop {
        let job = {
            let (lock, wake) = &*slot;
            let mut state = lock.lock().expect("line-diff queue lock poisoned");
            while state.queued.is_none() && !state.stopped {
                state = wake.wait(state).expect("line-diff queue lock poisoned");
            }
            if state.stopped {
                return;
            }
            let job = state.queued.take().expect("checked above");
            state.running = Some((job.key, job.request_id));
            job
        };

        let old = line_tokens(&job.old);
        let new = line_tokens(&job.new);
        let rows = Arc::from(engine(job.key.engine).diff(&old, &new));
        let hunk_starts = Arc::from(hunk_starts(&rows));
        if tx
            .send(ResultMessage {
                key: job.key,
                rows,
                hunk_starts,
            })
            .is_err()
        {
            return;
        }
        let (lock, _) = &*slot;
        if let Ok(mut state) = lock.lock()
            && state.running.is_some_and(|(key, _)| key == job.key)
        {
            state.running = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    use crate::asyncstate::{LINE_MODEL_VERSION, LineDiffEngineId};
    use crate::coords::LineIndex;
    use crate::ids::{ContentId, ContentIdentity, ContentPairId};
    use crate::text::{ClassifiedContent, classify};

    fn text(s: &str) -> Arc<TextContent> {
        match classify(Arc::from(s.as_bytes())) {
            ClassifiedContent::Text(text) => Arc::new(text),
            ClassifiedContent::Binary(_) => panic!("text fixture classified as binary"),
        }
    }

    fn key(old: &str, new: &str) -> LineDiffCacheKey {
        LineDiffCacheKey {
            pair: ContentPairId {
                old: ContentIdentity::Present(ContentId::compute(old.as_bytes())),
                new: ContentIdentity::Present(ContentId::compute(new.as_bytes())),
            },
            engine: LineDiffEngineId::Imara,
            options_fingerprint: 0,
            line_model_version: LINE_MODEL_VERSION,
        }
    }

    fn wait_ready(worker: &mut LineDiffCoordinator) -> Arc<[DiffRow]> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            worker.poll();
            if let AsyncState::Ready(rows) = worker.state() {
                return Arc::clone(rows);
            }
            assert!(Instant::now() < deadline, "worker did not finish");
            thread::yield_now();
        }
    }

    #[test]
    fn computes_and_reuses_a_keyed_result() {
        let mut worker = LineDiffCoordinator::new(1024 * 1024);
        let cache_key = key("a\n", "b\n");
        worker.request(cache_key, text("a\n"), text("b\n"));
        assert_eq!(wait_ready(&mut worker).len(), 2);
        assert_eq!(worker.hunk_starts(), &[0]);
        assert!(worker.is_cached(&cache_key));

        worker.reset();
        worker.request(cache_key, text("a\n"), text("b\n"));
        assert!(matches!(worker.state(), AsyncState::Ready(_)));
        assert_eq!(worker.hunk_starts(), &[0]);
    }

    #[test]
    fn stale_result_is_cached_but_not_applied() {
        let mut worker = LineDiffCoordinator::new(1024 * 1024);
        let old_key = key("a\n", "b\n");
        let current_key = key("x\n", "y\n");
        worker.current_key = Some(current_key);
        worker.state = AsyncState::Pending {
            request_id: RequestId(1),
        };
        assert!(!worker.accept_result(ResultMessage {
            key: old_key,
            rows: Arc::from([DiffRow::Removed { old: LineIndex(0) }]),
            hunk_starts: Arc::from([0]),
        }));
        assert!(matches!(worker.state(), AsyncState::Pending { .. }));
        assert!(worker.hunk_starts().is_empty());
        assert!(worker.is_cached(&old_key));
    }

    #[test]
    fn skip_keeps_late_result_cached_without_applying_it() {
        let mut worker = LineDiffCoordinator::new(1024 * 1024);
        let stale_key = key("a\n", "b\n");
        worker.current_key = Some(stale_key);
        worker.skip(LineDiffUnavailable::Binary);
        assert!(!worker.accept_result(ResultMessage {
            key: stale_key,
            rows: Arc::from([DiffRow::Removed { old: LineIndex(0) }]),
            hunk_starts: Arc::from([0]),
        }));
        assert!(matches!(
            worker.state(),
            AsyncState::Skipped(LineDiffUnavailable::Binary)
        ));
        assert!(worker.is_cached(&stale_key));
    }

    #[test]
    fn a_to_b_to_a_deduplicates_running_a_and_drops_queued_b() {
        let a = key("a\n", "b\n");
        let b = key("x\n", "y\n");
        let mut slot = Slot {
            queued: Some(Job {
                request_id: RequestId(8),
                key: b,
                old: text("x\n"),
                new: text("y\n"),
            }),
            running: Some((a, RequestId(7))),
            stopped: false,
        };
        let outcome = slot.enqueue(Job {
            request_id: RequestId(9),
            key: a,
            old: text("a\n"),
            new: text("b\n"),
        });
        assert!(matches!(outcome, Enqueue::Existing(RequestId(7))));
        assert!(slot.queued.is_none());
    }

    #[test]
    fn latest_queued_job_replaces_the_previous_one() {
        let a = key("a\n", "b\n");
        let b = key("x\n", "y\n");
        let mut slot = Slot {
            queued: None,
            running: None,
            stopped: false,
        };
        assert!(matches!(
            slot.enqueue(Job {
                request_id: RequestId(1),
                key: a,
                old: text("a\n"),
                new: text("b\n"),
            }),
            Enqueue::Queued(RequestId(1))
        ));
        assert!(matches!(
            slot.enqueue(Job {
                request_id: RequestId(2),
                key: b,
                old: text("x\n"),
                new: text("y\n"),
            }),
            Enqueue::Queued(RequestId(2))
        ));
        assert_eq!(slot.queued.as_ref().map(|job| job.key), Some(b));
    }
}
