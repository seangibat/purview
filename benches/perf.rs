//! Performance benchmarks for the paths that matter on a large monorepo:
//! syntax highlighting a big file, loading many agent replies, and
//! serializing a large review state.
//!
//! Run with: cargo bench

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use purview::highlight::Highlighter;
use purview::review_state::{FileState, HunkState, Replies, Reply, ReviewState};

/// A synthetic ~N-line Rust source string.
fn synthetic_rust(lines: usize) -> String {
    let mut s = String::with_capacity(lines * 40);
    for i in 0..lines {
        match i % 5 {
            0 => s.push_str(&format!("fn function_{i}(x: u32) -> u32 {{\n")),
            1 => s.push_str(&format!("    let y = x.wrapping_mul({i}); // comment {i}\n")),
            2 => s.push_str("    let s = \"a string literal with words\";\n"),
            3 => s.push_str(&format!("    y.saturating_add({i})\n")),
            _ => s.push_str("}\n"),
        }
    }
    s
}

fn bench_highlight(c: &mut Criterion) {
    let hl = Highlighter::new();
    let src_5k = synthetic_rust(5_000);
    let src_50k = synthetic_rust(50_000);

    let mut g = c.benchmark_group("highlight_file");
    g.sample_size(20);
    // Full-file highlight cost. The GUI NO LONGER pays this at open time
    // (highlighting is lazy/per-visible-row now) — kept as a regression
    // guard + to document the cost we avoid.
    g.bench_function("5k_lines", |b| {
        b.iter(|| {
            let out = hl.highlight_file("x.rs", src_5k.lines());
            std::hint::black_box(out.len());
        })
    });
    g.bench_function("50k_lines", |b| {
        b.iter(|| {
            let out = hl.highlight_file("x.rs", src_50k.lines());
            std::hint::black_box(out.len());
        })
    });
    g.finish();

    // What opening a file ACTUALLY costs now: highlight just the visible
    // window (~50 rows), regardless of total file size.
    let window: Vec<&str> = src_50k.lines().take(50).collect();
    c.bench_function("highlight_visible_window_50", |b| {
        b.iter(|| {
            for line in &window {
                std::hint::black_box(hl.highlight_line("x.rs", line).len());
            }
        })
    });
}

fn bench_replies_load(c: &mut Criterion) {
    // Stage a replies dir with 500 reply files.
    let dir = std::env::temp_dir().join(format!("purview-bench-replies-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for i in 0..500 {
        Replies::append(
            &dir,
            Reply {
                file: format!("src/file_{}.rs", i % 20),
                hunk_header: format!("@@ -{i} +{i} @@"),
                text: format!("reply number {i} with some explanatory text"),
            },
        )
        .unwrap();
    }
    c.bench_function("replies_load_500", |b| {
        b.iter(|| {
            let r = Replies::load(&dir);
            std::hint::black_box(r.replies.len());
        })
    });
    let _ = std::fs::remove_dir_all(&dir);
}

fn bench_state_serialize(c: &mut Criterion) {
    // A 300-file review, 8 hunks each — a big agent PR.
    let state = ReviewState {
        branch: "feature".into(),
        range: "main...HEAD".into(),
        files: (0..300)
            .map(|f| FileState {
                path: format!("src/module_{f}/file_{f}.rs"),
                hunks: (0..8)
                    .map(|h| HunkState {
                        header: format!("@@ -{h},5 +{h},9 @@ fn thing_{h}()"),
                        status: if h % 2 == 0 { "approved" } else { "unreviewed" }.into(),
                        comment: None,
                    })
                    .collect(),
            })
            .collect(),
    };
    c.bench_function("state_serialize_300_files", |b| {
        b.iter_batched(
            || state.clone(),
            |s| {
                let json = serde_json::to_string(&s).unwrap();
                std::hint::black_box(json.len());
            },
            BatchSize::SmallInput,
        )
    });
}

criterion_group!(benches, bench_highlight, bench_replies_load, bench_state_serialize);
criterion_main!(benches);
