//! sprig-ui: a desktop window on a sprig repository. History with diffs,
//! working changes and commit, branches, merging and resolving conflicts,
//! and syncing with a sprig server or GitHub.
//!
//! Every action runs the same code as the matching `sprig` command (in a
//! background thread, so the window never freezes), so it behaves the same
//! and is recorded in the operation log: `sprig undo` undoes it.
//!
//!     sprig-ui [repository]
//!
//! The look follows GitHub, GitLab and Cursor's source-control screens on
//! Mobbin (see `theme` and the README): a light neutral canvas, bordered
//! cards, a dark primary button, +N −N counts and tinted diff lines with
//! line-number gutters.

use eframe::egui::{self, Align2, Color32, CornerRadius, FontId, Margin, RichText, Sense, Stroke, Vec2};
use sprig::store::{Change, Conflict, Repo, short};
use sprig::tree::{Delta, RENAME_THRESHOLD};
use sprig::util::{Error, fmt_date};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc;

fn main() -> eframe::Result {
    let dir = std::env::args().nth(1).map_or_else(|| std::env::current_dir().unwrap(), PathBuf::from);
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1280.0, 800.0]).with_title("sprig"),
        ..Default::default()
    };
    eframe::run_native(
        "sprig",
        opts,
        Box::new(|cc| {
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(App::open(dir)))
        }),
    )
}

/// The design system: every color, radius and spacing the window uses.
mod theme {
    use super::*;

    const fn hex(c: u32) -> Color32 {
        Color32::from_rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
    }
    pub const BG: Color32 = hex(0xffffff);
    pub const SURFACE: Color32 = hex(0xf6f8fa); // panels, card headers
    pub const BORDER: Color32 = hex(0xd1d9e0);
    pub const TEXT: Color32 = hex(0x1f2328);
    pub const MUTED: Color32 = hex(0x59636e);
    pub const ACCENT: Color32 = hex(0x0969da);
    pub const SELECTED: Color32 = hex(0xddf4ff);
    pub const PRIMARY: Color32 = hex(0x1f2328); // Cursor's dark "Squash and merge"
    pub const ADD: Color32 = hex(0x1a7f37);
    pub const ADD_BG: Color32 = hex(0xdafbe1);
    pub const ADD_NUM_BG: Color32 = hex(0xaceebb);
    pub const DEL: Color32 = hex(0xd1242f);
    pub const DEL_BG: Color32 = hex(0xffebe9);
    pub const DEL_NUM_BG: Color32 = hex(0xffcecb);
    pub const HUNK_BG: Color32 = hex(0xddf4ff);
    pub const WARN: Color32 = hex(0x9a6700);
    pub const WARN_BG: Color32 = hex(0xfff8c5);
    pub const RADIUS: u8 = 6;
    pub const MONO: f32 = 12.5;

    pub fn apply(ctx: &egui::Context) {
        ctx.set_theme(egui::Theme::Light);
        ctx.all_styles_mut(|s| {
            s.spacing.item_spacing = Vec2::new(8.0, 6.0);
            s.spacing.button_padding = Vec2::new(10.0, 5.0);
            s.spacing.interact_size.y = 28.0;
            s.text_styles.insert(egui::TextStyle::Monospace, FontId::monospace(MONO));
            s.text_styles.insert(egui::TextStyle::Body, FontId::proportional(13.5));
            s.text_styles.insert(egui::TextStyle::Button, FontId::proportional(13.5));
            let v = &mut s.visuals;
            v.panel_fill = BG;
            v.window_fill = BG;
            v.extreme_bg_color = BG;
            v.faint_bg_color = SURFACE;
            v.code_bg_color = SURFACE;
            v.override_text_color = Some(TEXT);
            v.hyperlink_color = ACCENT;
            v.selection.bg_fill = SELECTED;
            v.selection.stroke = Stroke::new(1.0, ACCENT);
            v.window_stroke = Stroke::new(1.0, BORDER);
            v.window_corner_radius = CornerRadius::same(RADIUS + 2);
            v.menu_corner_radius = CornerRadius::same(RADIUS);
            let w = &mut v.widgets;
            for (st, fill) in [
                (&mut w.noninteractive, BG),
                (&mut w.inactive, BG),
                (&mut w.hovered, SURFACE),
                (&mut w.active, hex(0xeff2f5)),
                (&mut w.open, SURFACE),
            ] {
                st.bg_fill = fill;
                st.weak_bg_fill = fill;
                st.corner_radius = CornerRadius::same(RADIUS);
                st.bg_stroke = Stroke::new(1.0, BORDER);
                st.fg_stroke = Stroke::new(1.0, TEXT);
                st.expansion = 0.0;
            }
            w.hovered.bg_stroke = Stroke::new(1.0, hex(0xb7c0c9));
        });
    }

    /// A bordered card, as GitHub and GitLab put each file in.
    pub fn card() -> egui::Frame {
        egui::Frame::new()
            .fill(BG)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(RADIUS)
            .inner_margin(Margin::same(10))
    }

    /// The dark call-to-action button: commit, push, save.
    pub fn primary(text: &str) -> egui::Button<'static> {
        egui::Button::new(RichText::new(text.to_string()).color(Color32::WHITE).strong()).fill(PRIMARY)
    }

    /// GitLab's "+23 −0".
    pub fn counts(ui: &mut egui::Ui, add: usize, del: usize) {
        ui.label(RichText::new(format!("+{add}")).color(ADD).monospace());
        ui.label(RichText::new(format!("−{del}")).color(DEL).monospace());
    }

    /// A small rounded label: a change kind, "merge", an agent.
    pub fn pill(ui: &mut egui::Ui, text: &str, fg: Color32, bg: Color32) {
        egui::Frame::new().fill(bg).corner_radius(10).inner_margin(Margin::symmetric(7, 1)).show(ui, |ui| {
            ui.label(RichText::new(text).color(fg).size(11.5).strong());
        });
    }

    pub fn kind(k: char) -> (&'static str, Color32, Color32) {
        match k {
            'A' => ("A", ADD, ADD_BG),
            'D' => ("D", DEL, DEL_BG),
            'R' => ("R", ACCENT, SELECTED),
            'C' => ("C", WARN, WARN_BG),
            _ => ("M", WARN, WARN_BG),
        }
    }

    /// Front's "1 conflict found" strip.
    pub fn banner(ui: &mut egui::Ui, text: &str) {
        egui::Frame::new()
            .fill(WARN_BG)
            .stroke(Stroke::new(1.0, hex(0xeed888)))
            .corner_radius(RADIUS)
            .inner_margin(Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(text).color(WARN));
            });
    }

    /// A list row that is one big click target, with GitHub's blue
    /// selection and a left accent bar.
    pub fn row(ui: &mut egui::Ui, selected: bool, add: impl FnOnce(&mut egui::Ui)) -> egui::Response {
        let fill = if selected { SELECTED } else { Color32::TRANSPARENT };
        let frame = egui::Frame::new().fill(fill).corner_radius(RADIUS).inner_margin(Margin::symmetric(10, 6));
        let inner = frame.show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        });
        let resp = ui.interact(inner.response.rect, inner.response.id.with("row"), Sense::click());
        if resp.hovered() && !selected {
            ui.painter().rect_filled(resp.rect, RADIUS, Color32::from_black_alpha(8));
        }
        if selected {
            let r = resp.rect;
            ui.painter().rect_filled(
                egui::Rect::from_min_size(r.min, Vec2::new(3.0, r.height())),
                CornerRadius { nw: RADIUS, sw: RADIUS, ne: 0, se: 0 },
                ACCENT,
            );
        }
        resp
    }
}

use theme::*;

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Changes,
    History,
    Conflicts,
}

/// One line of a rendered diff, ready to paint.
enum Row {
    File(String),
    Note(String),
    Hunk(String),
    /// A unified line: old number, new number, ' ' '+' or '-', text.
    Line(Option<u32>, Option<u32>, u8, String),
    /// A side-by-side line: left (old) and right (new).
    Split(Option<(u32, u8, String)>, Option<(u32, u8, String)>),
}

/// A diff parsed once for both views, with +/- counts per file.
#[derive(Default)]
struct Diff {
    unified: Vec<Row>,
    split: Vec<Row>,
    counts: HashMap<String, (usize, usize)>,
}

type Run = Vec<(u32, String)>;

impl Diff {
    fn parse(text: &str) -> Diff {
        let mut d = Diff::default();
        let (mut old, mut new, mut file) = (0u32, 0u32, String::new());
        // the pending "-" and "+" run, paired up for the split view
        let (mut dels, mut adds): (Run, Run) = (vec![], vec![]);
        let flush = |split: &mut Vec<Row>, dels: &mut Run, adds: &mut Run| {
            let n = dels.len().max(adds.len());
            let (mut a, mut b) = (dels.drain(..), adds.drain(..));
            for _ in 0..n {
                split.push(Row::Split(a.next().map(|(n, t)| (n, b'-', t)), b.next().map(|(n, t)| (n, b'+', t))));
            }
        };
        let both = |d: &mut Diff, r: fn(String) -> Row, s: String| {
            d.unified.push(r(s.clone()));
            d.split.push(r(s));
        };
        let mut lines = text.lines().take(50_000);
        while let Some(line) = lines.next() {
            let line = line.replace('\t', "    ");
            if let Some(p) = line.strip_prefix("--- a/") {
                flush(&mut d.split, &mut dels, &mut adds);
                lines.next(); // +++ b/…
                file = p.to_string();
                both(&mut d, Row::File, file.clone());
                continue;
            }
            if let Some(h) = line.strip_prefix("@@ ") {
                flush(&mut d.split, &mut dels, &mut adds);
                let num = |s: &str| s.get(1..).and_then(|s| s.split(',').next()?.parse().ok()).unwrap_or(1u32);
                let mut parts = h.split(' ');
                old = num(parts.next().unwrap_or(""));
                new = num(parts.next().unwrap_or(""));
                both(&mut d, Row::Hunk, line.clone());
                continue;
            }
            let op = match line.as_bytes().first() {
                Some(&c @ (b'+' | b'-' | b' ')) if !file.is_empty() => c,
                _ => {
                    flush(&mut d.split, &mut dels, &mut adds);
                    if !line.is_empty() && !line.starts_with('\\') {
                        both(&mut d, Row::Note, line);
                    }
                    continue;
                }
            };
            let rest = line[1..].to_string();
            let c = d.counts.entry(file.clone()).or_default();
            match op {
                b'+' => {
                    c.0 += 1;
                    d.unified.push(Row::Line(None, Some(new), op, rest.clone()));
                    adds.push((new, rest));
                    new += 1;
                }
                b'-' => {
                    c.1 += 1;
                    d.unified.push(Row::Line(Some(old), None, op, rest.clone()));
                    dels.push((old, rest));
                    old += 1;
                }
                _ => {
                    flush(&mut d.split, &mut dels, &mut adds);
                    d.unified.push(Row::Line(Some(old), Some(new), op, rest.clone()));
                    d.split.push(Row::Split(Some((old, op, rest.clone())), Some((new, op, rest))));
                    old += 1;
                    new += 1;
                }
            }
        }
        flush(&mut d.split, &mut dels, &mut adds);
        d
    }
}

/// The finished background action: its name, and its output or error.
type Done = (String, Result<String, String>);

struct App {
    repo: Option<Repo>,
    error: String, // why there is no repo
    tab: Tab,
    split: bool,
    // what refresh reads
    name: String,
    branch: String,
    head: String,
    branches: Vec<String>,
    remotes: Vec<String>,
    github: String,
    changes: Vec<Delta>,
    stats: HashMap<String, (usize, usize)>,
    conflicts: Vec<Conflict>,
    history: Vec<(String, Change)>,
    // what the user picked or typed
    selected: Option<usize>,
    diff: Diff,
    detail_error: String,
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
            split: false,
            name: String::new(),
            branch: String::new(),
            head: String::new(),
            branches: vec![],
            remotes: vec![],
            github: String::new(),
            changes: vec![],
            stats: HashMap::new(),
            conflicts: vec![],
            history: vec![],
            selected: None,
            diff: Diff::default(),
            detail_error: String::new(),
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
                app.name = r.root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
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
            let mut out = vec![];
            sprig::cli::render_diff(&r, &mut out, self.changes.clone(), RENAME_THRESHOLD, Some(&work))?;
            self.stats = Diff::parse(&String::from_utf8_lossy(&out)).counts;
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
                Some((_, c)) => r
                    .tree_of(c.parents.first().map_or("", String::as_str))
                    .and_then(|pt| r.diff_trees(&pt, &c.tree))
                    .and_then(|ds| sprig::cli::render_diff(r, &mut out, ds, RENAME_THRESHOLD, None)),
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
        self.detail_error = res.err().map(|e| e.to_string()).unwrap_or_default();
        self.diff = Diff::parse(&String::from_utf8_lossy(&out));
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

    /// GitHub's repository header: name and branch on the left, sync on the right.
    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let idle = !self.busy();
        ui.horizontal(|ui| {
            ui.label(RichText::new("sprig").strong().size(16.0).color(ADD));
            ui.label(RichText::new("/").color(MUTED));
            ui.label(RichText::new(&self.name).strong().size(15.0));
            ui.add_space(8.0);
            let mut pick: Option<Vec<String>> = None;
            egui::ComboBox::from_id_salt("branch")
                .selected_text(RichText::new(&self.branch).monospace())
                .width(150.0)
                .show_ui(ui, |ui| {
                    ui.label(RichText::new("Switch branch").color(MUTED).small());
                    for b in &self.branches {
                        if ui.selectable_label(*b == self.branch, b).clicked() && *b != self.branch {
                            pick = Some(vec![b.clone()]);
                        }
                    }
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.new_branch)
                                .hint_text("new branch")
                                .desired_width(120.0),
                        );
                        if ui.add_enabled(idle && !self.new_branch.is_empty(), egui::Button::new("Create")).clicked() {
                            pick = Some(vec!["-b".into(), std::mem::take(&mut self.new_branch)]);
                        }
                    });
                });
            if let Some(b) = pick {
                let args: Vec<&str> = std::iter::once("checkout").chain(b.iter().map(String::as_str)).collect();
                self.run(&ctx, &args.join(" "), &args);
            }
            egui::ComboBox::from_id_salt("merge")
                .selected_text(if self.merge_from.is_empty() { "Merge from…" } else { &self.merge_from })
                .width(130.0)
                .show_ui(ui, |ui| {
                    for b in self.branches.iter().filter(|b| **b != self.branch) {
                        ui.selectable_value(&mut self.merge_from, b.clone(), b);
                    }
                });
            if !self.merge_from.is_empty() && ui.add_enabled(idle, primary("Merge")).clicked() {
                let b = std::mem::take(&mut self.merge_from);
                self.run(&ctx, &format!("merge {b}"), &["merge", &b]);
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add_enabled(idle, egui::Button::new("Refresh")).clicked() {
                    self.refresh();
                }
                let undo = ui.add_enabled(idle, egui::Button::new("Undo"));
                if undo.on_hover_text("undo the last operation (again = redo)").clicked() {
                    self.run(&ctx, "undo", &["undo"]);
                }
                ui.separator();
                if self.github.is_empty() || self.github_edit.trim() != self.github {
                    let ok = idle && !self.github_edit.trim().is_empty();
                    if ui.add_enabled(ok, primary("Connect")).clicked() {
                        let u = self.github_edit.trim().to_string();
                        self.run(&ctx, "github set", &["github", "set", &u]);
                    }
                    ui.add(
                        egui::TextEdit::singleline(&mut self.github_edit)
                            .hint_text("https://github.com/you/repo.git")
                            .desired_width(240.0),
                    );
                } else {
                    let push = ui.add_enabled(idle, primary("Push"));
                    if push.on_hover_text("push the current branch to GitHub").clicked() {
                        self.run(&ctx, "GitHub push", &["github", "push"]);
                    }
                    let pull = ui.add_enabled(idle, egui::Button::new("Pull"));
                    if pull.on_hover_text("fetch from GitHub and merge").clicked() {
                        self.run(&ctx, "GitHub pull", &["github", "pull"]);
                    }
                    let parts: Vec<&str> = self.github.trim_end_matches(".git").rsplit(['/', ':']).take(2).collect();
                    let label = format!("{}/{}", parts.get(1).unwrap_or(&""), parts[0]);
                    let link = ui.link(RichText::new(label).color(MUTED));
                    if link.on_hover_text(format!("{}\nclick to change", self.github)).clicked() {
                        self.github_edit = format!("{} ", self.github); // differs, so the URL field shows
                    }
                }
                ui.label(RichText::new("GitHub").strong());
                for rm in self.remotes.clone() {
                    ui.separator();
                    if ui.add_enabled(idle, egui::Button::new("Push")).clicked() {
                        self.run(&ctx, &format!("push {rm}"), &["push", &rm]);
                    }
                    if ui.add_enabled(idle, egui::Button::new("Pull")).clicked() {
                        self.run(&ctx, &format!("pull {rm}"), &["pull", &rm]);
                    }
                    ui.label(RichText::new(&rm).strong());
                }
            });
        });
    }

    /// Cursor's segmented Changes / History / Conflicts switch.
    fn tabs(&mut self, ui: &mut egui::Ui) {
        let mut tab = self.tab;
        egui::Frame::new().fill(SURFACE).corner_radius(RADIUS + 2).inner_margin(Margin::same(3)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.x = 2.0;
            let w = (ui.available_width() - 4.0) / 3.0;
            ui.horizontal(|ui| {
                for (t, name, n) in [
                    (Tab::Changes, "Changes", self.changes.len()),
                    (Tab::History, "History", self.history.len()),
                    (Tab::Conflicts, "Conflicts", self.conflicts.len()),
                ] {
                    let on = tab == t;
                    let color = if t == Tab::Conflicts && n > 0 {
                        DEL
                    } else if on {
                        TEXT
                    } else {
                        MUTED
                    };
                    let text = RichText::new(format!("{name}  {n}")).color(color);
                    let b = egui::Button::new(if on { text.strong() } else { text })
                        .fill(if on { BG } else { Color32::TRANSPARENT })
                        .stroke(if on { Stroke::new(1.0, BORDER) } else { Stroke::NONE })
                        .min_size(Vec2::new(w, 26.0));
                    if ui.add(b).clicked() {
                        tab = t;
                    }
                }
            });
        });
        if tab != self.tab {
            self.tab = tab;
            let first = (tab != Tab::Changes && self.len() > 0).then_some(0);
            self.select(first);
        }
    }

    fn len(&self) -> usize {
        match self.tab {
            Tab::Changes => self.changes.len(),
            Tab::History => self.history.len(),
            Tab::Conflicts => self.conflicts.len(),
        }
    }

    fn list(&mut self, ui: &mut egui::Ui) {
        let mut pick = None;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            if self.len() == 0 {
                ui.add_space(24.0);
                ui.vertical_centered(|ui| {
                    let empty = match self.tab {
                        Tab::Changes => "Working tree clean",
                        Tab::History => "No changes yet",
                        Tab::Conflicts => "No unresolved conflicts",
                    };
                    ui.label(RichText::new(empty).color(MUTED));
                });
            }
            for i in 0..self.len() {
                let on = self.selected == Some(i);
                let r = row(ui, on, |ui| match self.tab {
                    Tab::Changes => {
                        let d = &self.changes[i];
                        ui.horizontal(|ui| {
                            let (k, fg, bg) = kind(d.kind);
                            pill(ui, k, fg, bg);
                            let path =
                                if d.kind == 'R' { format!("{} → {}", d.from, d.path) } else { d.path.clone() };
                            ui.add(egui::Label::new(RichText::new(path).monospace()).truncate());
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let (a, del) = self.stats.get(&d.path).copied().unwrap_or_default();
                                ui.spacing_mut().item_spacing.x = 4.0;
                                ui.label(RichText::new(format!("−{del}")).color(DEL).monospace());
                                ui.label(RichText::new(format!("+{a}")).color(ADD).monospace());
                            });
                        });
                    }
                    Tab::History => {
                        let (id, c) = &self.history[i];
                        let first = c.message.lines().next().unwrap_or("");
                        ui.add(egui::Label::new(RichText::new(first).size(14.5)).truncate());
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            if !c.meta.agent.is_empty() {
                                pill(ui, &c.meta.agent, ACCENT, SELECTED);
                            }
                            if c.parents.len() > 1 {
                                pill(ui, "merge", MUTED, SURFACE);
                            }
                            let who = c.author.split(" <").next().unwrap_or("");
                            let meta = format!("{who} · {} · {}", short(id), ago(now - c.time));
                            ui.add(egui::Label::new(RichText::new(meta).color(MUTED).size(12.0)).truncate());
                        });
                    }
                    Tab::Conflicts => {
                        ui.horizontal(|ui| {
                            let (k, fg, bg) = kind('C');
                            pill(ui, k, fg, bg);
                            ui.label(RichText::new(&self.conflicts[i].path).monospace());
                        });
                    }
                });
                if r.clicked() {
                    // a second click on a working change goes back to all of them
                    pick = Some(if on && self.tab == Tab::Changes { None } else { Some(i) });
                }
            }
        });
        if let Some(p) = pick {
            self.select(p);
        }
    }

    fn commit_box(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.add_space(6.0);
        ui.add(
            egui::TextEdit::multiline(&mut self.message)
                .hint_text("Commit message")
                .desired_rows(3)
                .margin(Margin::same(8))
                .desired_width(f32::INFINITY),
        );
        let can =
            !self.busy() && !self.message.trim().is_empty() && (!self.changes.is_empty() || !self.conflicts.is_empty());
        let what = match self.selected.and_then(|i| self.changes.get(i)) {
            Some(d) if self.tab == Tab::Changes => d.path.clone(),
            _ => String::new(),
        };
        let label = match (what.as_str(), self.changes.len()) {
            ("", n) => format!("Commit {}", plural(n, "file")),
            (w, _) => format!("Commit {w}"),
        };
        let w = ui.available_width();
        if ui.add_enabled(can, primary(&label).min_size(Vec2::new(w, 32.0))).clicked() {
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
        ui.add_space(4.0);
    }

    /// The picked change's title, message and metadata above its diff.
    fn change_header(&self, ui: &mut egui::Ui) {
        let Some((id, c)) = self.selected.and_then(|i| self.history.get(i)) else { return };
        card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            let mut lines = c.message.trim().lines();
            ui.label(RichText::new(lines.next().unwrap_or("")).size(17.0).strong());
            let body = lines.collect::<Vec<_>>().join("\n");
            if !body.trim().is_empty() {
                ui.label(RichText::new(body.trim()).color(MUTED));
            }
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(&c.author).strong());
                ui.label(RichText::new(format!("committed {}", fmt_date(c.time))).color(MUTED));
                if !c.meta.agent.is_empty() {
                    pill(ui, format!("{} {}", c.meta.agent, c.meta.model).trim(), ACCENT, SELECTED);
                }
                if !c.conflicts.is_empty() {
                    pill(ui, &plural(c.conflicts.len(), "conflict"), WARN, WARN_BG);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Copy").on_hover_text(id).clicked() {
                        ui.ctx().copy_text(id.clone());
                    }
                    ui.label(RichText::new(short(id)).monospace().color(MUTED));
                });
            });
            if !c.meta.intent.is_empty() {
                ui.label(RichText::new(format!("Intent: {}", c.meta.intent)).color(MUTED));
            }
        });
        ui.add_space(8.0);
    }

    /// The diff toolbar (files and line counts, Unified/Split) and the lines.
    fn diff_view(&mut self, ui: &mut egui::Ui) {
        if !self.detail_error.is_empty() {
            ui.colored_label(DEL, &self.detail_error);
        }
        if self.diff.unified.is_empty() {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                let empty = if self.tab == Tab::History { "Pick a change" } else { "Nothing to show" };
                ui.label(RichText::new(empty).color(MUTED));
            });
            return;
        }
        let (files, add, del) = self.diff.counts.values().fold((0, 0, 0), |(f, a, d), (x, y)| (f + 1, a + x, d + y));
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{} changed", plural(files, "file"))).strong());
            counts(ui, add, del);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                for (on, name) in [(true, "Split"), (false, "Unified")] {
                    let sel = self.split == on;
                    let b = egui::Button::new(RichText::new(name).color(if sel { TEXT } else { MUTED })).fill(if sel {
                        SURFACE
                    } else {
                        BG
                    });
                    if ui.add(b).clicked() {
                        self.split = on;
                    }
                }
            });
        });
        ui.add_space(4.0);
        let rows = if self.split { &self.diff.split } else { &self.diff.unified };
        let font = FontId::monospace(MONO);
        let (h, cw) = ui.fonts_mut(|f| (f.row_height(&font) + 4.0, f.glyph_width(&font, 'm')));
        let widest = rows.iter().map(|r| if let Row::Line(.., t) = r { t.chars().count() } else { 0 }).max();
        let split = self.split;
        egui::Frame::new().stroke(Stroke::new(1.0, BORDER)).corner_radius(RADIUS).show(ui, |ui| {
            let outer = ui.available_width();
            let width = if split { outer } else { outer.max(GUTTER * 2.0 + 28.0 + widest.unwrap_or(0) as f32 * cw) };
            egui::ScrollArea::both().auto_shrink(false).show_rows(ui, h, rows.len(), |ui, range| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for r in &rows[range] {
                    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, h), Sense::hover());
                    paint_row(ui, rect, r, &font);
                }
            });
        });
    }

    /// Three cards, ours / base / theirs, each with a "Use" button under
    /// it, and the merged result in the working tree as an editor below.
    fn conflict_view(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let Some(cf) = self.selected.and_then(|i| self.conflicts.get(i)).cloned() else {
            banner(ui, "Pick a conflicted file. Take one side, or edit the result and save it; then commit.");
            return;
        };
        let Some(r) = &self.repo else { return };
        let text = |id: &str| -> String {
            if id.is_empty() {
                return "(no file on this side)".into();
            }
            r.get_blob(id).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_else(|e| format!("({e})"))
        };
        let sides = [
            ("ours", "current branch", text(&cf.ours), ACCENT),
            ("base", "common ancestor", text(&cf.base), MUTED),
            ("theirs", "incoming", text(&cf.theirs), ADD),
        ];
        let abs = r.abs(&cf.path);
        let n = self.conflicts.len();
        banner(
            ui,
            &format!("{} to resolve. Keep a whole side, or edit the merged result below.", plural(n, "conflict")),
        );
        ui.add_space(6.0);
        ui.label(RichText::new(&cf.path).monospace().strong().size(14.0));
        ui.add_space(4.0);
        let mut take = None;
        ui.columns(3, |cols| {
            for (col, (name, what, t, color)) in cols.iter_mut().zip(&sides) {
                card().inner_margin(Margin::ZERO).show(col, |ui| {
                    ui.set_width(ui.available_width());
                    let head = egui::Frame::new().fill(SURFACE).inner_margin(Margin::symmetric(10, 6));
                    head.corner_radius(RADIUS).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(*name).strong().color(*color));
                            ui.label(RichText::new(*what).color(MUTED).size(12.0));
                        });
                    });
                    let body = egui::ScrollArea::both().id_salt(*name).max_height(200.0);
                    body.auto_shrink([false, true]).show(ui, |ui| {
                        egui::Frame::new().inner_margin(Margin::same(8)).show(ui, |ui| {
                            ui.add(egui::Label::new(RichText::new(t).monospace()).extend());
                        });
                    });
                });
                col.add_space(4.0);
                let b = if *name == "base" { egui::Button::new("Use base") } else { primary(&format!("Use {name}")) };
                if col.add_enabled(!self.busy(), b.min_size(Vec2::new(col.available_width(), 30.0))).clicked() {
                    take = Some(*name);
                }
            }
        });
        if let Some(side) = take {
            self.run(&ctx, &format!("resolve --{side}"), &["resolve", &format!("--{side}"), &cf.path]);
        }
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Merged result").strong());
            ui.label(RichText::new("in the working tree").color(MUTED));
            if self.editor.contains("<<<<<<<") {
                pill(ui, "still has conflict markers", DEL, DEL_BG);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add_enabled(!self.busy(), primary("Save result")).clicked() {
                    let (p, data) = (abs.clone(), self.editor.clone());
                    self.job_with(&ctx, &format!("save {}", cf.path), move || {
                        std::fs::write(p, data).map(|_| String::new()).map_err(|e| e.to_string())
                    });
                }
            });
        });
        card().inner_margin(Margin::same(2)).show(ui, |ui| {
            egui::ScrollArea::vertical().id_salt("editor").show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut self.editor)
                        .code_editor()
                        .frame(egui::Frame::NONE)
                        .desired_width(f32::INFINITY)
                        .desired_rows(16),
                );
            });
        });
    }
}

/// "3 minutes ago", as GitHub writes it.
fn ago(secs: i64) -> String {
    let (n, unit) = match secs.max(0) {
        s if s < 60 => return "just now".into(),
        s if s < 3600 => (s / 60, "minute"),
        s if s < 86400 => (s / 3600, "hour"),
        s if s < 86400 * 30 => (s / 86400, "day"),
        s if s < 86400 * 365 => (s / (86400 * 30), "month"),
        s => (s / (86400 * 365), "year"),
    };
    format!("{} ago", plural(n as usize, unit))
}

/// "1 file", "2 files".
fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

/// Width of a line-number column.
const GUTTER: f32 = 44.0;

/// Paints one diff row: tinted background, number gutters, text.
fn paint_row(ui: &egui::Ui, rect: egui::Rect, row: &Row, font: &FontId) {
    let colors = |op: u8| match op {
        b'+' => (ADD_BG, ADD_NUM_BG, ADD),
        b'-' => (DEL_BG, DEL_NUM_BG, DEL),
        _ => (BG, BG, MUTED),
    };
    let y = rect.center().y;
    // one side of a line: number gutter(s), sign and text, from x on
    let side = |p: &egui::Painter, x: f32, nums: &[Option<u32>], op: u8, t: &str| {
        let (bg, nbg, sign) = colors(op);
        p.rect_filled(egui::Rect::from_x_y_ranges(x..=p.clip_rect().right(), rect.y_range()), 0.0, bg);
        let mut x = x;
        for n in nums {
            p.rect_filled(egui::Rect::from_x_y_ranges(x..=x + GUTTER, rect.y_range()), 0.0, nbg);
            if let Some(n) = n {
                p.text(egui::pos2(x + GUTTER - 6.0, y), Align2::RIGHT_CENTER, n, font.clone(), MUTED);
            }
            x += GUTTER;
        }
        p.text(egui::pos2(x + 6.0, y), Align2::LEFT_CENTER, (op as char).to_string(), font.clone(), sign);
        p.text(egui::pos2(x + 20.0, y), Align2::LEFT_CENTER, t, font.clone(), TEXT);
    };
    let p = ui.painter_at(rect);
    let label = |fill, text: &str, x: f32, color| {
        p.rect_filled(rect, 0.0, fill);
        p.text(egui::pos2(rect.left() + x, y), Align2::LEFT_CENTER, text, font.clone(), color);
    };
    match row {
        Row::File(path) => {
            label(SURFACE, path, 10.0, TEXT);
            p.line_segment([rect.left_top(), rect.right_top()], Stroke::new(1.0, BORDER));
            p.line_segment([rect.left_bottom(), rect.right_bottom()], Stroke::new(1.0, BORDER));
        }
        Row::Note(t) => label(SURFACE, t, 10.0, MUTED),
        Row::Hunk(t) => label(HUNK_BG, t, 2.0 * GUTTER + 8.0, MUTED),
        Row::Line(o, n, op, t) => side(&p, rect.left(), &[*o, *n], *op, t),
        Row::Split(l, r) => {
            let half = rect.width() / 2.0;
            for (k, s) in [l, r].into_iter().enumerate() {
                let x = rect.left() + k as f32 * half;
                let area = egui::Rect::from_x_y_ranges(x..=x + half, rect.y_range());
                let p = ui.painter_at(area);
                match s {
                    Some((n, op, t)) => side(&p, x, &[Some(*n)], *op, t),
                    None => {
                        p.rect_filled(area, 0.0, SURFACE);
                    }
                }
            }
            let mid = rect.left() + half;
            p.line_segment([egui::pos2(mid, rect.top()), egui::pos2(mid, rect.bottom())], Stroke::new(1.0, BORDER));
        }
    }
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
        let bar = |fill| {
            egui::Frame::new().fill(fill).inner_margin(Margin::symmetric(14, 8)).stroke(Stroke::new(1.0, BORDER))
        };
        if self.repo.is_none() {
            egui::CentralPanel::default().frame(bar(BG).inner_margin(Margin::same(32))).show(ui, |ui| {
                ui.heading("Not a sprig repository");
                ui.label(RichText::new(&self.error).color(MUTED));
                ui.label("Run `sprig init` there (or `sprig init` + `sprig github set <url>` + Pull), or start sprig-ui with a repository path.");
            });
            return;
        }
        egui::Panel::top("top").frame(bar(BG)).show(ui, |ui| self.top_bar(ui));
        egui::Panel::bottom("status").frame(bar(SURFACE).inner_margin(Margin::symmetric(14, 4))).show(ui, |ui| {
            ui.horizontal(|ui| {
                if let Some((name, _)) = &self.job {
                    ui.spinner();
                    ui.label(format!("{name}…"));
                } else {
                    match &self.status {
                        Ok(s) => ui.label(RichText::new(s.lines().last().unwrap_or("")).color(MUTED)).on_hover_text(s),
                        Err(e) => ui.colored_label(DEL, e.lines().next().unwrap_or("")).on_hover_text(e),
                    };
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let at = if self.head.is_empty() {
                        "no changes yet".to_string()
                    } else {
                        format!("{} at {}", self.branch, short(&self.head))
                    };
                    ui.label(RichText::new(at).monospace().color(MUTED));
                });
            });
        });
        egui::Panel::left("list")
            .resizable(true)
            .default_size(360.0)
            .frame(bar(BG).inner_margin(Margin::same(10)))
            .show(ui, |ui| {
                self.tabs(ui);
                ui.add_space(6.0);
                if self.tab == Tab::Changes {
                    egui::Panel::bottom("commit").frame(egui::Frame::new()).show(ui, |ui| self.commit_box(ui));
                }
                self.list(ui);
            });
        egui::CentralPanel::default().frame(egui::Frame::new().fill(BG).inner_margin(Margin::same(16))).show(
            ui,
            |ui| match self.tab {
                Tab::Conflicts => self.conflict_view(ui),
                Tab::History => {
                    self.change_header(ui);
                    self.diff_view(ui);
                }
                Tab::Changes => self.diff_view(ui),
            },
        );
    }
}
