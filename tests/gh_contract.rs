//! Contract of the gh client, exercised with fake gh executables (shell
//! scripts) so no real gh and no network is needed.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use tsuiku::gh::{CancelFlag, GhClient, GhError};

const VALID_JSON: &str = r#"{"number":123,"title":"Fix the frobnicator","state":"OPEN","baseRefName":"main","baseRefOid":"88f83455ac2158e6b0f9c2b4f87893b6b2f5e598","headRefName":"feature/frob","headRefOid":"a3c2e0e29f0d29a9d0d7c81e160f8b95a2b3d701","isCrossRepository":false,"url":"https://github.com/octo/frob/pull/123"}"#;

/// Write an executable shell script standing in for gh.
fn fake_gh(dir: &tempfile::TempDir, body: &str) -> PathBuf {
    let path = dir.path().join("fake-gh");
    let mut f = fs::File::create(&path).expect("create script");
    writeln!(f, "#!/bin/sh\n{body}").expect("write script");
    f.set_permissions(fs::Permissions::from_mode(0o755))
        .expect("chmod script");
    path
}

/// Generous timeout: tests run in parallel and process spawn can be slow
/// under load. Only the timeout test itself uses a short limit.
fn client(binary: PathBuf) -> GhClient {
    GhClient {
        binary,
        timeout: Duration::from_secs(10),
        max_stdout_bytes: 64 * 1024,
        max_stderr_bytes: 4096,
        cancel: CancelFlag::default(),
    }
}

fn view(c: &GhClient, selector: Option<&str>) -> Result<tsuiku::gh::PrInfo, GhError> {
    let dir = tempfile::TempDir::new().expect("temp dir");
    c.pr_view(dir.path(), selector.map(std::ffi::OsStr::new))
}

#[test]
fn a_successful_view_parses_into_pr_info() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let bin = fake_gh(&dir, &format!("echo '{VALID_JSON}'"));
    let info = view(&client(bin), Some("123")).expect("pr info");
    assert_eq!(info.number, 123);
    assert_eq!(info.base_ref_name, "main");
    assert_eq!(
        info.head_ref_oid.to_hex(),
        "a3c2e0e29f0d29a9d0d7c81e160f8b95a2b3d701"
    );
}

#[test]
fn the_selector_and_json_fields_are_passed_through() {
    // The script replays its arguments; the client must ask for `pr view`,
    // the selector, and the exact field list its parser expects.
    let dir = tempfile::TempDir::new().expect("temp dir");
    let args_file = dir.path().join("args");
    let bin = fake_gh(
        &dir,
        &format!(
            "printf '%s\\n' \"$@\" > {}\necho '{VALID_JSON}'",
            args_file.display()
        ),
    );
    view(&client(bin), Some("123")).expect("pr info");
    let args = fs::read_to_string(&args_file).expect("recorded args");
    let args: Vec<&str> = args.lines().collect();
    assert_eq!(
        args,
        vec![
            "pr",
            "view",
            "--json",
            "number,title,state,baseRefName,baseRefOid,headRefName,headRefOid,isCrossRepository,url",
            "--",
            "123",
        ]
    );
}

#[test]
fn a_selector_named_like_an_option_stays_positional() {
    // gh must never parse a branch selector such as "--depth=1" as a flag;
    // the "--" in the argument order keeps it positional.
    let dir = tempfile::TempDir::new().expect("temp dir");
    let args_file = dir.path().join("args");
    let bin = fake_gh(
        &dir,
        &format!(
            "printf '%s\\n' \"$@\" > {}\necho '{VALID_JSON}'",
            args_file.display()
        ),
    );
    view(&client(bin), Some("--depth=1")).expect("pr info");
    let args = fs::read_to_string(&args_file).expect("recorded args");
    assert!(args.ends_with("--\n--depth=1\n"), "got {args:?}");
}

#[test]
fn no_selector_omits_the_positional_argument() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let args_file = dir.path().join("args");
    let bin = fake_gh(
        &dir,
        &format!(
            "printf '%s\\n' \"$@\" > {}\necho '{VALID_JSON}'",
            args_file.display()
        ),
    );
    view(&client(bin), None).expect("pr info");
    let args = fs::read_to_string(&args_file).expect("recorded args");
    assert!(args.starts_with("pr\nview\n--json\n"), "got {args:?}");
}

#[test]
fn a_missing_binary_reports_not_installed() {
    let c = client(PathBuf::from("/nonexistent/gh"));
    assert!(matches!(view(&c, None), Err(GhError::NotInstalled)));
}

#[test]
fn exit_code_four_reports_not_authenticated() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let bin = fake_gh(&dir, "echo 'To get started with GitHub CLI' >&2\nexit 4");
    assert!(matches!(
        view(&client(bin), None),
        Err(GhError::NotAuthenticated { .. })
    ));
}

#[test]
fn a_branch_without_a_pr_reports_no_pull_request() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let bin = fake_gh(
        &dir,
        "echo 'no pull requests found for branch \"main\"' >&2\nexit 1",
    );
    assert!(matches!(
        view(&client(bin), None),
        Err(GhError::NoPullRequest { .. })
    ));
}

#[test]
fn a_hung_gh_times_out() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let bin = fake_gh(&dir, "sleep 60");
    let c = GhClient {
        timeout: Duration::from_secs(2),
        ..client(bin)
    };
    let started = std::time::Instant::now();
    assert!(matches!(view(&c, None), Err(GhError::TimedOut { .. })));
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn cancelling_a_running_view_reports_interrupted_promptly() {
    // Ctrl-C during PR resolution is routed into this flag; a hung gh must
    // be killed and reported well before its timeout.
    let dir = tempfile::TempDir::new().expect("temp dir");
    let bin = fake_gh(&dir, "sleep 60");
    let c = client(bin);
    let cancel = c.cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        cancel.cancel();
    });
    let started = std::time::Instant::now();
    assert!(matches!(view(&c, None), Err(GhError::Interrupted)));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "cancellation must not wait out the timeout"
    );
}

#[test]
fn oversized_output_is_rejected() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let bin = fake_gh(&dir, "head -c 131072 /dev/zero | tr '\\0' 'o'");
    assert!(matches!(
        view(&client(bin), None),
        Err(GhError::OutputTooLarge)
    ));
}

#[test]
fn garbage_stdout_with_a_zero_exit_is_invalid_json() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let bin = fake_gh(&dir, "echo 'not json'");
    assert!(matches!(
        view(&client(bin), None),
        Err(GhError::InvalidJson)
    ));
}

// --- ensure_pr_commits, against a local file remote -----------------------

use std::path::Path;

use tsuiku::discover::GixDiscoverer;
use tsuiku::gh::{PrInfo, ensure_pr_commits};
use tsuiku::ids::Oid;

fn git(repo: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "tsuiku")
        .env("GIT_AUTHOR_EMAIL", "tsuiku@example.invalid")
        .env("GIT_COMMITTER_NAME", "tsuiku")
        .env("GIT_COMMITTER_EMAIL", "tsuiku@example.invalid")
        .current_dir(repo)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("utf-8 git output")
        .trim()
        .to_string()
}

fn oid(hex: &str) -> Oid {
    let mut out = [0u8; 20];
    for (i, pair) in hex.as_bytes().chunks_exact(2).enumerate() {
        let hex_pair = std::str::from_utf8(pair).expect("hex");
        out[i] = u8::from_str_radix(hex_pair, 16).expect("hex");
    }
    Oid(out)
}

/// A "GitHub" remote with a PR head only advertised as refs/pull/1/head,
/// and a clone taken before that commit existed.
fn pr_fixture() -> (tempfile::TempDir, PathBuf, PrInfo) {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let remote = dir.path().join("remote");
    let local = dir.path().join("local");
    fs::create_dir(&remote).expect("mkdir remote");

    git(&remote, &["init", "--quiet", "-b", "main"]);
    fs::write(remote.join("a.txt"), "base\n").expect("write");
    git(&remote, &["add", "a.txt"]);
    git(&remote, &["commit", "--quiet", "-m", "base"]);
    let base = git(&remote, &["rev-parse", "HEAD"]);

    git(
        dir.path(),
        &[
            "clone",
            "--quiet",
            remote.to_str().expect("utf-8"),
            local.to_str().expect("utf-8"),
        ],
    );

    // The PR head is created only after the clone, and never lands on a
    // branch the clone would fetch by default.
    git(&remote, &["checkout", "--quiet", "-b", "feature"]);
    fs::write(remote.join("a.txt"), "changed\n").expect("write");
    git(&remote, &["commit", "--quiet", "-am", "change"]);
    let head = git(&remote, &["rev-parse", "HEAD"]);
    git(&remote, &["update-ref", "refs/pull/1/head", &head]);
    git(&remote, &["checkout", "--quiet", "main"]);
    git(&remote, &["branch", "--quiet", "-D", "feature"]);

    let info = PrInfo {
        number: 1,
        title: "change".to_string(),
        state: "OPEN".to_string(),
        base_ref_name: "main".to_string(),
        base_ref_oid: oid(&base),
        head_ref_name: "feature".to_string(),
        head_ref_oid: oid(&head),
        is_cross_repository: false,
        // Not a github.com URL on purpose: a github slug would make the
        // fetch source the PR repository's URL (a network address). With no
        // slug the fetch falls back to "origin", which is the file remote —
        // these tests must stay offline.
        url: "https://example.invalid/octo/frob/pull/1".to_string(),
    };
    (dir, local, info)
}

#[test]
fn a_missing_pr_head_is_fetched_without_creating_refs() {
    let (_dir, local, info) = pr_fixture();
    let discoverer = GixDiscoverer::open(&local).expect("open clone");
    assert!(!discoverer.contains_commit(info.head_ref_oid));

    let refs_before = git(&local, &["for-each-ref"]);
    ensure_pr_commits(
        &discoverer,
        &local,
        &info,
        Duration::from_secs(30),
        CancelFlag::default(),
    )
    .expect("fetch");

    assert!(discoverer.contains_commit(info.head_ref_oid));
    assert!(discoverer.contains_commit(info.base_ref_oid));
    assert_eq!(
        git(&local, &["for-each-ref"]),
        refs_before,
        "no ref was created"
    );
    assert!(
        !local.join(".git/FETCH_HEAD").exists(),
        "FETCH_HEAD was not written"
    );
}

#[test]
fn present_commits_skip_the_fetch_entirely() {
    let (_dir, local, info) = pr_fixture();
    ensure_pr_commits(
        &GixDiscoverer::open(&local).expect("open clone"),
        &local,
        &info,
        Duration::from_secs(30),
        CancelFlag::default(),
    )
    .expect("first fetch");

    // Cut the clone off from its remote; a second call must not need it.
    let discoverer = GixDiscoverer::open(&local).expect("reopen clone");
    git(
        &local,
        &["remote", "set-url", "origin", "/nonexistent/remote"],
    );
    ensure_pr_commits(
        &discoverer,
        &local,
        &info,
        Duration::from_secs(30),
        CancelFlag::default(),
    )
    .expect("no network needed");
}

#[test]
fn a_fetch_that_cannot_supply_the_commits_fails() {
    let (_dir, local, mut info) = pr_fixture();
    // A PR number whose ref does not exist on the remote.
    info.number = 99;
    let discoverer = GixDiscoverer::open(&local).expect("open clone");
    let err = ensure_pr_commits(
        &discoverer,
        &local,
        &info,
        Duration::from_secs(30),
        CancelFlag::default(),
    )
    .expect_err("missing pull ref fails");
    assert!(matches!(err, GhError::FetchFailed { .. }), "got {err:?}");
}

#[test]
fn a_shallow_clone_is_unshallowed_so_the_merge_base_exists() {
    // History: root → base tip on main; the PR head branches off the root.
    // A depth-1 clone holds the base tip but not the root, so even with
    // both PR commits fetched the merge base would be unreachable.
    let dir = tempfile::TempDir::new().expect("temp dir");
    let remote = dir.path().join("remote");
    let local = dir.path().join("local");
    fs::create_dir(&remote).expect("mkdir remote");

    git(&remote, &["init", "--quiet", "-b", "main"]);
    fs::write(remote.join("a.txt"), "root\n").expect("write");
    git(&remote, &["add", "a.txt"]);
    git(&remote, &["commit", "--quiet", "-m", "root"]);
    let root = git(&remote, &["rev-parse", "HEAD"]);
    fs::write(remote.join("a.txt"), "base\n").expect("write");
    git(&remote, &["commit", "--quiet", "-am", "base"]);
    let base = git(&remote, &["rev-parse", "HEAD"]);
    git(&remote, &["checkout", "--quiet", "-b", "feature", &root]);
    fs::write(remote.join("b.txt"), "head\n").expect("write");
    git(&remote, &["add", "b.txt"]);
    git(&remote, &["commit", "--quiet", "-m", "head"]);
    let head = git(&remote, &["rev-parse", "HEAD"]);
    git(&remote, &["update-ref", "refs/pull/1/head", &head]);
    git(&remote, &["checkout", "--quiet", "main"]);
    git(&remote, &["branch", "--quiet", "-D", "feature"]);

    // --depth needs a transport; a plain local path would ignore it.
    let remote_url = format!("file://{}", remote.display());
    git(
        dir.path(),
        &[
            "clone",
            "--quiet",
            "--depth=1",
            &remote_url,
            local.to_str().expect("utf-8"),
        ],
    );
    assert!(local.join(".git/shallow").exists(), "the clone is shallow");

    let info = PrInfo {
        number: 1,
        title: "head".to_string(),
        state: "OPEN".to_string(),
        base_ref_name: "main".to_string(),
        base_ref_oid: oid(&base),
        head_ref_name: "feature".to_string(),
        head_ref_oid: oid(&head),
        is_cross_repository: false,
        url: "https://example.invalid/octo/frob/pull/1".to_string(),
    };
    let discoverer = GixDiscoverer::open(&local).expect("open clone");
    ensure_pr_commits(
        &discoverer,
        &local,
        &info,
        Duration::from_secs(30),
        CancelFlag::default(),
    )
    .expect("fetch");

    assert!(
        !local.join(".git/shallow").exists(),
        "the clone was unshallowed"
    );
    let merge_base = discoverer
        .merge_base(info.base_ref_oid, info.head_ref_oid)
        .expect("the merge base is computable after the fetch");
    assert_eq!(merge_base, oid(&root));
}

#[test]
fn a_base_branch_named_like_an_option_is_fetched_verbatim() {
    // GitHub allows branch names such as "--depth=1"; passed bare to
    // `git fetch` that is silently consumed as an option and the base
    // commit never arrives.
    let (dir, local, mut info) = pr_fixture();
    let remote = dir.path().join("remote");
    fs::write(remote.join("a.txt"), "option base\n").expect("write");
    git(&remote, &["commit", "--quiet", "-am", "option base"]);
    let base = git(&remote, &["rev-parse", "HEAD"]);
    git(&remote, &["update-ref", "refs/heads/--depth=1", &base]);
    git(&remote, &["checkout", "--quiet", "-q", "main"]);
    info.base_ref_name = "--depth=1".to_string();
    info.base_ref_oid = oid(&base);

    let discoverer = GixDiscoverer::open(&local).expect("open clone");
    assert!(!discoverer.contains_commit(info.base_ref_oid));
    ensure_pr_commits(
        &discoverer,
        &local,
        &info,
        Duration::from_secs(30),
        CancelFlag::default(),
    )
    .expect("fetch");
    assert!(discoverer.contains_commit(info.base_ref_oid));
    assert!(discoverer.contains_commit(info.head_ref_oid));
}
