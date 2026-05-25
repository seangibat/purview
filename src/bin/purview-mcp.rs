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

use purview::review_state::ReviewState;
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
                let name = msg
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or("");
                Some(call_tool(&repo_root, name, id))
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
    ])
}

fn call_tool(repo_root: &std::path::Path, name: &str, id: Option<Value>) -> Value {
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
