//! Change discovery contracts: what is being compared, and what came back.
//!
//! A `ChangeSet` lists *candidate* changes. Discovery reports what git
//! considers changed, which is not the same as what actually differs: staging
//! a modification and then restoring the file leaves an entry that git still
//! reports, even though HEAD and the worktree now hold identical bytes.
//! Deciding that requires reading both sides, so it belongs to the resolve
//! step in [`crate::resolve`], not here.

use crate::ids::{ContentSource, Oid};
use crate::path::GitPath;

/// What the two sides of the comparison are.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DiffTarget {
    /// HEAD against the final worktree state, with the index folded in.
    WorktreeVsHead,
    /// A commit against its first parent. Merges are compared against parent
    /// one only; this is not git's combined diff.
    CommitVsParent { commit: Oid },
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ChangeQuery {
    pub target: DiffTarget,
    /// Git pathspec patterns. Empty means everything.
    pub pathspecs: Vec<GitPath>,
}

impl ChangeQuery {
    pub fn new(target: DiffTarget) -> Self {
        Self {
            target,
            pathspecs: Vec::new(),
        }
    }
}

/// The kind of entry one side of a change holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EntryMode {
    File,
    Executable,
    Symlink,
    /// A gitlink: a commit id recorded in a tree, not a file.
    Submodule,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChangeStatus {
    Add,
    Delete,
    Modify,
    Rename,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FileChange {
    pub status: ChangeStatus,
    /// The path on the old side. `None` for an addition.
    pub old_path: Option<GitPath>,
    /// The path on the new side. `None` for a deletion.
    pub new_path: Option<GitPath>,
    pub old: ContentSource,
    pub new: ContentSource,
    pub old_mode: Option<EntryMode>,
    pub new_mode: Option<EntryMode>,
}

impl FileChange {
    /// Build a change from its two sides, or `None` when neither side exists —
    /// the shape a staged addition deleted again from the worktree produces.
    pub fn classify(
        old_path: Option<GitPath>,
        new_path: Option<GitPath>,
        old: ContentSource,
        new: ContentSource,
        old_mode: Option<EntryMode>,
        new_mode: Option<EntryMode>,
    ) -> Option<Self> {
        let old_present = old != ContentSource::Absent;
        let new_present = new != ContentSource::Absent;
        let status = match (old_present, new_present) {
            (false, false) => return None,
            (false, true) => ChangeStatus::Add,
            (true, false) => ChangeStatus::Delete,
            (true, true) if old_path != new_path => ChangeStatus::Rename,
            (true, true) => ChangeStatus::Modify,
        };
        Some(Self {
            status,
            old_path: old_present.then_some(old_path).flatten(),
            new_path: new_present.then_some(new_path).flatten(),
            old,
            new,
            old_mode,
            new_mode,
        })
    }

    /// The path to show for this change: the new one where there is one.
    pub fn display_path(&self) -> &GitPath {
        self.new_path
            .as_ref()
            .or(self.old_path.as_ref())
            .expect("a change has at least one side")
    }

    /// True when the entry changed kind, e.g. a file replaced by a symlink.
    /// Git reports this as one modification; tsuiku treats it as a delete plus
    /// an add, because the two sides share no content to line up.
    pub fn is_type_change(&self) -> bool {
        matches!((self.old_mode, self.new_mode), (Some(a), Some(b)) if a != b && (a == EntryMode::Symlink || b == EntryMode::Symlink || a == EntryMode::Submodule || b == EntryMode::Submodule))
    }
}

/// Something the caller should be told about, but which does not stop
/// discovery.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DiscoveryWarning {
    /// An unmerged index entry. The comparison uses stage 2 ("ours", i.e. what
    /// HEAD holds) against the file on disk.
    Unmerged { path: GitPath },
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ChangeSet {
    pub target: DiffTarget,
    pub changes: Vec<FileChange>,
    pub warnings: Vec<DiscoveryWarning>,
}

#[derive(Debug)]
pub enum DiscoverError {
    /// The path is not inside a git repository, or the repository is unusable.
    OpenRepository(Box<dyn std::error::Error + Send + Sync>),
    /// `WorktreeVsHead` was asked for in a bare repository.
    NoWorktree,
    /// The revision named by `CommitVsParent` does not exist.
    NoSuchCommit { commit: Oid },
    /// Reading refs, the index, or the object database failed.
    Repository(Box<dyn std::error::Error + Send + Sync>),
}

impl std::fmt::Display for DiscoverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OpenRepository(e) => write!(f, "cannot open repository: {e}"),
            Self::NoWorktree => write!(f, "repository has no worktree"),
            Self::NoSuchCommit { commit } => write!(f, "no such commit: {}", commit.to_hex()),
            Self::Repository(e) => write!(f, "repository read failed: {e}"),
        }
    }
}

impl std::error::Error for DiscoverError {}

pub trait ChangeDiscoverer {
    fn discover(&self, query: &ChangeQuery) -> Result<ChangeSet, DiscoverError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob() -> ContentSource {
        ContentSource::GitBlob { oid: Oid([1; 20]) }
    }

    fn worktree(path: &[u8]) -> ContentSource {
        ContentSource::Worktree {
            path: GitPath::from_bytes(path),
            hint: crate::ids::FileStamp {
                modified: std::time::SystemTime::UNIX_EPOCH,
                size: 0,
            },
        }
    }

    #[test]
    fn both_sides_absent_is_not_a_change() {
        assert!(
            FileChange::classify(
                None,
                None,
                ContentSource::Absent,
                ContentSource::Absent,
                None,
                None
            )
            .is_none()
        );
    }

    #[test]
    fn differing_paths_with_both_sides_present_is_a_rename() {
        let c = FileChange::classify(
            Some(GitPath::from_bytes(b"old.txt")),
            Some(GitPath::from_bytes(b"new.txt")),
            blob(),
            worktree(b"new.txt"),
            Some(EntryMode::File),
            Some(EntryMode::File),
        )
        .expect("a change");
        assert_eq!(c.status, ChangeStatus::Rename);
        assert_eq!(c.display_path(), &GitPath::from_bytes(b"new.txt"));
    }

    #[test]
    fn an_addition_carries_no_old_path() {
        let c = FileChange::classify(
            Some(GitPath::from_bytes(b"a.txt")),
            Some(GitPath::from_bytes(b"a.txt")),
            ContentSource::Absent,
            worktree(b"a.txt"),
            None,
            Some(EntryMode::File),
        )
        .expect("a change");
        assert_eq!(c.status, ChangeStatus::Add);
        assert_eq!(c.old_path, None);
    }

    #[test]
    fn an_executable_bit_change_is_not_a_type_change() {
        let c = FileChange::classify(
            Some(GitPath::from_bytes(b"s.sh")),
            Some(GitPath::from_bytes(b"s.sh")),
            blob(),
            worktree(b"s.sh"),
            Some(EntryMode::File),
            Some(EntryMode::Executable),
        )
        .expect("a change");
        assert!(!c.is_type_change());
    }

    #[test]
    fn a_file_becoming_a_symlink_is_a_type_change() {
        let c = FileChange::classify(
            Some(GitPath::from_bytes(b"l")),
            Some(GitPath::from_bytes(b"l")),
            blob(),
            worktree(b"l"),
            Some(EntryMode::File),
            Some(EntryMode::Symlink),
        )
        .expect("a change");
        assert!(c.is_type_change());
    }
}
