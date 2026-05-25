//! purview — codebase-first code review.
//!
//! v0: open a git repo, list changed files (working tree vs HEAD) in the
//! left panel, render the selected file's diff with +/- coloring in the
//! right. Proves the core loop; the "codebase-first, diff-as-overlay"
//! model and LSP/agent panes come later.

use std::path::PathBuf;

use eframe::egui;
use git2::{Diff, DiffFormat, DiffOptions, Repository};

fn main() -> eframe::Result<()> {
    let repo_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap());

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_title("purview"),
        ..Default::default()
    };

    eframe::run_native(
        "purview",
        native_options,
        Box::new(move |_cc| Ok(Box::new(App::new(repo_path)))),
    )
}

/// One changed file plus its rendered diff lines.
struct ChangedFile {
    path: String,
    lines: Vec<DiffLine>,
}

#[derive(Clone)]
enum DiffLine {
    Add(String),
    Del(String),
    Ctx(String),
    Hunk(String),
}

struct App {
    repo_path: PathBuf,
    files: Vec<ChangedFile>,
    selected: Option<usize>,
    error: Option<String>,
}

impl App {
    fn new(repo_path: PathBuf) -> Self {
        let mut app = App {
            repo_path,
            files: Vec::new(),
            selected: None,
            error: None,
        };
        app.reload();
        app
    }

    /// Recompute the working-tree-vs-HEAD diff.
    fn reload(&mut self) {
        self.files.clear();
        self.selected = None;
        self.error = None;

        match self.compute_diff() {
            Ok(files) => {
                self.files = files;
                if !self.files.is_empty() {
                    self.selected = Some(0);
                }
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    fn compute_diff(&self) -> Result<Vec<ChangedFile>, git2::Error> {
        let repo = Repository::discover(&self.repo_path)?;
        let head_tree = repo.head().ok().and_then(|h| h.peel_to_tree().ok());

        let mut opts = DiffOptions::new();
        opts.context_lines(3)
            .include_untracked(true)
            .recurse_untracked_dirs(true);

        let diff: Diff =
            repo.diff_tree_to_workdir_with_index(head_tree.as_ref(), Some(&mut opts))?;

        // Collect per-file diff lines by walking the diff print callback.
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
                    lines: Vec::new(),
                });
            }
            let content = String::from_utf8_lossy(line.content())
                .trim_end_matches('\n')
                .to_string();
            let dl = match line.origin() {
                '+' => DiffLine::Add(content),
                '-' => DiffLine::Del(content),
                'H' => DiffLine::Hunk(content),
                'F' => return true, // file header — skip; we key on delta path
                _ => DiffLine::Ctx(content),
            };
            files.last_mut().unwrap().lines.push(dl);
            true
        })?;

        Ok(files.into_inner())
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("purview");
                ui.label(self.repo_path.to_string_lossy());
                if ui.button("⟳ reload").clicked() {
                    self.reload();
                }
                ui.label(format!("{} changed", self.files.len()));
            });
        });

        egui::SidePanel::left("files")
            .resizable(true)
            .default_width(280.0)
            .show(ctx, |ui| {
                ui.add_space(4.0);
                if let Some(err) = &self.error {
                    ui.colored_label(egui::Color32::LIGHT_RED, err);
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

        egui::CentralPanel::default().show(ctx, |ui| {
            let Some(idx) = self.selected else {
                ui.centered_and_justified(|ui| {
                    ui.label("no changes — working tree matches HEAD")
                });
                return;
            };
            let file = &self.files[idx];
            egui::ScrollArea::both()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let mono = egui::TextStyle::Monospace;
                    for line in &file.lines {
                        let (text, color) = match line {
                            DiffLine::Add(s) => {
                                (format!("+ {s}"), egui::Color32::from_rgb(120, 200, 120))
                            }
                            DiffLine::Del(s) => {
                                (format!("- {s}"), egui::Color32::from_rgb(220, 120, 120))
                            }
                            DiffLine::Hunk(s) => {
                                (s.clone(), egui::Color32::from_rgb(120, 160, 220))
                            }
                            DiffLine::Ctx(s) => {
                                (format!("  {s}"), egui::Color32::GRAY)
                            }
                        };
                        ui.label(
                            egui::RichText::new(text)
                                .text_style(mono.clone())
                                .color(color),
                        );
                    }
                });
        });
    }
}
