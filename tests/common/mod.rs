#![allow(dead_code)] // Each test binary uses a different part of this module.

//! Fixture repositories, and the git-CLI oracle the gix discoverer is checked
//! against.
//!
//! The oracle parses `git status --porcelain=v2`, which carries everything the
//! composition needs: HEAD's blob id per path, index-recorded renames,
//! untracked entries, and unmerged stages. It exists so that a change in gix's
//! behaviour shows up as a test failure rather than as a wrong diff.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;
use tsuiku::change::{ChangeSet, ChangeStatus, FileChange};
use tsuiku::ids::ContentSource;
use tsuiku::path::GitPath;

pub struct Fixtures {
    dir: TempDir,
}

impl Fixtures {
    pub fn build() -> Self {
        let dir = TempDir::new().expect("create temp dir");
        let script =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/build_fixture_repos.sh");
        let out = Command::new("bash")
            .arg(&script)
            .arg(dir.path().join("repos"))
            .output()
            .expect("run fixture builder");
        assert!(
            out.status.success(),
            "fixture builder failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        Self { dir }
    }

    pub fn repo(&self, name: &str) -> PathBuf {
        self.dir.path().join("repos").join(name)
    }
}

/// Built once per test binary; the builder runs git a few hundred times.
pub fn shared() -> &'static Fixtures {
    static FIXTURES: std::sync::OnceLock<Fixtures> = std::sync::OnceLock::new();
    FIXTURES.get_or_init(Fixtures::build)
}

pub fn rev_parse(repo: &Path, rev: &str) -> tsuiku::ids::Oid {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", rev])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git rev-parse");
    assert!(out.status.success(), "git rev-parse {rev} failed");
    let hex = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let mut bytes = [0u8; 20];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).expect("hex digit");
    }
    tsuiku::ids::Oid(bytes)
}

/// One line per change, in a form both the discoverer and the oracle can
/// produce.
pub fn describe(change: &FileChange) -> String {
    let kind = match change.status {
        ChangeStatus::Add => "Add",
        ChangeStatus::Delete => "Delete",
        ChangeStatus::Modify => "Modify",
        ChangeStatus::Rename => "Rename",
    };
    let path = change.display_path().display_escaped();
    let from = match (&change.old_path, &change.new_path) {
        (Some(old), Some(new)) if old != new => format!(" <- {}", old.display_escaped()),
        _ => String::new(),
    };
    format!(
        "{kind:<6} {path}{from} old={} new={}",
        render_source(&change.old, Side::Old),
        render_source(&change.new, Side::New)
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Old,
    New,
}

/// The old side names the object it reads; the new side only names where it
/// reads from, because the bytes on disk have no id until they are read. The
/// oracle can produce exactly this much from `git status`.
fn render_source(source: &ContentSource, side: Side) -> String {
    match (source, side) {
        (ContentSource::Absent, _) => "absent".to_string(),
        (ContentSource::GitBlob { oid }, Side::Old) => format!("blob:{}", &oid.to_hex()[..7]),
        // On the new side a blob means the index is standing in for the
        // worktree, which is what git does for skip-worktree entries.
        (ContentSource::GitBlob { .. }, Side::New) => "index".to_string(),
        (ContentSource::Worktree { .. }, _) => "worktree".to_string(),
        (ContentSource::Submodule { commit, .. }, Side::Old) => {
            format!("submodule:{}", &commit.to_hex()[..7])
        }
        (ContentSource::Submodule { dirty, .. }, Side::New) => if *dirty {
            "submodule-dirty"
        } else {
            "submodule"
        }
        .to_string(),
    }
}

pub fn describe_all(set: &ChangeSet) -> Vec<String> {
    let mut lines: Vec<String> = set.changes.iter().map(describe).collect();
    lines.sort();
    lines
}

// ---------------------------------------------------------------------------
// git CLI oracle
// ---------------------------------------------------------------------------

const NULL_MODE: &str = "000000";

#[derive(Clone)]
struct OracleRecord {
    /// Raw bytes, because git paths are arbitrary byte strings and turning
    /// them into text here would disagree with how the discoverer escapes
    /// them.
    path: Vec<u8>,
    old_path: Option<Vec<u8>>,
    /// `None` means HEAD has nothing at this path.
    old_blob: Option<String>,
    /// Whether the new side exists at all.
    present: bool,
    submodule: bool,
    /// A submodule whose checkout carries uncommitted work. Git renders this
    /// by suffixing the commit id with `-dirty`.
    dirty: bool,
}

/// Compose "HEAD against the final worktree" from `git status --porcelain=v2`.
pub fn git_oracle(repo: &Path) -> Vec<String> {
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
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
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
    let mut records: Vec<OracleRecord> = Vec::new();
    while let Some(field) = fields.next() {
        match field.first() {
            // 1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>
            Some(b'1') => {
                let (meta, path) = split_entry(&field, 8);
                records.push(entry(&meta, 3, 5, 6, None, path));
            }
            // 2 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <X><score> <path> NUL <orig>
            Some(b'2') => {
                let (meta, path) = split_entry(&field, 9);
                let orig = fields.next().expect("rename entry lacks its origPath");
                let mut record = entry(&meta, 3, 5, 6, Some(orig), path);
                // A rename whose destination is not on disk is no rename at
                // all: nothing holds HEAD's side except the original path, so
                // that is where it belongs.
                if !record.present {
                    record.path = record.old_path.take().expect("a rename has an origPath");
                }
                records.push(record);
            }
            // u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>
            // Stage 2 is "ours", which is what HEAD holds.
            Some(b'u') => {
                let (meta, path) = split_entry(&field, 10);
                records.push(entry(&meta, 4, 6, 8, None, path));
            }
            Some(b'?') => {
                let (_, path) = split_entry(&field, 1);
                records.push(OracleRecord {
                    path,
                    old_path: None,
                    old_blob: None,
                    present: true,
                    submodule: false,
                    dirty: false,
                });
            }
            Some(b'!') => {}
            _ => panic!(
                "unexpected porcelain v2 field {:?}",
                String::from_utf8_lossy(&field)
            ),
        }
    }

    // A path staged as deleted and written again arrives twice: once as a
    // deletion, once as an untracked file. Against HEAD it is one change.
    let mut by_path: BTreeMap<Vec<u8>, OracleRecord> = BTreeMap::new();
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
            render_oracle(&r, skipped)
        })
        .collect();
    lines.sort();
    lines
}

/// Paths carrying the skip-worktree bit. `git status` reports them as present
/// with an ordinary mode, so the bit is the only way to tell that the index,
/// not the filesystem, is the new side.
fn skip_worktree_paths(repo: &Path) -> std::collections::BTreeSet<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["ls-files", "-v", "-z"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
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
) -> OracleRecord {
    OracleRecord {
        path,
        old_path,
        old_blob: (meta[head_mode] != NULL_MODE).then(|| meta[head_oid].clone()),
        present: meta[worktree_mode] != NULL_MODE,
        submodule: meta[head_mode] == "160000" || meta[worktree_mode] == "160000",
        dirty: submodule_is_dirty(&meta[2]),
    }
}

/// The submodule field is `S<c><m><u>`. Only modified tracked files earn the
/// `-dirty` suffix: with untracked files alone, `git diff` emits no body at
/// all, so the change is not a difference.
fn submodule_is_dirty(field: &str) -> bool {
    let b = field.as_bytes();
    b.first() == Some(&b'S') && b.get(2) == Some(&b'M')
}

fn render_oracle(r: &OracleRecord, skipped: bool) -> Option<String> {
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
