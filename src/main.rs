use std::ffi::OsString;
use std::path::{Path, PathBuf};

const USAGE: &str = "usage: tsuiku [path]\n       tsuiku show <rev>\n       tsuiku pr [<number> | <url> | <branch>]\n       tsuiku diff <rev1> <rev2> | <rev1>..<rev2> | <rev1>...<rev2>";

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Worktree {
        path: PathBuf,
    },
    Show {
        revision: OsString,
    },
    /// A pull request resolved through gh; `None` means the current branch's.
    Pr {
        selector: Option<OsString>,
    },
    Diff {
        base: OsString,
        head: OsString,
        merge_base: bool,
    },
}

#[derive(Debug)]
enum CliError {
    Usage,
    App(tsuiku::app::AppError),
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Usage => f.write_str(USAGE),
            Self::App(error) => error.fmt(f),
        }
    }
}

impl From<tsuiku::app::AppError> for CliError {
    fn from(value: tsuiku::app::AppError) -> Self {
        Self::App(value)
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("tsuiku: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), CliError> {
    use std::os::unix::ffi::OsStrExt;
    match parse_args(std::env::args_os().skip(1))? {
        Command::Worktree { path } => tsuiku::app::App::run_path(&path)?,
        Command::Show { revision } => {
            tsuiku::app::App::run_show(Path::new("."), revision.as_os_str().as_bytes())?;
        }
        Command::Pr { selector } => {
            tsuiku::app::App::run_pr(Path::new("."), selector.as_deref())?;
        }
        Command::Diff {
            base,
            head,
            merge_base,
        } => {
            tsuiku::app::App::run_diff(
                Path::new("."),
                base.as_os_str().as_bytes(),
                head.as_os_str().as_bytes(),
                merge_base,
            )?;
        }
    }
    Ok(())
}

fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<Command, CliError> {
    let mut args = args.into_iter();
    let Some(first) = args.next() else {
        return Ok(Command::Worktree {
            path: PathBuf::from("."),
        });
    };
    if first == "show" {
        let revision = args.next().ok_or(CliError::Usage)?;
        if args.next().is_some() {
            return Err(CliError::Usage);
        }
        return Ok(Command::Show { revision });
    }
    if first == "pr" {
        let selector = args.next();
        if args.next().is_some() {
            return Err(CliError::Usage);
        }
        return Ok(Command::Pr { selector });
    }
    if first == "diff" {
        let one = args.next().ok_or(CliError::Usage)?;
        let two = args.next();
        if args.next().is_some() {
            return Err(CliError::Usage);
        }
        return match two {
            Some(head) => Ok(Command::Diff {
                base: one,
                head,
                merge_base: false,
            }),
            None => parse_range(&one).ok_or(CliError::Usage),
        };
    }
    if args.next().is_some() {
        return Err(CliError::Usage);
    }
    Ok(Command::Worktree {
        path: PathBuf::from(first),
    })
}

/// Split `rev1..rev2` / `rev1...rev2` at the byte level so non-UTF-8
/// revisions survive. Three dots (checked first) means merge-base.
fn parse_range(range: &OsString) -> Option<Command> {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    let bytes = range.as_os_str().as_bytes();
    let (separator, merge_base) = if let Some(at) = find(bytes, b"...") {
        (at, true)
    } else if let Some(at) = find(bytes, b"..") {
        (at, false)
    } else {
        return None;
    };
    let base = &bytes[..separator];
    let head = &bytes[separator + if merge_base { 3 } else { 2 }..];
    if base.is_empty() || head.is_empty() {
        return None;
    }
    Some(Command::Diff {
        base: OsString::from_vec(base.to_vec()),
        head: OsString::from_vec(head.to_vec()),
        merge_base,
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Command, CliError> {
        parse_args(args.iter().map(OsString::from))
    }

    #[test]
    fn no_arguments_select_the_current_worktree() {
        assert_eq!(
            parse(&[]).expect("parse"),
            Command::Worktree {
                path: PathBuf::from(".")
            }
        );
    }

    #[test]
    fn one_path_selects_that_worktree() {
        assert_eq!(
            parse(&["nested"]).expect("parse"),
            Command::Worktree {
                path: PathBuf::from("nested")
            }
        );
    }

    #[test]
    fn show_requires_exactly_one_revision() {
        assert_eq!(
            parse(&["show", "HEAD"]).expect("parse"),
            Command::Show {
                revision: OsString::from("HEAD")
            }
        );
        assert!(matches!(parse(&["show"]), Err(CliError::Usage)));
        assert!(matches!(
            parse(&["show", "HEAD", "extra"]),
            Err(CliError::Usage)
        ));
    }

    #[test]
    fn multiple_path_arguments_are_rejected() {
        assert!(matches!(parse(&["a", "b"]), Err(CliError::Usage)));
    }

    #[test]
    fn pr_takes_at_most_one_selector() {
        assert_eq!(
            parse(&["pr"]).expect("parse"),
            Command::Pr { selector: None }
        );
        assert_eq!(
            parse(&["pr", "123"]).expect("parse"),
            Command::Pr {
                selector: Some(OsString::from("123"))
            }
        );
        assert_eq!(
            parse(&["pr", "https://github.com/octo/frob/pull/9"]).expect("parse"),
            Command::Pr {
                selector: Some(OsString::from("https://github.com/octo/frob/pull/9"))
            }
        );
        assert!(matches!(parse(&["pr", "a", "b"]), Err(CliError::Usage)));
    }

    #[test]
    fn diff_takes_two_revisions_or_a_range() {
        let direct = Command::Diff {
            base: OsString::from("main"),
            head: OsString::from("feature"),
            merge_base: false,
        };
        assert_eq!(parse(&["diff", "main", "feature"]).expect("parse"), direct);
        assert_eq!(parse(&["diff", "main..feature"]).expect("parse"), direct);
        assert_eq!(
            parse(&["diff", "main...feature"]).expect("parse"),
            Command::Diff {
                base: OsString::from("main"),
                head: OsString::from("feature"),
                merge_base: true,
            }
        );
    }

    #[test]
    fn malformed_diff_arguments_are_usage_errors() {
        assert!(matches!(parse(&["diff"]), Err(CliError::Usage)));
        assert!(matches!(parse(&["diff", "main"]), Err(CliError::Usage)));
        assert!(matches!(
            parse(&["diff", "..feature"]),
            Err(CliError::Usage)
        ));
        assert!(matches!(parse(&["diff", "main.."]), Err(CliError::Usage)));
        assert!(matches!(parse(&["diff", "main..."]), Err(CliError::Usage)));
        assert!(matches!(
            parse(&["diff", "a", "b", "c"]),
            Err(CliError::Usage)
        ));
    }

    #[test]
    fn diff_ranges_preserve_non_utf8_revision_bytes() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let range = OsString::from_vec(b"br-\xff..br-\xfe".to_vec());
        let command = parse_args([OsString::from("diff"), range]).expect("parse");
        let Command::Diff {
            base,
            head,
            merge_base,
        } = command
        else {
            panic!("diff command");
        };
        assert_eq!(base.as_os_str().as_bytes(), b"br-\xff");
        assert_eq!(head.as_os_str().as_bytes(), b"br-\xfe");
        assert!(!merge_base);
    }

    #[test]
    fn show_preserves_non_utf8_revision_bytes() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let revision = OsString::from_vec(b"branch-\xff".to_vec());
        let command = parse_args([OsString::from("show"), revision]).expect("parse");
        let Command::Show { revision } = command else {
            panic!("show command");
        };
        assert_eq!(revision.as_os_str().as_bytes(), b"branch-\xff");
    }
}
