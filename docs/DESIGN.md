# purview — design notes

## Thesis

Every modern review tool treats the **PR** as the unit of work. For
AI-generated code the **codebase** is the unit of work — the reviewer's
job is "does this fit what already exists," not "did the author err."

So purview is a **codebase IDE with the diff as an overlay**, not a diff
viewer with codebase context bolted on. Same file tree, same navigation,
same speed as an editor — with the diff painted on top and review state
tracked.

Native (Rust + egui), no webview, no Electron. Fast on a massive monorepo
is a hard requirement (Aurora's repo). Claude is a first-class
participant, not a post-hoc commenter.

## Requirements (from Sean, 2026-05-25)

- Syntax highlighting (diffs and full files).
- Full-file view, optional — and the ability to open *unchanged* files.
- Performance on a massive monorepo is paramount.
- Symbol lookup (go-to-definition). (Sean floated a Haiku call for it;
  see "Symbol nav" below — probably wrong tool at monorepo scale.)
- Multiple diff sources / workflows:
  - unstaged changes, stage one at a time (git add -p style)
  - everything-since-a-base → add to an approved list that builds a
    report you send to Claude
  - explicit approve/deny per change; editor tracks what you've reviewed
    ("what's left?")
  - comprehension mode: explain each chunk, Claude grades you
- Cloud/Claude features gated behind selecting a session.
- Prominently display worktree / branch / session (header or footer).
- Workflow sketch:
  - open reviewer onto a worktree → select base + diff
  - top-left: changed-files list; bottom-left: full file-structure tree
  - click changed file → diffs, optionally whole file, easy jump
    change-section → change-section
  - per-chunk approve / deny + comment (GitHub-review-like)
  - jump to definition of a function the change uses
  - right-click → comment / ask question; posting a comment notifies
    Claude, which can respond live in the thread — OR batch comments and
    submit at end
  - inline file editing

## Research-backed feature priority (2026-05-25)

From a survey of GitHub-PR-UI gripes + code-review best-practice writing
(Google eng-practices, academic mental-model work, AI-PR-review posts).
Ranked by impact:

1. **Full-file + whole-codebase context on demand, no "show more."**
   Kills the #1 GitHub gripe (starved diff context); enables review-in-
   context. *Critical for AI code — judging fit requires the surroundings.*
2. **Semantic navigation (LSP): go-to-def, find-refs, hover types,
   cross-file.** The mechanism for design/fit judgment. *Critical for AI —
   "does a utility for this already exist?" is a find-references question.*
3. **First-class review-state model** — per-file/per-chunk
   reviewed/needs-another-look, persistent, with a progress overview.
   *Critical for huge diffs: you must resume across sessions.*
4. **Reviewer-chosen reading order, broad-to-narrow** — surface the PR
   description + "main" files + tests-first toggle; don't dump
   alphabetical. Encodes Google's navigate strategy.
5. **Fast on huge diffs.** Sub-second file switching where GitHub takes
   8-10s. The native app's whole premise.
6. **"Does this duplicate existing code?"** — on a new function, surface
   similar/identical existing symbols. *AI-specific: code-reuse blindness.*
7. **Critical-path tracing** — follow a call chain through the diff to
   catch hallucinated correctness. *AI-specific.*
8. **Coherent comment model + batched suggestion apply** (no reflow per
   resolution).
9. **CI/test-config diff highlighted first** — flag weakened thresholds,
   deleted tests, skipped lint. *AI-specific: CI-gaming detection.*
10. **Comment durability across commits.**

Table stakes that make it *feel* different in 30s: 1, 2, 5. What makes a
300-file AI PR completable: 3. The AI-native differentiators: 6, 7, 9.

### Symbol nav: Haiku vs LSP

Sean floated a Haiku call for symbol lookup. At monorepo scale that's the
wrong tool — per-symbol model calls are too slow and costly, and won't be
authoritative. Use a real index: LSP (clangd / rust-analyzer / pyright
per language) or a ctags/Sourcegraph-style symbol index. Reserve Claude
for the *judgment* layer (does this fit, is this duplicated, explain this
chunk) where it's actually differentiated — not for mechanical lookup.

## Status

- **v0** (commit 9ce5a32): working-tree-vs-HEAD diff, changed-files list,
  color-coded diff pane.
- **v0.2** (in progress): syntect syntax highlighting, Diff/Full-File
  toggle, branch in header, virtualized rows (monorepo perf).

## Roadmap (rough next order)

1. Branch-range base selection (`main...HEAD`) — what you actually review.
2. Nested file tree (bottom-left), full repo structure.
3. Jump change-section → change-section (next/prev hunk keys).
4. Per-chunk approve/deny review state + "what's left" overview.
5. LSP integration (go-to-def, find-refs).
6. Claude agent pane (session-gated): comments → live Claude responses,
   blast-radius queries, duplicate detection, comprehension-mode grading.
7. Inline editing.
