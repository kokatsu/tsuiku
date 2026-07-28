//! Normalization: raw difft JSON → validated `StructuralOverlay`.
//!
//! Every span is checked against our own line view before it may render:
//!
//! 1. the target line must exist on that side,
//! 2. the byte range must lie within the line body (CR/LF excluded),
//! 3. both endpoints must be UTF-8 character boundaries,
//! 4. the span's `content` must equal the bytes our coordinates select —
//!    a mismatch means the coordinate contract drifted, so the span is
//!    dropped rather than misrendered,
//! 5. zero-width spans are dropped.
//!
//! Surviving spans are sorted by (line, start) — difft emits hunk entries in
//! no particular order — and same-kind overlapping/adjacent spans on one line
//! are merged. `OverlayDiagnostics` counts every decision so rejection rates
//! can be monitored.

use crate::coords::{LineByteRange, LineIndex};
use crate::structural::json::{RawFileDiff, RawSide};
use crate::text::TextContent;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DifftStatus {
    Unchanged,
    Changed,
    Created,
    Deleted,
    Unknown,
}

impl DifftStatus {
    pub fn from_label(s: &str) -> Self {
        match s {
            "unchanged" => Self::Unchanged,
            "changed" => Self::Changed,
            "created" => Self::Created,
            "deleted" => Self::Deleted,
            _ => Self::Unknown,
        }
    }
}

/// Syntax category difft assigned to a changed token. Open set upstream;
/// unknown values map to `Other`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HighlightKind {
    Normal,
    Keyword,
    String,
    Comment,
    Delimiter,
    TypeName,
    Other,
}

impl HighlightKind {
    pub fn from_label(s: &str) -> Self {
        match s {
            "normal" => Self::Normal,
            "keyword" => Self::Keyword,
            "string" => Self::String,
            "comment" => Self::Comment,
            "delimiter" => Self::Delimiter,
            "type" => Self::TypeName,
            _ => Self::Other,
        }
    }
}

/// A validated change span on one line of one side.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LineSpan {
    pub line: LineIndex,
    /// Byte range relative to the line body.
    pub range: LineByteRange,
    pub kind: HighlightKind,
}

/// All validated spans for one side, sorted by (line, start).
#[derive(Clone, Debug, Default)]
pub struct SideOverlay {
    spans: Vec<LineSpan>,
}

impl SideOverlay {
    pub fn spans(&self) -> &[LineSpan] {
        &self.spans
    }

    /// All spans on one line, via binary search.
    pub fn spans_for_line(&self, line: LineIndex) -> &[LineSpan] {
        let lo = self.spans.partition_point(|s| s.line < line);
        let hi = self.spans.partition_point(|s| s.line <= line);
        &self.spans[lo..hi]
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct OverlayDiagnostics {
    pub total: u32,
    pub accepted: u32,
    pub rejected_out_of_bounds: u32,
    pub rejected_invalid_boundary: u32,
    pub rejected_content_mismatch: u32,
    pub rejected_empty: u32,
    /// Spans absorbed into a neighbor during merging.
    pub merged: u32,
}

/// The normalized product of one difft run.
#[derive(Clone, Debug)]
pub struct StructuralOverlay {
    pub status: DifftStatus,
    /// Free-form language label from difft, for the status bar only.
    pub language: String,
    pub old: SideOverlay,
    pub new: SideOverlay,
    pub diagnostics: OverlayDiagnostics,
}

/// Validate and normalize a raw difft result against the two line views.
/// A side given as `None` is absent (created/deleted file); spans targeting
/// it are rejected as out of bounds.
pub fn normalize(
    raw: &RawFileDiff,
    old: Option<&TextContent>,
    new: Option<&TextContent>,
) -> StructuralOverlay {
    let mut diagnostics = OverlayDiagnostics::default();
    let mut old_spans = Vec::new();
    let mut new_spans = Vec::new();

    if let Some(chunks) = &raw.chunks {
        for hunk in chunks {
            for entry in hunk {
                if let Some(side) = &entry.lhs {
                    collect_side(side, old, &mut old_spans, &mut diagnostics);
                }
                if let Some(side) = &entry.rhs {
                    collect_side(side, new, &mut new_spans, &mut diagnostics);
                }
            }
        }
    }

    StructuralOverlay {
        status: DifftStatus::from_label(&raw.status),
        language: raw.language.clone(),
        old: finish_side(old_spans, &mut diagnostics),
        new: finish_side(new_spans, &mut diagnostics),
        diagnostics,
    }
}

fn collect_side(
    side: &RawSide,
    text: Option<&TextContent>,
    out: &mut Vec<LineSpan>,
    diag: &mut OverlayDiagnostics,
) {
    for change in &side.changes {
        diag.total += 1;

        if change.start == change.end {
            diag.rejected_empty += 1;
            continue;
        }
        if change.start > change.end {
            diag.rejected_out_of_bounds += 1;
            continue;
        }
        let Some(text) = text else {
            diag.rejected_out_of_bounds += 1;
            continue;
        };
        let line = LineIndex(side.line_number as usize);
        let Some(body) = text.line_body_str(line) else {
            diag.rejected_out_of_bounds += 1;
            continue;
        };
        if change.end > body.len() {
            diag.rejected_out_of_bounds += 1;
            continue;
        }
        if !body.is_char_boundary(change.start) || !body.is_char_boundary(change.end) {
            diag.rejected_invalid_boundary += 1;
            continue;
        }
        if body.as_bytes()[change.start..change.end] != *change.content.as_bytes() {
            diag.rejected_content_mismatch += 1;
            continue;
        }

        diag.accepted += 1;
        out.push(LineSpan {
            line,
            range: LineByteRange::new(change.start, change.end),
            kind: HighlightKind::from_label(&change.highlight),
        });
    }
}

/// Sort and merge one side's accepted spans.
fn finish_side(mut spans: Vec<LineSpan>, diag: &mut OverlayDiagnostics) -> SideOverlay {
    spans.sort_by_key(|s| (s.line, s.range.start, s.range.end));
    let mut merged: Vec<LineSpan> = Vec::with_capacity(spans.len());
    for span in spans {
        match merged.last_mut() {
            Some(last)
                if last.line == span.line
                    && last.kind == span.kind
                    && span.range.start <= last.range.end =>
            {
                last.range =
                    LineByteRange::new(last.range.start, last.range.end.max(span.range.end));
                diag.merged += 1;
            }
            _ => merged.push(span),
        }
    }
    SideOverlay { spans: merged }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::structural::json::parse;
    use crate::text::{ClassifiedContent, classify};
    use std::sync::Arc;

    fn text(s: &str) -> TextContent {
        match classify(Arc::from(s.as_bytes())) {
            ClassifiedContent::Text(t) => t,
            ClassifiedContent::Binary(_) => panic!("fixture must be text"),
        }
    }

    /// Captured verbatim from difft 0.69.0 (CJK fixture).
    const CJK_CAPTURE: &str = r#"{"aligned_lines":[[0,0],[1,1],[2,2],[3,3],[4,4]],"chunks":[[{"lhs":{"line_number":0,"changes":[{"start":0,"end":18,"content":"// コメント甲","highlight":"comment"}]},"rhs":{"line_number":0,"changes":[{"start":0,"end":18,"content":"// コメント乙","highlight":"comment"}]}},{"lhs":{"line_number":2,"changes":[{"start":12,"end":32,"content":"\"日本語テスト\"","highlight":"string"}]},"rhs":{"line_number":2,"changes":[{"start":12,"end":29,"content":"\"日本語試験\"","highlight":"string"}]}}]],"language":"Rust","path":"cjk_new.rs","status":"changed"}"#;

    const CJK_OLD: &str = "// コメント甲\nfn main() {\n    let s = \"日本語テスト\";\n}\n";
    const CJK_NEW: &str = "// コメント乙\nfn main() {\n    let s = \"日本語試験\";\n}\n";

    #[test]
    fn cjk_capture_fully_accepted() {
        let raw = parse(CJK_CAPTURE).unwrap();
        let (old, new) = (text(CJK_OLD), text(CJK_NEW));
        let overlay = normalize(&raw, Some(&old), Some(&new));
        assert_eq!(overlay.diagnostics.total, 4);
        assert_eq!(overlay.diagnostics.accepted, 4);
        assert_eq!(overlay.old.spans().len(), 2);
        assert_eq!(overlay.new.spans().len(), 2);
        let s = overlay.new.spans_for_line(LineIndex(2));
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].range, LineByteRange::new(12, 29));
        assert_eq!(s[0].kind, HighlightKind::String);
    }

    #[test]
    fn zero_width_span_rejected() {
        let raw = parse(
            r#"{"language":"Text","path":"x","status":"changed","chunks":[[{"rhs":{"line_number":0,"changes":[{"start":3,"end":3,"content":"","highlight":"normal"}]}}]]}"#,
        )
        .unwrap();
        let new = text("abc\n");
        let overlay = normalize(&raw, None, Some(&new));
        assert_eq!(overlay.diagnostics.rejected_empty, 1);
        assert_eq!(overlay.diagnostics.accepted, 0);
    }

    #[test]
    fn out_of_bounds_span_rejected() {
        let raw = parse(
            r#"{"language":"Text","path":"x","status":"changed","chunks":[[{"rhs":{"line_number":0,"changes":[{"start":0,"end":10,"content":"toolongxxx","highlight":"normal"}]}}]]}"#,
        )
        .unwrap();
        let new = text("abc\n");
        let overlay = normalize(&raw, None, Some(&new));
        assert_eq!(overlay.diagnostics.rejected_out_of_bounds, 1);
    }

    #[test]
    fn missing_line_rejected() {
        let raw = parse(
            r#"{"language":"Text","path":"x","status":"changed","chunks":[[{"rhs":{"line_number":9,"changes":[{"start":0,"end":1,"content":"a","highlight":"normal"}]}}]]}"#,
        )
        .unwrap();
        let new = text("abc\n");
        let overlay = normalize(&raw, None, Some(&new));
        assert_eq!(overlay.diagnostics.rejected_out_of_bounds, 1);
    }

    #[test]
    fn non_char_boundary_rejected() {
        // Line is "あいう" (9 bytes); offset 1 splits あ.
        let raw = parse(
            r#"{"language":"Text","path":"x","status":"changed","chunks":[[{"rhs":{"line_number":0,"changes":[{"start":1,"end":4,"content":"xxx","highlight":"normal"}]}}]]}"#,
        )
        .unwrap();
        let new = text("あいう\n");
        let overlay = normalize(&raw, None, Some(&new));
        assert_eq!(overlay.diagnostics.rejected_invalid_boundary, 1);
    }

    #[test]
    fn content_mismatch_rejected() {
        let raw = parse(
            r#"{"language":"Text","path":"x","status":"changed","chunks":[[{"rhs":{"line_number":0,"changes":[{"start":0,"end":3,"content":"xyz","highlight":"normal"}]}}]]}"#,
        )
        .unwrap();
        let new = text("abc\n");
        let overlay = normalize(&raw, None, Some(&new));
        assert_eq!(overlay.diagnostics.rejected_content_mismatch, 1);
    }

    #[test]
    fn crlf_span_validates_against_body() {
        // Body is "foo" (3 bytes); full line is "foo\r\n". A span covering
        // the whole body must pass, one reaching into CR must fail.
        let ok = parse(
            r#"{"language":"Text","path":"x","status":"changed","chunks":[[{"rhs":{"line_number":0,"changes":[{"start":0,"end":3,"content":"foo","highlight":"normal"}]}}]]}"#,
        )
        .unwrap();
        let bad = parse(
            r#"{"language":"Text","path":"x","status":"changed","chunks":[[{"rhs":{"line_number":0,"changes":[{"start":0,"end":4,"content":"foo\r","highlight":"normal"}]}}]]}"#,
        )
        .unwrap();
        let new = text("foo\r\nbar\r\n");
        assert_eq!(normalize(&ok, None, Some(&new)).diagnostics.accepted, 1);
        assert_eq!(
            normalize(&bad, None, Some(&new))
                .diagnostics
                .rejected_out_of_bounds,
            1
        );
    }

    #[test]
    fn unsorted_entries_get_sorted_and_merged() {
        // difft emits entries in arbitrary order; adjacent same-kind spans
        // ("ab" then "c") must come out as one sorted span.
        let raw = parse(
            r#"{"language":"Text","path":"x","status":"changed","chunks":[[
                {"rhs":{"line_number":1,"changes":[{"start":0,"end":1,"content":"z","highlight":"normal"}]}},
                {"rhs":{"line_number":0,"changes":[{"start":2,"end":3,"content":"c","highlight":"normal"}]}},
                {"rhs":{"line_number":0,"changes":[{"start":0,"end":2,"content":"ab","highlight":"normal"}]}}
            ]]}"#,
        )
        .unwrap();
        let new = text("abc\nz\n");
        let overlay = normalize(&raw, None, Some(&new));
        assert_eq!(overlay.diagnostics.accepted, 3);
        assert_eq!(overlay.diagnostics.merged, 1);
        let spans = overlay.new.spans();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].line, LineIndex(0));
        assert_eq!(spans[0].range, LineByteRange::new(0, 3));
        assert_eq!(spans[1].line, LineIndex(1));
    }
}
