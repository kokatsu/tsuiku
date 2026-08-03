//! GitHub CLI (`gh`) integration for resolving pull requests.
//!
//! gh is only used to resolve pull-request *metadata* — number, title, and
//! the base/head commit ids. All content comes from the local object
//! database; when the commits are missing, a single narrow `git fetch`
//! supplies the objects without creating any local ref (see
//! [`ensure_pr_commits`]).
//!
//! Every invocation goes through the guarded execution path in
//! `crate::exec` with a wall-clock timeout and output caps.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::Deserialize;

use crate::discover::GixDiscoverer;
pub use crate::exec::CancelFlag;
use crate::exec::{ExecError, GuardedCommand};
use crate::ids::Oid;

/// The `--json` field list `pr_view` requests, and the shape [`parse_pr_view`]
/// expects back.
const PR_VIEW_FIELDS: &str =
    "number,title,state,baseRefName,baseRefOid,headRefName,headRefOid,isCrossRepository,url";

/// gh reserves exit code 4 for "authentication required".
const GH_EXIT_AUTH: i32 = 4;

/// How much of a stderr message survives into an error. Enough for gh's
/// multi-line hints, small enough that a runaway pipe cannot flood the
/// terminal.
const DETAIL_MAX_CHARS: usize = 500;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PrInfo {
    pub number: u64,
    pub title: String,
    /// "OPEN" | "CLOSED" | "MERGED", verbatim from gh.
    pub state: String,
    pub base_ref_name: String,
    pub base_ref_oid: Oid,
    pub head_ref_name: String,
    pub head_ref_oid: Oid,
    pub is_cross_repository: bool,
    /// `https://github.com/OWNER/REPO/pull/N`.
    pub url: String,
}

#[derive(Debug)]
pub enum GhError {
    /// The gh binary is not on PATH.
    NotInstalled,
    NotAuthenticated {
        detail: String,
    },
    /// gh could not resolve the selector to a pull request.
    NoPullRequest {
        detail: String,
    },
    /// gh exited nonzero for any other reason.
    Failed {
        exit_code: Option<i32>,
        detail: String,
    },
    /// The fetch that should have supplied the PR commits failed, or the
    /// commits were still missing afterwards.
    FetchFailed {
        detail: String,
    },
    TimedOut {
        limit: Duration,
    },
    /// The shared cancel flag stopped a run (Ctrl-C during resolution); the
    /// child has already been killed and reaped.
    Interrupted,
    OutputTooLarge,
    /// gh exited zero but its output was not the JSON we asked for.
    InvalidJson,
    /// A pull-request view without a worktree makes no sense: gh resolves
    /// the repository from the working directory.
    NoWorkdir,
    Io,
}

impl std::fmt::Display for GhError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotInstalled => {
                write!(
                    f,
                    "gh not found; install the GitHub CLI (https://cli.github.com)"
                )
            }
            Self::NotAuthenticated { detail } => {
                write!(f, "gh is not authenticated; run 'gh auth login'")?;
                write_detail(f, detail)
            }
            Self::NoPullRequest { detail } => {
                write!(f, "no pull request")?;
                write_detail(f, detail)
            }
            Self::Failed { exit_code, detail } => {
                match exit_code {
                    Some(code) => write!(f, "gh failed with exit code {code}")?,
                    None => write!(f, "gh was killed by a signal")?,
                }
                write_detail(f, detail)
            }
            Self::FetchFailed { detail } => {
                write!(f, "fetching pull request commits failed")?;
                write_detail(f, detail)
            }
            Self::TimedOut { limit } => write!(f, "gh timed out after {}s", limit.as_secs()),
            Self::Interrupted => write!(f, "interrupted"),
            Self::OutputTooLarge => write!(f, "gh produced more output than expected"),
            Self::InvalidJson => write!(f, "unexpected gh output"),
            Self::NoWorkdir => write!(f, "a pull request view requires a worktree"),
            Self::Io => write!(f, "running gh failed"),
        }
    }
}

impl std::error::Error for GhError {}

fn write_detail(f: &mut std::fmt::Formatter<'_>, detail: &str) -> std::fmt::Result {
    if detail.is_empty() {
        return Ok(());
    }
    write!(f, ": {detail}")
}

pub struct GhClient {
    /// The binary to invoke; tests substitute a script.
    pub binary: PathBuf,
    /// Generous compared to difft: `gh pr view` is a network round-trip.
    pub timeout: Duration,
    pub max_stdout_bytes: usize,
    pub max_stderr_bytes: usize,
    /// Cancelling mid-run kills and reaps the child before the call returns.
    pub cancel: CancelFlag,
}

impl Default for GhClient {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("gh"),
            timeout: Duration::from_secs(30),
            max_stdout_bytes: 1024 * 1024,
            max_stderr_bytes: 64 * 1024,
            cancel: CancelFlag::default(),
        }
    }
}

impl GhClient {
    /// Resolve a pull request to its metadata. `selector` is a PR number,
    /// URL, or branch name; `None` asks gh for the current branch's PR.
    pub fn pr_view(&self, workdir: &Path, selector: Option<&OsStr>) -> Result<PrInfo, GhError> {
        let mut cmd = Command::new(&self.binary);
        cmd.current_dir(workdir)
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_NO_UPDATE_NOTIFIER", "1")
            .env("NO_COLOR", "1")
            .args(["pr", "view", "--json", PR_VIEW_FIELDS]);
        // A branch selector may look like an option ("--depth=1"); keep it
        // behind "--" so gh never parses it as a flag.
        if let Some(selector) = selector {
            cmd.arg("--").arg(selector);
        }

        let output = self.guard().run(cmd).map_err(|e| self.map_exec(e))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(classify_failure(output.status.code(), &stderr));
        }
        parse_pr_view(&output.stdout)
    }

    fn guard(&self) -> GuardedCommand {
        GuardedCommand {
            timeout: self.timeout,
            max_stdout_bytes: self.max_stdout_bytes,
            max_stderr_bytes: self.max_stderr_bytes,
            cancel: self.cancel.clone(),
        }
    }

    fn map_exec(&self, e: ExecError) -> GhError {
        match e {
            ExecError::NotFound => GhError::NotInstalled,
            ExecError::TimedOut => GhError::TimedOut {
                limit: self.timeout,
            },
            ExecError::OutputTooLarge => GhError::OutputTooLarge,
            ExecError::Cancelled => GhError::Interrupted,
            ExecError::Io => GhError::Io,
        }
    }
}

/// The JSON object `gh pr view --json` prints for [`PR_VIEW_FIELDS`].
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPrView {
    number: u64,
    title: String,
    state: String,
    base_ref_name: String,
    base_ref_oid: String,
    head_ref_name: String,
    head_ref_oid: String,
    is_cross_repository: bool,
    url: String,
}

pub(crate) fn parse_pr_view(stdout: &[u8]) -> Result<PrInfo, GhError> {
    let raw: RawPrView = serde_json::from_slice(stdout).map_err(|_| GhError::InvalidJson)?;
    let base_ref_oid = oid_from_hex(&raw.base_ref_oid).ok_or(GhError::InvalidJson)?;
    let head_ref_oid = oid_from_hex(&raw.head_ref_oid).ok_or(GhError::InvalidJson)?;
    Ok(PrInfo {
        number: raw.number,
        title: raw.title,
        state: raw.state,
        base_ref_name: raw.base_ref_name,
        base_ref_oid,
        head_ref_name: raw.head_ref_name,
        head_ref_oid,
        is_cross_repository: raw.is_cross_repository,
        url: raw.url,
    })
}

pub(crate) fn oid_from_hex(hex: &str) -> Option<Oid> {
    let bytes = hex.as_bytes();
    if bytes.len() != 40 {
        return None;
    }
    let mut out = [0u8; 20];
    for (i, pair) in bytes.chunks_exact(2).enumerate() {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out[i] = (hi as u8) << 4 | lo as u8;
    }
    Some(Oid(out))
}

/// Classify a nonzero gh exit. The exit code is the stable signal; stderr
/// wording is matched conservatively and anything unrecognised is passed
/// through verbatim, because gh's own message is the best explanation.
pub(crate) fn classify_failure(exit_code: Option<i32>, stderr: &str) -> GhError {
    let detail = sanitize_detail(stderr);
    if exit_code == Some(GH_EXIT_AUTH) {
        return GhError::NotAuthenticated { detail };
    }
    let lower = detail.to_ascii_lowercase();
    if lower.contains("no pull requests found") || lower.contains("no default remote") {
        return GhError::NoPullRequest { detail };
    }
    GhError::Failed { exit_code, detail }
}

/// Strip terminal controls from text that came from a subprocess and bound
/// its length; it ends up on the user's terminal verbatim.
fn sanitize_detail(text: &str) -> String {
    let mut out = String::new();
    for c in text.trim().chars().take(DETAIL_MAX_CHARS) {
        if c == '\n' {
            out.push(' ');
        } else if !c.is_control() {
            out.push(c);
        }
    }
    out.trim().to_string()
}

/// `https://github.com/OWNER/REPO/pull/123` → `OWNER/REPO`.
pub(crate) fn repo_slug_from_pr_url(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://github.com/")?;
    let mut parts = rest.split('/');
    let owner = parts.next().filter(|s| !s.is_empty())?;
    let repo = parts.next().filter(|s| !s.is_empty())?;
    (parts.next() == Some("pull")).then(|| format!("{owner}/{repo}"))
}

/// Whether a remote URL (https or scp-like ssh) points at the GitHub
/// repository named by `slug` (`OWNER/REPO`).
pub(crate) fn remote_url_matches_slug(url: &str, slug: &str) -> bool {
    let url = url.trim();
    let rest = if let Some(rest) = url.strip_prefix("https://github.com/") {
        rest
    } else if let Some(rest) = url.strip_prefix("ssh://git@github.com/") {
        rest
    } else if let Some(rest) = url.strip_prefix("git@github.com:") {
        rest
    } else {
        return false;
    };
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    rest.eq_ignore_ascii_case(slug)
}

/// Pick what to pass to `git fetch` as the repository: the configured remote
/// whose fetch URL matches the PR's repository (so credentials and insteadOf
/// rewrites keep applying), or — when no remote points there, as in a fork
/// clone without an upstream remote — the PR repository's own URL, which git
/// accepts in place of a remote name. Only without a slug is "origin" left to
/// fail with git's own message rather than a guess of ours.
pub(crate) fn choose_fetch_source<'a>(
    remotes: impl IntoIterator<Item = (&'a str, &'a str)>,
    slug: Option<&str>,
) -> String {
    let Some(slug) = slug else {
        return "origin".to_string();
    };
    for (name, url) in remotes {
        if remote_url_matches_slug(url, slug) {
            return name.to_string();
        }
    }
    format!("https://github.com/{slug}")
}

/// Make sure the base and head commits of a PR exist in the local object
/// database — with the history connecting them — fetching once if needed.
///
/// The fetch names `refs/pull/<n>/head` (which GitHub always advertises,
/// even for a cross-repository PR) and the base branch, with no destination
/// refspec and no FETCH_HEAD write: objects land in the ODB and nothing
/// else in the repository changes. A shallow clone is unshallowed in the
/// same fetch: the commits alone are not enough, because a shallow boundary
/// cuts the history the merge-base computation has to walk.
pub fn ensure_pr_commits(
    discoverer: &GixDiscoverer,
    workdir: &Path,
    info: &PrInfo,
    timeout: Duration,
    cancel: CancelFlag,
) -> Result<(), GhError> {
    let shallow = discoverer.repository().is_shallow();
    if !shallow
        && discoverer.contains_commit(info.head_ref_oid)
        && discoverer.contains_commit(info.base_ref_oid)
    {
        return Ok(());
    }

    let slug = repo_slug_from_pr_url(&info.url);
    let remotes = collect_remotes(discoverer.repository());
    let source = choose_fetch_source(
        remotes.iter().map(|(n, u)| (n.as_str(), u.as_str())),
        slug.as_deref(),
    );

    let mut cmd = Command::new("git");
    cmd.current_dir(workdir)
        .args(["fetch", "--quiet", "--no-tags", "--no-write-fetch-head"]);
    if shallow {
        cmd.arg("--unshallow");
    }
    // Git allows remote and branch names that look like options
    // ("--depth=1"), so the source and the fully-qualified refspecs all sit
    // behind "--".
    cmd.arg("--")
        .arg(&source)
        .arg(format!("refs/pull/{}/head", info.number))
        .arg(format!("refs/heads/{}", info.base_ref_name));
    let guard = GuardedCommand {
        timeout,
        max_stdout_bytes: 64 * 1024,
        max_stderr_bytes: 64 * 1024,
        cancel,
    };
    let output = guard.run(cmd).map_err(|e| match e {
        ExecError::TimedOut => GhError::FetchFailed {
            detail: format!("git fetch {source} timed out after {}s", timeout.as_secs()),
        },
        ExecError::Cancelled => GhError::Interrupted,
        _ => GhError::FetchFailed {
            detail: format!("running git fetch {source} failed"),
        },
    })?;
    if !output.status.success() {
        return Err(GhError::FetchFailed {
            detail: sanitize_detail(&String::from_utf8_lossy(&output.stderr)),
        });
    }

    if discoverer.contains_commit(info.head_ref_oid)
        && discoverer.contains_commit(info.base_ref_oid)
    {
        return Ok(());
    }
    Err(GhError::FetchFailed {
        detail: format!("the pull request commits are still missing; run 'git fetch {source}'"),
    })
}

fn collect_remotes(repo: &gix::Repository) -> Vec<(String, String)> {
    repo.remote_names()
        .into_iter()
        .filter_map(|name| {
            use gix::bstr::ByteSlice;
            let remote = repo.find_remote(name.as_bstr()).ok()?;
            let url = remote.url(gix::remote::Direction::Fetch)?;
            Some((name.to_string(), url.to_bstring().to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "number": 123,
        "title": "Fix the frobnicator",
        "state": "OPEN",
        "baseRefName": "main",
        "baseRefOid": "88f83455ac2158e6b0f9c2b4f87893b6b2f5e598",
        "headRefName": "feature/frob",
        "headRefOid": "a3c2e0e29f0d29a9d0d7c81e160f8b95a2b3d701",
        "isCrossRepository": false,
        "url": "https://github.com/octo/frob/pull/123"
    }"#;

    #[test]
    fn a_pr_view_payload_parses() {
        let info = parse_pr_view(SAMPLE.as_bytes()).expect("parse");
        assert_eq!(info.number, 123);
        assert_eq!(info.title, "Fix the frobnicator");
        assert_eq!(info.state, "OPEN");
        assert_eq!(info.base_ref_name, "main");
        assert_eq!(
            info.base_ref_oid.to_hex(),
            "88f83455ac2158e6b0f9c2b4f87893b6b2f5e598"
        );
        assert_eq!(
            info.head_ref_oid.to_hex(),
            "a3c2e0e29f0d29a9d0d7c81e160f8b95a2b3d701"
        );
        assert!(!info.is_cross_repository);
        assert_eq!(info.url, "https://github.com/octo/frob/pull/123");
    }

    #[test]
    fn a_short_or_invalid_oid_is_rejected() {
        let short = SAMPLE.replace("88f83455ac2158e6b0f9c2b4f87893b6b2f5e598", "88f834");
        assert!(matches!(
            parse_pr_view(short.as_bytes()),
            Err(GhError::InvalidJson)
        ));
        let bad = SAMPLE.replace('8', "g");
        assert!(matches!(
            parse_pr_view(bad.as_bytes()),
            Err(GhError::InvalidJson)
        ));
    }

    #[test]
    fn a_missing_field_is_rejected() {
        let missing = SAMPLE.replace("\"state\": \"OPEN\",", "");
        assert!(matches!(
            parse_pr_view(missing.as_bytes()),
            Err(GhError::InvalidJson)
        ));
    }

    #[test]
    fn non_json_output_is_rejected() {
        assert!(matches!(
            parse_pr_view(b"To https://github.com\nEverything up-to-date"),
            Err(GhError::InvalidJson)
        ));
    }

    #[test]
    fn exit_code_four_wins_over_stderr_wording() {
        let err = classify_failure(Some(4), "no pull requests found for branch");
        assert!(matches!(err, GhError::NotAuthenticated { .. }), "{err:?}");
    }

    #[test]
    fn a_missing_pr_is_classified_from_stderr() {
        let err = classify_failure(Some(1), "no pull requests found for branch \"main\"");
        assert!(matches!(err, GhError::NoPullRequest { .. }), "{err:?}");
    }

    #[test]
    fn an_unrecognised_failure_keeps_the_message_and_code() {
        let err = classify_failure(Some(1), "GraphQL: Something went wrong");
        match err {
            GhError::Failed { exit_code, detail } => {
                assert_eq!(exit_code, Some(1));
                assert_eq!(detail, "GraphQL: Something went wrong");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn detail_text_is_stripped_of_terminal_controls() {
        let err = classify_failure(Some(1), "line one\nline two\x1b[31m red");
        match err {
            GhError::Failed { detail, .. } => assert_eq!(detail, "line one line two[31m red"),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn a_pr_url_yields_its_repository_slug() {
        assert_eq!(
            repo_slug_from_pr_url("https://github.com/octo/frob/pull/123").as_deref(),
            Some("octo/frob")
        );
        assert_eq!(repo_slug_from_pr_url("https://github.com/octo/frob"), None);
        assert_eq!(
            repo_slug_from_pr_url("https://example.com/octo/frob/pull/1"),
            None
        );
    }

    #[test]
    fn remote_urls_match_their_slug_in_all_syntaxes() {
        for url in [
            "https://github.com/octo/frob",
            "https://github.com/octo/frob.git",
            "https://github.com/Octo/Frob.git",
            "git@github.com:octo/frob.git",
            "ssh://git@github.com/octo/frob.git",
        ] {
            assert!(remote_url_matches_slug(url, "octo/frob"), "{url}");
        }
        assert!(!remote_url_matches_slug(
            "https://github.com/octo/other",
            "octo/frob"
        ));
        assert!(!remote_url_matches_slug(
            "https://gitlab.com/octo/frob",
            "octo/frob"
        ));
    }

    #[test]
    fn a_remote_matching_the_pr_repository_wins() {
        let remotes = [
            ("origin", "git@github.com:me/fork.git"),
            ("upstream", "https://github.com/octo/frob.git"),
        ];
        assert_eq!(choose_fetch_source(remotes, Some("octo/frob")), "upstream");
    }

    #[test]
    fn a_fork_clone_without_an_upstream_remote_fetches_from_the_pr_url() {
        // The common fork setup: `origin` is the fork, no upstream remote is
        // configured, and the PR lives on the parent repository. Fetching
        // refs/pull/<n>/head from the fork would fail, so the PR
        // repository's own URL must be the fetch source.
        let remotes = [("origin", "git@github.com:me/fork.git")];
        assert_eq!(
            choose_fetch_source(remotes, Some("octo/frob")),
            "https://github.com/octo/frob"
        );
    }

    #[test]
    fn without_a_slug_origin_is_the_last_resort() {
        let remotes = [("origin", "git@github.com:me/fork.git")];
        assert_eq!(choose_fetch_source(remotes, None), "origin");
    }
}
