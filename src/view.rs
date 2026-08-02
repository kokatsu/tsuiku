//! Builds the unified diff rows visible in the terminal body.
//!
//! Here, a viewport means the scrollable body area between the title and
//! footer. The complete diff remains a compact `DiffRow` table; only rows that
//! fit in the viewport are converted to ratatui `Line` and `Span` values.

use crate::compose::{RowKind as ComposedRowKind, RowOverlays, compose_row};
use crate::linediff::DiffRow;
use crate::structural::normalize::HighlightKind;
use crate::syntax::SyntaxFg;
use crate::text::TextContent;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::fmt::Write;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RowKind {
    /// The same text exists on both sides.
    Context,
    /// Text exists only on the old side.
    Removed,
    /// Text exists only on the new side.
    Added,
}

/// One display row borrowed from the underlying old or new text.
#[derive(Clone, Copy, Debug)]
pub struct VisibleRow<'a> {
    /// Visual category used to choose the row marker and color.
    pub kind: RowKind,
    /// One-based old-file line number, or `None` for an inserted line.
    pub old_line: Option<usize>,
    /// One-based new-file line number, or `None` for a removed line.
    pub new_line: Option<usize>,
    /// Line text borrowed from the loaded content.
    pub text: &'a str,
}

/// Iterates over at most `height` display rows beginning at scroll `offset`.
///
/// This performs no line matching; it only maps an already computed diff row
/// table back to borrowed source text.
pub fn visible_rows<'a>(
    rows: &'a [DiffRow],
    old: &'a TextContent,
    new: &'a TextContent,
    offset: usize,
    height: usize,
) -> impl Iterator<Item = VisibleRow<'a>> + 'a {
    rows.iter()
        .skip(offset)
        .take(height)
        .map(move |row| match *row {
            DiffRow::Context { old: o, new: n } => VisibleRow {
                kind: RowKind::Context,
                old_line: Some(o.0 + 1),
                new_line: Some(n.0 + 1),
                text: new.line_body_str(n).expect("diff row references new line"),
            },
            DiffRow::Removed { old: o } => VisibleRow {
                kind: RowKind::Removed,
                old_line: Some(o.0 + 1),
                new_line: None,
                text: old.line_body_str(o).expect("diff row references old line"),
            },
            DiffRow::Added { new: n } => VisibleRow {
                kind: RowKind::Added,
                old_line: None,
                new_line: Some(n.0 + 1),
                text: new.line_body_str(n).expect("diff row references new line"),
            },
        })
}

/// Build exactly the `Line`/`Span` objects handed to ratatui for one
/// viewport. Source text remains borrowed; only the line-number prefix is
/// allocated per visible row.
pub fn build_unified_lines<'a>(
    rows: &'a [DiffRow],
    old: &'a TextContent,
    new: &'a TextContent,
    offset: usize,
    height: usize,
) -> Vec<Line<'a>> {
    build_unified_lines_with_overlay(rows, old, new, RowOverlays::default(), offset, height)
}

/// Build a viewport while applying validated structural and syntax spans.
/// The line-diff row remains the sole source of layout; overlays only
/// decorate text.
pub fn build_unified_lines_with_overlay<'a>(
    rows: &'a [DiffRow],
    old: &'a TextContent,
    new: &'a TextContent,
    overlays: RowOverlays<'a>,
    offset: usize,
    height: usize,
) -> Vec<Line<'a>> {
    let number_width = decimal_width(old.lines.len().max(new.lines.len())).max(5);
    rows.iter()
        .skip(offset)
        .take(height)
        .copied()
        .map(|row| {
            let row = compose_row(row, old, new, overlays);
            let (marker, line_style) = match row.kind {
                ComposedRowKind::Context => (' ', Style::default()),
                ComposedRowKind::Removed => ('-', Style::default().bg(Color::Rgb(60, 20, 25))),
                ComposedRowKind::Added => ('+', Style::default().bg(Color::Rgb(15, 55, 35))),
            };
            let mut prefix = String::with_capacity(number_width * 2 + 5);
            push_number(
                &mut prefix,
                row.old_line.map(|line| line.0 + 1),
                number_width,
            );
            prefix.push(' ');
            push_number(
                &mut prefix,
                row.new_line.map(|line| line.0 + 1),
                number_width,
            );
            let _ = write!(prefix, " {marker} ");
            let mut spans = Vec::with_capacity(row.segments.len() + 1);
            spans.push(Span::styled(prefix, line_style));
            spans.extend(row.segments.into_iter().map(|segment| {
                Span::styled(
                    segment.text,
                    segment_style(line_style, segment.structural, segment.syntax),
                )
            }));
            Line::from(spans)
        })
        .collect()
}

/// The two columns of one split viewport, ready for two paragraphs.
pub struct SplitLines<'a> {
    pub left: Vec<Line<'a>>,
    pub right: Vec<Line<'a>>,
}

/// Build one split viewport from the same row table the unified view uses.
///
/// The line diff already carries the row correspondence, so this is pure
/// view work: context rows appear on both sides, removed rows on the left
/// with a blank right, added rows on the right with a blank left. Each side
/// composes its *own* text and spans — structural lhs/rhs spans are per-side
/// decorations by contract, and a context row's old side may carry different
/// syntax spans than its new side.
pub fn build_split_lines<'a>(
    rows: &'a [DiffRow],
    old: &'a TextContent,
    new: &'a TextContent,
    overlays: RowOverlays<'a>,
    offset: usize,
    height: usize,
) -> SplitLines<'a> {
    let number_width = decimal_width(old.lines.len().max(new.lines.len())).max(5);
    let mut left = Vec::with_capacity(height.min(rows.len()));
    let mut right = Vec::with_capacity(height.min(rows.len()));
    for row in rows.iter().skip(offset).take(height).copied() {
        let (left_line, right_line) = match row {
            DiffRow::Context { old: o, new: n } => (
                Some((o, ' ', Style::default())),
                Some((n, ' ', Style::default())),
            ),
            DiffRow::Removed { old: o } => (
                Some((o, '-', Style::default().bg(Color::Rgb(60, 20, 25)))),
                None,
            ),
            DiffRow::Added { new: n } => (
                None,
                Some((n, '+', Style::default().bg(Color::Rgb(15, 55, 35)))),
            ),
        };
        left.push(side_line(
            left_line,
            old,
            overlays.structural.map(|ov| &ov.old),
            overlays.syntax_old,
            number_width,
        ));
        right.push(side_line(
            right_line,
            new,
            overlays.structural.map(|ov| &ov.new),
            overlays.syntax_new,
            number_width,
        ));
    }
    SplitLines { left, right }
}

/// One column cell: a numbered, decorated source line or a blank filler.
fn side_line<'a>(
    cell: Option<(crate::coords::LineIndex, char, Style)>,
    text: &'a TextContent,
    structural: Option<&'a crate::structural::normalize::SideOverlay>,
    syntax: Option<&'a crate::syntax::SyntaxSpans>,
    number_width: usize,
) -> Line<'a> {
    let Some((line, marker, line_style)) = cell else {
        return Line::from("");
    };
    let structural_spans = structural
        .map(|side| side.spans_for_line(line))
        .unwrap_or(&[]);
    let syntax_spans = syntax.map(|side| side.spans_for_line(line)).unwrap_or(&[]);
    let segments = crate::compose::segment_line(text, line, structural_spans, syntax_spans);
    let mut prefix = String::with_capacity(number_width + 3);
    push_number(&mut prefix, Some(line.0 + 1), number_width);
    let _ = write!(prefix, " {marker} ");
    let mut spans = Vec::with_capacity(segments.len() + 1);
    spans.push(Span::styled(prefix, line_style));
    spans.extend(segments.into_iter().map(|segment| {
        Span::styled(
            segment.text,
            segment_style(line_style, segment.structural, segment.syntax),
        )
    }));
    Line::from(spans)
}

/// Style composition: an explicit structural foreground beats the syntax
/// foreground, which beats the line-diff default; a structural background
/// beats the line background; syntax never touches background or attributes.
fn segment_style(
    base: Style,
    structural: Option<HighlightKind>,
    syntax: Option<SyntaxFg>,
) -> Style {
    if let Some(highlight) = structural {
        let foreground = match highlight {
            HighlightKind::Keyword => Color::LightMagenta,
            HighlightKind::String => Color::LightYellow,
            HighlightKind::Comment => Color::Gray,
            HighlightKind::Delimiter => Color::LightCyan,
            HighlightKind::TypeName => Color::LightBlue,
            HighlightKind::Normal | HighlightKind::Other => Color::White,
        };
        return base
            .fg(foreground)
            .bg(Color::Rgb(85, 65, 15))
            .add_modifier(Modifier::BOLD);
    }
    match syntax {
        Some(fg) => base.fg(Color::Rgb(fg.r, fg.g, fg.b)),
        None => base,
    }
}

fn decimal_width(value: usize) -> usize {
    value.max(1).ilog10() as usize + 1
}

fn push_number(output: &mut String, number: Option<usize>, width: usize) {
    match number {
        Some(number) => {
            let _ = write!(output, "{number:>width$}");
        }
        None => output.extend(std::iter::repeat_n(' ', width)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coords::LineIndex;
    use crate::text::{ClassifiedContent, classify};
    use std::sync::Arc;

    fn text(s: &str) -> TextContent {
        match classify(Arc::from(s.as_bytes())) {
            ClassifiedContent::Text(text) => text,
            ClassifiedContent::Binary(_) => panic!("text fixture"),
        }
    }

    #[test]
    fn materializes_only_the_viewport() {
        let old = text("a\nb\nc\n");
        let new = text("a\nB\nc\n");
        let rows = [
            DiffRow::Context {
                old: LineIndex(0),
                new: LineIndex(0),
            },
            DiffRow::Removed { old: LineIndex(1) },
            DiffRow::Added { new: LineIndex(1) },
            DiffRow::Context {
                old: LineIndex(2),
                new: LineIndex(2),
            },
        ];
        let visible: Vec<_> = visible_rows(&rows, &old, &new, 1, 2).collect();
        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0].kind, RowKind::Removed);
        assert_eq!(visible[0].text, "b");
        assert_eq!(visible[1].kind, RowKind::Added);
        assert_eq!(visible[1].text, "B");
    }

    #[test]
    fn unified_lines_keep_six_digit_columns_aligned() {
        let old = text("a\n");
        let new = text("b\n");
        let rows = [DiffRow::Added {
            new: LineIndex(999_999),
        }];
        // Use synthetic line counts without allocating a million strings.
        let mut large_new = new;
        let record = large_new.lines[0];
        large_new.lines.resize(1_000_000, record);
        let lines = build_unified_lines(&rows, &old, &large_new, 0, 1);
        assert_eq!(lines[0].spans[0].content, "        1000000 + ");
    }

    #[test]
    fn structural_span_overrides_the_line_background() {
        use crate::structural::json::parse;
        use crate::structural::normalize::normalize;

        let old = text("let old = 1;\n");
        let new = text("let new = 1;\n");
        let raw = parse(
            r#"{"language":"Rust","path":"x.rs","status":"changed","chunks":[[{"rhs":{"line_number":0,"changes":[{"start":4,"end":7,"content":"new","highlight":"normal"}]}}]]}"#,
        )
        .unwrap();
        let overlay = normalize(&raw, Some(&old), Some(&new));
        let rows = [DiffRow::Added {
            new: crate::coords::LineIndex(0),
        }];

        let lines = build_unified_lines_with_overlay(
            &rows,
            &old,
            &new,
            RowOverlays {
                structural: Some(&overlay),
                ..RowOverlays::default()
            },
            0,
            1,
        );

        assert_eq!(lines[0].spans.len(), 4);
        assert_eq!(lines[0].spans[2].content, "new");
        assert_eq!(lines[0].spans[2].style.bg, Some(Color::Rgb(85, 65, 15)));
        assert!(
            lines[0].spans[2]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(lines[0].spans[1].style.bg, Some(Color::Rgb(15, 55, 35)));
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn split_rows_assign_sides_and_blank_fillers() {
        let old = text("shared\nremoved\n");
        let new = text("shared\nadded\n");
        let rows = [
            DiffRow::Context {
                old: LineIndex(0),
                new: LineIndex(0),
            },
            DiffRow::Removed { old: LineIndex(1) },
            DiffRow::Added { new: LineIndex(1) },
        ];

        let split = build_split_lines(&rows, &old, &new, RowOverlays::default(), 0, 10);

        assert_eq!(split.left.len(), 3);
        assert_eq!(split.right.len(), 3);
        // Context appears on both sides with its own side's line number.
        assert_eq!(line_text(&split.left[0]), "    1   shared");
        assert_eq!(line_text(&split.right[0]), "    1   shared");
        // Removed: left only, right blank.
        assert_eq!(line_text(&split.left[1]), "    2 - removed");
        assert_eq!(line_text(&split.right[1]), "");
        // Added: right only, left blank.
        assert_eq!(line_text(&split.left[2]), "");
        assert_eq!(line_text(&split.right[2]), "    2 + added");
    }

    #[test]
    fn split_viewport_respects_offset_and_height() {
        let old = text("a\nb\nc\nd\n");
        let new = text("a\nb\nc\nd\n");
        let rows: Vec<DiffRow> = (0..4)
            .map(|index| DiffRow::Context {
                old: LineIndex(index),
                new: LineIndex(index),
            })
            .collect();
        let split = build_split_lines(&rows, &old, &new, RowOverlays::default(), 1, 2);
        assert_eq!(split.left.len(), 2);
        assert_eq!(line_text(&split.left[0]), "    2   b");
        assert_eq!(line_text(&split.right[1]), "    3   c");
    }

    #[test]
    fn split_applies_each_overlay_to_its_own_side_only() {
        use crate::structural::json::parse;
        use crate::structural::normalize::normalize;

        let old = text("let value = old;\n");
        let new = text("let value = new;\n");
        // lhs marks "old" on the old side, rhs marks "new" on the new side.
        let raw = parse(
            r#"{"language":"Rust","path":"x.rs","status":"changed","chunks":[[{"lhs":{"line_number":0,"changes":[{"start":12,"end":15,"content":"old","highlight":"normal"}]},"rhs":{"line_number":0,"changes":[{"start":12,"end":15,"content":"new","highlight":"normal"}]}}]]}"#,
        )
        .unwrap();
        let overlay = normalize(&raw, Some(&old), Some(&new));
        let rows = [
            DiffRow::Removed { old: LineIndex(0) },
            DiffRow::Added { new: LineIndex(0) },
        ];

        let split = build_split_lines(
            &rows,
            &old,
            &new,
            RowOverlays {
                structural: Some(&overlay),
                ..RowOverlays::default()
            },
            0,
            10,
        );

        let structural_bg = Some(Color::Rgb(85, 65, 15));
        let left_highlighted: Vec<&str> = split.left[0]
            .spans
            .iter()
            .filter(|span| span.style.bg == structural_bg)
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(left_highlighted, vec!["old"]);
        let right_highlighted: Vec<&str> = split.right[1]
            .spans
            .iter()
            .filter(|span| span.style.bg == structural_bg)
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(right_highlighted, vec!["new"]);
    }

    #[test]
    fn split_context_rows_use_per_side_syntax_spans() {
        use crate::structural::tempfiles::LanguagePathHint;
        use crate::syntax::{DEFAULT_THEME, HighlightAssets, HighlightOutcome};

        // Identical context text, but each side gets its own span set; the
        // left column must consult the old side, the right the new side.
        let source = "let shared = \"text\";\n";
        let (old, new) = (text(source), text(source));
        let hint = LanguagePathHint {
            extension: Some(b"rs".to_vec()),
            basename: Some(b"a.rs".to_vec()),
        };
        let spans = match HighlightAssets::load().highlight(&old, &hint, DEFAULT_THEME) {
            HighlightOutcome::Ready(spans) => spans,
            _ => panic!("rust fixture must highlight"),
        };
        let rows = [DiffRow::Context {
            old: LineIndex(0),
            new: LineIndex(0),
        }];

        // Old side highlighted, new side not: only the left shows colors.
        let split = build_split_lines(
            &rows,
            &old,
            &new,
            RowOverlays {
                structural: None,
                syntax_old: Some(&spans),
                syntax_new: None,
            },
            0,
            10,
        );
        let has_rgb = |line: &Line<'_>| {
            line.spans
                .iter()
                .any(|span| matches!(span.style.fg, Some(Color::Rgb(..))))
        };
        assert!(has_rgb(&split.left[0]));
        assert!(!has_rgb(&split.right[0]));
    }

    #[test]
    fn composition_priority_follows_the_style_contract() {
        let base = Style::default().bg(Color::Rgb(15, 55, 35));
        let syntax = SyntaxFg {
            r: 10,
            g: 20,
            b: 30,
        };

        // Syntax alone: foreground only, background and attributes untouched.
        let syntax_only = segment_style(base, None, Some(syntax));
        assert_eq!(syntax_only.fg, Some(Color::Rgb(10, 20, 30)));
        assert_eq!(syntax_only.bg, base.bg);
        assert_eq!(syntax_only.add_modifier, Modifier::empty());

        // Structural beats syntax on foreground and the line bg on background.
        let both = segment_style(base, Some(HighlightKind::String), Some(syntax));
        assert_eq!(both.fg, Some(Color::LightYellow));
        assert_eq!(both.bg, Some(Color::Rgb(85, 65, 15)));
        assert!(both.add_modifier.contains(Modifier::BOLD));

        // Neither layer: the line-diff style passes through unchanged.
        assert_eq!(segment_style(base, None, None), base);
    }
}
