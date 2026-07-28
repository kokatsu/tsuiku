//! Text classification and the line view.
//!
//! Raw bytes are never normalized: CRLF, missing trailing newlines, and every
//! other byte-level detail stay in the buffer untouched. The line view is a
//! table of byte ranges over those raw bytes:
//!
//! - `full_range` covers the line including its terminator, so consecutive
//!   lines tile the file exactly.
//! - `body_range` excludes the CR/LF terminator. Display and span validation
//!   use the body.
//!
//! Classification is deliberately conservative and does not follow
//! `.gitattributes`: a file is binary if a NUL byte appears in its first
//! 8 KiB (heuristic) or if the whole file is not valid UTF-8. Everything else
//! is text. Binary content is shown as "Binary files differ" and never gets
//! line diffs or overlays.

use std::sync::Arc;

use crate::coords::{FileByteOffset, FileByteRange, LineIndex};

/// How many leading bytes are scanned for NUL when classifying.
pub const NUL_SCAN_LIMIT: usize = 8 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NewlineKind {
    /// Last line of a file with no trailing newline.
    None,
    Lf,
    CrLf,
}

/// One line of a text file, as byte ranges into the raw buffer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LineRecord {
    /// The line including its terminator.
    pub full_range: FileByteRange,
    /// The line without CR/LF. Display and span validation use this.
    pub body_range: FileByteRange,
    pub newline: NewlineKind,
}

/// A classified text file: raw bytes plus the line table.
#[derive(Clone, Debug)]
pub struct TextContent {
    pub bytes: Arc<[u8]>,
    pub lines: Vec<LineRecord>,
}

impl TextContent {
    pub fn line(&self, idx: LineIndex) -> Option<&LineRecord> {
        self.lines.get(idx.0)
    }

    /// The line body as `&str`. Safe because classification guaranteed the
    /// whole buffer is valid UTF-8 and body ranges never split the buffer
    /// mid-character (they end at CR/LF or EOF).
    pub fn line_body_str(&self, idx: LineIndex) -> Option<&str> {
        let rec = self.line(idx)?;
        Some(std::str::from_utf8(rec.body_range.slice(&self.bytes)).expect("classified as UTF-8"))
    }

    /// Binary-search the line containing a file byte offset.
    pub fn line_of_offset(&self, off: FileByteOffset) -> Option<LineIndex> {
        let i = self
            .lines
            .partition_point(|rec| rec.full_range.end <= off.0);
        self.lines
            .get(i)
            .filter(|rec| rec.full_range.contains_offset(off))
            .map(|_| LineIndex(i))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BinaryReason {
    ContainsNul,
    InvalidUtf8,
}

/// Result of classifying present (non-absent) content.
#[derive(Clone, Debug)]
pub enum ClassifiedContent {
    Text(TextContent),
    Binary(BinaryReason),
}

/// Classify raw bytes as text or binary and build the line view.
pub fn classify(bytes: Arc<[u8]>) -> ClassifiedContent {
    let scan = &bytes[..bytes.len().min(NUL_SCAN_LIMIT)];
    if scan.contains(&0) {
        return ClassifiedContent::Binary(BinaryReason::ContainsNul);
    }
    if std::str::from_utf8(&bytes).is_err() {
        return ClassifiedContent::Binary(BinaryReason::InvalidUtf8);
    }
    let lines = split_lines(&bytes);
    ClassifiedContent::Text(TextContent { bytes, lines })
}

/// Split raw bytes into lines. Only LF terminates a line; a CR immediately
/// before the LF belongs to the terminator (CRLF). A lone CR is ordinary
/// line content, matching git's line model.
pub fn split_lines(bytes: &[u8]) -> Vec<LineRecord> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            let (body_end, newline) = if i > start && bytes[i - 1] == b'\r' {
                (i - 1, NewlineKind::CrLf)
            } else {
                (i, NewlineKind::Lf)
            };
            lines.push(LineRecord {
                full_range: FileByteRange::new(start, i + 1),
                body_range: FileByteRange::new(start, body_end),
                newline,
            });
            start = i + 1;
        }
    }
    if start < bytes.len() {
        lines.push(LineRecord {
            full_range: FileByteRange::new(start, bytes.len()),
            body_range: FileByteRange::new(start, bytes.len()),
            newline: NewlineKind::None,
        });
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(bytes: &[u8]) -> TextContent {
        match classify(Arc::from(bytes)) {
            ClassifiedContent::Text(t) => t,
            ClassifiedContent::Binary(r) => panic!("expected text, got binary: {r:?}"),
        }
    }

    fn binary_reason(bytes: &[u8]) -> BinaryReason {
        match classify(Arc::from(bytes)) {
            ClassifiedContent::Binary(r) => r,
            ClassifiedContent::Text(_) => panic!("expected binary"),
        }
    }

    #[test]
    fn empty_file_has_no_lines() {
        assert!(text(b"").lines.is_empty());
    }

    #[test]
    fn lf_lines_tile_the_file() {
        let t = text(b"a\nbb\n");
        assert_eq!(t.lines.len(), 2);
        assert_eq!(t.lines[0].full_range, FileByteRange::new(0, 2));
        assert_eq!(t.lines[0].body_range, FileByteRange::new(0, 1));
        assert_eq!(t.lines[1].full_range, FileByteRange::new(2, 5));
        assert_eq!(t.lines[1].body_range, FileByteRange::new(2, 4));
        assert_eq!(t.lines[1].newline, NewlineKind::Lf);
    }

    #[test]
    fn crlf_body_excludes_cr() {
        let t = text(b"ab\r\ncd\r\n");
        assert_eq!(t.lines[0].body_range, FileByteRange::new(0, 2));
        assert_eq!(t.lines[0].full_range, FileByteRange::new(0, 4));
        assert_eq!(t.lines[0].newline, NewlineKind::CrLf);
    }

    #[test]
    fn lone_cr_is_line_content() {
        let t = text(b"a\rb\n");
        assert_eq!(t.lines.len(), 1);
        assert_eq!(t.lines[0].body_range, FileByteRange::new(0, 3));
        assert_eq!(t.lines[0].newline, NewlineKind::Lf);
    }

    #[test]
    fn missing_final_newline() {
        let t = text(b"a\nb");
        assert_eq!(t.lines.len(), 2);
        assert_eq!(t.lines[1].newline, NewlineKind::None);
        assert_eq!(t.lines[1].body_range, FileByteRange::new(2, 3));
        assert_eq!(t.lines[1].full_range, FileByteRange::new(2, 3));
    }

    #[test]
    fn crlf_only_line_has_empty_body() {
        let t = text(b"\r\n");
        assert_eq!(t.lines.len(), 1);
        assert!(t.lines[0].body_range.is_empty());
        assert_eq!(t.lines[0].newline, NewlineKind::CrLf);
    }

    #[test]
    fn cjk_line_body_str() {
        let t = text("こんにちは\n世界\n".as_bytes());
        assert_eq!(t.line_body_str(LineIndex(0)), Some("こんにちは"));
        assert_eq!(t.line_body_str(LineIndex(1)), Some("世界"));
    }

    #[test]
    fn nul_in_head_is_binary() {
        assert_eq!(binary_reason(b"ab\x00cd"), BinaryReason::ContainsNul);
    }

    #[test]
    fn nul_beyond_scan_limit_stays_text() {
        let mut bytes = vec![b'a'; NUL_SCAN_LIMIT];
        bytes.push(0);
        bytes.push(b'\n');
        // NUL after the scan window: the heuristic accepts it as text
        // (NUL is a valid UTF-8 code point).
        assert!(matches!(
            classify(Arc::from(&bytes[..])),
            ClassifiedContent::Text(_)
        ));
    }

    #[test]
    fn invalid_utf8_is_binary() {
        assert_eq!(binary_reason(b"ok\xff\xfe"), BinaryReason::InvalidUtf8);
    }

    #[test]
    fn invalid_utf8_beyond_8k_is_still_binary() {
        let mut bytes = vec![b'a'; NUL_SCAN_LIMIT + 10];
        bytes.push(0xff);
        assert_eq!(binary_reason(&bytes), BinaryReason::InvalidUtf8);
    }

    #[test]
    fn line_of_offset_lookup() {
        let t = text(b"aa\nbb\ncc");
        assert_eq!(t.line_of_offset(FileByteOffset(0)), Some(LineIndex(0)));
        assert_eq!(t.line_of_offset(FileByteOffset(2)), Some(LineIndex(0)));
        assert_eq!(t.line_of_offset(FileByteOffset(3)), Some(LineIndex(1)));
        assert_eq!(t.line_of_offset(FileByteOffset(7)), Some(LineIndex(2)));
        assert_eq!(t.line_of_offset(FileByteOffset(8)), None);
    }
}
