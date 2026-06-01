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

/// A diff line: its kind, raw text, and (where applicable) its source line
/// numbers on the old and new sides. Context lines have both; a deletion has
/// only an old number; an addition has only a new number.
#[derive(Clone, Debug)]
pub struct DiffLineRow {
    pub kind: LineKind,
    pub text: String,
    pub old_lineno: Option<u32>,
    pub new_lineno: Option<u32>,
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

    /// Assign `old_lineno`/`new_lineno` to every row by walking from this
    /// hunk's `@@ -old_start,_ +new_start,_ @@` starting points. Context lines
    /// advance both counters; deletions advance only the old side (new = None);
    /// additions advance only the new side (old = None). A header with no
    /// parseable `@@` starts (e.g. binary / synthesized) leaves rows as-is.
    pub fn assign_line_numbers(&mut self) {
        let Some((mut old, mut new)) = parse_hunk_starts(&self.header) else {
            return;
        };
        for row in &mut self.rows {
            match row.kind {
                LineKind::Ctx => {
                    row.old_lineno = Some(old);
                    row.new_lineno = Some(new);
                    old += 1;
                    new += 1;
                }
                LineKind::Del => {
                    row.old_lineno = Some(old);
                    row.new_lineno = None;
                    old += 1;
                }
                LineKind::Add => {
                    row.old_lineno = None;
                    row.new_lineno = Some(new);
                    new += 1;
                }
            }
        }
    }
}

/// Parse the old/new starting line numbers from a hunk header of the form
/// `@@ -<old_start>[,<count>] +<new_start>[,<count>] @@ ...`. Returns
/// `(old_start, new_start)`, or None if the header doesn't match.
pub fn parse_hunk_starts(header: &str) -> Option<(u32, u32)> {
    let rest = header.strip_prefix("@@")?.trim_start();
    let mut parts = rest.split_whitespace();
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    // Each side is "start" or "start,count"; we only want the start.
    let old_start: u32 = old.split(',').next()?.parse().ok()?;
    let new_start: u32 = new.split(',').next()?.parse().ok()?;
    Some((old_start, new_start))
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
                    .push(DiffLineRow {
                        kind,
                        text: content,
                        old_lineno: None,
                        new_lineno: None,
                    });
            }
        }
        true
    })?;

    for file in &mut files {
        for hunk in &mut file.hunks {
            hunk.assign_line_numbers();
        }
    }

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
                old_lineno: None,
                new_lineno: None,
            });
        }
    }
    for file in &mut files {
        for hunk in &mut file.hunks {
            hunk.assign_line_numbers();
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
            old_lineno: None,
            new_lineno: None,
        });
    }
    hunk.assign_line_numbers();
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
    fn line_numbers_across_multiple_hunks() {
        // Two hunks. Hunk 1 starts at old 1 / new 1; hunk 2 at old 10 / new 11
        // (the new side is one ahead because hunk 1 added a net line).
        let patch = "\
diff --git a/f.txt b/f.txt
--- a/f.txt
+++ b/f.txt
@@ -1,3 +1,4 @@
 alpha
-beta
+BETA
+gamma
 delta
@@ -10,2 +11,2 @@
 ten
-eleven
+ELEVEN
";
        let files = parse_unified_patch(patch);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].hunks.len(), 2);

        // Hunk 1 rows: ctx, del, add, add, ctx.
        let h0 = &files[0].hunks[0].rows;
        // ctx "alpha": both advance from the starts.
        assert_eq!((h0[0].old_lineno, h0[0].new_lineno), (Some(1), Some(1)));
        // del "beta": old only.
        assert_eq!((h0[1].old_lineno, h0[1].new_lineno), (Some(2), None));
        // add "BETA": new only.
        assert_eq!((h0[2].old_lineno, h0[2].new_lineno), (None, Some(2)));
        // add "gamma": new only, continues.
        assert_eq!((h0[3].old_lineno, h0[3].new_lineno), (None, Some(3)));
        // ctx "delta": old advanced past the single deletion (3), new past the
        // two additions (4).
        assert_eq!((h0[4].old_lineno, h0[4].new_lineno), (Some(3), Some(4)));

        // Hunk 2 restarts from its OWN @@ starts (10 / 11), not continuing h0.
        let h1 = &files[0].hunks[1].rows;
        assert_eq!((h1[0].old_lineno, h1[0].new_lineno), (Some(10), Some(11)));
        assert_eq!((h1[1].old_lineno, h1[1].new_lineno), (Some(11), None));
        assert_eq!((h1[2].old_lineno, h1[2].new_lineno), (None, Some(12)));
    }

    #[test]
    fn line_numbers_for_non_one_start() {
        // A hunk that doesn't start at line 1: numbers must honor the @@ starts.
        let patch = "\
diff --git a/g.txt b/g.txt
--- a/g.txt
+++ b/g.txt
@@ -40,6 +40,7 @@ fn context()
 a
 b
 c
+inserted
 d
 e
 f
";
        let files = parse_unified_patch(patch);
        let rows = &files[0].hunks[0].rows;
        // First context line is line 40 on both sides.
        assert_eq!((rows[0].old_lineno, rows[0].new_lineno), (Some(40), Some(40)));
        assert_eq!((rows[1].old_lineno, rows[1].new_lineno), (Some(41), Some(41)));
        assert_eq!((rows[2].old_lineno, rows[2].new_lineno), (Some(42), Some(42)));
        // The insertion: new only, at 43; old unchanged.
        assert_eq!((rows[3].old_lineno, rows[3].new_lineno), (None, Some(43)));
        // Context after the insertion: old continues at 43, new at 44.
        assert_eq!((rows[4].old_lineno, rows[4].new_lineno), (Some(43), Some(44)));
        assert_eq!((rows[5].old_lineno, rows[5].new_lineno), (Some(44), Some(45)));
    }

    #[test]
    fn parse_hunk_starts_handles_both_forms() {
        assert_eq!(parse_hunk_starts("@@ -1,3 +1,4 @@"), Some((1, 1)));
        assert_eq!(parse_hunk_starts("@@ -40,6 +40,7 @@ fn foo()"), Some((40, 40)));
        // Single-line hunks omit the count.
        assert_eq!(parse_hunk_starts("@@ -5 +6 @@"), Some((5, 6)));
        // Non-headers / synthesized headers don't parse.
        assert_eq!(parse_hunk_starts("(binary file)"), None);
        assert_eq!(parse_hunk_starts(""), None);
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
