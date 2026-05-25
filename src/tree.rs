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
