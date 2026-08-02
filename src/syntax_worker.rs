//! Background syntax-highlight computation and result coordination.
//!
//! Same single-running + one-queued pattern as the other coordinators, but
//! the cache unit is one *side* (keyed by `ContentId`) while the job unit is
//! a *batch* of the selected pair's uncached sides. Enqueueing sides
//! individually would let the second side replace the first in the single
//! queued slot, leaving one side permanently pending; a batch keeps the
//! replacement semantics and still fills both sides from one result.
//!
//! syntect's syntax and theme sets load lazily on the worker thread, so the
//! terminal thread never pays their initialization cost.

use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::{self, JoinHandle};

use crate::asyncstate::{
    AsyncState, RequestId, SyntaxError, SyntaxHighlightCacheKey, SyntaxHighlightState, SyntaxSkip,
};
use crate::cache::WeightedLru;
use crate::syntax::{HighlightAssets, HighlightOutcome, SyntaxSpans};
use crate::text::TextContent;

/// Same starting guards as the structural layer, applied per side.
pub const MAX_SYNTAX_LINES: usize = 5_000;
pub const MAX_SYNTAX_BYTES: usize = 2 * 1024 * 1024;

/// One side the UI wants highlighted.
pub struct SideRequest {
    pub key: SyntaxHighlightCacheKey,
    pub text: Arc<TextContent>,
}

struct BatchJob {
    request_id: RequestId,
    sides: Vec<(SyntaxHighlightCacheKey, Arc<TextContent>)>,
}

struct Slot {
    queued: Option<BatchJob>,
    running: Option<(Vec<SyntaxHighlightCacheKey>, RequestId)>,
    stopped: bool,
}

enum Enqueue {
    Queued(RequestId),
    Existing(RequestId),
}

impl Slot {
    fn enqueue(&mut self, job: BatchJob) -> Enqueue {
        if let Some((keys, request_id)) = &self.running
            && job.sides.iter().all(|(key, _)| keys.contains(key))
        {
            self.queued = None;
            return Enqueue::Existing(*request_id);
        }
        if let Some(queued) = &self.queued
            && job
                .sides
                .iter()
                .all(|(key, _)| queued.sides.iter().any(|(queued_key, _)| queued_key == key))
        {
            return Enqueue::Existing(queued.request_id);
        }
        let request_id = job.request_id;
        self.queued = Some(job);
        Enqueue::Queued(request_id)
    }
}

/// Completed work for one side. `Failed` is not cached so a transient
/// highlighting error is retried on the next visit.
enum SideOutcome {
    Ready(Arc<SyntaxSpans>),
    Unsupported,
    Failed,
}

#[derive(Clone)]
enum CachedResult {
    Ready(Arc<SyntaxSpans>),
    Unsupported,
}

struct ResultMessage {
    sides: Vec<(SyntaxHighlightCacheKey, SideOutcome)>,
}

/// Runs syntect highlighting off the terminal thread, one side-batch at a
/// time, and caches completed sides by content identity.
pub struct SyntaxHighlightCoordinator {
    slot: Arc<(Mutex<Slot>, Condvar)>,
    results: mpsc::Receiver<ResultMessage>,
    thread: Option<JoinHandle<()>>,
    cache: WeightedLru<SyntaxHighlightCacheKey, CachedResult>,
    current_old: Option<SyntaxHighlightCacheKey>,
    current_new: Option<SyntaxHighlightCacheKey>,
    old_state: SyntaxHighlightState,
    new_state: SyntaxHighlightState,
    next_request_id: u64,
}

impl SyntaxHighlightCoordinator {
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
            .name("tsuiku-syntax".into())
            .spawn(move || worker_loop(worker_slot, tx))
            .expect("syntax-highlight worker thread must start");
        Self {
            slot,
            results,
            thread: Some(thread),
            cache: WeightedLru::new(cache_capacity),
            current_old: None,
            current_new: None,
            old_state: AsyncState::NotRequested,
            new_state: AsyncState::NotRequested,
            next_request_id: 1,
        }
    }

    /// Makes the given sides the highlight the UI currently wants. `None`
    /// means that side is absent (add/delete) and stays `NotRequested`.
    pub fn request(&mut self, old: Option<SideRequest>, new: Option<SideRequest>) {
        self.poll();
        self.clear_request();

        let mut batch: Vec<(SyntaxHighlightCacheKey, Arc<TextContent>)> = Vec::new();
        let request_id = RequestId(self.next_request_id);

        let old_state = Self::admit(&mut self.cache, &mut self.current_old, old, &mut batch);
        let new_state = Self::admit(&mut self.cache, &mut self.current_new, new, &mut batch);
        self.old_state = old_state.unwrap_or(AsyncState::NotRequested);
        self.new_state = new_state.unwrap_or(AsyncState::NotRequested);

        if batch.is_empty() {
            return;
        }
        self.next_request_id += 1;
        let job = BatchJob {
            request_id,
            sides: batch,
        };
        let (lock, wake) = &*self.slot;
        let mut slot = lock.lock().expect("syntax queue lock poisoned");
        let effective_id = match slot.enqueue(job) {
            Enqueue::Queued(id) => {
                wake.notify_one();
                id
            }
            Enqueue::Existing(id) => id,
        };
        drop(slot);
        let pending = AsyncState::Pending {
            request_id: effective_id,
        };
        if matches!(self.old_state, AsyncState::Pending { .. }) {
            self.old_state = pending.clone();
        }
        if matches!(self.new_state, AsyncState::Pending { .. }) {
            self.new_state = pending;
        }
    }

    /// Guard, cache-probe and current-key bookkeeping for one side. A side
    /// that needs worker time is added to `batch` and reported as `Pending`
    /// (its request id is patched once the batch is enqueued).
    fn admit(
        cache: &mut WeightedLru<SyntaxHighlightCacheKey, CachedResult>,
        current: &mut Option<SyntaxHighlightCacheKey>,
        side: Option<SideRequest>,
        batch: &mut Vec<(SyntaxHighlightCacheKey, Arc<TextContent>)>,
    ) -> Option<SyntaxHighlightState> {
        let side = side?;
        if side.text.bytes.len() > MAX_SYNTAX_BYTES || side.text.lines.len() > MAX_SYNTAX_LINES {
            return Some(AsyncState::Skipped(SyntaxSkip::SizeLimited));
        }
        *current = Some(side.key.clone());
        if let Some(cached) = cache.get_cloned(&side.key) {
            return Some(apply_cached(cached));
        }
        // Old and new sides of an unmodified-mode pair can share a key.
        if !batch.iter().any(|(key, _)| key == &side.key) {
            batch.push((side.key, side.text));
        }
        Some(AsyncState::Pending {
            request_id: RequestId(0),
        })
    }

    /// Drains completed work without waiting. Every completed side enters the
    /// cache; only sides matching a current key change visible state.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(message) = self.results.try_recv() {
            for (key, outcome) in message.sides {
                changed |= self.accept_side(key, outcome);
            }
        }
        if self.thread.as_ref().is_some_and(JoinHandle::is_finished) {
            for state in [&mut self.old_state, &mut self.new_state] {
                if matches!(state, AsyncState::Pending { .. }) {
                    *state = AsyncState::Failed(SyntaxError::WorkerGone);
                    changed = true;
                }
            }
        }
        changed
    }

    fn accept_side(&mut self, key: SyntaxHighlightCacheKey, outcome: SideOutcome) -> bool {
        let cached = match outcome {
            SideOutcome::Ready(spans) => Some(CachedResult::Ready(spans)),
            SideOutcome::Unsupported => Some(CachedResult::Unsupported),
            SideOutcome::Failed => None,
        };
        if let Some(cached) = &cached {
            let weight = std::mem::size_of::<SyntaxHighlightCacheKey>()
                + match cached {
                    CachedResult::Ready(spans) => spans.estimated_bytes(),
                    CachedResult::Unsupported => 0,
                };
            self.cache.insert(key.clone(), cached.clone(), weight);
        }
        let state = match cached {
            Some(cached) => apply_cached(cached),
            None => AsyncState::Failed(SyntaxError::HighlightFailed),
        };
        let mut changed = false;
        if self.current_old.as_ref() == Some(&key) {
            self.old_state = state.clone();
            changed = true;
        }
        if self.current_new.as_ref() == Some(&key) {
            self.new_state = state;
            changed = true;
        }
        changed
    }

    fn clear_request(&mut self) {
        self.current_old = None;
        self.current_new = None;
        if let Ok(mut slot) = self.slot.0.lock() {
            slot.queued = None;
        }
    }

    /// Returns to the initial state with no requested highlight.
    pub fn reset(&mut self) {
        self.poll();
        self.clear_request();
        self.old_state = AsyncState::NotRequested;
        self.new_state = AsyncState::NotRequested;
    }

    pub fn old_state(&self) -> &SyntaxHighlightState {
        &self.old_state
    }

    pub fn new_state(&self) -> &SyntaxHighlightState {
        &self.new_state
    }

    pub fn cache_weight(&self) -> usize {
        self.cache.total_weight()
    }
}

fn apply_cached(cached: CachedResult) -> SyntaxHighlightState {
    match cached {
        CachedResult::Ready(spans) => AsyncState::Ready(spans),
        CachedResult::Unsupported => AsyncState::Skipped(SyntaxSkip::UnsupportedLanguage),
    }
}

impl Drop for SyntaxHighlightCoordinator {
    fn drop(&mut self) {
        let (lock, wake) = &*self.slot;
        if let Ok(mut slot) = lock.lock() {
            slot.stopped = true;
            slot.queued = None;
            wake.notify_one();
        }
        // Detach: quitting must not wait for a running whole-file highlight.
        self.thread.take();
    }
}

fn worker_loop(slot: Arc<(Mutex<Slot>, Condvar)>, tx: mpsc::Sender<ResultMessage>) {
    // Loaded here so the cost (a few ms) lands on this thread, before the
    // first job rather than during terminal startup.
    let assets = HighlightAssets::load();
    loop {
        let job = {
            let (lock, wake) = &*slot;
            let mut state = lock.lock().expect("syntax queue lock poisoned");
            while state.queued.is_none() && !state.stopped {
                state = wake.wait(state).expect("syntax queue lock poisoned");
            }
            if state.stopped {
                return;
            }
            let job = state.queued.take().expect("checked above");
            state.running = Some((
                job.sides.iter().map(|(key, _)| key.clone()).collect(),
                job.request_id,
            ));
            job
        };

        let sides = job
            .sides
            .into_iter()
            .map(|(key, text)| {
                let outcome = match assets.highlight(&text, &key.language_hint, key.theme_id) {
                    HighlightOutcome::Ready(spans) => SideOutcome::Ready(spans),
                    HighlightOutcome::UnsupportedLanguage => SideOutcome::Unsupported,
                    HighlightOutcome::Failed => SideOutcome::Failed,
                };
                (key, outcome)
            })
            .collect();
        if tx.send(ResultMessage { sides }).is_err() {
            return;
        }
        let (lock, _) = &*slot;
        if let Ok(mut state) = lock.lock()
            && state
                .running
                .as_ref()
                .is_some_and(|(_, id)| *id == job.request_id)
        {
            state.running = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asyncstate::HIGHLIGHTER_VERSION;
    use crate::ids::ContentId;
    use crate::path::GitPath;
    use crate::structural::tempfiles::LanguagePathHint;
    use crate::syntax::{DEFAULT_THEME, ThemeId};
    use crate::text::{ClassifiedContent, classify};
    use std::time::{Duration, Instant};

    fn text(source: &str) -> Arc<TextContent> {
        match classify(Arc::from(source.as_bytes())) {
            ClassifiedContent::Text(t) => Arc::new(t),
            ClassifiedContent::Binary(_) => panic!("fixture must be text"),
        }
    }

    fn side(source: &str, path: &[u8], theme: ThemeId) -> SideRequest {
        SideRequest {
            key: SyntaxHighlightCacheKey {
                content: ContentId::compute(source.as_bytes()),
                language_hint: LanguagePathHint::from_git_path(&GitPath::from_bytes(path)),
                theme_id: theme,
                highlighter_version: HIGHLIGHTER_VERSION,
                options_fingerprint: 0,
            },
            text: text(source),
        }
    }

    fn wait_both_settled(worker: &mut SyntaxHighlightCoordinator) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            worker.poll();
            let pending = |s: &SyntaxHighlightState| matches!(s, AsyncState::Pending { .. });
            if !pending(worker.old_state()) && !pending(worker.new_state()) {
                return;
            }
            assert!(Instant::now() < deadline, "highlight did not finish");
            thread::yield_now();
        }
    }

    const OLD_RS: &str = "fn old() -> u32 { 1 } // note\n";
    const NEW_RS: &str = "fn new() -> u32 { 2 } // note\n";

    #[test]
    fn one_batch_fills_both_sides_and_revisits_hit_the_cache() {
        let mut worker = SyntaxHighlightCoordinator::new(1024 * 1024);
        worker.request(
            Some(side(OLD_RS, b"a.rs", DEFAULT_THEME)),
            Some(side(NEW_RS, b"a.rs", DEFAULT_THEME)),
        );
        wait_both_settled(&mut worker);
        assert!(matches!(worker.old_state(), AsyncState::Ready(_)));
        assert!(matches!(worker.new_state(), AsyncState::Ready(_)));

        worker.reset();
        assert!(matches!(worker.old_state(), AsyncState::NotRequested));
        worker.request(
            Some(side(OLD_RS, b"a.rs", DEFAULT_THEME)),
            Some(side(NEW_RS, b"a.rs", DEFAULT_THEME)),
        );
        // Cached: both sides are Ready without another worker round trip.
        assert!(matches!(worker.old_state(), AsyncState::Ready(_)));
        assert!(matches!(worker.new_state(), AsyncState::Ready(_)));
    }

    #[test]
    fn one_sided_pair_leaves_the_absent_side_not_requested() {
        let mut worker = SyntaxHighlightCoordinator::new(1024 * 1024);
        worker.request(None, Some(side(NEW_RS, b"a.rs", DEFAULT_THEME)));
        wait_both_settled(&mut worker);
        assert!(matches!(worker.old_state(), AsyncState::NotRequested));
        assert!(matches!(worker.new_state(), AsyncState::Ready(_)));
    }

    #[test]
    fn unsupported_language_is_reported_and_cached() {
        let mut worker = SyntaxHighlightCoordinator::new(1024 * 1024);
        worker.request(None, Some(side("plain\n", b"a.zzznope", DEFAULT_THEME)));
        wait_both_settled(&mut worker);
        assert!(matches!(
            worker.new_state(),
            AsyncState::Skipped(SyntaxSkip::UnsupportedLanguage)
        ));

        worker.reset();
        worker.request(None, Some(side("plain\n", b"a.zzznope", DEFAULT_THEME)));
        assert!(matches!(
            worker.new_state(),
            AsyncState::Skipped(SyntaxSkip::UnsupportedLanguage)
        ));
    }

    #[test]
    fn oversized_side_is_skipped_without_touching_the_worker() {
        let mut worker = SyntaxHighlightCoordinator::new(1024 * 1024);
        let big = "x\n".repeat(MAX_SYNTAX_LINES + 1);
        worker.request(
            Some(side(&big, b"a.rs", DEFAULT_THEME)),
            Some(side(NEW_RS, b"a.rs", DEFAULT_THEME)),
        );
        assert!(matches!(
            worker.old_state(),
            AsyncState::Skipped(SyntaxSkip::SizeLimited)
        ));
        wait_both_settled(&mut worker);
        assert!(matches!(worker.new_state(), AsyncState::Ready(_)));
    }

    #[test]
    fn stale_result_is_cached_but_not_applied() {
        let mut worker = SyntaxHighlightCoordinator::new(1024 * 1024);
        let stale = side(OLD_RS, b"a.rs", DEFAULT_THEME);
        let stale_key = stale.key.clone();
        worker.current_new = Some(side(NEW_RS, b"a.rs", DEFAULT_THEME).key);
        worker.new_state = AsyncState::Pending {
            request_id: RequestId(1),
        };

        let applied = worker.accept_side(
            stale_key.clone(),
            SideOutcome::Ready(Arc::new(SyntaxSpans::default())),
        );

        assert!(!applied);
        assert!(matches!(worker.new_state(), AsyncState::Pending { .. }));
        assert!(worker.cache.contains_key(&stale_key));
    }

    #[test]
    fn a_different_theme_id_never_reuses_cached_spans() {
        let mut worker = SyntaxHighlightCoordinator::new(1024 * 1024);
        worker.request(None, Some(side(NEW_RS, b"a.rs", ThemeId(0))));
        wait_both_settled(&mut worker);
        assert!(matches!(worker.new_state(), AsyncState::Ready(_)));

        worker.reset();
        worker.request(None, Some(side(NEW_RS, b"a.rs", ThemeId(1))));
        // Same content, different theme: must go back to the worker.
        assert!(matches!(worker.new_state(), AsyncState::Pending { .. }));
        wait_both_settled(&mut worker);
        assert!(matches!(worker.new_state(), AsyncState::Ready(_)));
    }

    #[test]
    fn identical_sides_collapse_into_one_batch_entry() {
        let mut batch = Vec::new();
        let mut cache = WeightedLru::new(1024);
        let mut current_old = None;
        let mut current_new = None;
        let old = SyntaxHighlightCoordinator::admit(
            &mut cache,
            &mut current_old,
            Some(side(OLD_RS, b"a.rs", DEFAULT_THEME)),
            &mut batch,
        );
        let new = SyntaxHighlightCoordinator::admit(
            &mut cache,
            &mut current_new,
            Some(side(OLD_RS, b"a.rs", DEFAULT_THEME)),
            &mut batch,
        );
        assert!(matches!(old, Some(AsyncState::Pending { .. })));
        assert!(matches!(new, Some(AsyncState::Pending { .. })));
        assert_eq!(batch.len(), 1);
    }
}
