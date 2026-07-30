//! Structural-overlay scenario matrix against the installed difftastic.
//!
//! These are contract tests, not golden tests for difft's matching choices:
//! upstream may choose different changed tokens in a newer compatible release.
//! Tsuiku's invariant is that every emitted span is either validated exactly
//! or rejected and counted, never silently misapplied.

use std::sync::Arc;

use tsuiku::asyncstate::StructuralError;
use tsuiku::path::GitPath;
use tsuiku::structural::normalize::{DifftStatus, OverlayDiagnostics, normalize};
use tsuiku::structural::runner::DifftRunner;
use tsuiku::structural::tempfiles::{LanguagePathHint, materialize};
use tsuiku::text::{ClassifiedContent, TextContent, classify};

struct Scenario {
    name: &'static str,
    old_path: &'static [u8],
    new_path: &'static [u8],
    old: &'static str,
    new: &'static str,
}

fn text(source: &str) -> TextContent {
    match classify(Arc::from(source.as_bytes())) {
        ClassifiedContent::Text(text) => text,
        ClassifiedContent::Binary(_) => panic!("scenario must be text"),
    }
}

fn rejected(diag: OverlayDiagnostics) -> u32 {
    diag.rejected_out_of_bounds
        + diag.rejected_invalid_boundary
        + diag.rejected_content_mismatch
        + diag.rejected_empty
}

#[test]
fn m2_scenario_matrix_preserves_the_overlay_contract() {
    let runner = DifftRunner::default();
    match runner.version() {
        Ok(version) => eprintln!("structural difft_version={version}"),
        Err(StructuralError::ToolNotFound) => {
            eprintln!("skipping structural scenarios: difft is not installed");
            return;
        }
        Err(error) => panic!("difft version probe failed: {error:?}"),
    }

    let scenarios = [
        Scenario {
            name: "formatter_only",
            old_path: b"format.rs",
            new_path: b"format.rs",
            old: "fn add(a: i32, b: i32) -> i32 { a + b }\n",
            new: "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        },
        Scenario {
            name: "function_move",
            old_path: b"move.rs",
            new_path: b"move.rs",
            old: "fn first() { println!(\"first\"); }\nfn second() { println!(\"second\"); }\n",
            new: "fn second() { println!(\"second\"); }\nfn first() { println!(\"first\"); }\n",
        },
        Scenario {
            name: "argument_reorder",
            old_path: b"args.rs",
            new_path: b"args.rs",
            old: "fn main() { send(alpha, beta, gamma); }\n",
            new: "fn main() { send(gamma, alpha, beta); }\n",
        },
        Scenario {
            name: "nesting_change",
            old_path: b"nest.rs",
            new_path: b"nest.rs",
            old: "fn main() { if ready() { run(); } cleanup(); }\n",
            new: "fn main() { if ready() { run(); cleanup(); } }\n",
        },
        Scenario {
            name: "repeated_text",
            old_path: b"repeat.rs",
            new_path: b"repeat.rs",
            old: "fn main() { log(\"same\"); log(\"same\"); log(\"old\"); }\n",
            new: "fn main() { log(\"same\"); log(\"new\"); log(\"same\"); }\n",
        },
        Scenario {
            name: "rename_and_edit",
            old_path: b"before.rs",
            new_path: b"after.rs",
            old: "fn old_name() { println!(\"old\"); }\n",
            new: "fn new_name() { println!(\"new\"); }\n",
        },
        Scenario {
            name: "one_sided_syntax_error",
            old_path: b"broken.rs",
            new_path: b"broken.rs",
            old: "fn main() { println!(\"ok\"); }\n",
            new: "fn main( { println!(\"broken\"); }\n",
        },
        Scenario {
            name: "language_detection_fallback",
            old_path: b"sample.unknownext",
            new_path: b"sample.unknownext",
            old: "alpha beta\n",
            new: "alpha gamma\n",
        },
    ];

    for scenario in scenarios {
        let old = text(scenario.old);
        let new = text(scenario.new);
        let pair = materialize(
            scenario.old.as_bytes(),
            scenario.new.as_bytes(),
            &LanguagePathHint::from_git_path(&GitPath::from_bytes(scenario.old_path)),
            &LanguagePathHint::from_git_path(&GitPath::from_bytes(scenario.new_path)),
        )
        .expect("materialize scenario");
        let raw = runner
            .run(&pair.old_path, &pair.new_path)
            .unwrap_or_else(|error| panic!("{} failed: {error:?}", scenario.name));
        let overlay = normalize(&raw, Some(&old), Some(&new));

        eprintln!(
            "structural scenario={} language={} accepted={} total={} rejected={} merged={}",
            scenario.name,
            overlay.language,
            overlay.diagnostics.accepted,
            overlay.diagnostics.total,
            rejected(overlay.diagnostics),
            overlay.diagnostics.merged,
        );
        assert!(
            matches!(
                overlay.status,
                DifftStatus::Changed | DifftStatus::Unchanged
            ),
            "{} returned an unexpected file status",
            scenario.name
        );
        assert_eq!(
            rejected(overlay.diagnostics),
            0,
            "{} emitted spans outside the fixed coordinate contract",
            scenario.name
        );
        assert_eq!(
            overlay.diagnostics.accepted, overlay.diagnostics.total,
            "{} did not accept every observed span",
            scenario.name
        );
    }
}
