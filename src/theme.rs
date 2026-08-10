//! Bundled color themes.
//!
//! A theme supplies the line-diff palette (row backgrounds, markers, chrome)
//! and the structural-emphasis palette, plus the name of the syntect theme
//! used for syntax foregrounds. The composition priority between the layers
//! is a contract, not a setting — themes only change colors.

use ratatui::style::Color;

use crate::structural::normalize::HighlightKind;
use crate::syntax::ThemeId;

/// Which bundled theme to render with.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ThemeChoice {
    #[default]
    Dark,
    Light,
}

/// One bundled theme's palette.
pub struct Theme {
    pub removed_bg: Color,
    pub added_bg: Color,
    /// Background of structurally emphasized runs on removed rows: a more
    /// pronounced shade of the row background so the emphasis stays in the
    /// same hue family instead of clashing with it.
    pub structural_removed_bg: Color,
    /// Background of structurally emphasized runs on added rows.
    pub structural_added_bg: Color,
    /// Background of structurally emphasized runs on context rows, where
    /// there is no row tint to harmonize with.
    pub structural_context_bg: Color,
    keyword_fg: Color,
    string_fg: Color,
    comment_fg: Color,
    delimiter_fg: Color,
    type_fg: Color,
    /// Foreground for emphasized runs without a token-kind color; must stay
    /// readable on all three structural backgrounds of its theme.
    other_fg: Color,
    pub sidebar_fg: Color,
    pub sidebar_selected_fg: Color,
    pub sidebar_selected_bg: Color,
    pub footer_fg: Color,
    /// The syntect theme rendering syntax foregrounds.
    pub syntax_theme: ThemeId,
}

impl Theme {
    /// Foreground for a structurally emphasized run of this kind.
    pub fn structural_fg(&self, kind: HighlightKind) -> Color {
        match kind {
            HighlightKind::Keyword => self.keyword_fg,
            HighlightKind::String => self.string_fg,
            HighlightKind::Comment => self.comment_fg,
            HighlightKind::Delimiter => self.delimiter_fg,
            HighlightKind::TypeName => self.type_fg,
            HighlightKind::Normal | HighlightKind::Other => self.other_fg,
        }
    }
}

static DARK: Theme = Theme {
    removed_bg: Color::Rgb(60, 20, 25),
    added_bg: Color::Rgb(15, 55, 35),
    structural_removed_bg: Color::Rgb(110, 40, 45),
    structural_added_bg: Color::Rgb(30, 100, 60),
    structural_context_bg: Color::Rgb(85, 65, 15),
    keyword_fg: Color::LightMagenta,
    string_fg: Color::LightYellow,
    comment_fg: Color::Gray,
    delimiter_fg: Color::LightCyan,
    type_fg: Color::LightBlue,
    other_fg: Color::Rgb(255, 255, 158),
    sidebar_fg: Color::DarkGray,
    sidebar_selected_fg: Color::Black,
    sidebar_selected_bg: Color::LightCyan,
    footer_fg: Color::DarkGray,
    syntax_theme: ThemeId(0),
};

static LIGHT: Theme = Theme {
    removed_bg: Color::Rgb(255, 220, 223),
    added_bg: Color::Rgb(214, 245, 214),
    structural_removed_bg: Color::Rgb(255, 175, 178),
    structural_added_bg: Color::Rgb(160, 235, 175),
    structural_context_bg: Color::Rgb(250, 236, 160),
    keyword_fg: Color::Rgb(150, 0, 150),
    string_fg: Color::Rgb(130, 90, 0),
    comment_fg: Color::Rgb(100, 100, 100),
    delimiter_fg: Color::Rgb(0, 110, 110),
    type_fg: Color::Rgb(0, 60, 180),
    other_fg: Color::Black,
    sidebar_fg: Color::Rgb(100, 100, 100),
    sidebar_selected_fg: Color::White,
    sidebar_selected_bg: Color::Rgb(0, 95, 135),
    footer_fg: Color::Rgb(100, 100, 100),
    syntax_theme: ThemeId(1),
};

pub fn theme(choice: ThemeChoice) -> &'static Theme {
    match choice {
        ThemeChoice::Dark => &DARK,
        ThemeChoice::Light => &LIGHT,
    }
}
