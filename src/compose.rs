//! Style composition: line-diff rows decorated with structural and syntax
//! spans.
//!
//! The line-diff layer decides layout (which line appears on which row and
//! whether it is context/removed/added); the structural and syntax layers
//! only split a row's text into styled and plain segments. When no overlay
//! data exists for a line the row renders unhighlighted — layers compose,
//! they never replace each other.
//!
//! Each segment carries the two layers separately: the structural layer may
//! set foreground, background and attributes; the syntax layer supplies
//! foreground only, and loses to an explicit structural foreground. The
//! final priority is resolved at render time.
//!
//! Removed rows read the old side, added rows the new side; context rows
//! read the new side (both sides hold identical text there).

use crate::coords::LineIndex;
use crate::linediff::DiffRow;
use crate::structural::normalize::{HighlightKind, LineSpan, StructuralOverlay};
use crate::syntax::{SyntaxFg, SyntaxLineSpan, SyntaxSpans};
use crate::text::TextContent;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RowKind {
    Context,
    Removed,
    Added,
}

/// One contiguous run of a line body with a single style per layer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Segment<'a> {
    pub text: &'a str,
    /// `Some` when a structural span covers this run.
    pub structural: Option<HighlightKind>,
    /// `Some` when a syntax span covers this run.
    pub syntax: Option<SyntaxFg>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ComposedRow<'a> {
    pub kind: RowKind,
    pub old_line: Option<LineIndex>,
    pub new_line: Option<LineIndex>,
    pub segments: Vec<Segment<'a>>,
}

/// The overlays a viewport build applies on top of the line diff. Any part
/// may be absent; missing data only means fewer decorations.
#[derive(Clone, Copy, Default)]
pub struct RowOverlays<'a> {
    pub structural: Option<&'a StructuralOverlay>,
    pub syntax_old: Option<&'a SyntaxSpans>,
    pub syntax_new: Option<&'a SyntaxSpans>,
}

/// Split one line body into segments at the union of both layers' span
/// boundaries. Both span lists must be the normalized (sorted, merged,
/// validated) spans of exactly this line.
pub fn segment_line<'a>(
    text: &'a TextContent,
    line: LineIndex,
    structural: &[LineSpan],
    syntax: &[SyntaxLineSpan],
) -> Vec<Segment<'a>> {
    let body = text
        .line_body_str(line)
        .expect("caller resolved the line from this text");
    if body.is_empty() {
        return vec![Segment {
            text: "",
            structural: None,
            syntax: None,
        }];
    }

    // Normalization guarantees in-bounds, char-aligned, sorted spans; clamp
    // anyway so a violated invariant degrades to plain text, never a panic.
    let mut cuts = Vec::with_capacity(structural.len() * 2 + syntax.len() * 2 + 2);
    cuts.push(0);
    cuts.push(body.len());
    for span in structural {
        debug_assert_eq!(span.line, line);
        cuts.push(span.range.start.min(body.len()));
        cuts.push(span.range.end.min(body.len()));
    }
    for span in syntax {
        debug_assert_eq!(span.line, line);
        cuts.push(span.range.start.min(body.len()));
        cuts.push(span.range.end.min(body.len()));
    }
    cuts.sort_unstable();
    cuts.dedup();
    cuts.retain(|&cut| body.is_char_boundary(cut));

    // Both span lists and the cut list are sorted, so one index per list
    // advances monotonically and composition stays linear in the span count.
    // A single permitted line can carry tens of thousands of spans (minified
    // sources), where a per-cut scan would stall the terminal thread.
    let mut structural_idx = 0usize;
    let mut syntax_idx = 0usize;
    cuts.windows(2)
        .map(|window| {
            let (start, end) = (window[0], window[1]);
            while structural
                .get(structural_idx)
                .is_some_and(|span| span.range.end <= start)
            {
                structural_idx += 1;
            }
            while syntax
                .get(syntax_idx)
                .is_some_and(|span| span.range.end <= start)
            {
                syntax_idx += 1;
            }
            Segment {
                text: &body[start..end],
                structural: structural
                    .get(structural_idx)
                    .filter(|span| span.range.start <= start)
                    .map(|span| span.kind),
                syntax: syntax
                    .get(syntax_idx)
                    .filter(|span| span.range.start <= start)
                    .map(|span| span.fg),
            }
        })
        .collect()
}

/// Compose one display row from the line diff and the overlays.
pub fn compose_row<'a>(
    row: DiffRow,
    old_text: &'a TextContent,
    new_text: &'a TextContent,
    overlays: RowOverlays<'a>,
) -> ComposedRow<'a> {
    let (kind, old_line, new_line, text, side_line) = match row {
        DiffRow::Context { old, new } => (RowKind::Context, Some(old), Some(new), new_text, new),
        DiffRow::Removed { old } => (RowKind::Removed, Some(old), None, old_text, old),
        DiffRow::Added { new } => (RowKind::Added, None, Some(new), new_text, new),
    };
    let structural = overlays
        .structural
        .map(|ov| match kind {
            RowKind::Removed => ov.old.spans_for_line(side_line),
            _ => ov.new.spans_for_line(side_line),
        })
        .unwrap_or(&[]);
    let syntax = match kind {
        RowKind::Removed => overlays.syntax_old,
        _ => overlays.syntax_new,
    }
    .map(|spans| spans.spans_for_line(side_line))
    .unwrap_or(&[]);
    ComposedRow {
        kind,
        old_line,
        new_line,
        segments: segment_line(text, side_line, structural, syntax),
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

    fn structural_only(overlay: &StructuralOverlay) -> RowOverlays<'_> {
        RowOverlays {
            structural: Some(overlay),
            ..RowOverlays::default()
        }
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
            let composed = compose_row(row, &old_t, &new_t, structural_only(&overlay));
            // Segments always reassemble the exact body text.
            let line = match composed.kind {
                RowKind::Removed => old_t.line_body_str(composed.old_line.unwrap()).unwrap(),
                _ => new_t.line_body_str(composed.new_line.unwrap()).unwrap(),
            };
            assert_eq!(reassembled(&composed.segments), line);

            for seg in &composed.segments {
                if seg.structural.is_some() {
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
            structural_only(&overlay),
        );
        let highlighted: Vec<&str> = composed
            .segments
            .iter()
            .filter(|s| s.structural.is_some())
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
            RowOverlays::default(),
        );
        assert_eq!(composed.segments.len(), 1);
        assert_eq!(composed.segments[0].text, "plain line");
        assert!(composed.segments[0].structural.is_none());
        assert!(composed.segments[0].syntax.is_none());
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
            RowOverlays::default(),
        );
        assert_eq!(composed.segments.len(), 1);
        assert_eq!(composed.segments[0].text, "");
    }

    mod many_spans {
        use super::*;
        use crate::coords::LineByteRange;
        use crate::syntax::SyntaxFg;
        use std::time::Instant;

        /// One span per two-byte token on a single line, alternating colors
        /// so no adjacent spans merge — the shape a minified one-line file
        /// produces (which passes the 5,000-line / 2 MiB size guards).
        fn dense_line(tokens: usize) -> (TextContent, Vec<SyntaxLineSpan>) {
            let content = text(&"ab".repeat(tokens));
            let spans = (0..tokens)
                .map(|i| SyntaxLineSpan {
                    line: LineIndex(0),
                    range: LineByteRange::new(i * 2, i * 2 + 2),
                    fg: SyntaxFg {
                        r: (i % 2) as u8,
                        g: 0,
                        b: 0,
                    },
                })
                .collect();
            (content, spans)
        }

        /// The pre-optimization per-offset lookup, kept as the reference.
        fn naive_segments<'a>(
            text: &'a TextContent,
            structural: &[LineSpan],
            syntax: &[SyntaxLineSpan],
        ) -> Vec<Segment<'a>> {
            let fast = segment_line(text, LineIndex(0), structural, syntax);
            fast.iter()
                .scan(0usize, |cursor, segment| {
                    let start = *cursor;
                    *cursor += segment.text.len();
                    Some(Segment {
                        text: segment.text,
                        structural: structural
                            .iter()
                            .find(|s| s.range.start <= start && start < s.range.end)
                            .map(|s| s.kind),
                        syntax: syntax
                            .iter()
                            .find(|s| s.range.start <= start && start < s.range.end)
                            .map(|s| s.fg),
                    })
                })
                .collect()
        }

        #[test]
        fn segmentation_matches_the_naive_reference() {
            let (content, syntax) = dense_line(500);
            // A structural span overlapping part of the line exercises both
            // pointers together.
            let structural = vec![
                LineSpan {
                    line: LineIndex(0),
                    range: LineByteRange::new(10, 25),
                    kind: HighlightKind::Keyword,
                },
                LineSpan {
                    line: LineIndex(0),
                    range: LineByteRange::new(600, 640),
                    kind: HighlightKind::String,
                },
            ];
            let fast = segment_line(&content, LineIndex(0), &structural, &syntax);
            assert_eq!(fast, naive_segments(&content, &structural, &syntax));
            assert_eq!(
                reassembled(&fast),
                content.line_body_str(LineIndex(0)).unwrap()
            );
        }

        #[test]
        fn composition_is_linear_in_the_span_count() {
            let (content, syntax) = dense_line(100_000);
            let start = Instant::now();
            let segments = segment_line(&content, LineIndex(0), &[], &syntax);
            let elapsed = start.elapsed();
            assert_eq!(segments.len(), 100_000);
            // Generous even for debug builds and noisy CI; the quadratic
            // lookup this guards against took whole seconds here.
            assert!(
                elapsed.as_millis() < 2_000,
                "one dense line took {elapsed:?} to segment"
            );
        }
    }

    mod syntax_layer {
        use super::*;
        use crate::structural::tempfiles::LanguagePathHint;
        use crate::syntax::{DEFAULT_THEME, HighlightAssets, HighlightOutcome};

        fn rust_spans(source: &str) -> Arc<SyntaxSpans> {
            let hint = LanguagePathHint {
                extension: Some(b"rs".to_vec()),
                basename: Some(b"a.rs".to_vec()),
            };
            match HighlightAssets::load().highlight(&text(source), &hint, DEFAULT_THEME) {
                HighlightOutcome::Ready(spans) => spans,
                _ => panic!("rust fixture must highlight"),
            }
        }

        #[test]
        fn syntax_segments_reassemble_losslessly() {
            let source = "fn main() {\n    let s = \"日本語\"; // コメント\n}\n";
            let t = text(source);
            let spans = rust_spans(source);
            for index in 0..t.lines.len() {
                let line = LineIndex(index);
                let segments = segment_line(&t, line, &[], spans.spans_for_line(line));
                assert_eq!(
                    reassembled(&segments),
                    t.line_body_str(line).unwrap(),
                    "line {index} must reassemble exactly"
                );
            }
        }

        #[test]
        fn structural_and_syntax_overlap_keeps_both_layers() {
            let source = "let s = \"value\";\n";
            let t = text(source);
            let spans = rust_spans(source);
            // A structural span over the string literal, overlapping the
            // syntax span for the same token.
            let raw = parse(
                r#"{"language":"Rust","path":"a.rs","status":"changed","chunks":[[{"rhs":{"line_number":0,"changes":[{"start":8,"end":15,"content":"\"value\"","highlight":"string"}]}}]]}"#,
            )
            .unwrap();
            let overlay = normalize(&raw, None, Some(&t));

            let composed = compose_row(
                DiffRow::Added { new: LineIndex(0) },
                &t,
                &t,
                RowOverlays {
                    structural: Some(&overlay),
                    syntax_old: None,
                    syntax_new: Some(&spans),
                },
            );

            assert_eq!(reassembled(&composed.segments), "let s = \"value\";");
            // The overlapping run reports both layers so the renderer can
            // apply the composition priority; the keyword run reports
            // syntax only.
            assert!(
                composed
                    .segments
                    .iter()
                    .any(|seg| seg.structural.is_some() && seg.syntax.is_some())
            );
            assert!(composed.segments.iter().any(|seg| {
                seg.structural.is_none() && seg.syntax.is_some() && seg.text == "let"
            }));
        }
    }
}
