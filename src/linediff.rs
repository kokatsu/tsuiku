//! Line-level diff (the layout layer).
//!
//! This layer is the only authority on how old and new lines align on
//! screen; structural overlays only decorate lines it has placed. Tokens are
//! the raw line bytes including their terminators — no normalization, so a
//! CRLF→LF rewrite is a real change here, exactly as git sees it.
//!
//! Two engine implementations exist behind one trait so they can be compared
//! on the same inputs; `examples/engine_bench.rs` is the comparison harness.

use imara_diff::{Algorithm, Diff, InternedInput, TokenSource};

use crate::asyncstate::LineDiffEngineId;
use crate::coords::LineIndex;
use crate::text::TextContent;

/// One display row of the diff, referring to source lines by index.
/// Rows never carry text; rendering slices the original buffers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DiffRow {
    Context { old: LineIndex, new: LineIndex },
    Removed { old: LineIndex },
    Added { new: LineIndex },
}

/// Row offsets at which a contiguous block of changed rows begins.
///
/// Context rows separate hunks. A removal immediately followed by additions
/// is one replacement hunk, not two navigation targets.
pub fn hunk_starts(rows: &[DiffRow]) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .filter_map(|(index, row)| {
            let changed = !matches!(row, DiffRow::Context { .. });
            let previous_was_context =
                index == 0 || matches!(rows.get(index - 1), Some(DiffRow::Context { .. }));
            (changed && previous_was_context).then_some(index)
        })
        .collect()
}

pub trait LineDiffEngine {
    fn id(&self) -> LineDiffEngineId;
    /// Diff two tokenized files. Each token is one raw line (terminator
    /// included).
    fn diff(&self, old: &[&[u8]], new: &[&[u8]]) -> Vec<DiffRow>;
}

/// Raw line tokens of a text file, terminators included.
pub fn line_tokens(text: &TextContent) -> Vec<&[u8]> {
    text.lines
        .iter()
        .map(|rec| rec.full_range.slice(&text.bytes))
        .collect()
}

/// The engine used in production: imara-diff won the bench comparison
/// (`examples/engine_bench.rs`) and its indent-heuristic postprocessing
/// places hunks the way git does. `Similar` remains as a comparison oracle.
pub const DEFAULT_ENGINE: LineDiffEngineId = LineDiffEngineId::Imara;

pub fn engine(id: LineDiffEngineId) -> Box<dyn LineDiffEngine> {
    match id {
        LineDiffEngineId::Imara => Box::new(ImaraEngine),
        LineDiffEngineId::Similar => Box::new(SimilarEngine),
    }
}

// --- imara-diff ---

struct SliceLines<'a>(&'a [&'a [u8]]);

impl<'a> TokenSource for SliceLines<'a> {
    type Token = &'a [u8];
    type Tokenizer = std::iter::Copied<std::slice::Iter<'a, &'a [u8]>>;

    fn tokenize(&self) -> Self::Tokenizer {
        self.0.iter().copied()
    }

    fn estimate_tokens(&self) -> u32 {
        self.0.len() as u32
    }
}

pub struct ImaraEngine;

impl LineDiffEngine for ImaraEngine {
    fn id(&self) -> LineDiffEngineId {
        LineDiffEngineId::Imara
    }

    fn diff(&self, old: &[&[u8]], new: &[&[u8]]) -> Vec<DiffRow> {
        let input = InternedInput::new(SliceLines(old), SliceLines(new));
        // Myers, not Histogram: measured 20-30x faster on files where most
        // lines are unique (the common case), and postprocess_lines provides
        // the git-like hunk placement either way.
        let mut diff = Diff::compute(Algorithm::Myers, &input);
        diff.postprocess_lines(&input);

        let mut rows = Vec::with_capacity(old.len().max(new.len()));
        let mut old_pos = 0usize;
        let mut new_pos = 0usize;
        for hunk in diff.hunks() {
            let (before, after) = (hunk.before, hunk.after);
            while old_pos < before.start as usize {
                rows.push(DiffRow::Context {
                    old: LineIndex(old_pos),
                    new: LineIndex(new_pos),
                });
                old_pos += 1;
                new_pos += 1;
            }
            for i in before.start..before.end {
                rows.push(DiffRow::Removed {
                    old: LineIndex(i as usize),
                });
            }
            for i in after.start..after.end {
                rows.push(DiffRow::Added {
                    new: LineIndex(i as usize),
                });
            }
            old_pos = before.end as usize;
            new_pos = after.end as usize;
        }
        while old_pos < old.len() {
            rows.push(DiffRow::Context {
                old: LineIndex(old_pos),
                new: LineIndex(new_pos),
            });
            old_pos += 1;
            new_pos += 1;
        }
        rows
    }
}

// --- similar ---

pub struct SimilarEngine;

impl LineDiffEngine for SimilarEngine {
    fn id(&self) -> LineDiffEngineId {
        LineDiffEngineId::Similar
    }

    fn diff(&self, old: &[&[u8]], new: &[&[u8]]) -> Vec<DiffRow> {
        let ops = similar::capture_diff_slices(similar::Algorithm::Myers, old, new);
        let mut rows = Vec::with_capacity(old.len().max(new.len()));
        for op in ops {
            match op {
                similar::DiffOp::Equal {
                    old_index,
                    new_index,
                    len,
                } => {
                    for i in 0..len {
                        rows.push(DiffRow::Context {
                            old: LineIndex(old_index + i),
                            new: LineIndex(new_index + i),
                        });
                    }
                }
                similar::DiffOp::Delete {
                    old_index, old_len, ..
                } => {
                    for i in 0..old_len {
                        rows.push(DiffRow::Removed {
                            old: LineIndex(old_index + i),
                        });
                    }
                }
                similar::DiffOp::Insert {
                    new_index, new_len, ..
                } => {
                    for i in 0..new_len {
                        rows.push(DiffRow::Added {
                            new: LineIndex(new_index + i),
                        });
                    }
                }
                similar::DiffOp::Replace {
                    old_index,
                    old_len,
                    new_index,
                    new_len,
                } => {
                    for i in 0..old_len {
                        rows.push(DiffRow::Removed {
                            old: LineIndex(old_index + i),
                        });
                    }
                    for i in 0..new_len {
                        rows.push(DiffRow::Added {
                            new: LineIndex(new_index + i),
                        });
                    }
                }
            }
        }
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::{ClassifiedContent, classify};
    use std::sync::Arc;

    fn text(s: &str) -> TextContent {
        match classify(Arc::from(s.as_bytes())) {
            ClassifiedContent::Text(t) => t,
            ClassifiedContent::Binary(_) => panic!("fixture must be text"),
        }
    }

    /// Any valid row list must visit every old line exactly once (as Context
    /// or Removed, in order) and every new line exactly once (as Context or
    /// Added, in order), and Context rows must pair identical lines.
    fn assert_rows_valid(rows: &[DiffRow], old: &[&[u8]], new: &[&[u8]]) {
        let mut old_seen = 0usize;
        let mut new_seen = 0usize;
        for row in rows {
            match *row {
                DiffRow::Context { old: o, new: n } => {
                    assert_eq!(o.0, old_seen);
                    assert_eq!(n.0, new_seen);
                    assert_eq!(old[o.0], new[n.0], "context rows must pair equal lines");
                    old_seen += 1;
                    new_seen += 1;
                }
                DiffRow::Removed { old: o } => {
                    assert_eq!(o.0, old_seen);
                    old_seen += 1;
                }
                DiffRow::Added { new: n } => {
                    assert_eq!(n.0, new_seen);
                    new_seen += 1;
                }
            }
        }
        assert_eq!(old_seen, old.len());
        assert_eq!(new_seen, new.len());
    }

    fn check_both_engines(old_src: &str, new_src: &str) {
        let (old_t, new_t) = (text(old_src), text(new_src));
        let (old, new) = (line_tokens(&old_t), line_tokens(&new_t));
        for id in [LineDiffEngineId::Imara, LineDiffEngineId::Similar] {
            let rows = engine(id).diff(&old, &new);
            assert_rows_valid(&rows, &old, &new);
        }
    }

    #[test]
    fn modify_middle_line() {
        check_both_engines("a\nb\nc\n", "a\nB\nc\n");
    }

    #[test]
    fn add_and_remove() {
        check_both_engines("a\nb\n", "b\nc\nd\n");
    }

    #[test]
    fn empty_to_content_and_back() {
        check_both_engines("", "a\nb\n");
        check_both_engines("a\nb\n", "");
    }

    #[test]
    fn identical_files_are_all_context() {
        let t = text("x\ny\n");
        let tokens = line_tokens(&t);
        for id in [LineDiffEngineId::Imara, LineDiffEngineId::Similar] {
            let rows = engine(id).diff(&tokens, &tokens);
            assert!(rows.iter().all(|r| matches!(r, DiffRow::Context { .. })));
            assert_eq!(rows.len(), 2);
        }
    }

    #[test]
    fn crlf_rewrite_is_a_real_change() {
        // Raw bytes are the token: LF→CRLF must show as changed lines.
        let (old_t, new_t) = (text("a\nb\n"), text("a\r\nb\r\n"));
        let (old, new) = (line_tokens(&old_t), line_tokens(&new_t));
        for id in [LineDiffEngineId::Imara, LineDiffEngineId::Similar] {
            let rows = engine(id).diff(&old, &new);
            assert!(
                rows.iter().all(|r| !matches!(r, DiffRow::Context { .. })),
                "no line may count as unchanged"
            );
        }
    }

    #[test]
    fn missing_trailing_newline_differs() {
        check_both_engines("a\nb", "a\nb\n");
    }

    #[test]
    fn hunk_index_groups_adjacent_removals_and_additions() {
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
            DiffRow::Added { new: LineIndex(3) },
            DiffRow::Added { new: LineIndex(4) },
        ];
        assert_eq!(hunk_starts(&rows), vec![1, 4]);
    }

    #[test]
    fn hunk_index_handles_edges_and_clean_inputs() {
        assert_eq!(
            hunk_starts(&[
                DiffRow::Removed { old: LineIndex(0) },
                DiffRow::Context {
                    old: LineIndex(1),
                    new: LineIndex(0),
                },
                DiffRow::Removed { old: LineIndex(2) },
            ]),
            vec![0, 2]
        );
        assert!(hunk_starts(&[]).is_empty());
        assert!(
            hunk_starts(&[DiffRow::Context {
                old: LineIndex(0),
                new: LineIndex(0),
            }])
            .is_empty()
        );
    }
}
