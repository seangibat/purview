//! purview-mcp — a minimal MCP (Model Context Protocol) stdio server that
//! exposes purview's review state read-only.
//!
//! Spawned by an MCP client (e.g. Claude) as:
//!     claude mcp add purview -- purview-mcp /path/to/repo
//!
//! Speaks newline-delimited JSON-RPC 2.0 on stdin/stdout (the MCP stdio
//! transport). Reads <repo>/.purview/review-state.json, which the purview
//! GUI keeps current. v1 is read-only: get_review_state, get_report,
//! list_rejected. Reply tools come once the read path is proven.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use purview::review_state::{Replies, Reply, ReviewState};
use serde_json::{json, Value};

const PROTOCOL_VERSION: &str = "2024-11-05";

fn main() {
    let repo_root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap());

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(line) else {
            eprintln!("purview-mcp: bad JSON: {line}");
            continue;
        };

        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");

        // Notifications (no id) get no response.
        let response = match method {
            "initialize" => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "purview", "version": env!("CARGO_PKG_VERSION") }
                }
            })),
            "tools/list" => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "tools": tool_specs() }
            })),
            "tools/call" => {
                let params = msg.get("params");
                let name = params
                    .and_then(|p| p.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or("");
                let args = params.and_then(|p| p.get("arguments")).cloned();
                Some(call_tool(&repo_root, name, args, id))
            }
            // notifications/initialized and other notifications: ignore.
            _ if id.is_none() => None,
            _ => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("method not found: {method}") }
            })),
        };

        if let Some(resp) = response {
            let _ = writeln!(out, "{resp}");
            let _ = out.flush();
        }
    }
}

fn tool_specs() -> Value {
    let empty = json!({ "type": "object", "properties": {} });
    json!([
        {
            "name": "get_review_state",
            "description": "Full review state: branch, diff range, and every changed file with its hunks and per-hunk review status (unreviewed/approved/rejected).",
            "inputSchema": empty,
        },
        {
            "name": "get_report",
            "description": "The human-readable markdown review report (progress + rejected + still-unreviewed hunks).",
            "inputSchema": empty,
        },
        {
            "name": "list_rejected",
            "description": "Just the hunks the reviewer rejected (need changes), grouped by file. Use this to know what to fix.",
            "inputSchema": empty,
        },
        {
            "name": "reply_to_comment",
            "description": "Post a reply to the reviewer on a specific hunk's comment thread. The reply appears inline in purview. Identify the hunk by its file path and hunk header (both from get_review_state).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": { "type": "string", "description": "File path, exactly as in get_review_state." },
                    "hunk_header": { "type": "string", "description": "The hunk's header line, exactly as in get_review_state." },
                    "text": { "type": "string", "description": "Your reply." }
                },
                "required": ["file", "hunk_header", "text"]
            },
        },
    ])
}

fn call_tool(
    repo_root: &std::path::Path,
    name: &str,
    args: Option<Value>,
    id: Option<Value>,
) -> Value {
    let text = match name {
        "get_review_state" => match ReviewState::load(repo_root) {
            Ok(state) => serde_json::to_string_pretty(&state)
                .unwrap_or_else(|e| format!("serialize error: {e}")),
            Err(e) => return tool_error(id, &format!("no review state: {e}")),
        },
        "get_report" => {
            let p = repo_root.join(".purview").join("review-report.md");
            match std::fs::read_to_string(&p) {
                Ok(s) => s,
                Err(e) => return tool_error(id, &format!("no report at {}: {e}", p.display())),
            }
        }
        "list_rejected" => match ReviewState::load(repo_root) {
            Ok(state) => {
                let mut s = String::new();
                for f in &state.files {
                    let rej: Vec<&_> =
                        f.hunks.iter().filter(|h| h.status == "rejected").collect();
                    if rej.is_empty() {
                        continue;
                    }
                    s.push_str(&format!("## {}\n", f.path));
                    for h in rej {
                        s.push_str(&format!("- {}\n", h.header.trim()));
                        if let Some(c) = &h.comment {
                            s.push_str(&format!("  comment: {c}\n"));
                        }
                    }
                }
                if s.is_empty() {
                    "No rejected hunks.".to_string()
                } else {
                    s
                }
            }
            Err(e) => return tool_error(id, &format!("no review state: {e}")),
        },
        "reply_to_comment" => {
            let args = args.unwrap_or(Value::Null);
            let file = args.get("file").and_then(|v| v.as_str()).unwrap_or("");
            let hunk_header = args.get("hunk_header").and_then(|v| v.as_str()).unwrap_or("");
            let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
            if file.is_empty() || hunk_header.is_empty() || text.is_empty() {
                return tool_error(id, "reply_to_comment requires file, hunk_header, text");
            }
            // Validate the target hunk exists, so a reply can't vanish into a
            // path/header that never renders.
            let exists = ReviewState::load(repo_root)
                .map(|s| {
                    s.files.iter().any(|f| {
                        f.path == file && f.hunks.iter().any(|h| h.header == hunk_header)
                    })
                })
                .unwrap_or(false);
            if !exists {
                return tool_error(
                    id,
                    &format!("no hunk matches file={file:?} hunk_header={hunk_header:?} — check get_review_state"),
                );
            }
            match Replies::append(
                repo_root,
                Reply {
                    file: file.to_string(),
                    hunk_header: hunk_header.to_string(),
                    text: text.to_string(),
                },
            ) {
                Ok(()) => "reply posted".to_string(),
                Err(e) => return tool_error(id, &format!("failed to post reply: {e}")),
            }
        }
        other => return tool_error(id, &format!("unknown tool: {other}")),
    };

    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [ { "type": "text", "text": text } ],
            "isError": false
        }
    })
}

fn tool_error(id: Option<Value>, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [ { "type": "text", "text": message } ],
            "isError": true
        }
    })
}
