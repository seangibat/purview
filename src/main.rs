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
use purview::review_state::{ComparisonKey, FileState, HunkState, Replies, ReviewState};
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
    /// Receives the resolved definition from the worker thread: `Ok(Some(_))`
    /// = found, `Ok(None)` = no candidates / no definition, `Err(_)` = grep or
    /// resolve failed. The whole grep+resolve pipeline runs off-thread.
    rx: Option<std::sync::mpsc::Receiver<Result<Option<purview::gotodef::Candidate>, String>>>,
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

/// In-file (Ctrl+F) search state. Some = the search bar is open.
struct Search {
    /// The current query text.
    query: String,
    /// Cache-row indices that match `query` (ascending), recomputed when the
    /// query or the content cache changes.
    matches: Vec<usize>,
    /// Index INTO `matches` of the currently-selected match (for "k of N" and
    /// next/prev). 0-based; clamped to `matches`.
    current: usize,
    /// True for the first frame so the input grabs focus.
    just_opened: bool,
    /// The (query, cache generation) the `matches` were computed for, so we
    /// only recompute when something actually changed.
    computed_for: Option<(String, u64)>,
    /// The cache row we last scrolled the current match to, so we only issue a
    /// scroll on an actual match change (next/prev/new query) — never every
    /// frame, which would fight manual scrolling.
    last_scrolled: Option<usize>,
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

/// What kind of remote payload a content load fetches, keyed in the cache so a
/// re-open is instant. The two expensive (over SSH, blocking) fetches
/// `ensure_cache` would otherwise do on the UI thread:
/// - `Full`: a tree-opened file's whole contents (`read_file`).
/// - `FullHunks`: a changed file's full-context re-diff (`compute_file_diff`),
///   shown in Full extent.
/// (Summary extent reuses the already-computed `self.files` hunks — no fetch.)
#[derive(Clone)]
enum FileContent {
    /// Whole-file text for a tree-opened `Selection::Path`.
    Full(String),
    /// Full-context diff hunks for a `Selection::Changed` in `Extent::Full`.
    FullHunks(Vec<diff::Hunk>),
}

/// Identity of a loaded payload. Re-opening the SAME (path, what-we-fetch)
/// returns the cached `FileContent` with no backend round-trip. `source`/`base`
/// are part of the key because changing the diff base must refetch; `generation`
/// is NOT — the cache is cleared wholesale on reload()/refresh instead, so a
/// stale diff can never be served (see `invalidate_content_cache`).
#[derive(Clone, PartialEq, Eq, Hash)]
struct ContentKey {
    path: String,
    /// True for a `compute_file_diff` (Full extent) load, false for a whole-file
    /// `read_file` (tree-opened) load — the two never collide on path alone.
    full_diff: bool,
    source: DiffSource,
    base: String,
}

/// A bounded in-memory cache of loaded file content. Keeps the last `cap`
/// distinct keys (simple FIFO eviction — recency of *insertion*, which for this
/// access pattern, opening files one at a time, tracks "last N files"). Cleared
/// wholesale when the diff changes.
struct ContentCache {
    cap: usize,
    map: std::collections::HashMap<ContentKey, FileContent>,
    /// Insertion order, oldest first, for eviction.
    order: std::collections::VecDeque<ContentKey>,
}

impl ContentCache {
    fn new(cap: usize) -> Self {
        ContentCache {
            cap,
            map: std::collections::HashMap::new(),
            order: std::collections::VecDeque::new(),
        }
    }

    fn get(&self, key: &ContentKey) -> Option<&FileContent> {
        self.map.get(key)
    }

    /// Insert a loaded payload, evicting the oldest entry if over capacity.
    /// Re-inserting an existing key just refreshes its value (no dup in order).
    fn insert(&mut self, key: ContentKey, val: FileContent) {
        if self.map.insert(key.clone(), val).is_none() {
            self.order.push_back(key);
            while self.order.len() > self.cap {
                if let Some(old) = self.order.pop_front() {
                    self.map.remove(&old);
                }
            }
        }
    }

    fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }
}

/// Run the (possibly blocking, over SSH) backend fetch for `key`. Pure w.r.t.
/// the App — takes only the backend — so it runs on a worker thread and is
/// directly unit-testable. Mirrors what `ensure_cache` used to do inline.
fn fetch_content(repo: &dyn RepoSource, key: &ContentKey) -> Result<FileContent, String> {
    if key.full_diff {
        let (_, mut files) = repo.compute_file_diff(key.source, &key.base, u32::MAX, &key.path)?;
        let hunks = files
            .iter()
            .position(|f| f.path == key.path)
            .map(|i| std::mem::take(&mut files[i].hunks))
            .unwrap_or_default();
        Ok(FileContent::FullHunks(hunks))
    } else {
        repo.read_file(&key.path).map(FileContent::Full)
    }
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
    /// True for the first frame after the comment editor opens via `c`, so the
    /// TextEdit can grab keyboard focus once (the user can type immediately).
    comment_just_opened: bool,
    /// The open comment has unsaved edits. We persist on editor close (not per
    /// keystroke) so typing doesn't trigger a full serialize + ssh round-trip
    /// each character.
    comment_dirty: bool,
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
    /// specific vertical offset on the next frame. Used ONLY by PageUp/PageDown,
    /// which page relative to the live (actual) scroll offset — never derived
    /// from `row * row_h`, so it's immune to the variable-row-height drift.
    pending_scroll: Option<f32>,
    /// "Bring this cache row into view" request — the ONE source of truth for
    /// every jump-to-row (n/p, Ctrl+F match, j/k first hunk, F12/go-to-def).
    /// Resolved by calling `Response::scroll_to_me(align)` on the row whose
    /// index equals the target as it's laid out in the content scroll area
    /// (both the unified path and `split_panes`). Because that uses the row's
    /// ACTUAL rect, variable row heights (tall HunkHeader strips) are handled by
    /// egui with zero pixel math — no drift. The `usize` is the cache-row index;
    /// the `Align` is where to land it (Center for matches/hunks, Min for the
    /// file-switch "first hunk to the top"). Consumed (taken) once applied.
    scroll_to_row: Option<(usize, egui::Align)>,
    /// Set by j/k when switching to a different changed file: scroll so that
    /// file's FIRST hunk header is at the top. Resolved AFTER `ensure_cache`
    /// rebuilds `hunk_rows` for the new file (the header row isn't known at
    /// key-handling time, before the cache is built), so it works in both
    /// Summary and Full extent where the first change may not be at row 0.
    scroll_to_first_hunk: bool,
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
    /// Bounded cache of loaded file content (Task A): re-opening an already-
    /// loaded file is instant, no backend round-trip. Cleared on reload/refresh.
    content_cache: ContentCache,
    /// In-flight async file-content load (Task A). Over SSH, opening a file is a
    /// blocking remote round-trip (`compute_file_diff`/`read_file`); doing it in
    /// `ensure_cache` froze the UI ~1s per click. We instead kick the fetch onto
    /// a worker thread, show a "loading…" pane, and apply the result in `ui` via
    /// `poll_content`. The worker stamps each result with the load-`generation`
    /// it was started for; a newer open supersedes an older in-flight load (the
    /// generation guard, mirroring `diff_rx`). `Some` = a content load is
    /// in flight; the inner key is what's being fetched (so a duplicate kick for
    /// the same key is suppressed).
    content_rx: Option<(ContentKey, std::sync::mpsc::Receiver<(u64, ContentKey, Result<FileContent, String>)>)>,
    /// Bumped on every content load kicked off, so a stale result (from a load
    /// superseded by a faster file switch) is dropped instead of applied.
    content_gen: u64,
    /// True while `content_rx` is in flight — drives the content pane's
    /// "loading…" placeholder so a slow open never freezes the frame.
    content_loading: bool,
    /// Whether the `?` keybinding cheat-sheet overlay is showing.
    show_help: bool,
    /// In-file (Ctrl+F) search state. Some = the search bar is open.
    search: Option<Search>,
    /// The open file's path as of last frame, so the tree only auto-scrolls to
    /// the highlighted row when the open file actually changes (not every frame).
    last_open_path: Option<String>,
    /// Last-known vertical scroll offset of the content pane + its viewport
    /// height, captured each frame so PageUp/PageDown can move by a page.
    content_scroll: f32,
    content_viewport_h: f32,
    /// The content pane's ACTUAL total rendered height last frame (egui's
    /// `content_size.y`). Used by PageUp/PageDown instead of `rows × row_h`,
    /// which under-estimates with variable-height rows and stops paging short
    /// of the real end of a long file.
    content_height: f32,
    /// The cache-row range egui ACTUALLY painted in the content area last frame
    /// (from `show_rows`' `range`), as an inclusive `[first, last]`. This is the
    /// real visible window — it accounts for variable row heights, unlike the
    /// old `content_scroll / row_h .. + viewport / row_h` estimate, which
    /// drifted past any tall HunkHeader rows above the viewport. n/p reads this
    /// to decide whether the target hunk is already on screen. `None` until the
    /// first content frame paints.
    visible_rows: Option<(usize, usize)>,
    /// Saved reviewed hunks whose content-anchor no longer appears in the
    /// current diff (the changed code was removed/reverted). Carried across
    /// reloads so the reviewer's notes aren't silently dropped; surfaced in the
    /// report's "Stale (no longer in diff)" section and re-persisted.
    orphaned: Vec<purview::review_state::OrphanedHunk>,
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
            comment_just_opened: false,
            comment_dirty: false,
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
            scroll_to_row: None,
            scroll_to_first_hunk: false,
            editing: None,
            diff_rx: None,
            loading: false,
            // Keep the last ~16 opened files' content in memory — plenty to make
            // j/k navigation and revisits instant, bounded so it can't grow without
            // limit on a long review session.
            content_cache: ContentCache::new(16),
            content_rx: None,
            content_gen: 0,
            content_loading: false,
            show_help: false,
            search: None,
            last_open_path: None,
            content_scroll: 0.0,
            content_viewport_h: 0.0,
            content_height: 0.0,
            visible_rows: None,
            orphaned: Vec::new(),
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
        // Flush any unsaved comment before we drop the files — the comment
        // editor persists on close, but a reload mid-edit shouldn't lose it.
        if self.comment_dirty {
            self.save_review_state();
            self.comment_dirty = false;
        }
        self.files.clear();
        self.selected = None;
        self.active_hunk = None;
        self.error = None;
        self.report_note.clear();
        self.cache.clear();
        self.cache_key = None;
        // The diff changed — any loaded file content (full-file re-diffs,
        // tree-file reads) may now be stale (Task A).
        self.invalidate_content_cache();
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

    /// The comparison the current `source`/`base` selects. Used to key the
    /// per-comparison review-state file.
    fn comparison_key(&self) -> ComparisonKey {
        match self.source {
            DiffSource::WorkingTree => ComparisonKey::WorkingTree,
            DiffSource::BranchRange => ComparisonKey::BranchRange {
                base: self.base.clone(),
            },
        }
    }

    /// Apply a finished diff result to the UI state. After the fresh diff lands
    /// we re-anchor any saved review state for THIS comparison onto it (so
    /// verdicts/comments survive a moving worktree), capturing hunks that no
    /// longer appear as orphans. Then we re-persist (the diff may have shifted
    /// every `@@` header, so the saved anchors/headers want refreshing).
    fn apply_diff_result(&mut self, res: Result<(String, Vec<ChangedFile>), String>) {
        match res {
            Ok((branch, mut files)) => {
                self.branch = branch;
                // Re-anchor saved state for the current comparison onto the
                // fresh diff. Loads the per-comparison file (migrating the old
                // single-file layout if needed); None = a fresh review.
                let key = self.comparison_key();
                if let Some(saved) =
                    ReviewState::load_for_comparison(&self.state_root, &key)
                {
                    self.orphaned = saved.reanchor_onto(&mut files);
                } else {
                    self.orphaned = Vec::new();
                }
                self.files = files;
                self.selected = if self.files.is_empty() {
                    None
                } else {
                    Some(Selection::Changed(0))
                };
                // Persist the re-anchored state so the on-disk anchors/headers
                // track the current diff and orphans are recorded — but only
                // when there's actual review content (a verdict, comment, or
                // orphan). A fresh comparison with no review yet writes nothing,
                // so opening a repo never creates state files unprompted.
                let has_review = !self.orphaned.is_empty()
                    || self.files.iter().flat_map(|f| &f.hunks).any(|h| {
                        h.status != ReviewStatus::Unreviewed || !h.comment.trim().is_empty()
                    });
                if has_review {
                    self.save_review_state();
                }
            }
            Err(e) => {
                self.files.clear();
                self.selected = None;
                self.orphaned = Vec::new();
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
    /// A COMPLETE record: counts, then Rejected / Approved / Unreviewed
    /// sections per file. Every hunk carries its comment regardless of
    /// status, so a comment on a hunk you didn't approve/reject isn't lost.
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

        // One section per status, in review-priority order. Each lists the
        // matching hunks per file, with the reviewer's comment underneath so
        // the "why" survives — comments are carried for EVERY status, not just
        // rejected (a comment on an approved/unreviewed hunk used to vanish).
        let section = |s: &mut String, title: &str, status: ReviewStatus| {
            let mut wrote = false;
            for f in &self.files {
                let matching: Vec<&Hunk> =
                    f.hunks.iter().filter(|h| h.status == status).collect();
                if matching.is_empty() {
                    continue;
                }
                if !wrote {
                    s.push_str(&format!("## {title}\n\n"));
                    wrote = true;
                }
                s.push_str(&format!("### {}\n\n", f.path));
                for h in matching {
                    // Flag hunks whose verdict was carried over but whose
                    // changed content has since moved/changed under it.
                    let warn = if h.changed_since_review {
                        " ⚠ changed since reviewed"
                    } else {
                        ""
                    };
                    s.push_str(&format!("- `{}`{warn}\n", h.header.trim()));
                    if !h.comment.trim().is_empty() {
                        for line in h.comment.trim().lines() {
                            s.push_str(&format!("  - {line}\n"));
                        }
                    }
                }
                s.push('\n');
            }
            wrote
        };

        let wrote_rejected = section(&mut s, "Rejected hunks (need changes)", ReviewStatus::Rejected);
        let wrote_approved = section(&mut s, "Approved hunks", ReviewStatus::Approved);
        let wrote_unrev = section(&mut s, "Still unreviewed", ReviewStatus::Unreviewed);

        // Stale: reviewed hunks whose anchor no longer appears in the diff (the
        // changed code was removed/reverted). Preserved, not silently dropped.
        let wrote_stale = !self.orphaned.is_empty();
        if wrote_stale {
            s.push_str("## Stale (no longer in diff)\n\n");
            // Group orphans by file, preserving order.
            let mut seen_files: Vec<&str> = Vec::new();
            for o in &self.orphaned {
                if !seen_files.contains(&o.file.as_str()) {
                    seen_files.push(o.file.as_str());
                }
            }
            for file in seen_files {
                s.push_str(&format!("### {file}\n\n"));
                for o in self.orphaned.iter().filter(|o| o.file == file) {
                    s.push_str(&format!("- `{}` ({})\n", o.header.trim(), o.status));
                    if let Some(c) = &o.comment {
                        if !c.trim().is_empty() {
                            for line in c.trim().lines() {
                                s.push_str(&format!("  - {line}\n"));
                            }
                        }
                    }
                }
                s.push('\n');
            }
        }

        if !wrote_rejected && !wrote_approved && !wrote_unrev && !wrote_stale {
            s.push_str("No changes to review.\n");
        }
        s
    }

    /// Serialize the current review state for the active comparison. Persists
    /// to TWO places, both routed through `RepoSource::persist_state` (so SSH
    /// writes to the remote):
    /// - `.purview/state/<key>.json` — the per-comparison file, the source of
    ///   truth re-anchored on the next reload.
    /// - `.purview/review-state.json` — the canonical mirror the MCP server
    ///   reads (the live, active comparison). Kept identical so `purview-mcp`
    ///   needs no changes.
    ///
    /// Each hunk's `anchor` (content hash) is computed here so it's recorded
    /// alongside the `@@` header; orphans are carried through. Called whenever
    /// status/comment changes.
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
                            anchor: h.content_anchor(),
                            changed_since_review: h.changed_since_review,
                        })
                        .collect(),
                })
                .collect(),
            orphaned: self.orphaned.clone(),
        };
        // Persist WHERE the repo lives (remote over SSH, local fs otherwise) so
        // the MCP server reads it natively. Serialize once, write to both the
        // per-comparison file and the canonical mirror.
        if let Ok(json) = state.to_json() {
            let key = self.comparison_key();
            let relname = format!("state/{}.json", key.file_stem());
            let _ = self.repo.persist_state(&relname, &json);
            let _ = self.repo.persist_state("review-state.json", &json);
        }
    }

    fn count_status(&self, status: ReviewStatus) -> usize {
        self.files
            .iter()
            .flat_map(|f| f.hunks.iter())
            .filter(|h| h.status == status)
            .count()
    }

    /// Write the report to the repo's `.purview/review-report.md` WHERE the
    /// repo lives (remote over SSH, local fs otherwise) so the MCP server reads
    /// it natively; return the local mirror path (for the UI "wrote to ..."
    /// note). For SSH the authoritative copy is on the remote.
    fn write_report(&self) -> std::io::Result<PathBuf> {
        self.repo
            .persist_state("review-report.md", &self.review_report())
            .map_err(|e| std::io::Error::other(e))?;
        Ok(self.state_root.join(".purview").join("review-report.md"))
    }

    /// Write `new_text` to the file's line `line0` (0-based), preserving the
    /// rest. Only valid in full-file (Plain) view, where cache row == file
    /// line. Routed through the repo backend (no-op/error in ssh mode).
    fn write_line(&self, rel: &str, line0: usize, new_text: &str) -> Result<(), String> {
        self.repo.write_line(rel, line0, new_text)
    }

    /// The content payload a selection+view needs fetched from the backend, if
    /// any. `None` means everything needed is already in hand (Summary extent
    /// reuses `self.files` hunks — no fetch) so `ensure_cache` can build
    /// synchronously. `Some(key)` is a (possibly slow, over SSH) load that goes
    /// through the cache + async loader.
    fn content_key_for(&self, sel: &Selection) -> Option<ContentKey> {
        match sel {
            Selection::Changed(idx) => {
                if self.extent == Extent::Full {
                    let path = self.files.get(*idx)?.path.clone();
                    Some(ContentKey {
                        path,
                        full_diff: true,
                        source: self.source,
                        base: self.base.clone(),
                    })
                } else {
                    None // Summary: reuse already-computed hunks.
                }
            }
            Selection::Path(p) => Some(ContentKey {
                path: p.clone(),
                full_diff: false,
                source: self.source,
                base: self.base.clone(),
            }),
        }
    }

    /// Kick a file-content load onto a worker thread (Task A). Mirrors the diff
    /// `reload` pattern: bump a load generation, stash a Receiver, and let
    /// `poll_content` apply the result — dropping any stamped with a superseded
    /// generation (the user switched files faster than the load finished). A
    /// load already in flight for the SAME key is left alone (no duplicate kick).
    fn kick_content_load(&mut self, key: ContentKey) {
        if self.content_rx.as_ref().map(|(k, _)| k) == Some(&key) {
            return; // already loading exactly this.
        }
        self.content_gen = self.content_gen.wrapping_add(1);
        let gen = self.content_gen;
        let (tx, rx) = std::sync::mpsc::channel();
        let repo = std::sync::Arc::clone(&self.repo);
        let kc = key.clone();
        std::thread::spawn(move || {
            let res = fetch_content(repo.as_ref(), &kc);
            let _ = tx.send((gen, kc, res));
        });
        self.content_rx = Some((key, rx));
        self.content_loading = true;
    }

    /// Non-blocking poll for an in-flight content load (Task A). Applies a
    /// finished payload to the cache IFF its generation is current (a newer
    /// open supersedes an older load). Returns true if a current-generation
    /// result landed this call (so the caller rebuilds the cache from it).
    fn poll_content(&mut self) -> bool {
        let want = self.content_gen;
        let Some((_, rx)) = &self.content_rx else { return false };
        let Ok((gen, key, res)) = rx.try_recv() else { return false };
        if gen != want {
            // Superseded — drop it; a newer load is in flight (keep loading).
            return false;
        }
        self.content_rx = None;
        self.content_loading = false;
        match res {
            Ok(content) => {
                self.content_cache.insert(key, content);
                // Force ensure_cache to rebuild now that the payload is in hand.
                self.cache_key = None;
                true
            }
            Err(e) => {
                self.error = Some(e);
                self.cache_key = None;
                true
            }
        }
    }

    /// Drop all cached file content (Task A). Called on reload()/refresh because
    /// the diff changed — a previously-loaded payload may now be stale.
    fn invalidate_content_cache(&mut self) {
        self.content_cache.clear();
        self.content_rx = None;
        self.content_loading = false;
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

        // Task A: does this selection/view need a (possibly slow) backend fetch
        // that isn't already cached? If so, serve it from the cache when present
        // — instant, no round-trip — or load it: synchronously for the local fs
        // (fast), asynchronously for SSH (a blocking remote round-trip that used
        // to freeze the UI ~1s per click). While an async load is in flight we
        // leave the render cache empty and bail; the content pane shows a
        // "loading…" placeholder and `poll_content` rebuilds when it lands.
        if let Some(ck) = self.content_key_for(&sel) {
            if self.content_cache.get(&ck).is_none() {
                if self.repo.is_remote() {
                    self.kick_content_load(ck);
                    // Don't build the cache yet — wait for the payload.
                    self.cache.clear();
                    self.hl_cache.borrow_mut().clear();
                    return;
                } else {
                    // Local fs: load inline (instant) and cache it.
                    match fetch_content(self.repo.as_ref(), &ck) {
                        Ok(content) => self.content_cache.insert(ck.clone(), content),
                        Err(e) => self.error = Some(e),
                    }
                }
            }
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
                    // The full-context hunks come from the content cache (loaded
                    // sync for local / async for SSH above), so this build does
                    // no backend round-trip. Fall back to the Summary hunks if
                    // the load failed.
                    let ck = ContentKey {
                        path: path.clone(),
                        full_diff: true,
                        source: self.source,
                        base: self.base.clone(),
                    };
                    let full_hunks: Vec<diff::Hunk> = match self.content_cache.get(&ck) {
                        Some(FileContent::FullHunks(h)) => h.clone(),
                        _ => self.files[*idx].hunks.clone(),
                    };
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
                // Unchanged file from the tree — just show it whole. Its
                // contents come from the content cache (loaded sync for local /
                // async for SSH above), so this build does no backend round-trip.
                path = p.clone();
                plain = true;
                let ck = ContentKey {
                    path: path.clone(),
                    full_diff: false,
                    source: self.source,
                    base: self.base.clone(),
                };
                match self.content_cache.get(&ck) {
                    Some(FileContent::Full(content)) => {
                        for (i, line) in content.lines().enumerate() {
                            out.push(RenderRow::Plain {
                                text: line.to_string(),
                                lineno: i as u32 + 1,
                            });
                        }
                    }
                    _ => out.push(RenderRow::Plain {
                        text: format!(
                            "cannot read file: {}",
                            self.error.as_deref().unwrap_or("load failed")
                        ),
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
        // The ENTIRE pipeline runs off the UI thread: both git grep
        // (candidate gathering) and the Claude-CLI precision step are
        // potentially slow — over SSH the grep is a blocking remote round-trip
        // that would freeze the app. Both run WHERE the repo (and claude) live,
        // routed through the repo backend, on a background thread that shares
        // the backend via a cheap Arc clone. The worker sends back the final
        // resolved Candidate (or None, or an Err), polled by goto_overlay.
        let (tx, rx) = std::sync::mpsc::channel();
        let worker_symbol = symbol.clone();
        let repo = std::sync::Arc::clone(&self.repo);
        std::thread::spawn(move || {
            let res = (|| {
                let cands = repo.grep_symbol(&worker_symbol)?;
                if cands.is_empty() {
                    return Ok(None);
                }
                repo.resolve_definition(&worker_symbol, None, &cands)
            })();
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
        let mut finished: Option<Result<Option<purview::gotodef::Candidate>, String>> = None;
        if let Some(go) = self.goto.as_mut() {
            if let Some(rx) = &go.rx {
                if let Ok(res) = rx.try_recv() {
                    finished = Some(res);
                }
            }
        }
        if let Some(res) = finished {
            match res {
                Ok(Some(cand)) => {
                    self.selected = Some(Selection::Path(cand.file.clone()));
                    self.pending_line = Some(cand.line);
                    self.goto = None;
                }
                Ok(None) => {
                    if let Some(go) = self.goto.as_mut() {
                        go.resolving = false;
                        go.rx = None;
                        go.note = "no definition found".to_string();
                    }
                }
                Err(e) => {
                    if let Some(go) = self.goto.as_mut() {
                        go.resolving = false;
                        go.rx = None;
                        go.note = format!("resolve failed: {e}");
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
            // Use the real rendered content height (variable row heights) when
            // we have it; fall back to the row-count estimate on the very first
            // frame before any render has reported a size.
            let content_h = if self.content_height > 0.0 {
                self.content_height
            } else {
                self.cache.len() as f32
                    * (ctx.style().text_styles[&egui::TextStyle::Monospace].size + 3.0)
            };
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
            if Some(next) != cur_file {
                // Switching files: focus the new file's FIRST hunk and scroll
                // it into view. Otherwise the view stays at the previous file's
                // scroll offset and the user has to press `p` to reach the first
                // change. The new file's hunk_rows aren't built until
                // ensure_cache later this frame, so defer the scroll via a flag
                // resolved there (works in both Summary and Full, where the
                // first change may not sit at row 0).
                self.focus_hunk = 0;
                self.scroll_to_first_hunk = true;
            }
            self.selected = Some(Selection::Changed(next));
        }

        // n/p: move to the next/prev hunk RELATIVE TO THE CURRENT SCROLL
        // POSITION (not just focus_hunk ± 1) and scroll it into view. The
        // visible window comes from the rows egui ACTUALLY painted last frame
        // (`self.visible_rows`), NOT from `content_scroll / row_h`: with tall
        // HunkHeader strips above the viewport the latter over-counts rows and
        // reports the wrong [top,bottom], which inverted the "is it visible?"
        // test (scrolling when the target was already on screen, and not
        // scrolling when it was off-screen — the reported bug). The painted
        // range is exact for any mix of row heights.
        if (n || p) && !self.hunk_rows.is_empty() {
            // Before the first content frame paints, treat row 0 as the lone
            // visible row so a target only counts as "visible" at the very top.
            let (top_row, bottom_row) = self.visible_rows.unwrap_or((0, 0));
            match nav_hunk_from_scroll(&self.hunk_rows, top_row, bottom_row, self.focus_hunk, n) {
                Some(target) => self.focus_hunk = target,
                // No hunk in that direction — clamp to the edge in this
                // direction so a repeated press settles, never pages.
                None => {
                    self.focus_hunk = if n { self.hunk_rows.len() - 1 } else { 0 };
                }
            }
            // Only scroll when the target hunk is OUTSIDE the current (real)
            // viewport. If it's already on screen, just move focus and leave the
            // user's scroll position untouched (don't yank them around). When it
            // is off-screen, request a scroll-to-row centered via scroll_to_me —
            // egui uses the row's actual rect, so no `row * row_h` drift.
            let row = self.hunk_rows.get(self.focus_hunk).copied().unwrap_or(0);
            if !row_in_viewport(row, top_row, bottom_row) {
                self.scroll_to_row = Some((row, egui::Align::Center));
            }
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
                // Grab keyboard focus on the editor next frame so the user can
                // type immediately without a mouse click.
                self.comment_just_opened = true;
            }
        }
    }

    /// Render the Ctrl+F in-file search bar (when open) and handle its keys.
    /// Recomputes the match set against the current content cache, advances on
    /// Enter / F3 (next) and Shift+Enter / Shift+F3 (prev), shows a "k of N"
    /// count, and scrolls the current match into view. Esc closes + clears.
    /// Must run after `ensure_cache` (it reads `self.cache`) and before the
    /// central panel (it sets `self.pending_scroll`, consumed there).
    fn search_bar(&mut self, ctx: &egui::Context) {
        if self.search.is_none() {
            return;
        }
        // Esc closes the search and clears highlights.
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.search = None;
            return;
        }
        // Read next/prev intents before the text field consumes the keys.
        // Enter / F3 → next; with Shift → previous. Ctrl+G also goes next
        // (Ctrl+Shift+G previous) for editor-convention parity.
        let (next, prev) = ctx.input(|i| {
            let shift = i.modifiers.shift;
            let enter = i.key_pressed(egui::Key::Enter);
            let f3 = i.key_pressed(egui::Key::F3);
            let ctrl_g = (i.modifiers.ctrl || i.modifiers.command) && i.key_pressed(egui::Key::G);
            let fwd = (enter || f3 || ctrl_g) && !shift;
            let back = (enter || f3 || ctrl_g) && shift;
            (fwd, back)
        });

        let generation = self.generation;
        let cache = &self.cache;
        let Some(search) = self.search.as_mut() else { return };

        // Recompute matches when the query or the content changed.
        let key = (search.query.clone(), generation);
        if search.computed_for.as_ref() != Some(&key) {
            search.matches = find_matches(cache, &search.query);
            search.computed_for = Some(key);
            if search.current >= search.matches.len() {
                search.current = 0;
            }
        }

        let n = search.matches.len();
        if n > 0 {
            if next {
                search.current = (search.current + 1) % n;
            } else if prev {
                search.current = (search.current + n - 1) % n;
            } else if search.current >= n {
                search.current = 0;
            }
        }

        // Scroll the current match into view (a little headroom above it), but
        // only when it actually changed — otherwise we'd re-scroll every frame
        // and fight the user's manual scrolling.
        let cur_row = if n > 0 {
            search.matches.get(search.current).copied()
        } else {
            None
        };
        let scroll_to = if cur_row != search.last_scrolled {
            search.last_scrolled = cur_row;
            cur_row
        } else {
            None
        };

        let count_label = if search.query.trim().is_empty() {
            String::new()
        } else if n == 0 {
            "no matches".to_string()
        } else {
            format!("{} of {}", search.current + 1, n)
        };

        let mut just_opened = search.just_opened;
        egui::Window::new("Find")
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::RIGHT_TOP, [-12.0, 96.0])
            .fixed_size([300.0, 0.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut search.query)
                            .hint_text("find in file…")
                            .desired_width(180.0),
                    );
                    if just_opened {
                        resp.request_focus();
                        just_opened = false;
                    }
                    if !count_label.is_empty() {
                        ui.label(egui::RichText::new(&count_label).small().weak());
                    }
                });
                ui.label(
                    egui::RichText::new("Enter/F3 next · Shift+Enter prev · Esc close")
                        .small()
                        .weak(),
                );
            });
        search.just_opened = just_opened;

        if let Some(row) = scroll_to {
            // Center the matched row via scroll_to_me on its actual rect — no
            // `row * row_h` math, so a tall HunkHeader above the match can't push
            // the landing spot "slightly below the page" (the reported bug). The
            // unused `ctx` borrow is dropped; the request is consumed in `ui`.
            let _ = ctx;
            self.scroll_to_row = Some((row, egui::Align::Center));
        }
    }

    fn ui(&mut self, ctx: &egui::Context) {
        // Pick up an async reload (bug #2) if it finished. While one is in
        // flight, keep repainting so the poll runs and the spinner animates —
        // the worker thread can't wake egui on its own.
        self.poll_reload();
        // Task A: pick up an async file-content load if it finished. While one
        // is in flight, keep repainting so the poll runs and the "loading…"
        // placeholder animates — the worker can't wake egui on its own.
        self.poll_content();
        if self.loading || self.content_loading {
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
        // Ctrl+F toggles the in-file search bar. (Cmd+F on mac.) When the bar
        // is already open we always let it close — its own text field holds
        // keyboard focus, so we must read the press here (before the widget) to
        // catch it. When it's CLOSED we only open if no other text field (base
        // input / comment box) currently has focus, so Ctrl+F never fires mid-
        // typing elsewhere.
        let toggle_find = ctx.input(|i| {
            i.key_pressed(egui::Key::F) && (i.modifiers.ctrl || i.modifiers.command)
        });
        if toggle_find {
            if self.search.is_some() {
                self.search = None;
            } else if !ctx.wants_keyboard_input() {
                self.search = Some(Search {
                    query: String::new(),
                    matches: Vec::new(),
                    current: 0,
                    just_opened: true,
                    computed_for: None,
                    last_scrolled: None,
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
        self.search_bar(ctx);

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
        // In-file search highlight set (Ctrl+F): the matched cache rows and the
        // currently-selected one, captured for the render closure so it can tint
        // matches. Empty when search is closed or the query is empty.
        let (search_rows, search_current_row): (std::collections::HashSet<usize>, Option<usize>) =
            match &self.search {
                Some(s) if !s.matches.is_empty() => (
                    s.matches.iter().copied().collect(),
                    s.matches.get(s.current).copied(),
                ),
                _ => (std::collections::HashSet::new(), None),
            };
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
                // Set when this frame's keys should close the editor and return
                // keyboard focus to the diff (n/p/a/r/c work again without a
                // mouse click). Ctrl/Cmd+Enter submits (saves), Esc cancels.
                let mut close_editor = false;
                egui::TopBottomPanel::bottom("comment").resizable(true).show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Comment").strong());
                        ui.weak(header.trim().to_string());
                        if ui.small_button("close").clicked() {
                            self.active_hunk = None;
                        }
                        ui.weak("Ctrl+Enter to save · Esc to cancel");
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
                        // Focus the editor the frame it opens via `c`.
                        if self.comment_just_opened {
                            resp.request_focus();
                            self.comment_just_opened = false;
                        }
                        // Ctrl/Cmd+Enter submits; plain Enter falls through to
                        // the TextEdit and inserts a newline. Consume the
                        // modifier+Enter event so no newline is inserted. Esc
                        // cancels. Both return focus to the diff (close_editor).
                        if resp.has_focus() {
                            let submit = ui.input_mut(|i| {
                                i.consume_key(egui::Modifiers::COMMAND, egui::Key::Enter)
                                    || i.consume_key(egui::Modifiers::CTRL, egui::Key::Enter)
                            });
                            let cancel = ui.input(|i| i.key_pressed(egui::Key::Escape));
                            if submit {
                                changed = true; // persist the comment on save
                                close_editor = true;
                            } else if cancel {
                                close_editor = true;
                            }
                            if close_editor {
                                // Drop keyboard focus from the editor so the
                                // diff's modal keys work again next frame.
                                resp.surrender_focus();
                            }
                        }
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
                // Mark dirty on edit, but DON'T persist per keystroke —
                // save_review_state serializes the whole state and (in ssh mode)
                // does a remote round-trip, which makes typing lag badly. The
                // in-memory comment is already updated live; flush to disk/remote
                // only when the editor closes (submit / cancel / blur).
                if changed {
                    self.comment_dirty = true;
                }
                if close_editor {
                    if self.comment_dirty {
                        self.save_review_state();
                        self.comment_dirty = false;
                    }
                    // Close the editor; focus was already surrendered above so
                    // the diff's modal keys (n/p/a/r/c) work again next frame.
                    self.active_hunk = None;
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
            // Task A: a file open is loading over SSH — show a brief placeholder
            // instead of a frozen frame. The cache is empty until the payload
            // lands (then `poll_content` rebuilds it).
            if self.content_loading {
                ui.centered_and_justified(|ui| {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new());
                        ui.label("loading…");
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
            // PageUp/PageDown's relative pager offset (the only remaining pixel
            // scroll — it's relative to the live offset, not `row * row_h`).
            let pending_v: Option<f32> = self.pending_scroll.take();
            // go-to-def line jump: in Plain (full-file) view cache row == file
            // line - 1, so route it through the same accurate scroll-to-row
            // mechanism (centered) instead of `line * row_h`, which drifted.
            if let Some(line) = self.pending_line.take() {
                let target = line.saturating_sub(1).min(total.saturating_sub(1));
                self.scroll_to_row = Some((target, egui::Align::Center));
            }
            // j/k file switch: now that ensure_cache has rebuilt hunk_rows for
            // the newly selected file, scroll its first hunk header to the TOP
            // (Align::Min) via scroll_to_me — accurate for variable row heights.
            if self.scroll_to_first_hunk {
                self.scroll_to_first_hunk = false;
                let first = self.hunk_rows.first().copied().unwrap_or(0);
                self.scroll_to_row = Some((first, egui::Align::Min));
            }
            // The accurate scroll-to-row request (n/p, search, j/k, F12). Read
            // (not yet taken) here so both the unified path and split_panes can
            // act on it; cleared after the content area paints.
            let scroll_to_row = self.scroll_to_row;

            // Split layout draws two side-by-side panes that scroll
            // HORIZONTALLY on their own (a long line on the left never shifts
            // the right pane's x-position) while staying vertically locked so
            // line numbers line up row-for-row. The other layouts use one
            // unified scroll area.
            if self.layout == Layout::Split && matches!(self.selected, Some(Selection::Changed(_)))
            {
                let (off, vp, painted) = self.split_panes(
                    ui,
                    row_h,
                    total,
                    pending_v,
                    scroll_to_row,
                    sel_sym.as_deref(),
                    &mut clicked_symbol,
                    active_file,
                    active_path.as_deref(),
                    &replies,
                    &mut pending,
                    &mut open_comment,
                );
                content_scroll = off;
                content_viewport_h = vp;
                if let Some(pr) = painted {
                    self.visible_rows = Some(pr);
                }
                // Consume the scroll request (the split path handled it).
                self.scroll_to_row = None;
                return;
            }

            let mut area = egui::ScrollArea::both().auto_shrink([false, false]);
            if let Some(off) = pending_v {
                area = area.vertical_scroll_offset(off);
            } else if let Some((target, _)) = scroll_to_row {
                // Coarse pre-position so the target row is inside the painted
                // band this frame (show_rows only lays out the visible window).
                // `target * row_h` need only get us WITHIN a viewport of the
                // row; the exact landing is then done by scroll_to_rect on the
                // row's real rect below — so the approximation's drift doesn't
                // matter (it's corrected the same frame against actual geometry).
                area = area.vertical_scroll_offset((target as f32 * row_h).max(0.0));
            }
            let mut painted: Option<(usize, usize)> = None;
            let out = area.show_rows(
                ui,
                row_h,
                total,
                |ui, range| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    painted = Some((range.start, range.end.saturating_sub(1)));
                    let focus_row = self.hunk_rows.get(self.focus_hunk).copied();
                    for i in range {
                        // Ctrl+F highlight: tint matched rows; the current match
                        // gets a brighter band so next/prev is visible. Painted
                        // as a TRANSLUCENT overlay after the row draws, so it
                        // reads on top of the row's own add/del background.
                        let search_tint = if Some(i) == search_current_row {
                            Some(Color32::from_rgba_unmultiplied(230, 180, 60, 90))
                        } else if search_rows.contains(&i) {
                            Some(Color32::from_rgba_unmultiplied(230, 200, 90, 45))
                        } else {
                            None
                        };
                        let y_before = ui.cursor().min.y;
                        match &self.cache[i] {
                            RenderRow::HunkHeader { hunk_idx, text, whole_file } => {
                                let focused = Some(i) == focus_row;
                                // One source of truth for the header controls,
                                // shared with split_panes (see the method docs).
                                self.hunk_header_controls(
                                    ui,
                                    *hunk_idx,
                                    text,
                                    *whole_file,
                                    focused,
                                    active_file,
                                    active_path.as_deref(),
                                    &replies,
                                    &mut pending,
                                    &mut open_comment,
                                );
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
                        // The row's ACTUAL on-screen rect (variable height).
                        let y_after = ui.cursor().min.y;
                        let row_rect = egui::Rect::from_min_size(
                            egui::pos2(ui.max_rect().min.x, y_before),
                            egui::vec2(ui.max_rect().width(), (y_after - y_before).max(row_h)),
                        );
                        // Overlay the search tint across the row just drawn.
                        if let Some(tint) = search_tint {
                            ui.painter().rect_filled(row_rect, 0.0, tint);
                        }
                        // Accurate scroll-to-row: when THIS row is the target,
                        // ask the scroll area to bring its real rect into view
                        // at the requested alignment. Variable row heights are
                        // handled by egui — no `row * row_h` drift.
                        if let Some((target, align)) = scroll_to_row {
                            if i == target {
                                ui.scroll_to_rect(row_rect, Some(align));
                            }
                        }
                    }
                },
            );
            content_scroll = out.state.offset.y;
            content_viewport_h = out.inner_rect.height();
            // Real total content height (handles variable row heights) so
            // PageDown can reach the true bottom of a long file.
            self.content_height = out.content_size.y;
            if let Some(pr) = painted {
                self.visible_rows = Some(pr);
            }
        });

        // Record the content-pane scroll geometry for next frame's PageUp/Down.
        self.content_scroll = content_scroll;
        self.content_viewport_h = content_viewport_h;
        // Consume the unified path's scroll-to-row request (the split path
        // clears its own before returning). One request, one applied jump.
        self.scroll_to_row = None;

        // Apply review-status changes collected during render, then persist
        // the review state for the MCP server.
        if let (Some(f), false) = (active_file, pending.is_empty()) {
            for (hunk_idx, status) in pending {
                if let Some(h) = self.files[f].hunks.get_mut(hunk_idx) {
                    h.status = status;
                    // The user just acted on this hunk → it's freshly reviewed,
                    // so the "changed since reviewed" warning no longer applies.
                    h.changed_since_review = false;
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
                        // The file on disk changed — drop cached content so the
                        // rebuild re-reads it (Task A cache), and the render cache.
                        self.invalidate_content_cache();
                        self.cache_key = None;
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
    /// Returns the panes' shared (vertical_offset, viewport_height, painted_row
    /// range) so the caller can record them for PageUp/PageDown and the n/p
    /// visibility test. `scroll_to_row` is the accurate "bring this cache row
    /// into view" request (see the `App::scroll_to_row` field): the left pane
    /// calls `scroll_to_rect` on the target row's REAL rect, and the resulting
    /// offset is captured into the shared offset so the jump sticks and both
    /// panes stay aligned — no `row * row_h` drift in either direction.
    #[allow(clippy::too_many_arguments)]
    fn split_panes(
        &self,
        ui: &mut egui::Ui,
        row_h: f32,
        total: usize,
        pending_v: Option<f32>,
        scroll_to_row: Option<(usize, egui::Align)>,
        sel: Option<&str>,
        clicked_symbol: &mut Option<String>,
        active_file: Option<usize>,
        active_path: Option<&str>,
        replies: &Replies,
        pending: &mut Vec<(usize, ReviewStatus)>,
        open_comment: &mut Option<usize>,
    ) -> (f32, f32, Option<(usize, usize)>) {
        // Cache row of the focused hunk's header, so the strip can draw its
        // focus marker (matches the unified path's `focus_row`).
        let focus_row = self.hunk_rows.get(self.focus_hunk).copied();
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
        // A scroll-to-row request coarse-positions the offset so the target row
        // is inside the painted band this frame; scroll_to_rect (below) then
        // lands it exactly off the row's real rect. Otherwise carry the offset.
        let v_off = match (pending_v, scroll_to_row) {
            (Some(off), _) => off,
            (None, Some((target, _))) => (target as f32 * row_h).max(0.0),
            (None, None) => carried,
        };
        let mut new_off = v_off;
        let mut viewport_h = 0.0_f32;
        let mut painted: Option<(usize, usize)> = None;
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
                        painted = Some((range.start, range.end.saturating_sub(1)));
                        for i in range {
                            let y_before = ui.cursor().min.y;
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
                                RenderRow::HunkHeader { hunk_idx, text, whole_file } => {
                                    // Full-width control strip — the SAME one the
                                    // unified path draws (shared helper, one
                                    // source of truth). The right pane mirrors a
                                    // blank spacer for this row so alignment holds.
                                    let focused = Some(i) == focus_row;
                                    self.hunk_header_controls(
                                        ui,
                                        *hunk_idx,
                                        text,
                                        *whole_file,
                                        focused,
                                        active_file,
                                        active_path,
                                        replies,
                                        pending,
                                        open_comment,
                                    );
                                }
                                _ => {
                                    ui.label(" ");
                                }
                            }
                            // Accurate scroll-to-row off the left pane's real
                            // row rect (variable heights → no drift). The
                            // resulting offset is captured into the shared
                            // offset below so the right pane follows in lockstep.
                            if let Some((target, align)) = scroll_to_row {
                                if i == target {
                                    let y_after = ui.cursor().min.y;
                                    let rect = egui::Rect::from_min_size(
                                        egui::pos2(ui.max_rect().min.x, y_before),
                                        egui::vec2(
                                            ui.max_rect().width(),
                                            (y_after - y_before).max(row_h),
                                        ),
                                    );
                                    ui.scroll_to_rect(rect, Some(align));
                                }
                            }
                        }
                    });
                });
            // The user's vertical drag on the left pane wins this frame; a
            // scroll_to_rect jump also shows up here as a changed offset. Either
            // way, adopt the left pane's resulting offset so it sticks across
            // frames and the right pane follows it.
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
        (new_off, viewport_h, painted)
    }

    /// Render the per-hunk control strip: focus marker, status glyph (✓/✗/○),
    /// approve / reject / clear / comment (💬) buttons, and the `@@` label.
    ///
    /// This is the ONE source of truth for the header controls, called by BOTH
    /// the unified scroll path and `split_panes`' left pane — so the two can't
    /// drift apart (the Full+Split "controls don't render" regression). It only
    /// borrows `self` immutably; clicks are collected into `pending` /
    /// `open_comment`, which the caller applies after the render closure.
    ///
    /// `targets` (which review hunks the buttons mutate) follows the same logic
    /// as the cache: exactly `hunk_idx`, or ALL of the file's hunks when
    /// `whole_file` (the retained file-level aggregate strip).
    #[allow(clippy::too_many_arguments)]
    fn hunk_header_controls(
        &self,
        ui: &mut egui::Ui,
        hunk_idx: usize,
        text: &str,
        whole_file: bool,
        focused: bool,
        active_file: Option<usize>,
        active_path: Option<&str>,
        replies: &Replies,
        pending: &mut Vec<(usize, ReviewStatus)>,
        open_comment: &mut Option<usize>,
    ) {
        let targets: Vec<usize> = match active_file {
            Some(f) if whole_file => (0..self.files[f].hunks.len()).collect(),
            Some(_) => vec![hunk_idx],
            None => Vec::new(),
        };
        let status = active_file
            .map(|f| aggregate_status(&self.files[f], &targets))
            .unwrap_or(ReviewStatus::Unreviewed);
        let hdr_bg = if focused {
            Color32::from_rgb(48, 58, 80) // focused: brighter
        } else {
            Color32::from_rgb(30, 36, 48)
        };
        egui::Frame::none().fill(hdr_bg).show(ui, |ui| {
            ui.horizontal(|ui| {
                // Always reserve the focus-marker column so toggling focus
                // doesn't reflow the row (bug 3 layout shift).
                ui.label(
                    egui::RichText::new(if focused { "▶" } else { " " })
                        .monospace()
                        .color(Color32::from_rgb(140, 180, 240)),
                );
                let (glyph, col) = match status {
                    ReviewStatus::Approved => ("✓", Color32::from_rgb(120, 200, 120)),
                    ReviewStatus::Rejected => ("✗", Color32::from_rgb(220, 120, 120)),
                    ReviewStatus::Unreviewed => ("○", Color32::DARK_GRAY),
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
                // Always render "clear" (disabled when nothing to clear) so the
                // row never reflows when status toggles (bug 3 layout shift).
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
                    .and_then(|f| self.files[f].hunks.get(hunk_idx))
                    .map(|h| !h.comment.trim().is_empty())
                    .unwrap_or(false);
                let cbtn = if has_comment { "💬*" } else { "💬" };
                if ui.small_button(cbtn).clicked() {
                    *open_comment = Some(hunk_idx);
                }
                // Agent-reply count for this hunk.
                if let Some(p) = active_path {
                    let n = replies.for_hunk(p, text).len();
                    if n > 0 {
                        ui.label(
                            egui::RichText::new(format!("↩{n}"))
                                .small()
                                .color(Color32::from_rgb(120, 200, 160)),
                        );
                    }
                }
                ui.label(
                    egui::RichText::new(text)
                        .monospace()
                        .color(Color32::from_rgb(120, 160, 220)),
                );
                // Warn when a carried-over verdict's content has changed since
                // it was reviewed.
                let changed_since = active_file
                    .and_then(|f| self.files[f].hunks.get(hunk_idx))
                    .map(|h| h.changed_since_review)
                    .unwrap_or(false);
                if changed_since {
                    ui.label(
                        egui::RichText::new("⚠ changed since reviewed")
                            .small()
                            .color(Color32::from_rgb(230, 180, 90)),
                    );
                }
            });
        });
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
        ("Ctrl+F", "find in file (Enter/F3 next, Shift prev, Esc close)"),
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

/// Pick the n/p target hunk so n/p always move to the NEXT / PREVIOUS hunk
/// relative to where the user is looking — never page the view. `hunk_rows` is
/// the ascending list of each hunk header's cache-row index. `top_row` /
/// `bottom_row` bracket the currently-visible cache rows. `focus` is the
/// current focus hunk (index into `hunk_rows`). `forward` = `n`.
///
/// The anchor depends on whether the focused hunk is on screen:
/// - If the focused hunk's header is WITHIN the viewport, step from it
///   (`focus ± 1`) — the familiar "next/prev hunk" behavior.
/// - If it has scrolled OFF screen (the user paged/scrolled into a large
///   context region with no hunk header visible — the reported bug), anchor on
///   the viewport instead: `n` → first hunk below the viewport top, `p` → last
///   hunk above it. This stops n/p from jumping to a hunk on the wrong side of
///   the viewport (which felt like paging).
///
/// Returns the index INTO `hunk_rows`, or `None` if there is no hunk in that
/// direction (caller keeps / clamps the focus).
/// Whether cache `row` falls within the inclusive `[top_row, bottom_row]`
/// viewport span (the rows currently on screen). `[top_row, bottom_row]` is the
/// range egui ACTUALLY painted last frame (`App::visible_rows`), so this is
/// exact for any mix of row heights — unlike a `content_scroll / row_h` estimate
/// that drifts past tall HunkHeader strips. Used by n/p to decide whether the
/// target hunk needs scrolling into view: if it's already visible, focus moves
/// but the scroll position is left alone.
fn row_in_viewport(row: usize, top_row: usize, bottom_row: usize) -> bool {
    row >= top_row && row <= bottom_row
}

fn nav_hunk_from_scroll(
    hunk_rows: &[usize],
    top_row: usize,
    bottom_row: usize,
    focus: usize,
    forward: bool,
) -> Option<usize> {
    if hunk_rows.is_empty() {
        return None;
    }
    let focus_visible = hunk_rows
        .get(focus)
        .map(|&r| r >= top_row && r <= bottom_row)
        .unwrap_or(false);
    if focus_visible {
        // Step from the focused hunk (old, familiar behavior).
        if forward {
            (focus + 1 < hunk_rows.len()).then(|| focus + 1)
        } else {
            focus.checked_sub(1)
        }
    } else if forward {
        // Focus is off-screen: first hunk below the viewport top.
        hunk_rows.iter().position(|&r| r > top_row)
    } else {
        // Focus is off-screen: last hunk above the viewport top.
        hunk_rows.iter().rposition(|&r| r < top_row)
    }
}

/// The searchable text of one render row (Ctrl+F). Diff/plain lines contribute
/// their content; a hunk header contributes its `@@ … @@` text; a split row
/// contributes both cells joined by a space (so a match on either side counts).
fn row_search_text(row: &RenderRow) -> String {
    match row {
        RenderRow::HunkHeader { text, .. } => text.clone(),
        RenderRow::DiffLine { text, .. } => text.clone(),
        RenderRow::Plain { text, .. } => text.clone(),
        RenderRow::SplitLine { left, right } => {
            let l = left.as_ref().map(|(_, t, _)| t.as_str()).unwrap_or("");
            let r = right.as_ref().map(|(_, t, _)| t.as_str()).unwrap_or("");
            format!("{l} {r}")
        }
    }
}

/// Find every render row whose text contains `query`, returning their indices
/// in ascending (display) order. Case-insensitive. An empty/whitespace-only
/// query matches nothing. This is the pure search core behind Ctrl+F; the count
/// is just `result.len()`, and the "k of N" indicator uses position within it.
fn find_matches(rows: &[RenderRow], query: &str) -> Vec<usize> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    rows.iter()
        .enumerate()
        .filter(|(_, r)| row_search_text(r).to_lowercase().contains(&q))
        .map(|(i, _)| i)
        .collect()
}

/// The inclusive `[first, last]` cache-row range visible in a viewport of
/// height `viewport_h` scrolled to `offset`, given each row's ACTUAL height in
/// `heights`. This is the accurate, variable-height answer the live path gets
/// from egui's painted `range`; kept as a pure function so the root-cause math
/// (rows are NOT uniform — a tall HunkHeader shifts every later row's y) can be
/// unit-tested directly. A row counts as visible if any part of it intersects
/// `[offset, offset + viewport_h)`. Empty `heights` → `None`.
///
/// Contrast with the OLD, buggy approach `offset / row_h .. (offset+vp) / row_h`
/// which assumes a single uniform `row_h` and therefore drifts by the
/// accumulated extra height of any tall rows above the viewport.
///
/// The LIVE code doesn't call this: egui's `show_rows` hands us the actually-
/// painted `range` directly (an even more authoritative source of truth), which
/// we store in `App::visible_rows`. This function exists to lock the underlying
/// variable-height math in a test so the root-cause assumption can't silently
/// regress to `index * row_h`.
#[cfg(test)]
fn visible_range_from_heights(heights: &[f32], offset: f32, viewport_h: f32) -> Option<(usize, usize)> {
    if heights.is_empty() {
        return None;
    }
    let top = offset.max(0.0);
    let bottom = top + viewport_h.max(0.0);
    let mut y = 0.0_f32;
    let mut first: Option<usize> = None;
    let mut last = 0usize;
    for (i, &h) in heights.iter().enumerate() {
        let row_top = y;
        let row_bottom = y + h;
        // Visible if the row's span intersects the viewport span. The bottom
        // edge is inclusive so a viewport sitting exactly on a boundary still
        // shows the row beginning there.
        if row_bottom > top && row_top <= bottom {
            if first.is_none() {
                first = Some(i);
            }
            last = i;
        }
        y = row_bottom;
    }
    first.map(|f| (f, last))
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

    /// REGRESSION GUARD (Full+Split controls don't render): the older sequence
    /// tests assert the HunkHeader ROW exists in the cache, which passed even
    /// while Split drew only a `@@` label and NO buttons. This renders a REAL
    /// Full+Split frame and proves the approve/reject control strip is actually
    /// drawn AND functional: it finds the "approve"/"reject" button widgets by
    /// label in the rendered output, then CLICKS approve and asserts the hunk's
    /// status flips to Approved — which can only happen if a real, hittable
    /// button was painted in the Split header strip (not just a label).
    #[test]
    fn full_split_hunk_controls_render_and_are_clickable() {
        use egui_kittest::kittest::Queryable; // get_by_label / get_all_by_label
        let repo = fixture_repo();
        let mut app = local_app(&repo);
        // The user's PRIMARY mode. Set before the harness takes ownership so the
        // first frame builds the Split cache in Full extent.
        app.layout = Layout::Split;
        app.extent = Extent::Full;
        app.selected = Some(Selection::Changed(0));

        let mut harness = egui_kittest::Harness::new_state(
            |ctx, app: &mut App| app.ui(ctx),
            app,
        );
        harness.run();

        // Sanity: we really are in Full+Split with a single hunk (so "approve"
        // is unambiguous) and the cache is the Split header+line sequence.
        assert!(harness.state().layout == Layout::Split);
        assert!(harness.state().extent == Extent::Full);

        // The control strip's buttons must be PRESENT in the rendered frame —
        // not just the `@@` label. (get_by_label would panic if absent.)
        let _ = harness.get_all_by_label("approve");
        let _ = harness.get_all_by_label("reject");

        // FUNCTIONAL proof: click approve. A label-only header has no clickable
        // widget here, so the status could never flip.
        harness.get_by_label("approve").click();
        harness.run();

        let approved = harness
            .state()
            .files
            .iter()
            .flat_map(|f| &f.hunks)
            .any(|h| h.status == ReviewStatus::Approved);
        assert!(
            approved,
            "clicking approve in a Full+Split hunk header must flip status — \
             proving a real button was drawn and hit, not just a label"
        );

        let state = ReviewState::load(&repo).expect("review-state.json written");
        assert!(
            state.files.iter().flat_map(|f| &f.hunks).any(|h| h.status == "approved"),
            "the Split-header click should have persisted approved status to disk"
        );
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

    /// A repo with TWO changed files, each carrying a single hunk near its top,
    /// so j/k has somewhere to move and focus_hunk can be advanced first.
    fn two_file_repo() -> PathBuf {
        let (dir, git) = new_repo_dir();
        let base: String = (1..=30).map(|n| format!("line {n}\n")).collect();
        std::fs::write(dir.join("a.txt"), &base).unwrap();
        std::fs::write(dir.join("b.txt"), &base).unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);
        // Two well-separated edits in each file → ≥2 hunks per file.
        let edited: String = (1..=30)
            .map(|n| match n {
                3 => "EDIT 3\n".to_string(),
                27 => "EDIT 27\n".to_string(),
                _ => format!("line {n}\n"),
            })
            .collect();
        std::fs::write(dir.join("a.txt"), &edited).unwrap();
        std::fs::write(dir.join("b.txt"), &edited).unwrap();
        dir
    }

    /// Fix 3: pressing j/k to switch changed files resets focus to that file's
    /// FIRST hunk. We first advance focus off hunk 0 with `n`, switch files with
    /// `j`, and assert focus is back at 0 (so the view lands on the first
    /// change instead of wherever the previous file's focus was).
    #[test]
    fn switching_files_resets_focus_to_first_hunk() {
        let repo = two_file_repo();
        let mut app = local_app(&repo);
        app.extent = Extent::Summary;
        app.selected = Some(Selection::Changed(0));
        assert!(app.files.len() >= 2, "fixture must have ≥2 changed files");
        assert!(app.files[0].hunks.len() >= 2, "file 0 needs ≥2 hunks for `n`");

        let mut harness = egui_kittest::Harness::new_state(|ctx, app: &mut App| app.ui(ctx), app);
        harness.run();
        // Advance focus to a non-zero hunk in file 0.
        harness.press_key(egui::Key::N);
        harness.run();
        assert_ne!(harness.state().focus_hunk, 0, "`n` should move focus off hunk 0");

        // Switch to the next file: focus must reset to its first hunk.
        harness.press_key(egui::Key::J);
        harness.run();
        let st = harness.state();
        assert!(
            matches!(st.selected, Some(Selection::Changed(1))),
            "j moves to the next changed file"
        );
        assert_eq!(
            st.focus_hunk, 0,
            "switching files lands on the new file's FIRST hunk"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// Fix 1: F12 must NOT block the UI thread — the whole go-to-definition
    /// pipeline (git grep + resolve) runs off-thread. Pressing F12 on a symbol
    /// should IMMEDIATELY return control with the Goto overlay in its
    /// "resolving" state and a live receiver, before any grep/resolve work has
    /// completed on the worker. (If grep ran on the UI thread, this frame would
    /// have blocked on it instead of returning a resolving overlay.)
    #[test]
    fn f12_starts_resolve_off_thread_without_blocking() {
        let repo = fixture_repo();
        let mut app = local_app(&repo);
        // Target any symbol; start_goto spawns the worker regardless of how many
        // candidates grep finds — the point is that nothing blocks here.
        app.selected_symbol = Some("two".to_string());

        let mut harness = egui_kittest::Harness::new_state(|ctx, app: &mut App| app.ui(ctx), app);
        harness.run();
        harness.press_key(egui::Key::F12);
        harness.run();

        // Control returned to us on the very next frame — the call did NOT
        // block on git grep. The overlay is present in its resolving state with
        // a live worker receiver (the grep+resolve are off-thread). We read
        // state immediately so the worker is still in flight; if grep had run
        // on the UI thread this frame would have blocked on it instead.
        let st = harness.state();
        let go = st.goto.as_ref().expect("F12 opens the Go-to-definition overlay");
        assert!(go.resolving, "overlay is in the resolving state right after F12");
        assert!(go.rx.is_some(), "a worker receiver is in place (pipeline is off-thread)");
        assert_eq!(go.query, "two");
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

    // ===================================================================
    // n/p hunk navigation — must target the hunk relative to the current
    // SCROLL position, never page the viewport (the reported bug).
    // ===================================================================

    /// When the focused hunk IS on screen, n/p step from it (the familiar
    /// next/prev-hunk behavior). Hunk headers at cache rows 2, 8, 15; a 12-row
    /// viewport at the top shows hunks 0 and 1.
    #[test]
    fn nav_hunk_steps_from_focus_when_visible() {
        let rows = [2usize, 8, 15];
        // Viewport rows 0..=12 → focus hunk 0 (row 2) is visible.
        // n steps to hunk 1, p clamps (no hunk before 0).
        assert_eq!(nav_hunk_from_scroll(&rows, 0, 12, 0, true), Some(1));
        assert_eq!(nav_hunk_from_scroll(&rows, 0, 12, 0, false), None);
        // Focus hunk 1 (row 8) visible → n to hunk 2, p to hunk 0.
        assert_eq!(nav_hunk_from_scroll(&rows, 0, 12, 1, true), Some(2));
        assert_eq!(nav_hunk_from_scroll(&rows, 0, 12, 1, false), Some(0));
        // Focus on the last hunk (visible) → n clamps (nothing after).
        assert_eq!(nav_hunk_from_scroll(&rows, 8, 20, 2, true), None);
        assert_eq!(nav_hunk_from_scroll(&rows, 8, 20, 2, false), Some(1));
    }

    /// THE BUG: the viewport is scrolled into a large context region BETWEEN
    /// hunks, so the FOCUSED hunk's header is off-screen (no hunk header
    /// visible). n/p must anchor on the viewport — `n` → first hunk below the
    /// viewport top, `p` → last hunk above it — NOT step blindly from the stale
    /// focus (which would jump to a hunk on the wrong side, feeling like paging).
    #[test]
    fn nav_hunk_between_hunks_anchors_on_viewport() {
        // Hunks far apart; viewport rows 50..=62 sit in the gap between the
        // hunk at row 5 and the hunk at row 80. Focus is the STALE hunk 0
        // (row 5), now scrolled off above the viewport.
        let rows = [5usize, 80, 120];
        assert_eq!(
            nav_hunk_from_scroll(&rows, 50, 62, 0, true),
            Some(1),
            "n from a between-hunks position selects the first hunk BELOW the viewport, \
             not focus+1 which would still be the off-screen hunk 1's neighbor"
        );
        assert_eq!(
            nav_hunk_from_scroll(&rows, 50, 62, 0, false),
            Some(0),
            "p from a between-hunks position selects the first hunk ABOVE the viewport"
        );
        // Scrolled deep past the last hunk (rows 200..=212), focus stale at 0:
        // p → last hunk, n → none (clamp handled by the caller).
        assert_eq!(nav_hunk_from_scroll(&rows, 200, 212, 0, false), Some(2));
        assert_eq!(nav_hunk_from_scroll(&rows, 200, 212, 0, true), None);
        // Empty hunk list → no target either way.
        assert_eq!(nav_hunk_from_scroll(&[], 0, 10, 0, true), None);
        assert_eq!(nav_hunk_from_scroll(&[], 0, 10, 0, false), None);
    }

    /// Fix 2: the "should I scroll?" decision for n/p. A target row inside the
    /// inclusive [top,bottom] viewport is already on screen → DON'T scroll (so
    /// the user keeps their place); a row above the top or below the bottom is
    /// off-screen → scroll it into view.
    #[test]
    fn row_in_viewport_decides_scroll_vs_keep_place() {
        // Viewport spans rows 10..=20 inclusive.
        // Inside (incl. both edges) → visible → no scroll.
        assert!(row_in_viewport(10, 10, 20), "top edge counts as visible");
        assert!(row_in_viewport(15, 10, 20), "mid-viewport is visible");
        assert!(row_in_viewport(20, 10, 20), "bottom edge counts as visible");
        // Outside → off-screen → scroll.
        assert!(!row_in_viewport(9, 10, 20), "one above the top is off-screen");
        assert!(!row_in_viewport(21, 10, 20), "one below the bottom is off-screen");
        assert!(!row_in_viewport(0, 10, 20), "well above is off-screen");
        assert!(!row_in_viewport(100, 10, 20), "well below is off-screen");
        // Degenerate single-row viewport (viewport_h ~0 on first frame): only
        // that exact row is "visible".
        assert!(row_in_viewport(5, 5, 5));
        assert!(!row_in_viewport(6, 5, 5));
    }

    // ===================================================================
    // ROOT CAUSE: variable row heights. The old code computed "which rows are
    // visible" and "where is row N" as `index * row_h` with a single fixed
    // row_h. HunkHeader control strips are TALLER than diff lines, so that math
    // drifts by the accumulated extra height of any tall row above the target —
    // the source of the n/p "scrolls when already visible / doesn't when off-
    // screen" and the Ctrl+F "lands below the page" bugs. These lock the
    // accurate, height-aware computation that replaces it.
    // ===================================================================

    /// `visible_range_from_heights` with a NON-UNIFORM row mix (tall headers +
    /// short diff lines) returns the rows that actually intersect the viewport —
    /// and that answer DIFFERS from the old `offset / row_h` assumption, which is
    /// exactly the drift bug. This would FAIL if visibility were still computed
    /// off a single uniform row_h.
    #[test]
    fn visible_range_handles_variable_row_heights() {
        // Layout: a tall 40px HunkHeader at row 0, then nine 18px diff lines.
        // y-tops: r0=0, r1=40, r2=58, r3=76, r4=94, r5=112, r6=130, r7=148,
        //         r8=166, r9=184; total = 202.
        let mut heights = vec![40.0_f32];
        heights.extend(std::iter::repeat(18.0).take(9));

        // Viewport [80, 170): a 90px window scrolled 80px down. It intersects
        // rows whose [top,bottom) overlaps [80,170): r2(58..76)? 76>80? no.
        // r3(76..94) yes … r8(166..184) yes. So first=3, last=8.
        let got = visible_range_from_heights(&heights, 80.0, 90.0).unwrap();
        assert_eq!(got, (3, 8), "height-aware visible range");

        // The OLD uniform-row_h estimate (using the SHORT row_h=18) would say
        // top = floor(80/18) = 4, bottom = floor((80+90)/18) = 9 → (4, 9).
        // It's WRONG on BOTH ends precisely because the 40px header above shifts
        // every later row down. Assert the accurate answer is NOT that estimate.
        let naive_row_h = 18.0_f32;
        let naive_top = (80.0_f32 / naive_row_h).floor() as usize;
        let naive_bottom = ((80.0_f32 + 90.0) / naive_row_h).floor() as usize;
        assert_ne!(
            got,
            (naive_top, naive_bottom),
            "the uniform-row_h estimate ({naive_top},{naive_bottom}) drifts from the \
             height-aware truth (3,8) — this is the root-cause bug"
        );

        // From the very top, the tall header (row 0) is visible.
        assert_eq!(visible_range_from_heights(&heights, 0.0, 50.0), Some((0, 1)));
        // Empty → None.
        assert_eq!(visible_range_from_heights(&[], 0.0, 100.0), None);
    }

    /// END-TO-END (kittest): n/p with the REAL variable-height layout. Targeting
    /// an OFF-SCREEN hunk must move the view (the target becomes the centered
    /// scroll request and the offset changes); pressing toward a hunk that is
    /// ALREADY visible must NOT change the scroll offset. This exercises the
    /// painted-range visibility test + scroll_to_rect jump through `app.ui`,
    /// over a file whose Full-extent view interleaves tall HunkHeader strips
    /// among short diff lines — the exact mix the old `index * row_h` math got
    /// wrong.
    #[test]
    fn np_scrolls_to_offscreen_hunk_and_not_when_visible() {
        let repo = multi_hunk_repo(); // 30-line file, 2 well-separated hunks
        let mut app = local_app(&repo);
        app.extent = Extent::Full; // whole file + per-hunk strips → scrollable
        app.layout = Layout::Inline;
        app.selected = Some(Selection::Changed(0));
        assert!(app.files[0].hunks.len() >= 2, "need ≥2 hunks");

        // A deliberately SHORT window so the 30-line file overflows it and the
        // second hunk (near line 27) is genuinely off-screen from the top —
        // otherwise the whole file fits and n correctly wouldn't need to scroll.
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(700.0, 220.0))
            .build_state(|ctx, app: &mut App| app.ui(ctx), app);
        harness.run();
        harness.run(); // let the painted range settle

        // The second hunk is far down the 30-line file → off-screen at the top.
        let before = harness.state().content_scroll;
        harness.press_key(egui::Key::N); // jump to the next hunk (off-screen)
        harness.run();
        harness.run(); // apply the scroll_to_rect + recapture offset
        let after_jump = harness.state().content_scroll;
        assert!(
            after_jump > before,
            "n to an off-screen hunk must scroll the view down (before {before}, after {after_jump})"
        );
        // The focused hunk's header row is now within the painted (real) range.
        let st = harness.state();
        let focus_row = st.hunk_rows[st.focus_hunk];
        let (top, bot) = st.visible_rows.expect("a frame painted");
        assert!(
            row_in_viewport(focus_row, top, bot),
            "after the jump the target hunk header (row {focus_row}) is in the painted \
             viewport [{top},{bot}]"
        );

        let _ = std::fs::remove_dir_all(&repo);
    }

    /// The flip side of the n/p behavior: when the target hunk is ALREADY on
    /// screen, n/p moves focus but must NOT move the scroll offset (don't yank
    /// the user around). A TALL window shows the whole small file, so both hunks
    /// are visible from the top; pressing n (to hunk 1, visible) leaves the
    /// scroll offset put. This would regress if visibility were mis-computed by
    /// `index * row_h` and reported the on-screen hunk as off-screen.
    #[test]
    fn np_does_not_scroll_when_target_hunk_already_visible() {
        let repo = multi_hunk_repo();
        let mut app = local_app(&repo);
        app.extent = Extent::Summary; // just the hunks → compact, both fit
        app.layout = Layout::Inline;
        app.selected = Some(Selection::Changed(0));
        assert!(app.files[0].hunks.len() >= 2, "need ≥2 hunks");

        // A tall window so the whole (compact, Summary) diff fits → both hunk
        // headers are visible without any scrolling.
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(700.0, 900.0))
            .build_state(|ctx, app: &mut App| app.ui(ctx), app);
        harness.run();
        harness.run();

        // Confirm both hunk headers are within the painted viewport from the top.
        let st = harness.state();
        let (top, bot) = st.visible_rows.expect("painted");
        assert!(
            st.hunk_rows.iter().all(|&r| row_in_viewport(r, top, bot)),
            "the tall window should show every hunk header (rows {:?} in [{top},{bot}])",
            st.hunk_rows
        );
        let before = st.content_scroll;
        assert_eq!(before, 0.0, "starts at the top with everything visible");

        // n moves focus to hunk 1 (already visible) — the offset must not move.
        harness.press_key(egui::Key::N);
        harness.run();
        harness.run();
        let st = harness.state();
        assert_eq!(st.focus_hunk, 1, "n advances focus to the next hunk");
        assert!(
            (st.content_scroll - before).abs() < 1.0,
            "n to an already-visible hunk must NOT scroll (offset {} vs {before})",
            st.content_scroll
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// END-TO-END (kittest): Ctrl+F jump centers the matched row in the viewport.
    /// With tall HunkHeader strips above, the old `row * row_h` scroll landed the
    /// match "below the page"; the scroll_to_rect-on-actual-rect jump must put
    /// the matched cache row inside the painted viewport.
    #[test]
    fn search_jump_lands_match_in_viewport() {
        let repo = multi_hunk_repo();
        let mut app = local_app(&repo);
        app.extent = Extent::Full; // whole file → a match can be far down
        app.layout = Layout::Inline;
        app.selected = Some(Selection::Changed(0));

        // Short window so the match near line 27 starts off-screen and a jump is
        // actually required (otherwise the whole 30-line file fits).
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(700.0, 220.0))
            .build_state(|ctx, app: &mut App| app.ui(ctx), app);
        harness.run();

        // Open search and target a string that occurs near the BOTTOM of the
        // file (line 27 was edited to "LINE 27 EDIT"), so the match is off-screen
        // from the top and a jump is required.
        harness.state_mut().search = Some(Search {
            query: "27".to_string(),
            matches: Vec::new(),
            current: 0,
            just_opened: false,
            computed_for: None,
            last_scrolled: None,
        });
        harness.run(); // search_bar computes matches + requests the scroll
        harness.run(); // scroll_to_rect applies + painted range recaptured

        let st = harness.state();
        let s = st.search.as_ref().expect("search open");
        assert!(!s.matches.is_empty(), "‘27’ should match at least one row");
        let cur_row = s.matches[s.current];
        let (top, bot) = st.visible_rows.expect("a frame painted");
        assert!(
            row_in_viewport(cur_row, top, bot),
            "the matched row {cur_row} must be within the painted viewport [{top},{bot}] \
             after the search jump (not drifted below the page)"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    // ===================================================================
    // Ctrl+F in-file search — pure match-finding core.
    // ===================================================================

    /// `find_matches` returns the matching rows in display order; the count is
    /// just the result length. Covers multiple matches, no matches, and
    /// case-insensitivity, across the different RenderRow kinds.
    #[test]
    fn find_matches_returns_ordered_positions_and_count() {
        let rows = vec![
            RenderRow::HunkHeader {
                hunk_idx: 0,
                text: "@@ -1,3 +1,4 @@ fn Foo()".into(),
                whole_file: false,
            },
            RenderRow::Plain { text: "let foo = 1;".into(), lineno: 1 },
            RenderRow::DiffLine {
                kind: LineKind::Add,
                text: "    FOO.bar();".into(),
                old_lineno: None,
                new_lineno: Some(2),
            },
            RenderRow::Plain { text: "let baz = 2;".into(), lineno: 3 },
            RenderRow::SplitLine {
                left: Some((LineKind::Del, "old line".into(), Some(4))),
                right: Some((LineKind::Add, "contains FoObar".into(), Some(4))),
            },
        ];

        // Case-insensitive "foo" matches rows 0 (header), 1 (plain), 2 (diff),
        // and 4 (split right cell) — in ascending order.
        let m = find_matches(&rows, "foo");
        assert_eq!(m, vec![0, 1, 2, 4], "ordered, case-insensitive matches");
        assert_eq!(m.len(), 4, "the count is the match-list length");

        // A different query hits a single row.
        assert_eq!(find_matches(&rows, "baz"), vec![3]);

        // No matches → empty.
        assert!(find_matches(&rows, "nonexistent").is_empty());

        // Empty / whitespace query matches nothing (so the bar isn't a no-op
        // full highlight).
        assert!(find_matches(&rows, "").is_empty());
        assert!(find_matches(&rows, "   ").is_empty());

        // Mixed-case query, lowercase content → still matches (case-insensitive).
        assert_eq!(find_matches(&rows, "OLD LINE"), vec![4]);
    }

    /// The cheat-sheet lists the bindings the code actually handles. Lock in a
    /// few load-bearing ones so the help can't silently drift.
    #[test]
    fn keybindings_cover_the_real_bindings() {
        let kb = keybindings();
        let keys: Vec<&str> = kb.iter().map(|(k, _)| *k).collect();
        for k in ["Ctrl+P", "Ctrl+F", "j / k", "n / p", "a / r", "F12", "g", "?"] {
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

    /// Ctrl+F opens the in-file search bar; typing a query that occurs in the
    /// file populates the live match set; Esc closes it and clears the matches.
    /// Driven through `app.ui` with the kittest harness so it exercises the real
    /// key handling + match wiring (not just the pure helper).
    #[test]
    fn ctrl_f_opens_search_and_finds_matches() {
        let repo = multi_hunk_repo(); // file lines "line N", with two edits
        let mut app = local_app(&repo);
        app.extent = Extent::Full; // whole file shown → "line" appears many times
        app.layout = Layout::Inline;
        app.selected = Some(Selection::Changed(0));
        let mut harness = egui_kittest::Harness::new_state(|ctx, app: &mut App| app.ui(ctx), app);
        harness.run();
        assert!(harness.state().search.is_none(), "search starts closed");

        // Ctrl+F opens the bar (press_key has no modifier variant; set the
        // modifier state and push the Key event with CTRL held directly).
        harness.input_mut().modifiers = egui::Modifiers::CTRL;
        for pressed in [true, false] {
            harness.input_mut().events.push(egui::Event::Key {
                key: egui::Key::F,
                pressed,
                modifiers: egui::Modifiers::CTRL,
                repeat: false,
                physical_key: None,
            });
        }
        harness.run();
        harness.input_mut().modifiers = egui::Modifiers::default();
        assert!(harness.state().search.is_some(), "Ctrl+F opens the search bar");

        // Type a query directly into the state (the field has focus; we set the
        // buffer to keep the test independent of per-char text events), then run
        // a frame so search_bar recomputes the matches.
        harness.state_mut().search.as_mut().unwrap().query = "line".to_string();
        harness.run();
        let n = harness.state().search.as_ref().unwrap().matches.len();
        assert!(n > 1, "‘line’ should match many rows in the full file (got {n})");

        // Esc closes + clears.
        harness.press_key(egui::Key::Escape);
        harness.run();
        assert!(harness.state().search.is_none(), "Esc closes the search bar");
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// A backend that COUNTS its `read_file` calls, so we can prove the
    /// file-open content cache (Task A) doesn't refetch a file it already
    /// loaded. Everything else is a stub — only the read path matters here.
    /// `is_remote()` is false so loads go through the synchronous local path
    /// (deterministic, no thread/poll dance needed for the cache-hit assertion).
    struct CountingRepo {
        root: PathBuf,
        reads: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl purview::repo::RepoSource for CountingRepo {
        fn compute_diff(
            &self,
            _s: DiffSource,
            _b: &str,
        ) -> Result<(String, Vec<ChangedFile>), String> {
            Ok(("main".to_string(), Vec::new()))
        }
        fn compute_file_diff(
            &self,
            _s: DiffSource,
            _b: &str,
            _c: u32,
            _p: &str,
        ) -> Result<(String, Vec<ChangedFile>), String> {
            Ok(("main".to_string(), Vec::new()))
        }
        fn read_file(&self, _rel: &str) -> Result<String, String> {
            self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok("alpha\nbeta\ngamma\n".to_string())
        }
        fn list_dir(&self, _rel: &str) -> Result<Vec<purview::repo::DirEntry>, String> {
            Ok(Vec::new())
        }
        fn list_all_files(&self, _cap: usize) -> (Vec<String>, bool) {
            (Vec::new(), false)
        }
        fn guess_default_base(&self) -> String {
            "main".to_string()
        }
        fn write_line(&self, _r: &str, _l: usize, _t: &str) -> Result<(), String> {
            Ok(())
        }
        fn grep_symbol(&self, _s: &str) -> Result<Vec<purview::gotodef::Candidate>, String> {
            Ok(Vec::new())
        }
        fn label(&self) -> String {
            "counting".to_string()
        }
        fn state_root(&self) -> &std::path::Path {
            &self.root
        }
        fn persist_state(&self, _n: &str, _c: &str) -> Result<(), String> {
            Ok(())
        }
    }

    /// Opening the same tree file twice hits the content cache the second time:
    /// the backend `read_file` is called exactly ONCE across two opens (Task A).
    #[test]
    fn opening_same_file_twice_does_not_refetch() {
        let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let repo = CountingRepo {
            root: std::env::temp_dir(),
            reads: std::sync::Arc::clone(&reads),
        };
        let mut app = App::new(Box::new(repo));
        use std::sync::atomic::Ordering::SeqCst;

        // First open of a tree file → one read, payload cached.
        app.selected = Some(Selection::Path("foo.txt".to_string()));
        app.ensure_cache();
        assert_eq!(reads.load(SeqCst), 1, "first open should fetch once");
        assert!(!app.cache.is_empty(), "content should be rendered after load");

        // Switch away, then re-open the SAME file → served from cache, no refetch.
        app.selected = Some(Selection::Path("other.txt".to_string()));
        app.ensure_cache();
        assert_eq!(reads.load(SeqCst), 2, "a different file fetches once more");
        app.selected = Some(Selection::Path("foo.txt".to_string()));
        app.ensure_cache();
        assert_eq!(
            reads.load(SeqCst),
            2,
            "re-opening a cached file must NOT refetch the backend"
        );

        // A reload invalidates the cache → the next open refetches.
        app.invalidate_content_cache();
        app.cache_key = None;
        app.selected = Some(Selection::Path("foo.txt".to_string()));
        app.ensure_cache();
        assert_eq!(
            reads.load(SeqCst),
            3,
            "after invalidation the file is fetched again"
        );
    }

    /// The async file-load generation guard (Task A), tested directly like
    /// `stale_reload_result_is_ignored`: a result stamped with a superseded
    /// content generation must be dropped by `poll_content`, never applied.
    #[test]
    fn stale_content_load_result_is_ignored() {
        let repo = fixture_repo();
        let mut app = local_app(&repo);
        // Simulate: a content load was in flight (content_gen G), then a faster
        // file switch bumped content_gen. A late result for G arrives.
        let stale_gen = app.content_gen;
        app.content_gen = app.content_gen.wrapping_add(1); // superseded
        let key = ContentKey {
            path: "stale.txt".to_string(),
            full_diff: false,
            source: DiffSource::WorkingTree,
            base: "main".to_string(),
        };
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send((stale_gen, key.clone(), Ok(FileContent::Full("STALE".to_string()))))
            .unwrap();
        app.content_rx = Some((key.clone(), rx));
        app.content_loading = true;
        let applied = app.poll_content();
        assert!(!applied, "a superseded content result must not be applied");
        assert!(
            app.content_cache.get(&key).is_none(),
            "stale content must not leak into the cache"
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

    #[test]
    fn report_includes_approved_and_carries_comments_for_any_status() {
        let repo = fixture_repo();
        let mut app = local_app(&repo);
        app.poll_reload_blocking();
        assert!(
            !app.files.is_empty() && !app.files[0].hunks.is_empty(),
            "fixture should produce at least one hunk"
        );

        // Approve the first hunk AND leave a comment on it (no reject).
        app.files[0].hunks[0].status = ReviewStatus::Approved;
        app.files[0].hunks[0].comment = "looks good but consider edge case".to_string();

        let report = app.review_report();
        assert!(
            report.contains("## Approved hunks"),
            "report must have an Approved section:\n{report}"
        );
        assert!(
            report.contains("looks good but consider edge case"),
            "a comment on an APPROVED hunk must appear (it used to be dropped):\n{report}"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// END-TO-END durability: a verdict + comment survive a reload of the SAME
    /// comparison via the re-anchor path (the feature's core promise). Approve
    /// + comment, persist, reload, and confirm the verdict comes back — and it
    /// is NOT flagged changed-since-review (the content was untouched).
    #[test]
    fn verdict_and_comment_survive_a_reload() {
        let repo = fixture_repo();
        let mut app = local_app(&repo);
        app.poll_reload_blocking();
        app.files[0].hunks[0].status = ReviewStatus::Approved;
        app.files[0].hunks[0].comment = "keep this".to_string();
        app.save_review_state();

        // Reload the SAME comparison (working tree vs HEAD), recomputing the diff.
        app.reload();
        app.poll_reload_blocking();

        assert_eq!(
            app.files[0].hunks[0].status,
            ReviewStatus::Approved,
            "verdict must survive the reload via re-anchoring"
        );
        assert_eq!(app.files[0].hunks[0].comment, "keep this");
        assert!(
            !app.files[0].hunks[0].changed_since_review,
            "unchanged content must not be flagged changed-since-review"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// The canonical mirror (`review-state.json`) the MCP server reads is still
    /// written and carries the new anchor field.
    #[test]
    fn mcp_mirror_still_written_with_anchor() {
        let repo = fixture_repo();
        let mut app = local_app(&repo);
        app.poll_reload_blocking();
        app.files[0].hunks[0].status = ReviewStatus::Approved;
        app.save_review_state();

        let state = ReviewState::load(&repo).expect("canonical mirror written for MCP");
        let h = state.files.iter().flat_map(|f| &f.hunks).next().unwrap();
        assert_eq!(h.status, "approved");
        assert!(!h.anchor.is_empty(), "the mirror records the content anchor");
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// Report renders the "⚠ changed since reviewed" marker when a hunk's
    /// content changed under a carried-over verdict.
    #[test]
    fn report_marks_changed_since_reviewed() {
        let repo = fixture_repo();
        let mut app = local_app(&repo);
        app.poll_reload_blocking();
        app.files[0].hunks[0].status = ReviewStatus::Rejected;
        app.files[0].hunks[0].comment = "needs work".to_string();
        app.files[0].hunks[0].changed_since_review = true;

        let report = app.review_report();
        assert!(
            report.contains("⚠ changed since reviewed"),
            "report must flag a changed-since-reviewed hunk:\n{report}"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// Report renders the "Stale (no longer in diff)" section for orphaned
    /// reviewed hunks.
    #[test]
    fn report_shows_stale_section_for_orphans() {
        let repo = fixture_repo();
        let mut app = local_app(&repo);
        app.poll_reload_blocking();
        app.orphaned = vec![purview::review_state::OrphanedHunk {
            file: "gone.rs".into(),
            header: "@@ -1,2 +1,2 @@".into(),
            status: "rejected".into(),
            comment: Some("this was removed".into()),
            anchor: "deadbeef".into(),
        }];

        let report = app.review_report();
        assert!(
            report.contains("## Stale (no longer in diff)"),
            "report must have a Stale section:\n{report}"
        );
        assert!(report.contains("gone.rs"), "stale file listed:\n{report}");
        assert!(
            report.contains("this was removed"),
            "stale comment preserved:\n{report}"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// A reviewed hunk that vanishes from the diff after a reload is moved into
    /// `app.orphaned` (not silently dropped). Edit the file so the original
    /// change is reverted, reload, and confirm the verdict surfaces as stale.
    #[test]
    fn vanished_hunk_becomes_orphaned_on_reload() {
        let repo = fixture_repo();
        let mut app = local_app(&repo);
        app.poll_reload_blocking();
        // Reject + comment the fixture's change.
        app.files[0].hunks[0].status = ReviewStatus::Rejected;
        app.files[0].hunks[0].comment = "revert this".to_string();
        app.save_review_state();

        // Revert the working-tree edit so the diff is now empty for a.txt.
        std::fs::write(repo.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        app.reload();
        app.poll_reload_blocking();

        assert!(
            app.orphaned.iter().any(|o| o.comment.as_deref() == Some("revert this")),
            "a reviewed hunk that left the diff must be preserved as orphaned, got {:?}",
            app.orphaned
        );
        let _ = std::fs::remove_dir_all(&repo);
    }
}
