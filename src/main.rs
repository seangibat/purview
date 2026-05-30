//! purview — codebase-first code review.
//!
//! v0.2: working-tree-vs-HEAD diff with a left changed-files list and a
//! right content pane that toggles between Diff and Full File views, both
//! syntax-highlighted (syntect). Rows are virtualized so the monorepo's
//! giant files stay snappy. Header shows the repo + current branch.
//!
//! Still ahead: branch-range base selection, nested file tree, per-chunk
//! approve/deny review state, comments, symbol jump, the Claude agent pane.

use std::path::PathBuf;

use eframe::egui;
use egui::Color32;
use git2::Repository;

use purview::diff::{self, ChangedFile, DiffSource, Hunk, LineKind, ReviewStatus};
use purview::highlight::{Highlighter, IncrementalHl, Spans};
use purview::review_state::{FileState, HunkState, Replies, ReviewState};
use purview::tree::{self, FileTree, Node};

fn main() -> eframe::Result<()> {
    let repo_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap());

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 840.0])
            .with_title("purview"),
        ..Default::default()
    };

    eframe::run_native(
        "purview",
        native_options,
        Box::new(move |_cc| Ok(Box::new(App::new(repo_path)))),
    )
}

/// Ctrl+P fuzzy file-open overlay.
struct QuickOpen {
    query: String,
    /// All repo file paths (collected once when the overlay opens).
    all: Vec<String>,
    truncated: bool,
    /// Currently highlighted match index (into the filtered list).
    sel: usize,
    /// True for the first frame so we can focus the text field.
    just_opened: bool,
}

/// How the diff is laid out.
#[derive(Clone, Copy, PartialEq)]
enum Layout {
    /// Unified: +/- in one column.
    Inline,
    /// Side-by-side: old (left) vs new (right).
    Split,
}

/// How much of the file is shown.
#[derive(Clone, Copy, PartialEq)]
enum Extent {
    /// Just the changed hunks + a few lines of context.
    Summary,
    /// The whole file, with the diff overlaid (every unchanged line as context).
    Full,
}

/// Current selection: either a changed file (diff-able) or an arbitrary
/// repo file opened from the tree (full-file only).
#[derive(Clone, PartialEq)]
enum Selection {
    Changed(usize),
    Path(String),
}

/// One row in the content pane. Holds RAW text; syntax highlighting is
/// applied lazily, only for rows actually scrolled into view (see
/// `hl_cache`). This keeps opening a 50k-line file from stalling — we
/// highlight ~the visible window, not the whole file.
enum RenderRow {
    /// A hunk boundary in diff view. Carries the hunk index so the row can
    /// draw approve/deny controls bound to that hunk's live status.
    HunkHeader { hunk_idx: usize, text: String },
    /// A diff content line (add/del/ctx), raw text.
    DiffLine { kind: LineKind, text: String },
    /// A full-file content line, raw text.
    Plain { text: String },
    /// A side-by-side row: a cell on each side, either of which may be empty
    /// (a deletion has no right cell; an addition has no left cell; context
    /// shows on both). Highlighted lazily via `split_spans`.
    SplitLine {
        left: Option<(LineKind, String)>,
        right: Option<(LineKind, String)>,
    },
}

struct App {
    repo_path: PathBuf,
    branch: String,
    base: String,
    base_input: String,
    source: DiffSource,
    files: Vec<ChangedFile>,
    selected: Option<Selection>,
    /// Hunk index whose comment editor is open in the bottom panel.
    active_hunk: Option<usize>,
    layout: Layout,
    extent: Extent,
    tree: FileTree,
    /// Ctrl+P fuzzy file-open overlay state. Some = open.
    quick_open: Option<QuickOpen>,
    error: Option<String>,
    report_note: String,
    hl: Highlighter,
    /// Bumped on every reload so the render cache can't serve content from a
    /// previous `self.files` under a value-equal Selection index.
    generation: u64,
    /// Raw render rows for the current selection/view.
    cache: Vec<RenderRow>,
    cache_key: Option<(u64, Selection, Layout, Extent)>,
    /// Syntax-highlight path for the cached rows (the selected file's path).
    cache_path: String,
    /// Lazy, per-row memoized highlight spans, parallel to `cache`. None =
    /// not yet highlighted. Interior mutability so the render closure (which
    /// borrows `&self`) can fill in newly-visible rows. egui is single-thread.
    hl_cache: std::cell::RefCell<Vec<Option<Vec<(Color32, String)>>>>,
    /// Incremental highlighter for the full-file view: carries parser state
    /// across lines so block comments etc. color correctly, while only
    /// advancing as far as the user has scrolled. None for diff view (its
    /// rows aren't contiguous source — per-line highlighting is correct).
    incr: std::cell::RefCell<Option<IncrementalHl>>,
    /// Lazy memo for split-view rows: (left_spans, right_spans) per cache row.
    split_cache: std::cell::RefCell<Vec<Option<(Spans, Spans)>>>,
    /// Keyboard-nav focus: which hunk (by hunk index) is "current" for n/p
    /// navigation and a/r/c actions.
    focus_hunk: usize,
    /// Cache-row index of each hunk's header row, so n/p can scroll to it.
    hunk_rows: Vec<usize>,
    /// Set when a key-nav action wants the content scroll area moved to a
    /// specific vertical offset on the next frame.
    pending_scroll: Option<f32>,
}

impl App {
    fn new(repo_path: PathBuf) -> Self {
        let tree_root = Repository::discover(&repo_path)
            .ok()
            .and_then(|r| r.workdir().map(|w| w.to_path_buf()))
            .unwrap_or_else(|| repo_path.clone());
        let mut app = App {
            repo_path,
            branch: String::new(),
            base: String::new(),
            base_input: String::new(),
            source: DiffSource::WorkingTree,
            files: Vec::new(),
            selected: None,
            active_hunk: None,
            layout: Layout::Inline,
            extent: Extent::Summary,
            tree: FileTree::new(tree_root),
            quick_open: None,
            error: None,
            report_note: String::new(),
            hl: Highlighter::new(),
            generation: 0,
            cache: Vec::new(),
            cache_key: None,
            cache_path: String::new(),
            hl_cache: std::cell::RefCell::new(Vec::new()),
            incr: std::cell::RefCell::new(None),
            split_cache: std::cell::RefCell::new(Vec::new()),
            focus_hunk: 0,
            hunk_rows: Vec::new(),
            pending_scroll: None,
        };
        // Guess a sensible default base for branch-range mode.
        app.base = app.guess_default_base();
        app.base_input = app.base.clone();
        app.reload();
        app
    }

    /// Pick a default base branch: first of main / master / develop / trunk
    /// that resolves in the repo, else "main".
    fn guess_default_base(&self) -> String {
        if let Ok(repo) = Repository::discover(&self.repo_path) {
            for cand in ["main", "master", "develop", "trunk"] {
                if repo.revparse_single(cand).is_ok() {
                    return cand.to_string();
                }
            }
        }
        "main".to_string()
    }

    fn reload(&mut self) {
        self.files.clear();
        self.selected = None;
        self.active_hunk = None;
        self.error = None;
        self.report_note.clear();
        self.cache.clear();
        self.cache_key = None;
        self.generation = self.generation.wrapping_add(1);

        match self.compute_diff() {
            Ok((branch, files)) => {
                self.branch = branch;
                self.files = files;
                if !self.files.is_empty() {
                    self.selected = Some(Selection::Changed(0));
                }
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    fn compute_diff(&self) -> Result<(String, Vec<ChangedFile>), git2::Error> {
        diff::compute(&self.repo_path, self.source, &self.base)
    }

    /// (reviewed, total) hunks across all changed files.
    fn review_totals(&self) -> (usize, usize) {
        self.files.iter().fold((0, 0), |(r, t), f| {
            let (fr, ft) = f.progress();
            (r + fr, t + ft)
        })
    }

    /// Build a markdown review report — the artifact to hand to Claude.
    /// Summarizes counts, then lists rejected and unreviewed hunks (the
    /// things that need attention) per file.
    fn review_report(&self) -> String {
        let (rev, tot) = self.review_totals();
        let approved = self.count_status(ReviewStatus::Approved);
        let rejected = self.count_status(ReviewStatus::Rejected);
        let mut s = String::new();
        s.push_str(&format!("# Review report — {}\n\n", self.branch));
        let range = match self.source {
            DiffSource::WorkingTree => "working tree vs HEAD".to_string(),
            DiffSource::BranchRange => format!("{}...HEAD", self.base),
        };
        s.push_str(&format!("Range: {range}\n\n"));
        s.push_str(&format!(
            "Progress: {rev}/{tot} hunks reviewed — {approved} approved, {rejected} rejected, {} unreviewed.\n\n",
            tot.saturating_sub(rev)
        ));

        let mut wrote_rejected = false;
        for f in &self.files {
            let rej: Vec<&Hunk> = f
                .hunks
                .iter()
                .filter(|h| h.status == ReviewStatus::Rejected)
                .collect();
            if rej.is_empty() {
                continue;
            }
            if !wrote_rejected {
                s.push_str("## Rejected hunks (need changes)\n\n");
                wrote_rejected = true;
            }
            s.push_str(&format!("### {}\n\n", f.path));
            for h in rej {
                s.push_str(&format!("- `{}`\n", h.header.trim()));
                // Carry the reviewer's comment so a pasted report has the
                // "why", not just which hunk — parity with the MCP path.
                if !h.comment.trim().is_empty() {
                    for line in h.comment.trim().lines() {
                        s.push_str(&format!("  - {line}\n"));
                    }
                }
            }
            s.push('\n');
        }

        let mut wrote_unrev = false;
        for f in &self.files {
            let un: Vec<&Hunk> = f
                .hunks
                .iter()
                .filter(|h| h.status == ReviewStatus::Unreviewed)
                .collect();
            if un.is_empty() {
                continue;
            }
            if !wrote_unrev {
                s.push_str("## Still unreviewed\n\n");
                wrote_unrev = true;
            }
            s.push_str(&format!("### {}\n\n", f.path));
            for h in un {
                s.push_str(&format!("- `{}`\n", h.header.trim()));
            }
            s.push('\n');
        }

        if !wrote_rejected && !wrote_unrev {
            s.push_str("All hunks approved. ✓\n");
        }
        s
    }

    /// Serialize current review state to <repo>/.purview/review-state.json
    /// so the MCP server can read it. Called whenever status changes.
    fn save_review_state(&self) {
        let range = match self.source {
            DiffSource::WorkingTree => "working tree vs HEAD".to_string(),
            DiffSource::BranchRange => format!("{}...HEAD", self.base),
        };
        let state = ReviewState {
            branch: self.branch.clone(),
            range,
            files: self
                .files
                .iter()
                .map(|f| FileState {
                    path: f.path.clone(),
                    hunks: f
                        .hunks
                        .iter()
                        .map(|h| HunkState {
                            header: h.header.clone(),
                            status: match h.status {
                                ReviewStatus::Approved => "approved",
                                ReviewStatus::Rejected => "rejected",
                                ReviewStatus::Unreviewed => "unreviewed",
                            }
                            .to_string(),
                            comment: if h.comment.trim().is_empty() {
                                None
                            } else {
                                Some(h.comment.clone())
                            },
                        })
                        .collect(),
                })
                .collect(),
        };
        let _ = state.save(&self.tree.root);
    }

    fn count_status(&self, status: ReviewStatus) -> usize {
        self.files
            .iter()
            .flat_map(|f| f.hunks.iter())
            .filter(|h| h.status == status)
            .count()
    }

    /// Write the report to <repo>/.purview/review-report.md; return its path.
    fn write_report(&self) -> std::io::Result<PathBuf> {
        let dir = self.tree.root.join(".purview");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("review-report.md");
        std::fs::write(&path, self.review_report())?;
        Ok(path)
    }

    /// Read the full working-tree file for the selected path. The repo
    /// workdir is already known (tree.root) — no need to re-discover.
    fn read_full_file(&self, rel: &str) -> std::io::Result<String> {
        std::fs::read_to_string(self.tree.root.join(rel))
    }

    /// Rebuild the (raw, un-highlighted) render cache if selection/view
    /// changed. Highlighting happens lazily per visible row at draw time —
    /// this stays O(rows) with no syntect work, so switching to a giant file
    /// is instant.
    fn ensure_cache(&mut self) {
        let Some(sel) = self.selected.clone() else {
            self.cache.clear();
            self.cache_key = None;
            self.hl_cache.borrow_mut().clear();
            return;
        };
        let key = (self.generation, sel.clone(), self.layout, self.extent);
        if self.cache_key.as_ref() == Some(&key) {
            return;
        }

        let mut out: Vec<RenderRow> = Vec::new();
        let path: String;
        // `plain` = unchanged file opened from the tree (no diff). Highlighted
        // incrementally-stateful; everything else (a diff) is per-line.
        let mut plain = false;

        match &sel {
            Selection::Changed(idx) => {
                path = self.files[*idx].path.clone();
                // Extent::Full re-diffs this one file with full context (the
                // whole file shown, changes overlaid). Summary uses the
                // already-computed 3-line-context hunks.
                let hunks: Vec<diff::Hunk> = if self.extent == Extent::Full {
                    diff::compute_with(
                        &self.repo_path,
                        self.source,
                        &self.base,
                        u32::MAX,
                        Some(&path),
                    )
                    .ok()
                    .and_then(|(_, mut files)| {
                        files
                            .iter()
                            .position(|f| f.path == path)
                            .map(|i| std::mem::take(&mut files[i].hunks))
                    })
                    .unwrap_or_else(|| self.files[*idx].hunks.clone())
                } else {
                    self.files[*idx].hunks.clone()
                };

                for (hunk_idx, hunk) in hunks.iter().enumerate() {
                    // In Full extent the single hunk spans the file; its header
                    // is noise, so only show headers in Summary extent.
                    if self.extent == Extent::Summary {
                        out.push(RenderRow::HunkHeader {
                            hunk_idx,
                            text: hunk.header.clone(),
                        });
                    } else if hunk_idx == 0 {
                        // One header carrying the hunk controls for the file.
                        out.push(RenderRow::HunkHeader {
                            hunk_idx,
                            text: String::new(),
                        });
                    }
                    match self.layout {
                        Layout::Inline => {
                            for r in &hunk.rows {
                                out.push(RenderRow::DiffLine {
                                    kind: r.kind,
                                    text: r.text.clone(),
                                });
                            }
                        }
                        Layout::Split => out.extend(split_align(&hunk.rows)),
                    }
                }
            }
            Selection::Path(p) => {
                // Unchanged file from the tree — just show it whole.
                path = p.clone();
                plain = true;
                match self.read_full_file(&path) {
                    Ok(content) => {
                        for line in content.lines() {
                            out.push(RenderRow::Plain { text: line.to_string() });
                        }
                    }
                    Err(e) => out.push(RenderRow::Plain {
                        text: format!("cannot read file: {e}"),
                    }),
                }
            }
        }

        let n = out.len();
        // Record each hunk header's row index (for n/p scroll-to nav).
        self.hunk_rows = out
            .iter()
            .enumerate()
            .filter(|(_, r)| matches!(r, RenderRow::HunkHeader { .. }))
            .map(|(i, _)| i)
            .collect();
        if self.focus_hunk >= self.hunk_rows.len() {
            self.focus_hunk = 0;
        }
        self.cache = out;
        self.cache_path = path;
        self.cache_key = Some(key);
        *self.hl_cache.borrow_mut() = vec![None; n];
        *self.split_cache.borrow_mut() = vec![None; n];
        *self.incr.borrow_mut() = if plain {
            Some(self.hl.new_incremental(&self.cache_path))
        } else {
            None
        };
    }

    /// Highlighted spans for cache row `i`, computed once and memoized.
    /// Diff rows are highlighted per-line (they're not contiguous source).
    /// Full-file rows are highlighted via the incremental stateful path,
    /// advancing from the last-highlighted line up to `i` so cross-line
    /// constructs color correctly — and never past what's been viewed.
    fn row_spans(&self, i: usize) -> Vec<(Color32, String)> {
        if let Some(spans) = &self.hl_cache.borrow()[i] {
            return spans.clone();
        }
        match &self.cache[i] {
            RenderRow::HunkHeader { .. } => Vec::new(),
            RenderRow::DiffLine { text, .. } => {
                let spans = self.hl.highlight_line(&self.cache_path, text);
                self.hl_cache.borrow_mut()[i] = Some(spans.clone());
                spans
            }
            RenderRow::SplitLine { .. } => Vec::new(),
            RenderRow::Plain { .. } => self.highlight_full_file_upto(i),
        }
    }

    /// Lazily highlighted (left, right) spans for a split-view row `i`,
    /// memoized. Each side is highlighted per-line (diff fragments aren't
    /// contiguous source).
    fn split_spans(&self, i: usize) -> (Spans, Spans) {
        if let Some(pair) = &self.split_cache.borrow()[i] {
            return pair.clone();
        }
        let (left, right) = match &self.cache[i] {
            RenderRow::SplitLine { left, right } => {
                let l = left
                    .as_ref()
                    .map(|(_, t)| self.hl.highlight_line(&self.cache_path, t))
                    .unwrap_or_default();
                let r = right
                    .as_ref()
                    .map(|(_, t)| self.hl.highlight_line(&self.cache_path, t))
                    .unwrap_or_default();
                (l, r)
            }
            _ => (Vec::new(), Vec::new()),
        };
        self.split_cache.borrow_mut()[i] = Some((left.clone(), right.clone()));
        (left, right)
    }

    /// Advance the incremental highlighter through rows [next..=i], caching
    /// each, then return row `i`'s spans.
    fn highlight_full_file_upto(&self, i: usize) -> Vec<(Color32, String)> {
        let mut incr = self.incr.borrow_mut();
        let st = incr.get_or_insert_with(|| self.hl.new_incremental(&self.cache_path));
        let mut hl = self.hl_cache.borrow_mut();
        while st.next <= i {
            let n = st.next;
            let text = match &self.cache[n] {
                RenderRow::Plain { text } => text.as_str(),
                _ => "",
            };
            let spans = self.hl.highlight_incremental(st, text);
            hl[n] = Some(spans);
            st.next += 1;
        }
        hl[i].clone().unwrap_or_default()
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.ui(ctx);
    }
}

impl App {
    /// All rendering for one frame. Split out of `eframe::App::update` (which
    /// only forwards here) so tests can drive a frame with a bare
    /// `egui::Context`, no `eframe::Frame` required.
    /// Render the Ctrl+P fuzzy file-open overlay if active. Esc closes;
    /// ↑/↓ move the selection; Enter opens the highlighted file (full-file
    /// view via Selection::Path).
    fn quick_open_overlay(&mut self, ctx: &egui::Context) {
        let Some(qo) = self.quick_open.as_mut() else { return };

        // Keyboard: Esc / Enter / Up / Down (read before the text field eats them).
        let (esc, enter, up, down) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Escape),
                i.key_pressed(egui::Key::Enter),
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::ArrowDown),
            )
        });
        if esc {
            self.quick_open = None;
            return;
        }

        // Filter + rank: fuzzy score asc, then shorter path as tiebreak.
        let mut scored: Vec<(i64, &String)> = qo
            .all
            .iter()
            .filter_map(|p| tree::fuzzy_score(&qo.query, p).map(|s| (s, p)))
            .collect();
        scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.len().cmp(&b.1.len())));
        let matches: Vec<&String> = scored.iter().take(200).map(|(_, p)| *p).collect();

        if down {
            qo.sel = (qo.sel + 1).min(matches.len().saturating_sub(1));
        }
        if up {
            qo.sel = qo.sel.saturating_sub(1);
        }
        if qo.sel >= matches.len() {
            qo.sel = matches.len().saturating_sub(1);
        }

        let mut open_path: Option<String> = None;
        if enter {
            if let Some(p) = matches.get(qo.sel) {
                open_path = Some((*p).clone());
            }
        }

        egui::Window::new("Open file")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_TOP, [0.0, 80.0])
            .fixed_size([640.0, 420.0])
            .show(ctx, |ui| {
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut qo.query)
                        .hint_text("fuzzy file search…")
                        .desired_width(f32::INFINITY),
                );
                if qo.just_opened {
                    resp.request_focus();
                    qo.just_opened = false;
                } else {
                    // Keep focus so typing always lands here.
                    resp.request_focus();
                }
                if qo.truncated {
                    ui.label(
                        egui::RichText::new("(file list truncated at 50k)")
                            .small()
                            .weak(),
                    );
                }
                ui.separator();
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    for (i, p) in matches.iter().enumerate() {
                        let selected = i == qo.sel;
                        if ui.selectable_label(selected, *p).clicked() {
                            open_path = Some((*p).clone());
                        }
                    }
                });
            });

        if let Some(p) = open_path {
            self.selected = Some(Selection::Path(p));
            self.quick_open = None;
        }
    }

    /// Modal keyboard navigation (Gerrit-style). Suppressed while the
    /// quick-open overlay is up or a text field has keyboard focus (so
    /// typing in the comment box / base field isn't hijacked).
    ///   j / k        next / prev changed file
    ///   n / p        next / prev hunk (scrolls to it)
    ///   a / r        approve / reject the focused hunk
    ///   c            open the comment editor for the focused hunk
    fn handle_nav_keys(&mut self, ctx: &egui::Context) {
        if self.quick_open.is_some() || ctx.wants_keyboard_input() {
            return;
        }
        let (j, k, n, p, a, r, c) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::J),
                i.key_pressed(egui::Key::K),
                i.key_pressed(egui::Key::N),
                i.key_pressed(egui::Key::P),
                i.key_pressed(egui::Key::A),
                i.key_pressed(egui::Key::R),
                i.key_pressed(egui::Key::C),
            )
        });

        // j/k: move through the changed-files list.
        let cur_file = match self.selected {
            Some(Selection::Changed(i)) => Some(i),
            _ => None,
        };
        if (j || k) && !self.files.is_empty() {
            let i = cur_file.unwrap_or(0);
            let next = if j {
                (i + 1).min(self.files.len() - 1)
            } else {
                i.saturating_sub(1)
            };
            self.selected = Some(Selection::Changed(next));
            self.focus_hunk = 0;
        }

        // n/p: move the focused hunk + scroll to it.
        if (n || p) && !self.hunk_rows.is_empty() {
            if n {
                self.focus_hunk = (self.focus_hunk + 1).min(self.hunk_rows.len() - 1);
            } else {
                self.focus_hunk = self.focus_hunk.saturating_sub(1);
            }
            let row_h = ctx.style().text_styles[&egui::TextStyle::Monospace].size + 3.0;
            let row = self.hunk_rows.get(self.focus_hunk).copied().unwrap_or(0);
            self.pending_scroll = Some(row as f32 * row_h);
        }

        // a/r/c: act on the focused hunk (only meaningful for a changed file).
        if let Some(fi) = cur_file {
            let set = |app: &mut App, status: ReviewStatus| {
                if let Some(h) = app.files[fi].hunks.get_mut(app.focus_hunk) {
                    h.status = status;
                }
                app.save_review_state();
            };
            if a {
                set(self, ReviewStatus::Approved);
            } else if r {
                set(self, ReviewStatus::Rejected);
            } else if c {
                self.active_hunk = Some(self.focus_hunk);
            }
        }
    }

    fn ui(&mut self, ctx: &egui::Context) {
        // Ctrl+P opens the fuzzy file finder. (Cmd+P on mac.)
        let toggle_qo = ctx.input(|i| {
            i.key_pressed(egui::Key::P) && (i.modifiers.ctrl || i.modifiers.command)
        });
        if toggle_qo {
            if self.quick_open.is_some() {
                self.quick_open = None;
            } else {
                let (all, truncated) = tree::collect_files(&self.tree.root, 50_000);
                self.quick_open = Some(QuickOpen {
                    query: String::new(),
                    all,
                    truncated,
                    sel: 0,
                    just_opened: true,
                });
            }
        }
        self.quick_open_overlay(ctx);
        self.handle_nav_keys(ctx);

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("purview");
                ui.separator();
                ui.label(format!("repo: {}", self.repo_path.to_string_lossy()));
                ui.separator();
                ui.label(format!("branch: {}", self.branch));
                ui.separator();
                let (rev, tot) = self.review_totals();
                ui.label(format!("reviewed: {rev}/{tot} hunks"));
                ui.separator();
                ui.label("session: (none)");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("⟳").clicked() {
                        self.reload();
                    }
                    ui.separator();
                    // Two orthogonal controls. (right_to_left, so added order
                    // is the visual reverse.)
                    ui.selectable_value(&mut self.extent, Extent::Full, "Full");
                    ui.selectable_value(&mut self.extent, Extent::Summary, "Summary");
                    ui.label("extent:");
                    ui.separator();
                    ui.selectable_value(&mut self.layout, Layout::Split, "Split");
                    ui.selectable_value(&mut self.layout, Layout::Inline, "Inline");
                    ui.label("layout:");
                    ui.separator();
                    if ui.button("report").clicked() {
                        match self.write_report() {
                            Ok(p) => {
                                ui.ctx().copy_text(self.review_report());
                                self.report_note =
                                    format!("report → {} (also copied)", p.display());
                            }
                            Err(e) => self.report_note = format!("report failed: {e}"),
                        }
                    }
                    if !self.report_note.is_empty() {
                        ui.label(egui::RichText::new(&self.report_note).small().weak());
                    }
                });
            });
            ui.horizontal(|ui| {
                ui.label("source:");
                let mut changed = false;
                changed |= ui
                    .selectable_value(&mut self.source, DiffSource::WorkingTree, "Working Tree")
                    .changed();
                changed |= ui
                    .selectable_value(&mut self.source, DiffSource::BranchRange, "Branch Range")
                    .changed();
                if self.source == DiffSource::BranchRange {
                    ui.separator();
                    ui.label("base:");
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.base_input)
                            .desired_width(140.0)
                            .hint_text("main"),
                    );
                    let apply = ui.button("apply").clicked()
                        || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                    if apply {
                        self.base = self.base_input.trim().to_string();
                        changed = true;
                    }
                    ui.weak(format!("{}...HEAD", self.base));
                }
                if changed {
                    self.reload();
                }
            });
        });

        egui::SidePanel::left("files")
            .resizable(true)
            .default_width(320.0)
            .show(ctx, |ui| {
                // Top: changed-files list.
                ui.add_space(4.0);
                ui.label(egui::RichText::new(format!("Changed ({})", self.files.len())).strong());
                if let Some(err) = &self.error {
                    ui.colored_label(Color32::LIGHT_RED, err);
                }
                let changed_h = (ui.available_height() * 0.45).max(80.0);
                egui::ScrollArea::vertical()
                    .id_salt("changed")
                    .max_height(changed_h)
                    .show(ui, |ui| {
                        for i in 0..self.files.len() {
                            let selected = self.selected == Some(Selection::Changed(i));
                            let (reviewed, total) = self.files[i].progress();
                            let glyph = if total > 0 && reviewed == total {
                                "✓"
                            } else if reviewed > 0 {
                                "◐"
                            } else {
                                "○"
                            };
                            let label = format!("{glyph} {}", self.files[i].path);
                            if ui.selectable_label(selected, label).clicked() {
                                self.selected = Some(Selection::Changed(i));
                            }
                        }
                    });

                ui.separator();

                // Bottom: full repo file tree (lazy). Click any file to open
                // it full-file, changed or not.
                ui.label(egui::RichText::new("Files").strong());
                egui::ScrollArea::vertical()
                    .id_salt("tree")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let root = self.tree.root.clone();
                        let mut clicked: Option<String> = None;
                        let cur = self.selected.clone();
                        render_tree(ui, &root, &mut self.tree.nodes, &cur, &mut clicked);
                        if let Some(rel) = clicked {
                            self.selected = Some(Selection::Path(rel));
                        }
                    });
            });

        self.ensure_cache();

        // The file whose hunks the controls mutate (only in Changed+Diff).
        let active_file = match &self.selected {
            Some(Selection::Changed(i)) => Some(*i),
            _ => None,
        };
        // Pending status changes collected during render, applied after (so
        // the render closure only needs immutable borrows of self).
        let mut pending: Vec<(usize, ReviewStatus)> = Vec::new();
        // Hunk whose comment button was clicked this frame (opens the editor).
        let mut open_comment: Option<usize> = None;
        // Agent replies, loaded once per frame (tiny dir). Used for both the
        // per-hunk indicator and the open thread. Poll while a changed file is
        // shown so a reply posted by the agent surfaces without interaction.
        let replies = Replies::load(&self.tree.root);
        if active_file.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_secs(2));
        }
        let active_path = active_file.map(|f| self.files[f].path.clone());

        // Bottom panel: comment editor for the active hunk. Rendered before
        // the central panel's scroll so it claims its space; the &mut borrow
        // of self.files is fine here (no cache borrow in scope yet).
        if let (Some(f), Some(h)) = (active_file, self.active_hunk) {
            let header = self.files[f].hunks.get(h).map(|hk| hk.header.clone());
            if let Some(header) = header {
                let mut changed = false;
                egui::TopBottomPanel::bottom("comment").resizable(true).show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Comment").strong());
                        ui.weak(header.trim().to_string());
                        if ui.small_button("close").clicked() {
                            self.active_hunk = None;
                        }
                    });
                    let file_path = self.files[f].path.clone();
                    if let Some(hunk) = self.files[f].hunks.get_mut(h) {
                        let resp = ui.add(
                            egui::TextEdit::multiline(&mut hunk.comment)
                                .desired_rows(3)
                                .desired_width(f32::INFINITY)
                                .hint_text("why this needs changing / a question for the agent"),
                        );
                        changed = resp.changed();
                    }
                    // Agent replies on this hunk's thread (loaded once per
                    // frame at top level; see `replies`).
                    let thread = replies.for_hunk(&file_path, &header);
                    if !thread.is_empty() {
                        ui.separator();
                        egui::ScrollArea::vertical().max_height(120.0).show(ui, |ui| {
                            for r in thread {
                                ui.label(
                                    egui::RichText::new("agent")
                                        .small()
                                        .color(Color32::from_rgb(120, 160, 220)),
                                );
                                ui.label(&r.text);
                            }
                        });
                    }
                });
                if changed {
                    self.save_review_state();
                }
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.selected.is_none() {
                ui.centered_and_justified(|ui| {
                    ui.label("no changes — working tree matches HEAD")
                });
                return;
            }

            let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
            let total = self.cache.len();
            let mut area = egui::ScrollArea::both().auto_shrink([false, false]);
            // Apply a pending key-nav scroll (n/p jumped to a hunk).
            if let Some(off) = self.pending_scroll.take() {
                area = area.vertical_scroll_offset(off);
            }
            area.show_rows(
                ui,
                row_h,
                total,
                |ui, range| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    let focus_row = self.hunk_rows.get(self.focus_hunk).copied();
                    for i in range {
                        match &self.cache[i] {
                            RenderRow::HunkHeader { hunk_idx, text } => {
                                let focused = Some(i) == focus_row;
                                let status = active_file
                                    .and_then(|f| self.files[f].hunks.get(*hunk_idx))
                                    .map(|h| h.status)
                                    .unwrap_or(ReviewStatus::Unreviewed);
                                let hdr_bg = if focused {
                                    Color32::from_rgb(48, 58, 80) // focused: brighter
                                } else {
                                    Color32::from_rgb(30, 36, 48)
                                };
                                egui::Frame::none()
                                    .fill(hdr_bg)
                                    .show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            if focused {
                                                ui.label(
                                                    egui::RichText::new("▶")
                                                        .color(Color32::from_rgb(140, 180, 240)),
                                                );
                                            }
                                            let (glyph, col) = match status {
                                                ReviewStatus::Approved => {
                                                    ("✓", Color32::from_rgb(120, 200, 120))
                                                }
                                                ReviewStatus::Rejected => {
                                                    ("✗", Color32::from_rgb(220, 120, 120))
                                                }
                                                ReviewStatus::Unreviewed => {
                                                    ("○", Color32::DARK_GRAY)
                                                }
                                            };
                                            ui.label(egui::RichText::new(glyph).color(col));
                                            if ui.small_button("approve").clicked() {
                                                pending.push((*hunk_idx, ReviewStatus::Approved));
                                            }
                                            if ui.small_button("reject").clicked() {
                                                pending.push((*hunk_idx, ReviewStatus::Rejected));
                                            }
                                            if status != ReviewStatus::Unreviewed
                                                && ui.small_button("clear").clicked()
                                            {
                                                pending
                                                    .push((*hunk_idx, ReviewStatus::Unreviewed));
                                            }
                                            let has_comment = active_file
                                                .and_then(|f| self.files[f].hunks.get(*hunk_idx))
                                                .map(|h| !h.comment.trim().is_empty())
                                                .unwrap_or(false);
                                            let cbtn = if has_comment { "💬*" } else { "💬" };
                                            if ui.small_button(cbtn).clicked() {
                                                open_comment = Some(*hunk_idx);
                                            }
                                            // Agent-reply count for this hunk.
                                            if let Some(p) = &active_path {
                                                let n = replies.for_hunk(p, text).len();
                                                if n > 0 {
                                                    ui.label(
                                                        egui::RichText::new(format!("↩{n}"))
                                                            .small()
                                                            .color(Color32::from_rgb(
                                                                120, 200, 160,
                                                            )),
                                                    );
                                                }
                                            }
                                            ui.label(
                                                egui::RichText::new(text)
                                                    .monospace()
                                                    .color(Color32::from_rgb(120, 160, 220)),
                                            );
                                        });
                                    });
                            }
                            RenderRow::DiffLine { kind, .. } => {
                                let (bg, gutter) = match kind {
                                    LineKind::Add => {
                                        (Some(Color32::from_rgb(22, 50, 22)), "+ ")
                                    }
                                    LineKind::Del => {
                                        (Some(Color32::from_rgb(55, 22, 22)), "- ")
                                    }
                                    _ => (None, "  "),
                                };
                                let spans = self.row_spans(i); // lazy, memoized
                                let draw = |ui: &mut egui::Ui| line_row(ui, gutter, &spans);
                                if let Some(bg) = bg {
                                    egui::Frame::none().fill(bg).show(ui, draw);
                                } else {
                                    draw(ui);
                                }
                            }
                            RenderRow::Plain { .. } => {
                                let spans = self.row_spans(i); // lazy, memoized
                                line_row(ui, "", &spans);
                            }
                            RenderRow::SplitLine { left, right } => {
                                let lkind = left.as_ref().map(|(k, _)| *k);
                                let rkind = right.as_ref().map(|(k, _)| *k);
                                let (lspans, rspans) = self.split_spans(i);
                                ui.columns(2, |cols| {
                                    split_cell(&mut cols[0], lkind, &lspans);
                                    split_cell(&mut cols[1], rkind, &rspans);
                                });
                            }
                        }
                    }
                },
            );
        });

        // Apply review-status changes collected during render, then persist
        // the review state for the MCP server.
        if let (Some(f), false) = (active_file, pending.is_empty()) {
            for (hunk_idx, status) in pending {
                if let Some(h) = self.files[f].hunks.get_mut(hunk_idx) {
                    h.status = status;
                }
            }
            self.save_review_state();
        }
        if let Some(h) = open_comment {
            self.active_hunk = Some(h);
        }
    }
}

/// Align a hunk's unified rows into side-by-side rows. Context lines show on
/// both sides; runs of deletions/additions are paired row-for-row (del↔add),
/// with any surplus shown one-sided (deletion → left only, addition → right
/// only). This is the standard split-diff pairing.
fn split_align(rows: &[diff::DiffLineRow]) -> Vec<RenderRow> {
    let mut out: Vec<RenderRow> = Vec::new();
    let mut dels: Vec<String> = Vec::new();
    let mut adds: Vec<String> = Vec::new();

    // Flush buffered deletions/additions as paired/one-sided split rows.
    let flush = |out: &mut Vec<RenderRow>, dels: &mut Vec<String>, adds: &mut Vec<String>| {
        let pairs = dels.len().max(adds.len());
        for i in 0..pairs {
            let left = dels.get(i).map(|t| (LineKind::Del, t.clone()));
            let right = adds.get(i).map(|t| (LineKind::Add, t.clone()));
            out.push(RenderRow::SplitLine { left, right });
        }
        dels.clear();
        adds.clear();
    };

    for r in rows {
        match r.kind {
            LineKind::Del => dels.push(r.text.clone()),
            LineKind::Add => adds.push(r.text.clone()),
            LineKind::Ctx => {
                flush(&mut out, &mut dels, &mut adds);
                out.push(RenderRow::SplitLine {
                    left: Some((LineKind::Ctx, r.text.clone())),
                    right: Some((LineKind::Ctx, r.text.clone())),
                });
            }
        }
    }
    flush(&mut out, &mut dels, &mut adds);
    out
}

/// Draw one side of a split-diff row: kind-tinted background + gutter + spans.
/// An empty cell (no kind) draws a faint filler so the gutter aligns.
fn split_cell(ui: &mut egui::Ui, kind: Option<LineKind>, spans: &[(Color32, String)]) {
    let (bg, gutter) = match kind {
        Some(LineKind::Add) => (Some(Color32::from_rgb(22, 50, 22)), "+ "),
        Some(LineKind::Del) => (Some(Color32::from_rgb(55, 22, 22)), "- "),
        Some(LineKind::Ctx) => (None, "  "),
        None => (Some(Color32::from_rgb(28, 28, 30)), "  "), // empty filler
    };
    let draw = |ui: &mut egui::Ui| line_row(ui, gutter, spans);
    if let Some(bg) = bg {
        egui::Frame::none().fill(bg).show(ui, draw);
    } else {
        draw(ui);
    }
}

/// Draw a single monospace content line: optional gutter + highlighted spans.
fn line_row(ui: &mut egui::Ui, gutter: &str, spans: &[(Color32, String)]) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        if !gutter.is_empty() {
            ui.label(egui::RichText::new(gutter).monospace().color(Color32::DARK_GRAY));
        }
        for (color, text) in spans {
            ui.label(egui::RichText::new(text).monospace().color(*color));
        }
    });
}

/// Recursively render the lazy file tree. Directories expand on click
/// (loading children on first expand); files are selectable and report
/// their rel path via `clicked`.
fn render_tree(
    ui: &mut egui::Ui,
    root: &std::path::Path,
    nodes: &mut [Node],
    cur: &Option<Selection>,
    clicked: &mut Option<String>,
) {
    for node in nodes.iter_mut() {
        if node.is_dir {
            let id = ui.make_persistent_id(&node.rel);
            egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false)
                .show_header(ui, |ui| {
                    ui.label(format!("📁 {}", node.name));
                })
                .body(|ui| {
                    // Lazy-load children on first expansion.
                    FileTree::load_children(root, node);
                    if let Some(children) = node.children.as_mut() {
                        render_tree(ui, root, children, cur, clicked);
                    }
                });
        } else {
            let selected = matches!(cur, Some(Selection::Path(p)) if *p == node.rel);
            if ui.selectable_label(selected, &node.name).clicked() {
                *clicked = Some(node.rel.clone());
            }
        }
    }
}

#[cfg(test)]
mod ui_tests {
    use super::*;
    use std::process::Command;

    /// A throwaway git repo with one committed file and a working-tree edit,
    /// so the App opens with a diff containing at least one hunk.
    fn fixture_repo() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "purview-ui-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            Command::new("git").args(args).current_dir(&dir).output().unwrap();
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        git(&["checkout", "-q", "-b", "main"]);
        std::fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);
        std::fs::write(dir.join("a.txt"), "one\nTWO\nthree\nfour\n").unwrap();
        dir
    }

    /// Run one egui frame against `app.ui` with a bare Context — verifies the
    /// UI builds without panicking. The click-simulating test below uses the
    /// egui_kittest harness for actual pointer interaction.
    fn frame(ctx: &egui::Context, app: &mut App) {
        let _ = ctx.run(egui::RawInput::default(), |ctx| app.ui(ctx));
    }

    #[test]
    fn app_opens_with_a_diff_and_renders_all_states_without_panic() {
        let repo = fixture_repo();
        let mut app = App::new(repo.clone());
        assert!(app.selected.is_some(), "a changed file should be auto-selected");
        assert!(!app.files.is_empty(), "the working-tree edit should produce a diff");

        let ctx = egui::Context::default();
        // Render all four layout×extent combinations without panicking.
        for layout in [Layout::Inline, Layout::Split] {
            for extent in [Extent::Summary, Extent::Full] {
                app.layout = layout;
                app.extent = extent;
                frame(&ctx, &mut app);
            }
        }
        // Reset + open a comment editor; render again.
        app.layout = Layout::Inline;
        app.extent = Extent::Summary;
        app.active_hunk = Some(0);
        frame(&ctx, &mut app);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn approving_a_hunk_persists_review_state() {
        // The click handler just sets this status; verify the persistence the
        // GUI then performs round-trips to disk.
        let repo = fixture_repo();
        let mut app = App::new(repo.clone());
        app.files[0].hunks[0].status = ReviewStatus::Approved;
        app.save_review_state();

        let state = ReviewState::load(&repo).expect("review-state.json written");
        assert!(
            state.files.iter().flat_map(|f| &f.hunks).any(|h| h.status == "approved"),
            "approved status should round-trip to disk"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn clicking_approve_button_flips_status_and_persists() {
        use egui_kittest::kittest::Queryable; // get_by_label lives here
        let repo = fixture_repo();
        let app = App::new(repo.clone());
        // Harness carries the App as state; the closure renders it each frame.
        let mut harness = egui_kittest::Harness::new_state(
            |ctx, app: &mut App| app.ui(ctx),
            app,
        );
        harness.run();
        // The "approve" button is labelled by its text.
        harness.get_by_label("approve").click();
        harness.run();

        let approved = harness
            .state()
            .files
            .iter()
            .flat_map(|f| &f.hunks)
            .any(|h| h.status == ReviewStatus::Approved);
        assert!(approved, "clicking approve should set a hunk Approved");

        let state = ReviewState::load(&repo).expect("review-state.json written");
        assert!(
            state.files.iter().flat_map(|f| &f.hunks).any(|h| h.status == "approved"),
            "the click should have persisted approved status to disk"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    fn row(kind: LineKind, t: &str) -> diff::DiffLineRow {
        diff::DiffLineRow { kind, text: t.into() }
    }

    #[test]
    fn split_align_pairs_dels_with_adds_and_mirrors_context() {
        // ctx, then 2 dels + 3 adds, then ctx.
        let rows = vec![
            row(LineKind::Ctx, "a"),
            row(LineKind::Del, "old1"),
            row(LineKind::Del, "old2"),
            row(LineKind::Add, "new1"),
            row(LineKind::Add, "new2"),
            row(LineKind::Add, "new3"),
            row(LineKind::Ctx, "z"),
        ];
        let out = split_align(&rows);
        // ctx(a) | 3 paired/surplus rows | ctx(z) = 5 rows.
        assert_eq!(out.len(), 5);

        let cell = |r: &RenderRow, side: usize| -> Option<(LineKind, String)> {
            match r {
                RenderRow::SplitLine { left, right } => {
                    if side == 0 { left.clone() } else { right.clone() }
                }
                _ => None,
            }
        };
        // Row 0: context on both sides.
        assert_eq!(cell(&out[0], 0), Some((LineKind::Ctx, "a".into())));
        assert_eq!(cell(&out[0], 1), Some((LineKind::Ctx, "a".into())));
        // Row 1: del1 ↔ add1.
        assert_eq!(cell(&out[1], 0), Some((LineKind::Del, "old1".into())));
        assert_eq!(cell(&out[1], 1), Some((LineKind::Add, "new1".into())));
        // Row 2: del2 ↔ add2.
        assert_eq!(cell(&out[2], 0), Some((LineKind::Del, "old2".into())));
        assert_eq!(cell(&out[2], 1), Some((LineKind::Add, "new2".into())));
        // Row 3: surplus add3 → right only, left empty.
        assert_eq!(cell(&out[3], 0), None);
        assert_eq!(cell(&out[3], 1), Some((LineKind::Add, "new3".into())));
        // Row 4: context on both sides.
        assert_eq!(cell(&out[4], 0), Some((LineKind::Ctx, "z".into())));
    }
}
