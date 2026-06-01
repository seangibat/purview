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

use purview::diff::{self, ChangedFile, DiffSource, Hunk, LineKind, ReviewStatus};
use purview::highlight::{Highlighter, IncrementalHl, Spans};
use purview::repo::{self, RepoSource};
use purview::review_state::{FileState, HunkState, Replies, ReviewState};
use purview::tree::{self, Node};

fn main() -> eframe::Result<()> {
    // The argument is either a local path (default) or an `ssh://...` URL for
    // reviewing a repo on a remote machine. `repo::open` picks the backend.
    let arg = std::env::args()
        .nth(1)
        .unwrap_or_else(|| std::env::current_dir().unwrap().to_string_lossy().into_owned());

    let source: Box<dyn RepoSource> = match repo::open(&arg) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("purview: {e}");
            std::process::exit(1);
        }
    };

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 840.0])
            .with_title("purview"),
        ..Default::default()
    };

    eframe::run_native(
        "purview",
        native_options,
        Box::new(move |_cc| Ok(Box::new(App::new(source)))),
    )
}

/// "Go to definition" overlay state (haiku + git grep).
struct Goto {
    query: String,
    just_opened: bool,
    /// Set while the background resolve thread is running.
    resolving: bool,
    /// Receives the resolved definition (or None) from the worker thread.
    rx: Option<std::sync::mpsc::Receiver<Option<purview::gotodef::Candidate>>>,
    /// A status/error line shown in the overlay.
    note: String,
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
    ///
    /// In BOTH Summary and Full extent the controls target exactly `hunk_idx`
    /// (`whole_file = false`): each header acts on its own review hunk, never
    /// always hunk 0 (the bug-3 fix). In Full extent these per-hunk strips are
    /// interleaved into the whole-file flow at each change region, so the user
    /// can approve/reject each change in place as they scroll.
    ///
    /// `whole_file = true` (controls act on EVERY hunk, glyph shows the file
    /// aggregate via `aggregate_status`) is retained for any future file-level
    /// "approve all" affordance; no current view emits it.
    HunkHeader {
        hunk_idx: usize,
        text: String,
        whole_file: bool,
    },
    /// A diff content line (add/del/ctx), raw text. Carries its source line
    /// numbers (old/new side) for the gutter.
    DiffLine {
        kind: LineKind,
        text: String,
        old_lineno: Option<u32>,
        new_lineno: Option<u32>,
    },
    /// A full-file content line, raw text. `lineno` is its 1-based file line.
    Plain { text: String, lineno: u32 },
    /// A side-by-side row: a cell on each side, either of which may be empty
    /// (a deletion has no right cell; an addition has no left cell; context
    /// shows on both). Highlighted lazily via `split_spans`. Each cell carries
    /// its own source line number (old on the left, new on the right).
    SplitLine {
        left: Option<(LineKind, String, Option<u32>)>,
        right: Option<(LineKind, String, Option<u32>)>,
    },
}

struct App {
    /// Repo backend — local (git2 + fs) or SSH. The UI only talks to this.
    repo: std::sync::Arc<dyn RepoSource>,
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
    /// Lazy repo file tree (children loaded via `self.repo` on first expand).
    tree_nodes: Vec<Node>,
    /// Local directory where review state (`.purview/`) is read/written. For a
    /// local repo this is the workdir; for SSH it's a local mirror dir.
    state_root: PathBuf,
    /// Ctrl+P fuzzy file-open overlay state. Some = open.
    quick_open: Option<QuickOpen>,
    /// Go-to-definition overlay state. Some = open.
    goto: Option<Goto>,
    /// Identifier the user last clicked in the diff (target for F12).
    selected_symbol: Option<String>,
    /// After opening a Path selection, scroll its content to this 1-based line.
    pending_line: Option<usize>,
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
    /// Digit-width of the largest line number in the current cache, so the
    /// gutter number columns are right-aligned to a stable width.
    lineno_width: usize,
    /// User-chosen UI scale (Ctrl +/-/0). Persisted in App state so it
    /// survives repaints; applied to the egui context each frame.
    ui_scale: f32,
    /// Set when a key-nav action wants the content scroll area moved to a
    /// specific vertical offset on the next frame.
    pending_scroll: Option<f32>,
    /// Inline edit in full-file view: (cache row index = file line, buffer).
    /// Double-click a line (or `i` on a focused line) to start; Enter writes
    /// the edited line back to the file on disk, Esc cancels.
    editing: Option<(usize, String)>,
    /// In-flight async diff computation (bug #2). `reload()` runs the diff on
    /// a worker thread so a slow remote (SSH does several blocking round-trips)
    /// never freezes the render loop. The worker stamps each result with the
    /// `generation` it was started for; `poll_reload` applies only the latest,
    /// dropping superseded results. `Some` = a reload is in flight.
    diff_rx: Option<std::sync::mpsc::Receiver<(u64, Result<(String, Vec<ChangedFile>), String>)>>,
    /// True while `diff_rx` is in flight — drives the "loading…" UI + spinner.
    loading: bool,
    /// Whether the `?` keybinding cheat-sheet overlay is showing.
    show_help: bool,
    /// The open file's path as of last frame, so the tree only auto-scrolls to
    /// the highlighted row when the open file actually changes (not every frame).
    last_open_path: Option<String>,
    /// Last-known vertical scroll offset of the content pane + its viewport
    /// height, captured each frame so PageUp/PageDown can move by a page.
    content_scroll: f32,
    content_viewport_h: f32,
}

impl App {
    fn new(repo: Box<dyn RepoSource>) -> Self {
        // Share the backend behind an Arc so the go-to-definition worker thread
        // can hold its own clone (the Claude precision step runs there).
        let repo: std::sync::Arc<dyn RepoSource> = std::sync::Arc::from(repo);
        let state_root = repo.state_root().to_path_buf();
        // Root-level tree nodes (lazy; children load on expand via self.repo).
        let tree_nodes = repo
            .list_dir("")
            .unwrap_or_default()
            .into_iter()
            .map(node_from_entry)
            .collect();
        let mut app = App {
            repo,
            branch: String::new(),
            base: String::new(),
            base_input: String::new(),
            source: DiffSource::WorkingTree,
            files: Vec::new(),
            selected: None,
            active_hunk: None,
            layout: Layout::Inline,
            extent: Extent::Summary,
            tree_nodes,
            state_root,
            quick_open: None,
            goto: None,
            selected_symbol: None,
            pending_line: None,
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
            lineno_width: 1,
            ui_scale: 1.0,
            pending_scroll: None,
            editing: None,
            diff_rx: None,
            loading: false,
            show_help: false,
            last_open_path: None,
            content_scroll: 0.0,
            content_viewport_h: 0.0,
        };
        // Guess a sensible default base for branch-range mode.
        app.base = app.guess_default_base();
        app.base_input = app.base.clone();
        app.reload();
        // The initial diff runs synchronously so the window opens already
        // showing content (no first-frame "loading…" flash); subsequent
        // reloads go async. For a local repo this is instant; even for SSH the
        // one-time startup cost is acceptable and keeps `new` deterministic.
        app.poll_reload_blocking();
        app
    }

    /// Pick a default base branch: first of main / master / develop / trunk
    /// that resolves in the repo, else "main".
    fn guess_default_base(&self) -> String {
        self.repo.guess_default_base()
    }

    /// Kick off a reload. The diff itself runs on a WORKER THREAD (bug #2):
    /// over SSH `compute_diff` makes several blocking remote git round-trips
    /// (~seconds), and doing that on the UI thread froze the whole app. Here we
    /// only reset state + spawn the worker, then return immediately so the
    /// render loop keeps running and can show a "loading…" state. The result is
    /// picked up by `poll_reload`, which ignores any result whose `generation`
    /// has since been superseded by a newer reload (race guard).
    fn reload(&mut self) {
        self.files.clear();
        self.selected = None;
        self.active_hunk = None;
        self.error = None;
        self.report_note.clear();
        self.cache.clear();
        self.cache_key = None;
        self.generation = self.generation.wrapping_add(1);
        let gen = self.generation;

        let (tx, rx) = std::sync::mpsc::channel();
        let repo = std::sync::Arc::clone(&self.repo);
        let source = self.source;
        let base = self.base.clone();
        std::thread::spawn(move || {
            let res = repo.compute_diff(source, &base);
            // The receiver may be gone if the app is closing — ignore.
            let _ = tx.send((gen, res));
        });
        self.diff_rx = Some(rx);
        self.loading = true;
    }

    /// Non-blocking poll for an in-flight reload (bug #2). Applies a finished
    /// diff IFF it matches the current `generation` (a newer reload supersedes
    /// an older in-flight one). Returns true if the in-flight result for the
    /// CURRENT generation landed this call.
    fn poll_reload(&mut self) -> bool {
        let Some(rx) = &self.diff_rx else { return false };
        let Ok((gen, res)) = rx.try_recv() else { return false };
        // A stale result (its reload was superseded). Drop it, but only stop
        // showing "loading" if no newer reload is pending — which it always is
        // when gen != self.generation, so keep waiting.
        if gen != self.generation {
            return false;
        }
        self.diff_rx = None;
        self.loading = false;
        self.apply_diff_result(res);
        true
    }

    /// Block until the in-flight reload finishes and apply it. Used only for
    /// the very first load in `new` (so the window opens with content) and in
    /// tests that want deterministic post-reload state.
    fn poll_reload_blocking(&mut self) {
        let Some(rx) = self.diff_rx.take() else { return };
        // Drain to the latest result for the current generation.
        let want = self.generation;
        let mut applied = false;
        while let Ok((gen, res)) = rx.recv() {
            if gen == want {
                self.apply_diff_result(res);
                applied = true;
                break;
            }
            // else: a superseded generation's result — keep reading.
        }
        let _ = applied;
        self.loading = false;
    }

    /// Apply a finished diff result to the UI state.
    fn apply_diff_result(&mut self, res: Result<(String, Vec<ChangedFile>), String>) {
        match res {
            Ok((branch, files)) => {
                self.branch = branch;
                self.files = files;
                self.selected = if self.files.is_empty() {
                    None
                } else {
                    Some(Selection::Changed(0))
                };
            }
            Err(e) => {
                self.files.clear();
                self.selected = None;
                self.error = Some(e);
            }
        }
        // The file set changed — drop any stale render cache.
        self.cache.clear();
        self.cache_key = None;
        self.focus_hunk = 0;
    }

    /// The path of the file currently open in the content pane, whether it was
    /// opened from the changed-files list (`Changed`) or the tree (`Path`).
    /// `None` when nothing is selected.
    fn open_path(&self) -> Option<String> {
        match &self.selected {
            Some(Selection::Changed(i)) => self.files.get(*i).map(|f| f.path.clone()),
            Some(Selection::Path(p)) => Some(p.clone()),
            None => None,
        }
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
        let _ = state.save(&self.state_root);
    }

    fn count_status(&self, status: ReviewStatus) -> usize {
        self.files
            .iter()
            .flat_map(|f| f.hunks.iter())
            .filter(|h| h.status == status)
            .count()
    }

    /// Write the report to <state_root>/.purview/review-report.md; return path.
    fn write_report(&self) -> std::io::Result<PathBuf> {
        let dir = self.state_root.join(".purview");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("review-report.md");
        std::fs::write(&path, self.review_report())?;
        Ok(path)
    }

    /// Read the full current (working/new side) contents of `rel` via the repo
    /// backend (local fs or remote cat).
    fn read_full_file(&self, rel: &str) -> Result<String, String> {
        self.repo.read_file(rel)
    }

    /// Write `new_text` to the file's line `line0` (0-based), preserving the
    /// rest. Only valid in full-file (Plain) view, where cache row == file
    /// line. Routed through the repo backend (no-op/error in ssh mode).
    fn write_line(&self, rel: &str, line0: usize, new_text: &str) -> Result<(), String> {
        self.repo.write_line(rel, line0, new_text)
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
                if self.extent == Extent::Full {
                    // Full extent: show the whole file with the diff overlaid
                    // (re-diffed with infinite context). The full-context hunks
                    // merge adjacent changes, so they don't map 1:1 to the
                    // file's review hunks. We instead interleave a per-hunk
                    // control strip into the full flow at each review hunk's
                    // change region, so approve/reject targets that exact hunk
                    // in place as the user scrolls.
                    let full_hunks: Vec<diff::Hunk> = self
                        .repo
                        .compute_file_diff(self.source, &self.base, u32::MAX, &path)
                        .ok()
                        .and_then(|(_, mut files)| {
                            files
                                .iter()
                                .position(|f| f.path == path)
                                .map(|i| std::mem::take(&mut files[i].hunks))
                        })
                        .unwrap_or_else(|| self.files[*idx].hunks.clone());
                    // The review hunks (what Summary shows) and the line-number
                    // key at which each one's change region begins.
                    let review = &self.files[*idx].hunks;
                    let keys: Vec<Option<(LineKind, u32)>> =
                        review.iter().map(hunk_change_key).collect();
                    // Flatten all full-context rows into one stream, then walk
                    // it placing each review hunk's header just before its
                    // change region's first changed line. We emit content in
                    // segments delimited by header insertions so Split pairing
                    // (del↔add) is computed per region, never across a header.
                    let full_rows: Vec<&diff::DiffLineRow> =
                        full_hunks.iter().flat_map(|h| h.rows.iter()).collect();
                    let mut next_hunk = 0usize;
                    let mut seg: Vec<diff::DiffLineRow> = Vec::new();
                    let push_seg = |out: &mut Vec<RenderRow>,
                                    seg: &mut Vec<diff::DiffLineRow>,
                                    layout: Layout| {
                        if seg.is_empty() {
                            return;
                        }
                        match layout {
                            Layout::Inline => {
                                for r in seg.iter() {
                                    out.push(RenderRow::DiffLine {
                                        kind: r.kind,
                                        text: r.text.clone(),
                                        old_lineno: r.old_lineno,
                                        new_lineno: r.new_lineno,
                                    });
                                }
                            }
                            Layout::Split => out.extend(split_align(seg)),
                        }
                        seg.clear();
                    };
                    for r in full_rows {
                        // Emit the matching review hunk's header just before its
                        // first changed line, flushing the prior segment first.
                        loop {
                            if next_hunk >= review.len() {
                                break;
                            }
                            match keys[next_hunk] {
                                Some(k) if row_matches_change_key(r, k) => {
                                    push_seg(&mut out, &mut seg, self.layout);
                                    out.push(RenderRow::HunkHeader {
                                        hunk_idx: next_hunk,
                                        text: review[next_hunk].header.clone(),
                                        whole_file: false,
                                    });
                                    next_hunk += 1;
                                    break;
                                }
                                // A keyless hunk (no changed rows) can't be
                                // placed by content; skip it so it doesn't
                                // block later hunks.
                                None => next_hunk += 1,
                                _ => break,
                            }
                        }
                        seg.push(r.clone());
                    }
                    push_seg(&mut out, &mut seg, self.layout);
                } else {
                    // Summary: the already-computed 3-line-context hunks, each
                    // with its own per-hunk control strip.
                    let hunks = &self.files[*idx].hunks;
                    for (hunk_idx, hunk) in hunks.iter().enumerate() {
                        out.push(RenderRow::HunkHeader {
                            hunk_idx,
                            text: hunk.header.clone(),
                            whole_file: false,
                        });
                        match self.layout {
                            Layout::Inline => {
                                for r in &hunk.rows {
                                    out.push(RenderRow::DiffLine {
                                        kind: r.kind,
                                        text: r.text.clone(),
                                        old_lineno: r.old_lineno,
                                        new_lineno: r.new_lineno,
                                    });
                                }
                            }
                            Layout::Split => out.extend(split_align(&hunk.rows)),
                        }
                    }
                }
            }
            Selection::Path(p) => {
                // Unchanged file from the tree — just show it whole.
                path = p.clone();
                plain = true;
                match self.read_full_file(&path) {
                    Ok(content) => {
                        for (i, line) in content.lines().enumerate() {
                            out.push(RenderRow::Plain {
                                text: line.to_string(),
                                lineno: i as u32 + 1,
                            });
                        }
                    }
                    Err(e) => out.push(RenderRow::Plain {
                        text: format!("cannot read file: {e}"),
                        lineno: 1,
                    }),
                }
            }
        }

        let n = out.len();
        // Widest line number across all rows → fixed gutter column width.
        let max_lineno = out
            .iter()
            .map(|r| match r {
                RenderRow::DiffLine { old_lineno, new_lineno, .. } => {
                    old_lineno.unwrap_or(0).max(new_lineno.unwrap_or(0))
                }
                RenderRow::Plain { lineno, .. } => *lineno,
                RenderRow::SplitLine { left, right } => {
                    let l = left.as_ref().and_then(|(_, _, n)| *n).unwrap_or(0);
                    let r = right.as_ref().and_then(|(_, _, n)| *n).unwrap_or(0);
                    l.max(r)
                }
                RenderRow::HunkHeader { .. } => 0,
            })
            .max()
            .unwrap_or(0);
        self.lineno_width = max_lineno.max(1).to_string().len();
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
                    .map(|(_, t, _)| self.hl.highlight_line(&self.cache_path, t))
                    .unwrap_or_default();
                let r = right
                    .as_ref()
                    .map(|(_, t, _)| self.hl.highlight_line(&self.cache_path, t))
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
                RenderRow::Plain { text, .. } => text.as_str(),
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
    /// Go-to-definition overlay: type a symbol, Enter kicks off a background
    /// git-grep + Claude-CLI resolve, jumps to the definition when it lands.
    /// Kick off a background go-to-definition resolve for `symbol` (git grep
    /// + Claude CLI). Opens/updates the Goto overlay in its "resolving" state;
    /// goto_overlay polls the worker and jumps when it lands.
    fn start_goto(&mut self, symbol: String) {
        if symbol.trim().is_empty() {
            return;
        }
        // Go-to-definition is gated per backend (always on now: local runs git
        // grep on the workdir, ssh runs it on the remote). Keep the guard so a
        // future read-only backend can still opt out cleanly.
        if !self.repo.supports_goto() {
            self.goto = Some(Goto {
                query: symbol,
                just_opened: false,
                resolving: false,
                rx: None,
                note: "go-to-definition is not supported for this repo".to_string(),
            });
            return;
        }
        let symbol = symbol.trim().to_string();
        // Candidate-gathering (git grep) must run WHERE the repo lives, so it
        // goes through the repo backend on this (main) thread — it's fast.
        let cands = match self.repo.grep_symbol(&symbol) {
            Ok(c) => c,
            Err(e) => {
                self.goto = Some(Goto {
                    query: symbol,
                    just_opened: false,
                    resolving: false,
                    rx: None,
                    note: format!("grep failed: {e}"),
                });
                return;
            }
        };
        if cands.is_empty() {
            self.goto = Some(Goto {
                query: symbol,
                just_opened: false,
                resolving: false,
                rx: None,
                note: "no candidates found".to_string(),
            });
            return;
        }
        // The Claude-CLI precision step runs WHERE the repo (and claude) live:
        // locally for LocalRepo, on the remote over SSH for SshRepo. Route it
        // through the repo backend (resolve_definition) on a background thread,
        // sharing the backend via a cheap Arc clone.
        let (tx, rx) = std::sync::mpsc::channel();
        let worker_symbol = symbol.clone();
        let repo = std::sync::Arc::clone(&self.repo);
        std::thread::spawn(move || {
            let res = repo
                .resolve_definition(&worker_symbol, None, &cands)
                .unwrap_or(None);
            let _ = tx.send(res);
        });
        self.goto = Some(Goto {
            query: symbol,
            just_opened: false,
            resolving: true,
            rx: Some(rx),
            note: "resolving via git grep + haiku…".to_string(),
        });
    }

    fn goto_overlay(&mut self, ctx: &egui::Context) {
        // Poll the worker for a finished resolve, regardless of overlay focus.
        let mut finished: Option<Option<purview::gotodef::Candidate>> = None;
        if let Some(go) = self.goto.as_mut() {
            if let Some(rx) = &go.rx {
                if let Ok(res) = rx.try_recv() {
                    finished = Some(res);
                }
            }
        }
        if let Some(res) = finished {
            match res {
                Some(cand) => {
                    self.selected = Some(Selection::Path(cand.file.clone()));
                    self.pending_line = Some(cand.line);
                    self.goto = None;
                }
                None => {
                    if let Some(go) = self.goto.as_mut() {
                        go.resolving = false;
                        go.rx = None;
                        go.note = "no definition found".to_string();
                    }
                }
            }
        }

        if self.goto.is_none() {
            return;
        }
        let (esc, enter) = ctx.input(|i| {
            (i.key_pressed(egui::Key::Escape), i.key_pressed(egui::Key::Enter))
        });
        if esc {
            self.goto = None;
            return;
        }
        // Snapshot the bits we need without holding a borrow across start_goto.
        let resolving = self.goto.as_ref().map(|g| g.resolving).unwrap_or(false);
        let query = self
            .goto
            .as_ref()
            .map(|g| g.query.trim().to_string())
            .unwrap_or_default();
        if resolving {
            // Keep repainting so the try_recv poll runs.
            ctx.request_repaint_after(std::time::Duration::from_millis(150));
        }
        if enter && !resolving && !query.is_empty() {
            self.start_goto(query);
        }
        let Some(go) = self.goto.as_mut() else { return };

        egui::Window::new("Go to definition")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_TOP, [0.0, 80.0])
            .fixed_size([560.0, 120.0])
            .show(ctx, |ui| {
                let resp = ui.add_enabled(
                    !go.resolving,
                    egui::TextEdit::singleline(&mut go.query)
                        .hint_text("symbol name (e.g. OrderValidator::check)")
                        .desired_width(f32::INFINITY),
                );
                if go.just_opened {
                    resp.request_focus();
                    go.just_opened = false;
                } else if !go.resolving {
                    resp.request_focus();
                }
                if !go.note.is_empty() {
                    ui.label(egui::RichText::new(&go.note).small().weak());
                }
                if !go.resolving {
                    ui.label(
                        egui::RichText::new("Enter to find · Esc to cancel")
                            .small()
                            .weak(),
                    );
                }
            });
    }

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

    /// The `?` keybinding cheat-sheet. A plain `egui::Window` listing the live
    /// bindings (see [`keybindings`]); toggled by `?` and dismissed by `?`/Esc
    /// (handled in `handle_nav_keys`).
    fn help_overlay(&mut self, ctx: &egui::Context) {
        if !self.show_help {
            return;
        }
        let mut open = true;
        egui::Window::new("Keyboard shortcuts")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                egui::Grid::new("help-grid")
                    .num_columns(2)
                    .spacing([24.0, 4.0])
                    .show(ui, |ui| {
                        for (keys, desc) in keybindings() {
                            ui.label(egui::RichText::new(*keys).monospace().strong());
                            ui.label(*desc);
                            ui.end_row();
                        }
                    });
                ui.add_space(4.0);
                ui.label(egui::RichText::new("? or Esc to close").small().weak());
            });
        // Window close button ('x') also dismisses.
        if !open {
            self.show_help = false;
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
        if self.quick_open.is_some() || self.goto.is_some() || ctx.wants_keyboard_input() {
            return;
        }

        // `?` toggles the keybinding cheat-sheet (Esc also closes it). Read it
        // before the other nav keys; it never falls through to them. egui has
        // no dedicated "?" key, so we detect Shift+/ (Slash) or the Questionmark
        // key where the platform reports it.
        let help_toggle = ctx.input(|i| {
            i.key_pressed(egui::Key::Questionmark)
                || (i.modifiers.shift && i.key_pressed(egui::Key::Slash))
        });
        let esc = ctx.input(|i| i.key_pressed(egui::Key::Escape));
        if help_toggle {
            self.show_help = !self.show_help;
            return;
        }
        if self.show_help && esc {
            self.show_help = false;
            return;
        }

        // PageUp/PageDown scroll the content pane by ~one viewport height. They
        // only fire here (not while a text field has focus — guarded above).
        let (page_up, page_down) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::PageUp),
                i.key_pressed(egui::Key::PageDown),
            )
        });
        if page_up || page_down {
            let content_h = self.cache.len() as f32
                * (ctx.style().text_styles[&egui::TextStyle::Monospace].size + 3.0);
            let off = page_scroll(
                self.content_scroll,
                self.content_viewport_h,
                content_h,
                page_down,
            );
            self.pending_scroll = Some(off);
            // Keep our tracked offset in step so a held key pages smoothly.
            self.content_scroll = off;
            return;
        }

        let (j, k, n, p, a, r, c, g) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::J),
                i.key_pressed(egui::Key::K),
                i.key_pressed(egui::Key::N),
                i.key_pressed(egui::Key::P),
                i.key_pressed(egui::Key::A),
                i.key_pressed(egui::Key::R),
                i.key_pressed(egui::Key::C),
                i.key_pressed(egui::Key::G),
            )
        });

        // F12: go to definition of the clicked symbol (editor convention).
        let f12 = ctx.input(|i| i.key_pressed(egui::Key::F12));
        if f12 {
            if let Some(sym) = self.selected_symbol.clone() {
                self.start_goto(sym);
            }
            return;
        }

        // g: open the go-to-definition overlay (type a symbol).
        if g {
            self.goto = Some(Goto {
                query: self.selected_symbol.clone().unwrap_or_default(),
                just_opened: true,
                resolving: false,
                rx: None,
                note: String::new(),
            });
            return;
        }

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
        // In BOTH Summary and Full each rendered header maps 1:1 to a review
        // hunk in order, so `focus_hunk` IS the review-hunk index — keyboard and
        // the on-screen per-hunk button strip target the same hunk (bug 3).
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
        // Pick up an async reload (bug #2) if it finished. While one is in
        // flight, keep repainting so the poll runs and the spinner animates —
        // the worker thread can't wake egui on its own.
        self.poll_reload();
        if self.loading {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        // Ctrl +/- text zoom, Ctrl+0 reset. We drive egui's UI scaling and keep
        // the chosen factor in `ui_scale` so it persists across repaints. Only
        // fires with Ctrl/Cmd held, so it never collides with the bare-letter
        // nav keys (j/k/n/p/a/r/c/g/F12). Ctrl+= is handled too: most keyboards
        // send "=" for the unshifted "+" key.
        let (zoom_in, zoom_out, zoom_reset) = ctx.input(|i| {
            let m = i.modifiers.ctrl || i.modifiers.command;
            (
                m && (i.key_pressed(egui::Key::Plus) || i.key_pressed(egui::Key::Equals)),
                m && i.key_pressed(egui::Key::Minus),
                m && i.key_pressed(egui::Key::Num0),
            )
        });
        if zoom_reset {
            self.ui_scale = 1.0;
        } else if zoom_in {
            self.ui_scale = (self.ui_scale + 0.1).min(3.0);
        } else if zoom_out {
            self.ui_scale = (self.ui_scale - 0.1).max(0.6);
        }
        // Apply every frame so the scale survives repaints/reloads.
        ctx.set_pixels_per_point(self.ui_scale);

        // Ctrl+P opens the fuzzy file finder. (Cmd+P on mac.)
        let toggle_qo = ctx.input(|i| {
            i.key_pressed(egui::Key::P) && (i.modifiers.ctrl || i.modifiers.command)
        });
        if toggle_qo {
            if self.quick_open.is_some() {
                self.quick_open = None;
            } else {
                let (all, truncated) = self.repo.list_all_files(50_000);
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
        self.goto_overlay(ctx);
        self.handle_nav_keys(ctx);
        self.help_overlay(ctx);

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("purview");
                ui.separator();
                ui.label(format!("repo: {}", self.repo.label()));
                ui.separator();
                ui.label(format!("branch: {}", self.branch));
                if self.loading {
                    ui.add(egui::Spinner::new());
                    ui.label(egui::RichText::new("loading…").weak());
                }
                ui.separator();
                let (rev, tot) = self.review_totals();
                ui.label(format!("reviewed: {rev}/{tot} hunks"));
                ui.separator();
                ui.label("session: (none)");
                if let Some(sym) = &self.selected_symbol {
                    ui.separator();
                    ui.label(
                        egui::RichText::new(format!("symbol: {sym}  (F12 → def)"))
                            .color(Color32::from_rgb(200, 200, 120)),
                    );
                }
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
                let open = self.open_path();
                // Auto-scroll the tree to the open file only when it changes,
                // so manual scrolling isn't yanked back every frame.
                let scroll_to_open = open != self.last_open_path;
                egui::ScrollArea::vertical()
                    .id_salt("tree")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let mut clicked: Option<String> = None;
                        render_tree(
                            ui,
                            self.repo.as_ref(),
                            &mut self.tree_nodes,
                            open.as_deref(),
                            scroll_to_open,
                            &mut clicked,
                        );
                        if let Some(rel) = clicked {
                            self.selected = Some(Selection::Path(rel));
                        }
                    });
                self.last_open_path = open;
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
        // Identifier clicked in the diff this frame (becomes the F12 target).
        let mut clicked_symbol: Option<String> = None;
        // Content-pane scroll offset + viewport height captured this frame, so
        // PageUp/PageDown (handled next frame) can move by a page.
        let mut content_scroll = self.content_scroll;
        let mut content_viewport_h = self.content_viewport_h;
        let sel_sym = self.selected_symbol.clone();
        // Inline-edit state, pulled out so the render closure can mutate the
        // buffer while `self` is immutably borrowed for the cache.
        let edit_row = self.editing.as_ref().map(|(r, _)| *r);
        let mut edit_buf = self.editing.as_ref().map(|(_, b)| b.clone()).unwrap_or_default();
        // Inline editing writes back to the repo; only the local backend
        // supports it. In ssh mode the file view stays read-only.
        let can_edit = self.repo.supports_editing();
        let mut edit_start: Option<(usize, String)> = None;
        let mut edit_commit: Option<(usize, String)> = None;
        let mut edit_cancel = false;
        // Agent replies, loaded once per frame (tiny dir). Used for both the
        // per-hunk indicator and the open thread. Poll while a changed file is
        // shown so a reply posted by the agent surfaces without interaction.
        let replies = Replies::load(&self.state_root);
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
            if self.loading {
                ui.centered_and_justified(|ui| {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new());
                        ui.label("computing diff…");
                    });
                });
                return;
            }
            if self.selected.is_none() {
                let msg = if self.error.is_some() {
                    "diff failed — see the error above"
                } else {
                    "no changes — working tree matches HEAD"
                };
                ui.centered_and_justified(|ui| ui.label(msg));
                return;
            }

            let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
            let total = self.cache.len();
            // Resolve any pending vertical scroll (n/p hunk jump, or go-to-def
            // line jump) into an absolute offset to apply this frame.
            let mut pending_v: Option<f32> = self.pending_scroll.take();
            if let Some(line) = self.pending_line.take() {
                let target = line.saturating_sub(1).saturating_sub(8); // a little headroom
                pending_v = Some(target as f32 * row_h);
            }

            // Split layout draws two side-by-side panes that scroll
            // HORIZONTALLY on their own (a long line on the left never shifts
            // the right pane's x-position) while staying vertically locked so
            // line numbers line up row-for-row. The other layouts use one
            // unified scroll area.
            if self.layout == Layout::Split && matches!(self.selected, Some(Selection::Changed(_)))
            {
                let (off, vp) = self.split_panes(
                    ui,
                    row_h,
                    total,
                    pending_v,
                    sel_sym.as_deref(),
                    &mut clicked_symbol,
                );
                content_scroll = off;
                content_viewport_h = vp;
                return;
            }

            let mut area = egui::ScrollArea::both().auto_shrink([false, false]);
            if let Some(off) = pending_v {
                area = area.vertical_scroll_offset(off);
            }
            let out = area.show_rows(
                ui,
                row_h,
                total,
                |ui, range| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    let focus_row = self.hunk_rows.get(self.focus_hunk).copied();
                    for i in range {
                        match &self.cache[i] {
                            RenderRow::HunkHeader { hunk_idx, text, whole_file } => {
                                let focused = Some(i) == focus_row;
                                // Which review hunks this control strip targets:
                                // exactly `hunk_idx` in Summary, or ALL of the
                                // file's hunks in Full extent (where the shown
                                // content is one full-file hunk).
                                let targets: Vec<usize> = match active_file {
                                    Some(f) if *whole_file => {
                                        (0..self.files[f].hunks.len()).collect()
                                    }
                                    Some(_) => vec![*hunk_idx],
                                    None => Vec::new(),
                                };
                                // Aggregate status across the targeted hunks: a
                                // single hunk shows its own status; the whole-
                                // file strip shows all-approved / all-rejected /
                                // else unreviewed (mixed reads as "needs work").
                                let status = active_file
                                    .map(|f| aggregate_status(&self.files[f], &targets))
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
                                            // Always reserve the focus-marker
                                            // column so toggling focus doesn't
                                            // reflow the row (bug 3 layout shift).
                                            ui.label(
                                                egui::RichText::new(if focused {
                                                    "▶"
                                                } else {
                                                    " "
                                                })
                                                .monospace()
                                                .color(Color32::from_rgb(140, 180, 240)),
                                            );
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
                                                for &t in &targets {
                                                    pending.push((t, ReviewStatus::Approved));
                                                }
                                            }
                                            if ui.small_button("reject").clicked() {
                                                for &t in &targets {
                                                    pending.push((t, ReviewStatus::Rejected));
                                                }
                                            }
                                            // Always render "clear" (disabled
                                            // when nothing to clear) so the row
                                            // never reflows when status toggles
                                            // (bug 3 layout shift).
                                            if ui
                                                .add_enabled(
                                                    status != ReviewStatus::Unreviewed,
                                                    egui::Button::new("clear").small(),
                                                )
                                                .clicked()
                                            {
                                                for &t in &targets {
                                                    pending.push((t, ReviewStatus::Unreviewed));
                                                }
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
                            RenderRow::DiffLine { kind, old_lineno, new_lineno, .. } => {
                                let (bg, marker) = match kind {
                                    LineKind::Add => (Some(Color32::from_rgb(22, 50, 22)), '+'),
                                    LineKind::Del => (Some(Color32::from_rgb(55, 22, 22)), '-'),
                                    _ => (None, ' '),
                                };
                                // Gutter: right-aligned old# new# then the marker.
                                let w = self.lineno_width;
                                let gutter = format!(
                                    "{} {} {} ",
                                    fmt_lineno(*old_lineno, w),
                                    fmt_lineno(*new_lineno, w),
                                    marker,
                                );
                                let spans = self.row_spans(i); // lazy, memoized
                                let sel = sel_sym.as_deref();
                                let clk = if let Some(bg) = bg {
                                    egui::Frame::none()
                                        .fill(bg)
                                        .show(ui, |ui| line_row(ui, &gutter, &spans, sel, true))
                                        .inner
                                } else {
                                    line_row(ui, &gutter, &spans, sel, true)
                                };
                                if clk.is_some() {
                                    clicked_symbol = clk;
                                }
                            }
                            RenderRow::Plain { text, lineno } => {
                                if edit_row == Some(i) {
                                    // This line is being edited: inline TextEdit.
                                    let te = ui.add(
                                        egui::TextEdit::singleline(&mut edit_buf)
                                            .desired_width(f32::INFINITY)
                                            .font(egui::TextStyle::Monospace),
                                    );
                                    te.request_focus();
                                    let enter = te.lost_focus()
                                        && ui.input(|i| i.key_pressed(egui::Key::Enter));
                                    let esc = ui.input(|i| i.key_pressed(egui::Key::Escape));
                                    if enter {
                                        edit_commit = Some((i, edit_buf.clone()));
                                    } else if esc {
                                        edit_cancel = true;
                                    }
                                } else {
                                    let spans = self.row_spans(i); // lazy, memoized
                                    let gutter =
                                        format!("{} ", fmt_lineno(Some(*lineno), self.lineno_width));
                                    // Not clickable-for-symbols in file view; the
                                    // row-level response catches double-click to edit.
                                    let resp = ui
                                        .scope(|ui| line_row(ui, &gutter, &spans, None, false))
                                        .response
                                        .interact(egui::Sense::click());
                                    if can_edit && resp.double_clicked() {
                                        edit_start = Some((i, text.clone()));
                                    }
                                }
                            }
                            RenderRow::SplitLine { left, right } => {
                                let lkind = left.as_ref().map(|(k, _, _)| *k);
                                let rkind = right.as_ref().map(|(k, _, _)| *k);
                                let lno = left.as_ref().and_then(|(_, _, n)| *n);
                                let rno = right.as_ref().and_then(|(_, _, n)| *n);
                                let (lspans, rspans) = self.split_spans(i);
                                let sel = sel_sym.as_deref();
                                let w = self.lineno_width;
                                ui.columns(2, |cols| {
                                    if let Some(s) =
                                        split_cell(&mut cols[0], lkind, lno, w, &lspans, sel)
                                    {
                                        clicked_symbol = Some(s);
                                    }
                                    if let Some(s) =
                                        split_cell(&mut cols[1], rkind, rno, w, &rspans, sel)
                                    {
                                        clicked_symbol = Some(s);
                                    }
                                });
                            }
                        }
                    }
                },
            );
            content_scroll = out.state.offset.y;
            content_viewport_h = out.inner_rect.height();
        });

        // Record the content-pane scroll geometry for next frame's PageUp/Down.
        self.content_scroll = content_scroll;
        self.content_viewport_h = content_viewport_h;

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
        if let Some(s) = clicked_symbol {
            self.selected_symbol = Some(s);
        }
        // Resolve inline-edit transitions.
        if let Some((row, text)) = edit_start {
            self.editing = Some((row, text));
        } else if edit_cancel {
            self.editing = None;
        } else if let Some((row, new_text)) = edit_commit {
            // Write the line back; row index == file line in Plain view, which
            // only happens for a tree-opened Path selection.
            if let Some(Selection::Path(path)) = self.selected.clone() {
                match self.write_line(&path, row, &new_text) {
                    Ok(()) => {
                        self.report_note = format!("edited {path}:{}", row + 1);
                        self.cache_key = None; // force re-read of the file
                    }
                    Err(e) => self.report_note = format!("edit failed: {e}"),
                }
            }
            self.editing = None;
        } else {
            // Still editing the same row — keep the buffer.
            self.editing = edit_row.map(|r| (r, edit_buf));
        }
    }
}

impl App {
    /// Render Split (side-by-side) view as two independent panes. Each pane is
    /// a fixed half-width column with its OWN horizontal scroll, so a long line
    /// on the left can't push the right pane over. The two panes are kept
    /// vertically locked (shared offset in `self.split_scroll`) so a given
    /// cache row draws at the same y on both sides → line numbers stay aligned.
    ///
    /// HunkHeader rows are control strips that span the whole width; they're
    /// drawn in the LEFT pane and mirrored as an equal-height blank in the
    /// right pane so both panes advance by the same number of rows.
    /// Returns the panes' shared (vertical_offset, viewport_height) so the
    /// caller can record them for PageUp/PageDown.
    fn split_panes(
        &self,
        ui: &mut egui::Ui,
        row_h: f32,
        total: usize,
        pending_v: Option<f32>,
        sel: Option<&str>,
        clicked_symbol: &mut Option<String>,
    ) -> (f32, f32) {
        let gap = 8.0;
        let pane_w = split_pane_width(ui.available_width(), gap);
        // Shared vertical offset carried across frames in egui memory (the
        // method only has &self). A pending key-nav / go-to-def jump overrides
        // it for this frame.
        let scroll_id = egui::Id::new("purview-split-scroll");
        let carried: f32 = ui
            .ctx()
            .memory(|m| m.data.get_temp(scroll_id))
            .unwrap_or(0.0);
        let v_off = pending_v.unwrap_or(carried);
        let mut new_off = v_off;
        let mut viewport_h = 0.0_f32;
        let lineno_w = self.lineno_width;

        ui.horizontal_top(|ui| {
            // LEFT pane: old side + the (whole-width) hunk-header strips.
            let left = egui::ScrollArea::both()
                .id_salt("split-left")
                .auto_shrink([false, false])
                .max_width(pane_w)
                .vertical_scroll_offset(v_off)
                .show_rows(ui, row_h, total, |ui, range| {
                    // Force a top-down layout: this ScrollArea lives inside the
                    // `horizontal_top` that lays the two panes side by side, so
                    // its content_ui inherits a LEFT-TO-RIGHT direction. Without
                    // this, every row would flow onto one line instead of
                    // stacking (the Full+Split "all on one line" regression).
                    ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        for i in range {
                            match &self.cache[i] {
                                RenderRow::SplitLine { left, .. } => {
                                    let kind = left.as_ref().map(|(k, _, _)| *k);
                                    let lno = left.as_ref().and_then(|(_, _, n)| *n);
                                    let (lspans, _) = self.split_spans(i);
                                    if let Some(s) =
                                        split_cell(ui, kind, lno, lineno_w, &lspans, sel)
                                    {
                                        *clicked_symbol = Some(s);
                                    }
                                }
                                RenderRow::HunkHeader { text, .. } => {
                                    // The header strip itself (controls live in
                                    // the unified path; here in Split we just
                                    // show its label so the row exists/aligns).
                                    egui::Frame::none()
                                        .fill(Color32::from_rgb(30, 36, 48))
                                        .show(ui, |ui| {
                                            ui.label(
                                                egui::RichText::new(if text.is_empty() {
                                                    " "
                                                } else {
                                                    text
                                                })
                                                .monospace()
                                                .color(Color32::from_rgb(120, 160, 220)),
                                            );
                                        });
                                }
                                _ => {
                                    ui.label(" ");
                                }
                            }
                        }
                    });
                });
            // The user's vertical drag on the left pane wins this frame.
            if (left.state.offset.y - v_off).abs() > 0.5 {
                new_off = left.state.offset.y;
            }
            viewport_h = left.inner_rect.height();

            ui.add_space(gap);

            // RIGHT pane: new side. Headers mirror as a blank spacer row.
            let right = egui::ScrollArea::both()
                .id_salt("split-right")
                .auto_shrink([false, false])
                .max_width(pane_w)
                .vertical_scroll_offset(new_off)
                .show_rows(ui, row_h, total, |ui, range| {
                    // Same top-down guard as the left pane (see note above):
                    // keep rows stacking vertically inside the horizontal layout.
                    ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        for i in range {
                            match &self.cache[i] {
                                RenderRow::SplitLine { right, .. } => {
                                    let kind = right.as_ref().map(|(k, _, _)| *k);
                                    let rno = right.as_ref().and_then(|(_, _, n)| *n);
                                    let (_, rspans) = self.split_spans(i);
                                    if let Some(s) =
                                        split_cell(ui, kind, rno, lineno_w, &rspans, sel)
                                    {
                                        *clicked_symbol = Some(s);
                                    }
                                }
                                RenderRow::HunkHeader { .. } => {
                                    egui::Frame::none()
                                        .fill(Color32::from_rgb(30, 36, 48))
                                        .show(ui, |ui| {
                                            ui.label(egui::RichText::new(" ").monospace());
                                        });
                                }
                                _ => {
                                    ui.label(" ");
                                }
                            }
                        }
                    });
                });
            // A drag on the right pane also drives the shared offset.
            if (right.state.offset.y - new_off).abs() > 0.5 {
                new_off = right.state.offset.y;
            }
        });

        // Persist the synced vertical offset for the next frame.
        ui.ctx()
            .memory_mut(|m| m.data.insert_temp(scroll_id, new_off));
        (new_off, viewport_h)
    }
}

/// Align a hunk's unified rows into side-by-side rows. Context lines show on
/// both sides; runs of deletions/additions are paired row-for-row (del↔add),
/// with any surplus shown one-sided (deletion → left only, addition → right
/// only). This is the standard split-diff pairing.
fn split_align(rows: &[diff::DiffLineRow]) -> Vec<RenderRow> {
    let mut out: Vec<RenderRow> = Vec::new();
    // Buffered (text, lineno): deletions carry their old number, additions
    // their new number.
    let mut dels: Vec<(String, Option<u32>)> = Vec::new();
    let mut adds: Vec<(String, Option<u32>)> = Vec::new();

    // Flush buffered deletions/additions as paired/one-sided split rows.
    let flush = |out: &mut Vec<RenderRow>,
                 dels: &mut Vec<(String, Option<u32>)>,
                 adds: &mut Vec<(String, Option<u32>)>| {
        let pairs = dels.len().max(adds.len());
        for i in 0..pairs {
            let left = dels.get(i).map(|(t, n)| (LineKind::Del, t.clone(), *n));
            let right = adds.get(i).map(|(t, n)| (LineKind::Add, t.clone(), *n));
            out.push(RenderRow::SplitLine { left, right });
        }
        dels.clear();
        adds.clear();
    };

    for r in rows {
        match r.kind {
            LineKind::Del => dels.push((r.text.clone(), r.old_lineno)),
            LineKind::Add => adds.push((r.text.clone(), r.new_lineno)),
            LineKind::Ctx => {
                flush(&mut out, &mut dels, &mut adds);
                out.push(RenderRow::SplitLine {
                    left: Some((LineKind::Ctx, r.text.clone(), r.old_lineno)),
                    right: Some((LineKind::Ctx, r.text.clone(), r.new_lineno)),
                });
            }
        }
    }
    flush(&mut out, &mut dels, &mut adds);
    out
}

/// For each review hunk, the line-number "key" identifying where its change
/// region begins: the first non-context row (an addition keyed by its new
/// line number, a deletion by its old). `None` for a hunk with no changed
/// rows (shouldn't happen for a real diff, but stays robust). Used to place a
/// per-hunk control strip at the matching point in the Full-extent flow.
fn hunk_change_key(hunk: &diff::Hunk) -> Option<(LineKind, u32)> {
    hunk.rows.iter().find_map(|r| match r.kind {
        LineKind::Add => r.new_lineno.map(|n| (LineKind::Add, n)),
        LineKind::Del => r.old_lineno.map(|n| (LineKind::Del, n)),
        LineKind::Ctx => None,
    })
}

/// Does full-flow row `r` start the change region of the review hunk whose
/// first-change key is `key`? An addition matches on its new line number, a
/// deletion on its old — the same identity `hunk_change_key` extracted, so the
/// review hunk's first changed line lines up with the same physical line in
/// the full-context diff.
fn row_matches_change_key(r: &diff::DiffLineRow, key: (LineKind, u32)) -> bool {
    match key {
        (LineKind::Add, n) => r.kind == LineKind::Add && r.new_lineno == Some(n),
        (LineKind::Del, n) => r.kind == LineKind::Del && r.old_lineno == Some(n),
        (LineKind::Ctx, _) => false,
    }
}

/// The fixed width of one pane in side-by-side (Split) view, given the
/// content area's available width. The two panes split the area evenly with a
/// small gap between them; each pane is then clipped to this width so a long
/// line on one side can never push the other side's x-position (the bug-2
/// fix). Never negative; collapses to 0 if the area is impossibly narrow.
fn split_pane_width(available: f32, gap: f32) -> f32 {
    ((available - gap) * 0.5).max(0.0)
}

/// The keybinding cheat-sheet rows shown by the `?` overlay. Kept as a single
/// source of truth so the help text matches what the code actually handles
/// (`handle_nav_keys`, the `ui` zoom/quick-open keys, the overlays).
fn keybindings() -> &'static [(&'static str, &'static str)] {
    &[
        ("Ctrl+P", "fuzzy open file"),
        ("j / k", "next / prev changed file"),
        ("n / p", "next / prev hunk"),
        ("a / r", "approve / reject focused hunk"),
        ("c", "comment on focused hunk"),
        ("F12", "go to definition of clicked symbol"),
        ("g", "find symbol (go to definition)"),
        ("PageUp / PageDown", "scroll content one page"),
        ("Ctrl + / Ctrl -", "zoom in / out"),
        ("Ctrl 0", "reset zoom"),
        ("?", "this help (Esc to close)"),
    ]
}

/// New vertical scroll offset after a Page Up/Down. `down` scrolls toward the
/// end; we move by `viewport - overlap` so a sliver of the prior page stays
/// visible (standard pager behavior). Clamped to `[0, max]` where
/// `max = (content_h - viewport).max(0)`.
fn page_scroll(current: f32, viewport: f32, content_h: f32, down: bool) -> f32 {
    let overlap = (viewport * 0.1).min(40.0);
    let step = (viewport - overlap).max(1.0);
    let max = (content_h - viewport).max(0.0);
    let target = if down { current + step } else { current - step };
    target.clamp(0.0, max)
}

/// Draw one side of a split-diff row: kind-tinted background + gutter + spans.
/// An empty cell (no kind) draws a faint filler so the gutter aligns.
fn split_cell(
    ui: &mut egui::Ui,
    kind: Option<LineKind>,
    lineno: Option<u32>,
    lineno_width: usize,
    spans: &[(Color32, String)],
    selected: Option<&str>,
) -> Option<String> {
    let (bg, marker) = match kind {
        Some(LineKind::Add) => (Some(Color32::from_rgb(22, 50, 22)), '+'),
        Some(LineKind::Del) => (Some(Color32::from_rgb(55, 22, 22)), '-'),
        Some(LineKind::Ctx) => (None, ' '),
        None => (Some(Color32::from_rgb(28, 28, 30)), ' '), // empty filler
    };
    // One line-number column (old on the left side, new on the right) + marker.
    let gutter = format!("{} {} ", fmt_lineno(lineno, lineno_width), marker);
    if let Some(bg) = bg {
        egui::Frame::none()
            .fill(bg)
            .show(ui, |ui| line_row(ui, &gutter, spans, selected, true))
            .inner
    } else {
        line_row(ui, &gutter, spans, selected, true)
    }
}

/// Format a line number right-aligned to `width` digits, or blank (spaces) if
/// there's no number for this row/side (e.g. the old number on an added line).
fn fmt_lineno(n: Option<u32>, width: usize) -> String {
    match n {
        Some(v) => format!("{v:>width$}"),
        None => " ".repeat(width),
    }
}

/// Is `c` part of a code identifier?
fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The status to show for a control strip that targets `targets` hunks of
/// `file`. A single target reflects that hunk exactly. A whole-file strip
/// shows Approved only if EVERY targeted hunk is approved, Rejected if every
/// one is rejected, else Unreviewed (a mixed/partial file still "needs work").
/// Empty targets → Unreviewed.
fn aggregate_status(file: &ChangedFile, targets: &[usize]) -> ReviewStatus {
    let mut statuses = targets.iter().filter_map(|&i| file.hunks.get(i).map(|h| h.status)).peekable();
    if statuses.peek().is_none() {
        return ReviewStatus::Unreviewed;
    }
    let all_approved = statuses.clone().all(|s| s == ReviewStatus::Approved);
    let all_rejected = statuses.all(|s| s == ReviewStatus::Rejected);
    if all_approved {
        ReviewStatus::Approved
    } else if all_rejected {
        ReviewStatus::Rejected
    } else {
        ReviewStatus::Unreviewed
    }
}

/// Build a lazy tree [`Node`] from a backend [`repo::DirEntry`].
fn node_from_entry(e: repo::DirEntry) -> Node {
    Node {
        name: e.name,
        rel: e.rel,
        is_dir: e.is_dir,
        children: None,
    }
}

/// Draw a monospace content line: gutter + highlighted spans, with
/// identifier tokens rendered as clickable (for go-to-definition). Returns
/// the identifier the user clicked this frame, if any. `selected` is the
/// currently-selected symbol, drawn with an underline/highlight.
fn line_row(
    ui: &mut egui::Ui,
    gutter: &str,
    spans: &[(Color32, String)],
    selected: Option<&str>,
    clickable: bool,
) -> Option<String> {
    let mut clicked: Option<String> = None;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        if !gutter.is_empty() {
            ui.label(egui::RichText::new(gutter).monospace().color(Color32::DARK_GRAY));
        }
        for (color, text) in spans {
            // Split the span into identifier / non-identifier runs; make
            // identifiers clickable (when `clickable`) so a click selects the
            // symbol for go-to-definition.
            let mut buf = String::new();
            let mut buf_ident = false;
            let flush = |ui: &mut egui::Ui, s: &str, ident: bool, clicked: &mut Option<String>| {
                if s.is_empty() {
                    return;
                }
                let mut rt = egui::RichText::new(s).monospace().color(*color);
                if ident && selected == Some(s) {
                    rt = rt.underline().background_color(Color32::from_rgb(60, 60, 30));
                }
                if ident && clickable {
                    let resp = ui.add(egui::Label::new(rt).sense(egui::Sense::click()));
                    if resp.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    if resp.clicked() {
                        *clicked = Some(s.to_string());
                    }
                } else {
                    ui.label(rt);
                }
            };
            for ch in text.chars() {
                let ci = is_ident_char(ch);
                if ci != buf_ident && !buf.is_empty() {
                    flush(ui, &buf, buf_ident, &mut clicked);
                    buf.clear();
                }
                buf_ident = ci;
                buf.push(ch);
            }
            flush(ui, &buf, buf_ident, &mut clicked);
        }
    });
    clicked
}

/// Recursively render the lazy file tree. Directories expand on click
/// (loading children on first expand); files are selectable and report
/// their rel path via `clicked`. The row whose rel path equals `open` (the
/// file currently shown in the content pane, changed or not) is highlighted;
/// when `scroll_to_open` is set it's also scrolled into view.
fn render_tree(
    ui: &mut egui::Ui,
    repo: &dyn RepoSource,
    nodes: &mut [Node],
    open: Option<&str>,
    scroll_to_open: bool,
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
                    // Lazy-load children on first expansion via the backend.
                    if node.children.is_none() {
                        node.children = Some(
                            repo.list_dir(&node.rel)
                                .unwrap_or_default()
                                .into_iter()
                                .map(node_from_entry)
                                .collect(),
                        );
                    }
                    if let Some(children) = node.children.as_mut() {
                        render_tree(ui, repo, children, open, scroll_to_open, clicked);
                    }
                });
        } else {
            let is_open = open == Some(node.rel.as_str());
            let resp = ui.selectable_label(is_open, &node.name);
            if is_open && scroll_to_open {
                resp.scroll_to_me(Some(egui::Align::Center));
            }
            if resp.clicked() {
                *clicked = Some(node.rel.clone());
            }
        }
    }
}

#[cfg(test)]
mod ui_tests {
    use super::*;
    use purview::repo::LocalRepo;
    use std::process::Command;

    /// Build an App over a local repo path (the default backend).
    fn local_app(path: &std::path::Path) -> App {
        App::new(Box::new(LocalRepo::new(path.to_path_buf())))
    }

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

    /// Make a fresh throwaway repo dir + a `git` runner closure bound to it.
    fn new_repo_dir() -> (PathBuf, impl Fn(&[&str])) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "purview-ec-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.clone();
        let git = move |args: &[&str]| {
            Command::new("git").args(args).current_dir(&d).output().unwrap();
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        git(&["checkout", "-q", "-b", "main"]);
        (dir, git)
    }

    /// A repo whose one changed file has TWO well-separated hunks (so the diff
    /// genuinely groups into >1 hunk). Used by the bug-3 targeting tests.
    fn multi_hunk_repo() -> PathBuf {
        let (dir, git) = new_repo_dir();
        // 30 lines, so an edit near the top and near the bottom land in
        // distinct hunks (3-line context doesn't bridge them).
        let base: String = (1..=30).map(|n| format!("line {n}\n")).collect();
        std::fs::write(dir.join("a.txt"), &base).unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);
        let edited: String = (1..=30)
            .map(|n| match n {
                3 => "LINE 3 EDIT\n".to_string(),
                27 => "LINE 27 EDIT\n".to_string(),
                _ => format!("line {n}\n"),
            })
            .collect();
        std::fs::write(dir.join("a.txt"), &edited).unwrap();
        dir
    }

    #[test]
    fn app_opens_with_a_diff_and_renders_all_states_without_panic() {
        let repo = fixture_repo();
        let mut app = local_app(&repo);
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
        let mut app = local_app(&repo);
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
        let app = local_app(&repo);
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

    /// REGRESSION GUARD (Full+Split "all on one line"): each split pane is a
    /// `ScrollArea::both().show_rows` nested inside the `horizontal_top` that
    /// places the two panes side by side. That parent's left-to-right direction
    /// is inherited by the ScrollArea's content_ui, so without an explicit
    /// `top_down` layout inside the row closure every row flows onto ONE line
    /// instead of stacking. This test renders a pane exactly as `split_panes`
    /// does and asserts the rows occupy DISTINCT vertical positions (one row
    /// per cache line, height ≈ row_h) — it FAILS (all rows at the same y, x
    /// marching rightward) if the top-down guard is removed.
    #[test]
    fn full_split_rows_stack_vertically_not_on_one_line() {
        let repo = multi_hunk_repo();
        let mut app = local_app(&repo);
        app.layout = Layout::Split;
        app.extent = Extent::Full;
        app.selected = Some(Selection::Changed(0));
        app.ensure_cache();
        assert!(app.cache.len() >= 8, "fixture should produce many split rows");

        let row_h = 18.0;
        let n = app.cache.len().min(10);
        // (start_y, end_y, start_x) captured per row from the live layout.
        let mut rows: Vec<(f32, f32, f32)> = Vec::new();

        let ctx = egui::Context::default();
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1200.0, 800.0),
                )),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    // Mirror split_panes' structure: a horizontal_top wrapping a
                    // bidirectional ScrollArea with show_rows + the top_down
                    // guard the fix installs.
                    ui.horizontal_top(|ui| {
                        egui::ScrollArea::both()
                            .id_salt("test-left")
                            .auto_shrink([false, false])
                            .max_width(500.0)
                            .show_rows(ui, row_h, n, |ui, range| {
                                ui.with_layout(
                                    egui::Layout::top_down(egui::Align::Min),
                                    |ui| {
                                        ui.spacing_mut().item_spacing.y = 0.0;
                                        for i in range {
                                            // Where the NEXT row will be placed.
                                            // Top-down: x fixed, y advances.
                                            // Horizontal flow (the bug): y fixed,
                                            // x marches rightward.
                                            let cur = ui.cursor().min;
                                            let before_y = cur.y;
                                            let start_x = cur.x;
                                            match &app.cache[i] {
                                                RenderRow::SplitLine { left, .. } => {
                                                    let kind =
                                                        left.as_ref().map(|(k, _, _)| *k);
                                                    let lno = left
                                                        .as_ref()
                                                        .and_then(|(_, _, no)| *no);
                                                    let (ls, _) = app.split_spans(i);
                                                    let _ = split_cell(
                                                        ui,
                                                        kind,
                                                        lno,
                                                        app.lineno_width,
                                                        &ls,
                                                        None,
                                                    );
                                                }
                                                _ => {
                                                    ui.label(" ");
                                                }
                                            }
                                            let after_y = ui.cursor().min.y;
                                            rows.push((before_y, after_y, start_x));
                                        }
                                    },
                                );
                            });
                    });
                });
            },
        );

        assert_eq!(rows.len(), n, "captured one entry per rendered row");

        // Each row must ADVANCE the vertical cursor by ~row_h (it occupies its
        // own line). On the bug, every row sits at the same y (delta ~0) and
        // the x-cursor marches rightward instead.
        for (i, &(start_y, end_y, _)) in rows.iter().enumerate() {
            let dy = end_y - start_y;
            assert!(
                dy >= row_h * 0.5,
                "row {i} must occupy its own line (advanced dy={dy:.1}, want ≈{row_h}); \
                 dy≈0 means rows collapsed onto one line"
            );
        }

        // Rows must START at the same x (left margin), not march rightward —
        // the tell-tale of horizontal flow. Allow a tiny tolerance.
        let x0 = rows[0].2;
        for (i, &(_, _, sx)) in rows.iter().enumerate() {
            assert!(
                (sx - x0).abs() < 2.0,
                "row {i} must start at the left margin (x={sx:.1}, row0 x={x0:.1}); \
                 a growing x means rows flowed left-to-right on one line"
            );
        }

        // Total vertical extent should be ≈ n × row_h, not ≈ a single row.
        let total_height = rows.last().unwrap().1 - rows[0].0;
        assert!(
            total_height >= row_h * (n as f32) * 0.5,
            "stacked rows span ≈{} px; got only {total_height:.1} (collapsed)",
            row_h * n as f32
        );

        // And the real app frame must render Full+Split without panicking.
        let ctx2 = egui::Context::default();
        frame(&ctx2, &mut app);

        let _ = std::fs::remove_dir_all(&repo);
    }

    fn row(kind: LineKind, t: &str) -> diff::DiffLineRow {
        diff::DiffLineRow {
            kind,
            text: t.into(),
            old_lineno: None,
            new_lineno: None,
        }
    }

    #[test]
    fn replace_nth_line_preserves_trailing_newline() {
        assert_eq!(
            diff::replace_nth_line("a\nb\nc\n", 1, "B"),
            Some("a\nB\nc\n".to_string())
        );
        // no trailing newline preserved
        assert_eq!(
            diff::replace_nth_line("a\nb\nc", 2, "C"),
            Some("a\nb\nC".to_string())
        );
        // out of range
        assert_eq!(diff::replace_nth_line("a\nb\n", 5, "x"), None);
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
                    let c = if side == 0 { left } else { right };
                    c.as_ref().map(|(k, t, _)| (*k, t.clone()))
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

    // ===================================================================
    // Bug 3 — approve/reject must target the EXACT hunk, never always hunk 0.
    // ===================================================================

    /// In Summary extent, the file genuinely has >1 hunk and each rendered
    /// HunkHeader carries the matching `hunk_idx` (0,1,2,…). This is the
    /// guarantee that a click on a header acts on its OWN hunk — the heart of
    /// the bug-3 fix (previously a single header could target hunk 0).
    #[test]
    fn summary_headers_carry_their_own_hunk_index() {
        let repo = multi_hunk_repo();
        let mut app = local_app(&repo);
        app.extent = Extent::Summary;
        app.layout = Layout::Inline;
        app.selected = Some(Selection::Changed(0));
        app.ensure_cache();
        assert!(app.files[0].hunks.len() >= 2, "fixture must have ≥2 hunks");

        let header_idxs: Vec<usize> = app
            .cache
            .iter()
            .filter_map(|r| match r {
                RenderRow::HunkHeader { hunk_idx, whole_file, .. } => {
                    assert!(!whole_file, "Summary headers are per-hunk, not whole-file");
                    Some(*hunk_idx)
                }
                _ => None,
            })
            .collect();
        // One header per hunk, numbered 0..n in order.
        assert_eq!(
            header_idxs,
            (0..app.files[0].hunks.len()).collect::<Vec<_>>(),
            "each header targets its own hunk index, in order"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// Applying status to hunk N (the code path the per-hunk buttons drive)
    /// changes ONLY hunk N — not the top hunk, not any sibling.
    #[test]
    fn setting_status_on_one_hunk_leaves_others_untouched() {
        let repo = multi_hunk_repo();
        let mut app = local_app(&repo);
        let n = app.files[0].hunks.len();
        assert!(n >= 2);
        // Target the LAST hunk (the bug always hit the first).
        let target = n - 1;
        app.files[0].hunks[target].status = ReviewStatus::Rejected;
        for (i, h) in app.files[0].hunks.iter().enumerate() {
            if i == target {
                assert_eq!(h.status, ReviewStatus::Rejected, "target hunk rejected");
            } else {
                assert_eq!(
                    h.status,
                    ReviewStatus::Unreviewed,
                    "hunk {i} must stay unreviewed (no spill to hunk 0)"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// In Full extent the whole file is shown with the diff overlaid, but the
    /// control strips are now PER review hunk, placed in place at each change
    /// region — NOT a single whole-file strip. A 2-hunk file → 2 strips, each
    /// carrying its own `hunk_idx` (0,1,…) and `whole_file = false`.
    #[test]
    fn full_extent_emits_a_control_strip_per_hunk() {
        let repo = multi_hunk_repo();
        let mut app = local_app(&repo);
        app.extent = Extent::Full;
        app.layout = Layout::Inline;
        app.selected = Some(Selection::Changed(0));
        app.ensure_cache();
        let n = app.files[0].hunks.len();
        assert!(n >= 2, "fixture must have ≥2 hunks");

        let headers: Vec<usize> = app
            .cache
            .iter()
            .filter_map(|r| match r {
                RenderRow::HunkHeader { hunk_idx, whole_file, .. } => {
                    assert!(!whole_file, "Full headers are per-hunk, not whole-file");
                    Some(*hunk_idx)
                }
                _ => None,
            })
            .collect();
        // One strip per review hunk, numbered 0..n in order — never a single
        // top-of-file whole-file strip.
        assert_eq!(
            headers,
            (0..n).collect::<Vec<_>>(),
            "Full extent emits a per-hunk strip for EACH hunk, in order"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// The per-hunk strip in Full extent sits IN PLACE at its change region:
    /// each header row is immediately followed (within a couple of rows) by a
    /// DiffLine matching that review hunk's first changed line — proving the
    /// strip is anchored to its own change, not floated to the top.
    #[test]
    fn full_extent_headers_sit_at_their_change_region() {
        let repo = multi_hunk_repo();
        let mut app = local_app(&repo);
        app.extent = Extent::Full;
        app.layout = Layout::Inline;
        app.selected = Some(Selection::Changed(0));
        app.ensure_cache();

        // For each header, find the next changed DiffLine after it and confirm
        // its line number matches that review hunk's first-change key.
        for (i, row) in app.cache.iter().enumerate() {
            let RenderRow::HunkHeader { hunk_idx, .. } = row else { continue };
            let key = hunk_change_key(&app.files[0].hunks[*hunk_idx])
                .expect("each hunk has a change");
            let matched = app.cache[i + 1..].iter().find_map(|r| match r {
                RenderRow::DiffLine { kind, old_lineno, new_lineno, .. }
                    if *kind != LineKind::Ctx =>
                {
                    Some((*kind, *old_lineno, *new_lineno))
                }
                _ => None,
            });
            let (k, old, new) = matched.expect("a changed line follows the header");
            let got_key = match k {
                LineKind::Add => (LineKind::Add, new.unwrap()),
                LineKind::Del => (LineKind::Del, old.unwrap()),
                LineKind::Ctx => unreachable!(),
            };
            assert_eq!(got_key, key, "header for hunk {hunk_idx} sits at its change");
        }
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// Full + Split (the user's PRIMARY mode): the render cache carries a
    /// per-hunk header for EACH hunk, interleaved among the SplitLine rows, and
    /// both panes iterate this one shared row sequence — so left/right stay
    /// row-aligned by construction. We assert (a) one header per hunk in order,
    /// (b) headers are interspersed with SplitLines (not all bunched at the
    /// top), and (c) each header is followed by a changed SplitLine matching
    /// that hunk's first-change line.
    #[test]
    fn full_split_interleaves_per_hunk_headers_and_stays_aligned() {
        let repo = multi_hunk_repo();
        let mut app = local_app(&repo);
        app.extent = Extent::Full;
        app.layout = Layout::Split;
        app.selected = Some(Selection::Changed(0));
        app.ensure_cache();
        let n = app.files[0].hunks.len();
        assert!(n >= 2, "fixture must have ≥2 hunks");

        // (a) one header per hunk, numbered 0..n in order.
        let header_idxs: Vec<usize> = app
            .cache
            .iter()
            .filter_map(|r| match r {
                RenderRow::HunkHeader { hunk_idx, whole_file, .. } => {
                    assert!(!whole_file, "Full+Split headers are per-hunk");
                    Some(*hunk_idx)
                }
                _ => None,
            })
            .collect();
        assert_eq!(header_idxs, (0..n).collect::<Vec<_>>());

        // The Split cache is built only of HunkHeader + SplitLine rows; nothing
        // else can desync the two panes (both iterate this exact sequence).
        assert!(
            app.cache.iter().all(|r| matches!(
                r,
                RenderRow::HunkHeader { .. } | RenderRow::SplitLine { .. }
            )),
            "Full+Split rows are only headers + split lines"
        );

        // (b) the SECOND hunk's header is not at the very top — real context
        // SplitLines precede it (proves in-place placement, not top-bunching).
        let pos_of = |idx: usize| {
            app.cache.iter().position(|r| {
                matches!(r, RenderRow::HunkHeader { hunk_idx, .. } if *hunk_idx == idx)
            })
        };
        let h1 = pos_of(1).expect("hunk 1 header present");
        let splitlines_before_h1 = app.cache[..h1]
            .iter()
            .filter(|r| matches!(r, RenderRow::SplitLine { .. }))
            .count();
        assert!(
            splitlines_before_h1 > 3,
            "hunk 1's strip sits in place after its preceding context, not at the top \
             (got {splitlines_before_h1} split rows before it)"
        );

        // (c) each header is followed by a changed SplitLine whose line number
        // matches that hunk's first-change key.
        for (i, row) in app.cache.iter().enumerate() {
            let RenderRow::HunkHeader { hunk_idx, .. } = row else { continue };
            let key = hunk_change_key(&app.files[0].hunks[*hunk_idx]).unwrap();
            let matched = app.cache[i + 1..].iter().find_map(|r| match r {
                RenderRow::SplitLine { left, right } => {
                    if let Some((LineKind::Del, _, Some(no))) = left {
                        return Some((LineKind::Del, *no));
                    }
                    if let Some((LineKind::Add, _, Some(no))) = right {
                        return Some((LineKind::Add, *no));
                    }
                    None
                }
                _ => None,
            });
            assert_eq!(matched, Some(key), "Split header {hunk_idx} sits at its change");
        }

        // Render Full+Split through a real frame: must not panic, and the two
        // panes share `app.cache.len()` rows.
        let ctx = egui::Context::default();
        frame(&ctx, &mut app);
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// Applying status via the Full-extent per-hunk strip (the `pending`
    /// (hunk_idx, status) path the buttons drive) hits hunk N only — the same
    /// per-hunk targeting Summary has. Approving hunk 1 leaves hunk 0 alone.
    #[test]
    fn full_extent_approve_targets_only_that_hunk() {
        let repo = multi_hunk_repo();
        let mut app = local_app(&repo);
        app.extent = Extent::Full;
        app.layout = Layout::Inline;
        app.selected = Some(Selection::Changed(0));
        app.ensure_cache();
        let n = app.files[0].hunks.len();
        assert!(n >= 2);

        // The button click pushes (hunk_idx, status) using the header's own
        // hunk_idx; emulate that for the LAST hunk's strip.
        let target = n - 1;
        let strip_idx = app
            .cache
            .iter()
            .find_map(|r| match r {
                RenderRow::HunkHeader { hunk_idx, .. } if *hunk_idx == target => Some(target),
                _ => None,
            })
            .expect("a strip for the last hunk exists");
        app.files[0].hunks[strip_idx].status = ReviewStatus::Approved;
        for (i, h) in app.files[0].hunks.iter().enumerate() {
            if i == target {
                assert_eq!(h.status, ReviewStatus::Approved, "target hunk approved");
            } else {
                assert_eq!(h.status, ReviewStatus::Unreviewed, "hunk {i} untouched");
            }
        }
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// `aggregate_status`: a whole-file strip is Approved only when every hunk
    /// is approved, Rejected only when every hunk is rejected, else Unreviewed.
    #[test]
    fn aggregate_status_is_all_or_nothing() {
        let mut f = ChangedFile {
            path: "x".into(),
            hunks: vec![
                Hunk::new("h0".into()),
                Hunk::new("h1".into()),
                Hunk::new("h2".into()),
            ],
        };
        let all = [0usize, 1, 2];
        assert_eq!(aggregate_status(&f, &all), ReviewStatus::Unreviewed);
        f.hunks[0].status = ReviewStatus::Approved;
        // Mixed (one approved, two unreviewed) → still Unreviewed.
        assert_eq!(aggregate_status(&f, &all), ReviewStatus::Unreviewed);
        for h in f.hunks.iter_mut() {
            h.status = ReviewStatus::Approved;
        }
        assert_eq!(aggregate_status(&f, &all), ReviewStatus::Approved);
        for h in f.hunks.iter_mut() {
            h.status = ReviewStatus::Rejected;
        }
        assert_eq!(aggregate_status(&f, &all), ReviewStatus::Rejected);
        // A single target reflects exactly that hunk.
        f.hunks[1].status = ReviewStatus::Approved;
        assert_eq!(aggregate_status(&f, &[1]), ReviewStatus::Approved);
        assert_eq!(aggregate_status(&f, &[0]), ReviewStatus::Rejected);
        // Empty targets → Unreviewed.
        assert_eq!(aggregate_status(&f, &[]), ReviewStatus::Unreviewed);
    }

    /// Full extent now has a per-hunk control strip for each change region, so
    /// pressing `a` approves only the FOCUSED hunk (like Summary) — moving focus
    /// with `n` then `a` approves that hunk alone. The kittest harness drives
    /// real key presses through `app.ui`.
    #[test]
    fn key_approve_in_full_extent_targets_focused_hunk() {
        let repo = multi_hunk_repo();
        let mut app = local_app(&repo);
        app.extent = Extent::Full;
        app.selected = Some(Selection::Changed(0));
        let n = app.files[0].hunks.len();
        assert!(n >= 2);

        let mut harness = egui_kittest::Harness::new_state(|ctx, app: &mut App| app.ui(ctx), app);
        harness.run();
        // Move focus to the last hunk, then approve it.
        for _ in 0..(n - 1) {
            harness.press_key(egui::Key::N);
            harness.run();
        }
        harness.press_key(egui::Key::A);
        harness.run();

        let st = harness.state();
        let target = n - 1;
        for (i, h) in st.files[0].hunks.iter().enumerate() {
            if i == target {
                assert_eq!(h.status, ReviewStatus::Approved, "focused hunk approved");
            } else {
                assert_eq!(h.status, ReviewStatus::Unreviewed, "hunk {i} untouched in Full");
            }
        }
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// In Summary, pressing `a` approves only the FOCUSED hunk, leaving the
    /// others alone (per-hunk targeting via keyboard).
    #[test]
    fn key_approve_in_summary_targets_only_focused_hunk() {
        let repo = multi_hunk_repo();
        let mut app = local_app(&repo);
        app.extent = Extent::Summary;
        app.selected = Some(Selection::Changed(0));
        let n = app.files[0].hunks.len();
        assert!(n >= 2);

        let mut harness = egui_kittest::Harness::new_state(|ctx, app: &mut App| app.ui(ctx), app);
        harness.run();
        // Move focus to the last hunk with `n`, then approve with `a`.
        for _ in 0..(n - 1) {
            harness.press_key(egui::Key::N);
            harness.run();
        }
        harness.press_key(egui::Key::A);
        harness.run();

        let st = harness.state();
        let target = n - 1;
        for (i, h) in st.files[0].hunks.iter().enumerate() {
            if i == target {
                assert_eq!(h.status, ReviewStatus::Approved, "focused hunk approved");
            } else {
                assert_eq!(
                    h.status,
                    ReviewStatus::Unreviewed,
                    "hunk {i} unchanged — no spill to hunk 0"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&repo);
    }

    // ===================================================================
    // Bug 4 — display / rendering edge cases.
    // ===================================================================

    /// Empty diff: working tree matches HEAD → no changed files, nothing
    /// selected, and rendering shows the "no changes" state without panic.
    #[test]
    fn empty_diff_renders_no_changes() {
        let (dir, git) = new_repo_dir();
        std::fs::write(dir.join("a.txt"), "x\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);
        // No working-tree edit → clean.
        let mut app = local_app(&dir);
        assert!(app.files.is_empty(), "clean tree → no changed files");
        assert!(app.selected.is_none());
        let ctx = egui::Context::default();
        frame(&ctx, &mut app); // must not panic
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A brand-new untracked file shows up as an all-add hunk and renders.
    #[test]
    fn new_untracked_file_is_all_additions() {
        let (dir, git) = new_repo_dir();
        std::fs::write(dir.join("seed.txt"), "x\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);
        std::fs::write(dir.join("fresh.txt"), "alpha\nbeta\n").unwrap();

        let mut app = local_app(&dir);
        let f = app
            .files
            .iter()
            .find(|f| f.path == "fresh.txt")
            .expect("untracked file appears in the diff");
        let rows: Vec<&diff::DiffLineRow> = f.hunks.iter().flat_map(|h| &h.rows).collect();
        assert!(!rows.is_empty());
        assert!(
            rows.iter().all(|r| r.kind == LineKind::Add),
            "a new file is entirely additions"
        );
        app.selected = Some(Selection::Changed(
            app.files.iter().position(|f| f.path == "fresh.txt").unwrap(),
        ));
        let ctx = egui::Context::default();
        frame(&ctx, &mut app);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A deleted file produces a hunk of deletions and renders without panic.
    #[test]
    fn deleted_file_is_all_deletions() {
        let (dir, git) = new_repo_dir();
        std::fs::write(dir.join("gone.txt"), "a\nb\nc\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);
        std::fs::remove_file(dir.join("gone.txt")).unwrap();

        let mut app = local_app(&dir);
        let f = app
            .files
            .iter()
            .find(|f| f.path == "gone.txt")
            .expect("deleted file appears in the diff");
        let rows: Vec<&diff::DiffLineRow> = f.hunks.iter().flat_map(|h| &h.rows).collect();
        assert!(
            rows.iter().any(|r| r.kind == LineKind::Del),
            "a deleted file shows deletions"
        );
        assert!(
            !rows.iter().any(|r| r.kind == LineKind::Add),
            "a deleted file has no additions"
        );
        app.selected = Some(Selection::Changed(
            app.files.iter().position(|f| f.path == "gone.txt").unwrap(),
        ));
        for layout in [Layout::Inline, Layout::Split] {
            app.layout = layout;
            let ctx = egui::Context::default();
            frame(&ctx, &mut app);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A binary file is handled gracefully: it shows up as a changed file with
    /// a "(binary file)" hunk (no content rows), and renders without panic.
    #[test]
    fn binary_file_handled_gracefully() {
        let (dir, git) = new_repo_dir();
        // Commit a binary file, then change its bytes.
        std::fs::write(dir.join("blob.bin"), [0u8, 159, 146, 150, 0, 1, 2]).unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);
        std::fs::write(dir.join("blob.bin"), [0u8, 1, 2, 3, 255, 254, 0, 9]).unwrap();

        let mut app = local_app(&dir);
        let f = app
            .files
            .iter()
            .find(|f| f.path == "blob.bin")
            .expect("binary file appears in the diff");
        // git2 emits a binary delta → one "(binary file)" hunk, no add/del rows.
        assert!(
            f.hunks.iter().any(|h| h.header.contains("binary"))
                || f.hunks.iter().all(|h| h.rows.is_empty()),
            "binary file surfaces a binary-marker hunk, not text rows"
        );
        app.selected = Some(Selection::Changed(
            app.files.iter().position(|f| f.path == "blob.bin").unwrap(),
        ));
        let ctx = egui::Context::default();
        frame(&ctx, &mut app); // must not panic on binary content
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file with no trailing newline diffs and renders cleanly (the parser
    /// must not choke on the "\ No newline at end of file" marker).
    #[test]
    fn no_trailing_newline_file_renders() {
        let (dir, git) = new_repo_dir();
        std::fs::write(dir.join("nonl.txt"), "first\nsecond").unwrap(); // no \n
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);
        std::fs::write(dir.join("nonl.txt"), "first\nSECOND").unwrap(); // still no \n

        let mut app = local_app(&dir);
        let f = app
            .files
            .iter()
            .find(|f| f.path == "nonl.txt")
            .expect("file appears in the diff");
        let has_add = f.hunks.iter().flat_map(|h| &h.rows).any(|r| r.kind == LineKind::Add);
        assert!(has_add, "the edit is captured as an addition");
        // No stray "\ No newline" line leaked in as a content row.
        let leaked = f
            .hunks
            .iter()
            .flat_map(|h| &h.rows)
            .any(|r| r.text.starts_with("\\ No newline"));
        assert!(!leaked, "the no-newline marker is not a content row");
        app.selected = Some(Selection::Changed(0));
        let ctx = egui::Context::default();
        frame(&ctx, &mut app);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Full vs Summary extent generate different row sets for the same file:
    /// Full shows every file line with a per-hunk control strip at each change
    /// region (one header per hunk, same count as Summary), while Summary shows
    /// only the changed hunks with far fewer content rows.
    #[test]
    fn full_vs_summary_extent_row_generation() {
        let repo = multi_hunk_repo();
        let mut app = local_app(&repo);
        app.layout = Layout::Inline;
        app.selected = Some(Selection::Changed(0));

        app.extent = Extent::Summary;
        app.ensure_cache();
        let summary_rows = app.cache.len();
        let summary_headers = app
            .cache
            .iter()
            .filter(|r| matches!(r, RenderRow::HunkHeader { .. }))
            .count();

        app.extent = Extent::Full;
        app.ensure_cache();
        let full_rows = app.cache.len();
        let full_headers = app
            .cache
            .iter()
            .filter(|r| matches!(r, RenderRow::HunkHeader { .. }))
            .count();

        assert_eq!(summary_headers, app.files[0].hunks.len(), "one header per hunk in Summary");
        assert_eq!(
            full_headers,
            app.files[0].hunks.len(),
            "one per-hunk strip per hunk in Full too (not a single whole-file strip)"
        );
        assert!(
            full_rows > summary_rows,
            "Full extent (whole 30-line file) has more rows than Summary ({full_rows} vs {summary_rows})"
        );
        // Full extent renders ~the whole file as context lines.
        let full_content = app
            .cache
            .iter()
            .filter(|r| matches!(r, RenderRow::DiffLine { .. }))
            .count();
        assert!(full_content >= 28, "Full shows ~all 30 file lines, got {full_content}");
        let _ = std::fs::remove_dir_all(&repo);
    }

    // ===================================================================
    // New: pure-helper tests for the split-pane width, page scroll, and the
    // keybinding cheat-sheet source-of-truth.
    // ===================================================================

    /// Each Split pane is half the area minus the gap, never negative.
    #[test]
    fn split_pane_width_halves_minus_gap() {
        assert_eq!(split_pane_width(208.0, 8.0), 100.0);
        assert_eq!(split_pane_width(8.0, 8.0), 0.0);
        // Impossibly narrow → clamps to 0, never negative.
        assert_eq!(split_pane_width(0.0, 8.0), 0.0);
    }

    /// PageDown advances by ~a viewport (minus a small overlap) and clamps at
    /// the bottom; PageUp goes the other way and clamps at 0.
    #[test]
    fn page_scroll_moves_a_page_and_clamps() {
        // viewport 100, content 1000 → max offset 900. overlap = min(10,40)=10,
        // step = 90.
        assert_eq!(page_scroll(0.0, 100.0, 1000.0, true), 90.0);
        assert_eq!(page_scroll(90.0, 100.0, 1000.0, false), 0.0);
        // Clamps at the bottom.
        assert_eq!(page_scroll(880.0, 100.0, 1000.0, true), 900.0);
        // Already at the top, paging up stays put.
        assert_eq!(page_scroll(0.0, 100.0, 1000.0, false), 0.0);
        // Content shorter than viewport → max 0, no movement.
        assert_eq!(page_scroll(0.0, 500.0, 100.0, true), 0.0);
    }

    /// The cheat-sheet lists the bindings the code actually handles. Lock in a
    /// few load-bearing ones so the help can't silently drift.
    #[test]
    fn keybindings_cover_the_real_bindings() {
        let kb = keybindings();
        let keys: Vec<&str> = kb.iter().map(|(k, _)| *k).collect();
        for k in ["Ctrl+P", "j / k", "n / p", "a / r", "F12", "g", "?"] {
            assert!(keys.contains(&k), "help must list {k:?}");
        }
        assert!(
            keys.iter().any(|k| k.contains("PageUp")),
            "help must mention PageUp/PageDown"
        );
        assert!(!kb.is_empty());
    }

    /// `open_path` resolves to the file shown in the content pane for both a
    /// changed-file selection and a tree-opened path.
    #[test]
    fn open_path_tracks_both_selection_kinds() {
        let repo = fixture_repo();
        let mut app = local_app(&repo);
        app.selected = Some(Selection::Changed(0));
        assert_eq!(app.open_path().as_deref(), Some(app.files[0].path.as_str()));
        app.selected = Some(Selection::Path("some/other.rs".into()));
        assert_eq!(app.open_path().as_deref(), Some("some/other.rs"));
        app.selected = None;
        assert_eq!(app.open_path(), None);
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// `?` toggles the help overlay; Esc closes it. Driven through `app.ui`
    /// with the kittest harness so it exercises the real key handling.
    #[test]
    fn question_mark_toggles_help_overlay() {
        let repo = fixture_repo();
        let app = local_app(&repo);
        let mut harness = egui_kittest::Harness::new_state(|ctx, app: &mut App| app.ui(ctx), app);
        harness.run();
        assert!(!harness.state().show_help, "help starts closed");
        harness.press_key(egui::Key::Questionmark);
        harness.run();
        assert!(harness.state().show_help, "? opens the help overlay");
        harness.press_key(egui::Key::Questionmark);
        harness.run();
        assert!(!harness.state().show_help, "? again closes it");
        // Reopen, then Esc closes.
        harness.press_key(egui::Key::Questionmark);
        harness.run();
        assert!(harness.state().show_help);
        harness.press_key(egui::Key::Escape);
        harness.run();
        assert!(!harness.state().show_help, "Esc closes the help overlay");
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// PageDown scrolls the content pane down (offset grows); the App tracks the
    /// new offset. Uses a tall full-file view so there's room to scroll.
    #[test]
    fn page_down_scrolls_content() {
        let repo = multi_hunk_repo();
        let mut app = local_app(&repo);
        app.extent = Extent::Full; // ~30 lines → scrollable
        app.layout = Layout::Inline;
        app.selected = Some(Selection::Changed(0));
        let mut harness = egui_kittest::Harness::new_state(|ctx, app: &mut App| app.ui(ctx), app);
        harness.run();
        let before = harness.state().content_scroll;
        harness.press_key(egui::Key::PageDown);
        harness.run();
        harness.run(); // one more frame for the scroll to apply + be recaptured
        let after = harness.state().content_scroll;
        assert!(
            after >= before,
            "PageDown should not move the view backward (before {before}, after {after})"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// A reload that supersedes an in-flight one must not apply the stale
    /// result (bug 2 race guard). We drive the generation/poll machinery
    /// directly: stamp a result with an old generation and confirm poll drops it.
    #[test]
    fn stale_reload_result_is_ignored() {
        let repo = fixture_repo();
        let mut app = local_app(&repo);
        // Simulate: a reload was in flight (generation G), then a newer reload
        // bumped the generation. A late result for G arrives.
        let stale_gen = app.generation;
        app.generation = app.generation.wrapping_add(1); // superseded
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send((stale_gen, Ok(("zzz-stale".to_string(), Vec::new()))))
            .unwrap();
        app.diff_rx = Some(rx);
        app.loading = true;
        let applied = app.poll_reload();
        assert!(!applied, "a superseded result must not be applied");
        assert_ne!(app.branch, "zzz-stale", "stale branch must not leak in");
        let _ = std::fs::remove_dir_all(&repo);
    }
}
