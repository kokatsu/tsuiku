//! One-shot comparison of the two line-diff engine candidates.
//! Run with: cargo run --release --example engine_bench
//!
//! Prints `METRIC name=value` lines (µs medians over N runs) for three input
//! shapes: a typical edited source file, a large file, and a pathological
//! input (thousands of identical lines) where Myers-family algorithms
//! degrade.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Instant;

use tsuiku::asyncstate::LineDiffEngineId;
use tsuiku::linediff::{engine, line_tokens};
use tsuiku::text::{ClassifiedContent, classify};

fn text_content(s: &str) -> tsuiku::text::TextContent {
    match classify(Arc::from(s.as_bytes())) {
        ClassifiedContent::Text(t) => t,
        ClassifiedContent::Binary(_) => panic!("bench input must be text"),
    }
}

/// A file of `n` distinct lines; `mutate` decides which lines change.
fn synth(n: usize, mutate: impl Fn(usize) -> Option<String>) -> (String, String) {
    let mut old = String::new();
    let mut new = String::new();
    for i in 0..n {
        let line = format!("    let value_{i} = compute({i}) + offset;");
        writeln!(old, "{line}").expect("infallible");
        match mutate(i) {
            Some(repl) => writeln!(new, "{repl}").expect("infallible"),
            None => writeln!(new, "{line}").expect("infallible"),
        }
    }
    (old, new)
}

fn bench(label: &str, old_src: &str, new_src: &str, runs: usize) {
    let (old_t, new_t) = (text_content(old_src), text_content(new_src));
    let (old, new) = (line_tokens(&old_t), line_tokens(&new_t));
    for id in [LineDiffEngineId::Imara, LineDiffEngineId::Similar] {
        let eng = engine(id);
        // Warm-up + correctness sanity.
        let rows = eng.diff(&old, &new);
        let mut samples: Vec<u128> = (0..runs)
            .map(|_| {
                let start = Instant::now();
                let r = eng.diff(&old, &new);
                let elapsed = start.elapsed().as_micros();
                assert_eq!(r.len(), rows.len());
                elapsed
            })
            .collect();
        samples.sort_unstable();
        let median = samples[samples.len() / 2];
        let p95 = samples[(samples.len() * 95) / 100];
        println!("METRIC {label}_{id:?}_median_us={median}");
        println!("METRIC {label}_{id:?}_p95_us={p95}");
    }
}

fn main() {
    // Typical: 300-line file, ~10 scattered edits.
    let (old, new) = synth(300, |i| {
        (i % 30 == 7).then(|| format!("    let value_{i} = recompute({i});"))
    });
    bench("typical_300", &old, &new, 200);

    // Large: 20k lines, 200 scattered edits.
    let (old, new) = synth(20_000, |i| {
        (i % 100 == 50).then(|| format!("    let value_{i} = recompute({i});"))
    });
    bench("large_20k", &old, &new, 30);

    // Pathological: 4000 identical lines vs 4000 identical-but-different
    // lines — worst case for token uniqueness heuristics.
    let old: String = "the same line every time\n".repeat(4000);
    let new: String = "a different same line every time\n".repeat(4000);
    bench("pathological_4k_identical", &old, &new, 10);

    // Pathological interleave: alternating blank/identical lines with a
    // block move — classic quadratic trap.
    let old: String = "x\n\n".repeat(2000);
    let new: String = format!("{}{}", "\nx\n".repeat(1000), "x\n\n".repeat(1000));
    bench("pathological_interleave", &old, &new, 10);
}
