//! Live contract tests against a real difft binary.
//!
//! These verify that the JSON contract measured against difft 0.69.0
//! still holds for the installed difft: byte-offset coordinates, 0-based
//! line numbers, CR exclusion, one-sided entries, and content round-trips.
//! Skipped (with a note) when difft is not on PATH.

use std::sync::Arc;

use tsuiku::asyncstate::StructuralError;
use tsuiku::coords::LineIndex;
use tsuiku::path::GitPath;
use tsuiku::structural::normalize::{DifftStatus, normalize};
use tsuiku::structural::runner::DifftRunner;
use tsuiku::structural::tempfiles::{LanguagePathHint, materialize};
use tsuiku::text::{ClassifiedContent, TextContent, classify};

fn difft_or_skip() -> Option<DifftRunner> {
    let runner = DifftRunner::default();
    // One guarded call decides everything: skip only when the binary is
    // genuinely absent; any other launch problem (crash, timeout, oversized
    // output) is a real failure and must not silently pass as "not
    // installed".
    match runner.version() {
        Ok(v) => {
            eprintln!("testing against {v}");
            Some(runner)
        }
        Err(StructuralError::ToolNotFound) => {
            eprintln!("difft not found on PATH; skipping live contract test");
            None
        }
        Err(e) => panic!("difft is present but failed the version query: {e:?}"),
    }
}

fn text(bytes: &[u8]) -> TextContent {
    match classify(Arc::from(bytes)) {
        ClassifiedContent::Text(t) => t,
        ClassifiedContent::Binary(_) => panic!("fixture must be text"),
    }
}

fn hint(name: &[u8]) -> LanguagePathHint {
    LanguagePathHint::from_git_path(&GitPath::from_bytes(name))
}

fn run_pair(
    runner: &DifftRunner,
    name: &[u8],
    old_bytes: &[u8],
    new_bytes: &[u8],
) -> tsuiku::structural::normalize::StructuralOverlay {
    let pair = materialize(old_bytes, new_bytes, &hint(name), &hint(name)).unwrap();
    let raw = runner.run(&pair.old_path, &pair.new_path).unwrap();
    normalize(&raw, Some(&text(old_bytes)), Some(&text(new_bytes)))
}

#[test]
fn cjk_spans_survive_normalization_unrejected() {
    let Some(runner) = difft_or_skip() else {
        return;
    };
    let old = "// コメント甲\nfn main() {\n    let s = \"日本語テスト\";\n}\n";
    let new = "// コメント乙\nfn main() {\n    let s = \"日本語試験\";\n}\n";
    let overlay = run_pair(&runner, b"cjk.rs", old.as_bytes(), new.as_bytes());
    assert_eq!(overlay.status, DifftStatus::Changed);
    assert!(overlay.diagnostics.accepted > 0);
    assert_eq!(overlay.diagnostics.rejected_out_of_bounds, 0);
    assert_eq!(overlay.diagnostics.rejected_invalid_boundary, 0);
    assert_eq!(
        overlay.diagnostics.rejected_content_mismatch, 0,
        "content mismatch means the coordinate contract drifted"
    );
}

#[test]
fn emoji_and_tabs_validate() {
    let Some(runner) = difft_or_skip() else {
        return;
    };
    let old = "\tlet e = \"🎉x\";\n";
    let new = "\tlet e = \"🎉y\";\n";
    let overlay = run_pair(&runner, b"emoji.rs", old.as_bytes(), new.as_bytes());
    assert!(overlay.diagnostics.accepted > 0);
    assert_eq!(overlay.diagnostics.rejected_invalid_boundary, 0);
    assert_eq!(overlay.diagnostics.rejected_content_mismatch, 0);
}

#[test]
fn crlf_spans_fit_the_body() {
    let Some(runner) = difft_or_skip() else {
        return;
    };
    let old = "fn a() {\r\n    let x = \"old\";\r\n}\r\n";
    let new = "fn a() {\r\n    let x = \"new\";\r\n}\r\n";
    let overlay = run_pair(&runner, b"crlf.rs", old.as_bytes(), new.as_bytes());
    assert!(overlay.diagnostics.accepted > 0);
    assert_eq!(overlay.diagnostics.rejected_out_of_bounds, 0);
    assert_eq!(overlay.diagnostics.rejected_content_mismatch, 0);
}

#[test]
fn added_line_yields_spans_only_on_the_new_side() {
    let Some(runner) = difft_or_skip() else {
        return;
    };
    let old = "fn a() {}\n";
    let new = "fn a() {}\nfn b() {}\n";
    let overlay = run_pair(&runner, b"added.rs", old.as_bytes(), new.as_bytes());
    assert!(overlay.diagnostics.accepted > 0);
    assert_eq!(overlay.diagnostics.rejected_content_mismatch, 0);
    assert!(
        overlay.old.spans().is_empty(),
        "a pure addition must not decorate the old side"
    );
    assert!(
        !overlay.new.spans_for_line(LineIndex(1)).is_empty(),
        "the added line must carry spans on the new side"
    );
}

#[test]
fn removed_line_yields_spans_only_on_the_old_side() {
    let Some(runner) = difft_or_skip() else {
        return;
    };
    let old = "fn a() {}\nfn b() {}\n";
    let new = "fn a() {}\n";
    let overlay = run_pair(&runner, b"removed.rs", old.as_bytes(), new.as_bytes());
    assert!(overlay.diagnostics.accepted > 0);
    assert_eq!(overlay.diagnostics.rejected_content_mismatch, 0);
    assert!(
        overlay.new.spans().is_empty(),
        "a pure removal must not decorate the new side"
    );
    assert!(
        !overlay.old.spans_for_line(LineIndex(1)).is_empty(),
        "the removed line must carry spans on the old side"
    );
}

#[test]
fn empty_to_content_reports_created() {
    let Some(runner) = difft_or_skip() else {
        return;
    };
    let overlay = run_pair(&runner, b"created.rs", b"", b"fn a() {}\n");
    assert_eq!(overlay.status, DifftStatus::Created);
    assert_eq!(overlay.diagnostics.total, 0);
}

#[test]
fn missing_final_newline_is_handled() {
    let Some(runner) = difft_or_skip() else {
        return;
    };
    let overlay = run_pair(&runner, b"noeol.rs", b"let x = 1;", b"let x = 2;");
    assert!(overlay.diagnostics.accepted > 0);
    assert_eq!(overlay.diagnostics.rejected_out_of_bounds, 0);
}

#[test]
fn version_is_reported() {
    let Some(runner) = difft_or_skip() else {
        return;
    };
    let v = runner.version().unwrap();
    assert!(v.starts_with("Difftastic"), "unexpected version line: {v}");
}
