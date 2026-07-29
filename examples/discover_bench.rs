//! What change discovery costs, and whether it still agrees with git.
//!
//! The gix side calls the shipping `GixDiscoverer`; the git side composes the
//! same answer out of `git status --porcelain=v2`. Timing both against one
//! repository is what the backend decision rests on, and comparing their
//! output keeps the benchmark from measuring a stale copy of the real logic.
//!
//! ```text
//! cargo run --release --example discover_bench -- <repo-dir>
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use tsuiku::change::{ChangeDiscoverer, ChangeQuery, ChangeStatus, DiffTarget, FileChange};
use tsuiku::discover::GixDiscoverer;
use tsuiku::ids::ContentSource;
use tsuiku::path::GitPath;

// ---------------------------------------------------------------------------
// A shape both sides can produce
// ---------------------------------------------------------------------------

fn describe(change: &FileChange) -> String {
    let kind = match change.status {
        ChangeStatus::Add => "Add",
        ChangeStatus::Delete => "Delete",
        ChangeStatus::Modify => "Modify",
        ChangeStatus::Rename => "Rename",
    };
    let from = match (&change.old_path, &change.new_path) {
        (Some(old), Some(new)) if old != new => format!(" <- {}", old.display_escaped()),
        _ => String::new(),
    };
    format!(
        "{kind:<6} {path}{from} old={old} new={new}",
        path = change.display_path().display_escaped(),
        old = match &change.old {
            ContentSource::Absent => "absent".to_string(),
            ContentSource::GitBlob { oid } => format!("blob:{}", &oid.to_hex()[..7]),
            ContentSource::Worktree { .. } => "worktree".to_string(),
            ContentSource::Submodule { commit, .. } => {
                format!("submodule:{}", &commit.to_hex()[..7])
            }
        },
        new = match &change.new {
            ContentSource::Absent => "absent",
            // On the new side a blob means the index stands in for the
            // worktree, as git does for skip-worktree entries.
            ContentSource::GitBlob { .. } => "index",
            ContentSource::Worktree { .. } => "worktree",
            ContentSource::Submodule { dirty: true, .. } => "submodule-dirty",
            ContentSource::Submodule { dirty: false, .. } => "submodule",
        },
    )
}

// ---------------------------------------------------------------------------
// git CLI
// ---------------------------------------------------------------------------

const NULL_MODE: &str = "000000";

struct Record {
    /// Raw bytes: git paths are arbitrary byte strings, and turning them into
    /// text here would disagree with how the discoverer escapes them.
    path: Vec<u8>,
    old_path: Option<Vec<u8>>,
    old_blob: Option<String>,
    present: bool,
    submodule: bool,
    dirty: bool,
}

fn discover_cli(repo: &Path) -> Vec<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=all",
            "--renames",
        ])
        .output()
        .expect("run git status");
    assert!(out.status.success(), "git status failed");

    // Fields are NUL-terminated. A rename entry spends two of them: the
    // current path first, the original path second.
    let mut fields = out
        .stdout
        .split(|b| *b == 0)
        .filter(|f| !f.is_empty())
        .map(<[u8]>::to_vec)
        .collect::<Vec<_>>()
        .into_iter();

    let skipped = skip_worktree_paths(repo);
    let mut records: Vec<Record> = Vec::new();
    while let Some(field) = fields.next() {
        match field.first() {
            Some(b'1') => {
                let (meta, path) = split_entry(&field, 8);
                records.push(entry(&meta, 3, 5, 6, None, path));
            }
            Some(b'2') => {
                let (meta, path) = split_entry(&field, 9);
                let orig = fields.next().expect("rename entry lacks its origPath");
                let mut record = entry(&meta, 3, 5, 6, Some(orig), path);
                // A rename whose destination is not on disk is no rename at
                // all: HEAD's side belongs back at the original path.
                if !record.present {
                    record.path = record.old_path.take().expect("a rename has an origPath");
                }
                records.push(record);
            }
            // Stage 2 is "ours", which is what HEAD holds.
            Some(b'u') => {
                let (meta, path) = split_entry(&field, 10);
                records.push(entry(&meta, 4, 6, 8, None, path));
            }
            Some(b'?') => {
                let (_, path) = split_entry(&field, 1);
                records.push(Record {
                    path,
                    old_path: None,
                    old_blob: None,
                    present: true,
                    submodule: false,
                    dirty: false,
                });
            }
            Some(b'!') => {}
            _ => panic!("unexpected porcelain v2 field"),
        }
    }

    // A path staged as deleted and written again arrives twice: once as a
    // deletion, once as an untracked file.
    let mut by_path: BTreeMap<Vec<u8>, Record> = BTreeMap::new();
    for r in records {
        by_path
            .entry(r.path.clone())
            .and_modify(|existing| {
                existing.present |= r.present;
                if existing.old_blob.is_none() {
                    existing.old_blob = r.old_blob.clone();
                }
            })
            .or_insert(r);
    }

    let mut lines: Vec<String> = by_path
        .into_values()
        .filter_map(|r| {
            let skipped = skipped.contains(&r.path);
            render(&r, skipped)
        })
        .collect();
    lines.sort();
    lines
}

/// Paths carrying the skip-worktree bit. `git status` reports them as present
/// with an ordinary mode, so the bit is the only way to tell that the index,
/// not the filesystem, stands in for the new side.
fn skip_worktree_paths(repo: &Path) -> BTreeSet<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["ls-files", "-v", "-z"])
        .output()
        .expect("run git ls-files");
    assert!(out.status.success(), "git ls-files failed");
    out.stdout
        .split(|b| *b == 0)
        .filter(|entry| entry.first() == Some(&b'S'))
        .map(|entry| entry[2..].to_vec())
        .collect()
}

/// Split a status field into its leading space-separated metadata tokens and
/// the raw path bytes that follow. Only the metadata is ASCII, and a path may
/// contain spaces, so the split has to be by token count.
fn split_entry(field: &[u8], meta_tokens: usize) -> (Vec<String>, Vec<u8>) {
    let mut tokens = Vec::with_capacity(meta_tokens);
    let mut rest = field;
    for _ in 0..meta_tokens {
        match rest.iter().position(|b| *b == b' ') {
            Some(i) => {
                tokens.push(String::from_utf8_lossy(&rest[..i]).into_owned());
                rest = &rest[i + 1..];
            }
            None => {
                tokens.push(String::from_utf8_lossy(rest).into_owned());
                rest = &[];
            }
        }
    }
    (tokens, rest.to_vec())
}

fn entry(
    meta: &[String],
    head_mode: usize,
    worktree_mode: usize,
    head_oid: usize,
    old_path: Option<Vec<u8>>,
    path: Vec<u8>,
) -> Record {
    let sub = meta[2].as_bytes();
    Record {
        path,
        old_path,
        old_blob: (meta[head_mode] != NULL_MODE).then(|| meta[head_oid].clone()),
        present: meta[worktree_mode] != NULL_MODE,
        submodule: meta[head_mode] == "160000" || meta[worktree_mode] == "160000",
        // `S<c><m><u>`: only modified tracked files earn the dirty marker.
        dirty: sub.first() == Some(&b'S') && sub.get(2) == Some(&b'M'),
    }
}

fn render(r: &Record, skipped: bool) -> Option<String> {
    let old_present = r.old_blob.is_some();
    let renamed = r.old_path.as_deref().is_some_and(|p| p != r.path);
    let kind = match (old_present, r.present) {
        (false, false) => return None,
        (false, true) => "Add",
        (true, false) => "Delete",
        (true, true) if renamed => "Rename",
        (true, true) => "Modify",
    };
    let from = match &r.old_path {
        Some(p) if renamed && old_present && r.present => {
            format!(" <- {}", GitPath::from_bytes(p).display_escaped())
        }
        _ => String::new(),
    };
    let old = match &r.old_blob {
        Some(oid) if r.submodule => format!("submodule:{}", &oid[..7]),
        Some(oid) => format!("blob:{}", &oid[..7]),
        None => "absent".to_string(),
    };
    let new = match (r.present, r.submodule, r.dirty) {
        (false, _, _) => "absent",
        (true, true, true) => "submodule-dirty",
        (true, true, false) => "submodule",
        (true, false, _) if skipped => "index",
        (true, false, _) => "worktree",
    };
    Some(format!(
        "{kind:<6} {path}{from} old={old} new={new}",
        path = GitPath::from_bytes(&r.path).display_escaped()
    ))
}

// ---------------------------------------------------------------------------

fn discover_gix(repo: &Path) -> Vec<String> {
    let discoverer = GixDiscoverer::open(repo).expect("open repository");
    let set = discoverer
        .discover(&ChangeQuery::new(DiffTarget::WorktreeVsHead))
        .expect("discover");
    let mut lines: Vec<String> = set.changes.iter().map(describe).collect();
    lines.sort();
    lines
}

/// Repeat so the numbers reflect steady state; the fastest run is the one
/// least polluted by unrelated system noise.
fn bench(mut f: impl FnMut() -> Vec<String>) -> (Vec<String>, Duration) {
    let mut best = Duration::MAX;
    let mut out = Vec::new();
    for _ in 0..7 {
        let t = Instant::now();
        out = f();
        best = best.min(t.elapsed());
    }
    (out, best)
}

fn main() {
    let repo = std::env::args()
        .nth(1)
        .expect("usage: discover_bench <repo-dir>");
    let repo = Path::new(&repo);

    let (cli, cli_time) = bench(|| discover_cli(repo));
    let (gix, gix_time) = bench(|| discover_gix(repo));

    println!("== git CLI ({} records, {cli_time:?}) ==", cli.len());
    for line in &cli {
        println!("  {line}");
    }
    println!("\n== gix ({} records, {gix_time:?}) ==", gix.len());
    for line in &gix {
        println!("  {line}");
    }

    println!("\n== differences ==");
    let mut differences = 0;
    for line in &cli {
        if !gix.contains(line) {
            differences += 1;
            println!("  cli only: {line}");
        }
    }
    for line in &gix {
        if !cli.contains(line) {
            differences += 1;
            println!("  gix only: {line}");
        }
    }
    if differences == 0 {
        println!("  none");
    }
    println!("\ncli={cli_time:?} gix={gix_time:?} differences={differences}");
}
