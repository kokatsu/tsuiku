//! Raw serde types for difftastic's JSON output (`DFT_UNSTABLE=yes
//! --display json`), exactly as measured against difft 0.69.0.
//!
//! Shape summary (all measured, not documented upstream):
//!
//! - One JSON object per file pair. `chunks` and `aligned_lines` are absent
//!   for `unchanged` / `created` / `deleted` files.
//! - `chunks` is a list of hunks; each hunk is a list of entries carrying an
//!   optional `lhs` and/or `rhs` side. Entries within a hunk are NOT sorted
//!   by line number.
//! - `line_number` is 0-based. `start`/`end` are UTF-8 byte offsets relative
//!   to the line body (CR/LF excluded), end-exclusive. Zero-width spans
//!   (`start == end`, empty `content`) occur in practice.
//! - `language` is free-form text (parse failures yield strings like
//!   "Text (2 Rust parse errors, ...)"), so it is kept as a `String`.
//!
//! Unknown fields are ignored on purpose: newer difft versions may add
//! fields, and the version policy is "try, disable on failure".

use serde::Deserialize;

#[derive(Deserialize, Debug, Clone)]
pub struct RawFileDiff {
    pub language: String,
    pub path: String,
    pub status: String,
    #[serde(default)]
    pub chunks: Option<Vec<Vec<RawChunkEntry>>>,
    /// Parsed for completeness but never used for layout: line alignment is
    /// always our own line-diff engine's job.
    #[serde(default)]
    pub aligned_lines: Option<Vec<(Option<u32>, Option<u32>)>>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct RawChunkEntry {
    #[serde(default)]
    pub lhs: Option<RawSide>,
    #[serde(default)]
    pub rhs: Option<RawSide>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct RawSide {
    /// 0-based line index into that side's file.
    pub line_number: u32,
    pub changes: Vec<RawChange>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct RawChange {
    /// Byte offset into the line body, inclusive.
    pub start: usize,
    /// Byte offset into the line body, exclusive.
    pub end: usize,
    /// The changed token text; used to cross-check our coordinates.
    pub content: String,
    /// Syntax category of the token ("keyword", "string", "comment",
    /// "delimiter", "normal", ...). Open set.
    pub highlight: String,
}

pub fn parse(json: &str) -> Result<RawFileDiff, serde_json::Error> {
    serde_json::from_str(json)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured verbatim from difft 0.69.0 on a CJK fixture.
    const CJK_CAPTURE: &str = r#"{"aligned_lines":[[0,0],[1,1],[2,2],[3,3],[4,4]],"chunks":[[{"lhs":{"line_number":0,"changes":[{"start":0,"end":18,"content":"// コメント甲","highlight":"comment"}]},"rhs":{"line_number":0,"changes":[{"start":0,"end":18,"content":"// コメント乙","highlight":"comment"}]}},{"lhs":{"line_number":2,"changes":[{"start":12,"end":32,"content":"\"日本語テスト\"","highlight":"string"}]},"rhs":{"line_number":2,"changes":[{"start":12,"end":29,"content":"\"日本語試験\"","highlight":"string"}]}}]],"language":"Rust","path":"cjk_new.rs","status":"changed"}"#;

    /// Captured verbatim: pure creation carries no chunks/aligned_lines.
    const CREATED_CAPTURE: &str = r#"{"language":"Rust","path":"nonempty.rs","status":"created"}"#;

    /// Captured verbatim: one-sided entry (added line) and null alignment.
    const ADDED_CAPTURE: &str = r#"{"aligned_lines":[[0,0],[null,1],[1,2]],"chunks":[[{"rhs":{"line_number":1,"changes":[{"start":0,"end":2,"content":"fn","highlight":"keyword"}]}}]],"language":"Rust","path":"add_new.rs","status":"changed"}"#;

    #[test]
    fn parses_cjk_capture() {
        let d = parse(CJK_CAPTURE).unwrap();
        assert_eq!(d.status, "changed");
        let chunks = d.chunks.unwrap();
        assert_eq!(chunks.len(), 1);
        let entry = &chunks[0][0];
        let lhs = entry.lhs.as_ref().unwrap();
        assert_eq!(lhs.line_number, 0);
        // "// コメント甲" is 18 bytes: byte offsets, not chars.
        assert_eq!(lhs.changes[0].end, 18);
    }

    #[test]
    fn created_has_no_chunks() {
        let d = parse(CREATED_CAPTURE).unwrap();
        assert_eq!(d.status, "created");
        assert!(d.chunks.is_none());
        assert!(d.aligned_lines.is_none());
    }

    #[test]
    fn one_sided_entry_and_null_alignment() {
        let d = parse(ADDED_CAPTURE).unwrap();
        let chunks = d.chunks.unwrap();
        assert!(chunks[0][0].lhs.is_none());
        assert!(chunks[0][0].rhs.is_some());
        assert_eq!(d.aligned_lines.unwrap()[1], (None, Some(1)));
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let d = parse(r#"{"language":"Text","path":"x","status":"changed","future_field":42}"#)
            .unwrap();
        assert_eq!(d.language, "Text");
    }

    #[test]
    fn missing_required_field_is_an_error() {
        assert!(parse(r#"{"language":"Text","path":"x"}"#).is_err());
    }
}
