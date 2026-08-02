//! Tracked-first ignore pre-filter for worktree events.
//!
//! Matching an ignore pattern is not enough to drop an event: `.gitignore`
//! does not silence *tracked* files (a tracked `target/keep.txt` still shows
//! in status even when `target/` is ignored). The filter therefore decides
//! in this order:
//!
//! 1. a tracked file, or a directory with tracked descendants, is kept;
//! 2. an event is dropped only when the path is untracked *and* matches the
//!    ignore machinery (`.gitignore` files, `info/exclude`, the configured
//!    excludes file — via gix);
//! 3. anything undecidable is kept.
//!
//! Dropping happens before debouncing, so churn under `target/` or
//! `node_modules/` during a build does not schedule status walks at every
//! debounce period. The filter snapshot is rebuilt by its owner whenever an
//! ignore source or the index may have changed; while a rebuild is pending
//! the owner must stop filtering (dirty mode) rather than consult a stale
//! matcher.

use gix::bstr::BStr;

use crate::path::GitPath;

/// One snapshot of the tracked set and ignore matcher.
pub struct IgnoreFilter<'repo> {
    /// Index-recorded paths, sorted (the git index is sorted by path).
    tracked: Vec<Vec<u8>>,
    stack: gix::AttributeStack<'repo>,
}

/// Why the filter could not be built (the owner keeps everything then).
pub type BuildError = Box<dyn std::error::Error + Send + Sync>;

impl<'repo> IgnoreFilter<'repo> {
    pub fn build(repo: &'repo gix::Repository) -> Result<Self, BuildError> {
        let index = repo.index_or_empty()?;
        let mut tracked: Vec<Vec<u8>> = index
            .entries()
            .iter()
            .map(|entry| entry.path(&index).to_vec())
            .collect();
        tracked.sort_unstable();
        let stack = repo.excludes(
            &index,
            None,
            gix::worktree::stack::state::ignore::Source::WorktreeThenIdMappingIfNotSkipped,
        )?;
        Ok(Self { tracked, stack })
    }

    /// Whether a worktree event for `path` must survive to the debouncer.
    pub fn keep(&mut self, path: &GitPath) -> bool {
        if self.has_tracked_at_or_under(path.as_bytes()) {
            return true;
        }
        // Untracked: drop only on a positive ignore match. The leaf mode is
        // left unknown (treated as a file); directory-only patterns on
        // *ancestors* still apply because the stack evaluates parents as
        // directories.
        let Ok(platform) = self.stack.at_entry(<&BStr>::from(path.as_bytes()), None) else {
            return true;
        };
        !platform.is_excluded()
    }

    /// `path` itself is tracked, or is a directory holding tracked files.
    fn has_tracked_at_or_under(&self, path: &[u8]) -> bool {
        let from = self.tracked.partition_point(|entry| entry[..] < *path);
        let Some(candidate) = self.tracked.get(from) else {
            return false;
        };
        if candidate == path {
            return true;
        }
        candidate.len() > path.len() + 1
            && candidate.starts_with(path)
            && candidate[path.len()] == b'/'
    }
}
