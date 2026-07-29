//! Micro-benchmark for constructing one visible terminal body frame.
//!
//! The input is an already computed line-diff table. The timed section includes
//! mapping visible rows to their source text and constructing ratatui
//! `Line`/`Span` values; it excludes line-diff computation and terminal backend
//! drawing.
//!
//! Run with `cargo run --release --example view_bench`.

use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

use tsuiku::linediff::{DEFAULT_ENGINE, engine, line_tokens};
use tsuiku::text::{ClassifiedContent, TextContent, classify};
use tsuiku::view::build_unified_lines;

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
    let mut samples = Vec::with_capacity(10_000);
    for offset in (0..10_000).map(|i| i % rows.len().saturating_sub(50).max(1)) {
        let start = Instant::now();
        let visible = build_unified_lines(&rows, &old, &new, offset, 50);
        black_box(visible);
        samples.push(start.elapsed().as_nanos());
    }
    println!(
        "METRIC visible_frame_model_p95_ns={}",
        percentile(&mut samples, 95)
    );
}
