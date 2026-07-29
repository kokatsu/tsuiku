//! Change discovery backed by `gix`.
//!
//! `WorktreeVsHead` is the interesting one. gix reports two independent
//! streams — HEAD against the index, and the index against the worktree — and
//! neither answers the question tsuiku asks. Joining them by path does: for
//! every path either stream mentions, the old side is what HEAD holds and the
//! new side is what is on disk right now. Paths only one stream mentions get
//! their other side from the index, which by definition agrees with the side
//! that reported nothing.
//!
//! The join is what makes the composite staged/worktree states come out right.
//! A file staged as deleted and then written again arrives as a deletion *and*
//! an untracked entry at the same path; against HEAD it is one modification.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::SystemTime;

use gix::bstr::{BString, ByteSlice};

use crate::change::{
    ChangeDiscoverer, ChangeQuery, ChangeSet, DiffTarget, DiscoverError, DiscoveryWarning,
    EntryMode, FileChange,
};
use crate::ids::{ContentSource, FileStamp, Oid};
use crate::path::{GitPath, PathResolver};

pub struct GixDiscoverer {
    repo: gix::Repository,
    /// Absent for a bare repository, which can still answer commit diffs.
    paths: Option<PathResolver>,
}

impl GixDiscoverer {
    /// Open the repository containing `path`, searching parent directories the
    /// way git does, so the tool works from anywhere inside a checkout.
    pub fn open(path: &Path) -> Result<Self, DiscoverError> {
        let repo = gix::discover(path).map_err(|e| DiscoverError::OpenRepository(Box::new(e)))?;
        let paths = repo
            .workdir()
            .map(|dir| PathResolver::new(dir.to_path_buf()));
        Ok(Self { repo, paths })
    }

    pub fn path_resolver(&self) -> Option<&PathResolver> {
        self.paths.as_ref()
    }
}

/// What the status walk found out about a checked-out submodule, computed
/// with the submodule ignore settings already applied.
struct SubmoduleState {
    /// The commit the submodule's own HEAD sits on.
    commit: Option<Oid>,
    dirty: bool,
}

/// What HEAD holds for one path.
struct HeadSide {
    source: ContentSource,
    mode: Option<EntryMode>,
    /// Set when git reports the path as renamed; this is where it came from.
    from: Option<GitPath>,
}

impl ChangeDiscoverer for GixDiscoverer {
    fn discover(&self, query: &ChangeQuery) -> Result<ChangeSet, DiscoverError> {
        match query.target {
            DiffTarget::WorktreeVsHead => self.worktree_vs_head(&query.pathspecs),
            DiffTarget::CommitVsParent { commit } => {
                self.commit_vs_parent(commit, &query.pathspecs)
            }
        }
    }
}

impl GixDiscoverer {
    fn worktree_vs_head(&self, pathspecs: &[GitPath]) -> Result<ChangeSet, DiscoverError> {
        use gix::status::index_worktree::Item as IwItem;
        use gix::status::plumbing::index_as_worktree::{Change as IwChange, EntryStatus};
        use gix::status::{Item, UntrackedFiles};

        let workdir = self.paths.as_ref().ok_or(DiscoverError::NoWorktree)?;
        let index = self.repo.index_or_empty().map_err(repo_err)?;
        // `core.fileMode` off means the filesystem's executable bit carries no
        // information; git records and reports its own.
        let filemode_tracked = self
            .repo
            .config_snapshot()
            .boolean("core.fileMode")
            .unwrap_or(true);
        // With `core.symlinks` off, links are checked out as ordinary files
        // holding their target, but git keeps recording them as links.
        let symlinks_tracked = self
            .repo
            .config_snapshot()
            .boolean("core.symlinks")
            .unwrap_or(true);
        let patterns: Vec<BString> = pathspecs
            .iter()
            .map(|p| BString::from(p.as_bytes().to_vec()))
            .collect();

        let iter = self
            .repo
            .status(gix::progress::Discard)
            .map_err(repo_err)?
            .untracked_files(UntrackedFiles::Files)
            .into_iter(patterns)
            .map_err(repo_err)?;

        let mut head: BTreeMap<BString, HeadSide> = BTreeMap::new();
        // Path -> is the file present on disk, as far as the status stream saw.
        let mut work: BTreeMap<BString, bool> = BTreeMap::new();
        // Rename source -> destination. The source normally has no record of
        // its own, but it takes HEAD's side back if the destination turns out
        // to be missing from the worktree.
        let mut source_to_dest: BTreeMap<BString, BString> = BTreeMap::new();
        // Checked-out submodules the status walk had something to say about.
        let mut submodules: BTreeMap<BString, SubmoduleState> = BTreeMap::new();
        let mut warnings = Vec::new();

        for item in iter {
            match item.map_err(repo_err)? {
                Item::TreeIndex(change) => {
                    use gix::diff::index::ChangeRef as C;
                    match change {
                        C::Addition { location, .. } => {
                            head.insert(
                                location.into_owned(),
                                HeadSide {
                                    source: ContentSource::Absent,
                                    mode: None,
                                    from: None,
                                },
                            );
                        }
                        C::Deletion {
                            location,
                            entry_mode,
                            id,
                            ..
                        } => {
                            head.insert(location.into_owned(), head_side(&id, entry_mode, None));
                        }
                        C::Modification {
                            location,
                            previous_entry_mode,
                            previous_id,
                            ..
                        } => {
                            head.insert(
                                location.into_owned(),
                                head_side(&previous_id, previous_entry_mode, None),
                            );
                        }
                        C::Rewrite {
                            source_location,
                            source_entry_mode,
                            source_id,
                            location,
                            ..
                        } => {
                            let source_location = source_location.into_owned();
                            let location = location.into_owned();
                            source_to_dest.insert(source_location.clone(), location.clone());
                            head.insert(
                                location,
                                head_side(
                                    &source_id,
                                    source_entry_mode,
                                    Some(GitPath::from_bytes(&source_location)),
                                ),
                            );
                        }
                    }
                }
                Item::IndexWorktree(iw) => match iw {
                    IwItem::Modification {
                        rela_path, status, ..
                    } => match status {
                        EntryStatus::Conflict { .. } => {
                            warnings.push(DiscoveryWarning::Unmerged {
                                path: GitPath::from_bytes(&rela_path),
                            });
                            work.insert(rela_path, true);
                        }
                        EntryStatus::Change(IwChange::Removed) => {
                            work.insert(rela_path, false);
                        }
                        // gix computes this with the submodule ignore settings
                        // already applied, so taking dirtiness from here is
                        // what keeps `diff.ignoreSubmodules` in effect.
                        EntryStatus::Change(IwChange::SubmoduleModification(status)) => {
                            submodules.insert(
                                rela_path.clone(),
                                SubmoduleState {
                                    commit: status.checked_out_head_id.as_deref().map(oid_from_gix),
                                    dirty: submodule_checkout_is_dirty(&status),
                                },
                            );
                            work.insert(rela_path, true);
                        }
                        EntryStatus::Change(_) => {
                            work.insert(rela_path, true);
                        }
                        // Unchanged content, or a promise made by
                        // `git add --intent-to-add`.
                        EntryStatus::NeedsUpdate(_) | EntryStatus::IntentToAdd => {}
                    },
                    IwItem::DirectoryContents { entry, .. } => {
                        work.insert(entry.rela_path, true);
                    }
                    IwItem::Rewrite {
                        source,
                        dirwalk_entry,
                        ..
                    } => {
                        work.insert(dirwalk_entry.rela_path, true);
                        work.insert(source.rela_path().to_owned(), false);
                    }
                },
            }
        }

        let mut paths: Vec<BString> = head
            .keys()
            .chain(work.keys())
            .chain(source_to_dest.keys())
            .cloned()
            .collect();
        paths.sort();
        paths.dedup();

        // Every new side has to be known before any path can be classified: a
        // rename's source only knows what it is once the destination has been
        // checked. Whatever the streams said, the authority here is the
        // filesystem, since a file may have been removed after the walk.
        let new_sides: BTreeMap<&BString, (ContentSource, Option<EntryMode>)> = paths
            .iter()
            .map(|path| {
                let side = if work.get(path).is_some_and(|present| !present) {
                    (ContentSource::Absent, None)
                } else {
                    self.worktree_side(
                        workdir,
                        &GitPath::from_bytes(path),
                        index.entry_by_path(path.as_bstr()),
                        submodules.get(path),
                        filemode_tracked,
                        symlinks_tracked,
                    )
                };
                (path, side)
            })
            .collect();

        let mut changes = Vec::new();
        for path in &paths {
            let git_path = GitPath::from_bytes(path);
            let (new, new_mode) = new_sides
                .get(path)
                .cloned()
                .expect("computed for every path");

            if let Some(dest) = source_to_dest.get(path) {
                // A rename's source path. Normally the destination carries
                // HEAD's side and anything written back here is a new file.
                // But if the destination is gone from the worktree the rename
                // has collapsed, and this path is the only place HEAD's side
                // can go.
                let dest_present = new_sides
                    .get(dest)
                    .is_some_and(|(source, _)| *source != ContentSource::Absent);
                let (old, old_mode) = match (dest_present, head.get(dest)) {
                    (false, Some(side)) => (side.source.clone(), side.mode),
                    _ => (ContentSource::Absent, None),
                };
                if let Some(change) = FileChange::classify(
                    Some(git_path.clone()),
                    Some(git_path),
                    old,
                    new,
                    old_mode,
                    new_mode,
                ) {
                    changes.push(change);
                }
                continue;
            }

            let head_side = head.get(path);
            // The other half of a collapsed rename: its source emitted the
            // change already.
            if new == ContentSource::Absent && head_side.is_some_and(|side| side.from.is_some()) {
                continue;
            }

            let (old, old_mode, old_path) = match head_side {
                Some(side) => (
                    side.source.clone(),
                    side.mode,
                    side.from.clone().unwrap_or_else(|| git_path.clone()),
                ),
                // Unchanged between HEAD and the index, so the index entry
                // records HEAD's blob as well.
                None => match index.entry_by_path(path.as_bstr()) {
                    Some(entry) => {
                        let side = head_side_from_index(&entry.id, entry.mode);
                        (side.source, side.mode, git_path.clone())
                    }
                    None => (ContentSource::Absent, None, git_path.clone()),
                },
            };

            if let Some(change) =
                FileChange::classify(Some(old_path), Some(git_path), old, new, old_mode, new_mode)
            {
                changes.push(change);
            }
        }

        Ok(ChangeSet {
            target: DiffTarget::WorktreeVsHead,
            changes,
            warnings,
        })
    }

    /// Read the new side straight from disk. `index_entry` supplies what git
    /// records for this path, which is the authority wherever the filesystem
    /// cannot be trusted to carry it.
    fn worktree_side(
        &self,
        workdir: &PathResolver,
        path: &GitPath,
        index_entry: Option<&gix::index::Entry>,
        submodule: Option<&SubmoduleState>,
        filemode_tracked: bool,
        symlinks_tracked: bool,
    ) -> (ContentSource, Option<EntryMode>) {
        let indexed_kind = index_entry
            .and_then(|e| e.mode.to_tree_entry_mode())
            .map(|m| m.kind());

        // Entries outside a sparse checkout are deliberately absent from disk.
        // Git compares the index in their place rather than calling them
        // deleted.
        if let Some(entry) = index_entry
            && entry
                .flags
                .contains(gix::index::entry::Flags::SKIP_WORKTREE)
        {
            return side_from_kind(&entry.id, indexed_kind);
        }

        let resolved = workdir.resolve(path);
        let Ok(meta) = std::fs::symlink_metadata(&resolved.0) else {
            return (ContentSource::Absent, None);
        };

        if meta.is_dir() {
            // A directory in place of a tracked path is a checked-out
            // submodule. The status walk already computed both its current
            // commit and its dirtiness with the submodule ignore settings
            // applied, so asking the submodule ourselves would bypass config.
            let commit = submodule
                .and_then(|s| s.commit)
                .or_else(|| index_entry.map(|e| oid_from_gix(&e.id)));
            let dirty = submodule.is_some_and(|s| s.dirty);
            return match commit {
                Some(commit) => (
                    ContentSource::Submodule { commit, dirty },
                    Some(EntryMode::Submodule),
                ),
                None => (ContentSource::Absent, None),
            };
        }

        use std::os::unix::fs::PermissionsExt;
        let mode = if meta.is_symlink() {
            EntryMode::Symlink
        } else if !symlinks_tracked && indexed_kind == Some(gix::object::tree::EntryKind::Link) {
            // A link materialised as a regular file holding its target. Git
            // still calls it a link, and so must we, or restoring the original
            // target would read as a type change.
            EntryMode::Symlink
        } else if !filemode_tracked {
            // With `core.fileMode` off the executable bit on disk means
            // nothing; git keeps reporting whatever it recorded.
            match indexed_kind {
                Some(gix::object::tree::EntryKind::BlobExecutable) => EntryMode::Executable,
                _ => EntryMode::File,
            }
        } else if meta.permissions().mode() & 0o111 != 0 {
            EntryMode::Executable
        } else {
            EntryMode::File
        };
        let hint = FileStamp {
            modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            size: meta.len(),
        };
        (
            ContentSource::Worktree {
                path: path.clone(),
                hint,
            },
            Some(mode),
        )
    }

    fn commit_vs_parent(
        &self,
        commit: Oid,
        pathspecs: &[GitPath],
    ) -> Result<ChangeSet, DiscoverError> {
        use gix::diff::tree_with_rewrites::Change as C;

        let id = gix::ObjectId::Sha1(commit.0);
        let object = self
            .repo
            .find_commit(id)
            .map_err(|_| DiscoverError::NoSuchCommit { commit })?;
        let new_tree = object.tree().map_err(repo_err)?;

        // Merges are compared against their first parent. A root commit has no
        // parent, so everything in it reads as an addition.
        let old_tree = match object.parent_ids().next() {
            Some(parent) => Some(
                self.repo
                    .find_commit(parent)
                    .map_err(repo_err)?
                    .tree()
                    .map_err(repo_err)?,
            ),
            None => None,
        };

        let raw = self
            .repo
            .diff_tree_to_tree(old_tree.as_ref(), Some(&new_tree), None)
            .map_err(repo_err)?;

        let mut matcher = self.pathspec_matcher(pathspecs)?;
        let mut changes = Vec::new();
        for change in raw {
            let renamed = matches!(change, C::Rewrite { .. });
            let (old_path, new_path, mut old, mut old_mode, mut new, mut new_mode) = match change {
                C::Addition {
                    location,
                    entry_mode,
                    id,
                    ..
                } => {
                    let p = GitPath::from_bytes(&location);
                    let (src, mode) = tree_side(&id, entry_mode);
                    (p.clone(), p, ContentSource::Absent, None, src, mode)
                }
                C::Deletion {
                    location,
                    entry_mode,
                    id,
                    ..
                } => {
                    let p = GitPath::from_bytes(&location);
                    let (src, mode) = tree_side(&id, entry_mode);
                    (p.clone(), p, src, mode, ContentSource::Absent, None)
                }
                C::Modification {
                    location,
                    previous_entry_mode,
                    previous_id,
                    entry_mode,
                    id,
                } => {
                    let p = GitPath::from_bytes(&location);
                    let (old, old_mode) = tree_side(&previous_id, previous_entry_mode);
                    let (new, new_mode) = tree_side(&id, entry_mode);
                    (p.clone(), p, old, old_mode, new, new_mode)
                }
                C::Rewrite {
                    source_location,
                    source_entry_mode,
                    source_id,
                    location,
                    entry_mode,
                    id,
                    ..
                } => {
                    let (old, old_mode) = tree_side(&source_id, source_entry_mode);
                    let (new, new_mode) = tree_side(&id, entry_mode);
                    (
                        GitPath::from_bytes(&source_location),
                        GitPath::from_bytes(&location),
                        old,
                        old_mode,
                        new,
                        new_mode,
                    )
                }
            };

            // Git narrows the trees by pathspec before it looks for renames,
            // so naming one side of a rename splits it: the source alone is a
            // deletion, the destination alone an addition.
            //
            // Splitting afterwards reproduces that for a single rename
            // candidate, which is every case a caller can currently reach.
            // It does not when several deletions could pair with the same
            // addition and the pathspec excludes the one that won: git would
            // then pair the addition with a survivor instead of splitting.
            // Matching that needs rename detection to run over the narrowed
            // set, and gix only exposes it as part of the whole tree diff.
            if let Some(matcher) = matcher.as_mut() {
                let old_included = matcher.is_included(old_path.as_bytes().as_bstr(), Some(false));
                let new_included = matcher.is_included(new_path.as_bytes().as_bstr(), Some(false));
                match (old_included, new_included) {
                    (false, false) => continue,
                    (true, false) if renamed => {
                        new = ContentSource::Absent;
                        new_mode = None;
                    }
                    (false, true) if renamed => {
                        old = ContentSource::Absent;
                        old_mode = None;
                    }
                    _ => {}
                }
            }

            if let Some(change) =
                FileChange::classify(Some(old_path), Some(new_path), old, new, old_mode, new_mode)
            {
                changes.push(change);
            }
        }

        changes.sort_by(|a, b| a.display_path().as_bytes().cmp(b.display_path().as_bytes()));
        Ok(ChangeSet {
            target: DiffTarget::CommitVsParent { commit },
            changes,
            warnings: Vec::new(),
        })
    }

    fn pathspec_matcher(
        &self,
        pathspecs: &[GitPath],
    ) -> Result<Option<gix::Pathspec<'_>>, DiscoverError> {
        if pathspecs.is_empty() {
            return Ok(None);
        }
        let index = self.repo.index_or_empty().map_err(repo_err)?;
        let patterns: Vec<BString> = pathspecs
            .iter()
            .map(|p| BString::from(p.as_bytes().to_vec()))
            .collect();
        let spec = self
            .repo
            .pathspec(
                false,
                patterns.iter().map(|p| p.as_bstr()),
                true,
                &index,
                gix::worktree::stack::state::attributes::Source::IdMapping,
            )
            .map_err(repo_err)?;
        Ok(Some(spec))
    }
}

/// Whether a submodule checkout carries the kind of change git marks with
/// `-dirty`. `Status::is_dirty` is not the right question: it also fires on a
/// moved commit, and it counts untracked files, which git reports in `status`
/// but leaves out of the diff entirely. The change list gix hands over has the
/// submodule ignore settings already applied, so filtering it here keeps them
/// in effect.
fn submodule_checkout_is_dirty(status: &gix::submodule::Status) -> bool {
    use gix::status::Item;
    use gix::status::index_worktree::Item as IwItem;
    status.changes.as_ref().is_some_and(|changes| {
        changes.iter().any(|change| {
            !matches!(
                change,
                Item::IndexWorktree(IwItem::DirectoryContents { .. })
            )
        })
    })
}

fn head_side(
    id: &gix::hash::oid,
    mode: gix::index::entry::Mode,
    from: Option<GitPath>,
) -> HeadSide {
    let mut side = head_side_from_index(id, mode);
    side.from = from;
    side
}

fn head_side_from_index(id: &gix::hash::oid, mode: gix::index::entry::Mode) -> HeadSide {
    let kind = mode.to_tree_entry_mode().map(|m| m.kind());
    let (source, mode) = side_from_kind(id, kind);
    HeadSide {
        source,
        mode,
        from: None,
    }
}

fn tree_side(
    id: &gix::hash::oid,
    mode: gix::object::tree::EntryMode,
) -> (ContentSource, Option<EntryMode>) {
    side_from_kind(id, Some(mode.kind()))
}

fn side_from_kind(
    id: &gix::hash::oid,
    kind: Option<gix::object::tree::EntryKind>,
) -> (ContentSource, Option<EntryMode>) {
    use gix::object::tree::EntryKind as K;
    match kind {
        Some(K::Commit) => (
            ContentSource::Submodule {
                commit: oid_from_gix(id),
                // A tree records a commit id and nothing about its checkout.
                dirty: false,
            },
            Some(EntryMode::Submodule),
        ),
        Some(K::Link) => (
            ContentSource::GitBlob {
                oid: oid_from_gix(id),
            },
            Some(EntryMode::Symlink),
        ),
        Some(K::BlobExecutable) => (
            ContentSource::GitBlob {
                oid: oid_from_gix(id),
            },
            Some(EntryMode::Executable),
        ),
        Some(K::Blob) => (
            ContentSource::GitBlob {
                oid: oid_from_gix(id),
            },
            Some(EntryMode::File),
        ),
        // Trees never reach the change list: both diff sources report leaves.
        Some(K::Tree) | None => (ContentSource::Absent, None),
    }
}

pub(crate) fn oid_from_gix(id: &gix::hash::oid) -> Oid {
    let bytes = id.as_bytes();
    let mut out = [0u8; 20];
    out.copy_from_slice(
        bytes
            .get(..20)
            .expect("SHA-1 repositories only; see ContentId docs"),
    );
    Oid(out)
}

fn repo_err(e: impl std::error::Error + Send + Sync + 'static) -> DiscoverError {
    DiscoverError::Repository(Box::new(e))
}
