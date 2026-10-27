//! sprig-ui: a desktop window on a sprig repository. History with diffs,
//! working changes and commit, branches, merging and resolving conflicts,
//! and syncing with a sprig server or GitHub.
//!
//! Every action runs the same code as the matching `sprig` command (in a
//! background thread, so the window never freezes), so it behaves the same
//! and is recorded in the operation log: `sprig undo` undoes it.
//!
//!     sprig-ui [repository]

use eframe::egui::{self, Color32, RichText, TextFormat, text::LayoutJob};
use sprig::store::{Change, Conflict, Repo, short};
use sprig::tree::{Delta, RENAME_THRESHOLD};
use sprig::util::{Error, fmt_date};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc;

fn main() -> eframe::Result {
    let dir = std::env::args().nth(1).map_or_else(|| std::env::current_dir().unwrap(), PathBuf::from);
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1200.0, 760.0]).with_title("sprig"),
        ..Default::default()
    };
    eframe::run_native("sprig", opts, Box::new(|_| Ok(Box::new(App::open(dir)))))
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Changes,
    History,
    Conflicts,
}

/// The finished background action: its name, and its output or error.
type Done = (String, Result<String, String>);

struct App {
    repo: Option<Repo>,
    error: String, // why there is no repo
    tab: Tab,
    // what refresh reads
    branch: String,
    head: String,
    branches: Vec<String>,
    remotes: Vec<String>,
    github: String,
    changes: Vec<Delta>,
    conflicts: Vec<Conflict>,
    history: Vec<(String, Change)>,
    // what the user picked or typed
    selected: Option<usize>,
    detail: String,
    message: String,
    merge_from: String,
    new_branch: String,
    github_edit: String,
    editor: String,
    // the running action, and the last result
    job: Option<(String, mpsc::Receiver<Done>)>,
    status: Result<String, String>,
    focused: bool,
}

impl App {
    fn open(dir: PathBuf) -> App {
        let mut app = App {
            repo: None,
            error: String::new(),
            tab: Tab::Changes,
            branch: String::new(),
            head: String::new(),
            branches: vec![],
            remotes: vec![],
            github: String::new(),
            changes: vec![],
            conflicts: vec![],
            history: vec![],
            selected: None,
            detail: String::new(),
            message: String::new(),
            merge_from: String::new(),
            new_branch: String::new(),
            github_edit: String::new(),
            editor: String::new(),
            job: None,
            status: Ok(String::new()),
            focused: true,
        };
        // Actions run the CLI's code, which finds the repository from the
        // current directory.
        match std::env::set_current_dir(&dir).map_err(Error::from).and_then(|_| Repo::find()) {
            Ok(r) => {
                app.repo = Some(r);
                app.refresh();
            }
            Err(e) => app.error = format!("{}: {e}", dir.display()),
        }
        app
    }

    /// Reads the repository again: branch, changes, conflicts, history.
    fn refresh(&mut self) {
        let Some(r) = Repo::find().ok() else { return };
        let res = (|| -> sprig::util::Result<()> {
            let (rf, head) = r.head();
            self.branch = rf.strip_prefix("heads/").unwrap_or("(detached)").to_string();
            self.head = head.clone();
            self.branches = r.list_refs().keys().filter_map(|n| n.strip_prefix("heads/")).map(String::from).collect();
            let cfg = r.load_config();
            self.remotes = cfg.remotes.keys().cloned().collect();
            self.github = cfg.github.clone();
            if self.github_edit.is_empty() {
                self.github_edit = cfg.github;
            }
            let ds = r.working_deltas()?;
            let work: HashSet<String> = ds.iter().map(|d| d.path.clone()).collect();
            self.changes = r.with_renames_at(ds, RENAME_THRESHOLD, Some(&work));
            self.conflicts = if head.is_empty() { vec![] } else { r.get_change(&head)?.conflicts };
            self.history.clear();
            if !head.is_empty() {
                r.walk(&head, |id, c| {
                    self.history.push((id.to_string(), c.clone()));
                    self.history.len() < 500
                })?;
            }
            Ok(())
        })();
        if let Err(e) = res {
            self.status = Err(e.to_string());
        }
        self.repo = Some(r);
        self.select(self.selected);
    }

    /// Picks a row in the current tab and loads what it shows.
    fn select(&mut self, i: Option<usize>) {
        self.selected = i;
        let Some(r) = &self.repo else { return };
        let mut out = vec![];
        let res = match (self.tab, i) {
            (Tab::Changes, None) => {
                let work: HashSet<String> = self.changes.iter().map(|d| d.path.clone()).collect();
                sprig::cli::render_diff(r, &mut out, self.changes.clone(), RENAME_THRESHOLD, Some(&work))
            }
            (Tab::Changes, Some(i)) => match self.changes.get(i) {
                Some(d) => {
                    let work = HashSet::from([d.path.clone()]);
                    sprig::cli::render_diff(r, &mut out, vec![d.clone()], 0, Some(&work))
                }
                None => Ok(()),
            },
            (Tab::History, Some(i)) => match self.history.get(i) {
                Some((id, _)) => sprig::cli::run("show", std::slice::from_ref(id), &mut out),
                None => Ok(()),
            },
            (Tab::Conflicts, Some(i)) => {
                if let Some(cf) = self.conflicts.get(i) {
                    self.editor = std::fs::read(r.abs(&cf.path))
                        .map(|b| String::from_utf8_lossy(&b).into_owned())
                        .unwrap_or_default();
                }
                Ok(())
            }
            _ => Ok(()),
        };
        self.detail = match res {
            Ok(()) => String::from_utf8_lossy(&out).into_owned(),
            Err(e) => format!("error: {e}"),
        };
    }

    /// Runs a sprig command in the background.
    fn run(&mut self, ctx: &egui::Context, name: &str, args: &[&str]) {
        let (cmd, args): (String, Vec<String>) = (args[0].into(), args[1..].iter().map(|s| s.to_string()).collect());
        self.job_with(ctx, name, move || {
            let mut out = vec![];
            let res = sprig::cli::run(&cmd, &args, &mut out);
            let text = String::from_utf8_lossy(&out).trim().to_string();
            match res {
                Ok(()) => Ok(text),
                Err(Error::Exit(3, _)) => Ok(text), // a merge that recorded conflicts
                Err(Error::Exit(_, m)) if m.is_empty() => Err(text),
                Err(e) => Err(e.to_string()),
            }
        });
    }

    fn job_with(
        &mut self,
        ctx: &egui::Context,
        name: &str,
        f: impl FnOnce() -> Result<String, String> + Send + 'static,
    ) {
        if self.job.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let (name2, ctx) = (name.to_string(), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send((name2, f()));
            ctx.request_repaint();
        });
        self.job = Some((name.to_string(), rx));
    }

    fn poll(&mut self) {
        let Some((_, rx)) = &self.job else { return };
        if let Ok((name, res)) = rx.try_recv() {
            self.job = None;
            self.status = match res {
                Ok(t) if t.is_empty() => Ok(format!("{name}: done")),
                Ok(t) => Ok(format!("{name}: {t}")),
                Err(e) => Err(format!("{name}: {e}")),
            };
            if name == "commit" && self.status.is_ok() {
                self.message.clear();
            }
            self.refresh();
        }
    }

    fn busy(&self) -> bool {
        self.job.is_some()
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("sprig").strong());
            ui.separator();
            ui.label("branch");
            let mut pick = None;
            egui::ComboBox::from_id_salt("branch").selected_text(&self.branch).show_ui(ui, |ui| {
                for b in &self.branches {
                    if ui.selectable_label(*b == self.branch, b).clicked() && *b != self.branch {
                        pick = Some(b.clone());
                    }
                }
            });
            if let Some(b) = pick {
                self.run(&ctx, &format!("checkout {b}"), &["checkout", &b]);
            }
            ui.add(egui::TextEdit::singleline(&mut self.new_branch).hint_text("new branch").desired_width(110.0));
            if ui.add_enabled(!self.busy() && !self.new_branch.is_empty(), egui::Button::new("Create")).clicked() {
                let b = std::mem::take(&mut self.new_branch);
                self.run(&ctx, &format!("checkout -b {b}"), &["checkout", "-b", &b]);
            }
            ui.separator();
            egui::ComboBox::from_id_salt("merge")
                .selected_text(if self.merge_from.is_empty() { "merge from…" } else { &self.merge_from })
                .show_ui(ui, |ui| {
                    for b in self.branches.iter().filter(|b| **b != self.branch) {
                        ui.selectable_value(&mut self.merge_from, b.clone(), b);
                    }
                });
            if ui.add_enabled(!self.busy() && !self.merge_from.is_empty(), egui::Button::new("Merge")).clicked() {
                let b = std::mem::take(&mut self.merge_from);
                self.run(&ctx, &format!("merge {b}"), &["merge", &b]);
            }
            ui.separator();
            for rm in self.remotes.clone() {
                if ui.add_enabled(!self.busy(), egui::Button::new(format!("Pull {rm}"))).clicked() {
                    self.run(&ctx, &format!("pull {rm}"), &["pull", &rm]);
                }
                if ui.add_enabled(!self.busy(), egui::Button::new(format!("Push {rm}"))).clicked() {
                    self.run(&ctx, &format!("push {rm}"), &["push", &rm]);
                }
            }
            ui.separator();
            ui.label("GitHub");
            ui.add(
                egui::TextEdit::singleline(&mut self.github_edit)
                    .hint_text("https://github.com/you/repo.git")
                    .desired_width(220.0),
            );
            if self.github_edit.trim() != self.github {
                if ui
                    .add_enabled(!self.busy() && !self.github_edit.trim().is_empty(), egui::Button::new("Save"))
                    .clicked()
                {
                    let u = self.github_edit.trim().to_string();
                    self.run(&ctx, "github set", &["github", "set", &u]);
                }
            } else {
                let can = !self.busy() && !self.github.is_empty();
                if ui.add_enabled(can, egui::Button::new("Pull")).on_hover_text("fetch from GitHub and merge").clicked()
                {
                    self.run(&ctx, "GitHub pull", &["github", "pull"]);
                }
                if ui
                    .add_enabled(can, egui::Button::new("Push"))
                    .on_hover_text("push the current branch to GitHub")
                    .clicked()
                {
                    self.run(&ctx, "GitHub push", &["github", "push"]);
                }
            }
            ui.separator();
            if ui
                .add_enabled(!self.busy(), egui::Button::new("Undo"))
                .on_hover_text("undo the last operation (again = redo)")
                .clicked()
            {
                self.run(&ctx, "undo", &["undo"]);
            }
            if ui.add_enabled(!self.busy(), egui::Button::new("Refresh")).clicked() {
                self.refresh();
            }
        });
    }

    fn list(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let mut tab = self.tab;
            ui.selectable_value(&mut tab, Tab::Changes, format!("Changes ({})", self.changes.len()));
            ui.selectable_value(&mut tab, Tab::History, "History");
            let label = format!("Conflicts ({})", self.conflicts.len());
            let label = if self.conflicts.is_empty() {
                RichText::new(label)
            } else {
                RichText::new(label).color(Color32::from_rgb(220, 80, 60))
            };
            ui.selectable_value(&mut tab, Tab::Conflicts, label);
            if tab != self.tab {
                self.tab = tab;
                self.select(if tab == Tab::History && !self.history.is_empty() { Some(0) } else { None });
            }
        });
        ui.separator();
        let mut pick = None;
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| match self.tab {
            Tab::Changes => {
                if self.changes.is_empty() {
                    ui.weak("working tree clean");
                }
                for (i, d) in self.changes.iter().enumerate() {
                    let text = if d.kind == 'R' {
                        format!("R  {} → {}", d.from, d.path)
                    } else {
                        format!("{}  {}", d.kind, d.path)
                    };
                    let color = match d.kind {
                        'A' => Color32::from_rgb(70, 160, 80),
                        'D' => Color32::from_rgb(210, 80, 70),
                        _ => Color32::from_rgb(200, 150, 50),
                    };
                    if ui
                        .selectable_label(self.selected == Some(i), RichText::new(text).monospace().color(color))
                        .clicked()
                    {
                        pick = Some(if self.selected == Some(i) { None } else { Some(i) });
                    }
                }
            }
            Tab::History => {
                for (i, (id, c)) in self.history.iter().enumerate() {
                    let first = c.message.lines().next().unwrap_or("");
                    let who = if c.meta.agent.is_empty() {
                        c.author.split(" <").next().unwrap_or("").to_string()
                    } else {
                        format!("🤖 {}", c.meta.agent)
                    };
                    let mut text = format!("{}  {first}", short(id));
                    if c.parents.len() > 1 {
                        text += "  (merge)";
                    }
                    let r = ui.selectable_label(self.selected == Some(i), RichText::new(text).monospace());
                    let r = r.on_hover_text(format!("{who}\n{}", fmt_date(c.time)));
                    if r.clicked() {
                        pick = Some(Some(i));
                    }
                }
            }
            Tab::Conflicts => {
                if self.conflicts.is_empty() {
                    ui.weak("no unresolved conflicts");
                }
                for (i, cf) in self.conflicts.iter().enumerate() {
                    if ui
                        .selectable_label(
                            self.selected == Some(i),
                            RichText::new(format!("C  {}", cf.path)).monospace(),
                        )
                        .clicked()
                    {
                        pick = Some(Some(i));
                    }
                }
            }
        });
        if let Some(p) = pick {
            self.select(p);
        }
    }

    fn commit_box(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.add(
            egui::TextEdit::multiline(&mut self.message)
                .hint_text("commit message")
                .desired_rows(3)
                .desired_width(f32::INFINITY),
        );
        ui.horizontal(|ui| {
            let can = !self.busy()
                && !self.message.trim().is_empty()
                && (!self.changes.is_empty() || !self.conflicts.is_empty());
            let what = match self.selected.and_then(|i| self.changes.get(i)) {
                Some(d) if self.tab == Tab::Changes => d.path.clone(),
                _ => String::new(),
            };
            let label = if what.is_empty() { "Commit all".to_string() } else { format!("Commit {what}") };
            if ui.add_enabled(can, egui::Button::new(label)).clicked() {
                let msg = self.message.trim().to_string();
                let mut args = vec!["commit", "-m", &msg];
                if self.changes.is_empty() {
                    args.push("--allow-empty"); // recording a conflict resolution that matches HEAD
                }
                if !what.is_empty() {
                    args.push(&what);
                }
                self.run(&ctx, "commit", &args);
            }
        });
    }

    fn conflict_view(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let Some(cf) = self.selected.and_then(|i| self.conflicts.get(i)).cloned() else {
            ui.weak("Pick a conflicted file. Take one side, or edit the result below and save it; then commit.");
            return;
        };
        let Some(r) = &self.repo else { return };
        let text = |id: &str| -> String {
            if id.is_empty() {
                return "(no file on this side)".into();
            }
            r.get_blob(id).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_else(|e| format!("({e})"))
        };
        let sides = [("ours", text(&cf.ours)), ("base", text(&cf.base)), ("theirs", text(&cf.theirs))];
        let abs = r.abs(&cf.path);
        ui.heading(&cf.path);
        let mut take = None;
        ui.columns(3, |cols| {
            for (col, (name, t)) in cols.iter_mut().zip(&sides) {
                col.horizontal(|ui| {
                    ui.strong(*name);
                    if ui.add_enabled(!self.busy(), egui::Button::new(format!("Use {name}"))).clicked() {
                        take = Some(*name);
                    }
                });
                egui::ScrollArea::both().id_salt(*name).max_height(220.0).show(col, |ui| {
                    ui.add(egui::Label::new(RichText::new(t).monospace()).extend());
                });
            }
        });
        if let Some(side) = take {
            self.run(&ctx, &format!("resolve --{side}"), &["resolve", &format!("--{side}"), &cf.path]);
        }
        ui.separator();
        ui.horizontal(|ui| {
            ui.strong("result in the working tree");
            if ui.add_enabled(!self.busy(), egui::Button::new("Save")).clicked() {
                let (p, data) = (abs.clone(), self.editor.clone());
                self.job_with(&ctx, &format!("save {}", cf.path), move || {
                    std::fs::write(p, data).map(|_| String::new()).map_err(|e| e.to_string())
                });
            }
            if self.editor.contains("<<<<<<<") {
                ui.colored_label(Color32::from_rgb(220, 80, 60), "still has conflict markers");
            }
        });
        egui::ScrollArea::vertical().id_salt("editor").show(ui, |ui| {
            ui.add(
                egui::TextEdit::multiline(&mut self.editor).code_editor().desired_width(f32::INFINITY).desired_rows(16),
            );
        });
    }
}

/// Colors a diff or `show` output line by line.
fn diff_job(text: &str, ui: &egui::Ui) -> LayoutJob {
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    let base = ui.visuals().text_color();
    let mut job = LayoutJob::default();
    for line in text.split_inclusive('\n').take(20000) {
        let color = if line.starts_with("+++") || line.starts_with("---") {
            base
        } else if line.starts_with('+') {
            Color32::from_rgb(70, 160, 80)
        } else if line.starts_with('-') {
            Color32::from_rgb(210, 80, 70)
        } else if line.starts_with("@@") {
            Color32::from_rgb(90, 140, 220)
        } else if line.starts_with("change ")
            || line.starts_with("added:")
            || line.starts_with("deleted:")
            || line.starts_with("renamed:")
        {
            Color32::from_rgb(200, 150, 50)
        } else {
            base
        };
        job.append(line, 0.0, TextFormat::simple(font.clone(), color));
    }
    job
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll();
        // look again when the window comes back to the front: files may have changed
        let focused = ui.ctx().input(|i| i.focused);
        if focused && !self.focused && !self.busy() {
            self.refresh();
        }
        self.focused = focused;
        if self.repo.is_none() {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.heading("not a sprig repository");
                ui.label(&self.error);
                ui.label("Run `sprig init` there (or `sprig init` + `sprig github set <url>` + Pull), or start sprig-ui with a repository path.");
            });
            return;
        }
        egui::Panel::top("top").show(ui, |ui| self.top_bar(ui));
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                if let Some((name, _)) = &self.job {
                    ui.spinner();
                    ui.label(format!("{name}…"));
                } else {
                    match &self.status {
                        Ok(s) => ui.label(s.lines().last().unwrap_or("")).on_hover_text(s),
                        Err(e) => ui
                            .colored_label(Color32::from_rgb(220, 80, 60), e.lines().next().unwrap_or(""))
                            .on_hover_text(e),
                    };
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.weak(if self.head.is_empty() {
                        "no changes yet".to_string()
                    } else {
                        format!("{} at {}", self.branch, short(&self.head))
                    });
                });
            });
        });
        egui::Panel::left("list").resizable(true).default_size(380.0).show(ui, |ui| {
            if self.tab == Tab::Changes {
                egui::Panel::bottom("commit").show(ui, |ui| self.commit_box(ui));
            }
            self.list(ui);
        });
        egui::CentralPanel::default().show(ui, |ui| {
            if self.tab == Tab::Conflicts {
                self.conflict_view(ui);
                return;
            }
            egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
                if self.detail.is_empty() {
                    ui.weak(if self.tab == Tab::History { "pick a change" } else { "no changes" });
                } else {
                    ui.add(egui::Label::new(diff_job(&self.detail, ui)).extend());
                }
            });
        });
    }
}
