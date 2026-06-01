//! Go-to-definition via "git grep for recall, Haiku for precision".
//!
//! No language server. We `git grep` every word-boundary occurrence of the
//! symbol (fast, recall-complete, language-agnostic), then ask the Claude
//! CLI (Haiku) which candidate is the actual *definition*. The model only
//! ever sees the candidate set, so it scales to a monorepo and is grounded
//! (can't hallucinate a location that isn't a real grep hit).

use std::path::Path;
use std::process::Command;

/// Haiku model id used for the precision step.
const MODEL: &str = "claude-haiku-4-5-20251001";
/// Cap candidates so the prompt stays bounded on a huge monorepo.
const MAX_CANDIDATES: usize = 60;

#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub file: String,
    pub line: usize,
    pub text: String,
}

/// Is `symbol` an identifier-ish token safe to pass to `git grep` (no shell or
/// regex surprises)? Shared guard for the local and remote grep paths.
pub fn is_safe_symbol(symbol: &str) -> bool {
    !symbol.is_empty()
        && symbol
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == ':' || c == '~')
}

/// Parse `git grep -n` output (`path:lineno:code` per line) into candidates,
/// capped at MAX_CANDIDATES. Shared by the local backend (which runs git grep
/// itself) and the SSH backend (which runs it on the remote and pipes the same
/// text back).
pub fn parse_grep_output(stdout: &str) -> Vec<Candidate> {
    let mut cands = Vec::new();
    for line in stdout.lines() {
        // format: path:lineno:code
        let mut parts = line.splitn(3, ':');
        let (Some(file), Some(lineno), Some(code)) =
            (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let Ok(n) = lineno.parse::<usize>() else { continue };
        cands.push(Candidate {
            file: file.to_string(),
            line: n,
            text: code.trim().chars().take(200).collect(),
        });
        if cands.len() >= MAX_CANDIDATES {
            break;
        }
    }
    cands
}

/// `git grep -n -w <symbol>` → candidate locations. Word-boundary so we don't
/// match substrings. Returns up to MAX_CANDIDATES. Local-only; the SSH backend
/// runs the equivalent grep on the remote and parses via [`parse_grep_output`].
pub fn grep_candidates(root: &Path, symbol: &str) -> Vec<Candidate> {
    // Guard: only allow identifier-ish symbols (avoid shell/regex surprises).
    if !is_safe_symbol(symbol) {
        return Vec::new();
    }
    // --untracked so new files in the review (not yet git-added) are also
    // searched — a definition can live in a brand-new file.
    let out = Command::new("git")
        .args(["grep", "-n", "-w", "--untracked", "--", symbol])
        .current_dir(root)
        .output();
    let Ok(out) = out else { return Vec::new() };
    let stdout = String::from_utf8_lossy(&out.stdout);
    parse_grep_output(&stdout)
}

/// The Claude-CLI args (excluding the prompt itself) used for the precision
/// step. Shared so the local and SSH backends invoke `claude` identically.
/// The prompt is passed however the backend prefers (positional arg locally,
/// or over ssh stdin remotely — the CLI accepts either).
pub fn claude_args() -> [&'static str; 3] {
    ["-p", "--model", MODEL]
}

/// Build the precision prompt: numbered candidates, ask for the definition's
/// index (1-based) or 0. Shared verbatim by both backends so local and remote
/// resolution see identical input.
pub fn build_prompt(symbol: &str, usage: Option<&str>, cands: &[Candidate]) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "You are locating where the symbol `{symbol}` is DEFINED or DECLARED \
         (not merely used) in a codebase.\n"
    ));
    if let Some(u) = usage {
        s.push_str(&format!("It is used here: {u}\n"));
    }
    s.push_str(
        "\nCandidate locations (file:line: code):\n\
         Reply with ONLY the number of the candidate that is the definition/\
         declaration. If none is a definition, reply 0. No other text.\n\n",
    );
    for (i, c) in cands.iter().enumerate() {
        s.push_str(&format!("{}. {}:{}: {}\n", i + 1, c.file, c.line, c.text));
    }
    s
}

/// Run the local `claude` CLI on `prompt`, returning its stdout. This is the
/// exact invocation `LocalRepo` uses; the SSH backend runs the SAME command on
/// the remote instead. Blocking.
pub fn run_claude_local(prompt: &str) -> Result<String, String> {
    let out = Command::new("claude")
        .args(claude_args())
        .arg(prompt)
        .output()
        .map_err(|e| format!("claude: failed to launch: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "claude exited non-zero: {}",
            err.trim().lines().next().unwrap_or("(no stderr)")
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Turn the Claude CLI's reply into the chosen candidate. Shared by both
/// backends so the reply→candidate mapping (and the "0 = none" convention) is
/// identical regardless of where `claude` ran.
pub fn pick_candidate(reply: &str, cands: &[Candidate]) -> Option<Candidate> {
    let n = parse_choice(reply)?;
    if n == 0 {
        return None;
    }
    cands.get(n - 1).cloned()
}

/// Pull the first integer out of the model's reply.
fn parse_choice(reply: &str) -> Option<usize> {
    let digits: String = reply
        .trim()
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// Full LOCAL pipeline: grep then resolve via the local `claude` CLI. Blocking.
/// The trait method [`RepoSource::resolve_definition`](crate::repo::RepoSource::resolve_definition)
/// is the production entry point (it runs `claude` where the repo lives); this
/// stays as a convenience for the local case.
pub fn find_definition(root: &Path, symbol: &str, usage: Option<&str>) -> Option<Candidate> {
    let cands = grep_candidates(root, symbol);
    if cands.is_empty() {
        return None;
    }
    if cands.len() == 1 {
        return Some(cands[0].clone());
    }
    let prompt = build_prompt(symbol, usage, &cands);
    let reply = run_claude_local(&prompt).ok()?;
    pick_candidate(&reply, &cands)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_choice_extracts_first_int() {
        assert_eq!(parse_choice("3"), Some(3));
        assert_eq!(parse_choice("  3\n"), Some(3));
        assert_eq!(parse_choice("The answer is 12."), Some(12));
        assert_eq!(parse_choice("0"), Some(0));
        assert_eq!(parse_choice("none"), None);
    }

    #[test]
    fn claude_args_are_stable() {
        // Both backends invoke `claude` with exactly these args (+ the prompt),
        // so local and remote resolution are identical. Lock the shape.
        assert_eq!(claude_args(), ["-p", "--model", MODEL]);
    }

    #[test]
    fn pick_candidate_maps_reply_to_candidate() {
        let cands = vec![
            Candidate { file: "a.rs".into(), line: 1, text: "fn foo".into() },
            Candidate { file: "b.rs".into(), line: 9, text: "foo()".into() },
        ];
        // 1-based index; "0" and out-of-range / junk → None.
        assert_eq!(pick_candidate("1", &cands), Some(cands[0].clone()));
        assert_eq!(pick_candidate("2\n", &cands), Some(cands[1].clone()));
        assert_eq!(pick_candidate("0", &cands), None);
        assert_eq!(pick_candidate("9", &cands), None);
        assert_eq!(pick_candidate("none", &cands), None);
    }

    #[test]
    fn grep_rejects_non_identifier_symbols() {
        // Shouldn't shell out for junk; returns empty.
        assert!(grep_candidates(Path::new("."), "foo; rm -rf").is_empty());
        assert!(grep_candidates(Path::new("."), "").is_empty());
    }

    #[test]
    fn prompt_numbers_candidates() {
        let cands = vec![
            Candidate { file: "a.rs".into(), line: 1, text: "fn foo".into() },
            Candidate { file: "b.rs".into(), line: 9, text: "foo()".into() },
        ];
        let p = build_prompt("foo", Some("a.rs:5"), &cands);
        assert!(p.contains("1. a.rs:1: fn foo"));
        assert!(p.contains("2. b.rs:9: foo()"));
        assert!(p.contains("`foo`"));
    }

    #[test]
    fn parse_grep_output_into_candidates() {
        // The exact text shape `git grep -n` prints (path:line:code), as the
        // remote (SSH) backend pipes it back for parsing.
        let raw = "src/repo.rs:30:pub trait RepoSource {\n\
                   src/repo.rs:121:impl RepoSource for LocalRepo {\n\
                   src/main.rs:18:use purview::repo::{self, RepoSource};\n";
        let cands = parse_grep_output(raw);
        assert_eq!(cands.len(), 3);
        assert_eq!(cands[0].file, "src/repo.rs");
        assert_eq!(cands[0].line, 30);
        assert_eq!(cands[0].text, "pub trait RepoSource {");
        assert_eq!(cands[1].line, 121);
        assert_eq!(cands[2].file, "src/main.rs");
    }

    #[test]
    fn parse_grep_output_skips_malformed_and_keeps_colons_in_code() {
        // A line with no line-number column is skipped; colons inside the code
        // column survive (splitn(3) stops after the first two separators).
        let raw = "garbage-no-columns\n\
                   a.rs:7:let x: HashMap<K, V> = map;\n";
        let cands = parse_grep_output(raw);
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].file, "a.rs");
        assert_eq!(cands[0].line, 7);
        assert_eq!(cands[0].text, "let x: HashMap<K, V> = map;");
    }

    #[test]
    fn safe_symbol_guard() {
        assert!(is_safe_symbol("OrderValidator"));
        assert!(is_safe_symbol("Foo::bar"));
        assert!(is_safe_symbol("~Dtor"));
        assert!(!is_safe_symbol(""));
        assert!(!is_safe_symbol("foo; rm -rf"));
        assert!(!is_safe_symbol("a b"));
    }
}
