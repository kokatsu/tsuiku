//! Watch target resolution against real repositories, including a linked
//! worktree where HEAD/index live in the per-worktree gitdir while refs and
//! packed-refs live in the common dir.

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;
use tsuiku::path::GitPath;
use tsuiku::watch::WatchEvent;
use tsuiku::watch::targets::WatchTargets;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A repo on the hierarchical branch `feature/foo`, plus a linked worktree.
fn fixture() -> (TempDir, PathBuf, PathBuf) {
    let dir = TempDir::new().expect("temp dir");
    let main = dir.path().join("main");
    std::fs::create_dir(&main).expect("mkdir");
    git(&main, &["init", "-q", "-b", "main"]);
    std::fs::write(main.join("tracked.txt"), "one\n").expect("write");
    git(&main, &["add", "."]);
    git(&main, &["commit", "-q", "-m", "init"]);
    git(&main, &["checkout", "-q", "-b", "feature/foo"]);
    let linked = dir.path().join("linked");
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            linked.to_str().expect("utf8 tmp path"),
            "-b",
            "wt",
        ],
    );
    (dir, main, linked)
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).expect("fixture path exists")
}

#[test]
fn main_worktree_targets_cover_the_hierarchical_head_ref() {
    let (_dir, main, _linked) = fixture();
    let repo = gix::discover(&main).expect("open repo");
    let targets = WatchTargets::resolve(&repo).expect("resolve targets");

    assert_eq!(targets.worktree_root(), canonical(&main));
    let git_dir = canonical(&main.join(".git"));

    for name in ["HEAD", "index", "packed-refs", "config"] {
        assert_eq!(
            targets.classify(&git_dir.join(name)),
            Some(WatchEvent::GitMetadata),
            "{name} must classify as metadata"
        );
    }
    assert_eq!(
        targets.classify(&git_dir.join("refs/heads/feature/foo")),
        Some(WatchEvent::GitMetadata),
        "the checked-out hierarchical ref is watched in its deepest directory"
    );
    assert_eq!(
        targets.classify(&git_dir.join("refs/heads/main")),
        None,
        "other refs are not interesting"
    );
    assert_eq!(
        targets.classify(&git_dir.join("info/exclude")),
        Some(WatchEvent::IgnoreSource)
    );
    assert_eq!(
        targets.classify(&git_dir.join("objects/ab/cdef")),
        None,
        "object churn is git-internal noise"
    );
    assert_eq!(
        targets.classify(&canonical(&main).join("tracked.txt")),
        Some(WatchEvent::Worktree {
            path: GitPath::from_bytes(b"tracked.txt")
        })
    );
    assert!(
        targets
            .metadata_dirs()
            .contains(&git_dir.join("refs/heads/feature").as_path())
    );
}

#[test]
fn linked_worktree_splits_gitdir_and_common_dir_targets() {
    let (_dir, main, linked) = fixture();
    let repo = gix::discover(&linked).expect("open linked worktree");
    let targets = WatchTargets::resolve(&repo).expect("resolve targets");

    assert_eq!(targets.worktree_root(), canonical(&linked));
    let common = canonical(&main.join(".git"));
    let worktree_gitdir = canonical(&common.join("worktrees/linked"));

    // HEAD and the index are per-worktree.
    assert_eq!(
        targets.classify(&worktree_gitdir.join("HEAD")),
        Some(WatchEvent::GitMetadata)
    );
    assert_eq!(
        targets.classify(&worktree_gitdir.join("index")),
        Some(WatchEvent::GitMetadata)
    );
    assert_eq!(
        targets.classify(&worktree_gitdir.join("index.lock")),
        Some(WatchEvent::GitMetadata)
    );

    // Refs, packed-refs and info/exclude are shared in the common dir.
    assert_eq!(
        targets.classify(&common.join("packed-refs")),
        Some(WatchEvent::GitMetadata)
    );
    assert_eq!(
        targets.classify(&common.join("refs/heads/wt")),
        Some(WatchEvent::GitMetadata)
    );
    assert_eq!(
        targets.classify(&common.join("info/exclude")),
        Some(WatchEvent::IgnoreSource)
    );

    assert_eq!(
        targets.classify(&canonical(&linked).join("tracked.txt")),
        Some(WatchEvent::Worktree {
            path: GitPath::from_bytes(b"tracked.txt")
        })
    );
}

#[test]
fn actually_loaded_config_sources_are_watched() {
    // A file pulled in via include.path can redefine core.excludesFile, so
    // it must be watched even though it lives at no fixed location.
    let (_dir, main, _linked) = fixture();
    let extra = main.parent().expect("temp parent").join("extra.config");
    std::fs::write(&extra, "[user]\n\tname = extra\n").expect("write");
    git(&main, &["config", "include.path", "../../extra.config"]);

    let repo = gix::discover(&main).expect("open repo");
    let targets = WatchTargets::resolve(&repo).expect("resolve targets");

    // Classification happens on backend-delivered (canonical) paths.
    let extra = canonical(&extra);
    assert_eq!(
        targets.classify(&extra),
        Some(WatchEvent::IgnoreSource),
        "an included config file is an ignore source"
    );
    assert!(
        targets
            .metadata_dirs()
            .contains(&extra.parent().expect("parent")),
        "its parent directory must be in the watch set"
    );
}

#[test]
fn empty_and_missing_config_sources_stay_watched() {
    // An empty include target contributes no config section, so section
    // metadata alone would drop it from the watch set — yet a later edit
    // to it (e.g. adding core.excludesFile) must be noticed.
    let (_dir, main, _linked) = fixture();
    let empty = main.parent().expect("temp parent").join("empty.config");
    let missing = main.parent().expect("temp parent").join("missing.config");
    std::fs::write(&empty, "").expect("write");
    git(&main, &["config", "include.path", "../../empty.config"]);
    git(
        &main,
        &["config", "--add", "include.path", "../../missing.config"],
    );

    let repo = gix::discover(&main).expect("open repo");
    let targets = WatchTargets::resolve(&repo).expect("resolve targets");

    assert_eq!(
        targets.classify(&canonical(&empty)),
        Some(WatchEvent::IgnoreSource),
        "an empty include target must stay watched"
    );
    // The missing file cannot be canonicalized as a whole; its parent can.
    let missing_canonical =
        canonical(missing.parent().expect("parent")).join(missing.file_name().expect("file name"));
    assert_eq!(
        targets.classify(&missing_canonical),
        Some(WatchEvent::IgnoreSource),
        "a not-yet-created include target must stay watched"
    );
}

#[test]
fn bare_repositories_have_no_targets() {
    let dir = TempDir::new().expect("temp dir");
    let bare = dir.path().join("bare.git");
    std::fs::create_dir(&bare).expect("mkdir");
    git(&bare, &["init", "-q", "--bare"]);
    let repo = gix::open(&bare).expect("open bare repo");
    assert!(WatchTargets::resolve(&repo).is_err());
}
