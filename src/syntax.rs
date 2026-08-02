//! Whole-file syntax highlighting via syntect.
//!
//! Highlighting runs in a worker over one side's full text and produces
//! per-line foreground spans. Syntax supplies foreground color only:
//! backgrounds belong to the line-diff and structural layers, and an
//! explicit structural foreground wins over a syntax foreground.
//!
//! Spans live in line-body byte coordinates, like structural spans. They are
//! derived by slicing each line as `&str`, so both endpoints are UTF-8
//! character boundaries by construction; ranges reaching into the CR/LF
//! terminator are clamped to the body. Runs whose color equals the theme's
//! default foreground are not stored — an uncovered stretch of a line simply
//! renders in the default color.

use std::sync::Arc;

use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

use crate::coords::{LineByteRange, LineIndex};
use crate::structural::tempfiles::LanguagePathHint;
use crate::text::TextContent;

/// Identifies a bundled theme. Part of the syntax cache key, so spans colored
/// for one theme can never be applied under another.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ThemeId(pub u16);

/// The only theme until theme configuration exists.
pub const DEFAULT_THEME: ThemeId = ThemeId(0);

/// Foreground-only color contributed by the syntax layer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SyntaxFg {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// One foreground run on one line, in line-body byte coordinates.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SyntaxLineSpan {
    pub line: LineIndex,
    pub range: LineByteRange,
    pub fg: SyntaxFg,
}

/// All highlight spans for one side, sorted by (line, start).
#[derive(Clone, Debug, Default)]
pub struct SyntaxSpans {
    spans: Vec<SyntaxLineSpan>,
}

impl SyntaxSpans {
    pub fn spans(&self) -> &[SyntaxLineSpan] {
        &self.spans
    }

    /// All spans on one line, via binary search.
    pub fn spans_for_line(&self, line: LineIndex) -> &[SyntaxLineSpan] {
        let lo = self.spans.partition_point(|s| s.line < line);
        let hi = self.spans.partition_point(|s| s.line <= line);
        &self.spans[lo..hi]
    }

    pub fn estimated_bytes(&self) -> usize {
        self.spans.len() * std::mem::size_of::<SyntaxLineSpan>()
    }
}

/// Everything syntect needs, loaded once per worker thread (a few ms, kept
/// off the terminal thread so startup stays within budget).
pub struct HighlightAssets {
    defaults: SyntaxSet,
    extra: SyntaxSet,
    themes: ThemeSet,
}

/// Outcome of highlighting one side.
pub enum HighlightOutcome {
    Ready(Arc<SyntaxSpans>),
    /// No syntax matches the path hint — deterministic for this hint.
    UnsupportedLanguage,
    /// syntect reported a parse/apply error mid-file.
    Failed,
}

impl HighlightAssets {
    pub fn load() -> Self {
        Self {
            defaults: SyntaxSet::load_defaults_newlines(),
            extra: two_face::syntax::extra_newlines(),
            themes: ThemeSet::load_defaults(),
        }
    }

    fn theme(&self, _id: ThemeId) -> &Theme {
        // Theme configuration will map ThemeId values to bundled themes;
        // until then every id renders with the single built-in theme.
        &self.themes.themes["base16-ocean.dark"]
    }

    /// The syntax for a path hint: extension first, then the full basename
    /// (syntect lists names like `Makefile` among extensions). The default
    /// set wins over two-face so both sides of a pair agree with the bench.
    fn syntax_for(&self, hint: &LanguagePathHint) -> Option<(&SyntaxSet, &SyntaxReference)> {
        let candidates = [hint.extension.as_deref(), hint.basename.as_deref()];
        for candidate in candidates.into_iter().flatten() {
            let Ok(token) = std::str::from_utf8(candidate) else {
                continue;
            };
            for set in [&self.defaults, &self.extra] {
                if let Some(syntax) = set.find_syntax_by_extension(token) {
                    return Some((set, syntax));
                }
            }
        }
        None
    }

    /// Highlight one side's full text into per-line foreground spans.
    pub fn highlight(
        &self,
        text: &TextContent,
        hint: &LanguagePathHint,
        theme_id: ThemeId,
    ) -> HighlightOutcome {
        let Some((set, syntax)) = self.syntax_for(hint) else {
            return HighlightOutcome::UnsupportedLanguage;
        };
        let theme = self.theme(theme_id);
        let default_fg = theme.settings.foreground;
        let mut highlighter = HighlightLines::new(syntax, theme);
        let mut spans = Vec::new();

        for (index, record) in text.lines.iter().enumerate() {
            let line = std::str::from_utf8(record.full_range.slice(&text.bytes))
                .expect("classified as UTF-8");
            let Ok(regions) = highlighter.highlight_line(line, set) else {
                return HighlightOutcome::Failed;
            };
            let body_len = record.body_range.len();
            let mut cursor = 0usize;
            for (style, region) in regions {
                let start = cursor;
                cursor += region.len();
                // Clamp into the body; the CR/LF terminator is never styled.
                let end = cursor.min(body_len);
                if start >= end {
                    continue;
                }
                if Some(style.foreground) == default_fg {
                    continue;
                }
                let fg = SyntaxFg {
                    r: style.foreground.r,
                    g: style.foreground.g,
                    b: style.foreground.b,
                };
                match spans.last_mut() {
                    // syntect emits one region per token; adjacent same-color
                    // runs collapse so span storage stays proportional to
                    // color changes, not tokens.
                    Some(SyntaxLineSpan {
                        line: last_line,
                        range,
                        fg: last_fg,
                    }) if *last_line == LineIndex(index)
                        && *last_fg == fg
                        && range.end == start =>
                    {
                        *range = LineByteRange::new(range.start, end);
                    }
                    _ => spans.push(SyntaxLineSpan {
                        line: LineIndex(index),
                        range: LineByteRange::new(start, end),
                        fg,
                    }),
                }
            }
        }
        HighlightOutcome::Ready(Arc::new(SyntaxSpans { spans }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path::GitPath;
    use crate::text::{ClassifiedContent, classify};

    fn text(source: &str) -> TextContent {
        match classify(Arc::from(source.as_bytes())) {
            ClassifiedContent::Text(t) => t,
            ClassifiedContent::Binary(_) => panic!("fixture must be text"),
        }
    }

    fn hint(path: &[u8]) -> LanguagePathHint {
        LanguagePathHint::from_git_path(&GitPath::from_bytes(path))
    }

    fn assets() -> &'static HighlightAssets {
        use std::sync::OnceLock;
        static ASSETS: OnceLock<HighlightAssets> = OnceLock::new();
        ASSETS.get_or_init(HighlightAssets::load)
    }

    fn highlight(source: &str, path: &[u8]) -> Arc<SyntaxSpans> {
        match assets().highlight(&text(source), &hint(path), DEFAULT_THEME) {
            HighlightOutcome::Ready(spans) => spans,
            HighlightOutcome::UnsupportedLanguage => panic!("language must be supported"),
            HighlightOutcome::Failed => panic!("highlight must succeed"),
        }
    }

    /// The first span intersecting `needle` on its line, if any. Intersection
    /// rather than start-byte coverage: delimiters like an opening quote can
    /// legitimately render in the default color while the token body is
    /// colored.
    fn fg_at(spans: &SyntaxSpans, source: &str, needle: &str) -> Option<SyntaxFg> {
        let offset = source.find(needle).expect("needle must occur in source");
        let line_index = source[..offset].matches('\n').count();
        let line_start = source[..offset].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let start = offset - line_start;
        let end = start + needle.len();
        spans
            .spans_for_line(LineIndex(line_index))
            .iter()
            .find(|span| span.range.start < end && start < span.range.end)
            .map(|span| span.fg)
    }

    /// One representative-token fixture per supported language (the nine
    /// languages recorded in docs/syntax-highlight-engine.md). Each probe
    /// token must receive a non-default foreground.
    const LANG_FIXTURES: &[(&[u8], &str, &[&str])] = &[
        (
            b"a.rs",
            "fn greet(name: &str) -> String {\n    format!(\"hi {name}\") // note\n}\n",
            &["fn", "greet", "\"hi {name}\"", "// note"],
        ),
        (
            b"a.ts",
            "function greet(name: string): string {\n    return `hi`; // note\n}\n",
            &["function", "greet", "`hi`", "// note"],
        ),
        (
            b"a.py",
            "def greet(name):\n    return \"hi\"  # note\n",
            &["def", "greet", "\"hi\"", "# note"],
        ),
        (
            b"a.go",
            "func greet() string {\n    return \"hi\" // note\n}\n",
            &["func", "greet", "\"hi\"", "// note"],
        ),
        (
            b"a.nix",
            "{ pkgs }:\nlet name = \"hi\"; # note\nin { inherit name; }\n",
            &["let", "name", "\"hi\"", "# note"],
        ),
        // The `[package]` table name is scoped correctly but rendered in the
        // default foreground by the built-in theme, so the key probes it
        // instead.
        (
            b"a.toml",
            "[package]\nname = \"hi\" # note\nedition = 2024\n",
            &["name", "\"hi\"", "# note"],
        ),
        (
            b"a.yaml",
            "name: \"hi\" # note\nitems:\n  - one\n",
            &["name", "\"hi\"", "# note"],
        ),
        (
            b"a.md",
            "# Title\n\nsome *emphasis* and `code`.\n",
            &["# Title", "*emphasis*", "`code`"],
        ),
        (
            b"a.sh",
            "greet() {\n    echo \"hi\" # note\n}\n",
            &["greet", "echo", "\"hi\"", "# note"],
        ),
    ];

    #[test]
    fn representative_tokens_are_colored_in_all_nine_languages() {
        for (path, source, needles) in LANG_FIXTURES {
            let spans = highlight(source, path);
            for needle in *needles {
                assert!(
                    fg_at(&spans, source, needle).is_some(),
                    "{}: token {needle:?} must get a non-default foreground",
                    String::from_utf8_lossy(path),
                );
            }
        }
    }

    #[test]
    fn distinct_token_classes_get_distinct_colors() {
        let source = "fn greet(name: &str) -> String {\n    format!(\"hi {name}\") // note\n}\n";
        let spans = highlight(source, b"a.rs");
        let keyword = fg_at(&spans, source, "fn").expect("keyword colored");
        let string = fg_at(&spans, source, "\"hi {name}\"").expect("string colored");
        let comment = fg_at(&spans, source, "// note").expect("comment colored");
        assert_ne!(keyword, string);
        assert_ne!(string, comment);
        assert_ne!(keyword, comment);
    }

    #[test]
    fn spans_stay_inside_the_body_and_on_char_boundaries() {
        // CJK / emoji / combining char / tab / CRLF / no trailing newline.
        let fixtures: &[&str] = &[
            "let s = \"日本語テスト\"; // コメント\n",
            "let e = \"👩‍🔬🎌\"; // emoji\n",
            "let c = \"e\u{301}tude\"; // combining\n",
            "fn main() {\n\tlet tabbed = 1;\n}\n",
            "let crlf = 1; // cr\r\nlet next = 2;\r\n",
            "let no_newline = 1; // end",
        ];
        for source in fixtures {
            let content = text(source);
            let spans = match assets().highlight(&content, &hint(b"a.rs"), DEFAULT_THEME) {
                HighlightOutcome::Ready(spans) => spans,
                _ => panic!("rust fixture must highlight"),
            };
            for span in spans.spans() {
                let body = content
                    .line_body_str(span.line)
                    .expect("span line must exist");
                assert!(
                    span.range.end <= body.len(),
                    "span {span:?} exceeds body {body:?}"
                );
                assert!(span.range.start < span.range.end, "span must be non-empty");
                assert!(body.is_char_boundary(span.range.start));
                assert!(body.is_char_boundary(span.range.end));
            }
        }
    }

    #[test]
    fn spans_are_sorted_by_line_and_start() {
        let source = "fn a() { let x = \"s\"; } // c\nfn b() { let y = \"t\"; } // d\n";
        let spans = highlight(source, b"a.rs");
        let ordered = spans
            .spans()
            .windows(2)
            .all(|w| (w[0].line, w[0].range.start) <= (w[1].line, w[1].range.start));
        assert!(ordered);
    }

    #[test]
    fn unknown_extension_is_unsupported() {
        let content = text("plain text\n");
        assert!(matches!(
            assets().highlight(&content, &hint(b"a.zzznope"), DEFAULT_THEME),
            HighlightOutcome::UnsupportedLanguage
        ));
        assert!(matches!(
            assets().highlight(&content, &LanguagePathHint::none(), DEFAULT_THEME),
            HighlightOutcome::UnsupportedLanguage
        ));
    }

    #[test]
    fn basename_resolves_extensionless_files() {
        let content = text("all: build\n\nbuild:\n\tcargo build\n");
        assert!(matches!(
            assets().highlight(&content, &hint(b"Makefile"), DEFAULT_THEME),
            HighlightOutcome::Ready(_)
        ));
    }

    #[test]
    fn empty_file_yields_no_spans() {
        let content = text("");
        match assets().highlight(&content, &hint(b"a.rs"), DEFAULT_THEME) {
            HighlightOutcome::Ready(spans) => assert!(spans.spans().is_empty()),
            _ => panic!("empty rust file must highlight"),
        }
    }
}
