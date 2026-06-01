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
pub trait RepoSource {
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

    /// A short human label for the repo (shown in the title bar).
    fn label(&self) -> String;

    /// The local directory where review state (`.purview/`) is persisted. For
    /// a local repo this is the workdir; for SSH it's a local mirror dir so
    /// the MCP server (which runs locally) can still read it.
    fn state_root(&self) -> &Path;
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
