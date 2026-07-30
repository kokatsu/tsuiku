use std::ffi::OsString;
use std::path::{Path, PathBuf};

const USAGE: &str = "usage: tsuiku [path]\n       tsuiku show <rev>";

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Worktree { path: PathBuf },
    Show { revision: OsString },
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
    match parse_args(std::env::args_os().skip(1))? {
        Command::Worktree { path } => tsuiku::app::App::run_path(&path)?,
        Command::Show { revision } => {
            use std::os::unix::ffi::OsStrExt;
            tsuiku::app::App::run_show(Path::new("."), revision.as_os_str().as_bytes())?;
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
    if args.next().is_some() {
        return Err(CliError::Usage);
    }
    Ok(Command::Worktree {
        path: PathBuf::from(first),
    })
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
