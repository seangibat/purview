//! Live SSH integration test for the full-file-view fix (bug #1).
//!
//! The "Full extent" view asks the diff for u32::MAX context lines. Over SSH
//! that became `git diff --unified=4294967295`, which the git CLI mishandles
//! (integer overflow → a near-zero-context diff), so the full file never
//! showed. `SshRepo::diff_args` clamps the context to span any real file.
//!
//! This test exercises the REAL remote path: it builds a throwaway git repo
//! under /tmp, opens it via `ssh://localhost/<dir>`, asks for a full-context
//! file diff, and asserts every line of the file is represented (the bug made
//! the middle of the file disappear).
//!
//! It is best-effort: if `ssh localhost` (key-based, no prompt) or `git`
//! aren't available in the harness, it SKIPS rather than fails — the
//! deterministic unit tests in `src/repo/ssh.rs` and `src/diff.rs` are the
//! hard guard; this is the belt-and-suspenders live check.

use std::path::Path;
use std::process::Command;

use purview::diff::{DiffSource, LineKind};
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
fn ssh_full_file_diff_shows_every_line() {
    if !ssh_localhost_works() {
        eprintln!("SKIP ssh_full_file_diff_shows_every_line: ssh localhost unavailable");
        return;
    }

    // Throwaway repo under /tmp.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("purview-ssh-it-{}-{nanos}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    git(&dir, &["init", "-q"]);
    git(&dir, &["config", "user.email", "t@t"]);
    git(&dir, &["config", "user.name", "t"]);
    git(&dir, &["checkout", "-q", "-b", "main"]);

    // A file with many unchanged lines surrounding a single edit. The bug
    // dropped everything but a couple of lines around the change.
    let original: String = (1..=40).map(|n| format!("line {n}\n")).collect();
    std::fs::write(dir.join("f.txt"), &original).unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "init"]);
    // Change just line 20.
    let edited: String = (1..=40)
        .map(|n| if n == 20 { "LINE 20 CHANGED\n".to_string() } else { format!("line {n}\n") })
        .collect();
    std::fs::write(dir.join("f.txt"), &edited).unwrap();

    let url = format!("ssh://localhost{}", dir.display());
    let target = SshTarget::parse(&url).expect("parse ssh url");
    let repo = match SshRepo::connect(target) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("SKIP ssh_full_file_diff_shows_every_line: connect failed: {e}");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
    };

    // Full-extent view: u32::MAX context, single file. With the bug this
    // returned a near-empty diff; with the clamp it spans the whole file.
    let (_branch, files) = repo
        .compute_file_diff(DiffSource::WorkingTree, "main", u32::MAX, "f.txt")
        .expect("remote full-file diff");

    let file = files
        .iter()
        .find(|f| f.path == "f.txt")
        .expect("f.txt in the diff");
    // Reconstruct the NEW side from the diff rows (drop deletions).
    let new_side: Vec<String> = file
        .hunks
        .iter()
        .flat_map(|h| &h.rows)
        .filter(|r| r.kind != LineKind::Del)
        .map(|r| r.text.clone())
        .collect();

    // Every one of the 40 lines must be present — the heart of the bug.
    assert_eq!(
        new_side.len(),
        40,
        "full-file view must show all 40 lines, got {}",
        new_side.len()
    );
    assert_eq!(new_side[0], "line 1");
    assert_eq!(new_side[19], "LINE 20 CHANGED");
    assert_eq!(new_side[39], "line 40");

    drop(repo);
    let _ = std::fs::remove_dir_all(&dir);
}
