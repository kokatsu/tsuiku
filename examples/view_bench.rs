//! Micro-benchmark for constructing one visible terminal body frame.
//!
//! The input is an already computed line-diff table. The timed section includes
//! mapping visible rows to their source text and constructing ratatui
//! `Line`/`Span` values; it excludes line-diff computation and terminal backend
//! drawing.
//!
//! Three frame paths are measured: without overlays, with a structural
//! overlay covering every row (the worst case for span splitting; a real
//! overlay decorates only changed lines), and with syntax spans for both
//! sides stacked on top of that overlay.
//!
//! Run with `cargo run --release --example view_bench`.

use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

use tsuiku::compose::RowOverlays;
use tsuiku::linediff::{DEFAULT_ENGINE, engine, line_tokens};
use tsuiku::structural::json::parse;
use tsuiku::structural::normalize::{StructuralOverlay, normalize};
use tsuiku::structural::tempfiles::LanguagePathHint;
use tsuiku::syntax::{DEFAULT_THEME, HighlightAssets, HighlightOutcome, SyntaxSpans};
use tsuiku::text::{ClassifiedContent, TextContent, classify};
use tsuiku::theme::{ThemeChoice, theme};
use tsuiku::view::{build_split_lines, build_unified_lines, build_unified_lines_with_overlay};

fn text(source: String) -> TextContent {
    match classify(Arc::from(source.into_bytes())) {
        ClassifiedContent::Text(text) => text,
        ClassifiedContent::Binary(_) => unreachable!("generated fixture is UTF-8"),
    }
}

fn percentile(samples: &mut [u128], percentile: usize) -> u128 {
    samples.sort_unstable();
    samples[(samples.len() - 1) * percentile / 100]
}

/// One accepted span on every line of `new`, covering the leading keyword.
fn dense_overlay(new: &TextContent, lines: usize) -> StructuralOverlay {
    let entries: Vec<String> = (0..lines)
        .map(|i| {
            format!(
                r#"{{"rhs":{{"line_number":{i},"changes":[{{"start":0,"end":3,"content":"let","highlight":"keyword"}}]}}}}"#
            )
        })
        .collect();
    let raw = format!(
        r#"{{"language":"Rust","path":"bench.rs","status":"changed","chunks":[[{}]]}}"#,
        entries.join(",")
    );
    let overlay = normalize(&parse(&raw).expect("bench fixture parses"), None, Some(new));
    assert_eq!(
        overlay.diagnostics.accepted, overlay.diagnostics.total,
        "bench overlay must be fully accepted"
    );
    overlay
}

fn rust_syntax(content: &TextContent) -> Arc<SyntaxSpans> {
    let hint = LanguagePathHint {
        extension: Some(b"rs".to_vec()),
        basename: Some(b"bench.rs".to_vec()),
    };
    match HighlightAssets::load().highlight(content, &hint, DEFAULT_THEME) {
        HighlightOutcome::Ready(spans) => spans,
        _ => unreachable!("rust fixture must highlight"),
    }
}

fn main() {
    let old = text(
        (0..20_000)
            .map(|i| format!("let item_{i} = \"old\"; // note\n"))
            .collect(),
    );
    let new = text(
        (0..20_000)
            .map(|i| {
                if i % 50 == 0 {
                    format!("let item_{i} = \"changed\"; // note\n")
                } else {
                    format!("let item_{i} = \"old\"; // note\n")
                }
            })
            .collect(),
    );
    let rows = engine(DEFAULT_ENGINE).diff(&line_tokens(&old), &line_tokens(&new));
    let offsets: Vec<usize> = (0..10_000)
        .map(|i| i % rows.len().saturating_sub(50).max(1))
        .collect();

    let overlay = dense_overlay(&new, 20_000);
    let syntax_old = rust_syntax(&old);
    let syntax_new = rust_syntax(&new);
    let structural_only = RowOverlays {
        structural: Some(&overlay),
        syntax_old: None,
        syntax_new: None,
    };
    let full = RowOverlays {
        structural: Some(&overlay),
        syntax_old: Some(&syntax_old),
        syntax_new: Some(&syntax_new),
    };

    // All variants are timed in one interleaved loop: measuring them in
    // sequence made whichever ran later look faster purely from warm-up.
    let mut plain = Vec::with_capacity(offsets.len());
    let mut with_overlay = Vec::with_capacity(offsets.len());
    let mut with_syntax = Vec::with_capacity(offsets.len());
    let mut split = Vec::with_capacity(offsets.len());
    for (iteration, &offset) in offsets.iter().enumerate() {
        let start = Instant::now();
        black_box(build_unified_lines(&rows, &old, &new, offset, 50));
        let plain_elapsed = start.elapsed().as_nanos();

        let start = Instant::now();
        black_box(build_unified_lines_with_overlay(
            &rows,
            &old,
            &new,
            structural_only,
            theme(ThemeChoice::Dark),
            offset,
            50,
        ));
        let overlay_elapsed = start.elapsed().as_nanos();

        let start = Instant::now();
        black_box(build_unified_lines_with_overlay(
            &rows,
            &old,
            &new,
            full,
            theme(ThemeChoice::Dark),
            offset,
            50,
        ));
        let syntax_elapsed = start.elapsed().as_nanos();

        let start = Instant::now();
        black_box(build_split_lines(
            &rows,
            &old,
            &new,
            full,
            theme(ThemeChoice::Dark),
            offset,
            50,
        ));
        let split_elapsed = start.elapsed().as_nanos();

        // Discard the warm-up prefix rather than the fixture's cold pages.
        if iteration >= 1_000 {
            plain.push(plain_elapsed);
            with_overlay.push(overlay_elapsed);
            with_syntax.push(syntax_elapsed);
            split.push(split_elapsed);
        }
    }
    println!(
        "METRIC visible_frame_model_p95_ns={}",
        percentile(&mut plain, 95)
    );
    println!(
        "METRIC visible_frame_overlay_p95_ns={}",
        percentile(&mut with_overlay, 95)
    );
    println!(
        "METRIC visible_frame_syntax_p95_ns={}",
        percentile(&mut with_syntax, 95)
    );
    println!(
        "METRIC visible_frame_split_p95_ns={}",
        percentile(&mut split, 95)
    );
}
