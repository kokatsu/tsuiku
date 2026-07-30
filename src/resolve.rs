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
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingObject(e) => write!(f, "cannot read object: {e}"),
            Self::Io { path, source } => write!(f, "cannot read {path}: {source}"),
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
                let meta =
                    std::fs::symlink_metadata(&resolved.0).map_err(|e| ResolveError::Io {
                        path: path.display_escaped(),
                        source: e,
                    })?;
                // A symlink's content is its target string. Following the link
                // would compare the wrong file, and would break on a dangling
                // one.
                if meta.is_symlink() {
                    use std::os::unix::ffi::OsStrExt;
                    let target = std::fs::read_link(&resolved.0).map_err(|e| ResolveError::Io {
                        path: path.display_escaped(),
                        source: e,
                    })?;
                    Arc::from(target.as_os_str().as_bytes())
                } else {
                    let read = std::fs::read(&resolved.0).map_err(|e| ResolveError::Io {
                        path: path.display_escaped(),
                        source: e,
                    })?;
                    Arc::from(read.as_slice())
                }
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
