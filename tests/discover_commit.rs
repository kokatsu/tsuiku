//! `CommitVsParent` discovery: commit against first parent, root commits, and
//! the special entries a tree can hold.

mod common;

use common::{rev_parse, shared};
use tsuiku::change::{
    ChangeDiscoverer, ChangeQuery, ChangeSet, ChangeStatus, DiffTarget, DiscoverError, EntryMode,
};
use tsuiku::discover::GixDiscoverer;
use tsuiku::ids::{ContentSource, ResolvedContent};
use tsuiku::path::GitPath;
use tsuiku::resolve::{GixResolver, resolve_changes};

// The fixture tags the commits these tests name, so adding history to the
// builder does not silently retarget them.
const ROOT: &str = "fixture-root";
const ANNOTATED_ROOT: &str = "fixture-annotated-root";
const MERGE: &str = "fixture-merge";
const GITLINK: &str = "fixture-gitlink";
const RENAME: &str = "fixture-rename";

fn discover_rev(rev: &str) -> ChangeSet {
    let repo = shared().repo("main");
    let commit = rev_parse(&repo, rev);
    let discoverer = GixDiscoverer::open(&repo).expect("open repository");
    discoverer
        .discover(&ChangeQuery::new(DiffTarget::CommitVsParent { commit }))
        .expect("discover")
}

fn paths(set: &ChangeSet) -> Vec<String> {
    set.changes
        .iter()
        .map(|c| c.display_path().display_escaped())
        .collect()
}

#[test]
fn revision_expressions_resolve_and_peel_to_commits() {
    let repo = shared().repo("main");
    let discoverer = GixDiscoverer::open(&repo).expect("open repository");

    let root = discoverer
        .resolve_commit_revision(ROOT.as_bytes())
        .expect("resolve root tag");
    assert_eq!(root.commit, rev_parse(&repo, ROOT));
    assert!(!root.has_parent);

    let annotated = discoverer
        .resolve_commit_revision(ANNOTATED_ROOT.as_bytes())
        .expect("peel annotated tag");
    assert_eq!(
        annotated.commit,
        rev_parse(&repo, &format!("{ANNOTATED_ROOT}^{{commit}}"))
    );
    assert!(!annotated.has_parent);

    let parent = discoverer
        .resolve_commit_revision(b"fixture-merge^1")
        .expect("resolve revision expression");
    assert_eq!(parent.commit, rev_parse(&repo, "fixture-merge^1"));
    assert!(parent.has_parent);
}

#[test]
fn an_invalid_revision_is_reported_without_terminal_controls() {
    let repo = shared().repo("main");
    let discoverer = GixDiscoverer::open(&repo).expect("open repository");
    let err = discoverer
        .resolve_commit_revision(b"missing\x1b[31m")
        .expect_err("missing revision fails");
    assert!(
        matches!(
            err,
            DiscoverError::InvalidRevision { ref revision }
                if revision == r"missing\x1b[31m"
        ),
        "got {err:?}"
    );
}

#[test]
fn head_is_invalid_in_an_unborn_repository() {
    let repo = shared().repo("unborn");
    let discoverer = GixDiscoverer::open(&repo).expect("open unborn repository");
    let err = discoverer
        .resolve_commit_revision(b"HEAD")
        .expect_err("unborn HEAD has no commit");
    assert!(
        matches!(err, DiscoverError::InvalidRevision { ref revision } if revision == "HEAD"),
        "got {err:?}"
    );
}

#[test]
fn a_root_commit_is_all_additions() {
    let set = discover_rev(ROOT);
    assert!(!set.changes.is_empty());
    for change in &set.changes {
        assert_eq!(change.status, ChangeStatus::Add, "{change:?}");
        assert_eq!(change.old, ContentSource::Absent);
    }
    assert!(paths(&set).contains(&"tracked_modify.txt".to_string()));
}

#[test]
fn a_merge_is_compared_against_its_first_parent_only() {
    // The merge brought in side_only.txt from the second parent. Against the
    // first parent that is an addition; git's combined diff would show
    // nothing, which is why the README does not claim `show` compatibility.
    let set = discover_rev(MERGE);
    let repo = shared().repo("main");
    // The first parent is the main-line commit, not the side branch.
    let first_parent = rev_parse(&repo, "fixture-merge^1");
    assert_ne!(first_parent, rev_parse(&repo, "fixture-merge^2"));
    assert_eq!(paths(&set), vec!["side_only.txt".to_string()]);
    assert_eq!(set.changes[0].status, ChangeStatus::Add);
}

#[test]
fn a_gitlink_addition_carries_the_commit_id() {
    let set = discover_rev(GITLINK);
    let sub = set
        .changes
        .iter()
        .find(|c| c.display_path().as_bytes() == b"submodule_like")
        .expect("the gitlink is reported");
    assert_eq!(sub.status, ChangeStatus::Add);
    assert_eq!(sub.new_mode, Some(EntryMode::Submodule));
    let root = rev_parse(&shared().repo("main"), ROOT);
    assert_eq!(
        sub.new,
        ContentSource::Submodule {
            commit: root,
            dirty: false
        }
    );
}

#[test]
fn a_gitlink_resolves_to_the_body_git_shows() {
    let repo = shared().repo("main");
    let set = discover_rev(GITLINK);
    let resolver = GixResolver::open(&repo).expect("open resolver");
    let resolved = resolve_changes(&resolver, &set).expect("resolve");
    let sub = resolved
        .iter()
        .find(|r| r.change.display_path().as_bytes() == b"submodule_like")
        .expect("the gitlink survives resolution");
    let ResolvedContent::Present(new) = &sub.new else {
        panic!("the new side exists");
    };
    let root = rev_parse(&repo, ROOT);
    assert_eq!(
        String::from_utf8_lossy(&new.bytes),
        format!("Subproject commit {}\n", root.to_hex())
    );
}

#[test]
fn both_sides_of_a_tree_diff_read_from_the_object_database() {
    let repo = shared().repo("main");
    let set = discover_rev("HEAD~1");
    let resolver = GixResolver::open(&repo).expect("open resolver");
    let resolved = resolve_changes(&resolver, &set).expect("resolve");
    assert_eq!(paths(&set), vec!["staged_delete.txt".to_string()]);
    let ResolvedContent::Present(new) = &resolved[0].new else {
        panic!("the new side exists");
    };
    assert_eq!(&*new.bytes, b"staged delete: base\n");
    assert!(new.git_oid.is_some(), "blobs carry their object id");
}

#[test]
fn a_pathspec_limits_a_tree_diff() {
    let repo = shared().repo("main");
    let commit = rev_parse(&repo, ROOT);
    let discoverer = GixDiscoverer::open(&repo).expect("open repository");
    let query = ChangeQuery {
        target: DiffTarget::CommitVsParent { commit },
        pathspecs: vec![GitPath::from_bytes("対句".as_bytes())],
    };
    let set = discoverer.discover(&query).expect("discover");
    assert_eq!(paths(&set), vec!["対句/漢詩.txt".to_string()]);
}

#[test]
fn an_unknown_commit_is_an_error() {
    let repo = shared().repo("main");
    let discoverer = GixDiscoverer::open(&repo).expect("open repository");
    let missing = tsuiku::ids::Oid([0xab; 20]);
    let err = discoverer
        .discover(&ChangeQuery::new(DiffTarget::CommitVsParent {
            commit: missing,
        }))
        .expect_err("a missing commit fails");
    assert!(
        matches!(err, tsuiku::change::DiscoverError::NoSuchCommit { .. }),
        "got {err:?}"
    );
}

#[test]
fn a_bare_repository_can_diff_commits() {
    // There is no worktree, but a commit-to-commit diff never needs one.
    let repo = shared().repo("bare");
    let discoverer = GixDiscoverer::open(&repo).expect("open a bare repository");
    let commit = rev_parse(&repo, RENAME);
    let set = discoverer
        .discover(&ChangeQuery::new(DiffTarget::CommitVsParent { commit }))
        .expect("discover");
    assert_eq!(paths(&set), vec!["committed_rename_dst.txt".to_string()]);
}

#[test]
fn a_bare_repository_can_resolve_a_symbolic_revision() {
    let repo = shared().repo("bare");
    let discoverer = GixDiscoverer::open(&repo).expect("open a bare repository");
    let resolved = discoverer
        .resolve_commit_revision(RENAME.as_bytes())
        .expect("resolve tag");
    assert_eq!(resolved.commit, rev_parse(&repo, RENAME));
    assert!(resolved.has_parent);
}

#[test]
fn a_bare_repository_rejects_a_worktree_comparison() {
    let repo = shared().repo("bare");
    let discoverer = GixDiscoverer::open(&repo).expect("open a bare repository");
    let err = discoverer
        .discover(&ChangeQuery::new(DiffTarget::WorktreeVsHead))
        .expect_err("there is no worktree to compare against");
    assert!(
        matches!(err, tsuiku::change::DiscoverError::NoWorktree),
        "got {err:?}"
    );
}

#[test]
fn a_committed_rename_is_reported_as_one_change() {
    let set = discover_rev(RENAME);
    assert_eq!(paths(&set), vec!["committed_rename_dst.txt".to_string()]);
    assert_eq!(set.changes[0].status, ChangeStatus::Rename);
}

/// Git applies the pathspec before rename detection, so naming only one side
/// of a rename splits it: the source alone is a deletion, the destination
/// alone is an addition.
fn rename_under_pathspec(pattern: &[u8]) -> ChangeSet {
    let repo = shared().repo("main");
    let commit = rev_parse(&repo, RENAME);
    let discoverer = GixDiscoverer::open(&repo).expect("open repository");
    discoverer
        .discover(&ChangeQuery {
            target: DiffTarget::CommitVsParent { commit },
            pathspecs: vec![GitPath::from_bytes(pattern)],
        })
        .expect("discover")
}

#[test]
fn a_pathspec_matching_only_the_rename_source_is_a_deletion() {
    let set = rename_under_pathspec(b"committed_rename_src.txt");
    assert_eq!(paths(&set), vec!["committed_rename_src.txt".to_string()]);
    assert_eq!(set.changes[0].status, ChangeStatus::Delete);
}

#[test]
fn a_pathspec_matching_only_the_rename_destination_is_an_addition() {
    let set = rename_under_pathspec(b"committed_rename_dst.txt");
    assert_eq!(paths(&set), vec!["committed_rename_dst.txt".to_string()]);
    assert_eq!(set.changes[0].status, ChangeStatus::Add);
}
