//! `WorktreeVsHead` discovery against the fixture repositories.

mod common;

use common::{describe_all, git_oracle, rev_parse, shared};
use tsuiku::change::{
    ChangeDiscoverer, ChangeQuery, ChangeSet, ChangeStatus, DiffTarget, DiscoveryWarning, EntryMode,
};
use tsuiku::discover::GixDiscoverer;
use tsuiku::ids::ContentSource;
use tsuiku::resolve::{GixResolver, resolve_changes};

fn discover(repo_name: &str) -> ChangeSet {
    let repo = shared().repo(repo_name);
    let discoverer = GixDiscoverer::open(&repo).expect("open repository");
    discoverer
        .discover(&ChangeQuery::new(DiffTarget::WorktreeVsHead))
        .expect("discover")
}

fn line_for<'a>(lines: &'a [String], path: &str) -> Option<&'a String> {
    lines.iter().find(|l| {
        let rest = l.split_once(' ').map(|(_, r)| r.trim_start()).unwrap_or(l);
        rest.split(' ').next() == Some(path)
    })
}

// ---------------------------------------------------------------------------
// The git CLI oracle
// ---------------------------------------------------------------------------

#[test]
fn matches_the_git_oracle_on_the_main_fixture() {
    let set = discover("main");
    assert_eq!(describe_all(&set), git_oracle(&shared().repo("main")));
}

#[test]
fn matches_the_git_oracle_with_an_unborn_head() {
    let set = discover("unborn");
    assert_eq!(describe_all(&set), git_oracle(&shared().repo("unborn")));
}

#[test]
fn matches_the_git_oracle_with_unmerged_entries() {
    let set = discover("conflict");
    assert_eq!(describe_all(&set), git_oracle(&shared().repo("conflict")));
}

// ---------------------------------------------------------------------------
// Composite staged/worktree states
// ---------------------------------------------------------------------------

/// Discovery reports what git considers changed. Whether the bytes differ is
/// settled by reading them, so these two layers are asserted separately.
fn resolved_paths(repo_name: &str) -> Vec<String> {
    let repo = shared().repo(repo_name);
    let discoverer = GixDiscoverer::open(&repo).expect("open repository");
    let set = discoverer
        .discover(&ChangeQuery::new(DiffTarget::WorktreeVsHead))
        .expect("discover");
    let resolver = GixResolver::open(&repo).expect("open resolver");
    resolve_changes(&resolver, &set)
        .expect("resolve")
        .iter()
        .map(|r| r.change.display_path().display_escaped())
        .collect()
}

#[test]
fn staged_modify_then_worktree_revert_is_no_difference() {
    // git still reports the path, because the index differs from both sides.
    let set = discover("main");
    assert!(line_for(&describe_all(&set), "compose_revert.txt").is_some());
    // Reading both sides settles it: HEAD and the worktree hold the same bytes.
    assert!(!resolved_paths("main").contains(&"compose_revert.txt".to_string()));
}

#[test]
fn staged_delete_then_worktree_recreate_is_a_modification() {
    let lines = describe_all(&discover("main"));
    let line = line_for(&lines, "compose_recreate.txt").expect("the path is reported");
    assert!(line.starts_with("Modify"), "got {line}");
    assert!(line.contains("old=blob:"), "got {line}");
    assert!(line.ends_with("new=worktree"), "got {line}");
}

#[test]
fn staged_rename_then_worktree_modify_keeps_the_rename() {
    let lines = describe_all(&discover("main"));
    let line = line_for(&lines, "compose_renamed.txt").expect("the path is reported");
    assert!(line.starts_with("Rename"), "got {line}");
    assert!(line.contains("<- compose_rename.txt"), "got {line}");
    // The source path must not also appear as a deletion.
    assert!(
        line_for(&lines, "compose_rename.txt").is_none(),
        "{lines:#?}"
    );
}

#[test]
fn staged_add_then_worktree_delete_is_no_difference() {
    let lines = describe_all(&discover("main"));
    assert!(
        line_for(&lines, "compose_add_delete.txt").is_none(),
        "{lines:#?}"
    );
}

// ---------------------------------------------------------------------------
// Ordinary change kinds and special entries
// ---------------------------------------------------------------------------

#[test]
fn reports_every_ordinary_change_kind() {
    let lines = describe_all(&discover("main"));
    for (path, prefix) in [
        ("tracked_modify.txt", "Modify"),
        ("staged_only.txt", "Modify"),
        ("unstaged_only.txt", "Modify"),
        ("both.txt", "Modify"),
        ("to_delete.txt", "Delete"),
        ("staged_delete.txt", "Delete"),
        ("untracked.txt", "Add"),
        ("renamed.txt", "Rename"),
    ] {
        let line = line_for(&lines, path).unwrap_or_else(|| panic!("{path} missing:\n{lines:#?}"));
        assert!(
            line.starts_with(prefix),
            "expected {prefix} for {path}, got {line}"
        );
    }
}

#[test]
fn ignored_files_are_not_reported() {
    let lines = describe_all(&discover("main"));
    assert!(
        line_for(&lines, "ignored/ignored.txt").is_none(),
        "{lines:#?}"
    );
    assert!(line_for(&lines, "debug.log").is_none(), "{lines:#?}");
}

#[test]
fn an_unchanged_file_is_not_reported() {
    let lines = describe_all(&discover("main"));
    assert!(line_for(&lines, "unchanged.txt").is_none(), "{lines:#?}");
    assert!(line_for(&lines, "empty.txt").is_none(), "{lines:#?}");
}

#[test]
fn symlinks_are_classified_by_their_own_mode() {
    let set = discover("main");
    let added = set
        .changes
        .iter()
        .find(|c| c.display_path().as_bytes() == b"link_added")
        .expect("link_added is reported");
    assert_eq!(added.new_mode, Some(EntryMode::Symlink));

    let retargeted = set
        .changes
        .iter()
        .find(|c| c.display_path().as_bytes() == b"link_retarget")
        .expect("link_retarget is reported");
    assert_eq!(retargeted.old_mode, Some(EntryMode::Symlink));
    assert_eq!(retargeted.new_mode, Some(EntryMode::Symlink));
    assert!(!retargeted.is_type_change());
}

#[test]
fn a_symlink_resolves_to_its_target_string() {
    let repo = shared().repo("main");
    let discoverer = GixDiscoverer::open(&repo).expect("open repository");
    let set = discoverer
        .discover(&ChangeQuery::new(DiffTarget::WorktreeVsHead))
        .expect("discover");
    let resolver = GixResolver::open(&repo).expect("open resolver");
    let resolved = resolve_changes(&resolver, &set).expect("resolve");

    let link = resolved
        .iter()
        .find(|r| r.change.display_path().as_bytes() == b"link_retarget")
        .expect("link_retarget survives resolution");
    let tsuiku::ids::ResolvedContent::Present(new) = &link.new else {
        panic!("the new side exists");
    };
    assert_eq!(&*new.bytes, b"unchanged.txt");
}

#[test]
fn a_mode_only_change_survives_resolution() {
    // Identical bytes, different mode. The content check must not drop it.
    let repo = shared().repo("main");
    let discoverer = GixDiscoverer::open(&repo).expect("open repository");
    let set = discoverer
        .discover(&ChangeQuery::new(DiffTarget::WorktreeVsHead))
        .expect("discover");
    let resolver = GixResolver::open(&repo).expect("open resolver");
    let resolved = resolve_changes(&resolver, &set).expect("resolve");

    let mode_change = resolved
        .iter()
        .find(|r| r.change.display_path().as_bytes() == b"mode_change.sh")
        .expect("mode_change.sh survives resolution");
    let pair = mode_change.pair_id();
    assert_eq!(pair.old, pair.new, "the bytes are the same");
    assert_eq!(mode_change.change.old_mode, Some(EntryMode::File));
    assert_eq!(mode_change.change.new_mode, Some(EntryMode::Executable));
}

#[test]
fn a_gitlink_is_reported_as_a_submodule() {
    let set = discover("main");
    let sub = set
        .changes
        .iter()
        .find(|c| c.display_path().as_bytes() == b"submodule_like")
        .expect("the gitlink is reported");
    assert_eq!(sub.old_mode, Some(EntryMode::Submodule));
    assert!(matches!(sub.old, ContentSource::Submodule { .. }));
    // The directory was never checked out, so the worktree side is absent.
    assert_eq!(sub.status, ChangeStatus::Delete);
}

#[test]
fn binary_content_is_discovered_like_any_other_file() {
    // Classification as binary happens later, over the bytes. Discovery only
    // has to report the change.
    let lines = describe_all(&discover("main"));
    for path in ["invalid_utf8.bin", "nul_valid_utf8.txt"] {
        let line = line_for(&lines, path).unwrap_or_else(|| panic!("{path} missing"));
        assert!(line.starts_with("Modify"), "got {line}");
    }
}

#[test]
fn non_ascii_paths_round_trip() {
    let repo = shared().repo("main");
    let discoverer = GixDiscoverer::open(&repo).expect("open repository");
    // The CJK path is committed and unchanged, so it must not be reported.
    let set = discoverer
        .discover(&ChangeQuery::new(DiffTarget::WorktreeVsHead))
        .expect("discover");
    assert!(
        !set.changes
            .iter()
            .any(|c| c.display_path().display_escaped().contains("漢詩")),
        "an unchanged CJK path must not appear"
    );
}

// ---------------------------------------------------------------------------
// Unmerged entries and unborn HEAD
// ---------------------------------------------------------------------------

#[test]
fn an_unmerged_entry_warns_and_compares_ours_against_the_worktree() {
    let set = discover("conflict");
    assert_eq!(
        set.warnings,
        vec![DiscoveryWarning::Unmerged {
            path: tsuiku::path::GitPath::from_bytes(b"conflicted.txt")
        }]
    );
    let change = set
        .changes
        .iter()
        .find(|c| c.display_path().as_bytes() == b"conflicted.txt")
        .expect("the conflicted path is reported");
    assert_eq!(change.status, ChangeStatus::Modify);
    // Stage 2 is "ours"; comparing against stage 1 or 3 would show the wrong
    // old side.
    let ours = rev_parse(&shared().repo("conflict"), "HEAD:conflicted.txt");
    assert_eq!(change.old, ContentSource::GitBlob { oid: ours });
}

#[test]
fn an_unborn_head_makes_everything_an_addition() {
    let set = discover("unborn");
    assert!(!set.changes.is_empty());
    for change in &set.changes {
        assert_eq!(change.status, ChangeStatus::Add, "{change:?}");
        assert_eq!(change.old, ContentSource::Absent);
    }
}

// ---------------------------------------------------------------------------
// Pathspecs
// ---------------------------------------------------------------------------

#[test]
fn a_pathspec_limits_the_change_set() {
    let repo = shared().repo("main");
    let discoverer = GixDiscoverer::open(&repo).expect("open repository");
    let query = ChangeQuery {
        target: DiffTarget::WorktreeVsHead,
        pathspecs: vec![tsuiku::path::GitPath::from_bytes(b"both.txt")],
    };
    let set = discoverer.discover(&query).expect("discover");
    let paths: Vec<String> = set
        .changes
        .iter()
        .map(|c| c.display_path().display_escaped())
        .collect();
    assert_eq!(paths, vec!["both.txt".to_string()]);
}

// ---------------------------------------------------------------------------
// Repository location and submodules
// ---------------------------------------------------------------------------

#[test]
fn opens_a_repository_from_a_subdirectory() {
    // tsuiku is run from wherever the user happens to be standing, which is
    // usually not the repository root.
    let nested = shared().repo("main").join("対句");
    let discoverer = GixDiscoverer::open(&nested).expect("discover the enclosing repository");
    let set = discoverer
        .discover(&ChangeQuery::new(DiffTarget::WorktreeVsHead))
        .expect("discover");
    assert!(
        set.changes
            .iter()
            .any(|c| c.display_path().as_bytes() == b"both.txt"),
        "paths stay relative to the repository root"
    );
}

#[test]
fn a_recreated_rename_source_is_reported_as_an_addition() {
    // `git mv a b` followed by writing a different file back at `a`. Git
    // reports the rename and the untracked addition; both must survive.
    let lines = describe_all(&discover("main"));
    let renamed = line_for(&lines, "rename_recreated.txt").expect("the rename is reported");
    assert!(renamed.starts_with("Rename"), "got {renamed}");
    assert!(renamed.contains("<- rename_recreate.txt"), "got {renamed}");

    let recreated = line_for(&lines, "rename_recreate.txt").expect("the old path is reported");
    assert!(recreated.starts_with("Add"), "got {recreated}");
    assert!(recreated.contains("old=absent"), "got {recreated}");
}

#[test]
fn matches_the_git_oracle_on_a_superproject() {
    let set = discover("super");
    assert_eq!(describe_all(&set), git_oracle(&shared().repo("super")));
}

#[test]
fn a_dirty_submodule_survives_resolution() {
    // The submodule sits on the recorded commit, but its checkout has
    // uncommitted work. Comparing commit ids alone would call this unchanged.
    let repo = shared().repo("super");
    let discoverer = GixDiscoverer::open(&repo).expect("open repository");
    let set = discoverer
        .discover(&ChangeQuery::new(DiffTarget::WorktreeVsHead))
        .expect("discover");
    let resolver = GixResolver::open(&repo).expect("open resolver");
    let resolved = resolve_changes(&resolver, &set).expect("resolve");

    let sub = resolved
        .iter()
        .find(|r| r.change.display_path().as_bytes() == b"sub")
        .expect("the dirty submodule is still a change");
    assert!(matches!(
        sub.change.new,
        ContentSource::Submodule { dirty: true, .. }
    ));
    let tsuiku::ids::ResolvedContent::Present(new) = &sub.new else {
        panic!("the new side exists");
    };
    assert!(
        String::from_utf8_lossy(&new.bytes).ends_with("-dirty\n"),
        "git renders a dirty submodule with a -dirty suffix"
    );

    // Untracked files alone are not a difference: git reports them in status
    // but emits no diff body, so this one must drop out.
    assert!(
        !resolved
            .iter()
            .any(|r| r.change.display_path().as_bytes() == b"sub_untracked"),
        "a submodule with only untracked files is not a change"
    );
}

#[test]
fn an_ignored_dirty_submodule_shows_only_its_commit() {
    // The superproject sets diff.ignoreSubmodules=dirty and has a staged
    // submodule commit plus a dirty checkout. Git shows the new commit with no
    // `-dirty`, and taking dirtiness from the submodule directly would ignore
    // that configuration.
    let repo = shared().repo("super-ignore");
    let discoverer = GixDiscoverer::open(&repo).expect("open repository");
    let set = discoverer
        .discover(&ChangeQuery::new(DiffTarget::WorktreeVsHead))
        .expect("discover");
    assert_eq!(describe_all(&set), git_oracle(&repo));

    let change = set
        .changes
        .iter()
        .find(|c| c.display_path().as_bytes() == b"sub")
        .expect("the submodule is reported");
    assert!(
        matches!(change.new, ContentSource::Submodule { dirty: false, .. }),
        "got {:?}",
        change.new
    );

    let resolver = GixResolver::open(&repo).expect("open resolver");
    let resolved = resolve_changes(&resolver, &set).expect("resolve");
    let sub = resolved
        .iter()
        .find(|r| r.change.display_path().as_bytes() == b"sub")
        .expect("the moved commit is still a change");
    let tsuiku::ids::ResolvedContent::Present(new) = &sub.new else {
        panic!("the new side exists");
    };
    assert!(
        !String::from_utf8_lossy(&new.bytes).contains("-dirty"),
        "the configuration suppresses the dirty marker"
    );
}

#[test]
fn the_executable_bit_is_not_a_change_when_git_does_not_track_it() {
    // core.fileMode is off. The worktree content matches HEAD again and only
    // the executable bit was set, so git reports no difference at all.
    let repo = shared().repo("filemode");
    let discoverer = GixDiscoverer::open(&repo).expect("open repository");
    let set = discoverer
        .discover(&ChangeQuery::new(DiffTarget::WorktreeVsHead))
        .expect("discover");
    assert_eq!(describe_all(&set), git_oracle(&repo));

    let change = set
        .changes
        .iter()
        .find(|c| c.display_path().as_bytes() == b"s.sh")
        .expect("the staged change puts the path in the list");
    assert_eq!(change.new_mode, Some(EntryMode::File));

    let resolver = GixResolver::open(&repo).expect("open resolver");
    let resolved = resolve_changes(&resolver, &set).expect("resolve");
    assert!(
        !resolved
            .iter()
            .any(|r| r.change.display_path().as_bytes() == b"s.sh"),
        "an untracked mode bit must not become a difference"
    );
}

// ---------------------------------------------------------------------------
// States that depend on git configuration or on the index standing in for the
// worktree
// ---------------------------------------------------------------------------

#[test]
fn an_undone_staged_rename_is_no_difference() {
    // `git mv a b` followed by `mv b a`. The destination is gone and the
    // original file is back untouched, so HEAD's side belongs to the original
    // path again — as one record, not a delete and an add at the same path.
    let lines = describe_all(&discover("main"));
    let line = line_for(&lines, "rename_undo.txt").expect("the path is reported");
    assert!(line.starts_with("Modify"), "got {line}");
    assert!(line.contains("old=blob:"), "got {line}");
    assert_eq!(
        lines
            .iter()
            .filter(|l| l.contains("rename_undo.txt"))
            .count(),
        1,
        "one path, one record:\n{lines:#?}"
    );
    assert!(
        line_for(&lines, "rename_undone.txt").is_none(),
        "{lines:#?}"
    );

    // Same bytes on both sides, so nothing survives resolution.
    assert!(!resolved_paths("main").contains(&"rename_undo.txt".to_string()));
}

#[test]
fn matches_the_git_oracle_on_a_sparse_checkout() {
    let set = discover("sparse");
    assert_eq!(describe_all(&set), git_oracle(&shared().repo("sparse")));
}

#[test]
fn a_sparse_checkout_entry_compares_the_index_not_the_missing_file() {
    // The path is deliberately absent from disk. Calling that a deletion would
    // report every excluded file as removed.
    let set = discover("sparse");
    let change = set
        .changes
        .iter()
        .find(|c| c.display_path().as_bytes() == b"omitted/out.txt")
        .expect("the excluded path is reported");
    assert_eq!(change.status, ChangeStatus::Modify);
    assert!(
        matches!(change.new, ContentSource::GitBlob { .. }),
        "got {:?}",
        change.new
    );

    let repo = shared().repo("sparse");
    let resolver = GixResolver::open(&repo).expect("open resolver");
    let resolved = resolve_changes(&resolver, &set).expect("resolve");
    let entry = resolved
        .iter()
        .find(|r| r.change.display_path().as_bytes() == b"omitted/out.txt")
        .expect("the change survives resolution");
    let tsuiku::ids::ResolvedContent::Present(new) = &entry.new else {
        panic!("the new side exists");
    };
    assert_eq!(&*new.bytes, b"omitted v2\n");
}

#[test]
fn a_symlink_checked_out_as_a_regular_file_keeps_its_mode() {
    // With core.symlinks off the link is a plain file holding its target.
    // Reading the mode off the filesystem would turn restoring the original
    // target into a type change.
    let repo = shared().repo("symlinks-off");
    let discoverer = GixDiscoverer::open(&repo).expect("open repository");
    let set = discoverer
        .discover(&ChangeQuery::new(DiffTarget::WorktreeVsHead))
        .expect("discover");
    assert_eq!(describe_all(&set), git_oracle(&repo));

    let change = set
        .changes
        .iter()
        .find(|c| c.display_path().as_bytes() == b"link")
        .expect("the staged change puts the path in the list");
    assert_eq!(change.old_mode, Some(EntryMode::Symlink));
    assert_eq!(change.new_mode, Some(EntryMode::Symlink));
    assert!(!change.is_type_change());

    let resolver = GixResolver::open(&repo).expect("open resolver");
    let resolved = resolve_changes(&resolver, &set).expect("resolve");
    assert!(
        !resolved
            .iter()
            .any(|r| r.change.display_path().as_bytes() == b"link"),
        "the target is back to what HEAD records, so nothing differs"
    );
}

#[test]
fn a_non_utf8_path_is_escaped_the_same_way_on_both_sides() {
    // APFS and HFS+ reject these names, so the fixture only has one where the
    // filesystem allows it.
    let repo = shared().repo("main");
    let lines = describe_all(&discover("main"));
    let Some(line) = lines.iter().find(|l| l.contains("\\xff")) else {
        return;
    };
    assert!(line.contains("invalid\\xff_path.txt"), "got {line}");
    assert_eq!(lines, git_oracle(&repo));
}
