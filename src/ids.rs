//! Content identity and snapshot contracts.
//!
//! `ContentId` is a pure content hash (blake3) computed over the raw bytes at
//! read time, regardless of where the bytes came from. Git OIDs are carried as
//! extra information but never used for identity. `Absent` is distinct from
//! `Present(empty)`: absent→empty is an empty-file add, empty→absent is a
//! delete.

use std::sync::Arc;
use std::time::SystemTime;

use crate::path::GitPath;

/// Monotonic generation of one discovery snapshot.
///
/// File indices are only meaningful within one snapshot: a re-discover may
/// insert, remove, or reorder entries. Background results therefore carry the
/// generation they were requested under, and a result applies only when both
/// its generation and its file index match the current snapshot.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SnapshotId(pub u64);

impl SnapshotId {
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// Pure content hash: blake3 over the full raw bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ContentId(blake3::Hash);

impl ContentId {
    pub fn compute(bytes: &[u8]) -> Self {
        Self(blake3::hash(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        self.0.as_bytes()
    }
}

/// Identity of one side of a diff. `Absent` ≠ `Present(hash of [])`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ContentIdentity {
    Absent,
    Present(ContentId),
}

/// Identity of a (old, new) content pair — the core of every cache key.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ContentPairId {
    pub old: ContentIdentity,
    pub new: ContentIdentity,
}

/// Git object id. Assumes SHA-1 repositories; SHA-256 repos are unsupported.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Oid(pub [u8; 20]);

impl Oid {
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// Cheap re-read hint for worktree files. Never used as identity: whether a
/// cached result applies is always decided by `ContentId`, not by mtime/size.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FileStamp {
    pub modified: SystemTime,
    pub size: u64,
}

/// Where one side of a diff comes from.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ContentSource {
    Absent,
    GitBlob {
        oid: Oid,
    },
    Worktree {
        path: GitPath,
        hint: FileStamp,
    },
    /// A gitlink. There are no bytes to read; the entry *is* the commit id,
    /// which is rendered the way git renders it in a diff body.
    Submodule {
        commit: Oid,
        /// The submodule sits on `commit` but its worktree has uncommitted
        /// changes. Git marks this by suffixing the id with `-dirty`.
        dirty: bool,
    },
}

/// Fully resolved content for one side.
#[derive(Clone, Debug)]
pub enum ResolvedContent {
    Absent,
    Present(ResolvedPresentContent),
}

impl ResolvedContent {
    pub fn identity(&self) -> ContentIdentity {
        match self {
            ResolvedContent::Absent => ContentIdentity::Absent,
            ResolvedContent::Present(p) => ContentIdentity::Present(p.content_id),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ResolvedPresentContent {
    pub source: ContentSource,
    pub bytes: Arc<[u8]>,
    /// blake3 over `bytes`, computed at read time.
    pub content_id: ContentId,
    /// Present when the bytes came from git. Informational only.
    pub git_oid: Option<Oid>,
}

impl ResolvedPresentContent {
    pub fn new(source: ContentSource, bytes: Arc<[u8]>, git_oid: Option<Oid>) -> Self {
        let content_id = ContentId::compute(&bytes);
        Self {
            source,
            bytes,
            content_id,
            git_oid,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_bytes_same_id_regardless_of_source() {
        let bytes: Arc<[u8]> = Arc::from(&b"hello\n"[..]);
        let from_git = ResolvedPresentContent::new(
            ContentSource::GitBlob { oid: Oid([0; 20]) },
            bytes.clone(),
            Some(Oid([0; 20])),
        );
        let from_worktree = ResolvedPresentContent::new(
            ContentSource::Worktree {
                path: GitPath::from_bytes(b"a.txt"),
                hint: FileStamp {
                    modified: SystemTime::UNIX_EPOCH,
                    size: 6,
                },
            },
            bytes,
            None,
        );
        assert_eq!(from_git.content_id, from_worktree.content_id);
    }

    #[test]
    fn absent_differs_from_empty() {
        let empty = ContentIdentity::Present(ContentId::compute(b""));
        assert_ne!(ContentIdentity::Absent, empty);
    }
}
