//! purview — codebase-first code review.
//!
//! v0.2: working-tree-vs-HEAD diff with a left changed-files list and a
//! right content pane that toggles between Diff and Full File views, both
//! syntax-highlighted (syntect). Rows are virtualized so the monorepo's
//! giant files stay snappy. Header shows the repo + current branch.
//!
//! Still ahead: branch-range base selection, nested file tree, per-chunk
//! approve/deny review state, comments, symbol jump, the Claude agent pane.

use std::path::PathBuf;

use eframe::egui;
use egui::Color32;
use git2::{Diff, DiffFormat, DiffOptions, Repository};

mod highlight;
use highlight::Highlighter;

fn main() -> eframe::Result<()> {
    let repo_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap());

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 840.0])
            .with_title("purview"),
        ..Default::default()
    };

    eframe::run_native(
        "purview",
        native_options,
        Box::new(move |_cc| Ok(Box::new(App::new(repo_path)))),
    )
}

#[derive(Clone, Copy, PartialEq)]
enum LineKind {
    Add,
    Del,
    Ctx,
    Hunk,
}

/// A diff line: its kind plus the raw text (highlighting applied at render
/// time from the cached per-file highlight, keyed by line content).
#[derive(Clone)]
struct DiffLineRow {
    kind: LineKind,
    text: String,
}

struct ChangedFile {
    path: String,
    diff_rows: Vec<DiffLineRow>,
}

#[derive(Clone, Copy, PartialEq)]
enum ViewMode {
    Diff,
    FullFile,
}

struct App {
    repo_path: PathBuf,
    branch: String,
    files: Vec<ChangedFile>,
    selected: Option<usize>,
    view: ViewMode,
    error: Option<String>,
    hl: Highlighter,
    /// Cached highlighted spans for the currently-shown content, one entry
    /// per visual row: (kind-or-None, Vec<(color, text)>).
    cache: Vec<(Option<LineKind>, Vec<(Color32, String)>)>,
    cache_key: Option<(usize, ViewMode)>,
}

impl App {
    fn new(repo_path: PathBuf) -> Self {
        let mut app = App {
            repo_path,
            branch: String::new(),
            files: Vec::new(),
            selected: None,
            view: ViewMode::Diff,
            error: None,
            hl: Highlighter::new(),
            cache: Vec::new(),
            cache_key: None,
        };
        app.reload();
        app
    }

    fn reload(&mut self) {
        self.files.clear();
        self.selected = None;
        self.error = None;
        self.cache.clear();
        self.cache_key = None;

        match self.compute_diff() {
            Ok((branch, files)) => {
                self.branch = branch;
                self.files = files;
                if !self.files.is_empty() {
                    self.selected = Some(0);
                }
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    fn compute_diff(&self) -> Result<(String, Vec<ChangedFile>), git2::Error> {
        let repo = Repository::discover(&self.repo_path)?;
        let branch = repo
            .head()
            .ok()
            .and_then(|h| h.shorthand().map(String::from))
            .unwrap_or_else(|| "(detached)".into());
        let head_tree = repo.head().ok().and_then(|h| h.peel_to_tree().ok());

        let mut opts = DiffOptions::new();
        opts.context_lines(3)
            .include_untracked(true)
            .recurse_untracked_dirs(true);

        let diff: Diff =
            repo.diff_tree_to_workdir_with_index(head_tree.as_ref(), Some(&mut opts))?;

        use std::cell::RefCell;
        let files: RefCell<Vec<ChangedFile>> = RefCell::new(Vec::new());

        diff.print(DiffFormat::Patch, |delta, _hunk, line| {
            let path = delta
                .new_file()
                .path()
                .or_else(|| delta.old_file().path())
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "<unknown>".into());

            let mut files = files.borrow_mut();
            if files.last().map(|f| f.path != path).unwrap_or(true) {
                files.push(ChangedFile {
                    path: path.clone(),
                    diff_rows: Vec::new(),
                });
            }
            let content = String::from_utf8_lossy(line.content())
                .trim_end_matches('\n')
                .to_string();
            let kind = match line.origin() {
                '+' => LineKind::Add,
                '-' => LineKind::Del,
                'H' => LineKind::Hunk,
                'F' => return true,
                _ => LineKind::Ctx,
            };
            files
                .last_mut()
                .unwrap()
                .diff_rows
                .push(DiffLineRow { kind, text: content });
            true
        })?;

        Ok((branch, files.into_inner()))
    }

    /// Read the full working-tree file for the selected path.
    fn read_full_file(&self, rel: &str) -> std::io::Result<String> {
        let repo_root = Repository::discover(&self.repo_path)
            .ok()
            .and_then(|r| r.workdir().map(|w| w.to_path_buf()))
            .unwrap_or_else(|| self.repo_path.clone());
        std::fs::read_to_string(repo_root.join(rel))
    }

    /// Rebuild the highlighted render cache if the selection / view changed.
    fn ensure_cache(&mut self) {
        let Some(idx) = self.selected else {
            self.cache.clear();
            self.cache_key = None;
            return;
        };
        let key = (idx, self.view);
        if self.cache_key == Some(key) {
            return;
        }

        let path = self.files[idx].path.clone();
        let mut out: Vec<(Option<LineKind>, Vec<(Color32, String)>)> = Vec::new();

        match self.view {
            ViewMode::Diff => {
                let rows = self.files[idx].diff_rows.clone();
                for r in rows {
                    if r.kind == LineKind::Hunk {
                        out.push((
                            Some(LineKind::Hunk),
                            vec![(Color32::from_rgb(120, 160, 220), r.text)],
                        ));
                    } else {
                        let spans = self.hl.highlight_line(&path, &r.text);
                        out.push((Some(r.kind), spans));
                    }
                }
            }
            ViewMode::FullFile => match self.read_full_file(&path) {
                Ok(content) => {
                    for line in content.lines() {
                        let spans = self.hl.highlight_line(&path, line);
                        out.push((None, spans));
                    }
                }
                Err(e) => {
                    out.push((None, vec![(Color32::LIGHT_RED, format!("cannot read file: {e}"))]));
                }
            },
        }

        self.cache = out;
        self.cache_key = Some(key);
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("purview");
                ui.separator();
                ui.label(format!("repo: {}", self.repo_path.to_string_lossy()));
                ui.separator();
                ui.label(format!("branch: {}", self.branch));
                ui.separator();
                ui.label("session: (none)");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("⟳").clicked() {
                        self.reload();
                    }
                    ui.selectable_value(&mut self.view, ViewMode::FullFile, "Full File");
                    ui.selectable_value(&mut self.view, ViewMode::Diff, "Diff");
                });
            });
        });

        egui::SidePanel::left("files")
            .resizable(true)
            .default_width(300.0)
            .show(ctx, |ui| {
                ui.add_space(4.0);
                ui.label(egui::RichText::new(format!("Changed ({})", self.files.len())).strong());
                if let Some(err) = &self.error {
                    ui.colored_label(Color32::LIGHT_RED, err);
                    return;
                }
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for i in 0..self.files.len() {
                        let selected = self.selected == Some(i);
                        let label = self.files[i].path.clone();
                        if ui.selectable_label(selected, label).clicked() {
                            self.selected = Some(i);
                        }
                    }
                });
            });

        self.ensure_cache();

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.selected.is_none() {
                ui.centered_and_justified(|ui| {
                    ui.label("no changes — working tree matches HEAD")
                });
                return;
            }

            let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
            let total = self.cache.len();
            egui::ScrollArea::both().auto_shrink([false, false]).show_rows(
                ui,
                row_h,
                total,
                |ui, range| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for i in range {
                        let (kind, spans) = &self.cache[i];
                        let bg = match kind {
                            Some(LineKind::Add) => Some(Color32::from_rgb(22, 50, 22)),
                            Some(LineKind::Del) => Some(Color32::from_rgb(55, 22, 22)),
                            _ => None,
                        };
                        let gutter = match kind {
                            Some(LineKind::Add) => "+ ",
                            Some(LineKind::Del) => "- ",
                            Some(LineKind::Hunk) => "",
                            _ => "  ",
                        };
                        let draw = |ui: &mut egui::Ui| {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 0.0;
                                if !gutter.is_empty() {
                                    ui.label(
                                        egui::RichText::new(gutter)
                                            .monospace()
                                            .color(Color32::DARK_GRAY),
                                    );
                                }
                                for (color, text) in spans {
                                    ui.label(egui::RichText::new(text).monospace().color(*color));
                                }
                            });
                        };
                        if let Some(bg) = bg {
                            egui::Frame::none().fill(bg).show(ui, draw);
                        } else {
                            draw(ui);
                        }
                    }
                },
            );
        });
    }
}
