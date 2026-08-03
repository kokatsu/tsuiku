//! User configuration: `$XDG_CONFIG_HOME/tsuiku/config.toml`.
//!
//! Configuration only overrides defaults — every feature works without a
//! file. Invalid input follows a fixed, case-by-case policy so a typo can
//! never take the viewer down:
//!
//! | case                | handling                                        |
//! |---------------------|-------------------------------------------------|
//! | TOML syntax error   | whole file rejected, warning with position,     |
//! |                     | all defaults                                     |
//! | number out of range | warning naming the key, clamped value            |
//! | unknown theme name  | warning, default theme, other settings kept      |
//! | unknown key         | warning, ignored, other settings kept            |
//!
//! Numeric settings are clamped because unlimited overrides could break the
//! responsiveness and subprocess-guard contracts: a config must not be able
//! to configure the viewer into unresponsiveness.

use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use toml::Spanned;

use crate::theme::ThemeChoice;

/// Initial diff-body layout.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ViewMode {
    #[default]
    Unified,
    Split,
}

/// Effective settings after defaults, file values, and clamping.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub theme: ThemeChoice,
    pub view: ViewMode,
    /// Terminal columns below which the sidebar is hidden.
    pub sidebar_min_width: u16,
    /// Diff-area columns below which split falls back to unified.
    pub split_min_width: u16,
    pub difft_timeout: Duration,
    pub structural_max_bytes: usize,
    pub structural_max_lines: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::Dark,
            view: ViewMode::Unified,
            sidebar_min_width: 72,
            split_min_width: 120,
            difft_timeout: Duration::from_secs(5),
            structural_max_bytes: 2 * 1024 * 1024,
            structural_max_lines: 5_000,
        }
    }
}

// Clamp bounds, recorded here as the single source of truth. Timeouts stay
// within the subprocess-guard contract; size guards may move a factor of
// four either way from their defaults; widths stay renderable.
const DIFFT_TIMEOUT_RANGE: (i64, i64) = (1, 30);
const STRUCTURAL_MAX_BYTES_RANGE: (i64, i64) = (512 * 1024, 8 * 1024 * 1024);
const STRUCTURAL_MAX_LINES_RANGE: (i64, i64) = (1_250, 20_000);
const SIDEBAR_MIN_WIDTH_RANGE: (i64, i64) = (48, 300);
const SPLIT_MIN_WIDTH_RANGE: (i64, i64) = (60, 400);

/// The parsed configuration plus everything worth telling the user.
#[derive(Debug, Default)]
pub struct LoadedConfig {
    pub config: Config,
    pub warnings: Vec<String>,
}

impl LoadedConfig {
    fn defaults() -> Self {
        Self::default()
    }
}

/// Load the configuration from the XDG location. A missing file is the
/// normal case and produces no warnings; an unreadable one is warned about.
pub fn load() -> LoadedConfig {
    let Some(path) = config_path() else {
        return LoadedConfig::defaults();
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => LoadedConfig::defaults(),
        Err(error) => {
            let mut loaded = LoadedConfig::defaults();
            loaded
                .warnings
                .push(format!("config: cannot read {}: {error}", path.display()));
            loaded
        }
    }
}

/// `$XDG_CONFIG_HOME/tsuiku/config.toml`, XDG-first even on macOS.
pub fn config_path() -> Option<PathBuf> {
    config_path_from(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
}

/// Only absolute environment values are honored (as the XDG spec requires):
/// tsuiku runs *inside* repositories, and a relative or empty `HOME` /
/// `XDG_CONFIG_HOME` would otherwise resolve against the current directory —
/// letting the repository being viewed supply the viewer's configuration.
fn config_path_from(
    xdg_config_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    let base = match xdg_config_home.map(PathBuf::from) {
        Some(explicit) if explicit.is_absolute() => explicit,
        _ => {
            let home = PathBuf::from(home?);
            if !home.is_absolute() {
                return None;
            }
            home.join(".config")
        }
    };
    Some(base.join("tsuiku").join("config.toml"))
}

/// Known keys as spanned raw values: `Spanned` keeps each value's byte
/// range for positioned warnings, and `toml::Value` keeps a wrong type
/// from failing the whole file.
#[derive(Deserialize, Default)]
#[serde(default)]
struct RawConfig {
    theme: Option<Spanned<toml::Value>>,
    view: Option<Spanned<toml::Value>>,
    sidebar_min_width: Option<Spanned<toml::Value>>,
    split_min_width: Option<Spanned<toml::Value>>,
    difft_timeout_seconds: Option<Spanned<toml::Value>>,
    structural_max_bytes: Option<Spanned<toml::Value>>,
    structural_max_lines: Option<Spanned<toml::Value>>,
}

const KNOWN_KEYS: [&str; 7] = [
    "theme",
    "view",
    "sidebar_min_width",
    "split_min_width",
    "difft_timeout_seconds",
    "structural_max_bytes",
    "structural_max_lines",
];

/// Parse one config file's text. Never fails: problems become warnings and
/// the affected setting keeps its default.
pub fn parse(text: &str) -> LoadedConfig {
    let mut loaded = LoadedConfig::defaults();
    // First pass enumerates keys for unknown-key warnings and turns a
    // syntax error into the all-defaults rejection — partial application
    // of a half-parsed file would be harder to reason about. The toml
    // error text carries line and column.
    let table = match text.parse::<toml::Table>() {
        Ok(table) => table,
        Err(error) => {
            loaded.warnings.push(format!("config: {error}"));
            return loaded;
        }
    };
    for key in table.keys() {
        if !KNOWN_KEYS.contains(&key.as_str()) {
            loaded
                .warnings
                .push(format!("config: unknown key `{key}` ignored"));
        }
    }
    // Second pass re-parses with value spans; it cannot fail where the
    // first succeeded, but a surprise degrades to defaults, never a crash.
    let raw: RawConfig = match toml::from_str(text) {
        Ok(raw) => raw,
        Err(error) => {
            loaded.warnings.push(format!("config: {error}"));
            return loaded;
        }
    };

    if let Some(value) = raw.theme {
        let at = position(text, &value);
        match value.get_ref().as_str() {
            Some("dark") => loaded.config.theme = ThemeChoice::Dark,
            Some("light") => loaded.config.theme = ThemeChoice::Light,
            Some(other) => loaded.warnings.push(format!(
                "config: unknown theme {other:?} at {at} (expected \"dark\" or \"light\"); using the default theme"
            )),
            None => loaded.warnings.push(format!(
                "config: `theme` must be a string ({at}); using the default theme"
            )),
        }
    }
    if let Some(value) = raw.view {
        let at = position(text, &value);
        match value.get_ref().as_str() {
            Some("unified") => loaded.config.view = ViewMode::Unified,
            Some("split") => loaded.config.view = ViewMode::Split,
            Some(other) => loaded.warnings.push(format!(
                "config: unknown view {other:?} at {at} (expected \"unified\" or \"split\"); using unified"
            )),
            None => loaded.warnings.push(format!(
                "config: `view` must be a string ({at}); using unified"
            )),
        }
    }
    if let Some(clamped) = clamp_integer(
        text,
        "sidebar_min_width",
        raw.sidebar_min_width,
        SIDEBAR_MIN_WIDTH_RANGE,
        &mut loaded.warnings,
    ) {
        loaded.config.sidebar_min_width = clamped as u16;
    }
    if let Some(clamped) = clamp_integer(
        text,
        "split_min_width",
        raw.split_min_width,
        SPLIT_MIN_WIDTH_RANGE,
        &mut loaded.warnings,
    ) {
        loaded.config.split_min_width = clamped as u16;
    }
    if let Some(clamped) = clamp_integer(
        text,
        "difft_timeout_seconds",
        raw.difft_timeout_seconds,
        DIFFT_TIMEOUT_RANGE,
        &mut loaded.warnings,
    ) {
        loaded.config.difft_timeout = Duration::from_secs(clamped as u64);
    }
    if let Some(clamped) = clamp_integer(
        text,
        "structural_max_bytes",
        raw.structural_max_bytes,
        STRUCTURAL_MAX_BYTES_RANGE,
        &mut loaded.warnings,
    ) {
        loaded.config.structural_max_bytes = clamped as usize;
    }
    if let Some(clamped) = clamp_integer(
        text,
        "structural_max_lines",
        raw.structural_max_lines,
        STRUCTURAL_MAX_LINES_RANGE,
        &mut loaded.warnings,
    ) {
        loaded.config.structural_max_lines = clamped as usize;
    }
    loaded
}

/// `line L, column C` (1-based) of a spanned value's start.
fn position(text: &str, value: &Spanned<toml::Value>) -> String {
    let offset = value.span().start.min(text.len());
    let line = text[..offset].bytes().filter(|&b| b == b'\n').count() + 1;
    let column = text[..offset]
        .rfind('\n')
        .map_or(offset + 1, |newline| offset - newline);
    format!("line {line}, column {column}")
}

/// An integer clamped into its documented range; out-of-range values warn
/// with their position and continue clamped, non-integers warn and keep
/// the default.
fn clamp_integer(
    text: &str,
    key: &str,
    value: Option<Spanned<toml::Value>>,
    (low, high): (i64, i64),
    warnings: &mut Vec<String>,
) -> Option<i64> {
    let value = value?;
    let at = position(text, &value);
    let Some(number) = value.get_ref().as_integer() else {
        warnings.push(format!(
            "config: `{key}` must be an integer ({at}); keeping the default"
        ));
        return None;
    };
    if number < low || number > high {
        let clamped = number.clamp(low, high);
        warnings.push(format!(
            "config: `{key}` = {number} at {at} is outside {low}..={high}; clamped to {clamped}"
        ));
        return Some(clamped);
    }
    Some(number)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_or_empty_env_never_resolves_into_the_working_directory() {
        let os = |s: &str| Some(std::ffi::OsString::from(s));

        // Empty or relative HOME must not become `./.config/...`.
        assert_eq!(config_path_from(None, os("")), None);
        assert_eq!(config_path_from(None, os("relative/home")), None);
        assert_eq!(config_path_from(None, None), None);

        // Empty or relative XDG_CONFIG_HOME falls back to an absolute HOME,
        // never into the current directory.
        assert_eq!(
            config_path_from(os("relative"), os("/home/user")),
            Some(PathBuf::from("/home/user/.config/tsuiku/config.toml"))
        );
        assert_eq!(
            config_path_from(os(""), os("/home/user")),
            Some(PathBuf::from("/home/user/.config/tsuiku/config.toml"))
        );
        // Both relative: no config at all.
        assert_eq!(config_path_from(os("relative"), os("also/relative")), None);

        // The absolute cases behave as documented.
        assert_eq!(
            config_path_from(os("/xdg"), os("/home/user")),
            Some(PathBuf::from("/xdg/tsuiku/config.toml"))
        );
    }

    #[test]
    fn an_empty_file_and_a_missing_file_are_all_defaults() {
        let loaded = parse("");
        assert_eq!(loaded.config, Config::default());
        assert!(loaded.warnings.is_empty());
    }

    #[test]
    fn a_full_valid_file_overrides_everything() {
        let loaded = parse(
            r#"
theme = "light"
view = "split"
sidebar_min_width = 80
split_min_width = 100
difft_timeout_seconds = 10
structural_max_bytes = 1048576
structural_max_lines = 2500
"#,
        );
        assert!(loaded.warnings.is_empty());
        assert_eq!(loaded.config.theme, ThemeChoice::Light);
        assert_eq!(loaded.config.view, ViewMode::Split);
        assert_eq!(loaded.config.sidebar_min_width, 80);
        assert_eq!(loaded.config.split_min_width, 100);
        assert_eq!(loaded.config.difft_timeout, Duration::from_secs(10));
        assert_eq!(loaded.config.structural_max_bytes, 1048576);
        assert_eq!(loaded.config.structural_max_lines, 2500);
    }

    #[test]
    fn a_syntax_error_rejects_the_whole_file_with_a_positioned_warning() {
        let loaded = parse("theme = \"light\"\nview = [broken\n");
        assert_eq!(
            loaded.config,
            Config::default(),
            "a half-parsed file must not be applied partially"
        );
        assert_eq!(loaded.warnings.len(), 1);
        assert!(
            loaded.warnings[0].contains("line 2"),
            "the warning must carry the position, got {:?}",
            loaded.warnings[0]
        );
    }

    #[test]
    fn out_of_range_numbers_warn_with_their_position_and_clamp() {
        let loaded = parse("difft_timeout_seconds = 3600\nstructural_max_lines = 1\n");
        assert_eq!(loaded.config.difft_timeout, Duration::from_secs(30));
        assert_eq!(loaded.config.structural_max_lines, 1_250);
        assert_eq!(loaded.warnings.len(), 2);
        assert!(loaded.warnings[0].contains("difft_timeout_seconds"));
        assert!(loaded.warnings[0].contains("clamped to 30"));
        assert!(
            loaded.warnings[0].contains("line 1, column 25"),
            "the range warning must carry the value's position, got {:?}",
            loaded.warnings[0]
        );
        assert!(loaded.warnings[1].contains("structural_max_lines"));
        assert!(
            loaded.warnings[1].contains("line 2, column 24"),
            "got {:?}",
            loaded.warnings[1]
        );
    }

    #[test]
    fn warnings_may_carry_raw_key_bytes_the_display_layer_must_escape() {
        // TOML quoted keys legally contain escapes; the parser reports them
        // verbatim and printing them safely is the display layer's job.
        let loaded = parse("\"evil\\u001b[31mkey\" = 1\n");
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].contains('\u{1b}'));
    }

    #[test]
    fn an_unknown_theme_warns_and_keeps_the_other_settings() {
        let loaded = parse("theme = \"solarized\"\nview = \"split\"\n");
        assert_eq!(loaded.config.theme, ThemeChoice::Dark);
        assert_eq!(
            loaded.config.view,
            ViewMode::Split,
            "other settings stay effective"
        );
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].contains("solarized"));
    }

    #[test]
    fn an_unknown_key_warns_and_is_ignored() {
        let loaded = parse("keybindings = \"vi\"\ntheme = \"light\"\n");
        assert_eq!(loaded.config.theme, ThemeChoice::Light);
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].contains("keybindings"));
    }

    #[test]
    fn a_wrong_type_warns_and_keeps_the_default() {
        let loaded = parse("split_min_width = \"wide\"\n");
        assert_eq!(
            loaded.config.split_min_width,
            Config::default().split_min_width
        );
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].contains("split_min_width"));
    }
}
