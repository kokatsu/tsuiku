//! Temp file strategy for difft runs.
//!
//! difft picks its parser from the file extension, so both sides are written
//! into a throwaway directory whose file names carry a sanitized hint derived
//! from the git path. The original directory structure is deliberately not
//! reproduced:
//!
//! ```text
//! <tmp>/tsuiku-<rand>/old/<sanitized-name>
//! <tmp>/tsuiku-<rand>/new/<sanitized-name>
//! ```
//!
//! Sanitization drops anything that could escape the directory or confuse
//! the filesystem: separators, `..`, NUL, non-UTF-8, extreme lengths.
//! The hint is also part of the structural cache key, so it must be a pure
//! function of the git path.

use std::fs;
use std::io;
use std::path::PathBuf;

use tempfile::TempDir;

use crate::path::GitPath;

const MAX_BASENAME_BYTES: usize = 64;
const MAX_EXTENSION_BYTES: usize = 16;

/// Language hint for difft, extracted from a git path. Not a path: just a
/// basename and extension, both optional and unsanitized (sanitization
/// happens when a file name is built).
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct LanguagePathHint {
    pub extension: Option<Vec<u8>>,
    pub basename: Option<Vec<u8>>,
}

impl LanguagePathHint {
    pub fn from_git_path(path: &GitPath) -> Self {
        let name = path.file_name();
        Self {
            extension: path.extension().map(|e| e.to_vec()),
            basename: if name.is_empty() {
                None
            } else {
                Some(name.to_vec())
            },
        }
    }

    pub fn none() -> Self {
        Self {
            extension: None,
            basename: None,
        }
    }

    /// A safe file name carrying this hint. Always non-empty, never a path.
    pub fn sanitized_file_name(&self) -> String {
        let base = self
            .basename
            .as_deref()
            .and_then(|b| sanitize_component(b, MAX_BASENAME_BYTES));
        let ext = self
            .extension
            .as_deref()
            .and_then(|e| sanitize_extension(e, MAX_EXTENSION_BYTES));

        match (base, ext) {
            // A surviving basename already ends with the extension.
            (Some(b), _) => b,
            (None, Some(e)) => format!("file.{e}"),
            (None, None) => "file".to_string(),
        }
    }
}

/// Keep a component only if it is valid UTF-8, free of separators and NUL,
/// not a dot-only name, and within the length cap. All-or-nothing: a mangled
/// hint is worse than no hint.
fn sanitize_component(bytes: &[u8], max_len: usize) -> Option<String> {
    if bytes.is_empty() || bytes.len() > max_len {
        return None;
    }
    let s = std::str::from_utf8(bytes).ok()?;
    if s == "." || s == ".." {
        return None;
    }
    if s.bytes().any(|b| b == b'/' || b == b'\\' || b == 0) {
        return None;
    }
    Some(s.to_string())
}

fn sanitize_extension(bytes: &[u8], max_len: usize) -> Option<String> {
    let s = sanitize_component(bytes, max_len)?;
    if s.contains('.') {
        return None;
    }
    Some(s)
}

/// A materialized pair of temp files for one difft invocation. The backing
/// directory is removed on drop.
pub struct DifftTempPair {
    _dir: TempDir,
    pub old_path: PathBuf,
    pub new_path: PathBuf,
}

/// Write both sides to disk under hint-derived names.
pub fn materialize(
    old_bytes: &[u8],
    new_bytes: &[u8],
    old_hint: &LanguagePathHint,
    new_hint: &LanguagePathHint,
) -> io::Result<DifftTempPair> {
    let dir = TempDir::with_prefix("tsuiku-")?;
    let old_path = dir.path().join("old").join(old_hint.sanitized_file_name());
    let new_path = dir.path().join("new").join(new_hint.sanitized_file_name());
    fs::create_dir(dir.path().join("old"))?;
    fs::create_dir(dir.path().join("new"))?;
    fs::write(&old_path, old_bytes)?;
    fs::write(&new_path, new_bytes)?;
    Ok(DifftTempPair {
        _dir: dir,
        old_path,
        new_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hint(path: &[u8]) -> LanguagePathHint {
        LanguagePathHint::from_git_path(&GitPath::from_bytes(path))
    }

    #[test]
    fn plain_rust_file() {
        assert_eq!(hint(b"src/main.rs").sanitized_file_name(), "main.rs");
    }

    #[test]
    fn cjk_basename_survives() {
        assert_eq!(hint("対句.rs".as_bytes()).sanitized_file_name(), "対句.rs");
    }

    #[test]
    fn non_utf8_basename_falls_back_to_extension() {
        assert_eq!(hint(b"dir/\xff\xfe.rs").sanitized_file_name(), "file.rs");
    }

    #[test]
    fn non_utf8_everything_falls_back_to_bare_file() {
        assert_eq!(hint(b"\xff\xfe").sanitized_file_name(), "file");
    }

    #[test]
    fn overlong_basename_keeps_extension() {
        let long = [b'a'; 100];
        let mut p = long.to_vec();
        p.extend_from_slice(b".rs");
        assert_eq!(hint(&p).sanitized_file_name(), "file.rs");
    }

    #[test]
    fn dotfile_keeps_its_name() {
        assert_eq!(hint(b".gitignore").sanitized_file_name(), ".gitignore");
    }

    #[test]
    fn materialize_writes_both_sides() {
        let pair = materialize(b"old\n", b"new\n", &hint(b"a.rs"), &hint(b"a.rs")).unwrap();
        assert_eq!(fs::read(&pair.old_path).unwrap(), b"old\n");
        assert_eq!(fs::read(&pair.new_path).unwrap(), b"new\n");
        assert!(pair.old_path.ends_with("old/a.rs"));
        assert!(pair.new_path.ends_with("new/a.rs"));
        let dir = pair
            .old_path
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        drop(pair);
        assert!(!dir.exists(), "temp dir must be removed on drop");
    }
}
