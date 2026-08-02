//! Reading the bytes each side of a change points at.
//!
//! Resolution is also where "git says this changed" becomes "these bytes
//! differ". Staging a modification and then restoring the file leaves an entry
//! git keeps reporting; once both sides are read, their `ContentId`s match and
//! the entry drops out. A mode-only change survives, because identical bytes
//! under a different mode is still a change.

use std::path::Path;
use std::sync::Arc;

use crate::change::{ChangeSet, FileChange};
use crate::ids::{ContentPairId, ContentSource, ResolvedContent, ResolvedPresentContent};
use crate::path::PathResolver;

#[derive(Debug)]
pub enum ResolveError {
    /// The object is not in the database, or could not be decoded.
    MissingObject(Box<dyn std::error::Error + Send + Sync>),
    /// The worktree file could not be read.
    Io {
        path: String,
        source: std::io::Error,
    },
    /// The file kept changing while being read: its metadata differed
    /// before and after the read, twice in a row. The bytes cannot be
    /// trusted as one consistent snapshot and were discarded; the owner
    /// keeps the previous display and schedules one delayed re-read
    /// (never a busy loop here).
    UnstableRead { path: String },
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingObject(e) => write!(f, "cannot read object: {e}"),
            Self::Io { path, source } => write!(f, "cannot read {path}: {source}"),
            Self::UnstableRead { path } => {
                write!(f, "{path} kept changing while being read")
            }
        }
    }
}

impl std::error::Error for ResolveError {}

pub trait ContentResolver {
    fn resolve(&self, source: &ContentSource) -> Result<ResolvedContent, ResolveError>;
}

pub struct GixResolver {
    repo: gix::Repository,
    paths: PathResolver,
}

impl GixResolver {
    pub fn new(repo: gix::Repository, paths: PathResolver) -> Self {
        Self { repo, paths }
    }

    /// Reuse an already-open repository. Bare repositories have no worktree,
    /// but commit diffs only resolve object-database content, so the git
    /// directory is a harmless path fallback.
    pub(crate) fn from_repository(repo: gix::Repository, paths: Option<PathResolver>) -> Self {
        let paths = paths.unwrap_or_else(|| PathResolver::new(repo.git_dir().to_path_buf()));
        Self { repo, paths }
    }

    /// Locate the repository containing `path`, searching parent directories
    /// the way [`crate::discover::GixDiscoverer::open`] does.
    pub fn open(path: &Path) -> Result<Self, ResolveError> {
        let repo = gix::discover(path).map_err(|e| ResolveError::MissingObject(Box::new(e)))?;
        let workdir = repo
            .workdir()
            .unwrap_or_else(|| repo.git_dir())
            .to_path_buf();
        Ok(Self {
            paths: PathResolver::new(workdir),
            repo,
        })
    }
}

impl ContentResolver for GixResolver {
    fn resolve(&self, source: &ContentSource) -> Result<ResolvedContent, ResolveError> {
        let bytes: Arc<[u8]> = match source {
            ContentSource::Absent => return Ok(ResolvedContent::Absent),
            ContentSource::GitBlob { oid } => {
                let id = gix::ObjectId::Sha1(oid.0);
                let object = self
                    .repo
                    .find_object(id)
                    .map_err(|e| ResolveError::MissingObject(Box::new(e)))?;
                Arc::from(object.data.as_slice())
            }
            ContentSource::Worktree { path, .. } => {
                let resolved = self.paths.resolve(path);
                read_stable(&resolved.0, &path.display_escaped())?
            }
            // There are no bytes behind a gitlink. Rendering it the way git
            // renders it in a diff body keeps every downstream layer uniform.
            ContentSource::Submodule { commit, dirty } => {
                let suffix = if *dirty { "-dirty" } else { "" };
                Arc::from(format!("Subproject commit {}{suffix}\n", commit.to_hex()).as_bytes())
            }
        };

        let git_oid = match source {
            ContentSource::GitBlob { oid } => Some(*oid),
            _ => None,
        };
        Ok(ResolvedContent::Present(ResolvedPresentContent::new(
            source.clone(),
            bytes,
            git_oid,
        )))
    }
}

/// Change-detection stamp of one probe: symlink-ness, size, mtime. A hint,
/// not identity — same-size same-instant rewrites can pass — but it removes
/// the common case of reading a file mid-write.
type Stamp = (bool, u64, Option<std::time::SystemTime>);

fn stamp_of(meta: &std::fs::Metadata) -> Stamp {
    (meta.is_symlink(), meta.len(), meta.modified().ok())
}

/// Read one worktree file as a single consistent snapshot.
///
/// A symlink's content is its target string. Following the link would
/// compare the wrong file, and would break on a dangling one.
fn read_stable(path: &Path, display: &str) -> Result<Arc<[u8]>, ResolveError> {
    read_stable_with(
        display,
        || std::fs::symlink_metadata(path).map(|meta| stamp_of(&meta)),
        |symlink| {
            if symlink {
                use std::os::unix::ffi::OsStrExt;
                std::fs::read_link(path).map(|target| Arc::from(target.as_os_str().as_bytes()))
            } else {
                std::fs::read(path).map(|bytes| Arc::from(bytes.as_slice()))
            }
        },
    )
}

/// The stat–read–stat core, with the filesystem probes injected so the
/// retry contract is testable deterministically: metadata is compared
/// before and after the read; on a mismatch the read is retried once, and
/// a second mismatch discards the bytes as [`ResolveError::UnstableRead`]
/// rather than displaying a torn mixture of two versions.
fn read_stable_with(
    display: &str,
    mut probe: impl FnMut() -> std::io::Result<Stamp>,
    mut read: impl FnMut(bool) -> std::io::Result<Arc<[u8]>>,
) -> Result<Arc<[u8]>, ResolveError> {
    for _attempt in 0..2 {
        let before = probe().map_err(|source| ResolveError::Io {
            path: display.to_owned(),
            source,
        })?;
        let bytes = read(before.0).map_err(|source| ResolveError::Io {
            path: display.to_owned(),
            source,
        })?;
        match probe() {
            Ok(after) if after == before => return Ok(bytes),
            // Changed underneath, or gone between read and re-stat: retry
            // once from the top; a deletion then fails the next probe as Io.
            Ok(_) | Err(_) => {}
        }
    }
    Err(ResolveError::UnstableRead {
        path: display.to_owned(),
    })
}

/// A change with both sides read.
#[derive(Clone, Debug)]
pub struct ResolvedChange {
    pub change: FileChange,
    pub old: ResolvedContent,
    pub new: ResolvedContent,
}

impl ResolvedChange {
    pub fn pair_id(&self) -> ContentPairId {
        ContentPairId {
            old: self.old.identity(),
            new: self.new.identity(),
        }
    }

    /// True when nothing actually differs: same bytes under the same mode at
    /// the same path.
    pub fn is_no_op(&self) -> bool {
        let pair = self.pair_id();
        pair.old == pair.new
            && self.change.old_mode == self.change.new_mode
            && self.change.old_path == self.change.new_path
    }
}

/// Read both sides of every change, dropping the ones that turn out to be no
/// difference at all.
pub fn resolve_changes(
    resolver: &impl ContentResolver,
    set: &ChangeSet,
) -> Result<Vec<ResolvedChange>, ResolveError> {
    let mut out = Vec::with_capacity(set.changes.len());
    for change in &set.changes {
        let resolved = ResolvedChange {
            old: resolver.resolve(&change.old)?,
            new: resolver.resolve(&change.new)?,
            change: change.clone(),
        };
        if !resolved.is_no_op() {
            out.push(resolved);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, SystemTime};

    fn stamp(len: u64, tick: u64) -> Stamp {
        (
            false,
            len,
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(tick)),
        )
    }

    /// Drive `read_stable_with` from scripted probe results, counting reads.
    fn scripted(probes: Vec<std::io::Result<Stamp>>) -> (Result<Arc<[u8]>, ResolveError>, usize) {
        let probes = RefCell::new(VecDeque::from(probes));
        let reads = RefCell::new(0usize);
        let result = read_stable_with(
            "scripted",
            || {
                probes
                    .borrow_mut()
                    .pop_front()
                    .expect("script exhausted early")
            },
            |_| {
                *reads.borrow_mut() += 1;
                Ok(Arc::from(&b"bytes"[..]))
            },
        );
        (result, reads.into_inner())
    }

    #[test]
    fn a_quiet_file_is_accepted_on_the_first_read() {
        let (result, reads) = scripted(vec![Ok(stamp(5, 1)), Ok(stamp(5, 1))]);
        assert!(result.is_ok());
        assert_eq!(reads, 1);
    }

    #[test]
    fn one_mismatch_retries_once_and_then_accepts() {
        let (result, reads) = scripted(vec![
            Ok(stamp(5, 1)),
            Ok(stamp(9, 2)),
            Ok(stamp(9, 2)),
            Ok(stamp(9, 2)),
        ]);
        assert!(result.is_ok());
        assert_eq!(reads, 2, "exactly one retry");
    }

    #[test]
    fn two_mismatches_reject_the_bytes_as_unstable() {
        let (result, reads) = scripted(vec![
            Ok(stamp(5, 1)),
            Ok(stamp(9, 2)),
            Ok(stamp(9, 2)),
            Ok(stamp(12, 3)),
        ]);
        assert!(matches!(result, Err(ResolveError::UnstableRead { .. })));
        assert_eq!(reads, 2, "never a third attempt — no busy loop");
    }

    #[test]
    fn a_same_size_same_mtime_rewrite_is_beyond_the_stamp() {
        // The stamp is a hint: identical size and mtime pass. This test
        // documents the accepted limitation rather than a guarantee.
        let (result, reads) = scripted(vec![Ok(stamp(5, 1)), Ok(stamp(5, 1))]);
        assert!(result.is_ok());
        assert_eq!(reads, 1);
    }

    #[test]
    fn a_vanished_after_probe_retries_and_a_failed_before_probe_is_io() {
        // After-probe failure (deleted between read and re-stat): retried.
        let (result, reads) = scripted(vec![
            Ok(stamp(5, 1)),
            Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
            Ok(stamp(9, 2)),
            Ok(stamp(9, 2)),
        ]);
        assert!(result.is_ok());
        assert_eq!(reads, 2);

        // Before-probe failure propagates as a plain read error.
        let (result, reads) = scripted(vec![Err(std::io::Error::from(
            std::io::ErrorKind::NotFound,
        ))]);
        assert!(matches!(result, Err(ResolveError::Io { .. })));
        assert_eq!(reads, 0);
    }

    #[test]
    fn read_stable_returns_the_final_content_after_writing_stops() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let file = dir.path().join("churn.txt");
        std::fs::write(&file, "final\n").expect("write");
        let bytes = read_stable(&file, "churn.txt").expect("stable read");
        assert_eq!(&*bytes, b"final\n");
    }

    /// One write round: an 8-digit header naming the round, a newline, and
    /// a body whose length is derived from the round. An accepted read must
    /// reconstruct exactly (`fs::write` truncates first, so the only
    /// inconsistent states are the empty window and torn prefixes — and a
    /// torn prefix fails the length check for its own header).
    fn round_content(round: usize) -> String {
        format!("{round:08}\n{}", "x".repeat(round * 3))
    }

    #[test]
    fn read_stable_never_returns_a_torn_mixture_under_concurrent_writes() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let file = dir.path().join("churn.txt");
        std::fs::write(&file, round_content(0)).expect("write");
        let done = Arc::new(AtomicBool::new(false));

        let writer = {
            let file = file.clone();
            let done = Arc::clone(&done);
            std::thread::spawn(move || {
                for round in 1..3_000usize {
                    let _ = std::fs::write(&file, round_content(round));
                }
                done.store(true, Ordering::Relaxed);
            })
        };

        while !done.load(Ordering::Relaxed) {
            match read_stable(&file, "churn.txt") {
                // Empty is a real momentary state (truncate-then-write), so
                // it is a consistent observation. Anything else must be the
                // complete content of the round its header names.
                Ok(bytes) if bytes.is_empty() => {}
                Ok(bytes) => {
                    let text = std::str::from_utf8(&bytes).expect("rounds are ASCII");
                    let round: usize = text
                        .get(..8)
                        .and_then(|header| header.parse().ok())
                        .unwrap_or_else(|| panic!("torn header: {} bytes", bytes.len()));
                    assert_eq!(
                        text,
                        round_content(round),
                        "an accepted read must be one complete write"
                    );
                }
                // Rejection is the designed outcome for unlucky timing;
                // deletion races cannot happen here (rewrites only).
                Err(ResolveError::UnstableRead { .. }) => {}
                Err(error) => panic!("unexpected error: {error}"),
            }
        }
        writer.join().expect("writer thread");
        let settled = read_stable(&file, "churn.txt").expect("quiet file reads cleanly");
        assert_eq!(&*settled, round_content(2_999).as_bytes());
    }
}
