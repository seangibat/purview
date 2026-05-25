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

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ReviewState {
    pub branch: String,
    /// Human-readable diff range, e.g. "main...HEAD" or "working tree vs HEAD".
    pub range: String,
    pub files: Vec<FileState>,
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

    /// Replies matching a given file + hunk header, in order.
    pub fn for_hunk<'a>(&'a self, file: &str, hunk_header: &str) -> Vec<&'a Reply> {
        self.replies
            .iter()
            .filter(|r| r.file == file && r.hunk_header == hunk_header)
            .collect()
    }
}

impl ReviewState {
    /// `<repo_root>/.purview/review-state.json`.
    pub fn path_for(repo_root: &Path) -> PathBuf {
        repo_root.join(".purview").join("review-state.json")
    }

    pub fn save(&self, repo_root: &Path) -> std::io::Result<()> {
        let dir = repo_root.join(".purview");
        std::fs::create_dir_all(&dir)?;
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        // Atomic: the MCP server may be reading this file concurrently.
        atomic_write(&Self::path_for(repo_root), &json)
    }

    pub fn load(repo_root: &Path) -> std::io::Result<Self> {
        let raw = std::fs::read_to_string(Self::path_for(repo_root))?;
        serde_json::from_str(&raw)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
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
