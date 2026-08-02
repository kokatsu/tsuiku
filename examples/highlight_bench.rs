//! Syntax highlighting engine comparison: syntect vs tree-sitter.
//!
//! Verdict (2026-08, both engines at the versions in Cargo.toml): syntect,
//! with two-face supplying the syntaxes missing from the default set
//! (TypeScript, Nix, TOML). tree-sitter highlighted 5-11x faster, but the
//! grammar crates' bundled queries misclassify Go function names and Nix
//! attributes and miss Markdown inline elements; fixing that would mean
//! vendoring per-language queries. Highlighting runs asynchronously in a
//! worker, so syntect's speed is sufficient. The tree-sitter dev-dependencies
//! stay so this comparison remains reproducible.
//!
//! Three sections, all printed to stdout:
//!
//! 1. init cost — one-time setup each engine needs before the first
//!    highlight, weighed against the startup budget (p95 < 30ms).
//! 2. token probes — for each language tsuiku aims to color, the scope/capture
//!    each engine reports at representative tokens (keyword, string,
//!    comment, function name). "NOT FOUND" rows are coverage findings, not
//!    errors.
//! 3. whole-file timing — `METRIC` lines for 300 / 1.7k / 20k-line files,
//!    median of 5 runs, matching how the app would highlight one side of a
//!    pair in a worker.
//!
//! Run with `cargo run --release --example highlight_bench`.

use std::time::Instant;

use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::{ParseState, ScopeStack, SyntaxSet};
use syntect::util::LinesWithEndings;
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

/// Capture names recognized for tree-sitter, in priority order.
const TS_CAPTURE_NAMES: &[&str] = &[
    "attribute",
    "comment",
    "constant",
    "constant.builtin",
    "constructor",
    "embedded",
    "function",
    "function.builtin",
    "function.method",
    "keyword",
    "label",
    "number",
    "operator",
    "property",
    "punctuation",
    "punctuation.bracket",
    "punctuation.delimiter",
    "punctuation.special",
    "string",
    "string.special",
    "tag",
    "text.literal",
    "text.title",
    "text.emphasis",
    "text.reference",
    "text.uri",
    "type",
    "type.builtin",
    "variable",
    "variable.builtin",
    "variable.parameter",
];

/// One representative token to classify: a label plus the needle to locate
/// inside the snippet. The probe reports whatever the engine says at the
/// needle's first byte.
struct Probe {
    label: &'static str,
    needle: &'static str,
}

struct Lang {
    name: &'static str,
    /// Extension syntect resolves the syntax with.
    ext: &'static str,
    /// Grammar + highlight query, `None` when the crate ships no query.
    ts: Option<fn() -> Result<HighlightConfiguration, tree_sitter::QueryError>>,
    snippet: &'static str,
    probes: &'static [Probe],
}

fn ts_config(
    language: tree_sitter::Language,
    name: &str,
    highlights: &str,
    injections: &str,
    locals: &str,
) -> Result<HighlightConfiguration, tree_sitter::QueryError> {
    let mut config = HighlightConfiguration::new(language, name, highlights, injections, locals)?;
    config.configure(TS_CAPTURE_NAMES);
    Ok(config)
}

fn langs() -> Vec<Lang> {
    vec![
        Lang {
            name: "Rust",
            ext: "rs",
            ts: Some(|| {
                ts_config(
                    tree_sitter_rust::LANGUAGE.into(),
                    "rust",
                    tree_sitter_rust::HIGHLIGHTS_QUERY,
                    tree_sitter_rust::INJECTIONS_QUERY,
                    "",
                )
            }),
            snippet: "fn greet(name: &str) -> String {\n    format!(\"hi {name}\") // note\n}\n",
            probes: &[
                Probe {
                    label: "keyword",
                    needle: "fn",
                },
                Probe {
                    label: "function",
                    needle: "greet",
                },
                Probe {
                    label: "string",
                    needle: "\"hi {name}\"",
                },
                Probe {
                    label: "comment",
                    needle: "// note",
                },
            ],
        },
        Lang {
            name: "TypeScript",
            ext: "ts",
            ts: Some(|| {
                // The TS query extends the JS one; they must be concatenated.
                let highlights = format!(
                    "{}\n{}",
                    tree_sitter_javascript::HIGHLIGHT_QUERY,
                    tree_sitter_typescript::HIGHLIGHTS_QUERY,
                );
                let locals = format!(
                    "{}\n{}",
                    tree_sitter_javascript::LOCALS_QUERY,
                    tree_sitter_typescript::LOCALS_QUERY,
                );
                ts_config(
                    tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                    "typescript",
                    &highlights,
                    "",
                    &locals,
                )
            }),
            snippet: "function greet(name: string): string {\n    return `hi`; // note\n}\n",
            probes: &[
                Probe {
                    label: "keyword",
                    needle: "function",
                },
                Probe {
                    label: "function",
                    needle: "greet",
                },
                Probe {
                    label: "string",
                    needle: "`hi`",
                },
                Probe {
                    label: "comment",
                    needle: "// note",
                },
            ],
        },
        Lang {
            name: "Python",
            ext: "py",
            ts: Some(|| {
                ts_config(
                    tree_sitter_python::LANGUAGE.into(),
                    "python",
                    tree_sitter_python::HIGHLIGHTS_QUERY,
                    "",
                    "",
                )
            }),
            snippet: "def greet(name):\n    return \"hi\"  # note\n",
            probes: &[
                Probe {
                    label: "keyword",
                    needle: "def",
                },
                Probe {
                    label: "function",
                    needle: "greet",
                },
                Probe {
                    label: "string",
                    needle: "\"hi\"",
                },
                Probe {
                    label: "comment",
                    needle: "# note",
                },
            ],
        },
        Lang {
            name: "Go",
            ext: "go",
            ts: Some(|| {
                ts_config(
                    tree_sitter_go::LANGUAGE.into(),
                    "go",
                    tree_sitter_go::HIGHLIGHTS_QUERY,
                    "",
                    "",
                )
            }),
            snippet: "func greet() string {\n    return \"hi\" // note\n}\n",
            probes: &[
                Probe {
                    label: "keyword",
                    needle: "func",
                },
                Probe {
                    label: "function",
                    needle: "greet",
                },
                Probe {
                    label: "string",
                    needle: "\"hi\"",
                },
                Probe {
                    label: "comment",
                    needle: "// note",
                },
            ],
        },
        Lang {
            name: "Nix",
            ext: "nix",
            ts: Some(|| {
                ts_config(
                    tree_sitter_nix::LANGUAGE.into(),
                    "nix",
                    tree_sitter_nix::HIGHLIGHTS_QUERY,
                    "",
                    "",
                )
            }),
            snippet: "{ pkgs }:\nlet name = \"hi\"; # note\nin { inherit name; }\n",
            probes: &[
                Probe {
                    label: "keyword",
                    needle: "let",
                },
                Probe {
                    label: "attribute",
                    needle: "name",
                },
                Probe {
                    label: "string",
                    needle: "\"hi\"",
                },
                Probe {
                    label: "comment",
                    needle: "# note",
                },
            ],
        },
        Lang {
            name: "TOML",
            ext: "toml",
            ts: Some(|| {
                ts_config(
                    tree_sitter_toml_ng::LANGUAGE.into(),
                    "toml",
                    tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
                    "",
                    "",
                )
            }),
            snippet: "[package]\nname = \"hi\" # note\nedition = 2024\n",
            probes: &[
                Probe {
                    label: "table",
                    needle: "package",
                },
                Probe {
                    label: "key",
                    needle: "name",
                },
                Probe {
                    label: "string",
                    needle: "\"hi\"",
                },
                Probe {
                    label: "comment",
                    needle: "# note",
                },
            ],
        },
        Lang {
            name: "YAML",
            ext: "yaml",
            ts: Some(|| {
                ts_config(
                    tree_sitter_yaml::LANGUAGE.into(),
                    "yaml",
                    tree_sitter_yaml::HIGHLIGHTS_QUERY,
                    "",
                    "",
                )
            }),
            snippet: "name: \"hi\" # note\nitems:\n  - one\n",
            probes: &[
                Probe {
                    label: "key",
                    needle: "name",
                },
                Probe {
                    label: "string",
                    needle: "\"hi\"",
                },
                Probe {
                    label: "comment",
                    needle: "# note",
                },
            ],
        },
        Lang {
            name: "Markdown",
            ext: "md",
            ts: Some(|| {
                ts_config(
                    tree_sitter_md::LANGUAGE.into(),
                    "markdown",
                    tree_sitter_md::HIGHLIGHT_QUERY_BLOCK,
                    tree_sitter_md::INJECTION_QUERY_BLOCK,
                    "",
                )
            }),
            snippet: "# Title\n\nsome *emphasis* and `code`.\n",
            probes: &[
                Probe {
                    label: "heading",
                    needle: "# Title",
                },
                Probe {
                    label: "emphasis",
                    needle: "*emphasis*",
                },
                Probe {
                    label: "code",
                    needle: "`code`",
                },
            ],
        },
        Lang {
            name: "shell",
            ext: "sh",
            ts: Some(|| {
                ts_config(
                    tree_sitter_bash::LANGUAGE.into(),
                    "bash",
                    tree_sitter_bash::HIGHLIGHT_QUERY,
                    "",
                    "",
                )
            }),
            snippet: "greet() {\n    echo \"hi\" # note\n}\n",
            probes: &[
                Probe {
                    label: "function",
                    needle: "greet",
                },
                Probe {
                    label: "builtin",
                    needle: "echo",
                },
                Probe {
                    label: "string",
                    needle: "\"hi\"",
                },
                Probe {
                    label: "comment",
                    needle: "# note",
                },
            ],
        },
    ]
}

/// Scope stack syntect reports at `offset` bytes into `text`.
fn syntect_scope_at(ss: &SyntaxSet, ext: &str, text: &str, offset: usize) -> Option<String> {
    let syntax = ss.find_syntax_by_extension(ext)?;
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut line_start = 0;
    for line in LinesWithEndings::from(text) {
        let ops = state.parse_line(line, ss).ok()?;
        if offset < line_start + line.len() {
            let in_line = offset - line_start;
            for (op_offset, op) in &ops {
                if *op_offset > in_line {
                    break;
                }
                stack.apply(op).ok()?;
            }
            let scopes: Vec<String> = stack.scopes.iter().map(|s| s.build_string()).collect();
            return Some(scopes.join(" "));
        }
        for (_, op) in &ops {
            stack.apply(op).ok()?;
        }
        line_start += line.len();
    }
    None
}

/// Capture name tree-sitter reports at `offset` bytes into `text`.
fn ts_capture_at(config: &HighlightConfiguration, text: &str, offset: usize) -> Option<String> {
    let mut highlighter = Highlighter::new();
    let events = highlighter
        .highlight(config, text.as_bytes(), None, |_| None)
        .ok()?;
    let mut active: Vec<usize> = Vec::new();
    for event in events {
        match event.ok()? {
            HighlightEvent::HighlightStart(h) => active.push(h.0),
            HighlightEvent::HighlightEnd => {
                active.pop();
            }
            HighlightEvent::Source { start, end } => {
                if start <= offset && offset < end {
                    return match active.last() {
                        Some(&idx) => Some(TS_CAPTURE_NAMES[idx].to_string()),
                        None => Some("(no capture)".to_string()),
                    };
                }
            }
        }
    }
    None
}

/// Synthetic file of roughly `lines` lines in the given language.
fn synth(ext: &str, lines: usize) -> String {
    let block: &str = match ext {
        "rs" => {
            "fn item_N() -> usize {\n    let value = \"vN\"; // block N\n    value.len() + N\n}\n\n"
        }
        "ts" => {
            "function itemN(): number {\n    const value = `vN`; // block N\n    return value.length + N;\n}\n\n"
        }
        "py" => "def item_N():\n    value = \"vN\"  # block N\n    return len(value) + N\n\n",
        "go" => {
            "func itemN() int {\n    value := \"vN\" // block N\n    return len(value) + N\n}\n\n"
        }
        _ => unreachable!("timing fixtures cover rs/ts/py/go only"),
    };
    let per_block = block.lines().count();
    let mut out = String::new();
    for n in 0..lines.div_ceil(per_block) {
        out.push_str(&block.replace('N', &n.to_string()));
    }
    assert_eq!(
        out.lines().count(),
        lines,
        "synthetic fixture must have exactly the requested line count"
    );
    out
}

fn median_us(mut samples: Vec<u128>) -> u128 {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

fn main() {
    let mut init = Instant::now();
    let defaults = SyntaxSet::load_defaults_newlines();
    let syntax_set_us = init.elapsed().as_micros();
    init = Instant::now();
    let extra = two_face::syntax::extra_newlines();
    let extra_us = init.elapsed().as_micros();
    init = Instant::now();
    let themes = ThemeSet::load_defaults();
    let theme_us = init.elapsed().as_micros();
    let theme = &themes.themes["base16-ocean.dark"];

    let langs = langs();

    println!("== init cost ==");
    println!("syntect SyntaxSet::load_defaults_newlines: {syntax_set_us}us");
    println!("syntect two-face extra_newlines:           {extra_us}us");
    println!("syntect ThemeSet::load_defaults:           {theme_us}us");
    let mut ts_configs = Vec::new();
    for lang in &langs {
        // A build error means a grammar/query incompatibility: fail loudly so
        // CI catches it instead of silently dropping the comparison.
        let (config, us) = match lang.ts {
            Some(build) => {
                let t = Instant::now();
                let config = build().unwrap_or_else(|error| {
                    panic!("{} highlight query failed to build: {error}", lang.name)
                });
                let us = t.elapsed().as_micros();
                (Some(config), us)
            }
            None => (None, 0),
        };
        let status = if config.is_some() { "ok" } else { "no query" };
        println!("tree-sitter config {:<10} {us:>6}us  {status}", lang.name);
        ts_configs.push(config);
    }

    let set_for = |ext: &str| {
        if defaults.find_syntax_by_extension(ext).is_some() {
            Some((&defaults, "defaults"))
        } else if extra.find_syntax_by_extension(ext).is_some() {
            Some((&extra, "two-face"))
        } else {
            None
        }
    };

    println!("\n== token probes ==");
    for (lang, config) in langs.iter().zip(&ts_configs) {
        println!("--- {} ---", lang.name);
        let set = set_for(lang.ext);
        match set {
            Some((_, origin)) => println!("  syntect syntax source: {origin}"),
            None => println!(
                "  syntect: extension .{} NOT FOUND in defaults or two-face",
                lang.ext
            ),
        }
        for probe in lang.probes {
            let offset = lang
                .snippet
                .find(probe.needle)
                .expect("probe needle must occur in its snippet");
            let syn = match set {
                Some((set, _)) => syntect_scope_at(set, lang.ext, lang.snippet, offset)
                    .unwrap_or_else(|| "(no scope)".into()),
                None => "-".into(),
            };
            let ts = match config {
                Some(c) => {
                    ts_capture_at(c, lang.snippet, offset).unwrap_or_else(|| "(no capture)".into())
                }
                None => "-".into(),
            };
            println!("  {:<10} syntect: {syn}", probe.label);
            println!("  {:<10} tree-sitter: {ts}", "");
        }
    }

    println!("\n== whole-file timing (median of 5) ==");
    for (lang, config) in langs.iter().zip(&ts_configs) {
        if !matches!(lang.ext, "rs" | "ts" | "py" | "go") {
            continue;
        }
        for &lines in &[300usize, 1_700, 20_000] {
            let text = synth(lang.ext, lines);
            if let Some((set, _)) = set_for(lang.ext) {
                let syntax = set
                    .find_syntax_by_extension(lang.ext)
                    .expect("set_for guarantees the syntax exists");
                let samples: Vec<u128> = (0..5)
                    .map(|_| {
                        let t = Instant::now();
                        let mut hl = HighlightLines::new(syntax, theme);
                        for line in LinesWithEndings::from(&text) {
                            hl.highlight_line(line, set)
                                .expect("syntect must highlight synthetic fixture");
                        }
                        t.elapsed().as_micros()
                    })
                    .collect();
                println!(
                    "METRIC highlight_{}_{}_syntect_median_us={}",
                    lang.ext,
                    lines,
                    median_us(samples)
                );
            }
            if let Some(config) = config {
                let samples: Vec<u128> = (0..5)
                    .map(|_| {
                        let t = Instant::now();
                        let mut highlighter = Highlighter::new();
                        let events = highlighter
                            .highlight(config, text.as_bytes(), None, |_| None)
                            .expect("tree-sitter must highlight synthetic fixture");
                        for event in events {
                            event.expect("highlight event must decode");
                        }
                        t.elapsed().as_micros()
                    })
                    .collect();
                println!(
                    "METRIC highlight_{}_{}_treesitter_median_us={}",
                    lang.ext,
                    lines,
                    median_us(samples)
                );
            }
        }
    }
}
