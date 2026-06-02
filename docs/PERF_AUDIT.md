# purview Performance Audit

*Dr. Mara Voss — systems & Rust performance · 2026-06-02 · against commit 9caaf5e*

## Executive summary

purview's architecture is fundamentally sound for a GUI diff viewer: row rendering
is virtualized via `show_rows`, syntax highlighting is genuinely lazy and memoized
per visible row, and all the slow IO (diff compute, SSH file reads, state
persistence, go-to-def) is correctly pushed onto worker threads. What remains are
two classes of problem: (1) a **continuous-repaint loop driven by a 2-second timer
plus a per-frame disk scan** that burns CPU even when nothing changes, and (2)
**per-visible-row allocation churn** in the paint path (clone-on-cache-hit,
per-character widget splitting, per-row gutter `format!`) that caps scroll
smoothness on large files. The build profile also leaves single-binary wins on
the table.

---

## CRITICAL

### C1. Per-frame disk scan + forced 2s repaint whenever a changed file is open — *certain*
`src/main.rs:2254-2257`, every frame of `ui()`:
```rust
let replies = Replies::load(&self.state_root);
if active_file.is_some() {
    ctx.request_repaint_after(std::time::Duration::from_secs(2));
}
```
`Replies::load` runs every frame the content pane is up — `read_dir`, sort, then
`read_to_string` + `serde_json::from_str` on each reply file. The
`active_file.is_some()` forces `request_repaint_after(2s)`, so the app never goes
idle while reviewing — re-running this scan at least every 2s forever, plus on
every actual repaint. Biggest steady-state CPU/IO drain.

**Fix:** Cache `Replies` in `App`; reload only on directory-mtime change (one
`stat`), or move reply-watching to a background thread that pushes new replies
over a channel and calls `request_repaint()` only when one arrives. Drop the
unconditional 2s repaint — make reply arrival event-driven like the diff/content
loaders already are.

---

## HIGH

### H2. `row_spans`/`split_spans` clone the memoized span vec on every cache hit — *certain*
`src/main.rs:1330-1331`, `1349-1351`: `return spans.clone()` per visible row per
frame. Memoization avoids re-highlighting, but the hit path clones the entire
`Vec<(Color32, String)>` (every span string). ~50 rows × 60fps = thousands of
String allocs/sec for read-only data.
**Fix:** read `&self.hl_cache.borrow()[i]` directly while drawing (ensure-then-borrow
helper), or store `Rc<Spans>` and bump a refcount instead of deep-cloning.

### H3. `line_row` splits every span into per-character runs and emits a widget per identifier run — *certain*
`src/main.rs:3348-3384`. Per visible row per frame, each line is walked char-by-char
and each ident/non-ident run becomes a separate egui widget (`Label`/`label`),
each laid out + hit-tested. 10-30 widgets/row × ~50 rows/frame. Likely the
per-frame layout-cost ceiling.
**Fix:** render the whole line as one `LayoutJob`/galley with one clickable
`Response`; map click x → token via the galley cursor API. Collapses 10-30
widgets/row to 1. (Touches the symbol-click feature — needs care + a regression
test.) Quick partial: skip ident-splitting on non-clickable Plain rows.

### H4. `reanchor_onto` is O(live × saved) with a hash per hunk — *certain, per-rebuild (off-thread)*
`src/review_state.rs:350-441`. Three nested `find` passes over saved hunks per live
hunk, per file. Quadratic on huge diffs, but runs on the reload worker thread, so
it delays content appearing, not frame rate.
**Fix:** index saved hunks in `HashMap<anchor, idx>` + `HashMap<header, idx>` once
per file → O(1) lookups.

---

## MEDIUM
- **M5** `ensure_contrast` 24-iter binary search (~72 `powf`) per dark token
  (`highlight.rs:114-173`) — memoize over the theme palette (`HashMap<Color32,Color32>`).
- **M6** `SynHighlighter::new(&self.theme)` rebuilt per incremental line
  (`highlight.rs:82`) — build once, store it.
- **M7** Per-row gutter `format!`/`fmt_lineno` allocations in the inner loop —
  precompute gutter strings into the cache (pure function of fixed inputs).
- **M8** `extent`/`layout` toggle rebuilds the cache cloning every line
  (`split_align` 3032-3048, Full 1159/1222) — one-shot/user-initiated; could
  borrow `Rc<str>`/indices instead of cloning.
- **M9** Build profile: add `lto="fat"`, `codegen-units=1`, `panic="abort"`,
  `strip=true` to `[profile.release]`. Free.

## LOW
- **L10** `SyntaxSet::load_defaults_newlines()` at startup (one-shot) — trim only if
  startup is a complaint.
- **L11** `quick_open_overlay` re-scores up to 50k paths every frame while open —
  cache scored result keyed on query.
- **L12** `find_matches` lowercases a String per row on recompute — gated to
  query/generation change, fine as-is.

## Suspected — needs a profiler
- **S1** Which dominates per-frame: H3 (widget-per-token layout/hit-test) or H2
  (span clone allocs). Flamegraph while holding scroll on a 5k-line file, Inline vs Split.
- **S2** `Replies::load` real cost at realistic reply counts (the `replies_load_500`
  bench × repaint frequency). >1ms ⇒ C1 unambiguously critical.
- **S3** Watch idle CPU% with a changed file open — periodic nonzero confirms C1.
- **S4** egui galley cache may soften H3's layout cost (not its hit-test/widget count).
  Profile static vs scrolling frame.

## Quick wins (cheap, high-confidence)
1. C1 — drop the 2s repaint + cache Replies (mtime/event-driven). Biggest steady win.
2. H2 — return `&Spans`/`Rc<Spans>` instead of deep-cloning on the hit path.
3. M5 — memoize `ensure_contrast` over the palette.
4. M6 — build `SynHighlighter` once.
5. M9 — profile tweaks (`lto=fat`, `codegen-units=1`, `panic=abort`, `strip`).
6. M7 — precompute gutter strings into the cache.

## Deeper work
- H3 — one-galley `line_row` with cursor-based click mapping (highest-value structural).
- H4 — HashMap-index the re-anchor lookups.
- M8 — borrow instead of clone in split/Full cache builds.

## Already well-optimized
Lazy per-visible-row highlight memoization + correct incremental cross-line path;
all blocking IO off the UI thread with generation-guarded application (diff, SSH
content, `StateWriter` with burst coalescing, go-to-def); `show_rows`
virtualization (per-row costs scale with *visible* rows); bounded FIFO content
cache; comment editor persists on close not per keystroke.
