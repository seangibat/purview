//! SSH-backed [`RepoSource`](super::RepoSource): review a repo on a remote
//! machine while running the purview GUI locally (like VS Code Remote-SSH).
//!
//! It shells out to the system `ssh` binary (assumes key-based auth, no
//! password prompts) and runs the SAME git commands `LocalRepo` would, just
//! on the other end of the connection. To keep each call cheap we set up SSH
//! connection multiplexing once at startup (ControlMaster + ControlPersist):
//! a single TCP/auth handshake is shared by every subsequent `ssh` call.
//!
//! v1 scope is the review READ path — diff, viewing full files, navigation.
//! Inline editing (write-back) and F12 go-to-definition are disabled in this
//! mode (clear status, not a crash) and noted as follow-ups.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::diff::{self, ChangedFile, DiffSource};
use crate::gotodef::Candidate;

use super::{DirEntry, RepoSource};

/// A parsed `ssh://[user@]host[:port]/abs/path` target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshTarget {
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
    /// Absolute path to the repo on the remote.
    pub path: String,
}

impl SshTarget {
    /// Parse an `ssh://` URL. Returns None if `arg` isn't an ssh URL (so the
    /// caller can fall back to treating it as a local path). The path must be
    /// absolute (leading `/`), matching the documented invocation.
    pub fn parse(arg: &str) -> Option<SshTarget> {
        let rest = arg.strip_prefix("ssh://")?;
        // Split authority from path at the first '/'. The remote path keeps
        // that leading slash (absolute).
        let slash = rest.find('/')?;
        let authority = &rest[..slash];
        let path = &rest[slash..];
        if authority.is_empty() || path.len() < 2 {
            return None;
        }

        // authority = [user@]host[:port]
        let (user, hostport) = match authority.split_once('@') {
            Some((u, hp)) => (Some(u.to_string()), hp),
            None => (None, authority),
        };
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) => match p.parse::<u16>() {
                Ok(n) => (h.to_string(), Some(n)),
                // A colon with a non-numeric tail isn't a port — treat the
                // whole thing as the host (e.g. an IPv6-ish literal). Keep it
                // simple: bail rather than guess.
                Err(_) => (hostport.to_string(), None),
            },
            None => (hostport.to_string(), None),
        };
        if host.is_empty() {
            return None;
        }
        Some(SshTarget {
            user,
            host,
            port,
            path: path.to_string(),
        })
    }

    /// The `[user@]host` SSH destination.
    fn destination(&self) -> String {
        match &self.user {
            Some(u) => format!("{u}@{}", self.host),
            None => self.host.clone(),
        }
    }
}

/// An SSH-backed repo. Owns the multiplexing control socket; dropping it tears
/// the master connection down.
pub struct SshRepo {
    target: SshTarget,
    /// Temp dir holding the ControlPath socket (removed on drop).
    ctl_dir: PathBuf,
    /// Local mirror dir where `.purview/` review state is written, so the
    /// (local) MCP server can still read it. Per-target, stable across runs.
    state_root: PathBuf,
}

impl SshRepo {
    /// Open the master connection and verify the remote repo is reachable.
    pub fn connect(target: SshTarget) -> Result<SshRepo, String> {
        // A unique temp dir for the control socket. ssh requires the socket
        // path be short-ish; a temp dir keeps it out of the way.
        let ctl_dir = std::env::temp_dir().join(format!(
            "purview-ssh-{}-{}",
            std::process::id(),
            nanos()
        ));
        std::fs::create_dir_all(&ctl_dir)
            .map_err(|e| format!("cannot create ssh control dir: {e}"))?;

        // A stable local dir to mirror review state into (so re-runs against
        // the same remote repo reuse it). Keyed by host + path.
        let key = sanitize(&format!(
            "{}_{}",
            target.host,
            target.path.trim_start_matches('/')
        ));
        let state_root = local_state_base().join(key);
        std::fs::create_dir_all(&state_root)
            .map_err(|e| format!("cannot create local state dir: {e}"))?;

        let repo = SshRepo {
            target,
            ctl_dir,
            state_root,
        };

        // Open the master connection (background, persists). `-M` master,
        // `-N` no remote command, `-f` background after auth.
        let status = Command::new("ssh")
            .args(repo.mux_opts())
            .args(["-M", "-N", "-f"])
            .arg(repo.target.destination())
            .args(port_arg(&repo.target))
            .status()
            .map_err(|e| format!("failed to launch ssh: {e}"))?;
        if !status.success() {
            return Err(format!(
                "ssh: could not open connection to {} (check host/key)",
                repo.target.destination()
            ));
        }

        // Sanity-check the remote path is a git repo. Gives a clear error up
        // front instead of an opaque failure on first diff.
        let out = repo.run_git(&["rev-parse", "--is-inside-work-tree"])?;
        if out.trim() != "true" {
            return Err(format!(
                "{} on {} is not a git work tree",
                repo.target.path, repo.target.host
            ));
        }
        Ok(repo)
    }

    /// ssh `-o` options enabling connection multiplexing — passed to EVERY
    /// call so they share the one master connection opened at connect().
    fn mux_opts(&self) -> Vec<String> {
        let ctl_path = self.ctl_dir.join("ctl").to_string_lossy().into_owned();
        vec![
            "-o".into(),
            "ControlMaster=auto".into(),
            "-o".into(),
            format!("ControlPath={ctl_path}"),
            "-o".into(),
            "ControlPersist=60s".into(),
            "-o".into(),
            "BatchMode=yes".into(), // never block on a password prompt
        ]
    }

    /// Run `git <args>` in the remote repo dir, returning stdout. The remote
    /// command is `cd '<path>' && git <args...>`, with each git arg shell-
    /// quoted so paths/revs with spaces survive the remote shell.
    fn run_git(&self, args: &[&str]) -> Result<String, String> {
        let mut remote = format!("cd {} && git", shell_quote(&self.target.path));
        for a in args {
            remote.push(' ');
            remote.push_str(&shell_quote(a));
        }
        self.run_remote(&remote)
    }

    /// Run an arbitrary remote shell command line over the multiplexed
    /// connection, returning stdout (stderr is surfaced in the error on a
    /// non-zero exit).
    fn run_remote(&self, remote_cmd: &str) -> Result<String, String> {
        let out = Command::new("ssh")
            .args(self.mux_opts())
            .args(port_arg(&self.target))
            .arg(self.target.destination())
            .arg(remote_cmd)
            .output()
            .map_err(|e| format!("ssh exec failed: {e}"))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(format!(
                "remote command failed: {}",
                err.trim().lines().next().unwrap_or("(no stderr)")
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Like [`run_remote`] but pipes `input` to the remote command's stdin.
    /// Used for write-back: the file content goes over stdin (so it never needs
    /// shell-quoting), and the remote `cat > tmp && mv tmp dst` does the write.
    fn run_remote_stdin(&self, remote_cmd: &str, input: &[u8]) -> Result<(), String> {
        self.run_remote_stdin_out(remote_cmd, input, "remote write failed")
            .map(|_| ())
    }

    /// Like [`run_remote_stdin`] but RETURNS the remote command's stdout. Used
    /// for go-to-definition: the (untrusted-length) prompt goes over stdin so it
    /// never needs shell-quoting, and we need `claude`'s reply back. `err_label`
    /// prefixes the error on a non-zero exit.
    fn run_remote_stdin_out(
        &self,
        remote_cmd: &str,
        input: &[u8],
        err_label: &str,
    ) -> Result<String, String> {
        use std::io::Write;
        use std::process::Stdio;

        let mut child = Command::new("ssh")
            .args(self.mux_opts())
            .args(port_arg(&self.target))
            .arg(self.target.destination())
            .arg(remote_cmd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("ssh exec failed: {e}"))?;
        // Take stdin and write in a scope so it's dropped (closed) before we
        // wait — otherwise the remote command blocks for EOF and we deadlock.
        {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| "ssh: could not open stdin".to_string())?;
            stdin
                .write_all(input)
                .map_err(|e| format!("ssh: failed writing to remote stdin: {e}"))?;
        }
        let out = child
            .wait_with_output()
            .map_err(|e| format!("ssh exec failed: {e}"))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(format!(
                "{err_label}: {}",
                err.trim().lines().next().unwrap_or("(no stderr)")
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Resolve the merge-base tree-ish for BranchRange (mirrors LocalRepo:
    /// `base...HEAD` three-dot semantics). Returns the rev to `git show` the
    /// base side from, plus the diff command's range argument.
    fn branch(&self) -> Result<String, String> {
        let head = self.run_git(&["rev-parse", "--abbrev-ref", "HEAD"])?;
        let b = head.trim();
        Ok(if b == "HEAD" {
            "(detached)".to_string()
        } else {
            b.to_string()
        })
    }

    /// The git diff arguments matching `source` — same semantics LocalRepo's
    /// git2 calls produce (working-tree-vs-HEAD, or base...HEAD three-dot).
    fn diff_args(source: DiffSource, base: &str, context: u32, path: Option<&str>) -> Vec<String> {
        let mut v: Vec<String> = vec!["diff".into()];
        // Match git2: full index, no color, the requested context. The "Full
        // extent" view passes u32::MAX, but the git CLI's `-U` overflows on a
        // value that large and silently produces a broken, near-zero-context
        // diff. git2 (the local backend) clamps internally; the CLI does not,
        // so clamp here to a value larger than any real file.
        let context = context.min(1_000_000_000);
        v.push(format!("--unified={context}"));
        match source {
            DiffSource::WorkingTree => {
                // working tree (incl. staged) vs HEAD, plus untracked files —
                // git2 here used include_untracked + show_untracked_content.
                // `git diff HEAD` doesn't show untracked; we add them via a
                // separate pass in compute_diff. Here: tracked changes vs HEAD.
                v.push("HEAD".into());
            }
            DiffSource::BranchRange => {
                v.push(format!("{base}...HEAD"));
            }
        }
        if let Some(p) = path {
            v.push("--".into());
            v.push(p.into());
        }
        v
    }
}

impl RepoSource for SshRepo {
    fn compute_diff(
        &self,
        source: DiffSource,
        base: &str,
    ) -> Result<(String, Vec<ChangedFile>), String> {
        self.compute_full(source, base, 3, None)
    }

    fn compute_file_diff(
        &self,
        source: DiffSource,
        base: &str,
        context_lines: u32,
        path: &str,
    ) -> Result<(String, Vec<ChangedFile>), String> {
        let p = if path.is_empty() { None } else { Some(path) };
        self.compute_full(source, base, context_lines, p)
    }

    fn read_file(&self, rel: &str) -> Result<String, String> {
        // Working/new side = the file as it currently exists on the remote.
        let remote = format!(
            "cat {}",
            shell_quote(&format!("{}/{}", self.target.path, rel))
        );
        self.run_remote(&remote)
    }

    fn list_dir(&self, rel: &str) -> Result<Vec<DirEntry>, String> {
        // `git ls-tree`-style listing would miss untracked files; instead list
        // the working dir via `ls`, then drop .git and gitignored entries.
        let dir = if rel.is_empty() {
            self.target.path.clone()
        } else {
            format!("{}/{}", self.target.path, rel)
        };
        // -p marks dirs with a trailing slash, -A skips . and ..
        let out = self.run_remote(&format!("ls -Ap {}", shell_quote(&dir)))?;
        let mut dirs: Vec<DirEntry> = Vec::new();
        let mut files: Vec<DirEntry> = Vec::new();
        for line in out.lines() {
            let line = line.trim_end();
            if line.is_empty() {
                continue;
            }
            let is_dir = line.ends_with('/');
            let name = line.trim_end_matches('/').to_string();
            if name == ".git" {
                continue;
            }
            let child_rel = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            let entry = DirEntry {
                name,
                rel: child_rel,
                is_dir,
            };
            if is_dir {
                dirs.push(entry);
            } else {
                files.push(entry);
            }
        }
        dirs.sort_by(|a, b| a.name.cmp(&b.name));
        files.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(dirs.into_iter().chain(files).collect())
    }

    fn list_all_files(&self, cap: usize) -> (Vec<String>, bool) {
        // `git ls-files` (tracked + untracked, gitignore-respecting) is the
        // remote analog of tree::collect_files. One round trip for the whole
        // tree beats walking dir-by-dir.
        let out = match self.run_git(&[
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
        ]) {
            Ok(o) => o,
            Err(_) => return (Vec::new(), false),
        };
        let mut all: Vec<String> = out.lines().map(|l| l.to_string()).collect();
        all.sort();
        all.dedup();
        let truncated = all.len() > cap;
        all.truncate(cap);
        (all, truncated)
    }

    fn guess_default_base(&self) -> String {
        // Prefer the current branch's upstream tracking ref (PR-stacking base).
        // Equivalent to `git rev-parse --abbrev-ref @{upstream}`, run remotely.
        if let Ok(up) = self.run_git(&["rev-parse", "--abbrev-ref", "@{upstream}"]) {
            let up = up.trim();
            if !up.is_empty() {
                return up.to_string();
            }
        }
        for cand in ["main", "master", "develop", "trunk"] {
            if self
                .run_git(&["rev-parse", "--verify", "--quiet", cand])
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false)
            {
                return cand.to_string();
            }
        }
        "main".to_string()
    }

    fn write_line(&self, rel: &str, line0: usize, new_text: &str) -> Result<(), String> {
        // Read the remote file, apply the single-line edit locally with the
        // SAME helper LocalRepo uses (identical semantics + trailing-newline
        // handling), then write the whole file back atomically.
        let content = self.read_file(rel)?;
        let out = diff::replace_nth_line(&content, line0, new_text)
            .ok_or_else(|| "line out of range".to_string())?;
        let remote_path = format!("{}/{}", self.target.path, rel);
        let quoted = shell_quote(&remote_path);
        let tmp = shell_quote(&format!("{remote_path}.purview.tmp"));
        // Atomic on the remote: stream the new content into a temp file, then
        // mv it over the original (rename is atomic within a filesystem) so a
        // concurrent reader never sees a half-written file. Content arrives on
        // ssh's stdin so it never has to be shell-quoted.
        let remote_cmd = format!("cat > {tmp} && mv {tmp} {quoted}");
        self.run_remote_stdin(&remote_cmd, out.as_bytes())
    }

    fn grep_symbol(&self, symbol: &str) -> Result<Vec<Candidate>, String> {
        // Same identifier-ish guard the local path applies, so we never send a
        // shell/regex-hostile token to the remote.
        if !crate::gotodef::is_safe_symbol(symbol) {
            return Ok(Vec::new());
        }
        // git grep on the REMOTE, then parse the same `path:line:code` text the
        // local backend parses. `--untracked` so brand-new files are searched.
        // git grep exits non-zero with no matches; treat that as "no
        // candidates" rather than an error.
        let args = [
            "grep",
            "-n",
            "-w",
            "--untracked",
            "--",
            symbol,
        ];
        match self.run_git(&args) {
            Ok(out) => Ok(crate::gotodef::parse_grep_output(&out)),
            Err(_) => Ok(Vec::new()),
        }
    }

    fn resolve_definition(
        &self,
        symbol: &str,
        usage: Option<&str>,
        cands: &[Candidate],
    ) -> Result<Option<Candidate>, String> {
        // Same short-circuits as the default impl (no model call needed).
        if cands.is_empty() {
            return Ok(None);
        }
        if cands.len() == 1 {
            return Ok(Some(cands[0].clone()));
        }
        // Run the SAME `claude` command as the local backend, but on the REMOTE
        // (where the code and `claude` live). The prompt is built by the shared
        // helper so it's byte-identical to local, and streamed over ssh stdin so
        // it never needs shell-quoting (it can be large / contain anything).
        let prompt = crate::gotodef::build_prompt(symbol, usage, cands);
        // Remote: `cd '<repo>' && claude -p --model <MODEL>` reading stdin.
        let mut remote = format!("cd {} && claude", shell_quote(&self.target.path));
        for a in crate::gotodef::claude_args() {
            remote.push(' ');
            remote.push_str(&shell_quote(a));
        }
        let reply =
            self.run_remote_stdin_out(&remote, prompt.as_bytes(), "remote claude failed")?;
        Ok(crate::gotodef::pick_candidate(&reply, cands))
    }

    fn supports_editing(&self) -> bool {
        true
    }

    fn supports_goto(&self) -> bool {
        // git grep AND the Claude-CLI precision step both run on the remote
        // (grep_symbol + resolve_definition), where the code and `claude` live.
        true
    }

    fn label(&self) -> String {
        format!("ssh://{}{}", self.target.host, self.target.path)
    }

    fn state_root(&self) -> &Path {
        &self.state_root
    }
}

impl SshRepo {
    /// Shared diff implementation: run `git diff` on the remote and parse its
    /// unified patch with the SAME parser LocalRepo's output flows through.
    fn compute_full(
        &self,
        source: DiffSource,
        base: &str,
        context_lines: u32,
        path: Option<&str>,
    ) -> Result<(String, Vec<ChangedFile>), String> {
        let branch = self.branch()?;

        let args = Self::diff_args(source, base, context_lines, path);
        let argrefs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let patch = self.run_git(&argrefs)?;
        let mut files = diff::parse_unified_patch(&patch);

        // WorkingTree mode: `git diff HEAD` omits untracked files, but the
        // local backend includes them. Add each untracked file as an all-add
        // hunk so parity holds. (Skip when a single pathspec is requested and
        // it's tracked — the diff already covers it.)
        if source == DiffSource::WorkingTree {
            if let Ok(untracked) = self.run_git(&[
                "ls-files",
                "--others",
                "--exclude-standard",
            ]) {
                for u in untracked.lines() {
                    let u = u.trim();
                    if u.is_empty() {
                        continue;
                    }
                    if let Some(p) = path {
                        if p != u {
                            continue;
                        }
                    }
                    if let Ok(content) = self.read_file(u) {
                        files.push(diff::untracked_as_changed_file(u, &content));
                    }
                }
            }
        }

        Ok((branch, files))
    }
}

impl Drop for SshRepo {
    fn drop(&mut self) {
        // Tear down the master connection, then remove the control dir.
        let ctl_path = self.ctl_dir.join("ctl").to_string_lossy().into_owned();
        let _ = Command::new("ssh")
            .args(["-o", &format!("ControlPath={ctl_path}")])
            .args(port_arg(&self.target))
            .args(["-O", "exit"])
            .arg(self.target.destination())
            .output();
        let _ = std::fs::remove_dir_all(&self.ctl_dir);
    }
}

/// The `-p PORT` ssh argument, or empty if the default port.
fn port_arg(target: &SshTarget) -> Vec<String> {
    match target.port {
        Some(p) => vec!["-p".into(), p.to_string()],
        None => Vec::new(),
    }
}

/// Single-quote a string for a POSIX remote shell: wrap in '...', escaping any
/// embedded single quote as '\''. Safe for paths/revs with spaces or quotes.
fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Sanitize a string into a safe directory name (alnum/._- kept, rest → '_').
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == '.' || c == '_' || c == '-' { c } else { '_' })
        .collect()
}

/// Base dir for local mirrors of remote review state.
fn local_state_base() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(|h| PathBuf::from(h).join(".cache"))
                .unwrap_or_else(std::env::temp_dir)
        })
        .join("purview")
        .join("ssh")
}

fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_host_only() {
        let t = SshTarget::parse("ssh://host/p").unwrap();
        assert_eq!(t.user, None);
        assert_eq!(t.host, "host");
        assert_eq!(t.port, None);
        assert_eq!(t.path, "/p");
    }

    #[test]
    fn parse_user_at_host() {
        let t = SshTarget::parse("ssh://user@host/p").unwrap();
        assert_eq!(t.user.as_deref(), Some("user"));
        assert_eq!(t.host, "host");
        assert_eq!(t.port, None);
        assert_eq!(t.path, "/p");
    }

    #[test]
    fn parse_user_host_port() {
        let t = SshTarget::parse("ssh://user@host:2222/p").unwrap();
        assert_eq!(t.user.as_deref(), Some("user"));
        assert_eq!(t.host, "host");
        assert_eq!(t.port, Some(2222));
        assert_eq!(t.path, "/p");
    }

    #[test]
    fn parse_deep_path_preserved() {
        let t = SshTarget::parse("ssh://h/home/user/code/repo").unwrap();
        assert_eq!(t.path, "/home/user/code/repo");
        assert_eq!(t.host, "h");
    }

    #[test]
    fn non_ssh_is_none() {
        assert!(SshTarget::parse("/local/path").is_none());
        assert!(SshTarget::parse("./rel").is_none());
        assert!(SshTarget::parse("ssh://host").is_none()); // no path
        assert!(SshTarget::parse("ssh:///p").is_none()); // no authority
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    // --- Bug 1: full-file view over SSH ---------------------------------
    // The "Full extent" view asks for u32::MAX context. The git CLI's
    // `--unified=` overflows on a value that large and silently returns a
    // broken, near-zero-context diff (so the full file never shows). git2
    // (the local backend) clamps internally; the CLI does not, so diff_args
    // clamps to a value larger than any real file. These tests lock that in.

    /// The emitted `--unified` value must be clamped — never u32::MAX (which
    /// the remote git CLI mishandles) — yet still large enough to span any
    /// real file. This is the regression guard for the SSH full-file bug.
    #[test]
    fn diff_args_clamps_full_context_below_u32_max() {
        let args =
            SshRepo::diff_args(DiffSource::WorkingTree, "main", u32::MAX, None);
        let unified = args
            .iter()
            .find(|a| a.starts_with("--unified="))
            .expect("a --unified arg is emitted");
        let n: u64 = unified
            .strip_prefix("--unified=")
            .unwrap()
            .parse()
            .expect("--unified value is numeric");
        assert!(
            n < u32::MAX as u64,
            "context must be clamped below u32::MAX, got {n}"
        );
        // Still huge, so it really does span any real file.
        assert!(n >= 1_000_000, "clamped value must still be large, got {n}");
        assert_eq!(unified, "--unified=1000000000");
    }

    /// A normal (small) context value passes through untouched.
    #[test]
    fn diff_args_small_context_passes_through() {
        let args = SshRepo::diff_args(DiffSource::WorkingTree, "main", 3, None);
        assert!(args.contains(&"--unified=3".to_string()));
    }

    /// WorkingTree vs BranchRange produce the right rev arguments, and a
    /// pathspec is appended after `--`.
    #[test]
    fn diff_args_source_and_pathspec() {
        let wt = SshRepo::diff_args(DiffSource::WorkingTree, "main", 3, None);
        assert!(wt.contains(&"HEAD".to_string()));
        assert!(!wt.iter().any(|a| a.contains("...")));

        let br = SshRepo::diff_args(DiffSource::BranchRange, "main", 3, None);
        assert!(br.contains(&"main...HEAD".to_string()));

        let with_path =
            SshRepo::diff_args(DiffSource::WorkingTree, "main", 3, Some("src/a.rs"));
        let dd = with_path.iter().position(|a| a == "--").unwrap();
        assert_eq!(with_path[dd + 1], "src/a.rs");
    }

    /// A full-context patch (every line emitted as context, with a couple of
    /// real edits) must parse so that EVERY line of the file is represented —
    /// this is the parser side of the full-file-view fix. Mirrors the patch
    /// shape `git diff --unified=<huge>` produces.
    #[test]
    fn parse_unified_patch_full_context_keeps_all_lines() {
        // 6-line file; line 3 changed from "three" to "THREE".
        let patch = "\
diff --git a/f.txt b/f.txt
index 1111111..2222222 100644
--- a/f.txt
+++ b/f.txt
@@ -1,6 +1,6 @@
 one
 two
-three
+THREE
 four
 five
 six
";
        let files = crate::diff::parse_unified_patch(patch);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "f.txt");
        let rows = &files[0].hunks[0].rows;
        // All 6 source lines are present (5 context + 1 add; the deletion of
        // "three" is also a row). Reconstruct the NEW-side file from the rows.
        use crate::diff::LineKind;
        let new_side: Vec<&str> = rows
            .iter()
            .filter(|r| r.kind != LineKind::Del)
            .map(|r| r.text.as_str())
            .collect();
        assert_eq!(new_side, ["one", "two", "THREE", "four", "five", "six"]);
        // And the removed line is represented as a deletion row.
        assert!(rows
            .iter()
            .any(|r| r.kind == LineKind::Del && r.text == "three"));
    }
}
