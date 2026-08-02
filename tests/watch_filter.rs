//! Tracked-first ignore filtering against a real repository.

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;
use tsuiku::path::GitPath;
use tsuiku::watch::filter::IgnoreFilter;

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

/// `target/` is ignored but contains one tracked file (forced add), the
/// classic shape where pure ignore matching goes wrong.
fn fixture() -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("temp dir");
    let root = dir.path().join("repo");
    std::fs::create_dir(&root).expect("mkdir");
    git(&root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join(".gitignore"), "target/\n*.log\n").expect("write");
    std::fs::create_dir(root.join("target")).expect("mkdir");
    std::fs::create_dir(root.join("src")).expect("mkdir");
    std::fs::write(root.join("target/tracked.txt"), "kept\n").expect("write");
    std::fs::write(root.join("src/main.rs"), "fn main() {}\n").expect("write");
    git(&root, &["add", "-f", ".gitignore", "target/tracked.txt"]);
    git(&root, &["add", "src/main.rs"]);
    git(&root, &["commit", "-q", "-m", "init"]);
    (dir, root)
}

fn keep(filter: &mut IgnoreFilter<'_>, path: &[u8]) -> bool {
    filter.keep(&GitPath::from_bytes(path))
}

#[test]
fn tracked_files_survive_even_inside_ignored_directories() {
    let (_dir, root) = fixture();
    let repo = gix::discover(&root).expect("open repo");
    let mut filter = IgnoreFilter::build(&repo).expect("build filter");

    assert!(keep(&mut filter, b"target/tracked.txt"));
    assert!(keep(&mut filter, b"src/main.rs"));
    assert!(
        keep(&mut filter, b"target"),
        "a directory with tracked descendants stays interesting"
    );
    assert!(
        keep(&mut filter, b"src"),
        "directory events over tracked trees are kept"
    );
}

#[test]
fn untracked_ignored_churn_is_dropped_before_debouncing() {
    let (_dir, root) = fixture();
    let repo = gix::discover(&root).expect("open repo");
    let mut filter = IgnoreFilter::build(&repo).expect("build filter");

    assert!(!keep(&mut filter, b"target/debug/build_artifact.o"));
    assert!(!keep(&mut filter, b"build.log"));
    assert!(
        !keep(&mut filter, b"target/deleted_artifact.o"),
        "ancestor directory patterns apply to paths that no longer exist"
    );
}

#[test]
fn untracked_but_not_ignored_paths_are_kept() {
    let (_dir, root) = fixture();
    let repo = gix::discover(&root).expect("open repo");
    let mut filter = IgnoreFilter::build(&repo).expect("build filter");

    assert!(keep(&mut filter, b"new_file.rs"));
    assert!(keep(&mut filter, b"src/new_module.rs"));
    assert!(
        keep(&mut filter, b"src/main.rs.tmp"),
        "a sibling sharing a tracked prefix is undecided, kept"
    );
}

#[test]
fn a_rebuilt_filter_sees_new_ignore_rules() {
    let (_dir, root) = fixture();
    {
        let repo = gix::discover(&root).expect("open repo");
        let mut filter = IgnoreFilter::build(&repo).expect("build filter");
        assert!(keep(&mut filter, b"generated/out.txt"));
    }

    std::fs::write(root.join(".gitignore"), "target/\n*.log\ngenerated/\n").expect("write");
    let repo = gix::discover(&root).expect("open repo");
    let mut filter = IgnoreFilter::build(&repo).expect("rebuild filter");
    assert!(!keep(&mut filter, b"generated/out.txt"));
}
