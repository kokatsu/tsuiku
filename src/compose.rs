//! Style composition: line-diff rows decorated with structural spans.
//!
//! The line-diff layer decides layout (which line appears on which row and
//! whether it is context/removed/added); the structural layer only splits a
//! row's text into highlighted and plain segments. When no overlay data
//! exists for a line the row renders unhighlighted — layers compose, they
//! never replace each other.
//!
//! Removed rows read the old side, added rows the new side; context rows
//! read the new side (both sides hold identical text there).

use crate::coords::LineIndex;
use crate::linediff::DiffRow;
use crate::structural::normalize::{HighlightKind, LineSpan, StructuralOverlay};
use crate::text::TextContent;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RowKind {
    Context,
    Removed,
    Added,
}

/// One contiguous run of a line body with a single style.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Segment<'a> {
    pub text: &'a str,
    /// `Some` when a structural span covers this run.
    pub highlight: Option<HighlightKind>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ComposedRow<'a> {
    pub kind: RowKind,
    pub old_line: Option<LineIndex>,
    pub new_line: Option<LineIndex>,
    pub segments: Vec<Segment<'a>>,
}

/// Split one line body into segments at span boundaries. `spans` must be the
/// normalized (sorted, merged, validated) spans of exactly this line.
pub fn segment_line<'a>(
    text: &'a TextContent,
    line: LineIndex,
    spans: &[LineSpan],
) -> Vec<Segment<'a>> {
    let body = text
        .line_body_str(line)
        .expect("caller resolved the line from this text");
    let mut segments = Vec::with_capacity(spans.len() * 2 + 1);
    let mut cursor = 0usize;
    for span in spans {
        debug_assert_eq!(span.line, line);
        // Normalization guarantees in-bounds, char-aligned, sorted spans;
        // guard anyway so a violated invariant degrades to plain text.
        if span.range.start < cursor || span.range.end > body.len() {
            continue;
        }
        if span.range.start > cursor {
            segments.push(Segment {
                text: &body[cursor..span.range.start],
                highlight: None,
            });
        }
        segments.push(Segment {
            text: &body[span.range.start..span.range.end],
            highlight: Some(span.kind),
        });
        cursor = span.range.end;
    }
    if cursor < body.len() || body.is_empty() {
        segments.push(Segment {
            text: &body[cursor..],
            highlight: None,
        });
    }
    segments
}

/// Compose one display row from the line diff and the structural overlay.
pub fn compose_row<'a>(
    row: DiffRow,
    old_text: &'a TextContent,
    new_text: &'a TextContent,
    overlay: Option<&StructuralOverlay>,
) -> ComposedRow<'a> {
    let (kind, old_line, new_line, text, side_line) = match row {
        DiffRow::Context { old, new } => (RowKind::Context, Some(old), Some(new), new_text, new),
        DiffRow::Removed { old } => (RowKind::Removed, Some(old), None, old_text, old),
        DiffRow::Added { new } => (RowKind::Added, None, Some(new), new_text, new),
    };
    let spans = overlay
        .map(|ov| match kind {
            RowKind::Removed => ov.old.spans_for_line(side_line),
            _ => ov.new.spans_for_line(side_line),
        })
        .unwrap_or(&[]);
    ComposedRow {
        kind,
        old_line,
        new_line,
        segments: segment_line(text, side_line, spans),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asyncstate::LineDiffEngineId;
    use crate::linediff::{engine, line_tokens};
    use crate::structural::json::parse;
    use crate::structural::normalize::normalize;
    use crate::text::{ClassifiedContent, classify};
    use std::sync::Arc;

    fn text(s: &str) -> TextContent {
        match classify(Arc::from(s.as_bytes())) {
            ClassifiedContent::Text(t) => t,
            ClassifiedContent::Binary(_) => panic!("fixture must be text"),
        }
    }

    fn reassembled(segments: &[Segment<'_>]) -> String {
        segments.iter().map(|s| s.text).collect()
    }

    /// Captured verbatim from difft 0.69.0 (CJK fixture).
    const CJK_CAPTURE: &str = r#"{"aligned_lines":[[0,0],[1,1],[2,2],[3,3],[4,4]],"chunks":[[{"lhs":{"line_number":0,"changes":[{"start":0,"end":18,"content":"// コメント甲","highlight":"comment"}]},"rhs":{"line_number":0,"changes":[{"start":0,"end":18,"content":"// コメント乙","highlight":"comment"}]}},{"lhs":{"line_number":2,"changes":[{"start":12,"end":32,"content":"\"日本語テスト\"","highlight":"string"}]},"rhs":{"line_number":2,"changes":[{"start":12,"end":29,"content":"\"日本語試験\"","highlight":"string"}]}}]],"language":"Rust","path":"cjk_new.rs","status":"changed"}"#;

    const CJK_OLD: &str = "// コメント甲\nfn main() {\n    let s = \"日本語テスト\";\n}\n";
    const CJK_NEW: &str = "// コメント乙\nfn main() {\n    let s = \"日本語試験\";\n}\n";

    #[test]
    fn cjk_rows_compose_with_overlay() {
        let (old_t, new_t) = (text(CJK_OLD), text(CJK_NEW));
        let overlay = normalize(&parse(CJK_CAPTURE).unwrap(), Some(&old_t), Some(&new_t));
        let rows = engine(LineDiffEngineId::Imara).diff(&line_tokens(&old_t), &line_tokens(&new_t));

        let mut saw_highlighted_removed = false;
        let mut saw_highlighted_added = false;
        for row in rows {
            let composed = compose_row(row, &old_t, &new_t, Some(&overlay));
            // Segments always reassemble the exact body text.
            let line = match composed.kind {
                RowKind::Removed => old_t.line_body_str(composed.old_line.unwrap()).unwrap(),
                _ => new_t.line_body_str(composed.new_line.unwrap()).unwrap(),
            };
            assert_eq!(reassembled(&composed.segments), line);

            for seg in &composed.segments {
                if seg.highlight.is_some() {
                    match composed.kind {
                        RowKind::Removed => saw_highlighted_removed = true,
                        RowKind::Added => saw_highlighted_added = true,
                        RowKind::Context => {}
                    }
                }
            }
        }
        assert!(saw_highlighted_removed);
        assert!(saw_highlighted_added);
    }

    #[test]
    fn highlighted_segment_is_exactly_the_changed_token() {
        let (old_t, new_t) = (text(CJK_OLD), text(CJK_NEW));
        let overlay = normalize(&parse(CJK_CAPTURE).unwrap(), Some(&old_t), Some(&new_t));
        let composed = compose_row(
            DiffRow::Added { new: LineIndex(2) },
            &old_t,
            &new_t,
            Some(&overlay),
        );
        let highlighted: Vec<&str> = composed
            .segments
            .iter()
            .filter(|s| s.highlight.is_some())
            .map(|s| s.text)
            .collect();
        assert_eq!(highlighted, vec!["\"日本語試験\""]);
    }

    #[test]
    fn no_overlay_yields_single_plain_segment() {
        let t = text("plain line\n");
        let composed = compose_row(
            DiffRow::Context {
                old: LineIndex(0),
                new: LineIndex(0),
            },
            &t,
            &t,
            None,
        );
        assert_eq!(composed.segments.len(), 1);
        assert_eq!(composed.segments[0].text, "plain line");
        assert!(composed.segments[0].highlight.is_none());
    }

    #[test]
    fn empty_line_yields_one_empty_segment() {
        let t = text("\nnext\n");
        let composed = compose_row(
            DiffRow::Context {
                old: LineIndex(0),
                new: LineIndex(0),
            },
            &t,
            &t,
            None,
        );
        assert_eq!(composed.segments.len(), 1);
        assert_eq!(composed.segments[0].text, "");
    }
}
