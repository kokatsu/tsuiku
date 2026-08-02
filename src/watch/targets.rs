//! Watch target resolution and raw-path classification.
//!
//! Git metadata is watched as *parent directory plus file name*: the index,
//! refs and packed-refs are replaced by atomic rename, so watching the file
//! itself would silently track a dead inode after the first replacement.
//! The worktree gitdir and the common dir are resolved separately via gix —
//! in a linked worktree HEAD and the index live in the per-worktree gitdir
//! while refs, packed-refs, config and info/exclude live in the common dir,
//! and a single "the .git directory" watch would miss one side.
//!
//! Classification turns an absolute filesystem path into a [`WatchEvent`]:
//! named metadata files (and their `.lock` twins) become `GitMetadata` or
//! `IgnoreSource`; everything else under a git dir is noise and classifies
//! to `None`; `.gitignore` files anywhere in the worktree are ignore
//! sources; remaining worktree paths become `Worktree` events; the worktree
//! root itself is `Unknown` (it may affect everything).

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use super::WatchEvent;
use crate::path::GitPath;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TargetKind {
    GitMetadata,
    IgnoreSource,
}

impl TargetKind {
    fn event(self) -> WatchEvent {
        match self {
            Self::GitMetadata => WatchEvent::GitMetadata,
            Self::IgnoreSource => WatchEvent::IgnoreSource,
        }
    }
}

/// One watched file, as its parent directory and file name.
#[derive(Clone, PartialEq, Eq, Debug)]
struct NamedTarget {
    dir: PathBuf,
    name: OsString,
    kind: TargetKind,
}

impl NamedTarget {
    /// Matches the file itself and its `.lock` twin: git prepares atomic
    /// replacements under `<name>.lock`, and seeing either means the value
    /// is changing.
    fn matches(&self, dir: &Path, name: &OsStr) -> bool {
        if self.dir != dir {
            return false;
        }
        let name = name.as_bytes();
        let own = self.name.as_bytes();
        name == own
            || (name.len() == own.len() + 5
                && name.starts_with(own)
                && &name[own.len()..] == b".lock")
    }
}

/// Everything the backend should watch, and the classifier for its events.
pub struct WatchTargets {
    worktree_root: PathBuf,
    /// Directories whose *contents* are git-internal: any unmatched event
    /// beneath them is noise (object packing, gc, …), not a worktree change.
    git_roots: Vec<PathBuf>,
    named: Vec<NamedTarget>,
}

/// Why watch targets could not be resolved.
#[derive(Debug, PartialEq, Eq)]
pub enum TargetError {
    /// The repository has no worktree to watch.
    Bare,
}

impl std::fmt::Display for TargetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bare => write!(f, "bare repositories have no worktree to watch"),
        }
    }
}

impl std::error::Error for TargetError {}

impl WatchTargets {
    /// Resolve the current watch targets. Re-run after a HEAD or branch
    /// switch so the symbolic-ref directory and filter follow the new ref.
    pub fn resolve(repo: &gix::Repository) -> Result<Self, TargetError> {
        let worktree_root = canonical(repo.workdir().ok_or(TargetError::Bare)?);
        let git_dir = canonical(repo.git_dir());
        let common = canonical(repo.common_dir());

        let mut named = Vec::new();
        let mut push = |dir: PathBuf, name: &OsStr, kind: TargetKind| {
            named.push(NamedTarget {
                dir,
                name: name.to_owned(),
                kind,
            });
        };

        // Per-worktree gitdir: HEAD, the index, and worktree-scoped config.
        push(git_dir.clone(), OsStr::new("HEAD"), TargetKind::GitMetadata);
        let index_path = repo.index_path();
        if let (Some(dir), Some(name)) = (index_path.parent(), index_path.file_name()) {
            push(canonical(dir), name, TargetKind::GitMetadata);
        }
        push(
            git_dir.clone(),
            OsStr::new("config.worktree"),
            TargetKind::GitMetadata,
        );

        // Common dir: shared refs and config.
        push(
            common.clone(),
            OsStr::new("packed-refs"),
            TargetKind::GitMetadata,
        );
        push(
            common.clone(),
            OsStr::new("config"),
            TargetKind::GitMetadata,
        );
        push(
            common.join("info"),
            OsStr::new("exclude"),
            TargetKind::IgnoreSource,
        );

        // The resolved symbolic ref, e.g. refs/heads/feature/foo: watch
        // refs/heads/feature/ and filter for "foo". Hierarchical ref names
        // must land on the deepest directory, not on refs/.
        if let Ok(Some(full_name)) = repo.head_name() {
            let ref_path = common.join(OsStr::from_bytes(full_name.as_bstr()));
            if let (Some(dir), Some(name)) = (ref_path.parent(), ref_path.file_name()) {
                push(dir.to_path_buf(), name, TargetKind::GitMetadata);
            }
        }

        // Global ignore sources: the resolved core.excludesFile and the
        // config files able to redefine it.
        for path in global_ignore_sources(repo) {
            let path = canonical_lenient(&path);
            if let (Some(dir), Some(name)) = (path.parent(), path.file_name()) {
                push(dir.to_path_buf(), name, TargetKind::IgnoreSource);
            }
        }

        Ok(Self {
            worktree_root,
            git_roots: vec![git_dir, common],
            named,
        })
    }

    /// The directory to watch recursively.
    pub fn worktree_root(&self) -> &Path {
        &self.worktree_root
    }

    /// Directories to watch non-recursively (deduplicated), including git
    /// metadata parents that live outside the worktree.
    pub fn metadata_dirs(&self) -> Vec<&Path> {
        self.metadata_dirs_with_events()
            .into_iter()
            .map(|(dir, _)| dir)
            .collect()
    }

    /// Like [`Self::metadata_dirs`], with the event a change to that
    /// directory's targets represents. A directory hosting both kinds
    /// reports `GitMetadata` (the stronger recovery path).
    pub fn metadata_dirs_with_events(&self) -> Vec<(&Path, WatchEvent)> {
        let mut dirs: Vec<(&Path, WatchEvent)> = Vec::new();
        for target in &self.named {
            let dir = target.dir.as_path();
            match dirs.iter_mut().find(|(existing, _)| *existing == dir) {
                Some((_, event)) => {
                    if target.kind == TargetKind::GitMetadata {
                        *event = WatchEvent::GitMetadata;
                    }
                }
                None => dirs.push((dir, target.kind.event())),
            }
        }
        dirs.sort_unstable_by_key(|(dir, _)| *dir);
        dirs
    }

    /// Classify one absolute event path. `None` means the event is
    /// git-internal noise that must not trigger a refresh.
    pub fn classify(&self, path: &Path) -> Option<WatchEvent> {
        if let (Some(dir), Some(name)) = (path.parent(), path.file_name())
            && let Some(target) = self.named.iter().find(|t| t.matches(dir, name))
        {
            return Some(target.kind.event());
        }
        if self
            .git_roots
            .iter()
            .any(|root| path.starts_with(root) || path == root)
        {
            return None;
        }

        let relative = match path.strip_prefix(&self.worktree_root) {
            Ok(relative) => relative,
            // Outside the worktree and not a named target: none of ours
            // (e.g. unrelated churn in a watched global-config directory).
            Err(_) => return None,
        };
        if relative.as_os_str().is_empty() {
            return Some(WatchEvent::Unknown);
        }
        if path.file_name() == Some(OsStr::new(".gitignore")) {
            return Some(WatchEvent::IgnoreSource);
        }
        Some(WatchEvent::Worktree {
            path: GitPath::from_bytes(relative.as_os_str().as_bytes()),
        })
    }
}

/// The files whose change can alter ignore behavior: the resolved
/// `core.excludesFile` (or its XDG default when unset) plus every config
/// file git may consult. Three overlapping enumerations, because each one
/// alone has a blind spot:
///
/// 1. gix's section metadata covers every file that contributed at least
///    one section — but an *empty* (or comment-only, or not-yet-created)
///    file leaves no section behind and would vanish from the watch set;
/// 2. the standard search candidates (`GIT_CONFIG_SYSTEM`/`GIT_CONFIG_GLOBAL`
///    or their default locations, `~/.gitconfig`, the XDG config) are added
///    unconditionally so they are watched even while empty or missing;
/// 3. `include.path` / `includeIf.*.path` values are resolved from the raw
///    sections so an empty include target stays watched too.
fn global_ignore_sources(repo: &gix::Repository) -> Vec<PathBuf> {
    let mut sources = Vec::new();
    let snapshot = repo.config_snapshot();
    let excludes = snapshot.trusted_path("core.excludesFile").ok().flatten();
    match excludes {
        Some(path) => sources.push(path),
        None => {
            if let Some(config_home) = xdg_config_home() {
                sources.push(config_home.join("git").join("ignore"));
            }
        }
    }

    for section in snapshot.plumbing().sections() {
        let meta = section.meta();
        if let Some(path) = &meta.path {
            sources.push(path.clone());
        }
        // Include directives, resolved like git does: relative to the file
        // containing them. Conditional includes are watched regardless of
        // whether their condition currently holds — flipping the condition
        // is itself a config change arriving via the containing file.
        let name = section.header().name();
        if name.eq_ignore_ascii_case(b"include") || name.eq_ignore_ascii_case(b"includeIf") {
            for value in section.values("path") {
                if let Some(path) = resolve_config_path(&value, meta.path.as_deref()) {
                    sources.push(path);
                }
            }
        }
    }

    sources.extend(standard_config_candidates());
    sources.sort_unstable();
    sources.dedup();
    sources
}

/// A config-file path value, interpolated the way git does (`~/`, `~user/`,
/// `%(prefix)/`) via gix; a path that stays relative after interpolation
/// resolves against the directory of the containing file. `None` means the
/// value cannot name a file in this environment (and thus cannot be
/// watched) — hand-rolling the expansion here produced wrong watch paths
/// for exactly the empty/missing targets the enumeration exists for.
fn resolve_config_path(value: &[u8], containing: Option<&Path>) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let interpolated =
        gix::config::Path::from(std::borrow::Cow::Borrowed(gix::bstr::BStr::new(value)))
            .interpolate(gix::config::path::interpolate::Context {
                git_install_dir: gix::path::env::installation_config_prefix(),
                home_dir: home.as_deref(),
                ..Default::default()
            })
            .ok()?;
    if interpolated.is_absolute() {
        return Some(interpolated);
    }
    Some(containing?.parent()?.join(interpolated))
}

/// Where git looks for installation, system and global configuration,
/// watched even when the files are empty or do not exist yet. Enumeration
/// and environment interpretation (`GIT_CONFIG_NOSYSTEM` booleans,
/// `GIT_CONFIG_SYSTEM`/`GIT_CONFIG_GLOBAL` overrides, installation prefix)
/// are delegated to gix so they cannot drift from what actually gets read.
fn standard_config_candidates() -> Vec<PathBuf> {
    use gix::config::Source;
    let mut env = |name: &str| std::env::var_os(name);
    [
        Source::GitInstallation,
        Source::System,
        Source::Git,
        Source::User,
    ]
    .into_iter()
    .filter_map(|source| source.storage_location(&mut env))
    .collect()
}

fn xdg_config_home() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("XDG_CONFIG_HOME")
        && !explicit.is_empty()
    {
        return Some(PathBuf::from(explicit));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config"))
}

/// Watched paths and classified paths must agree on symlink resolution
/// (macOS delivers /private/tmp for /tmp); failure falls back to the raw
/// path rather than refusing to watch.
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Like [`canonical`], but a path that does not (fully) exist resolves its
/// deepest existing prefix and keeps the missing suffix verbatim — a
/// configured-but-not-yet-created excludes file still needs a canonical
/// parent chain for event matching.
fn canonical_lenient(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => {
            canonical_lenient(parent).join(name)
        }
        _ => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn targets() -> WatchTargets {
        let root = PathBuf::from("/work");
        let git = PathBuf::from("/work/.git");
        WatchTargets {
            worktree_root: root,
            git_roots: vec![git.clone()],
            named: vec![
                NamedTarget {
                    dir: git.clone(),
                    name: OsString::from("HEAD"),
                    kind: TargetKind::GitMetadata,
                },
                NamedTarget {
                    dir: git.clone(),
                    name: OsString::from("index"),
                    kind: TargetKind::GitMetadata,
                },
                NamedTarget {
                    dir: git.join("refs/heads/feature"),
                    name: OsString::from("foo"),
                    kind: TargetKind::GitMetadata,
                },
                NamedTarget {
                    dir: git.join("info"),
                    name: OsString::from("exclude"),
                    kind: TargetKind::IgnoreSource,
                },
            ],
        }
    }

    #[test]
    fn worktree_paths_classify_relative_to_the_root() {
        let targets = targets();
        assert_eq!(
            targets.classify(Path::new("/work/src/main.rs")),
            Some(WatchEvent::Worktree {
                path: GitPath::from_bytes(b"src/main.rs")
            })
        );
        assert_eq!(
            targets.classify(Path::new("/work")),
            Some(WatchEvent::Unknown),
            "an event on the root itself may affect everything"
        );
        assert_eq!(targets.classify(Path::new("/elsewhere/file")), None);
    }

    #[test]
    fn gitignore_anywhere_in_the_worktree_is_an_ignore_source() {
        let targets = targets();
        assert_eq!(
            targets.classify(Path::new("/work/.gitignore")),
            Some(WatchEvent::IgnoreSource)
        );
        assert_eq!(
            targets.classify(Path::new("/work/deep/dir/.gitignore")),
            Some(WatchEvent::IgnoreSource)
        );
    }

    #[test]
    fn named_metadata_files_and_their_lock_twins_match() {
        let targets = targets();
        for name in ["HEAD", "HEAD.lock", "index", "index.lock"] {
            assert_eq!(
                targets.classify(&Path::new("/work/.git").join(name)),
                Some(WatchEvent::GitMetadata),
                "{name} must classify as metadata"
            );
        }
        assert_eq!(
            targets.classify(Path::new("/work/.git/refs/heads/feature/foo")),
            Some(WatchEvent::GitMetadata),
            "hierarchical refs match in their deepest directory"
        );
        assert_eq!(
            targets.classify(Path::new("/work/.git/info/exclude")),
            Some(WatchEvent::IgnoreSource)
        );
    }

    #[test]
    fn unmatched_git_internal_paths_are_noise() {
        let targets = targets();
        assert_eq!(
            targets.classify(Path::new("/work/.git/objects/ab/cdef0123")),
            None
        );
        assert_eq!(
            targets.classify(Path::new("/work/.git/refs/heads/other")),
            None,
            "only the resolved symbolic ref is interesting"
        );
        assert_eq!(targets.classify(Path::new("/work/.git/index.stale")), None);
        assert_eq!(targets.classify(Path::new("/work/.git/HEADER")), None);
    }

    #[test]
    fn config_path_values_resolve_like_git() {
        let containing = Some(Path::new("/repo/.git/config"));
        assert_eq!(
            resolve_config_path(b"/abs/x.config", None),
            Some(PathBuf::from("/abs/x.config"))
        );
        assert_eq!(
            resolve_config_path(b"../shared.config", containing),
            Some(PathBuf::from("/repo/.git/../shared.config"))
        );
        assert_eq!(
            resolve_config_path(b"relative.config", None),
            None,
            "a relative include without a containing file cannot resolve"
        );
        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(
                resolve_config_path(b"~/x.config", None),
                Some(PathBuf::from(home).join("x.config"))
            );
        }
    }

    #[test]
    fn git_interpolations_never_resolve_containing_relative() {
        let containing = Some(Path::new("/repo/.git/config"));
        // `%(prefix)/…` binds to the git installation prefix (or fails when
        // there is none) — never to the containing file's directory.
        if let Some(prefix) = gix::path::env::installation_config_prefix() {
            assert_eq!(
                resolve_config_path(b"%(prefix)/etc/gitconfig", containing),
                Some(prefix.join("etc/gitconfig"))
            );
        } else {
            assert_eq!(
                resolve_config_path(b"%(prefix)/etc/gitconfig", containing),
                None
            );
        }
        // `~user/…` binds to that user's home directory.
        if let Some(user) = std::env::var_os("USER").and_then(|u| u.into_string().ok()) {
            let value = format!("~{user}/x.config");
            let resolved = resolve_config_path(value.as_bytes(), containing);
            assert!(
                resolved
                    .as_ref()
                    .is_some_and(|p| p.is_absolute() && !p.starts_with("/repo")),
                "~user must resolve into a home directory, got {resolved:?}"
            );
        }
    }

    #[test]
    fn standard_candidates_match_gix_enumeration() {
        use gix::config::Source;
        let mut env = |name: &str| std::env::var_os(name);
        let expected: Vec<PathBuf> = [
            Source::GitInstallation,
            Source::System,
            Source::Git,
            Source::User,
        ]
        .into_iter()
        .filter_map(|source| source.storage_location(&mut env))
        .collect();
        assert_eq!(standard_config_candidates(), expected);
    }

    #[test]
    fn metadata_dirs_deduplicate() {
        let targets = targets();
        let dirs = targets.metadata_dirs();
        assert_eq!(
            dirs,
            vec![
                Path::new("/work/.git"),
                Path::new("/work/.git/info"),
                Path::new("/work/.git/refs/heads/feature"),
            ]
        );
    }
}
