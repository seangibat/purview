//! Syntect-backed syntax highlighting → egui colors.

use egui::Color32;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

pub type Spans = Vec<(Color32, String)>;

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

    /// Highlight a whole file's lines in one **stateful** pass: a single
    /// `HighlightLines` carries parser state across lines, so block comments
    /// and multi-line strings color correctly — and we pay the parser setup
    /// cost once per file, not once per line. Returns one span-vec per line.
    pub fn highlight_file<'a>(
        &self,
        path: &str,
        lines: impl Iterator<Item = &'a str>,
    ) -> Vec<Spans> {
        let syntax = self.syntax_for(path);
        let mut h = HighlightLines::new(syntax, &self.theme);
        lines
            .map(|line| match h.highlight_line(line, &self.syntaxes) {
                Ok(ranges) => ranges
                    .into_iter()
                    .map(|(style, text)| (to_color(style), text.to_string()))
                    .collect(),
                Err(_) => vec![(Color32::GRAY, line.to_string())],
            })
            .collect()
    }

    /// Highlight a single isolated line (diff rows aren't a contiguous file,
    /// so each is highlighted independently). On error, neutral gray.
    pub fn highlight_line(&self, path: &str, line: &str) -> Spans {
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
