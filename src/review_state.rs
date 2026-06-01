//! Serializable review state, written by the GUI to
//! `<repo>/.purview/review-state.json` and read by the MCP server. This is
//! the contract between the two processes — keep it stable.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Write `contents` to `path` atomically: write a sibling temp file, then
/// rename over the target. Rename is atomic on the same filesystem, so a
/// concurrent reader sees either the old file or the new one — never a
/// half-written one.
fn atomic_write(path: &Path, contents: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

/// Create `<repo_root>/.purview/` and ensure it self-ignores via a
/// `.gitignore` containing `*`, so review state never gets committed into
/// the repo being reviewed. The user doesn't have to add anything by hand.
pub fn ensure_purview_dir(repo_root: &Path) -> std::io::Result<PathBuf> {
    let dir = repo_root.join(".purview");
    std::fs::create_dir_all(&dir)?;
    let gi = dir.join(".gitignore");
    if !gi.exists() {
        // Ignore everything in .purview/ (including this file).
        let _ = std::fs::write(&gi, "*\n");
    }
    Ok(dir)
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ReviewState {
    pub branch: String,
    /// Human-readable diff range, e.g. "main...HEAD" or "working tree vs HEAD".
    pub range: String,
    pub files: Vec<FileState>,
    /// Hunks that carried a verdict/comment but whose `anchor` no longer
    /// appears anywhere in the freshly-recomputed diff (the changed code was
    /// removed or reverted). Preserved here rather than silently dropped so the
    /// reviewer's notes survive, and surfaced in the report under a "Stale" /
    /// "no longer in diff" section. Defaulted so older files load cleanly.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub orphaned: Vec<OrphanedHunk>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FileState {
    pub path: String,
    pub hunks: Vec<HunkState>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct HunkState {
    pub header: String,
    /// "unreviewed" | "approved" | "rejected".
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    /// Content-anchor of the hunk (see `diff::Hunk::content_anchor`). The
    /// stable identity used to carry a verdict/comment across a moving
    /// worktree. Empty for migrated old-format entries where the changed lines
    /// weren't available to hash; those fall back to matching by `header`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub anchor: String,
    /// True when this hunk was reviewed but its changed content has since
    /// changed (the surrounding region still matches but the +/- lines differ).
    /// The comment is carried over but flagged so the UI + report can warn
    /// "⚠ changed since reviewed".
    #[serde(default, skip_serializing_if = "is_false")]
    pub changed_since_review: bool,
}

/// A reviewed hunk whose anchor no longer appears in the current diff.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct OrphanedHunk {
    pub file: String,
    pub header: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub anchor: String,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Carry a saved hunk's verdict + comment onto a live diff hunk, setting the
/// `changed_since_review` flag. Shared by the re-anchor passes.
fn apply_saved(live: &mut crate::diff::Hunk, saved: &HunkState, changed: bool) {
    live.status = match saved.status.as_str() {
        "approved" => crate::diff::ReviewStatus::Approved,
        "rejected" => crate::diff::ReviewStatus::Rejected,
        _ => crate::diff::ReviewStatus::Unreviewed,
    };
    if let Some(c) = &saved.comment {
        live.comment = c.clone();
    }
    live.changed_since_review = changed;
}

/// What the review state is keyed against — one comparison. Sanitized into a
/// filename so reviews for different bases never clobber each other.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ComparisonKey {
    /// Working tree vs HEAD.
    WorkingTree,
    /// `<base>...HEAD` three-dot branch range.
    BranchRange { base: String },
}

impl ComparisonKey {
    /// The sanitized filename stem for this comparison (no extension). Slashes
    /// and other path-hostile characters in a base ref are mapped the same way
    /// `SshRepo`'s key sanitizer does (alnum/._- kept, rest → '_').
    pub fn file_stem(&self) -> String {
        match self {
            ComparisonKey::WorkingTree => "worktree-vs-HEAD".to_string(),
            ComparisonKey::BranchRange { base } => {
                format!("{}__HEAD", sanitize_key(base))
            }
        }
    }

    /// The human-readable range label the GUI stores in `ReviewState.range`.
    /// Used by migration to tell which comparison an old single-file state
    /// belonged to. MUST stay in sync with `main.rs`'s `range` string.
    pub fn range_label(&self) -> String {
        match self {
            ComparisonKey::WorkingTree => "working tree vs HEAD".to_string(),
            ComparisonKey::BranchRange { base } => format!("{base}...HEAD"),
        }
    }
}

/// Sanitize a string into a safe filename component: alnum/._- kept, all else
/// → '_'. Mirrors `repo::ssh`'s `sanitize` so the keying is consistent.
fn sanitize_key(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == '.' || c == '_' || c == '-' { c } else { '_' })
        .collect()
}

/// A reply posted by the connected agent (via the MCP `reply_to_comment`
/// tool) against a specific hunk's comment. Stored in a SEPARATE file from
/// ReviewState so the GUI (which owns review-state.json) and the MCP server
/// (which owns replies.json) never clobber each other's writes.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Reply {
    pub file: String,
    pub hunk_header: String,
    pub text: String,
    /// Content-anchor of the target hunk, carried alongside `hunk_header` so a
    /// reply still attaches to the right hunk after the worktree moves and the
    /// `@@` header shifts. Empty for replies posted before anchors existed (or
    /// by the MCP server, which only knows the header) — those match by header.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub anchor: String,
}

/// Agent replies, stored as one file per reply under `.purview/replies/`.
///
/// One-file-per-reply (rather than a single appended array) makes concurrent
/// posting inherently safe: there's no read-modify-write, so two MCP
/// processes (e.g. Claude restarting / multiple clients on one repo) can't
/// lose each other's replies, and each file write can't corrupt another.
/// Files are named `<millis>-<counter>.json` so directory order is post order.
#[derive(Clone, Debug, Default)]
pub struct Replies {
    pub replies: Vec<Reply>,
}

impl Replies {
    pub fn dir_for(repo_root: &Path) -> PathBuf {
        repo_root.join(".purview").join("replies")
    }

    /// Load all reply files (sorted by filename = post order). Malformed
    /// individual files are skipped, not fatal.
    pub fn load(repo_root: &Path) -> Self {
        let dir = Self::dir_for(repo_root);
        let mut entries: Vec<PathBuf> = match std::fs::read_dir(&dir) {
            Ok(rd) => rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().map(|e| e == "json").unwrap_or(false))
                .collect(),
            Err(_) => return Self::default(),
        };
        entries.sort();
        let replies = entries
            .iter()
            .filter_map(|p| std::fs::read_to_string(p).ok())
            .filter_map(|s| serde_json::from_str::<Reply>(&s).ok())
            .collect();
        Replies { replies }
    }

    pub fn append(repo_root: &Path, reply: Reply) -> std::io::Result<()> {
        ensure_purview_dir(repo_root)?; // self-ignoring .purview/
        let dir = Self::dir_for(repo_root);
        std::fs::create_dir_all(&dir)?;
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        // Add the process id + a nanosecond tail to avoid same-millis collisions
        // between distinct posts/processes.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let name = format!("{millis}-{}-{nanos}.json", std::process::id());
        let json = serde_json::to_string_pretty(&reply)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        atomic_write(&dir.join(name), &json)
    }

    /// Replies matching a given file + hunk header, in order. Header-only
    /// matching (kept for the MCP server and any pre-anchor replies).
    pub fn for_hunk<'a>(&'a self, file: &str, hunk_header: &str) -> Vec<&'a Reply> {
        self.replies
            .iter()
            .filter(|r| r.file == file && r.hunk_header == hunk_header)
            .collect()
    }

    /// Replies matching a given file + hunk, identified by EITHER its content
    /// `anchor` (preferred — survives the `@@` header shifting) OR its header
    /// (fallback for replies stored without an anchor). An empty `anchor`
    /// disables the anchor path (header-only), matching `for_hunk`.
    pub fn for_hunk_anchored<'a>(
        &'a self,
        file: &str,
        hunk_header: &str,
        anchor: &str,
    ) -> Vec<&'a Reply> {
        self.replies
            .iter()
            .filter(|r| {
                r.file == file
                    && ((!anchor.is_empty() && !r.anchor.is_empty() && r.anchor == anchor)
                        || r.hunk_header == hunk_header)
            })
            .collect()
    }
}

impl ReviewState {
    /// `<repo_root>/.purview/review-state.json` — the CANONICAL "current
    /// comparison" mirror. The GUI writes the active comparison's state here in
    /// addition to its per-comparison file, so `purview-mcp` (which reads this
    /// exact path) keeps seeing the live review with no changes. See `save`.
    pub fn path_for(repo_root: &Path) -> PathBuf {
        repo_root.join(".purview").join("review-state.json")
    }

    /// `<repo_root>/.purview/state/` — the per-comparison state directory.
    pub fn state_dir(repo_root: &Path) -> PathBuf {
        repo_root.join(".purview").join("state")
    }

    /// `<repo_root>/.purview/state/<key>.json` — the per-comparison file. Each
    /// base↔head comparison gets its own file so switching the base never
    /// clobbers another comparison's review.
    pub fn path_for_comparison(repo_root: &Path, key: &ComparisonKey) -> PathBuf {
        Self::state_dir(repo_root).join(format!("{}.json", key.file_stem()))
    }

    /// Write to the canonical mirror only. Kept for the MCP contract + the
    /// existing tests that round-trip through `load`.
    pub fn save(&self, repo_root: &Path) -> std::io::Result<()> {
        ensure_purview_dir(repo_root)?;
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        // Atomic: the MCP server may be reading this file concurrently.
        atomic_write(&Self::path_for(repo_root), &json)
    }

    /// Read the canonical mirror (`review-state.json`). This is what the MCP
    /// server reads — the live, active comparison.
    pub fn load(repo_root: &Path) -> std::io::Result<Self> {
        let raw = std::fs::read_to_string(Self::path_for(repo_root))?;
        serde_json::from_str(&raw)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Serialize this state to JSON (shared by both save paths). The GUI routes
    /// the bytes through `RepoSource::persist_state` so SSH mode writes to the
    /// remote; this helper just produces the bytes.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Load the per-comparison file for `key`, migrating the old single-file
    /// layout if needed. Resolution order:
    /// 1. If `.purview/state/<key>.json` exists, load it.
    /// 2. Else, if the OLD `.purview/review-state.json` exists AND it describes
    ///    this comparison (matched by `range`), import it into the new layout
    ///    and rename the old file to `review-state.json.bak` so it's preserved
    ///    but not re-imported. (Old files have empty anchors; those hunks fall
    ///    back to header matching at re-anchor time.)
    /// 3. Else, `None` — a brand-new comparison.
    ///
    /// Never errors on a missing/corrupt file: returns `None` so the caller
    /// starts a fresh review rather than crashing.
    pub fn load_for_comparison(repo_root: &Path, key: &ComparisonKey) -> Option<Self> {
        let per = Self::path_for_comparison(repo_root, key);
        if let Ok(raw) = std::fs::read_to_string(&per) {
            if let Ok(state) = serde_json::from_str::<Self>(&raw) {
                return Some(state);
            }
        }
        // Migration: import the old single-file layout if it matches.
        let old = Self::path_for(repo_root);
        if let Ok(raw) = std::fs::read_to_string(&old) {
            if let Ok(state) = serde_json::from_str::<Self>(&raw) {
                if state.range == key.range_label() {
                    // Persist into the new layout, then sideline the old file so
                    // it's not re-imported (and a later comparison switch can't
                    // mistake it for that comparison).
                    let _ = state.save_for_comparison(repo_root, key);
                    let _ = std::fs::rename(&old, old.with_extension("json.bak"));
                    return Some(state);
                }
            }
        }
        None
    }

    /// Write the per-comparison file directly to the local fs (atomic). The GUI
    /// normally persists through `RepoSource::persist_state` (so SSH writes to
    /// the remote); this direct path is used by the migration importer.
    pub fn save_for_comparison(
        &self,
        repo_root: &Path,
        key: &ComparisonKey,
    ) -> std::io::Result<()> {
        let dir = Self::state_dir(repo_root);
        std::fs::create_dir_all(&dir)?;
        // Keep .purview/ self-ignoring.
        let _ = ensure_purview_dir(repo_root);
        let json = self
            .to_json()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        atomic_write(&Self::path_for_comparison(repo_root, key), &json)
    }

    /// Re-anchor this saved state onto a freshly-recomputed diff (the SAME
    /// comparison — the caller must not have changed base/source). Mutates the
    /// live `files`' hunks in place, carrying verdicts + comments over, and
    /// returns the set of orphaned (no-longer-present) reviewed hunks.
    ///
    /// Per live hunk, in priority order:
    /// 1. **Anchor match** — a saved hunk in the same file with the same
    ///    non-empty `content_anchor`: carry status + comment SILENTLY
    ///    (`changed_since_review = false`).
    /// 2. **Header match** — a saved hunk in the same file with the same `@@`
    ///    header (used for old/migrated entries with no anchor, and as a
    ///    fallback): carry status + comment, flag `changed_since_review` IFF the
    ///    anchors differ (the region matched but the content changed).
    /// 3. **Near-location** — for a live hunk still unmatched, a saved reviewed
    ///    hunk in the same file whose `@@` new-side start is within a small
    ///    window of the live hunk's start (and isn't itself matched elsewhere):
    ///    carry the comment, flag `changed_since_review`.
    ///
    /// Any saved hunk with a verdict or comment that matched no live hunk is
    /// collected into the returned orphaned list.
    pub fn reanchor_onto(&self, files: &mut [crate::diff::ChangedFile]) -> Vec<OrphanedHunk> {
        use std::collections::HashSet;

        // Saved hunks that have been consumed (so each carries to at most one
        // live hunk, and the leftovers become orphans). Indexed (file_i, hunk_i).
        let mut used: HashSet<(usize, usize)> = HashSet::new();

        // For O(1)-ish lookup, index saved hunks per file by anchor + header.
        for live_file in files.iter_mut() {
            // The saved file with this path (if any).
            let Some((sf_i, saved_file)) = self
                .files
                .iter()
                .enumerate()
                .find(|(_, f)| f.path == live_file.path)
            else {
                continue;
            };

            // First pass: anchor matches (strongest), then header matches, per
            // live hunk. We compute each live hunk's anchor once.
            // Track which live hunks are still unmatched for the near-location pass.
            let mut live_matched = vec![false; live_file.hunks.len()];
            let live_anchors: Vec<String> =
                live_file.hunks.iter().map(|h| h.content_anchor()).collect();

            // Pass A — exact anchor.
            for (li, live) in live_file.hunks.iter_mut().enumerate() {
                let la = &live_anchors[li];
                if la.is_empty() {
                    continue;
                }
                if let Some((si, saved)) = saved_file.hunks.iter().enumerate().find(|(si, s)| {
                    !used.contains(&(sf_i, *si)) && !s.anchor.is_empty() && &s.anchor == la
                }) {
                    apply_saved(live, saved, false);
                    used.insert((sf_i, si));
                    live_matched[li] = true;
                }
            }

            // Pass B — header match (old/migrated entries, or content changed
            // under the same @@ header). changed_since_review iff anchors differ.
            for (li, live) in live_file.hunks.iter_mut().enumerate() {
                if live_matched[li] {
                    continue;
                }
                let la = &live_anchors[li];
                if let Some((si, saved)) = saved_file.hunks.iter().enumerate().find(|(si, s)| {
                    !used.contains(&(sf_i, *si)) && s.header == live.header
                }) {
                    // If the saved anchor is present and matches, it's clean;
                    // otherwise the region matched but content changed.
                    let clean = !saved.anchor.is_empty() && &saved.anchor == la;
                    apply_saved(live, saved, !clean);
                    used.insert((sf_i, si));
                    live_matched[li] = true;
                }
            }

            // Pass C — near-location: an unmatched reviewed saved hunk whose @@
            // new-side start is within WINDOW lines of an unmatched live hunk's
            // start. Carry the comment, flag changed_since_review.
            const WINDOW: u32 = 8;
            for (li, live) in live_file.hunks.iter_mut().enumerate() {
                if live_matched[li] {
                    continue;
                }
                let Some((_, live_new)) = crate::diff::parse_hunk_starts(&live.header) else {
                    continue;
                };
                if let Some((si, saved)) = saved_file.hunks.iter().enumerate().find(|(si, s)| {
                    if used.contains(&(sf_i, *si)) {
                        return false;
                    }
                    // Only reviewed/commented saved hunks are worth carrying.
                    if s.status == "unreviewed" && s.comment.as_deref().unwrap_or("").is_empty() {
                        return false;
                    }
                    match crate::diff::parse_hunk_starts(&s.header) {
                        Some((_, saved_new)) => {
                            (saved_new as i64 - live_new as i64).unsigned_abs() as u32 <= WINDOW
                        }
                        None => false,
                    }
                }) {
                    apply_saved(live, saved, true);
                    used.insert((sf_i, si));
                    live_matched[li] = true;
                }
            }
        }

        // Whatever saved hunk with a verdict/comment we never consumed is an
        // orphan (its anchor no longer appears in the diff).
        let mut orphans: Vec<OrphanedHunk> = Vec::new();
        for (fi, f) in self.files.iter().enumerate() {
            for (hi, h) in f.hunks.iter().enumerate() {
                if used.contains(&(fi, hi)) {
                    continue;
                }
                let has_comment = h.comment.as_deref().map(|c| !c.trim().is_empty()).unwrap_or(false);
                if h.status == "unreviewed" && !has_comment {
                    continue; // nothing worth preserving
                }
                orphans.push(OrphanedHunk {
                    file: f.path.clone(),
                    header: h.header.clone(),
                    status: h.status.clone(),
                    comment: h.comment.clone(),
                    anchor: h.anchor.clone(),
                });
            }
        }
        // Carry forward any pre-existing orphans from the saved state too, so a
        // hunk that orphaned in an earlier refresh isn't dropped on the next one
        // (dedup by anchor+header+file).
        for o in &self.orphaned {
            if !orphans.iter().any(|x| {
                x.file == o.file && x.header == o.header && x.anchor == o.anchor
            }) {
                orphans.push(o.clone());
            }
        }
        orphans
    }

    /// (reviewed, total) hunks.
    pub fn progress(&self) -> (usize, usize) {
        let total: usize = self.files.iter().map(|f| f.hunks.len()).sum();
        let reviewed: usize = self
            .files
            .iter()
            .flat_map(|f| &f.hunks)
            .filter(|h| h.status != "unreviewed")
            .count();
        (reviewed, total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("purview-rs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn sample() -> ReviewState {
        ReviewState {
            branch: "feature".into(),
            range: "main...HEAD".into(),
            files: vec![FileState {
                path: "src/a.rs".into(),
                hunks: vec![
                    HunkState { header: "@@ -1 +1 @@".into(), status: "approved".into(), comment: None, anchor: String::new(), changed_since_review: false },
                    HunkState { header: "@@ -9 +9 @@".into(), status: "rejected".into(), comment: Some("why".into()), anchor: String::new(), changed_since_review: false },
                    HunkState { header: "@@ -20 +20 @@".into(), status: "unreviewed".into(), comment: None, anchor: String::new(), changed_since_review: false },
                ],
            }],
            orphaned: Vec::new(),
        }
    }

    #[test]
    fn state_round_trips_through_disk() {
        let dir = tmp_dir("rt");
        let s = sample();
        s.save(&dir).unwrap();
        let back = ReviewState::load(&dir).unwrap();
        assert_eq!(back.branch, "feature");
        assert_eq!(back.files.len(), 1);
        assert_eq!(back.files[0].hunks.len(), 3);
        assert_eq!(back.files[0].hunks[1].comment.as_deref(), Some("why"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn progress_counts_reviewed() {
        assert_eq!(sample().progress(), (2, 3));
    }

    #[test]
    fn save_creates_self_ignoring_purview_dir() {
        let dir = tmp_dir("gi");
        sample().save(&dir).unwrap();
        let gi = dir.join(".purview").join(".gitignore");
        assert!(gi.exists(), ".purview/.gitignore should be created");
        assert_eq!(std::fs::read_to_string(&gi).unwrap(), "*\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_is_atomic_no_tmp_left_behind() {
        let dir = tmp_dir("atomic");
        sample().save(&dir).unwrap();
        assert!(ReviewState::path_for(&dir).exists());
        assert!(!ReviewState::path_for(&dir).with_extension("tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replies_are_per_file_and_survive_many_appends() {
        let dir = tmp_dir("replies");
        for i in 0..50 {
            Replies::append(
                &dir,
                Reply {
                    file: "src/a.rs".into(),
                    hunk_header: "@@ -9 +9 @@".into(),
                    text: format!("reply {i}"),
                    anchor: String::new(),
                },
            )
            .unwrap();
        }
        let all = Replies::load(&dir);
        assert_eq!(all.replies.len(), 50, "no replies lost");
        let thread = all.for_hunk("src/a.rs", "@@ -9 +9 @@");
        assert_eq!(thread.len(), 50);
        // Different hunk → empty.
        assert!(all.for_hunk("src/a.rs", "@@ -1 +1 @@").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_is_empty_not_error() {
        let dir = tmp_dir("missing");
        assert_eq!(Replies::load(&dir).replies.len(), 0);
        assert!(ReviewState::load(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- Comparison keying ------------------------------------------------

    #[test]
    fn comparison_keys_sanitize_and_separate() {
        assert_eq!(ComparisonKey::WorkingTree.file_stem(), "worktree-vs-HEAD");
        assert_eq!(
            ComparisonKey::BranchRange { base: "main".into() }.file_stem(),
            "main__HEAD"
        );
        // Slashes (e.g. origin/feature) are sanitized to underscores.
        assert_eq!(
            ComparisonKey::BranchRange { base: "origin/feature".into() }.file_stem(),
            "origin_feature__HEAD"
        );
        // Different bases → different files (no collision).
        assert_ne!(
            ComparisonKey::BranchRange { base: "main".into() }.file_stem(),
            ComparisonKey::BranchRange { base: "develop".into() }.file_stem(),
        );
    }

    #[test]
    fn per_comparison_files_do_not_collide() {
        let dir = tmp_dir("keying");
        let main_key = ComparisonKey::BranchRange { base: "main".into() };
        let dev_key = ComparisonKey::BranchRange { base: "develop".into() };

        let mut main_state = sample();
        main_state.range = "main...HEAD".into();
        main_state.branch = "from-main".into();
        main_state.save_for_comparison(&dir, &main_key).unwrap();

        let mut dev_state = sample();
        dev_state.range = "develop...HEAD".into();
        dev_state.branch = "from-develop".into();
        dev_state.save_for_comparison(&dir, &dev_key).unwrap();

        // Distinct files on disk.
        let main_path = ReviewState::path_for_comparison(&dir, &main_key);
        let dev_path = ReviewState::path_for_comparison(&dir, &dev_key);
        assert_ne!(main_path, dev_path);
        assert!(main_path.exists() && dev_path.exists());

        // Each loads back independently — no clobber.
        let back_main = ReviewState::load_for_comparison(&dir, &main_key).unwrap();
        let back_dev = ReviewState::load_for_comparison(&dir, &dev_key).unwrap();
        assert_eq!(back_main.branch, "from-main");
        assert_eq!(back_dev.branch, "from-develop");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- Migration --------------------------------------------------------

    #[test]
    fn old_single_file_is_migrated_into_new_layout() {
        let dir = tmp_dir("migrate");
        // Write an OLD-format file at the canonical path. (sample()'s range is
        // "main...HEAD", so it migrates under the main__HEAD comparison.)
        sample().save(&dir).unwrap();
        let old_path = ReviewState::path_for(&dir);
        assert!(old_path.exists());

        let key = ComparisonKey::BranchRange { base: "main".into() };
        // The new per-comparison file doesn't exist yet.
        assert!(!ReviewState::path_for_comparison(&dir, &key).exists());

        // Loading the comparison imports the old file without loss.
        let migrated = ReviewState::load_for_comparison(&dir, &key)
            .expect("old file should migrate");
        assert_eq!(migrated.files.len(), 1);
        assert_eq!(migrated.files[0].hunks.len(), 3);
        assert_eq!(migrated.files[0].hunks[1].comment.as_deref(), Some("why"));
        assert_eq!(migrated.files[0].hunks[0].status, "approved");

        // The new file now exists; the old one was renamed to .bak (preserved).
        assert!(ReviewState::path_for_comparison(&dir, &key).exists());
        assert!(!old_path.exists(), "old file renamed");
        assert!(old_path.with_extension("json.bak").exists(), "old file preserved as .bak");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migration_skips_when_range_does_not_match() {
        let dir = tmp_dir("migrate-skip");
        // Old file describes main...HEAD.
        sample().save(&dir).unwrap();
        // Loading a DIFFERENT comparison must not import it.
        let dev_key = ComparisonKey::BranchRange { base: "develop".into() };
        assert!(ReviewState::load_for_comparison(&dir, &dev_key).is_none());
        // The old file is left untouched (not renamed).
        assert!(ReviewState::path_for(&dir).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- Re-anchoring -----------------------------------------------------

    /// One-file diff from a unified patch (for re-anchor tests).
    fn changed_files(patch: &str) -> Vec<crate::diff::ChangedFile> {
        crate::diff::parse_unified_patch(patch)
    }

    /// Build a saved ReviewState carrying a verdict+comment on the (only) hunk
    /// of `patch`, computing the real anchor so re-anchoring can match it.
    fn saved_from(patch: &str, status: &str, comment: &str) -> ReviewState {
        let files = changed_files(patch);
        let f = &files[0];
        let h = &f.hunks[0];
        ReviewState {
            branch: "b".into(),
            range: "main...HEAD".into(),
            files: vec![FileState {
                path: f.path.clone(),
                hunks: vec![HunkState {
                    header: h.header.clone(),
                    status: status.into(),
                    comment: Some(comment.into()),
                    anchor: h.content_anchor(),
                    changed_since_review: false,
                }],
            }],
            orphaned: Vec::new(),
        }
    }

    const PATCH_A: &str =
        "diff --git a/f.txt b/f.txt\n--- a/f.txt\n+++ b/f.txt\n@@ -10,4 +10,4 @@\n ctx\n-old line\n+new line\n ctx2\n";

    /// Anchor match across a shift → verdict carries SILENTLY (not stale).
    #[test]
    fn reanchor_carries_verdict_on_shifted_unchanged_hunk() {
        let saved = saved_from(PATCH_A, "approved", "looks good");
        // Same change, shifted to a different file offset (different @@).
        let shifted =
            "diff --git a/f.txt b/f.txt\n--- a/f.txt\n+++ b/f.txt\n@@ -300,4 +305,4 @@\n ctx\n-old line\n+new line\n ctx2\n";
        let mut live = changed_files(shifted);
        let orphans = saved.reanchor_onto(&mut live);
        assert_eq!(live[0].hunks[0].status, crate::diff::ReviewStatus::Approved);
        assert_eq!(live[0].hunks[0].comment, "looks good");
        assert!(!live[0].hunks[0].changed_since_review, "clean anchor match is silent");
        assert!(orphans.is_empty());
    }

    /// Content changed under the same @@ header → comment carried + flagged.
    #[test]
    fn reanchor_flags_changed_content_as_changed_since_review() {
        let saved = saved_from(PATCH_A, "rejected", "fix this");
        // Same header (@@ -10,4 +10,4 @@) but the replacement line differs.
        let edited =
            "diff --git a/f.txt b/f.txt\n--- a/f.txt\n+++ b/f.txt\n@@ -10,4 +10,4 @@\n ctx\n-old line\n+TOTALLY DIFFERENT\n ctx2\n";
        let mut live = changed_files(edited);
        let orphans = saved.reanchor_onto(&mut live);
        assert_eq!(live[0].hunks[0].comment, "fix this", "comment carried over");
        assert!(
            live[0].hunks[0].changed_since_review,
            "content changed under the same header → flagged"
        );
        assert!(orphans.is_empty());
    }

    /// Reviewed hunk gone from the diff → lands in the orphaned list.
    #[test]
    fn reanchor_orphans_a_vanished_hunk() {
        let saved = saved_from(PATCH_A, "rejected", "must change");
        // A completely different file/hunk in the new diff.
        let other =
            "diff --git a/g.txt b/g.txt\n--- a/g.txt\n+++ b/g.txt\n@@ -1,3 +1,3 @@\n x\n-y\n+Y\n";
        let mut live = changed_files(other);
        let orphans = saved.reanchor_onto(&mut live);
        // The live (unrelated) hunk got nothing.
        assert_eq!(live[0].hunks[0].status, crate::diff::ReviewStatus::Unreviewed);
        assert_eq!(orphans.len(), 1, "the vanished reviewed hunk is orphaned");
        assert_eq!(orphans[0].file, "f.txt");
        assert_eq!(orphans[0].comment.as_deref(), Some("must change"));
    }

    /// Migrated (anchor-less) saved hunks re-anchor by header, flagged stale
    /// only if the content differs.
    #[test]
    fn reanchor_matches_old_anchorless_entry_by_header() {
        // Saved entry with NO anchor (as a migrated old file would have).
        let saved = ReviewState {
            branch: "b".into(),
            range: "main...HEAD".into(),
            files: vec![FileState {
                path: "f.txt".into(),
                hunks: vec![HunkState {
                    header: "@@ -10,4 +10,4 @@".into(),
                    status: "approved".into(),
                    comment: Some("ok".into()),
                    anchor: String::new(),
                    changed_since_review: false,
                }],
            }],
            orphaned: Vec::new(),
        };
        let mut live = changed_files(PATCH_A);
        let orphans = saved.reanchor_onto(&mut live);
        assert_eq!(live[0].hunks[0].status, crate::diff::ReviewStatus::Approved);
        assert_eq!(live[0].hunks[0].comment, "ok");
        // No saved anchor to compare → conservatively flagged changed.
        assert!(live[0].hunks[0].changed_since_review);
        assert!(orphans.is_empty());
    }
}
