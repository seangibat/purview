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

/// The dark background purview renders the content pane against. `base16-mocha`
/// is very dark, so its lower-luminance scopes (some punctuation, comments —
/// the closing-paren the user noticed) come out nearly invisible. We floor every
/// token color's luminance against THIS bg so nothing renders illegibly dark
/// (see [`ensure_contrast`]).
const DARK_BG: Color32 = Color32::from_rgb(24, 24, 24);

fn to_color(style: Style) -> Color32 {
    let c = style.foreground;
    ensure_contrast(Color32::from_rgb(c.r, c.g, c.b), DARK_BG)
}

/// Relative luminance (WCAG sRGB) of a color in 0.0..=1.0. Used as the
/// perceived-brightness measure for the contrast floor.
fn relative_luminance(c: Color32) -> f32 {
    fn lin(ch: u8) -> f32 {
        let s = ch as f32 / 255.0;
        if s <= 0.03928 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
}

/// Lighten `fg` only if it's too dark to read against `bg`, preserving its hue.
///
/// A general fix for ALL low-contrast tokens (not just parens): if `fg`'s
/// luminance is below a floor relative to the background, blend it toward white
/// just enough to clear the floor; colors already bright enough are returned
/// UNCHANGED, so the palette isn't washed out. Hue is preserved because we
/// interpolate each channel toward white by the same factor (a tint), which
/// keeps the ratios between channels roughly intact while raising lightness.
fn ensure_contrast(fg: Color32, bg: Color32) -> Color32 {
    // Minimum acceptable foreground luminance above the background's. Tuned so
    // near-black tokens on the ~0.01-luminance mocha bg get lifted to a clearly
    // readable mid-gray, while anything already legible is left alone.
    const FLOOR: f32 = 0.18;
    let bg_lum = relative_luminance(bg);
    let fg_lum = relative_luminance(fg);
    let target = bg_lum + FLOOR;
    if fg_lum >= target {
        return fg; // already bright enough — don't touch it.
    }
    // Blend fg toward white by a factor `t` (a tint: each channel moves the same
    // fraction toward 255, so hue is preserved). Luminance is NON-linear in `t`
    // because of sRGB gamma, so we can't solve for `t` in closed form — we
    // binary-search the smallest `t` whose tinted color clears the floor. ~24
    // iterations is exact to well under one 8-bit step.
    let tint = |t: f32| -> Color32 {
        let mix = |ch: u8| -> u8 {
            (ch as f32 + t * (255.0 - ch as f32)).round().clamp(0.0, 255.0) as u8
        };
        Color32::from_rgb(mix(fg.r()), mix(fg.g()), mix(fg.b()))
    };
    let (mut lo, mut hi) = (0.0_f32, 1.0_f32);
    for _ in 0..24 {
        let mid = 0.5 * (lo + hi);
        if relative_luminance(tint(mid)) >= target {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    tint(hi)
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

    /// A near-black token against the dark bg must be lifted above the
    /// luminance floor so it's legible — the general fix for low-contrast
    /// punctuation (the closing-paren the user noticed).
    #[test]
    fn ensure_contrast_lifts_near_black_token() {
        let near_black = Color32::from_rgb(10, 10, 12);
        let lifted = ensure_contrast(near_black, DARK_BG);
        assert_ne!(lifted, near_black, "a near-black token must be lightened");
        let lum = relative_luminance(lifted);
        assert!(
            lum >= relative_luminance(DARK_BG) + 0.18 - 1e-3,
            "lifted luminance {lum} must clear the contrast floor"
        );
        // Each channel only ever moves toward white (lighter), never darker.
        assert!(lifted.r() >= near_black.r());
        assert!(lifted.g() >= near_black.g());
        assert!(lifted.b() >= near_black.b());
    }

    /// An already-bright color is returned UNCHANGED — the floor only lifts
    /// genuinely dark tokens, it doesn't wash out the palette.
    #[test]
    fn ensure_contrast_leaves_bright_color_unchanged() {
        let bright = Color32::from_rgb(220, 200, 120); // a normal syntax color
        assert_eq!(
            ensure_contrast(bright, DARK_BG),
            bright,
            "a bright, legible color must pass through untouched"
        );
        // White is trivially unchanged too.
        assert_eq!(ensure_contrast(Color32::WHITE, DARK_BG), Color32::WHITE);
    }

    /// Hue is roughly preserved: a dark-but-saturated token stays recognizably
    /// the same hue after lifting (we tint toward white, not recolor). The
    /// dominant channel before stays the dominant channel after.
    #[test]
    fn ensure_contrast_preserves_hue_ordering() {
        let dark_blue = Color32::from_rgb(10, 20, 60); // blue-dominant
        let lifted = ensure_contrast(dark_blue, DARK_BG);
        assert!(
            lifted.b() >= lifted.r() && lifted.b() >= lifted.g(),
            "blue should remain the dominant channel after lifting: {lifted:?}"
        );
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
