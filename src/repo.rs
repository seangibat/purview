//! Repo-access abstraction.
//!
//! purview reads four things from a repo: the changed-file set + diff hunks,
//! a file's base (committed) contents, a file's current (working-tree)
//! contents, and the file listing for navigation. Historically all of that
//! went straight to the local filesystem + git2. To support reviewing a repo
//! on a remote machine over SSH (à la VS Code Remote-SSH), those operations
//! are funneled through the [`RepoSource`] trait.
//!
//! Two implementations:
//! - [`LocalRepo`] — the original git2 + `std::fs` behavior, unchanged. Default.
//! - [`SshRepo`] — shells out to the system `ssh` binary to run git and read
//!   files on the remote. Connection multiplexing keeps each call cheap.
//!
//! The UI talks only to a `Box<dyn RepoSource>`, so the backend swap is
//! invisible above this layer.

use std::path::{Path, PathBuf};

use crate::diff::{self, ChangedFile, DiffSource};
use crate::gotodef::Candidate;

mod ssh;
pub use ssh::{SshRepo, SshTarget};

/// Everything the UI needs from a repo, abstracted over local-vs-remote.
///
/// Methods return `String` errors (already human-readable) rather than a
/// concrete error type, because the two backends fail in very different ways
/// (git2::Error vs. a non-zero ssh exit) and the UI only ever shows the text.
pub trait RepoSource: Send + Sync {
    /// Current branch shorthand + the changed files (grouped into hunks) for
    /// `source`. `base` is only used for [`DiffSource::BranchRange`]. Mirrors
    /// [`diff::compute`].
    fn compute_diff(
        &self,
        source: DiffSource,
        base: &str,
    ) -> Result<(String, Vec<ChangedFile>), String>;

    /// Like [`compute_diff`] but for a single file with full-file context —
    /// the "Full extent" view. `context_lines` is typically `u32::MAX`.
    /// Mirrors [`diff::compute_with`] with a pathspec.
    fn compute_file_diff(
        &self,
        source: DiffSource,
        base: &str,
        context_lines: u32,
        path: &str,
    ) -> Result<(String, Vec<ChangedFile>), String>;

    /// Current (working-tree) contents of `rel`. This is the "new" side the
    /// full-file view shows.
    fn read_file(&self, rel: &str) -> Result<String, String>;

    /// List immediate children of directory `rel` ("" = repo root), for the
    /// lazy file tree. Returns (name, rel_path, is_dir) sorted dirs-first.
    fn list_dir(&self, rel: &str) -> Result<Vec<DirEntry>, String>;

    /// Every file path in the repo (gitignore-respecting), capped at `cap`.
    /// Powers the Ctrl+P fuzzy finder. Returns (paths, truncated).
    fn list_all_files(&self, cap: usize) -> (Vec<String>, bool);

    /// Pick a default base branch for branch-range mode: first of
    /// main/master/develop/trunk that resolves, else "main".
    fn guess_default_base(&self) -> String;

    /// Write `new_text` over 0-based line `line0` of `rel`, preserving the
    /// rest of the file. Inline-edit write-back.
    fn write_line(&self, rel: &str, line0: usize, new_text: &str) -> Result<(), String>;

    /// `git grep -n -w <symbol>` for go-to-definition candidates, run WHERE the
    /// repo lives (locally for [`LocalRepo`], on the remote for [`SshRepo`]).
    /// Returns the same [`Candidate`] set `gotodef` then feeds to the LOCAL
    /// Claude-CLI precision step. Identifier-ish symbols only (the caller and
    /// each impl guard against shell/regex surprises).
    fn grep_symbol(&self, symbol: &str) -> Result<Vec<Candidate>, String>;

    /// Pick the defining candidate among `cands` for `symbol` by running the
    /// Claude CLI precision step WHERE the repo (and `claude`) live: locally for
    /// [`LocalRepo`], on the remote over SSH for [`SshRepo`]. `usage` is the
    /// optional call-site hint. Blocking — call from a background thread.
    ///
    /// The default impl runs `claude` locally (correct for [`LocalRepo`]); the
    /// SSH backend overrides it to run the SAME command on the remote. The
    /// prompt is built by the shared `gotodef::build_prompt` so both backends
    /// send byte-identical input.
    fn resolve_definition(
        &self,
        symbol: &str,
        usage: Option<&str>,
        cands: &[Candidate],
    ) -> Result<Option<Candidate>, String> {
        // Short-circuits shared by every backend (no model call needed).
        if cands.is_empty() {
            return Ok(None);
        }
        if cands.len() == 1 {
            return Ok(Some(cands[0].clone()));
        }
        let prompt = crate::gotodef::build_prompt(symbol, usage, cands);
        let reply = crate::gotodef::run_claude_local(&prompt)?;
        Ok(crate::gotodef::pick_candidate(&reply, cands))
    }

    /// Whether inline editing (write-back) is supported. The UI hides/disables
    /// the edit affordance and shows a note when false.
    fn supports_editing(&self) -> bool {
        true
    }

    /// Whether F12 go-to-definition (git grep + Claude) is supported. The UI
    /// disables the overlay with a note when false.
    fn supports_goto(&self) -> bool {
        true
    }

    /// Whether reads from this backend are slow enough to warrant loading file
    /// content off the UI thread. The local fs is fast → load synchronously
    /// (instant, no spinner). SSH does blocking remote round-trips → load async.
    /// Drives Task A's sync-vs-async file-open path.
    fn is_remote(&self) -> bool {
        false
    }

    /// A short human label for the repo (shown in the title bar).
    fn label(&self) -> String;

    /// The local directory where review state (`.purview/`) is persisted. For
    /// a local repo this is the workdir; for SSH it's a local mirror dir so
    /// the MCP server (which runs locally) can still read it.
    fn state_root(&self) -> &Path;

    /// Persist a review-state file (`relname`, e.g. `review-state.json` or
    /// `review-report.md`) into the repo's `.purview/` dir, WHERE the repo
    /// lives. For [`LocalRepo`] that's `<workdir>/.purview/<relname>` on the
    /// local fs; for [`SshRepo`] that's `<remote-repo>/.purview/<relname>` on
    /// the remote (so the remote `purview-mcp` server reads it natively),
    /// *plus* a copy in the local mirror dir for the local MCP fallback.
    ///
    /// The `.purview/` dir is created if missing and made self-ignoring (a
    /// `.gitignore` of `*`) so review state never pollutes git status.
    ///
    /// Blocking — for SSH this is one remote write. Called on infrequent user
    /// actions (approve/reject/comment), so a quick round trip is acceptable.
    fn persist_state(&self, relname: &str, contents: &str) -> Result<(), String>;
}

/// One entry in a directory listing.
#[derive(Clone, Debug)]
pub struct DirEntry {
    pub name: String,
    /// Path relative to repo root, forward-slashed.
    pub rel: String,
    pub is_dir: bool,
}

/// The original local behavior, refactored behind the trait. Default path —
/// must stay byte-for-byte equivalent to the pre-refactor code.
pub struct LocalRepo {
    /// Path the user passed (for `diff::discover`).
    repo_path: PathBuf,
    /// Repo workdir root (where `.purview/` lives and tree paths are relative).
    root: PathBuf,
}

impl LocalRepo {
    pub fn new(repo_path: PathBuf) -> Self {
        // Mirror main.rs's old tree-root discovery.
        let root = git2::Repository::discover(&repo_path)
            .ok()
            .and_then(|r| r.workdir().map(|w| w.to_path_buf()))
            .unwrap_or_else(|| repo_path.clone());
        LocalRepo { repo_path, root }
    }
}

impl RepoSource for LocalRepo {
    fn compute_diff(
        &self,
        source: DiffSource,
        base: &str,
    ) -> Result<(String, Vec<ChangedFile>), String> {
        diff::compute(&self.repo_path, source, base).map_err(|e| e.to_string())
    }

    fn compute_file_diff(
        &self,
        source: DiffSource,
        base: &str,
        context_lines: u32,
        path: &str,
    ) -> Result<(String, Vec<ChangedFile>), String> {
        diff::compute_with(&self.repo_path, source, base, context_lines, Some(path))
            .map_err(|e| e.to_string())
    }

    fn read_file(&self, rel: &str) -> Result<String, String> {
        std::fs::read_to_string(self.root.join(rel)).map_err(|e| e.to_string())
    }

    fn list_dir(&self, rel: &str) -> Result<Vec<DirEntry>, String> {
        Ok(crate::tree::read_children(&self.root, rel)
            .into_iter()
            .map(|n| DirEntry {
                name: n.name,
                rel: n.rel,
                is_dir: n.is_dir,
            })
            .collect())
    }

    fn list_all_files(&self, cap: usize) -> (Vec<String>, bool) {
        crate::tree::collect_files(&self.root, cap)
    }

    fn guess_default_base(&self) -> String {
        if let Ok(repo) = git2::Repository::discover(&self.repo_path) {
            // Prefer the current branch's upstream tracking ref (how Sean does
            // PR stacking: the base is the parent branch's upstream, e.g.
            // `origin/feature-parent`). Equivalent to
            // `git rev-parse --abbrev-ref --symbolic-full-name @{upstream}`.
            if let Some(up) = local_upstream(&repo) {
                return up;
            }
            for cand in ["main", "master", "develop", "trunk"] {
                if repo.revparse_single(cand).is_ok() {
                    return cand.to_string();
                }
            }
        }
        "main".to_string()
    }

    fn write_line(&self, rel: &str, line0: usize, new_text: &str) -> Result<(), String> {
        let path = self.root.join(rel);
        let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let out = crate::diff::replace_nth_line(&content, line0, new_text)
            .ok_or_else(|| "line out of range".to_string())?;
        // Atomic: temp + rename, so a concurrent reader never sees half.
        let tmp = path.with_extension("purview-tmp");
        std::fs::write(&tmp, out).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
    }

    fn grep_symbol(&self, symbol: &str) -> Result<Vec<Candidate>, String> {
        // Unchanged local behavior: git grep against the workdir.
        Ok(crate::gotodef::grep_candidates(&self.root, symbol))
    }

    fn label(&self) -> String {
        self.repo_path.to_string_lossy().into_owned()
    }

    fn state_root(&self) -> &Path {
        &self.root
    }

    fn persist_state(&self, relname: &str, contents: &str) -> Result<(), String> {
        // Create (+ self-ignore) the workdir's .purview/ and write atomically,
        // exactly as the GUI did before this was routed through the trait.
        let dir = crate::review_state::ensure_purview_dir(&self.root)
            .map_err(|e| e.to_string())?;
        let path = dir.join(relname);
        let tmp = path.with_extension("purview-tmp");
        std::fs::write(&tmp, contents).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
    }
}

/// The current branch's upstream tracking ref as a short name (e.g.
/// `origin/feature-parent`), or `None` if HEAD is detached or has no upstream
/// configured. Mirrors `git rev-parse --abbrev-ref --symbolic-full-name
/// @{upstream}`: git2 gives us `refs/remotes/origin/feature-parent`, which we
/// shorten the same way git does.
fn local_upstream(repo: &git2::Repository) -> Option<String> {
    let head = repo.head().ok()?;
    let shorthand = head.shorthand()?;
    let branch = repo.find_branch(shorthand, git2::BranchType::Local).ok()?;
    let upstream = branch.upstream().ok()?;
    let name = upstream.get().shorthand()?;
    Some(name.to_string())
}

/// Build a [`RepoSource`] from a CLI argument. `ssh://...` yields an
/// [`SshRepo`]; anything else is a local path → [`LocalRepo`].
pub fn open(arg: &str) -> Result<Box<dyn RepoSource>, String> {
    if let Some(target) = SshTarget::parse(arg) {
        Ok(Box::new(SshRepo::connect(target)?))
    } else {
        Ok(Box::new(LocalRepo::new(PathBuf::from(arg))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A throwaway git repo + a `git` runner closure bound to it. Starts on a
    /// branch `main` with one commit, no upstream configured.
    fn repo_dir() -> (PathBuf, impl Fn(&[&str])) {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "purview-repo-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.clone();
        let git = move |args: &[&str]| {
            let ok = Command::new("git")
                .args(args)
                .current_dir(&d)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?} failed");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        git(&["checkout", "-q", "-b", "main"]);
        std::fs::write(dir.join("a.txt"), "x\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);
        (dir, git)
    }

    /// `LocalRepo::persist_state` writes the file into `<workdir>/.purview/`,
    /// the content round-trips, and the dir self-ignores via `.gitignore`.
    #[test]
    fn local_persist_state_writes_into_purview_and_self_ignores() {
        let (dir, _git) = repo_dir();
        let repo = LocalRepo::new(dir.clone());
        let body = "{\"hello\":\"world\"}\n";
        repo.persist_state("review-state.json", body).unwrap();

        let written = dir.join(".purview").join("review-state.json");
        assert_eq!(
            std::fs::read_to_string(&written).unwrap(),
            body,
            "persisted content should round-trip from disk"
        );
        let gi = dir.join(".purview").join(".gitignore");
        assert!(gi.exists(), ".purview/.gitignore should exist");
        assert_eq!(std::fs::read_to_string(&gi).unwrap(), "*\n");
        // No temp file left behind by the atomic write.
        assert!(!written.with_extension("purview-tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With no upstream tracking ref configured, the default base falls back to
    /// the first of main/master/develop/trunk that resolves (here: `main`).
    #[test]
    fn guess_default_base_falls_back_without_upstream() {
        let (dir, _git) = repo_dir();
        let repo = LocalRepo::new(dir.clone());
        assert_eq!(repo.guess_default_base(), "main");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// When the current branch has an upstream tracking ref, the default base is
    /// that upstream (PR-stacking: the parent branch's upstream), not the
    /// main/master guess. We model the upstream with a local-self remote
    /// (`branch.<cur>.remote = .`) so no network/bare repo is needed — git2's
    /// `branch.upstream()` reads the same config `@{upstream}` resolves through.
    #[test]
    fn guess_default_base_prefers_upstream_when_set() {
        let (dir, git) = repo_dir();
        // A parent branch the current branch will track.
        git(&["branch", "feature-parent"]);
        // Make `main` track `feature-parent` via the local repo as its remote.
        git(&["config", "branch.main.remote", "."]);
        git(&["config", "branch.main.merge", "refs/heads/feature-parent"]);

        let repo = LocalRepo::new(dir.clone());
        let base = repo.guess_default_base();
        assert_eq!(
            base, "feature-parent",
            "upstream tracking ref should be the default base, got {base:?}"
        );
        // And the fallback list would NOT have chosen this branch.
        assert_ne!(base, "main", "must not fall back to main when an upstream is set");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
