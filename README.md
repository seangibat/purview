# purview

A native code-review tool for the age of AI-written code. The diff is an
overlay on the codebase, not the unit of work — full files, real navigation,
go-to-definition without a language server, and Claude attached to the review
over MCP.

Built in Rust with [egui](https://github.com/emilk/egui).

## Why

Most review tools treat the **pull request** as the unit of work. But for
AI-generated code, the question isn't "did the author slip up" — it's
**"does this fit the codebase that already exists."** So purview is a
codebase-first viewer with the diff painted on top: full files, real
navigation, with Claude as a first-class participant.

## Features

- **Two-axis views** — Inline ⇄ Split for layout, Summary ⇄ Full for extent.
- **Per-hunk review** — approve, reject, or comment each hunk; track progress;
  export a report.
- **Go to definition** (`F12`) — `git grep` for recall, the Claude CLI for
  precision. No language server, no setup.
- **Inline edits** — fix a line mid-review, written straight back to disk.
- **MCP-native** — attach your terminal Claude to the review over MCP.
- **Remote over SSH** — review a repo on a remote machine while running the GUI
  locally: `purview ssh://user@host/path/to/repo`. Diff, edits, and
  go-to-definition all run where the code lives.

## Install

### Debian / Ubuntu (amd64)

```sh
curl -fL https://cleo.computer/purview/purview_0.1.0-1_amd64.deb -o /tmp/purview.deb
sudo apt install /tmp/purview.deb
```

### Build from source

```sh
# Rust toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Build dependencies (Ubuntu / Debian)
sudo apt install build-essential pkg-config cmake \
  libgtk-3-dev libx11-dev libxcb1-dev libxkbcommon-dev \
  libwayland-dev libgl1-mesa-dev libfontconfig1-dev

cargo build --release
install -m755 target/release/purview target/release/purview-mcp ~/.local/bin/
```

## Usage

```sh
purview                                      # review the current directory
purview /path/to/repo                        # review a local repo
purview ssh://user@host/path/to/repo         # review a remote repo over SSH

# attach a terminal Claude to the review over MCP
claude mcp add purview -- purview-mcp /path/to/repo
```

### Keyboard

| Key | Action |
|-----|--------|
| `Ctrl`+`P` | fuzzy file open |
| `j` / `k` | changed files |
| `n` / `p` | hunks |
| `a` / `r` / `c` | approve / reject / comment |
| `F12` | go to definition |
| `g` | find a symbol |

## License

MIT — see [LICENSE](LICENSE).
