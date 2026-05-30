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
        ensure_purview_dir(repo_root)?;
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
                    HunkState { header: "@@ -1 +1 @@".into(), status: "approved".into(), comment: None },
                    HunkState { header: "@@ -9 +9 @@".into(), status: "rejected".into(), comment: Some("why".into()) },
                    HunkState { header: "@@ -20 +20 @@".into(), status: "unreviewed".into(), comment: None },
                ],
            }],
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
}
