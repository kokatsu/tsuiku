//! Path contracts. Git paths are arbitrary byte strings; OS paths are
//! `PathBuf`. Targets Unix (macOS/Linux), where the conversion is lossless.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// A path as git reports it: an arbitrary byte string, '/'-separated.
/// Never assume UTF-8; escape only for display.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct GitPath(Vec<u8>);

impl GitPath {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self(bytes.to_vec())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Final path component (after the last '/'), as raw bytes.
    pub fn file_name(&self) -> &[u8] {
        match self.0.iter().rposition(|&b| b == b'/') {
            Some(i) => &self.0[i + 1..],
            None => &self.0,
        }
    }

    /// Extension of the final component: bytes after the last '.', unless the
    /// dot is the first byte of the file name (dotfiles have no extension).
    pub fn extension(&self) -> Option<&[u8]> {
        let name = self.file_name();
        match name.iter().rposition(|&b| b == b'.') {
            Some(0) | None => None,
            Some(i) => Some(&name[i + 1..]),
        }
    }

    /// Lossy display form, safe to print to a terminal: invalid bytes become
    /// `\xNN`, and control characters (C0, DEL, C1) are escaped too so a
    /// crafted file name cannot inject terminal escape sequences.
    pub fn display_escaped(&self) -> String {
        let mut out = String::with_capacity(self.0.len());
        let mut rest = &self.0[..];
        loop {
            match std::str::from_utf8(rest) {
                Ok(s) => {
                    push_escaping_controls(&mut out, s);
                    return out;
                }
                Err(e) => {
                    let (valid, after) = rest.split_at(e.valid_up_to());
                    push_escaping_controls(
                        &mut out,
                        std::str::from_utf8(valid).expect("valid_up_to guarantees UTF-8"),
                    );
                    let bad_len = e.error_len().unwrap_or(after.len());
                    for b in &after[..bad_len] {
                        out.push_str(&format!("\\x{b:02x}"));
                    }
                    rest = &after[bad_len..];
                }
            }
        }
    }
}

fn push_escaping_controls(out: &mut String, s: &str) {
    for c in s.chars() {
        if c.is_control() {
            if (c as u32) < 0x80 {
                out.push_str(&format!("\\x{:02x}", c as u32));
            } else {
                out.push_str(&format!("\\u{{{:x}}}", c as u32));
            }
        } else {
            out.push(c);
        }
    }
}

/// A path on the local filesystem. Produced from `GitPath` only by the
/// path resolution layer, never by ad-hoc conversion.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct WorktreePath(pub PathBuf);

/// Turns repository-relative git paths into filesystem paths. On Unix the
/// conversion is lossless, because both sides are byte strings.
#[derive(Clone, Debug)]
pub struct PathResolver {
    workdir: PathBuf,
}

impl PathResolver {
    pub fn new(workdir: PathBuf) -> Self {
        Self { workdir }
    }

    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    pub fn resolve(&self, path: &GitPath) -> WorktreePath {
        use std::os::unix::ffi::OsStrExt;
        WorktreePath(self.workdir.join(OsStr::from_bytes(path.as_bytes())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_name_and_extension() {
        let p = GitPath::from_bytes(b"src/foo/bar.rs");
        assert_eq!(p.file_name(), b"bar.rs");
        assert_eq!(p.extension(), Some(&b"rs"[..]));
    }

    #[test]
    fn dotfile_has_no_extension() {
        let p = GitPath::from_bytes(b"dir/.gitignore");
        assert_eq!(p.file_name(), b".gitignore");
        assert_eq!(p.extension(), None);
    }

    #[test]
    fn no_dot_no_extension() {
        let p = GitPath::from_bytes(b"Makefile");
        assert_eq!(p.extension(), None);
    }

    #[test]
    fn display_escapes_invalid_utf8() {
        let p = GitPath::from_bytes(b"a\xffb.txt");
        assert_eq!(p.display_escaped(), "a\\xffb.txt");
    }

    #[test]
    fn display_passes_cjk() {
        let p = GitPath::from_bytes("対句/漢詩.rs".as_bytes());
        assert_eq!(p.display_escaped(), "対句/漢詩.rs");
    }

    #[test]
    fn display_escapes_ansi_escape_sequences() {
        // A file name carrying a terminal escape sequence must not reach the
        // terminal raw.
        let p = GitPath::from_bytes(b"evil\x1b[31mred.rs");
        assert_eq!(p.display_escaped(), "evil\\x1b[31mred.rs");
    }

    #[test]
    fn display_escapes_c0_and_del() {
        let p = GitPath::from_bytes(b"a\nb\tc\x7fd");
        assert_eq!(p.display_escaped(), "a\\x0ab\\x09c\\x7fd");
    }

    #[test]
    fn display_escapes_c1_controls() {
        // U+0085 (NEL) is a valid-UTF-8 control character.
        let p = GitPath::from_bytes("x\u{85}y".as_bytes());
        assert_eq!(p.display_escaped(), "x\\u{85}y");
    }
}
