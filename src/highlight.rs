//! Syntect-backed syntax highlighting → egui colors.
//!
//! We highlight a line at a time (the diff is line-oriented and we render
//! line-by-line). True syntect highlighting is stateful across lines, but
//! per-line is a fine approximation for a v0.2 review surface and keeps the
//! cache model trivial. Upgrade to stateful per-file highlighting when the
//! full-file view needs multi-line constructs (block comments, etc.) to
//! color correctly.

use egui::Color32;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

pub struct Highlighter {
    syntaxes: SyntaxSet,
    theme: syntect::highlighting::Theme,
}

impl Highlighter {
    pub fn new() -> Self {
        let syntaxes = SyntaxSet::load_defaults_newlines();
        let themes = ThemeSet::load_defaults();
        let theme = themes.themes["base16-mocha.dark"].clone();
        Highlighter { syntaxes, theme }
    }

    fn syntax_for(&self, path: &str) -> &SyntaxReference {
        let ext = std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        self.syntaxes
            .find_syntax_by_extension(ext)
            .unwrap_or_else(|| self.syntaxes.find_syntax_plain_text())
    }

    /// Highlight one line into (color, text) spans. On any error, return the
    /// whole line in a neutral gray so rendering never panics.
    pub fn highlight_line(&self, path: &str, line: &str) -> Vec<(Color32, String)> {
        let syntax = self.syntax_for(path);
        let mut h = HighlightLines::new(syntax, &self.theme);
        match h.highlight_line(line, &self.syntaxes) {
            Ok(ranges) => ranges
                .into_iter()
                .map(|(style, text)| (to_color(style), text.to_string()))
                .collect(),
            Err(_) => vec![(Color32::GRAY, line.to_string())],
        }
    }
}

fn to_color(style: Style) -> Color32 {
    let c = style.foreground;
    Color32::from_rgb(c.r, c.g, c.b)
}
