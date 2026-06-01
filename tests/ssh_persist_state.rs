//! Live SSH integration test for remote review-state persistence.
//!
//! In SSH mode the GUI runs locally but the repo (and the user's `claude` +
//! `purview-mcp` server) live on the remote. So `SshRepo::persist_state` must
//! write `.purview/review-state.json` (and the report) into the REMOTE repo,
//! where the remote MCP server reads it. This test exercises the REAL remote
//! path: it builds a throwaway git repo under /tmp, opens it via
//! `ssh://localhost/<dir>`, persists state, and asserts the file landed in the
//! remote repo's `.purview/` with matching content (read back via plain fs,
//! valid because the "remote" is localhost) — plus the self-ignoring
//! `.gitignore`.
//!
//! Best-effort: if `ssh localhost` (key-based, no prompt) or `git` aren't
//! available, it SKIPS rather than fails — matching `ssh_full_file.rs`. The
//! deterministic `LocalRepo` unit test is the hard guard for the local path.

use std::path::Path;
use std::process::Command;

use purview::repo::{RepoSource, SshRepo, SshTarget};

/// Can we ssh to localhost without an interactive prompt? Gate the test on it.
fn ssh_localhost_works() -> bool {
    Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            "localhost",
            "true",
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(ok, "git {args:?} failed");
}

#[test]
fn ssh_persist_state_writes_into_remote_repo() {
    if !ssh_localhost_works() {
        eprintln!("SKIP ssh_persist_state_writes_into_remote_repo: ssh localhost unavailable");
        return;
    }

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "purview-ssh-persist-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    git(&dir, &["init", "-q"]);
    git(&dir, &["config", "user.email", "t@t"]);
    git(&dir, &["config", "user.name", "t"]);
    git(&dir, &["checkout", "-q", "-b", "main"]);
    std::fs::write(dir.join("a.txt"), "x\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "init"]);

    let url = format!("ssh://localhost{}", dir.display());
    let target = SshTarget::parse(&url).expect("parse ssh url");
    let repo = match SshRepo::connect(target) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("SKIP ssh_persist_state_writes_into_remote_repo: connect failed: {e}");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
    };

    let body = "{\"branch\":\"main\",\"range\":\"working tree vs HEAD\",\"files\":[]}\n";
    repo.persist_state("review-state.json", body)
        .expect("remote persist_state");
    repo.persist_state("review-report.md", "# Review\nlooks good\n")
        .expect("remote persist_state report");

    // The "remote" is localhost, so read the file back IN THE REMOTE repo's
    // .purview/ via plain fs and assert it matches what we wrote. This is the
    // exact path the remote `purview-mcp` server reads (ReviewState::load on
    // the repo root).
    let remote_state = dir.join(".purview").join("review-state.json");
    assert_eq!(
        std::fs::read_to_string(&remote_state).unwrap(),
        body,
        "review-state.json must land in the REMOTE repo's .purview/ with matching content"
    );
    let remote_report = dir.join(".purview").join("review-report.md");
    assert_eq!(
        std::fs::read_to_string(&remote_report).unwrap(),
        "# Review\nlooks good\n",
        "the report must also land in the remote .purview/"
    );

    // .purview/ self-ignores on the remote so review state never pollutes the
    // user's git status.
    let gi = dir.join(".purview").join(".gitignore");
    assert!(gi.exists(), "remote .purview/.gitignore should exist");
    assert_eq!(std::fs::read_to_string(&gi).unwrap(), "*\n");

    // No temp file left behind by the atomic remote write.
    assert!(
        !dir.join(".purview")
            .join("review-state.json.purview.tmp")
            .exists(),
        "atomic write should leave no temp file"
    );

    // And the local mirror copy is written too (local MCP fallback).
    let mirror = repo.state_root().join(".purview").join("review-state.json");
    assert_eq!(
        std::fs::read_to_string(&mirror).unwrap(),
        body,
        "the local mirror copy should match"
    );

    drop(repo);
    let _ = std::fs::remove_dir_all(&dir);
}
