//! Debounced aggregation of watch events.
//!
//! The debouncer holds one open [`EventBatch`]. Every observed event is
//! absorbed into it and pushes the deadline out to `window` after that
//! event; the batch is released only once the stream has been quiet for a
//! full window. Time is passed in explicitly so tests need no sleeping.

use std::time::{Duration, Instant};

use super::{EventBatch, WatchEvent};

/// Starting value; tuned against real editors and build tools later.
pub const DEBOUNCE_WINDOW: Duration = Duration::from_millis(100);

pub struct Debouncer {
    window: Duration,
    pending: Option<(EventBatch, Instant)>,
}

impl Debouncer {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            pending: None,
        }
    }

    /// Absorb one event and extend the deadline from its arrival time.
    pub fn observe(&mut self, event: WatchEvent, now: Instant) {
        let deadline = now + self.window;
        match &mut self.pending {
            Some((batch, current)) => {
                batch.absorb(event);
                *current = deadline;
            }
            None => {
                let mut batch = EventBatch::default();
                batch.absorb(event);
                self.pending = Some((batch, deadline));
            }
        }
    }

    /// Release the aggregated batch once the window has expired.
    pub fn due(&mut self, now: Instant) -> Option<EventBatch> {
        if self.pending.as_ref().is_some_and(|(_, at)| *at <= now) {
            return self.pending.take().map(|(batch, _)| batch);
        }
        None
    }

    /// The instant the current batch becomes due, for event-loop timeouts.
    pub fn deadline(&self) -> Option<Instant> {
        self.pending.as_ref().map(|(_, at)| *at)
    }
}

impl Default for Debouncer {
    fn default() -> Self {
        Self::new(DEBOUNCE_WINDOW)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path::GitPath;

    fn path(p: &[u8]) -> GitPath {
        GitPath::from_bytes(p)
    }

    fn worktree(p: &[u8]) -> WatchEvent {
        WatchEvent::Worktree { path: path(p) }
    }

    #[test]
    fn a_window_aggregates_paths_and_flags_instead_of_keeping_the_last_event() {
        let mut debouncer = Debouncer::new(Duration::from_millis(100));
        let t0 = Instant::now();
        // The order that broke last-event-wins debouncing: the selected file
        // changes first, another file changes second.
        debouncer.observe(worktree(b"selected.rs"), t0);
        debouncer.observe(worktree(b"other.rs"), t0 + Duration::from_millis(10));
        debouncer.observe(WatchEvent::GitMetadata, t0 + Duration::from_millis(20));

        let batch = debouncer
            .due(t0 + Duration::from_millis(130))
            .expect("window expired");
        assert_eq!(
            batch.paths,
            [path(b"selected.rs"), path(b"other.rs")]
                .into_iter()
                .collect()
        );
        assert!(batch.git_metadata);
        assert!(!batch.ignore_source);
        assert!(batch.affects_selection(Some(&path(b"selected.rs")), None));
    }

    #[test]
    fn each_event_extends_the_deadline() {
        let mut debouncer = Debouncer::new(Duration::from_millis(100));
        let t0 = Instant::now();
        debouncer.observe(worktree(b"a"), t0);
        debouncer.observe(worktree(b"b"), t0 + Duration::from_millis(80));

        assert!(debouncer.due(t0 + Duration::from_millis(120)).is_none());
        assert_eq!(debouncer.deadline(), Some(t0 + Duration::from_millis(180)));
        assert!(debouncer.due(t0 + Duration::from_millis(180)).is_some());
        assert!(debouncer.deadline().is_none(), "released batches reset it");
    }

    #[test]
    fn overflow_is_sticky_within_the_window() {
        let mut debouncer = Debouncer::new(Duration::from_millis(100));
        let t0 = Instant::now();
        debouncer.observe(WatchEvent::Overflow, t0);
        debouncer.observe(worktree(b"a"), t0 + Duration::from_millis(10));

        let batch = debouncer
            .due(t0 + Duration::from_millis(200))
            .expect("window expired");
        assert!(batch.overflow);
        assert!(batch.lossy());
        assert!(
            batch.affects_selection(None, Some(&path(b"unrelated"))),
            "possible event loss must force a re-read"
        );
    }

    #[test]
    fn duplicate_paths_collapse() {
        let mut debouncer = Debouncer::new(Duration::from_millis(100));
        let t0 = Instant::now();
        debouncer.observe(worktree(b"a"), t0);
        debouncer.observe(worktree(b"a"), t0 + Duration::from_millis(1));
        let batch = debouncer
            .due(t0 + Duration::from_millis(200))
            .expect("window expired");
        assert_eq!(batch.paths.len(), 1);
    }

    #[test]
    fn selection_relevance_is_conservative() {
        let selected = path(b"src/app.rs");
        let mut exact = EventBatch::default();
        exact.absorb(worktree(b"src/app.rs"));
        assert!(exact.affects_selection(None, Some(&selected)));

        let mut ancestor = EventBatch::default();
        ancestor.absorb(worktree(b"src"));
        assert!(
            ancestor.affects_selection(None, Some(&selected)),
            "an ancestor directory rename/remove touches the selection"
        );

        let mut sibling_prefix = EventBatch::default();
        sibling_prefix.absorb(worktree(b"src/app.rs.bak"));
        assert!(
            !sibling_prefix.affects_selection(None, Some(&selected)),
            "a sibling sharing a byte prefix is not an ancestor"
        );

        let mut unknown = EventBatch::default();
        unknown.absorb(WatchEvent::Unknown);
        assert!(unknown.affects_selection(None, Some(&selected)));

        // An ignore-source event may *be* the selected file (a .gitignore
        // being viewed) and carries no path, so it forces a re-read.
        let mut ignore = EventBatch::default();
        ignore.absorb(WatchEvent::IgnoreSource);
        assert!(ignore.affects_selection(None, Some(&path(b".gitignore"))));
        assert!(ignore.affects_selection(None, Some(&selected)));

        // A root event should be classified as Unknown, but if an empty
        // path slips through it still counts as everyone's ancestor.
        let mut root = EventBatch::default();
        root.absorb(worktree(b""));
        assert!(root.affects_selection(None, Some(&selected)));

        let mut unrelated = EventBatch::default();
        unrelated.absorb(worktree(b"README.md"));
        assert!(!unrelated.affects_selection(None, Some(&selected)));
        // Only this last shape may carry displayed content over.
    }

    #[test]
    fn aggregation_is_linear_in_the_path_count() {
        // A checkout can flood one window with tens of thousands of unique
        // paths; per-event aggregation runs on the terminal thread. The
        // quadratic Vec::contains dedup this guards against took seconds.
        let mut debouncer = Debouncer::new(Duration::from_millis(100));
        let t0 = Instant::now();
        let started = Instant::now();
        for i in 0..100_000u32 {
            debouncer.observe(worktree(format!("dir/file_{i}.rs").as_bytes()), t0);
        }
        let elapsed = started.elapsed();
        let batch = debouncer
            .due(t0 + Duration::from_millis(200))
            .expect("window expired");
        assert_eq!(batch.paths.len(), 100_000);
        assert!(
            elapsed.as_millis() < 2_000,
            "aggregating 100k paths took {elapsed:?}"
        );
    }

    #[test]
    fn old_and_new_sides_are_both_checked() {
        let mut batch = EventBatch::default();
        batch.absorb(worktree(b"renamed_from.rs"));
        assert!(batch.affects_selection(
            Some(&path(b"renamed_from.rs")),
            Some(&path(b"renamed_to.rs"))
        ));
    }
}
