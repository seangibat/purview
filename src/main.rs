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
use git2::{Diff, DiffFormat, DiffOptions, Repository};

mod highlight;
mod tree;
use highlight::Highlighter;
use tree::{FileTree, Node};

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

#[derive(Clone, Copy, PartialEq)]
enum LineKind {
    Add,
    Del,
    Ctx,
}

/// A diff line: its kind plus the raw text (highlighting applied at render
/// time from the cached per-file highlight, keyed by line content).
#[derive(Clone)]
struct DiffLineRow {
    kind: LineKind,
    text: String,
}

/// Per-hunk review decision. The unit of review is the hunk — small enough
/// to judge, large enough to be meaningful.
#[derive(Clone, Copy, PartialEq)]
enum ReviewStatus {
    Unreviewed,
    Approved,
    Rejected,
}

struct Hunk {
    header: String,
    rows: Vec<DiffLineRow>,
    status: ReviewStatus,
}

struct ChangedFile {
    path: String,
    hunks: Vec<Hunk>,
}

impl ChangedFile {
    /// (reviewed, total) hunk counts for the progress indicator.
    fn progress(&self) -> (usize, usize) {
        let total = self.hunks.len();
        let reviewed = self
            .hunks
            .iter()
            .filter(|h| h.status != ReviewStatus::Unreviewed)
            .count();
        (reviewed, total)
    }
}

#[derive(Clone, Copy, PartialEq)]
enum ViewMode {
    Diff,
    FullFile,
}

/// Current selection: either a changed file (diff-able) or an arbitrary
/// repo file opened from the tree (full-file only).
#[derive(Clone, PartialEq)]
enum Selection {
    Changed(usize),
    Path(String),
}

/// One rendered row in the content pane (post-highlight, ready to draw).
enum RenderRow {
    /// A hunk boundary in diff view. Carries the hunk index so the row can
    /// draw approve/deny controls bound to that hunk's live status.
    HunkHeader { hunk_idx: usize, text: String },
    /// A diff content line (add/del/ctx) with highlighted spans.
    DiffLine {
        kind: LineKind,
        spans: Vec<(Color32, String)>,
    },
    /// A full-file content line with highlighted spans.
    Plain { spans: Vec<(Color32, String)> },
}

/// What we diff against.
#[derive(Clone, Copy, PartialEq)]
enum DiffSource {
    /// Working tree (incl. index + untracked) vs HEAD — local uncommitted work.
    WorkingTree,
    /// `base...HEAD` three-dot: merge-base(base, HEAD) tree vs HEAD tree.
    /// This is what a PR shows — only what this branch introduced.
    BranchRange,
}

struct App {
    repo_path: PathBuf,
    branch: String,
    base: String,
    base_input: String,
    source: DiffSource,
    files: Vec<ChangedFile>,
    selected: Option<Selection>,
    view: ViewMode,
    tree: FileTree,
    error: Option<String>,
    report_note: String,
    hl: Highlighter,
    /// Cached, highlighted, render-ready rows for the current selection/view.
    cache: Vec<RenderRow>,
    cache_key: Option<(Selection, ViewMode)>,
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
            view: ViewMode::Diff,
            tree: FileTree::new(tree_root),
            error: None,
            report_note: String::new(),
            hl: Highlighter::new(),
            cache: Vec::new(),
            cache_key: None,
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
        self.error = None;
        self.cache.clear();
        self.cache_key = None;

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
        let repo = Repository::discover(&self.repo_path)?;
        let branch = repo
            .head()
            .ok()
            .and_then(|h| h.shorthand().map(String::from))
            .unwrap_or_else(|| "(detached)".into());
        let head_tree = repo.head().ok().and_then(|h| h.peel_to_tree().ok());

        let mut opts = DiffOptions::new();
        opts.context_lines(3)
            .include_untracked(true)
            .recurse_untracked_dirs(true);

        let diff: Diff = match self.source {
            DiffSource::WorkingTree => {
                repo.diff_tree_to_workdir_with_index(head_tree.as_ref(), Some(&mut opts))?
            }
            DiffSource::BranchRange => {
                // base...HEAD three-dot: diff from merge-base(base, HEAD) to HEAD.
                let base_obj = repo.revparse_single(&self.base)?;
                let base_commit = base_obj.peel_to_commit()?;
                let head_commit = repo.head()?.peel_to_commit()?;
                let mb = repo.merge_base(base_commit.id(), head_commit.id())?;
                let mb_tree = repo.find_commit(mb)?.tree()?;
                let head_t = head_commit.tree()?;
                repo.diff_tree_to_tree(Some(&mb_tree), Some(&head_t), Some(&mut opts))?
            }
        };

        use std::cell::RefCell;
        let files: RefCell<Vec<ChangedFile>> = RefCell::new(Vec::new());

        diff.print(DiffFormat::Patch, |delta, _hunk, line| {
            let path = delta
                .new_file()
                .path()
                .or_else(|| delta.old_file().path())
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "<unknown>".into());

            let mut files = files.borrow_mut();
            if files.last().map(|f| f.path != path).unwrap_or(true) {
                files.push(ChangedFile {
                    path: path.clone(),
                    hunks: Vec::new(),
                });
            }
            let content = String::from_utf8_lossy(line.content())
                .trim_end_matches('\n')
                .to_string();

            let file = files.last_mut().unwrap();
            match line.origin() {
                'F' => {} // file header — skip; we key on delta path
                'H' => {
                    // Hunk header — start a fresh hunk.
                    file.hunks.push(Hunk {
                        header: content,
                        rows: Vec::new(),
                        status: ReviewStatus::Unreviewed,
                    });
                }
                origin => {
                    let kind = match origin {
                        '+' => LineKind::Add,
                        '-' => LineKind::Del,
                        _ => LineKind::Ctx,
                    };
                    // Content before any hunk header (rare) gets a synthetic hunk.
                    if file.hunks.is_empty() {
                        file.hunks.push(Hunk {
                            header: String::new(),
                            rows: Vec::new(),
                            status: ReviewStatus::Unreviewed,
                        });
                    }
                    file.hunks
                        .last_mut()
                        .unwrap()
                        .rows
                        .push(DiffLineRow { kind, text: content });
                }
            }
            true
        })?;

        Ok((branch, files.into_inner()))
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

    /// Read the full working-tree file for the selected path.
    fn read_full_file(&self, rel: &str) -> std::io::Result<String> {
        let repo_root = Repository::discover(&self.repo_path)
            .ok()
            .and_then(|r| r.workdir().map(|w| w.to_path_buf()))
            .unwrap_or_else(|| self.repo_path.clone());
        std::fs::read_to_string(repo_root.join(rel))
    }

    /// Rebuild the highlighted render cache if the selection / view changed.
    fn ensure_cache(&mut self) {
        let Some(sel) = self.selected.clone() else {
            self.cache.clear();
            self.cache_key = None;
            return;
        };
        // A tree-opened path can only be shown full-file; force it.
        let effective_view = match sel {
            Selection::Path(_) => ViewMode::FullFile,
            Selection::Changed(_) => self.view,
        };
        let key = (sel.clone(), effective_view);
        if self.cache_key == Some(key.clone()) {
            return;
        }

        let mut out: Vec<RenderRow> = Vec::new();

        match (&sel, effective_view) {
            (Selection::Changed(idx), ViewMode::Diff) => {
                let path = self.files[*idx].path.clone();
                // Clone the lightweight structure we need so we can borrow
                // self.hl immutably while iterating.
                let hunks: Vec<(usize, String, Vec<DiffLineRow>)> = self.files[*idx]
                    .hunks
                    .iter()
                    .enumerate()
                    .map(|(hi, h)| (hi, h.header.clone(), h.rows.clone()))
                    .collect();
                for (hunk_idx, header, rows) in hunks {
                    out.push(RenderRow::HunkHeader { hunk_idx, text: header });
                    for r in rows {
                        let spans = self.hl.highlight_line(&path, &r.text);
                        out.push(RenderRow::DiffLine { kind: r.kind, spans });
                    }
                }
            }
            (sel, ViewMode::FullFile) => {
                let path = match sel {
                    Selection::Changed(idx) => self.files[*idx].path.clone(),
                    Selection::Path(p) => p.clone(),
                };
                match self.read_full_file(&path) {
                    Ok(content) => {
                        for line in content.lines() {
                            let spans = self.hl.highlight_line(&path, line);
                            out.push(RenderRow::Plain { spans });
                        }
                    }
                    Err(e) => out.push(RenderRow::Plain {
                        spans: vec![(Color32::LIGHT_RED, format!("cannot read file: {e}"))],
                    }),
                }
            }
            // (Path, Diff) is impossible — forced to FullFile above.
            _ => {}
        }

        self.cache = out;
        self.cache_key = Some(key);
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
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
                    ui.selectable_value(&mut self.view, ViewMode::FullFile, "Full File");
                    ui.selectable_value(&mut self.view, ViewMode::Diff, "Diff");
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

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.selected.is_none() {
                ui.centered_and_justified(|ui| {
                    ui.label("no changes — working tree matches HEAD")
                });
                return;
            }

            let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
            let total = self.cache.len();
            egui::ScrollArea::both().auto_shrink([false, false]).show_rows(
                ui,
                row_h,
                total,
                |ui, range| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for i in range {
                        match &self.cache[i] {
                            RenderRow::HunkHeader { hunk_idx, text } => {
                                let status = active_file
                                    .and_then(|f| self.files[f].hunks.get(*hunk_idx))
                                    .map(|h| h.status)
                                    .unwrap_or(ReviewStatus::Unreviewed);
                                egui::Frame::none()
                                    .fill(Color32::from_rgb(30, 36, 48))
                                    .show(ui, |ui| {
                                        ui.horizontal(|ui| {
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
                                            ui.label(
                                                egui::RichText::new(text)
                                                    .monospace()
                                                    .color(Color32::from_rgb(120, 160, 220)),
                                            );
                                        });
                                    });
                            }
                            RenderRow::DiffLine { kind, spans } => {
                                let (bg, gutter) = match kind {
                                    LineKind::Add => {
                                        (Some(Color32::from_rgb(22, 50, 22)), "+ ")
                                    }
                                    LineKind::Del => {
                                        (Some(Color32::from_rgb(55, 22, 22)), "- ")
                                    }
                                    _ => (None, "  "),
                                };
                                let draw = |ui: &mut egui::Ui| line_row(ui, gutter, spans);
                                if let Some(bg) = bg {
                                    egui::Frame::none().fill(bg).show(ui, draw);
                                } else {
                                    draw(ui);
                                }
                            }
                            RenderRow::Plain { spans } => {
                                line_row(ui, "", spans);
                            }
                        }
                    }
                },
            );
        });

        // Apply review-status changes collected during render.
        if let (Some(f), false) = (active_file, pending.is_empty()) {
            for (hunk_idx, status) in pending {
                if let Some(h) = self.files[f].hunks.get_mut(hunk_idx) {
                    h.status = status;
                }
            }
        }
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
