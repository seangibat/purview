//! Lazy repo file tree.
//!
//! Directories list their children only when first expanded — eager-walking
//! a massive monorepo on startup would be the wrong move. We skip `.git` and
//! anything the repo's gitignore rules exclude.
//!
//! `read_children` opens the repo via `Repository::open(root)` (cheap, no
//! filesystem walk-up — `root` is already the workdir) once per directory
//! expansion, not the old per-call `discover`. Expansion is user-driven, not
//! per-frame, so this is plenty.

use std::path::{Path, PathBuf};

use git2::Repository;

pub struct FileTree {
    /// Repo workdir root. `Node.rel` paths are relative to this.
    pub root: PathBuf,
    pub nodes: Vec<Node>,
}

pub struct Node {
    /// Display name (file or dir basename).
    pub name: String,
    /// Path relative to repo root (forward-slashed).
    pub rel: String,
    pub is_dir: bool,
    /// None = not yet loaded (lazy). Some = loaded children. (Open/closed
    /// state lives in egui's CollapsingState, keyed by rel path.)
    pub children: Option<Vec<Node>>,
}

impl FileTree {
    pub fn new(root: PathBuf) -> Self {
        let nodes = read_children(&root, "");
        FileTree { root, nodes }
    }

    /// Ensure a directory node's children are loaded (lazy, on first expand).
    pub fn load_children(root: &Path, node: &mut Node) {
        if node.children.is_some() {
            return;
        }
        node.children = Some(read_children(root, &node.rel));
    }
}

/// Recursively collect every (gitignore-respecting, non-.git) file path in
/// the repo, relative + forward-slashed. Used to populate the Ctrl+P fuzzy
/// finder. Bounded by `cap` so a pathological monorepo can't hang the UI;
/// returns (paths, truncated).
pub fn collect_files(root: &Path, cap: usize) -> (Vec<String>, bool) {
    let repo = Repository::open(root).ok();
    let mut out = Vec::new();
    let mut stack = vec![String::new()];
    let mut truncated = false;
    while let Some(rel) = stack.pop() {
        if out.len() >= cap {
            truncated = true;
            break;
        }
        let abs = if rel.is_empty() { root.to_path_buf() } else { root.join(&rel) };
        let Ok(entries) = std::fs::read_dir(&abs) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == ".git" {
                continue;
            }
            let child_rel = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            if let Some(repo) = &repo {
                if repo.is_path_ignored(Path::new(&child_rel)).unwrap_or(false) {
                    continue;
                }
            }
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                stack.push(child_rel);
            } else {
                out.push(child_rel);
            }
        }
    }
    out.sort();
    (out, truncated)
}

/// Subsequence fuzzy match: does `query`'s chars appear in order within
/// `text` (case-insensitive)? Returns a score (lower = better: prefers
/// earlier + more contiguous matches) or None if no match. Empty query
/// matches everything with score 0.
pub fn fuzzy_score(query: &str, text: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let q: Vec<char> = query.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let mut qi = 0;
    let mut score: i64 = 0;
    let mut last_match: Option<usize> = None;
    for (ti, &c) in t.iter().enumerate() {
        if qi < q.len() && c == q[qi] {
            // Penalize gaps between consecutive matched chars + distance from start.
            if let Some(prev) = last_match {
                score += (ti - prev) as i64;
            } else {
                score += ti as i64; // distance of first match from start
            }
            last_match = Some(ti);
            qi += 1;
        }
    }
    if qi == q.len() {
        Some(score)
    } else {
        None
    }
}

/// List immediate children of `rel` (relative dir path, "" = root),
/// gitignore-aware, dirs first then files, both alphabetical.
fn read_children(root: &Path, rel: &str) -> Vec<Node> {
    let abs = if rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel)
    };
    // open (not discover): root is the workdir, so no walk-up needed.
    let repo = Repository::open(root).ok();

    let mut dirs: Vec<Node> = Vec::new();
    let mut files: Vec<Node> = Vec::new();

    let Ok(entries) = std::fs::read_dir(&abs) else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" {
            continue;
        }
        let child_rel = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        // gitignore filtering, using the workdir-relative path.
        if let Some(repo) = &repo {
            if repo
                .is_path_ignored(Path::new(&child_rel))
                .unwrap_or(false)
            {
                continue;
            }
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let node = Node {
            name,
            rel: child_rel,
            is_dir,
            children: None,
        };
        if is_dir {
            dirs.push(node);
        } else {
            files.push(node);
        }
    }
    dirs.sort_by(|a, b| a.name.cmp(&b.name));
    files.sort_by(|a, b| a.name.cmp(&b.name));
    dirs.into_iter().chain(files).collect()
}

#[cfg(test)]
mod tests {
    use super::fuzzy_score;
    #[test]
    fn fuzzy_basics() {
        assert!(fuzzy_score("", "anything").is_some());
        assert!(fuzzy_score("mainrs", "src/main.rs").is_some());
        assert!(fuzzy_score("xyz", "src/main.rs").is_none());
        // closer/contiguous match scores lower (better)
        let a = fuzzy_score("main", "main.rs").unwrap();
        let b = fuzzy_score("main", "zzz/m_a_i_n.rs").unwrap();
        assert!(a < b);
    }
}
