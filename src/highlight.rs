//! Syntect-backed syntax highlighting → egui colors.

use egui::Color32;
use syntect::easy::HighlightLines;
use syntect::highlighting::{
    HighlightIterator, HighlightState, Highlighter as SynHighlighter, Style, ThemeSet,
};
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};

pub type Spans = Vec<(Color32, String)>;

/// Carries syntect parser + highlight state across a file's lines so callers
/// can highlight incrementally (line by line, on demand) while still getting
/// correct cross-line coloring (block comments, multi-line strings). `next`
/// is the index of the next line to be highlighted — callers advance it.
pub struct IncrementalHl {
    parse: ParseState,
    hi: HighlightState,
    pub next: usize,
}

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

    /// Start incremental highlighting for `path`'s syntax. Feed lines in
    /// order to [`highlight_incremental`]; state carries across them.
    pub fn new_incremental(&self, path: &str) -> IncrementalHl {
        let syntax = self.syntax_for(path);
        let syn_hl = SynHighlighter::new(&self.theme);
        IncrementalHl {
            parse: ParseState::new(syntax),
            hi: HighlightState::new(&syn_hl, ScopeStack::new()),
            next: 0,
        }
    }

    /// Highlight the next line, advancing `st`'s parser state. Must be called
    /// in line order for correct results.
    pub fn highlight_incremental(&self, st: &mut IncrementalHl, line: &str) -> Spans {
        let syn_hl = SynHighlighter::new(&self.theme);
        let ops = match st.parse.parse_line(line, &self.syntaxes) {
            Ok(ops) => ops,
            Err(_) => return vec![(Color32::GRAY, line.to_string())],
        };
        HighlightIterator::new(&mut st.hi, &ops, line, &syn_hl)
            .map(|(style, text)| (to_color(style), text.to_string()))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Incremental highlighting (line-by-line, on demand) must produce the
    /// same result as the stateful one-pass `highlight_file` — including
    /// across a multi-line block comment, where naive per-line highlighting
    /// would mis-color the interior lines.
    fn src() -> &'static str {
        "fn a() {}\n\
         /* this block comment\n\
            spans several\n\
            lines */\n\
         fn b() {}\n"
    }

    #[test]
    fn incremental_matches_stateful_one_pass() {
        let hl = Highlighter::new();
        let reference = hl.highlight_file("x.rs", src().lines());

        let mut st = hl.new_incremental("x.rs");
        let incremental: Vec<Spans> = src()
            .lines()
            .map(|line| hl.highlight_incremental(&mut st, line))
            .collect();

        assert_eq!(incremental, reference, "incremental must equal stateful");
    }

    #[test]
    fn block_comment_interior_differs_from_naive_per_line() {
        // The middle comment line, highlighted with full state, should NOT
        // match highlighting it in isolation (proving cross-line state is
        // actually being carried — i.e. it's colored as a comment).
        let hl = Highlighter::new();
        let interior = "   spans several";
        let isolated = hl.highlight_line("x.rs", interior);

        let mut st = hl.new_incremental("x.rs");
        let mut stateful = Vec::new();
        for line in src().lines() {
            stateful.push(hl.highlight_incremental(&mut st, line));
        }
        // src()'s 3rd line (index 2) is the "spans several" interior line.
        assert_ne!(
            stateful[2], isolated,
            "interior comment line should color differently with carried state"
        );
    }
}
