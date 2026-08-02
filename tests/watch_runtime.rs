//! End-to-end watch runtime: real repository, real filesystem events.
//!
//! Filesystem notification latency differs wildly between backends
//! (inotify is immediate, FSEvents coalesces), so these tests only wait
//! for updates with generous deadlines and never assert on the absence of
//! an update within a fixed time. Filtering is asserted structurally: a
//! batch that *did* arrive must not contain paths the tracked-first filter
//! drops before debouncing.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tempfile::TempDir;
use tsuiku::watch::runtime::{WatchCoordinator, WatchUpdate};

/// Serializes the tests in this binary: a dozen concurrent FSEvents
/// streams under heavy churn drop deliveries on macOS, which shows up as
/// spurious timeouts. One live watcher at a time is also closer to how the
/// application runs.
fn serialized() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

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

fn fixture() -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("temp dir");
    let root = dir.path().join("repo");
    std::fs::create_dir(&root).expect("mkdir");
    git(&root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join(".gitignore"), "target/\n").expect("write");
    std::fs::write(root.join("tracked.txt"), "one\n").expect("write");
    std::fs::create_dir(root.join("target")).expect("mkdir");
    git(&root, &["add", ".gitignore", "tracked.txt"]);
    git(&root, &["commit", "-q", "-m", "init"]);
    (dir, root)
}

fn wait_refresh(coordinator: &WatchCoordinator, deadline: Duration) -> WatchUpdate {
    let until = Instant::now() + deadline;
    loop {
        if let Some(update) = coordinator.poll() {
            return update;
        }
        assert!(
            Instant::now() < until,
            "no watch update within {deadline:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The initial refresh doubles as the "watcher is armed" signal; edits made
/// before it may fall into the startup gap and produce no events. Its batch
/// is marked unknown so gap-time changes force a re-read of displayed
/// content instead of a carry-over.
fn wait_armed(coordinator: &WatchCoordinator) {
    let update = wait_refresh(coordinator, Duration::from_secs(10));
    let WatchUpdate::Refresh { batch, .. } = update else {
        panic!("watch degraded before arming");
    };
    assert!(batch.unknown, "the startup gap must not allow carry-over");
    assert!(batch.paths.is_empty());
}

fn changed_paths(update: &WatchUpdate) -> Vec<String> {
    match update {
        WatchUpdate::Refresh { changes, .. } => changes
            .changes
            .iter()
            .map(|change| change.display_path().display_escaped())
            .collect(),
        WatchUpdate::Degraded { reason } => panic!("watch degraded: {reason}"),
    }
}

#[test]
fn a_worktree_edit_produces_a_refreshed_snapshot() {
    let _serial = serialized();
    let (_dir, root) = fixture();
    let coordinator = WatchCoordinator::start(root.clone());
    wait_armed(&coordinator);

    // Ignored churn first, then a real edit: the refresh triggered by the
    // edit must not carry the churn path (dropped before debouncing).
    std::fs::write(root.join("target/artifact.o"), "junk").expect("write");
    std::fs::write(root.join("tracked.txt"), "two\n").expect("write");

    let update = wait_refresh(&coordinator, Duration::from_secs(10));
    let paths = changed_paths(&update);
    assert!(
        paths.contains(&"tracked.txt".to_owned()),
        "refresh must include the modified tracked file, got {paths:?}"
    );
    assert!(
        !paths.iter().any(|path| path.starts_with("target/")),
        "ignored untracked churn must not appear, got {paths:?}"
    );
    let WatchUpdate::Refresh { batch, .. } = update else {
        unreachable!("checked above");
    };
    assert!(
        !batch
            .paths
            .iter()
            .any(|path| path.as_bytes().starts_with(b"target/")),
        "churn under target/ must be dropped before the debouncer"
    );
}

#[test]
fn reverting_the_edit_empties_the_next_snapshot() {
    let _serial = serialized();
    let (_dir, root) = fixture();
    let coordinator = WatchCoordinator::start(root.clone());
    wait_armed(&coordinator);

    std::fs::write(root.join("tracked.txt"), "two\n").expect("write");
    let first = wait_refresh(&coordinator, Duration::from_secs(10));
    assert!(changed_paths(&first).contains(&"tracked.txt".to_owned()));

    std::fs::write(root.join("tracked.txt"), "one\n").expect("write");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let update = wait_refresh(&coordinator, Duration::from_secs(10));
        let paths = changed_paths(&update);
        // Depending on backend timing the revert may arrive merged with a
        // still-dirty intermediate state; wait for the settled snapshot.
        if paths.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "snapshot never settled to clean, last {paths:?}"
        );
    }
}

#[test]
fn a_config_change_is_seen_by_the_rebuilt_matcher() {
    let _serial = serialized();
    // gix snapshots config at open; the worker must reopen the repository
    // after a metadata batch, or a newly configured excludes file would be
    // invisible to the rebuilt matcher.
    let (_dir, root) = fixture();
    let coordinator = WatchCoordinator::start(root.clone());
    wait_armed(&coordinator);

    let excludes = root.join("my-excludes.txt");
    std::fs::write(&excludes, "generated/\nmy-excludes.txt\n").expect("write");
    git(
        &root,
        &[
            "config",
            "core.excludesFile",
            excludes.to_str().expect("utf8 tmp path"),
        ],
    );
    // The config edit itself produces a metadata refresh.
    let first = wait_refresh(&coordinator, Duration::from_secs(10));
    assert!(matches!(first, WatchUpdate::Refresh { .. }));

    std::fs::create_dir(root.join("generated")).expect("mkdir");
    std::fs::write(root.join("generated/out.txt"), "junk").expect("write");
    std::fs::write(root.join("tracked.txt"), "two\n").expect("write");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let update = wait_refresh(&coordinator, Duration::from_secs(10));
        let WatchUpdate::Refresh { batch, .. } = &update else {
            panic!("watch degraded");
        };
        if batch
            .paths
            .iter()
            .any(|path| path.as_bytes() == b"tracked.txt")
        {
            assert!(
                !batch
                    .paths
                    .iter()
                    .any(|path| path.as_bytes().starts_with(b"generated/")),
                "churn ignored via the *new* excludes file must be dropped"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the tracked edit never produced a refresh"
        );
    }
}

/// Wait until a refresh whose change list satisfies `accept` arrives.
fn wait_for_snapshot(
    coordinator: &WatchCoordinator,
    what: &str,
    accept: impl Fn(&[String]) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let update = wait_refresh(coordinator, Duration::from_secs(10));
        if accept(&changed_paths(&update)) {
            return;
        }
        assert!(Instant::now() < deadline, "never observed: {what}");
    }
}

#[test]
fn staging_keeps_the_composite_worktree_vs_head_diff() {
    let _serial = serialized();
    let (_dir, root) = fixture();
    let coordinator = WatchCoordinator::start(root.clone());
    wait_armed(&coordinator);

    std::fs::write(root.join("tracked.txt"), "two\n").expect("write");
    wait_for_snapshot(&coordinator, "the edit", |paths| {
        paths.contains(&"tracked.txt".to_owned())
    });

    // Staging moves the change into the index; the composite comparison
    // (HEAD vs final worktree) must keep showing it, via a metadata-driven
    // refresh rather than silence.
    git(&root, &["add", "tracked.txt"]);
    wait_for_snapshot(&coordinator, "the staged state", |paths| {
        paths.contains(&"tracked.txt".to_owned())
    });
}

#[test]
fn reset_hard_clears_the_snapshot() {
    let _serial = serialized();
    let (_dir, root) = fixture();
    let coordinator = WatchCoordinator::start(root.clone());
    wait_armed(&coordinator);

    std::fs::write(root.join("tracked.txt"), "two\n").expect("write");
    wait_for_snapshot(&coordinator, "the edit", |paths| {
        paths.contains(&"tracked.txt".to_owned())
    });

    git(&root, &["reset", "-q", "--hard"]);
    wait_for_snapshot(&coordinator, "a clean tree after reset", |paths| {
        paths.is_empty()
    });
}

#[test]
fn a_branch_switch_rearms_the_head_ref_watch() {
    let _serial = serialized();
    let (_dir, root) = fixture();
    let coordinator = WatchCoordinator::start(root.clone());
    wait_armed(&coordinator);

    // Switch to a new hierarchical branch: the watch must follow the new
    // resolved ref into its deeper directory.
    git(&root, &["checkout", "-q", "-b", "feature/deep/branch"]);
    std::fs::write(root.join("tracked.txt"), "on-branch\n").expect("write");
    wait_for_snapshot(&coordinator, "the on-branch edit", |paths| {
        paths.contains(&"tracked.txt".to_owned())
    });

    // Committing updates refs/heads/feature/deep/branch; only a correctly
    // re-armed ref watch turns that into a metadata refresh that clears
    // the change set.
    git(&root, &["commit", "-q", "-am", "absorb on branch"]);
    wait_for_snapshot(&coordinator, "a clean tree after the commit", |paths| {
        paths.is_empty()
    });
}

#[test]
fn files_created_while_an_ignore_rule_is_lifted_are_not_lost() {
    let _serial = serialized();
    // The matcher-dirty window: after .gitignore stops ignoring generated/,
    // a file created immediately afterwards must not be dropped by the old
    // matcher — conservative holding applies until the rebuilt matcher and
    // snapshot arrive together.
    let (_dir, root) = fixture();
    std::fs::write(root.join(".gitignore"), "target/\ngenerated/\n").expect("write");
    git(&root, &["add", ".gitignore"]);
    git(&root, &["commit", "-q", "-m", "ignore generated"]);
    std::fs::create_dir(root.join("generated")).expect("mkdir");

    let coordinator = WatchCoordinator::start(root.clone());
    wait_armed(&coordinator);

    // Lift the rule and immediately create a file that the *old* matcher
    // would still drop.
    std::fs::write(root.join(".gitignore"), "target/\n").expect("write");
    std::fs::write(root.join("generated/new.txt"), "kept\n").expect("write");

    wait_for_snapshot(&coordinator, "the no-longer-ignored file", |paths| {
        paths.contains(&"generated/new.txt".to_owned()) && paths.contains(&".gitignore".to_owned())
    });
}

#[test]
fn linked_worktree_edits_and_common_dir_ignores_are_tracked() {
    let _serial = serialized();
    let (dir, root) = fixture();
    let linked = dir.path().join("linked");
    git(
        &root,
        &[
            "worktree",
            "add",
            "-q",
            linked.to_str().expect("utf8 tmp path"),
            "-b",
            "wt",
        ],
    );
    let coordinator = WatchCoordinator::start(linked.clone());
    wait_armed(&coordinator);

    // An edit in the linked worktree is discovered through its own gitdir.
    std::fs::write(linked.join("tracked.txt"), "linked-edit\n").expect("write");
    wait_for_snapshot(&coordinator, "the linked-worktree edit", |paths| {
        paths.contains(&"tracked.txt".to_owned())
    });

    // info/exclude lives in the shared common dir; a pattern added there
    // must reach the linked worktree's rebuilt matcher.
    std::fs::write(root.join(".git/info/exclude"), "junk/\n").expect("write");
    std::fs::create_dir(linked.join("junk")).expect("mkdir");
    std::fs::write(linked.join("junk/artifact.o"), "junk").expect("write");
    std::fs::write(linked.join("tracked.txt"), "linked-edit-2\n").expect("write");
    wait_for_snapshot(
        &coordinator,
        "a refresh filtered by the common-dir exclude",
        |paths| {
            paths.contains(&"tracked.txt".to_owned())
                && !paths.iter().any(|path| path.starts_with("junk/"))
        },
    );

    // Committing in the linked worktree exercises its per-worktree index
    // and the shared refs.
    git(&linked, &["commit", "-q", "-am", "absorb in worktree"]);
    wait_for_snapshot(&coordinator, "a clean linked worktree", |paths| {
        paths.is_empty()
    });
}

#[test]
fn an_injected_rescan_runs_full_recovery_and_reports_a_lossy_refresh() {
    let _serial = serialized();
    let (_dir, root) = fixture();
    let coordinator = WatchCoordinator::start(root.clone());
    wait_armed(&coordinator);

    std::fs::write(root.join("tracked.txt"), "two\n").expect("write");
    wait_for_snapshot(&coordinator, "the edit", |paths| {
        paths.contains(&"tracked.txt".to_owned())
    });

    // Possible event loss: the recovery must re-arm, rediscover, and
    // deliver a refresh marked lossy — the marker the app side relies on
    // to re-read displayed content instead of carrying it over.
    coordinator.inject_rescan_for_test();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let update = wait_refresh(&coordinator, Duration::from_secs(10));
        let WatchUpdate::Refresh { changes, batch } = &update else {
            panic!("recovery must not degrade");
        };
        if batch.lossy() {
            let paths: Vec<String> = changes
                .changes
                .iter()
                .map(|change| change.display_path().display_escaped())
                .collect();
            assert!(
                paths.contains(&"tracked.txt".to_owned()),
                "the recovered walk must still see the edit, got {paths:?}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the injected rescan never produced a lossy refresh"
        );
    }

    // The recovered watcher keeps working: a later edit still arrives.
    std::fs::write(root.join("tracked.txt"), "three\n").expect("write");
    wait_for_snapshot(&coordinator, "an edit after recovery", |paths| {
        paths.contains(&"tracked.txt".to_owned())
    });
}

#[test]
fn packed_ref_updates_in_a_linked_worktree_stay_visible() {
    let _serial = serialized();
    let (dir, root) = fixture();
    let linked = dir.path().join("linked");
    git(
        &root,
        &[
            "worktree",
            "add",
            "-q",
            linked.to_str().expect("utf8 tmp path"),
            "-b",
            "wt",
        ],
    );
    let coordinator = WatchCoordinator::start(linked.clone());
    wait_armed(&coordinator);

    std::fs::write(linked.join("tracked.txt"), "linked-edit\n").expect("write");
    wait_for_snapshot(&coordinator, "the linked-worktree edit", |paths| {
        paths.contains(&"tracked.txt".to_owned())
    });

    // Migrate every ref into packed-refs and drop the loose files. The
    // packed-refs rewrite itself is a metadata change, and the snapshot
    // must keep showing the edit afterwards.
    git(&root, &["pack-refs", "--all", "--prune"]);
    wait_for_snapshot(&coordinator, "the edit surviving pack-refs", |paths| {
        paths.contains(&"tracked.txt".to_owned())
    });

    // Updating the current ref after the prune (the commit re-creates the
    // loose file) must still be noticed and clear the change set.
    git(&linked, &["commit", "-q", "-am", "absorb after pack-refs"]);
    wait_for_snapshot(&coordinator, "a clean tree after the commit", |paths| {
        paths.is_empty()
    });
}

#[test]
fn a_late_created_excludes_directory_is_eventually_watched() {
    let _serial = serialized();
    // core.excludesFile pointing into a directory that does not exist yet:
    // the nearest existing ancestor is watched, the creation triggers a
    // rearm, and churn matching the newly created excludes file is dropped.
    let (dir, root) = fixture();
    let excludes_dir = dir.path().join("missing").join("sub");
    let excludes = excludes_dir.join("exclude");
    let coordinator = WatchCoordinator::start(root.clone());
    wait_armed(&coordinator);

    git(
        &root,
        &[
            "config",
            "core.excludesFile",
            excludes.to_str().expect("utf8 tmp path"),
        ],
    );
    let first = wait_refresh(&coordinator, Duration::from_secs(10));
    assert!(matches!(first, WatchUpdate::Refresh { .. }));

    std::fs::create_dir_all(&excludes_dir).expect("mkdir");
    std::fs::write(&excludes, "generated/\n").expect("write");

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut churn_written = false;
    loop {
        // Keep producing the probe pair until a refresh shows the new
        // excludes file in effect; the number of intermediate rearms
        // depends on event coalescing.
        if !churn_written {
            let _ = std::fs::create_dir(root.join("generated"));
            std::fs::write(root.join("generated/out.txt"), "junk").expect("write");
            std::fs::write(root.join("tracked.txt"), format!("{:?}\n", Instant::now()))
                .expect("write");
            churn_written = true;
        }
        let update = wait_refresh(&coordinator, Duration::from_secs(15));
        let WatchUpdate::Refresh { batch, .. } = &update else {
            panic!("watch degraded");
        };
        if batch
            .paths
            .iter()
            .any(|path| path.as_bytes() == b"tracked.txt")
        {
            if batch
                .paths
                .iter()
                .any(|path| path.as_bytes().starts_with(b"generated/"))
            {
                // The matcher had not caught up yet for this round; probe
                // again with fresh writes.
                churn_written = false;
            } else {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the late-created excludes file never took effect"
        );
    }
}

#[test]
fn deleting_the_git_dir_degrades_instead_of_going_silent() {
    let _serial = serialized();
    let (_dir, root) = fixture();
    let coordinator = WatchCoordinator::start(root.clone());
    wait_armed(&coordinator);

    std::fs::remove_dir_all(root.join(".git")).expect("remove git dir");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match wait_refresh(&coordinator, Duration::from_secs(10)) {
            WatchUpdate::Degraded { .. } => break,
            WatchUpdate::Refresh { .. } => {
                // Intermediate refreshes may still succeed while directory
                // deletion events trickle in.
                assert!(
                    Instant::now() < deadline,
                    "losing the repository must end in Degraded, not silence"
                );
            }
        }
    }
}

#[test]
fn a_commit_is_noticed_via_git_metadata() {
    let _serial = serialized();
    let (_dir, root) = fixture();
    let coordinator = WatchCoordinator::start(root.clone());
    wait_armed(&coordinator);

    std::fs::write(root.join("tracked.txt"), "two\n").expect("write");
    let first = wait_refresh(&coordinator, Duration::from_secs(10));
    assert!(changed_paths(&first).contains(&"tracked.txt".to_owned()));

    git(&root, &["commit", "-q", "-am", "absorb"]);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let update = wait_refresh(&coordinator, Duration::from_secs(10));
        if changed_paths(&update).is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the commit must eventually clear the change set"
        );
    }
}
