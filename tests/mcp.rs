//! Integration test: drive the purview-mcp binary over stdio like a real
//! MCP client would, and assert the JSON-RPC responses.

use std::io::Write;
use std::process::{Command, Stdio};

/// Run purview-mcp against `repo_dir`, feeding `requests` (one JSON-RPC
/// message per line) on stdin; return stdout lines.
fn run_mcp(repo_dir: &std::path::Path, requests: &[&str]) -> Vec<String> {
    let bin = env!("CARGO_BIN_EXE_purview-mcp");
    let mut child = Command::new(bin)
        .arg(repo_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn purview-mcp");
    {
        let stdin = child.stdin.as_mut().unwrap();
        for r in requests {
            writeln!(stdin, "{r}").unwrap();
        }
    } // drop stdin → EOF → server exits its read loop
    let out = child.wait_with_output().expect("wait");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.to_string())
        .collect()
}

fn fixture(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("purview-mcp-it-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join(".purview")).unwrap();
    std::fs::write(
        d.join(".purview").join("review-state.json"),
        r#"{"branch":"feat","range":"main...HEAD","files":[
            {"path":"src/a.rs","hunks":[
              {"header":"@@ -1 +1 @@","status":"rejected","comment":"why?"}]}]}"#,
    )
    .unwrap();
    d
}

#[test]
fn initialize_and_list_tools() {
    let dir = fixture("init");
    let out = run_mcp(
        &dir,
        &[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
        ],
    );
    // notification produced no line → exactly two responses.
    assert_eq!(out.len(), 2, "init + tools/list, no reply to the notification");
    assert!(out[0].contains("\"protocolVersion\""));
    assert!(out[0].contains("purview"));
    for tool in ["get_review_state", "get_report", "list_rejected", "reply_to_comment"] {
        assert!(out[1].contains(tool), "tools/list missing {tool}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn list_rejected_returns_the_rejected_hunk() {
    let dir = fixture("rej");
    let out = run_mcp(
        &dir,
        &[r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_rejected","arguments":{}}}"#],
    );
    assert_eq!(out.len(), 1);
    assert!(out[0].contains("src/a.rs"));
    assert!(out[0].contains("@@ -1 +1 @@"));
    assert!(out[0].contains("\"isError\":false"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn reply_round_trip_and_validation() {
    let dir = fixture("reply");
    let out = run_mcp(
        &dir,
        &[
            // valid hunk
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"reply_to_comment","arguments":{"file":"src/a.rs","hunk_header":"@@ -1 +1 @@","text":"fixed it"}}}"#,
            // bogus hunk → isError
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"reply_to_comment","arguments":{"file":"src/a.rs","hunk_header":"@@ -99 +99 @@","text":"x"}}}"#,
        ],
    );
    assert_eq!(out.len(), 2);
    assert!(out[0].contains("reply posted"));
    assert!(out[0].contains("\"isError\":false"));
    assert!(out[1].contains("\"isError\":true"), "bogus hunk should error");

    // The reply file landed.
    let replies = purview::review_state::Replies::load(&dir);
    assert_eq!(replies.replies.len(), 1);
    assert_eq!(replies.replies[0].text, "fixed it");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unknown_method_errors_but_does_not_crash() {
    let dir = fixture("unknown");
    let out = run_mcp(
        &dir,
        &[
            r#"{"jsonrpc":"2.0","id":1,"method":"no/such/method","params":{}}"#,
            r#"not even json"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
        ],
    );
    // method-not-found gets an error response; bad JSON is skipped; tools/list still works.
    assert!(out.iter().any(|l| l.contains("-32601")));
    assert!(out.iter().any(|l| l.contains("get_review_state")));
    let _ = std::fs::remove_dir_all(&dir);
}
