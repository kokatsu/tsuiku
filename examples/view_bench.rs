//! Micro-benchmark for constructing one visible terminal body frame.
//!
//! The input is an already computed line-diff table. The timed section includes
//! mapping visible rows to their source text and constructing ratatui
//! `Line`/`Span` values; it excludes line-diff computation and terminal backend
//! drawing.
//!
//! Both frame paths are measured: without a structural overlay, and with one
//! that covers every row, which is the worst case for span splitting (a real
//! overlay decorates only changed lines).
//!
//! Run with `cargo run --release --example view_bench`.

use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

use tsuiku::linediff::{DEFAULT_ENGINE, engine, line_tokens};
use tsuiku::structural::json::parse;
use tsuiku::structural::normalize::{StructuralOverlay, normalize};
use tsuiku::text::{ClassifiedContent, TextContent, classify};
use tsuiku::view::{build_unified_lines, build_unified_lines_with_overlay};

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

/// One accepted span on every line of `new`, covering the leading word.
fn dense_overlay(new: &TextContent, lines: usize) -> StructuralOverlay {
    let entries: Vec<String> = (0..lines)
        .map(|i| {
            let word = if i % 50 == 0 { "changed" } else { "old" };
            format!(
                r#"{{"rhs":{{"line_number":{i},"changes":[{{"start":0,"end":{},"content":"{word}","highlight":"keyword"}}]}}}}"#,
                word.len()
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

fn main() {
    let old = text((0..20_000).map(|i| format!("old line {i}\n")).collect());
    let new = text(
        (0..20_000)
            .map(|i| {
                if i % 50 == 0 {
                    format!("changed line {i}\n")
                } else {
                    format!("old line {i}\n")
                }
            })
            .collect(),
    );
    let rows = engine(DEFAULT_ENGINE).diff(&line_tokens(&old), &line_tokens(&new));
    let offsets: Vec<usize> = (0..10_000)
        .map(|i| i % rows.len().saturating_sub(50).max(1))
        .collect();

    let overlay = dense_overlay(&new, 20_000);

    // Both variants are timed in one interleaved loop: measuring them in
    // sequence made whichever ran second look faster purely from warm-up.
    let mut plain = Vec::with_capacity(offsets.len());
    let mut with_overlay = Vec::with_capacity(offsets.len());
    for (iteration, &offset) in offsets.iter().enumerate() {
        let start = Instant::now();
        black_box(build_unified_lines(&rows, &old, &new, offset, 50));
        let plain_elapsed = start.elapsed().as_nanos();

        let start = Instant::now();
        black_box(build_unified_lines_with_overlay(
            &rows,
            &old,
            &new,
            Some(&overlay),
            offset,
            50,
        ));
        let overlay_elapsed = start.elapsed().as_nanos();

        // Discard the warm-up prefix rather than the fixture's cold pages.
        if iteration >= 1_000 {
            plain.push(plain_elapsed);
            with_overlay.push(overlay_elapsed);
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
}
