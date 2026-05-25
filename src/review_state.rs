//! Serializable review state, written by the GUI to
//! `<repo>/.purview/review-state.json` and read by the MCP server. This is
//! the contract between the two processes — keep it stable.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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
        std::fs::write(Self::path_for(repo_root), json)
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
