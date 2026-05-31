//! Diff computation and the review data model — pure logic (no egui), so it
//! can be unit-tested and benchmarked without a GUI.

use std::path::Path;

use git2::{Diff, DiffFormat, DiffOptions, Repository};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LineKind {
    Add,
    Del,
    Ctx,
}

/// A diff line: its kind plus the raw text.
#[derive(Clone, Debug)]
pub struct DiffLineRow {
    pub kind: LineKind,
    pub text: String,
}

/// Per-hunk review decision. The unit of review is the hunk.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReviewStatus {
    Unreviewed,
    Approved,
    Rejected,
}

impl ReviewStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ReviewStatus::Unreviewed => "unreviewed",
            ReviewStatus::Approved => "approved",
            ReviewStatus::Rejected => "rejected",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Hunk {
    pub header: String,
    pub rows: Vec<DiffLineRow>,
    pub status: ReviewStatus,
    pub comment: String,
}

impl Hunk {
    pub fn new(header: String) -> Self {
        Hunk {
            header,
            rows: Vec::new(),
            status: ReviewStatus::Unreviewed,
            comment: String::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ChangedFile {
    pub path: String,
    pub hunks: Vec<Hunk>,
}

impl ChangedFile {
    /// (reviewed, total) hunk counts for the progress indicator.
    pub fn progress(&self) -> (usize, usize) {
        let total = self.hunks.len();
        let reviewed = self
            .hunks
            .iter()
            .filter(|h| h.status != ReviewStatus::Unreviewed)
            .count();
        (reviewed, total)
    }
}

/// What we diff against.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DiffSource {
    /// Working tree (incl. index + untracked) vs HEAD — local uncommitted work.
    WorkingTree,
    /// `base...HEAD` three-dot: merge-base(base, HEAD) tree vs HEAD tree.
    BranchRange,
}

/// Compute the diff for `repo_path` under `source`. Returns the current
/// branch shorthand and the changed files grouped into hunks. `base` is only
/// used for `DiffSource::BranchRange`.
pub fn compute(
    repo_path: &Path,
    source: DiffSource,
    base: &str,
) -> Result<(String, Vec<ChangedFile>), git2::Error> {
    compute_with(repo_path, source, base, 3, None)
}

/// Like [`compute`] but with a chosen number of `context_lines` and an
/// optional single-file `pathspec`. A very large `context_lines` (e.g.
/// `u32::MAX`) yields the whole file as context — the "Full extent" view.
pub fn compute_with(
    repo_path: &Path,
    source: DiffSource,
    base: &str,
    context_lines: u32,
    pathspec: Option<&str>,
) -> Result<(String, Vec<ChangedFile>), git2::Error> {
    let repo = Repository::discover(repo_path)?;
    let branch = repo
        .head()
        .ok()
        .and_then(|h| h.shorthand().map(String::from))
        .unwrap_or_else(|| "(detached)".into());
    let head_tree = repo.head().ok().and_then(|h| h.peel_to_tree().ok());

    let mut opts = DiffOptions::new();
    opts.context_lines(context_lines)
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        // Without this, untracked files appear in the delta list but emit no
        // patch content — so they'd never produce reviewable hunks.
        .show_untracked_content(true);
    if let Some(ps) = pathspec {
        opts.pathspec(ps);
    }

    let diff: Diff = match source {
        DiffSource::WorkingTree => {
            repo.diff_tree_to_workdir_with_index(head_tree.as_ref(), Some(&mut opts))?
        }
        DiffSource::BranchRange => {
            let base_obj = repo.revparse_single(base)?;
            let base_commit = base_obj.peel_to_commit()?;
            let head_commit = repo.head()?.peel_to_commit()?;
            let mb = repo.merge_base(base_commit.id(), head_commit.id())?;
            let mb_tree = repo.find_commit(mb)?.tree()?;
            let head_t = head_commit.tree()?;
            repo.diff_tree_to_tree(Some(&mb_tree), Some(&head_t), Some(&mut opts))?
        }
    };

    let mut files: Vec<ChangedFile> = Vec::new();
    diff.print(DiffFormat::Patch, |delta, _hunk, line| {
        let path = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "<unknown>".into());

        if files.last().map(|f| f.path != path).unwrap_or(true) {
            files.push(ChangedFile {
                path: path.clone(),
                hunks: Vec::new(),
            });
        }
        let file = files.last_mut().unwrap();
        match line.origin() {
            'F' => {}
            'B' => {
                if file.hunks.is_empty() {
                    file.hunks.push(Hunk::new("(binary file)".to_string()));
                }
            }
            'H' => {
                let content = String::from_utf8_lossy(line.content())
                    .trim_end_matches('\n')
                    .to_string();
                file.hunks.push(Hunk::new(content));
            }
            origin => {
                let content = String::from_utf8_lossy(line.content())
                    .trim_end_matches('\n')
                    .to_string();
                let kind = match origin {
                    '+' => LineKind::Add,
                    '-' => LineKind::Del,
                    _ => LineKind::Ctx,
                };
                if file.hunks.is_empty() {
                    file.hunks.push(Hunk::new(String::new()));
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

    Ok((branch, files))
}

/// Parse a unified-diff patch (the text `git diff` prints) into the same
/// [`ChangedFile`] / [`Hunk`] model `compute_with` produces from git2. This
/// is the shared parser for the SSH backend, which gets raw patch text from
/// the remote `git diff` rather than a git2 `Diff` object.
///
/// It keys file boundaries off `diff --git a/<p> b/<p>` lines, hunk
/// boundaries off `@@ ... @@` lines, and classifies content lines by their
/// leading `+`/`-`/space — the same way git2's per-line callback does.
pub fn parse_unified_patch(patch: &str) -> Vec<ChangedFile> {
    let mut files: Vec<ChangedFile> = Vec::new();
    let mut in_hunks = false; // true once we've seen the first @@ for a file

    for line in patch.split('\n') {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            // "a/<path> b/<path>" — take the b-side path (new file), falling
            // back to the a-side. Both are prefixed a/ and b/.
            let path = parse_diff_git_path(rest);
            files.push(ChangedFile {
                path,
                hunks: Vec::new(),
            });
            in_hunks = false;
            continue;
        }
        if line.starts_with("@@") {
            // Hunk header: "@@ -l,s +l,s @@ optional section heading".
            if let Some(file) = files.last_mut() {
                file.hunks.push(Hunk::new(line.to_string()));
                in_hunks = true;
            }
            continue;
        }
        if !in_hunks {
            // Skip file-metadata lines (index, ---, +++, mode, etc.).
            continue;
        }
        let Some(file) = files.last_mut() else { continue };
        let (kind, text) = match line.as_bytes().first() {
            Some(b'+') => (LineKind::Add, &line[1..]),
            Some(b'-') => (LineKind::Del, &line[1..]),
            Some(b' ') => (LineKind::Ctx, &line[1..]),
            // "\ No newline at end of file" and blank trailing line: ignore.
            _ => continue,
        };
        if let Some(h) = file.hunks.last_mut() {
            h.rows.push(DiffLineRow {
                kind,
                text: text.to_string(),
            });
        }
    }
    files
}

/// Extract the file path from a `diff --git a/<p> b/<p>` line's tail. Prefers
/// the b-side; both sides equal for a normal edit. Paths with spaces work
/// because the a/ and b/ prefixes bracket each side.
fn parse_diff_git_path(rest: &str) -> String {
    // Find " b/" which separates the a-side from the b-side.
    if let Some(idx) = rest.find(" b/") {
        return rest[idx + 3..].to_string();
    }
    // Fallback: strip a leading "a/" off the whole thing.
    rest.strip_prefix("a/").unwrap_or(rest).to_string()
}

/// Build a [`ChangedFile`] for an untracked file: one hunk, every line an
/// addition. Mirrors how the local backend (git2 with show_untracked_content)
/// surfaces a brand-new file.
pub fn untracked_as_changed_file(path: &str, content: &str) -> ChangedFile {
    let mut hunk = Hunk::new(format!("@@ -0,0 +1,{} @@", content.lines().count()));
    for line in content.lines() {
        hunk.rows.push(DiffLineRow {
            kind: LineKind::Add,
            text: line.to_string(),
        });
    }
    ChangedFile {
        path: path.to_string(),
        hunks: vec![hunk],
    }
}

/// Replace 0-based line `n` of `content` with `new`, preserving the file's
/// trailing-newline state. None if `n` is out of range. Shared by the local
/// inline-edit write-back path.
pub fn replace_nth_line(content: &str, n: usize, new: &str) -> Option<String> {
    let had_trailing_nl = content.ends_with('\n');
    let mut lines: Vec<&str> = content.lines().collect();
    if n >= lines.len() {
        return None;
    }
    lines[n] = new;
    let mut out = lines.join("\n");
    if had_trailing_nl {
        out.push('\n');
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Build a throwaway git repo in a UNIQUE temp dir (tests run in
    /// parallel, so the dir must not be shared); run a closure with its path.
    fn with_repo(f: impl FnOnce(&Path)) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir()
            .join(format!("purview-difftest-{}-{n}-{nanos}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .expect("git");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        git(&["checkout", "-q", "-b", "main"]);
        f(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn working_tree_diff_groups_into_hunks() {
        with_repo(|dir| {
            std::fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
            Command::new("git").args(["add", "."]).current_dir(dir).output().unwrap();
            Command::new("git").args(["commit", "-qm", "init"]).current_dir(dir).output().unwrap();
            // Modify.
            std::fs::write(dir.join("a.txt"), "one\nTWO\nthree\nfour\n").unwrap();

            let (_branch, files) = compute(dir, DiffSource::WorkingTree, "main").unwrap();
            assert_eq!(files.len(), 1, "one changed file");
            assert_eq!(files[0].path, "a.txt");
            assert!(!files[0].hunks.is_empty(), "has at least one hunk");
            let has_add = files[0]
                .hunks
                .iter()
                .flat_map(|h| &h.rows)
                .any(|r| r.kind == LineKind::Add);
            assert!(has_add, "should have an added line");
        });
    }

    #[test]
    fn untracked_file_appears() {
        with_repo(|dir| {
            std::fs::write(dir.join("seed.txt"), "x\n").unwrap();
            Command::new("git").args(["add", "."]).current_dir(dir).output().unwrap();
            Command::new("git").args(["commit", "-qm", "init"]).current_dir(dir).output().unwrap();
            std::fs::write(dir.join("new.txt"), "brand\nnew\n").unwrap();

            let (_b, files) = compute(dir, DiffSource::WorkingTree, "main").unwrap();
            assert!(files.iter().any(|f| f.path == "new.txt"), "untracked file shows");
        });
    }

    #[test]
    fn branch_range_only_shows_branch_commits() {
        with_repo(|dir| {
            std::fs::write(dir.join("base.txt"), "base\n").unwrap();
            Command::new("git").args(["add", "."]).current_dir(dir).output().unwrap();
            Command::new("git").args(["commit", "-qm", "base"]).current_dir(dir).output().unwrap();
            Command::new("git").args(["checkout", "-q", "-b", "feature"]).current_dir(dir).output().unwrap();
            std::fs::write(dir.join("feat.txt"), "feature\n").unwrap();
            Command::new("git").args(["add", "."]).current_dir(dir).output().unwrap();
            Command::new("git").args(["commit", "-qm", "feat"]).current_dir(dir).output().unwrap();

            let (branch, files) = compute(dir, DiffSource::BranchRange, "main").unwrap();
            assert_eq!(branch, "feature");
            assert_eq!(files.len(), 1, "only the feature commit's file");
            assert_eq!(files[0].path, "feat.txt");
        });
    }

    #[test]
    fn progress_counts() {
        let mut f = ChangedFile {
            path: "x".into(),
            hunks: vec![Hunk::new("h1".into()), Hunk::new("h2".into())],
        };
        assert_eq!(f.progress(), (0, 2));
        f.hunks[0].status = ReviewStatus::Approved;
        assert_eq!(f.progress(), (1, 2));
    }
}
