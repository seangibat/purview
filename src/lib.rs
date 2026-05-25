//! Shared library crate for purview — code used by the GUI binary
//! (`main.rs`), the MCP server binary (`bin/purview-mcp.rs`), tests, and
//! benchmarks. Pure logic lives here; `main.rs` is just the egui shell.

pub mod diff;
pub mod highlight;
pub mod review_state;
pub mod tree;
