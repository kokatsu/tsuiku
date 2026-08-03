//! Background structural-diff computation and result coordination.
//!
//! Difftastic is deliberately serialized: one subprocess may run while at
//! most one newer request waits. Completed stale work still enters the cache,
//! but only the exact cache key selected by the UI may become visible.
//!
//! The version probe runs as the worker's first job rather than on the
//! terminal thread, so a difft that hangs cannot stall startup. Until the
//! version is known no cache key can be built, so at most one request is held
//! as `deferred` and dispatched once the answer arrives — the full-key match
//! contract is unchanged, only postponed.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::asyncstate::{
    AsyncState, NORMALIZER_VERSION, RequestId, StructuralDiffCacheKey, StructuralDiffState,
    StructuralError, StructuralSkip,
};
use crate::cache::WeightedLru;
use crate::ids::{ContentIdentity, ContentPairId};
use crate::structural::normalize::{StructuralOverlay, normalize};
use crate::structural::runner::{CancelFlag, DifftRunner};
use crate::structural::tempfiles::{LanguagePathHint, materialize};
use crate::text::TextContent;

/// Starting limits taken from measured difft runs: a pair costs roughly 30ms
/// flat and grows slowly with input (about 60ms at 1,000 lines, 390ms at
/// 10,000), so a few thousand lines stays well inside the five second timeout
/// while pathological inputs are kept out.
pub const MAX_STRUCTURAL_LINES: usize = 5_000;
pub const MAX_STRUCTURAL_BYTES: usize = 2 * 1024 * 1024;

/// Size guard applied before any difft work is scheduled. Configuration may
/// move these within clamped bounds; the defaults are the measured values.
#[derive(Clone, Copy, Debug)]
pub struct StructuralLimits {
    pub max_bytes: usize,
    pub max_lines: usize,
}

impl Default for StructuralLimits {
    fn default() -> Self {
        Self {
            max_bytes: MAX_STRUCTURAL_BYTES,
            max_lines: MAX_STRUCTURAL_LINES,
        }
    }
}

/// A timeout is usually a property of the input, but it can also be transient
/// load, so it expires rather than sticking for the session.
const TIMED_OUT_BACKOFF: Duration = Duration::from_secs(30);
/// A non-zero exit can be a signal or a resource shortage, so it retries
/// sooner than a timeout.
const PROCESS_FAILED_BACKOFF: Duration = Duration::from_secs(5);
/// How long shutdown waits for the worker to notice cancellation, kill its
/// child and unwind. Past this the thread is detached as a last resort.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// A request that has passed the cheap guards and is waiting either for the
/// difft version (to become a cache key) or for the worker.
struct PendingRequest {
    request_id: RequestId,
    pair: ContentPairId,
    old_path_hint: LanguagePathHint,
    new_path_hint: LanguagePathHint,
    old: Arc<TextContent>,
    new: Arc<TextContent>,
}

impl PendingRequest {
    fn cache_key(&self, version: String) -> StructuralDiffCacheKey {
        StructuralDiffCacheKey {
            pair: self.pair,
            old_path_hint: self.old_path_hint.clone(),
            new_path_hint: self.new_path_hint.clone(),
            difft_version: version,
            normalizer_version: NORMALIZER_VERSION,
            options_fingerprint: 0,
        }
    }
}

struct Job {
    request_id: RequestId,
    key: StructuralDiffCacheKey,
    old: Arc<TextContent>,
    new: Arc<TextContent>,
}

struct Slot {
    queued: Option<Job>,
    running: Option<(StructuralDiffCacheKey, RequestId)>,
    stopped: bool,
}

enum Enqueue {
    Queued(RequestId),
    Existing(RequestId),
}

impl Slot {
    fn enqueue(&mut self, job: Job) -> Enqueue {
        if let Some((key, request_id)) = &self.running
            && key == &job.key
        {
            self.queued = None;
            return Enqueue::Existing(*request_id);
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

#[derive(Clone)]
enum CachedResult {
    Ready(Arc<StructuralOverlay>),
    Failed(StructuralError),
}

enum WorkerMessage {
    /// The worker's first message: the validated difft version, or why there
    /// is none.
    Version(Result<String, StructuralError>),
    Result {
        key: StructuralDiffCacheKey,
        result: Result<Arc<StructuralOverlay>, StructuralError>,
    },
}

/// Runs guarded difftastic processes away from the terminal thread.
pub struct StructuralDiffCoordinator {
    slot: Arc<(Mutex<Slot>, Condvar)>,
    results: mpsc::Receiver<WorkerMessage>,
    thread: Option<JoinHandle<()>>,
    cancel: CancelFlag,
    limits: StructuralLimits,
    cache: WeightedLru<StructuralDiffCacheKey, CachedResult>,
    /// `None` until the worker reports the version probe result.
    version: Option<Result<String, StructuralError>>,
    /// Held only while the version is unknown; the newest request wins.
    deferred: Option<PendingRequest>,
    /// Failures worth retrying later, with the earliest retry time.
    backoff: HashMap<StructuralDiffCacheKey, (Instant, StructuralError)>,
    current_key: Option<StructuralDiffCacheKey>,
    state: StructuralDiffState,
    next_request_id: u64,
    diagnostics_total: u64,
    diagnostics_accepted: u64,
}

impl StructuralDiffCoordinator {
    pub fn new(cache_capacity: usize) -> Self {
        Self::with_runner(cache_capacity, DifftRunner::default())
    }

    pub fn with_runner(cache_capacity: usize, runner: DifftRunner) -> Self {
        Self::with_runner_and_limits(cache_capacity, runner, StructuralLimits::default())
    }

    pub fn with_runner_and_limits(
        cache_capacity: usize,
        mut runner: DifftRunner,
        limits: StructuralLimits,
    ) -> Self {
        let cancel = CancelFlag::default();
        runner.cancel = cancel.clone();
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
            .name("tsuiku-structural".into())
            .spawn(move || worker_loop(runner, worker_slot, tx))
            .expect("structural-diff worker thread must start");
        Self {
            slot,
            results,
            thread: Some(thread),
            cancel,
            cache: WeightedLru::new(cache_capacity),
            version: None,
            deferred: None,
            backoff: HashMap::new(),
            current_key: None,
            state: AsyncState::NotRequested,
            limits,
            next_request_id: 1,
            diagnostics_total: 0,
            diagnostics_accepted: 0,
        }
    }

    pub fn request(
        &mut self,
        pair: ContentPairId,
        old_path_hint: LanguagePathHint,
        new_path_hint: LanguagePathHint,
        old: Arc<TextContent>,
        new: Arc<TextContent>,
    ) {
        self.poll();
        // Nothing from the previous selection may stay applicable: a result
        // is only shown while its key is the current one, and only the
        // already-running job survives a replacement.
        self.clear_request();

        if matches!(pair.old, ContentIdentity::Absent)
            || matches!(pair.new, ContentIdentity::Absent)
        {
            self.state = AsyncState::Skipped(StructuralSkip::OneSided);
            return;
        }
        // Cheap, deterministic and evaluated before any key exists, so there
        // is nothing worth caching: a repeat request recomputes it in O(1).
        let bytes = old.bytes.len().saturating_add(new.bytes.len());
        let lines = old.lines.len().max(new.lines.len());
        if bytes > self.limits.max_bytes || lines > self.limits.max_lines {
            self.state = AsyncState::Skipped(StructuralSkip::SizeLimited);
            return;
        }

        let request_id = RequestId(self.next_request_id);
        self.next_request_id += 1;
        let request = PendingRequest {
            request_id,
            pair,
            old_path_hint,
            new_path_hint,
            old,
            new,
        };
        match self.version.clone() {
            Some(version) => self.dispatch(request, version),
            None => {
                self.state = AsyncState::Pending { request_id };
                self.deferred = Some(request);
            }
        }
    }

    /// Turn a request into a cache key and either answer it from what we
    /// already know or hand it to the worker.
    fn dispatch(&mut self, request: PendingRequest, version: Result<String, StructuralError>) {
        let version = match version {
            Ok(version) => version,
            Err(StructuralError::ToolNotFound) => {
                self.state = AsyncState::Skipped(StructuralSkip::ToolUnavailable);
                return;
            }
            Err(StructuralError::InvalidSchema) => {
                self.state = AsyncState::Skipped(StructuralSkip::IncompatibleVersion);
                return;
            }
            Err(error) => {
                self.state = AsyncState::Failed(error);
                return;
            }
        };
        let key = request.cache_key(version);
        self.current_key = Some(key.clone());
        if let Some(cached) = self.cache.get_cloned(&key) {
            self.apply_cached(cached);
            return;
        }
        if let Some(error) = self.active_backoff(&key) {
            self.state = AsyncState::Failed(error);
            return;
        }

        let job = Job {
            request_id: request.request_id,
            key,
            old: request.old,
            new: request.new,
        };
        let (lock, wake) = &*self.slot;
        let mut slot = lock.lock().expect("structural queue lock poisoned");
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

    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(message) = self.results.try_recv() {
            match message {
                WorkerMessage::Version(version) => {
                    self.version = Some(version.clone());
                    if let Some(deferred) = self.deferred.take() {
                        self.dispatch(deferred, version);
                        changed = true;
                    }
                }
                WorkerMessage::Result { key, result } => {
                    changed |= self.accept_result(key, result);
                }
            }
        }
        if self.thread.as_ref().is_some_and(JoinHandle::is_finished)
            && matches!(self.state, AsyncState::Pending { .. })
        {
            self.state = AsyncState::Failed(StructuralError::Io);
            changed = true;
        }
        changed
    }

    /// Record a finished job and report whether it changed what is visible.
    fn accept_result(
        &mut self,
        key: StructuralDiffCacheKey,
        result: Result<Arc<StructuralOverlay>, StructuralError>,
    ) -> bool {
        let is_current = self.current_key.as_ref() == Some(&key);
        match result {
            Ok(overlay) => {
                self.diagnostics_total += u64::from(overlay.diagnostics.total);
                self.diagnostics_accepted += u64::from(overlay.diagnostics.accepted);
                let cached = CachedResult::Ready(overlay);
                self.cache_result(key, cached.clone());
                if is_current {
                    self.apply_cached(cached);
                }
            }
            Err(error) => {
                match &error {
                    // Deterministic for this exact content pair: never worth
                    // spawning difft for again this session.
                    StructuralError::InvalidJson
                    | StructuralError::InvalidSchema
                    | StructuralError::OutputTooLarge => {
                        self.cache_result(key, CachedResult::Failed(error.clone()));
                    }
                    StructuralError::TimedOut => {
                        self.remember_backoff(key, error.clone(), TIMED_OUT_BACKOFF);
                    }
                    StructuralError::ProcessFailed { .. } => {
                        self.remember_backoff(key, error.clone(), PROCESS_FAILED_BACKOFF);
                    }
                    // Environmental or shutdown: retried on the next visit.
                    _ => {}
                }
                if is_current {
                    self.state = AsyncState::Failed(error);
                }
            }
        }
        is_current
    }

    fn remember_backoff(
        &mut self,
        key: StructuralDiffCacheKey,
        error: StructuralError,
        window: Duration,
    ) {
        let now = Instant::now();
        self.backoff.retain(|_, (until, _)| *until > now);
        self.backoff.insert(key, (now + window, error));
    }

    /// The failure still suppressing this key, dropping it once it expires.
    fn active_backoff(&mut self, key: &StructuralDiffCacheKey) -> Option<StructuralError> {
        let (until, error) = self.backoff.get(key)?;
        if *until > Instant::now() {
            return Some(error.clone());
        }
        self.backoff.remove(key);
        None
    }

    fn cache_result(&mut self, key: StructuralDiffCacheKey, result: CachedResult) {
        let overlay_weight = match &result {
            CachedResult::Ready(overlay) => {
                overlay.language.len()
                    + (overlay.old.spans().len() + overlay.new.spans().len())
                        * std::mem::size_of::<crate::structural::normalize::LineSpan>()
            }
            CachedResult::Failed(_) => 0,
        };
        self.cache.insert(
            key,
            result,
            std::mem::size_of::<StructuralDiffCacheKey>() + overlay_weight,
        );
    }

    fn apply_cached(&mut self, result: CachedResult) {
        self.state = match result {
            CachedResult::Ready(overlay) => AsyncState::Ready(overlay),
            CachedResult::Failed(error) => AsyncState::Failed(error),
        };
    }

    /// Drop everything that ties the coordinator to the previous selection.
    fn clear_request(&mut self) {
        self.current_key = None;
        self.deferred = None;
        if let Ok(mut slot) = self.slot.0.lock() {
            slot.queued = None;
        }
    }

    pub fn reset(&mut self) {
        self.poll();
        self.clear_request();
        self.state = AsyncState::NotRequested;
    }

    pub fn state(&self) -> &StructuralDiffState {
        &self.state
    }

    pub fn cache_weight(&self) -> usize {
        self.cache.total_weight()
    }

    pub fn diagnostic_totals(&self) -> (u64, u64) {
        (self.diagnostics_accepted, self.diagnostics_total)
    }
}

impl Drop for StructuralDiffCoordinator {
    fn drop(&mut self) {
        // The worker owns a difft child and its temp directory, neither of
        // which is cleaned up by process exit. Cancelling makes the runner
        // kill and reap the group, after which the worker unwinds normally
        // and its destructors remove the temp files.
        self.cancel.cancel();
        let (lock, wake) = &*self.slot;
        if let Ok(mut slot) = lock.lock() {
            slot.stopped = true;
            slot.queued = None;
            wake.notify_one();
        }
        let Some(thread) = self.thread.take() else {
            return;
        };
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        while !thread.is_finished() {
            if Instant::now() >= deadline {
                // Detach rather than block quitting any further.
                return;
            }
            thread::sleep(SHUTDOWN_POLL_INTERVAL);
        }
        let _ = thread.join();
    }
}

fn validate_version(version: String) -> Result<String, StructuralError> {
    let number = version
        .strip_prefix("Difftastic ")
        .and_then(|rest| rest.split_whitespace().next())
        .ok_or(StructuralError::InvalidSchema)?;
    let mut parts = number.split('.');
    let major = parts.next().and_then(|part| part.parse::<u64>().ok());
    let minor = parts.next().and_then(|part| part.parse::<u64>().ok());
    match (major, minor) {
        (Some(0), Some(minor)) if minor >= 69 => Ok(version),
        (Some(major), Some(_)) if major >= 1 => Ok(version),
        _ => Err(StructuralError::InvalidSchema),
    }
}

fn worker_loop(
    runner: DifftRunner,
    slot: Arc<(Mutex<Slot>, Condvar)>,
    tx: mpsc::Sender<WorkerMessage>,
) {
    let version = runner.version().and_then(validate_version);
    if tx.send(WorkerMessage::Version(version)).is_err() {
        return;
    }

    loop {
        let job = {
            let (lock, wake) = &*slot;
            let mut state = lock.lock().expect("structural queue lock poisoned");
            while state.queued.is_none() && !state.stopped {
                state = wake.wait(state).expect("structural queue lock poisoned");
            }
            if state.stopped {
                return;
            }
            let job = state.queued.take().expect("checked above");
            state.running = Some((job.key.clone(), job.request_id));
            job
        };

        let result = materialize(
            &job.old.bytes,
            &job.new.bytes,
            &job.key.old_path_hint,
            &job.key.new_path_hint,
        )
        .map_err(|_| StructuralError::Io)
        .and_then(|pair| runner.run(&pair.old_path, &pair.new_path))
        .map(|raw| Arc::new(normalize(&raw, Some(&job.old), Some(&job.new))));
        if tx
            .send(WorkerMessage::Result {
                key: job.key.clone(),
                result,
            })
            .is_err()
        {
            return;
        }
        let (lock, _) = &*slot;
        if let Ok(mut state) = lock.lock()
            && state
                .running
                .as_ref()
                .is_some_and(|(key, _)| key == &job.key)
        {
            state.running = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ContentId;
    use std::path::PathBuf;

    /// A coordinator whose worker can never run anything: these tests drive
    /// the backoff bookkeeping directly with windows short enough to expire.
    fn coordinator() -> StructuralDiffCoordinator {
        StructuralDiffCoordinator::with_runner(
            1024,
            DifftRunner {
                binary: PathBuf::from("/nonexistent/difft-for-backoff-tests"),
                ..DifftRunner::default()
            },
        )
    }

    fn key(fingerprint: u64) -> StructuralDiffCacheKey {
        StructuralDiffCacheKey {
            pair: ContentPairId {
                old: ContentIdentity::Present(ContentId::compute(b"old")),
                new: ContentIdentity::Present(ContentId::compute(b"new")),
            },
            old_path_hint: LanguagePathHint::none(),
            new_path_hint: LanguagePathHint::none(),
            difft_version: "Difftastic 0.69.0".to_owned(),
            normalizer_version: NORMALIZER_VERSION,
            options_fingerprint: fingerprint,
        }
    }

    #[test]
    fn a_backoff_suppresses_retries_only_until_its_window_expires() {
        let mut coordinator = coordinator();
        let key = key(0);
        let window = Duration::from_millis(20);

        coordinator.remember_backoff(key.clone(), StructuralError::TimedOut, window);
        assert!(matches!(
            coordinator.active_backoff(&key),
            Some(StructuralError::TimedOut)
        ));

        thread::sleep(window * 2);
        assert!(coordinator.active_backoff(&key).is_none());
        assert!(
            coordinator.backoff.is_empty(),
            "an expired entry must be dropped once it is looked up"
        );
    }

    #[test]
    fn remembering_a_backoff_prunes_expired_entries() {
        // Nothing else removes entries for keys that are never revisited.
        let mut coordinator = coordinator();
        let expired = key(0);
        let live = key(1);

        coordinator.remember_backoff(
            expired.clone(),
            StructuralError::TimedOut,
            Duration::from_millis(10),
        );
        thread::sleep(Duration::from_millis(30));
        coordinator.remember_backoff(
            live.clone(),
            StructuralError::ProcessFailed { exit_code: Some(1) },
            Duration::from_secs(30),
        );

        assert!(!coordinator.backoff.contains_key(&expired));
        assert!(coordinator.backoff.contains_key(&live));
    }

    #[test]
    fn version_policy_accepts_observed_and_newer_versions() {
        assert!(validate_version("Difftastic 0.69.0".into()).is_ok());
        assert!(validate_version("Difftastic 0.70.0".into()).is_ok());
        assert!(validate_version("Difftastic 1.0.0".into()).is_ok());
        assert!(validate_version("Difftastic 0.68.0".into()).is_err());
        assert!(validate_version("unexpected".into()).is_err());
    }
}
