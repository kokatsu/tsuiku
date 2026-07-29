//! Builds the unified diff rows visible in the terminal body.
//!
//! Here, a viewport means the scrollable body area between the title and
//! footer. The complete diff remains a compact `DiffRow` table; only rows that
//! fit in the viewport are converted to ratatui `Line` and `Span` values.

use crate::linediff::DiffRow;
use crate::text::TextContent;
use ratatui::style::{Color, Style};
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
    let number_width = decimal_width(old.lines.len().max(new.lines.len())).max(5);
    visible_rows(rows, old, new, offset, height)
        .map(|row| {
            let (marker, style) = match row.kind {
                RowKind::Context => (' ', Style::default()),
                RowKind::Removed => ('-', Style::default().bg(Color::Rgb(60, 20, 25))),
                RowKind::Added => ('+', Style::default().bg(Color::Rgb(15, 55, 35))),
            };
            let mut prefix = String::with_capacity(number_width * 2 + 5);
            push_number(&mut prefix, row.old_line, number_width);
            prefix.push(' ');
            push_number(&mut prefix, row.new_line, number_width);
            let _ = write!(prefix, " {marker} ");
            Line::from(vec![
                Span::styled(prefix, style),
                Span::styled(row.text, style),
            ])
        })
        .collect()
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
}
