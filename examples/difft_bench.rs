//! What one difft invocation costs, at the sizes tsuiku will actually feed it.
//!
//! What the guards around the difft subprocess need — its timeout, its size
//! limit, and how many may run at once — is not throughput but the fixed cost
//! per file pair: difft runs once per pair, so start-up plus parse dominates
//! for the small files that make up most of a review.
//!
//! ```text
//! cargo run --release --example difft_bench
//! ```

use std::time::{Duration, Instant};

use tsuiku::path::GitPath;
use tsuiku::structural::runner::DifftRunner;
use tsuiku::structural::tempfiles::{LanguagePathHint, materialize};

/// A pair of Rust files of roughly `lines` lines, differing in one place.
fn rust_pair(lines: usize) -> (Vec<u8>, Vec<u8>) {
    let mut old = String::new();
    let mut new = String::new();
    for i in 0..lines {
        old.push_str(&format!("fn f{i}(x: i64) -> i64 {{ x + {i} }}\n"));
        if i == lines / 2 {
            new.push_str(&format!("fn f{i}(x: i64) -> i64 {{ x * {i} }}\n"));
        } else {
            new.push_str(&format!("fn f{i}(x: i64) -> i64 {{ x + {i} }}\n"));
        }
    }
    (old.into_bytes(), new.into_bytes())
}

fn bench(label: &str, runner: &DifftRunner, old: &[u8], new: &[u8], runs: u32) {
    let hint = LanguagePathHint::from_git_path(&GitPath::from_bytes(b"bench.rs"));
    let mut best = Duration::MAX;
    let mut worst = Duration::ZERO;
    let mut total = Duration::ZERO;
    for _ in 0..runs {
        let pair = materialize(old, new, &hint, &hint).expect("write temp files");
        let t = Instant::now();
        runner
            .run(&pair.old_path, &pair.new_path)
            .expect("run difft");
        let elapsed = t.elapsed();
        best = best.min(elapsed);
        worst = worst.max(elapsed);
        total += elapsed;
    }
    println!(
        "  {label:<24} min {best:>9.3?}  mean {:>9.3?}  max {worst:>9.3?}",
        total / runs
    );
}

fn main() {
    let runner = DifftRunner::default();
    match runner.version() {
        Ok(v) => println!("{v}"),
        Err(e) => {
            println!("difft unavailable: {e:?}");
            return;
        }
    }

    // An empty pair isolates the fixed cost: process start, language
    // detection, and JSON emission with nothing to compare.
    println!("\nper-invocation cost:");
    bench("empty", &runner, b"", b"", 20);
    for lines in [10usize, 100, 1_000, 10_000] {
        let (old, new) = rust_pair(lines);
        bench(&format!("{lines} lines"), &runner, &old, &new, 20);
    }

    let t = Instant::now();
    runner.version().expect("difft version");
    println!("\n  difft --version          {:>9.3?}", t.elapsed());
}
