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
//! The look follows Cursor's Agents tab and Graphite's PR inbox on Mobbin,
//! in dark (see `theme` and the README): a sidebar with the repository,
//! navigation and who is working, an inbox of bordered sections, a change
//! page with its provenance beside the diff, +N −N counts and tinted diff
//! lines with line-number gutters.

use eframe::egui::{self, Align2, Color32, CornerRadius, FontId, Margin, RichText, Sense, Stroke, Vec2};
use sprig::store::{Change, Conflict, Repo, short};
use sprig::tree::{Delta, RENAME_THRESHOLD};
use sprig::util::{Error, fmt_date};
use std::cell::RefCell;
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

    pub const fn hex(c: u32) -> Color32 {
        Color32::from_rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
    }
    pub const BG: Color32 = hex(0x0e0e10);
    pub const SURFACE: Color32 = hex(0x17171a); // panels, card headers
    pub const RAISED: Color32 = hex(0x232327); // selected tab, pressed widget
    pub const BORDER: Color32 = hex(0x2a2a2f);
    pub const TEXT: Color32 = hex(0xededef);
    pub const MUTED: Color32 = hex(0x8b8b93);
    pub const ACCENT: Color32 = hex(0x5b8def);
    pub const SELECTED: Color32 = hex(0x1a2338);
    pub const PRIMARY: Color32 = hex(0x2f6bea); // Graphite's blue "Publish"
    pub const ADD: Color32 = hex(0x3fb950);
    pub const ADD_BG: Color32 = hex(0x12261a);
    pub const ADD_NUM_BG: Color32 = hex(0x1a3a25);
    pub const DEL: Color32 = hex(0xf85149);
    pub const DEL_BG: Color32 = hex(0x2d1517);
    pub const DEL_NUM_BG: Color32 = hex(0x47201f);
    pub const HUNK_BG: Color32 = hex(0x161d2c);
    pub const WARN: Color32 = hex(0xd29922);
    pub const WARN_BG: Color32 = hex(0x272010);
    pub const SIDEBAR: Color32 = hex(0x121214);
    pub const PURPLE: Color32 = hex(0xa371f7);
    pub const RADIUS: u8 = 6;
    pub const MONO: f32 = 12.5;

    /// The 600-weight family: egui's `strong()` only changes the color.
    pub fn semi() -> egui::FontFamily {
        egui::FontFamily::Name("semi".into())
    }

    /// The system's UI fonts where they exist (SF Pro and SF Mono on macOS,
    /// Segoe UI and Consolas on Windows, DejaVu on Linux), else egui's own.
    fn fonts(ctx: &egui::Context) {
        use egui::FontFamily::{Monospace, Proportional};
        use egui::epaint::text::{FontData, FontTweak, VariationCoords};
        // (regular, semibold); a variable font serves both through its weight axis
        const SANS: [(&str, &str); 4] = [
            ("/System/Library/Fonts/SFNS.ttf", "/System/Library/Fonts/SFNS.ttf"),
            ("C:\\Windows\\Fonts\\segoeui.ttf", "C:\\Windows\\Fonts\\seguisb.ttf"),
            ("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf"),
            (
                "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf",
                "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans-Bold.ttf",
            ),
        ];
        const MONO_FILES: [&str; 4] = [
            "/System/Library/Fonts/SFNSMono.ttf",
            "C:\\Windows\\Fonts\\consola.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
            "/usr/share/fonts/dejavu-sans-mono-fonts/DejaVuSansMono.ttf",
        ];
        let weight = |b: Vec<u8>, w: f32| {
            let tweak = FontTweak { coords: VariationCoords::new([(b"wght", w)]), ..Default::default() };
            std::sync::Arc::new(FontData::from_owned(b).tweak(tweak))
        };
        let mut f = egui::FontDefinitions::default();
        let mut semi_list = f.families[&Proportional].clone();
        if let Some((r, b)) = SANS.iter().find_map(|(r, b)| Some((std::fs::read(r).ok()?, std::fs::read(b).ok()?))) {
            f.font_data.insert("sans".into(), weight(r, 400.0));
            f.font_data.insert("sans-semi".into(), weight(b, 590.0));
            f.families.get_mut(&Proportional).unwrap().insert(0, "sans".into());
            semi_list.insert(0, "sans-semi".into());
        }
        f.families.insert(semi(), semi_list);
        if let Some(m) = MONO_FILES.iter().find_map(|p| std::fs::read(p).ok()) {
            f.font_data.insert("mono".into(), weight(m, 400.0));
            f.families.get_mut(&Monospace).unwrap().insert(0, "mono".into());
        }
        ctx.set_fonts(f);
    }

    pub fn apply(ctx: &egui::Context) {
        fonts(ctx);
        ctx.set_theme(egui::Theme::Dark);
        ctx.all_styles_mut(|s| {
            s.spacing.item_spacing = Vec2::new(8.0, 6.0);
            s.spacing.button_padding = Vec2::new(10.0, 5.0);
            s.spacing.interact_size.y = 28.0;
            s.text_styles.insert(egui::TextStyle::Monospace, FontId::monospace(MONO));
            s.text_styles.insert(egui::TextStyle::Body, FontId::proportional(13.0));
            s.text_styles.insert(egui::TextStyle::Button, FontId::proportional(13.0));
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
                (&mut w.active, RAISED),
                (&mut w.open, SURFACE),
            ] {
                st.bg_fill = fill;
                st.weak_bg_fill = fill;
                st.corner_radius = CornerRadius::same(RADIUS);
                st.bg_stroke = Stroke::new(1.0, BORDER);
                st.fg_stroke = Stroke::new(1.0, TEXT);
                st.expansion = 0.0;
            }
            w.hovered.bg_stroke = Stroke::new(1.0, hex(0x3a3a40));
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

    /// The blue call-to-action button: commit, push, save.
    pub fn primary(text: &str) -> egui::Button<'static> {
        egui::Button::new(RichText::new(text.to_string()).color(Color32::WHITE).family(semi())).fill(PRIMARY)
    }

    /// GitLab's "+23 −0".
    pub fn counts(ui: &mut egui::Ui, add: usize, del: usize) {
        ui.label(RichText::new(format!("+{add}")).color(ADD).monospace());
        ui.label(RichText::new(format!("−{del}")).color(DEL).monospace());
    }

    /// A small rounded label: a change kind, "merge", an agent.
    pub fn pill(ui: &mut egui::Ui, text: &str, fg: Color32, bg: Color32) {
        egui::Frame::new().fill(bg).corner_radius(10).inner_margin(Margin::symmetric(7, 1)).show(ui, |ui| {
            ui.label(RichText::new(text).color(fg).size(11.5).family(semi()));
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

    /// Graphite's inbox section: a bordered card with a "N  Title" header
    /// row and an optional right-side note, then the rows.
    pub fn section(
        ui: &mut egui::Ui,
        n: usize,
        title: &str,
        aside: impl FnOnce(&mut egui::Ui),
        body: impl FnOnce(&mut egui::Ui),
    ) {
        egui::Frame::new().fill(SIDEBAR).stroke(Stroke::new(1.0, BORDER)).corner_radius(RADIUS + 2).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            egui::Frame::new().inner_margin(Margin::symmetric(14, 9)).show(ui, |ui| {
                ui.horizontal(|ui| {
                    icon(ui, Icon::Down, 12.0, MUTED);
                    ui.add_sized([14.0, 18.0], egui::Label::new(RichText::new(n.to_string()).color(MUTED).size(12.0)));
                    ui.label(RichText::new(title).family(semi()));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), aside);
                });
            });
            let r = ui.min_rect();
            ui.painter().line_segment(
                [egui::pos2(r.left(), r.bottom()), egui::pos2(r.right(), r.bottom())],
                Stroke::new(1.0, BORDER),
            );
            body(ui);
        });
    }

    /// A rounded initial: square for agents, round for people, its color
    /// picked from the name so the same author always looks the same.
    pub fn avatar(ui: &mut egui::Ui, name: &str, agent: bool, size: f32) {
        const PAL: [(u32, u32); 4] =
            [(0x1a2338, 0x8fb0f5), (0x1d2b1f, 0x7ee08a), (0x2a1f3d, 0xc9a7ff), (0x2b2212, 0xe3b452)];
        let (bg, fg) = PAL[name.bytes().fold(0usize, |a, b| a.wrapping_mul(31).wrapping_add(b as usize)) % PAL.len()];
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
        let r = if agent { (size / 3.7) as u8 } else { (size / 2.0) as u8 };
        ui.painter().rect_filled(rect, r, hex(bg));
        let c = name.chars().next().unwrap_or('?').to_uppercase().to_string();
        ui.painter().text(rect.center(), Align2::CENTER_CENTER, c, FontId::proportional(size * 0.46), hex(fg));
    }

    /// A check as a pill, green when it passes (as `sprig audit` reads it),
    /// red when it failed, yellow otherwise.
    pub fn check(ui: &mut egui::Ui, name: &str, value: &str) {
        let (fg, bg) = match check_state(value) {
            Some(true) => (ADD, ADD_BG),
            Some(false) => (DEL, DEL_BG),
            None => (WARN, WARN_BG),
        };
        pill(ui, &format!("{name}={value}"), fg, bg);
    }

    /// Some(true) passing, Some(false) failing, None neither.
    pub fn check_state(value: &str) -> Option<bool> {
        match value.to_lowercase().as_str() {
            "pass" | "passed" | "ok" | "approved" | "true" => Some(true),
            "fail" | "failed" | "false" | "error" | "rejected" => Some(false),
            _ => None,
        }
    }

    /// A segmented switch (Cursor's All / Agents / People): returns the
    /// option clicked.
    pub fn segmented<T: PartialEq + Copy>(ui: &mut egui::Ui, cur: T, opts: &[(T, &str)]) -> T {
        let mut out = cur;
        egui::Frame::new()
            .fill(SURFACE)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(RADIUS + 1)
            .inner_margin(Margin::same(2))
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                ui.horizontal(|ui| {
                    // a right-to-left parent lays the row out backwards
                    let rtl = ui.layout().prefer_right_to_left();
                    let order: Vec<&(T, &str)> = if rtl { opts.iter().rev().collect() } else { opts.iter().collect() };
                    for (v, name) in order {
                        let on = *v == cur;
                        let b = egui::Button::new(RichText::new(*name).color(if on { TEXT } else { MUTED }))
                            .fill(if on { RAISED } else { Color32::TRANSPARENT })
                            .stroke(Stroke::NONE)
                            .min_size(Vec2::new(0.0, 24.0));
                        if ui.add(b).clicked() {
                            out = *v;
                        }
                    }
                });
            });
        out
    }

    /// Front's "1 conflict found" strip.
    pub fn banner(ui: &mut egui::Ui, text: &str) {
        egui::Frame::new()
            .fill(WARN_BG)
            .stroke(Stroke::new(1.0, hex(0x4a3c14)))
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
        let frame = egui::Frame::new().fill(fill).corner_radius(RADIUS).inner_margin(Margin::symmetric(10, 4));
        let inner = frame.show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        });
        let resp = ui.interact(inner.response.rect, inner.response.id.with("row"), Sense::click());
        if resp.hovered() && !selected {
            ui.painter().rect_filled(resp.rect, RADIUS, Color32::from_white_alpha(6));
        }
        resp
    }

    /// Stroke icons in a 24-unit box, as lucide draws them.
    #[derive(Clone, Copy)]
    pub enum Icon {
        Sprout,
        Inbox,
        Diff,
        Clock,
        Branch,
        Warn,
        Undo,
        Plus,
        Search,
        Down,
        Left,
        Check,
        Cross,
        Merge,
        Shield,
        Up,
        UpDown,
    }

    /// Paints an icon centred in rect.
    pub fn paint_icon(p: &egui::Painter, rect: egui::Rect, icon: Icon, color: Color32) {
        let k = rect.width().min(rect.height()) / 24.0;
        let o = rect.center() - Vec2::splat(12.0 * k);
        let pt = |x: f32, y: f32| o + Vec2::new(x, y) * k;
        let st = Stroke::new(1.7 * k.max(0.6), color);
        let line = |pts: &[(f32, f32)]| {
            p.add(egui::Shape::line(pts.iter().map(|&(x, y)| pt(x, y)).collect(), st));
        };
        let closed = |pts: &[(f32, f32)]| {
            p.add(egui::Shape::closed_line(pts.iter().map(|&(x, y)| pt(x, y)).collect(), st));
        };
        let circle = |x: f32, y: f32, r: f32| {
            p.circle_stroke(pt(x, y), r * k, st);
        };
        match icon {
            Icon::Sprout => {
                line(&[(12.0, 22.0), (12.0, 9.0)]);
                closed(&[(12.0, 13.0), (8.0, 12.0), (6.0, 7.0), (10.0, 8.0)]);
                closed(&[(12.0, 10.0), (14.0, 5.0), (18.0, 4.0), (17.0, 8.0)]);
            }
            Icon::Inbox => {
                line(&[(2.0, 12.0), (8.0, 12.0), (10.0, 15.0), (14.0, 15.0), (16.0, 12.0), (22.0, 12.0)]);
                closed(&[(5.5, 5.0), (18.5, 5.0), (22.0, 12.0), (22.0, 19.0), (2.0, 19.0), (2.0, 12.0)]);
            }
            Icon::Diff => {
                closed(&[(5.0, 3.0), (15.0, 3.0), (19.0, 7.0), (19.0, 21.0), (5.0, 21.0)]);
                line(&[(12.0, 8.0), (12.0, 14.0)]);
                line(&[(9.0, 11.0), (15.0, 11.0)]);
                line(&[(9.0, 17.0), (15.0, 17.0)]);
            }
            Icon::Clock => {
                circle(12.0, 12.0, 9.0);
                line(&[(12.0, 7.5), (12.0, 12.0), (15.0, 14.0)]);
            }
            Icon::Branch => {
                circle(6.0, 6.0, 2.0);
                circle(6.0, 18.0, 2.0);
                circle(18.0, 8.0, 2.0);
                line(&[(6.0, 8.0), (6.0, 16.0)]);
                line(&[(18.0, 10.0), (17.0, 13.0), (8.0, 16.5)]);
            }
            Icon::Warn => {
                closed(&[(12.0, 3.0), (22.0, 20.0), (2.0, 20.0)]);
                line(&[(12.0, 9.0), (12.0, 13.5)]);
                p.circle_filled(pt(12.0, 16.8), 1.1 * k, color);
            }
            Icon::Undo => {
                line(&[(3.0, 7.0), (3.0, 13.0), (9.0, 13.0)]);
                line(&[(3.0, 13.0), (7.0, 9.5), (12.0, 8.0), (17.0, 9.5), (20.0, 13.0), (21.0, 17.0)]);
            }
            Icon::Plus => {
                line(&[(12.0, 5.0), (12.0, 19.0)]);
                line(&[(5.0, 12.0), (19.0, 12.0)]);
            }
            Icon::Search => {
                circle(11.0, 11.0, 7.0);
                line(&[(20.0, 20.0), (16.0, 16.0)]);
            }
            Icon::Down => line(&[(6.0, 9.0), (12.0, 15.0), (18.0, 9.0)]),
            Icon::Left => line(&[(15.0, 18.0), (9.0, 12.0), (15.0, 6.0)]),
            Icon::Check => line(&[(20.0, 6.0), (9.0, 17.0), (4.0, 12.0)]),
            Icon::Cross => {
                line(&[(6.0, 6.0), (18.0, 18.0)]);
                line(&[(18.0, 6.0), (6.0, 18.0)]);
            }
            Icon::Merge => {
                circle(6.0, 6.0, 2.0);
                circle(18.0, 18.0, 2.0);
                line(&[(6.0, 8.0), (6.0, 11.0), (8.0, 15.0), (12.0, 17.5), (16.0, 18.0)]);
            }
            Icon::Shield => {
                closed(&[
                    (12.0, 2.0),
                    (20.0, 5.0),
                    (20.0, 12.0),
                    (17.0, 18.0),
                    (12.0, 22.0),
                    (7.0, 18.0),
                    (4.0, 12.0),
                    (4.0, 5.0),
                ]);
                line(&[(9.0, 12.0), (11.0, 14.0), (15.0, 10.0)]);
            }
            Icon::Up => {
                line(&[(12.0, 19.0), (12.0, 5.0)]);
                line(&[(5.0, 12.0), (12.0, 5.0), (19.0, 12.0)]);
            }
            Icon::UpDown => {
                line(&[(7.0, 15.0), (12.0, 20.0), (17.0, 15.0)]);
                line(&[(7.0, 9.0), (12.0, 4.0), (17.0, 9.0)]);
            }
        }
    }

    pub fn icon(ui: &mut egui::Ui, icon: Icon, size: f32, color: Color32) {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
        paint_icon(ui.painter(), rect, icon, color);
    }

    #[derive(Clone, Copy, PartialEq)]
    pub enum Btn {
        /// blue fill, white text
        Primary,
        /// outlined
        Secondary,
        /// filled with the surface tone
        Soft,
        /// amber fill: resolving conflicts
        Warn,
    }

    /// A button with an optional leading icon, drawn like the design's:
    /// 30 px tall, 6 px corners. Disabled ones are dimmed and explain why
    /// on hover.
    pub fn button(ui: &mut egui::Ui, enabled: bool, ic: Option<Icon>, text: &str, kind: Btn) -> egui::Response {
        button_w(ui, enabled, ic, text, kind, 0.0)
    }

    /// `button`, at least min_w wide, its label centred.
    pub fn button_w(
        ui: &mut egui::Ui,
        enabled: bool,
        ic: Option<Icon>,
        text: &str,
        kind: Btn,
        min_w: f32,
    ) -> egui::Response {
        let (fill, fg, stroke) = match kind {
            Btn::Primary => (PRIMARY, Color32::WHITE, Color32::TRANSPARENT),
            Btn::Secondary => (Color32::TRANSPARENT, TEXT, BORDER),
            Btn::Soft => (SURFACE, TEXT, BORDER),
            Btn::Warn => (WARN, hex(0x1a1406), Color32::TRANSPARENT),
        };
        let font = if kind == Btn::Secondary || kind == Btn::Soft {
            FontId::proportional(13.0)
        } else {
            FontId::new(13.0, semi())
        };
        let galley = ui.painter().layout_no_wrap(text.to_string(), font, fg);
        let iw = if ic.is_some() { 19.0 } else { 0.0 };
        let content = galley.size().x + iw;
        let size = Vec2::new((content + 24.0).max(min_w), 30.0);
        let sense = if enabled { Sense::click() } else { Sense::hover() };
        let (rect, resp) = ui.allocate_exact_size(size, sense);
        let dim = |c: Color32| if enabled { c } else { c.gamma_multiply(0.45) };
        let hover = enabled && resp.hovered();
        let fill = if hover && kind == Btn::Secondary { SURFACE } else { fill };
        let fill = if hover && kind != Btn::Secondary { fill.gamma_multiply(1.12) } else { fill };
        ui.painter().rect(rect, RADIUS, dim(fill), Stroke::new(1.0, dim(stroke)), egui::StrokeKind::Inside);
        let mut x = rect.center().x - content / 2.0;
        if let Some(i) = ic {
            let r = egui::Rect::from_center_size(egui::pos2(x + 7.0, rect.center().y), Vec2::splat(14.0));
            paint_icon(ui.painter(), r, i, dim(fg));
            x += iw;
        }
        ui.painter().galley(egui::pos2(x, rect.center().y - galley.size().y / 2.0), galley, dim(fg));
        if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp.on_hover_text("coming soon") }
    }

    /// A square icon-only button (search, repository switcher).
    pub fn icon_button(ui: &mut egui::Ui, enabled: bool, ic: Icon, label: &str) -> egui::Response {
        let (rect, resp) =
            ui.allocate_exact_size(Vec2::splat(34.0), if enabled { Sense::click() } else { Sense::hover() });
        let c = if enabled { hex(0xa1a1aa) } else { MUTED.gamma_multiply(0.6) };
        ui.painter().rect(rect, RADIUS, SURFACE, Stroke::new(1.0, BORDER), egui::StrokeKind::Inside);
        paint_icon(ui.painter(), rect.shrink(9.0), ic, c);
        resp.on_hover_text(if enabled { label.to_string() } else { format!("{label}: coming soon") })
    }

    /// A sidebar link: icon, label, count; blue tint when it's the page shown.
    pub fn nav(
        ui: &mut egui::Ui,
        on: bool,
        enabled: bool,
        ic: Icon,
        label: &str,
        n: usize,
        warn: bool,
    ) -> egui::Response {
        let (rect, resp) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), 32.0),
            if enabled { Sense::click() } else { Sense::hover() },
        );
        let p = ui.painter();
        if on {
            p.rect_filled(rect, RADIUS, SELECTED);
        } else if enabled && resp.hovered() {
            p.rect_filled(rect, RADIUS, Color32::from_white_alpha(6));
        }
        let fg = if !enabled {
            MUTED.gamma_multiply(0.7)
        } else if on {
            TEXT
        } else {
            hex(0xc4c4cc)
        };
        let ic_color = if warn {
            WARN
        } else if on {
            ACCENT
        } else {
            fg
        };
        paint_icon(
            p,
            egui::Rect::from_center_size(egui::pos2(rect.left() + 18.0, rect.center().y), Vec2::splat(15.0)),
            ic,
            ic_color,
        );
        p.text(
            egui::pos2(rect.left() + 36.0, rect.center().y),
            Align2::LEFT_CENTER,
            label,
            FontId::proportional(13.0),
            fg,
        );
        if n > 0 {
            let (c, f) = if warn {
                (WARN, FontId::new(12.0, semi()))
            } else {
                (if on { hex(0xa1a1aa) } else { MUTED }, FontId::proportional(12.0))
            };
            p.text(egui::pos2(rect.right() - 10.0, rect.center().y), Align2::RIGHT_CENTER, n.to_string(), f, c);
        }
        if enabled { resp } else { resp.on_hover_text("coming soon") }
    }

    /// A row inside a section: full width, a divider under it, a hover tint.
    pub fn list_row(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) -> egui::Response {
        let inner = egui::Frame::new().inner_margin(Margin::symmetric(14, 4)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        });
        let r = inner.response.rect;
        let resp = ui.interact(r, inner.response.id.with("list_row"), Sense::click());
        if resp.hovered() {
            ui.painter().rect_filled(r, 0.0, Color32::from_white_alpha(5));
        }
        ui.painter().line_segment([r.left_bottom(), r.right_bottom()], Stroke::new(1.0, hex(0x1d1d21)));
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    }

    /// A full-width row in a popup menu: icon and label, tinted when current.
    pub fn menu_item(ui: &mut egui::Ui, on: bool, ic: Icon, label: &str) -> egui::Response {
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 28.0), Sense::click());
        let p = ui.painter();
        if on {
            p.rect_filled(rect, RADIUS, SELECTED);
        } else if resp.hovered() {
            p.rect_filled(rect, RADIUS, Color32::from_white_alpha(8));
        }
        let c = if on { ACCENT } else { hex(0xa1a1aa) };
        paint_icon(
            p,
            egui::Rect::from_center_size(rect.left_center() + Vec2::new(14.0, 0.0), Vec2::splat(13.0)),
            ic,
            c,
        );
        p.text(rect.left_center() + Vec2::new(30.0, 0.0), Align2::LEFT_CENTER, label, FontId::monospace(12.5), TEXT);
        if on {
            let r = egui::Rect::from_center_size(rect.right_center() - Vec2::new(14.0, 0.0), Vec2::splat(12.0));
            paint_icon(p, r, Icon::Check, ACCENT);
        }
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    }

    /// A small square kind letter (M, A, D, R) as the working tree list shows it.
    pub fn kind_badge(ui: &mut egui::Ui, k: char) {
        let (t, fg, bg) = kind(k);
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
        ui.painter().rect_filled(rect, 4, bg);
        ui.painter().text(rect.center(), Align2::CENTER_CENTER, t, FontId::new(11.0, egui::FontFamily::Monospace), fg);
    }
}

use theme::*;

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Inbox,
    Changes,
    History,
    Conflicts,
}

/// The Inbox and History filter: everyone, agent-made changes, or people's.
#[derive(PartialEq, Clone, Copy)]
enum Who {
    All,
    Agents,
    People,
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

/// Someone who made changes lately, for the sidebar: an agent, or a person
/// when the change has no agent.
struct Worker {
    name: String,
    agent: bool,
    /// the latest change's first line
    latest: String,
    count: usize,
    /// index of the latest change in `history`
    at: usize,
}

struct App {
    repo: Option<Repo>,
    error: String, // why there is no repo
    tab: Tab,
    who: Who,
    split: bool,
    // what refresh reads
    name: String,
    branch: String,
    head: String,
    branches: Vec<String>,
    remotes: Vec<String>,
    github: String,
    /// commits on the current branch GitHub lacks, and the other way round,
    /// as of the last pull
    ahead_behind: Option<(usize, usize)>,
    changes: Vec<Delta>,
    stats: HashMap<String, (usize, usize)>,
    conflicts: Vec<Conflict>,
    history: Vec<(String, Change)>,
    /// agent changes without a passing review, as `sprig audit` finds them
    unreviewed: Vec<usize>,
    workers: Vec<Worker>,
    /// +N −N per change id; changes never change, so this only grows
    change_stats: RefCell<HashMap<String, (usize, usize)>>,
    trusted: std::collections::BTreeMap<String, String>,
    // what the user picked or typed
    selected: Option<usize>,
    diff: Diff,
    detail_error: String,
    /// the picked change's signature: who signed it, or why it isn't validly signed
    signature: Option<Result<String, String>>,
    message: String,
    new_branch: String,
    github_edit: String,
    editor: String,
    // the running action, and the last result
    job: Option<(String, mpsc::Receiver<Done>)>,
    status: Result<String, String>,
    focused: bool,
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

impl App {
    fn open(dir: PathBuf) -> App {
        let mut app = App {
            repo: None,
            error: String::new(),
            tab: Tab::Inbox,
            who: Who::All,
            split: true,
            name: String::new(),
            branch: String::new(),
            head: String::new(),
            branches: vec![],
            remotes: vec![],
            github: String::new(),
            ahead_behind: None,
            changes: vec![],
            stats: HashMap::new(),
            conflicts: vec![],
            history: vec![],
            unreviewed: vec![],
            workers: vec![],
            change_stats: RefCell::new(HashMap::new()),
            trusted: Default::default(),
            selected: None,
            diff: Diff::default(),
            detail_error: String::new(),
            signature: None,
            message: String::new(),
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
            self.trusted = cfg.trusted.clone();
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
            self.unreviewed = (0..self.history.len())
                .filter(|&i| {
                    let m = &self.history[i].1.meta;
                    !m.agent.is_empty() && check_state(m.checks.get("review").map_or("", String::as_str)) != Some(true)
                })
                .collect();
            // who made changes in the last week, busiest first
            let week = now() - 7 * 86400;
            let mut workers: Vec<Worker> = vec![];
            for (i, (_, c)) in self.history.iter().enumerate().take_while(|(_, (_, c))| c.time >= week) {
                let agent = !c.meta.agent.is_empty();
                let name = if agent { c.meta.agent.clone() } else { c.author.split(" <").next().unwrap_or("").into() };
                match workers.iter_mut().find(|w| w.name == name && w.agent == agent) {
                    Some(w) => w.count += 1,
                    None => workers.push(Worker {
                        name,
                        agent,
                        latest: c.message.lines().next().unwrap_or("").into(),
                        count: 1,
                        at: i,
                    }),
                }
            }
            workers.sort_by_key(|w| std::cmp::Reverse(w.count));
            self.workers = workers;
            // ponytail: walks both histories on every refresh; cache by tip if big repos make it slow
            self.ahead_behind = match r.read_ref(&format!("remotes/git/{}", self.branch)) {
                Some(theirs) if !head.is_empty() && !self.github.is_empty() => {
                    let (a, b) = (r.ancestors(&head)?, r.ancestors(&theirs)?);
                    Some((
                        a.keys().filter(|k| !b.contains_key(*k)).count(),
                        b.keys().filter(|k| !a.contains_key(*k)).count(),
                    ))
                }
                _ => None,
            };
            Ok(())
        })();
        if let Err(e) = res {
            self.status = Err(e.to_string());
        }
        self.repo = Some(r);
        self.select(self.selected);
    }

    /// Goes to a tab and picks a row in it.
    fn go(&mut self, tab: Tab, i: Option<usize>) {
        self.tab = tab;
        self.select(i);
    }

    /// Picks a row in the current tab and loads what it shows.
    fn select(&mut self, i: Option<usize>) {
        self.selected = i;
        self.signature = None;
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
                Some((_, c)) => {
                    self.signature = Some(match r.verify_change(c) {
                        Ok(key) => Ok(self
                            .trusted
                            .get(&key)
                            .cloned()
                            .map_or(format!("untrusted key {key}"), |who| format!("{who} ({key})"))),
                        Err((_, e)) => Err(e.to_string()),
                    });
                    r.tree_of(c.parents.first().map_or("", String::as_str))
                        .and_then(|pt| r.diff_trees(&pt, &c.tree))
                        .and_then(|ds| sprig::cli::render_diff(r, &mut out, ds, RENAME_THRESHOLD, None))
                }
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

    fn shown(&self, i: usize) -> bool {
        let agent = !self.history[i].1.meta.agent.is_empty();
        match self.who {
            Who::All => true,
            Who::Agents => agent,
            Who::People => !agent,
        }
    }

    // -----------------------------------------------------------------------
    // Sidebar: Cursor's Agents tab

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let idle = !self.busy();
        // GitHub and sprig remotes, pinned to the bottom
        egui::Panel::bottom("sync")
            .show_separator_line(false)
            .frame(egui::Frame::new().inner_margin(Margin { top: 10, ..Margin::ZERO }))
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 6.0;
                self.github_card(ui);
                for rm in self.remotes.clone() {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&rm).family(semi()));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if button(ui, idle, None, "Push", Btn::Soft).clicked() {
                                self.run(&ctx, &format!("push {rm}"), &["push", &rm]);
                            }
                            if button(ui, idle, None, "Pull", Btn::Secondary).clicked() {
                                self.run(&ctx, &format!("pull {rm}"), &["pull", &rm]);
                            }
                        });
                    });
                }
            });
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::hover());
            ui.painter().rect(rect, 7, SELECTED, Stroke::new(1.0, hex(0x2f3b5c)), egui::StrokeKind::Inside);
            paint_icon(ui.painter(), rect.shrink(7.0), Icon::Sprout, ACCENT);
            let w = ui.available_width() - 34.0 - ui.spacing().item_spacing.x;
            ui.allocate_ui_with_layout(Vec2::new(w, 32.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.set_width(w);
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.add(egui::Label::new(RichText::new(&self.name).family(semi())).truncate());
                if let Some(r) = &self.repo {
                    let home = std::env::var("HOME").unwrap_or_default();
                    let path = r.root.display().to_string();
                    let path = match path.strip_prefix(&home) {
                        Some(rest) if !home.is_empty() => format!("~{rest}"),
                        _ => path,
                    };
                    ui.add(egui::Label::new(RichText::new(path).monospace().size(11.0).color(MUTED)).truncate());
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                icon_button(ui, false, Icon::UpDown, "Switch repository");
            });
        });
        ui.add_space(10.0);
        let mut go = None;
        ui.horizontal(|ui| {
            let w = ui.available_width() - 34.0 - ui.spacing().item_spacing.x;
            let (rect, r) = ui.allocate_exact_size(Vec2::new(w, 34.0), Sense::click());
            let p = ui.painter();
            p.rect(
                rect,
                RADIUS + 1,
                if r.hovered() { RAISED } else { SURFACE },
                Stroke::new(1.0, BORDER),
                egui::StrokeKind::Inside,
            );
            paint_icon(
                p,
                egui::Rect::from_center_size(egui::pos2(rect.left() + 18.0, rect.center().y), Vec2::splat(14.0)),
                Icon::Plus,
                TEXT,
            );
            p.text(
                egui::pos2(rect.left() + 34.0, rect.center().y),
                Align2::LEFT_CENTER,
                "New change",
                FontId::proportional(13.0),
                TEXT,
            );
            let kbd = if cfg!(target_os = "macos") { "⌘N" } else { "Ctrl+N" };
            p.text(
                egui::pos2(rect.right() - 10.0, rect.center().y),
                Align2::RIGHT_CENTER,
                kbd,
                FontId::monospace(11.0),
                MUTED,
            );
            if r.on_hover_text("commit the working tree").clicked() {
                go = Some(Tab::Changes);
            }
            icon_button(ui, false, Icon::Search, "Search");
        });
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::N)) {
            go = Some(Tab::Changes);
        }
        ui.add_space(8.0);

        let mut open = None;
        ui.spacing_mut().item_spacing.y = 2.0;
        for (t, ic, name, n, warn) in [
            (Some(Tab::Inbox), Icon::Inbox, "Inbox", self.unreviewed.len() + self.conflicts.len(), false),
            (Some(Tab::Changes), Icon::Diff, "Changes", self.changes.len(), false),
            (Some(Tab::History), Icon::Clock, "History", 0, false),
            (None, Icon::Branch, "Branches", self.branches.len(), false),
            (Some(Tab::Conflicts), Icon::Warn, "Conflicts", self.conflicts.len(), !self.conflicts.is_empty()),
            (None, Icon::Undo, "Operations", 0, false),
        ] {
            let on = t == Some(self.tab);
            if nav(ui, on, t.is_some(), ic, name, n, warn).clicked() {
                go = t;
            }
        }

        if !self.workers.is_empty() {
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                ui.label(RichText::new("THIS WEEK").color(MUTED).size(11.0).family(semi()));
            });
            ui.add_space(2.0);
            for w in &self.workers {
                let (rect, r) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 40.0), Sense::click());
                let p = ui.painter();
                if r.hovered() {
                    p.rect_filled(rect, RADIUS, Color32::from_white_alpha(6));
                }
                let dot = if w.agent { [ADD, ACCENT, MUTED][w.name.len() % 3] } else { PURPLE };
                let c = egui::pos2(rect.left() + 14.0, rect.center().y);
                if w.agent && dot == ADD {
                    p.circle_filled(c, 7.0, ADD_BG);
                }
                p.circle_filled(c, 4.0, dot);
                p.text(
                    egui::pos2(rect.left() + 30.0, rect.top() + 12.0),
                    Align2::LEFT_CENTER,
                    &w.name,
                    FontId::proportional(13.0),
                    TEXT,
                );
                let sub = egui::Rect::from_min_max(
                    egui::pos2(rect.left() + 30.0, rect.top() + 20.0),
                    egui::pos2(rect.right() - 28.0, rect.bottom()),
                );
                p.with_clip_rect(sub).text(
                    egui::pos2(sub.left(), rect.top() + 28.0),
                    Align2::LEFT_CENTER,
                    &w.latest,
                    FontId::proportional(12.0),
                    MUTED,
                );
                p.text(
                    egui::pos2(rect.right() - 10.0, rect.center().y),
                    Align2::RIGHT_CENTER,
                    w.count.to_string(),
                    FontId::proportional(12.0),
                    MUTED,
                );
                if r.on_hover_text("open the latest change").clicked() {
                    open = Some(w.at);
                }
            }
        }
        if let Some(t) = go {
            let first = (t == Tab::Conflicts && !self.conflicts.is_empty()).then_some(0);
            self.go(t, first);
        }
        if open.is_some() {
            self.go(Tab::History, open);
        }
    }

    fn github_card(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let idle = !self.busy();
        egui::Frame::new()
            .fill(SURFACE)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(RADIUS + 2)
            .inner_margin(Margin::same(12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 8.0;
                    if self.github.is_empty() || self.github_edit.trim() != self.github {
                        ui.label(RichText::new("GitHub").family(semi()));
                        ui.add(
                            egui::TextEdit::singleline(&mut self.github_edit)
                                .hint_text("git@github.com:you/repo.git")
                                .desired_width(f32::INFINITY),
                        );
                        let ok = idle && !self.github_edit.trim().is_empty();
                        if button_w(ui, ok, None, "Connect", Btn::Primary, ui.available_width()).clicked() {
                            let u = self.github_edit.trim().to_string();
                            self.run(&ctx, "github set", &["github", "set", &u]);
                        }
                        return;
                    }
                    let parts: Vec<&str> = self.github.trim_end_matches(".git").rsplit(['/', ':']).take(2).collect();
                    let label = format!("{}/{}", parts.get(1).unwrap_or(&""), parts[0]);
                    ui.horizontal(|ui| {
                        let link = ui.link(RichText::new(label).family(semi()).color(TEXT));
                        if link.on_hover_text(format!("{}\nclick to change", self.github)).clicked() {
                            self.github_edit = format!("{} ", self.github); // differs, so the URL field shows
                        }
                        let ssh = !self.github.starts_with("http");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                RichText::new(if ssh { "ssh" } else { "https" }).monospace().size(11.0).color(MUTED),
                            );
                        });
                    });
                    let sync = match self.ahead_behind {
                        Some((a, b)) => format!("{} is {a} ahead, {b} behind", self.branch),
                        None => "not pulled yet".into(),
                    };
                    ui.label(RichText::new(sync).color(hex(0xa1a1aa)).size(12.0));
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        let n = self.ahead_behind.map_or(0, |(a, _)| a);
                        let push = if n > 0 { format!("Push {n}") } else { "Push".into() };
                        let w = (ui.available_width() - 6.0) / 2.0;
                        if button_w(ui, idle, None, "Pull", Btn::Soft, w)
                            .on_hover_text("fetch from GitHub and merge")
                            .clicked()
                        {
                            self.run(&ctx, "GitHub pull", &["github", "pull"]);
                        }
                        if button_w(ui, idle, None, &push, Btn::Primary, w)
                            .on_hover_text("push the current branch to GitHub")
                            .clicked()
                        {
                            self.run(&ctx, "GitHub push", &["github", "push"]);
                        }
                    });
                });
            });
    }

    // -----------------------------------------------------------------------
    // Header and the four pages

    /// The page title, branch and filter on the left; undo and the page's
    /// main action on the right.
    fn header(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let idle = !self.busy();
        let mut go = None;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            if self.tab == Tab::History && self.selected.is_some() {
                let (rect, r) = ui.allocate_exact_size(Vec2::new(64.0, 28.0), Sense::click());
                paint_icon(
                    ui.painter(),
                    egui::Rect::from_center_size(rect.left_center() + Vec2::new(8.0, 0.0), Vec2::splat(14.0)),
                    Icon::Left,
                    hex(0xa1a1aa),
                );
                ui.painter().text(
                    rect.left_center() + Vec2::new(20.0, 0.0),
                    Align2::LEFT_CENTER,
                    "History",
                    FontId::proportional(13.0),
                    hex(0xa1a1aa),
                );
                if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    self.select(None);
                }
                ui.label(RichText::new("/").color(hex(0x3a3a40)));
                if let Some((id, c)) = self.selected.and_then(|i| self.history.get(i)) {
                    ui.add(
                        egui::Label::new(
                            RichText::new(c.message.lines().next().unwrap_or("")).family(semi()).size(14.0),
                        )
                        .truncate(),
                    );
                    ui.label(RichText::new(short(id)).monospace().color(MUTED));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    button(ui, false, Some(Icon::Up), "Land on main", Btn::Primary);
                    button(ui, false, None, "Approve review", Btn::Soft);
                    button(ui, false, None, "Check out", Btn::Secondary);
                });
                return;
            }
            let title = match self.tab {
                Tab::Inbox => "Inbox",
                Tab::Changes => "Changes",
                Tab::History => "History",
                Tab::Conflicts => "Conflicts",
            };
            ui.label(RichText::new(title).family(semi()).size(15.0));
            self.branch_menu(ui);
            if matches!(self.tab, Tab::Inbox | Tab::History) {
                self.who =
                    segmented(ui, self.who, &[(Who::All, "All"), (Who::Agents, "Agents"), (Who::People, "People")]);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                if self.tab != Tab::Changes && !self.changes.is_empty() {
                    let label = format!("Commit {}", plural(self.changes.len(), "file"));
                    if button(ui, idle, Some(Icon::Check), &label, Btn::Primary).clicked() {
                        go = Some(Tab::Changes);
                    }
                }
                let undo = button(ui, idle, Some(Icon::Undo), "Undo", Btn::Secondary);
                if undo.on_hover_text("undo the last operation (again = redo)").clicked() {
                    self.run(&ctx, "undo", &["undo"]);
                }
            });
        });
        if let Some(t) = go {
            self.go(t, None);
        }
    }

    /// The branch chip: switch, create, merge another branch in, refresh.
    fn branch_menu(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let idle = !self.busy();
        let galley = ui.painter().layout_no_wrap(self.branch.clone(), FontId::monospace(12.0), TEXT);
        let (rect, r) = ui.allocate_exact_size(Vec2::new(galley.size().x + 48.0, 30.0), Sense::click());
        let p = ui.painter();
        p.rect(
            rect,
            RADIUS,
            if r.hovered() { RAISED } else { SURFACE },
            Stroke::new(1.0, BORDER),
            egui::StrokeKind::Inside,
        );
        paint_icon(
            p,
            egui::Rect::from_center_size(rect.left_center() + Vec2::new(14.0, 0.0), Vec2::splat(12.0)),
            Icon::Branch,
            TEXT,
        );
        p.galley(egui::pos2(rect.left() + 26.0, rect.center().y - galley.size().y / 2.0), galley, TEXT);
        paint_icon(
            p,
            egui::Rect::from_center_size(rect.right_center() - Vec2::new(12.0, 0.0), Vec2::splat(12.0)),
            Icon::Down,
            MUTED,
        );
        let mut pick: Option<Vec<String>> = None;
        let mut merge = None;
        // typing a branch name clicks inside, so only an outside click closes it
        let popup = egui::Popup::menu(&r)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .width(rect.width().max(260.0))
            .gap(4.0);
        let mut close = false;
        popup.show(|ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 4.0;
            let label = |ui: &mut egui::Ui, t: &str| {
                ui.add_space(2.0);
                ui.label(RichText::new(t).color(MUTED).size(11.0).family(semi()));
            };
            label(ui, "SWITCH BRANCH");
            let mut names = self.branches.clone();
            if !names.contains(&self.branch) {
                names.insert(0, self.branch.clone()); // no commit yet: the branch exists only in HEAD
            }
            for b in &names {
                let on = *b == self.branch;
                if menu_item(ui, on, Icon::Branch, b).clicked() && !on {
                    pick = Some(vec![b.clone()]);
                }
            }
            if self.head.is_empty() {
                ui.label(RichText::new("Commit first, then you can create branches").color(MUTED).size(12.0));
            } else {
                ui.horizontal(|ui| {
                    let w = ui.available_width() - 76.0;
                    let edit = ui
                        .add(egui::TextEdit::singleline(&mut self.new_branch).hint_text("new branch").desired_width(w));
                    let ok = idle && !self.new_branch.trim().is_empty();
                    let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if (button_w(ui, ok, None, "Create", Btn::Soft, 70.0).clicked() || enter) && ok {
                        pick = Some(vec!["-b".into(), std::mem::take(&mut self.new_branch).trim().to_string()]);
                    }
                });
            }
            ui.separator();
            label(ui, &format!("MERGE INTO {}", self.branch.to_uppercase()));
            let others: Vec<&String> = self.branches.iter().filter(|b| **b != self.branch).collect();
            if others.is_empty() {
                ui.label(RichText::new("No other branches").color(MUTED).size(12.0));
            }
            for b in others {
                if menu_item(ui, false, Icon::Merge, b).clicked() && idle {
                    merge = Some(b.clone());
                }
            }
            ui.separator();
            if menu_item(ui, false, Icon::Undo, "Refresh").clicked() {
                self.refresh();
                close = true;
            }
        });
        if pick.is_some() || merge.is_some() || close {
            egui::Popup::close_all(&ctx);
        }
        if let Some(b) = pick {
            let args: Vec<&str> = std::iter::once("checkout").chain(b.iter().map(String::as_str)).collect();
            self.run(&ctx, &args.join(" "), &args);
        }
        if let Some(b) = merge {
            self.run(&ctx, &format!("merge {b}"), &["merge", &b]);
        }
    }

    /// Graphite's inbox: what needs you, in sections.
    fn inbox(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let go = std::cell::Cell::new(None::<(Tab, Option<usize>)>);
        let mut theirs = false;
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            ui.set_max_width(ui.available_width().min(1180.0));
            ui.spacing_mut().item_spacing.y = 14.0;
            if !self.conflicts.is_empty() {
                egui::Frame::new()
                    .fill(WARN_BG)
                    .stroke(Stroke::new(1.0, hex(0x4a3c14)))
                    .corner_radius(RADIUS + 2)
                    .inner_margin(Margin::symmetric(14, 12))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            icon(ui, Icon::Warn, 16.0, WARN);
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 2.0;
                                let n = plural(self.conflicts.len(), "conflict");
                                let what = self
                                    .history
                                    .first()
                                    .map_or("HEAD", |(_, c)| c.message.lines().next().unwrap_or("HEAD"));
                                ui.label(RichText::new(format!("{what} recorded {n}")).family(semi()));
                                let files: Vec<&str> = self.conflicts.iter().take(3).map(|c| c.path.as_str()).collect();
                                let more = if self.conflicts.len() > 3 { ", …" } else { "" };
                                let t = format!(
                                    "The merge completed. {}{more} carry markers until resolved.",
                                    files.join(", ")
                                );
                                ui.label(RichText::new(t).color(hex(0xc9b58a)).size(12.0));
                            });
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if button(ui, true, None, "Resolve", Btn::Warn).clicked() {
                                    go.set(Some((Tab::Conflicts, Some(0))));
                                }
                                if button(ui, !self.busy(), None, "Take theirs", Btn::Secondary)
                                    .on_hover_text("resolve --theirs every conflicted file")
                                    .clicked()
                                {
                                    theirs = true;
                                }
                            });
                        });
                    });
            }

            let (add, del) = self.stats.values().fold((0, 0), |(a, d), (x, y)| (a + x, d + y));
            let base = if self.head.is_empty() {
                String::new()
            } else {
                format!("uncommitted against HEAD {}", short(&self.head))
            };
            section(
                ui,
                self.changes.len(),
                "Working tree",
                |ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    ui.label(RichText::new(format!("−{del}")).color(DEL).monospace());
                    ui.label(RichText::new(format!("+{add}")).color(ADD).monospace());
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.label(RichText::new(base).color(MUTED).size(12.0));
                    });
                },
                |ui| {
                    if self.changes.is_empty() {
                        empty_row(ui, "Clean: nothing to commit");
                    }
                    for (i, d) in self.changes.iter().enumerate().take(12) {
                        let (a, dl) = self.stats.get(&d.path).copied().unwrap_or_default();
                        let r = list_row(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.add_space(24.0);
                                kind_badge(ui, d.kind);
                                let path =
                                    if d.kind == 'R' { format!("{} → {}", d.from, d.path) } else { d.path.clone() };
                                ui.add(egui::Label::new(RichText::new(path).monospace()).truncate());
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    ui.spacing_mut().item_spacing.x = 4.0;
                                    ui.label(RichText::new(format!("−{dl}")).color(DEL).monospace().size(12.0));
                                    ui.label(RichText::new(format!("+{a}")).color(ADD).monospace().size(12.0));
                                });
                            });
                        });
                        if r.clicked() {
                            go.set(Some((Tab::Changes, Some(i))));
                        }
                    }
                    let more = self.changes.len().saturating_sub(12);
                    if more > 0
                        && list_row(ui, |ui| {
                            ui.label(RichText::new(format!("{more} more…")).color(MUTED));
                        })
                        .clicked()
                    {
                        go.set(Some((Tab::Changes, None)));
                    }
                },
            );

            let review: Vec<usize> = self.unreviewed.iter().copied().filter(|&i| self.shown(i)).collect();
            section(
                ui,
                review.len(),
                "Agent changes without a passing review",
                |ui| {
                    ui.label(RichText::new("audit --require review").monospace().color(MUTED).size(12.0));
                },
                |ui| {
                    if review.is_empty() {
                        empty_row(ui, "Every agent change has a passing review");
                        return;
                    }
                    columns_header(ui);
                    for &i in review.iter().take(10) {
                        if self.change_row(ui, i).clicked() {
                            go.set(Some((Tab::History, Some(i))));
                        }
                    }
                },
            );

            let recent: Vec<usize> = (0..self.history.len()).filter(|&i| self.shown(i)).take(8).collect();
            section(
                ui,
                recent.len(),
                &format!("Recent on {}", self.branch),
                |ui| {
                    if ui.link(RichText::new("All history").color(MUTED).size(12.0)).clicked() {
                        go.set(Some((Tab::History, None)));
                    }
                },
                |ui| {
                    if recent.is_empty() {
                        empty_row(ui, "No changes yet");
                    }
                    for &i in &recent {
                        if self.landed_row(ui, i).clicked() {
                            go.set(Some((Tab::History, Some(i))));
                        }
                    }
                },
            );
        });
        if theirs {
            let mut args = vec!["resolve", "--theirs"];
            args.extend(self.conflicts.iter().map(|c| c.path.as_str()));
            let args: Vec<String> = args.into_iter().map(String::from).collect();
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            self.run(&ctx, "resolve --theirs", &args);
        }
        if let Some((t, i)) = go.get() {
            self.go(t, i);
        }
    }

    /// +N −N of a change against its first parent, worked out once.
    fn change_stats(&self, i: usize) -> (usize, usize) {
        let (id, c) = &self.history[i];
        if let Some(s) = self.change_stats.borrow().get(id) {
            return *s;
        }
        let Some(r) = &self.repo else { return (0, 0) };
        let mut out = vec![];
        // ponytail: one diff per row on first draw; a long History of huge changes stutters once
        let _ = r
            .tree_of(c.parents.first().map_or("", String::as_str))
            .and_then(|pt| r.diff_trees(&pt, &c.tree))
            .and_then(|ds| sprig::cli::render_diff(r, &mut out, ds, RENAME_THRESHOLD, None));
        let s =
            Diff::parse(&String::from_utf8_lossy(&out)).counts.values().fold((0, 0), |(a, d), (x, y)| (a + x, d + y));
        self.change_stats.borrow_mut().insert(id.clone(), s);
        s
    }

    /// A change as a Graphite inbox row: avatar, title, who and how, then
    /// the Checks, Changes and Updated columns.
    fn change_row(&self, ui: &mut egui::Ui, i: usize) -> egui::Response {
        let (id, c) = &self.history[i];
        let agent = !c.meta.agent.is_empty();
        let who = c.author.split(" <").next().unwrap_or("");
        let (add, del) = self.change_stats(i);
        list_row(ui, |ui| {
            ui.horizontal(|ui| {
                avatar(ui, if agent { &c.meta.agent } else { who }, agent, 26.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_sized(
                        [COL_UPDATED, 20.0],
                        egui::Label::new(RichText::new(ago_short(now() - c.time)).color(hex(0xa1a1aa)).size(12.0)),
                    );
                    ui.allocate_ui_with_layout(
                        Vec2::new(COL_CHANGES, 20.0),
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            ui.spacing_mut().item_spacing.x = 3.0;
                            ui.label(RichText::new(format!("−{del}")).color(DEL).monospace().size(12.0));
                            ui.label(RichText::new("/").color(MUTED).monospace().size(12.0));
                            ui.label(RichText::new(format!("+{add}")).color(ADD).monospace().size(12.0));
                        },
                    );
                    ui.allocate_ui_with_layout(
                        Vec2::new(COL_CHECKS, 22.0),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            for (k, v) in c.meta.checks.iter().take(2) {
                                check(ui, k, v);
                            }
                            if agent && !c.meta.checks.contains_key("review") {
                                check(ui, "review", "?");
                            }
                        },
                    );
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 1.0;
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(c.message.lines().next().unwrap_or("")).family(semi()),
                                    )
                                    .truncate(),
                                );
                                if c.parents.len() > 1 {
                                    pill(ui, "merge", PURPLE, hex(0x2a1f3d));
                                }
                                if !c.conflicts.is_empty() {
                                    pill(ui, &plural(c.conflicts.len(), "conflict"), WARN, WARN_BG);
                                }
                            });
                            let mut meta = vec![];
                            if agent {
                                meta.push(
                                    format!("{} · {}", c.meta.agent, c.meta.model).trim_end_matches(" · ").to_string(),
                                );
                                if !c.meta.intent.is_empty() {
                                    meta.push(format!("intent: {}", c.meta.intent));
                                }
                            } else {
                                meta.push(who.to_string());
                            }
                            if c.sig.is_some() {
                                meta.push("signed".into());
                            }
                            meta.push(short(id).to_string());
                            ui.add(
                                egui::Label::new(RichText::new(meta.join(" · ")).color(MUTED).size(12.0)).truncate(),
                            );
                        });
                    });
                });
            });
        })
    }

    /// A change as the design's "landed" row: merge mark, title, id, who, age.
    fn landed_row(&self, ui: &mut egui::Ui, i: usize) -> egui::Response {
        let (id, c) = &self.history[i];
        let who = if c.meta.agent.is_empty() { c.author.split(" <").next().unwrap_or("") } else { &c.meta.agent };
        list_row(ui, |ui| {
            ui.horizontal(|ui| {
                icon(ui, Icon::Merge, 14.0, PURPLE);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_sized(
                        [48.0, 18.0],
                        egui::Label::new(RichText::new(ago_short(now() - c.time)).color(hex(0xa1a1aa)).size(12.0)),
                    );
                    ui.label(RichText::new(who).color(MUTED).size(12.0));
                    ui.label(RichText::new(short(id)).monospace().color(MUTED).size(12.0));
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.add(egui::Label::new(c.message.lines().next().unwrap_or("")).truncate());
                    });
                });
            });
        })
    }

    /// The working tree: files and commit on the left, the diff on the right.
    fn changes_page(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("files")
            .resizable(true)
            .show_separator_line(false)
            .default_size(340.0)
            .frame(egui::Frame::new().inner_margin(Margin { right: 20, ..Margin::ZERO }))
            .show(ui, |ui| {
                egui::Panel::bottom("commit").frame(egui::Frame::new()).show(ui, |ui| self.commit_box(ui));
                let mut pick = None;
                egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    if self.changes.is_empty() {
                        empty_row(ui, "Working tree clean");
                    }
                    for (i, d) in self.changes.iter().enumerate() {
                        let on = self.selected == Some(i);
                        if file_row(ui, d, self.stats.get(&d.path).copied().unwrap_or_default(), on).clicked() {
                            // a second click goes back to all of them
                            pick = Some(if on { None } else { Some(i) });
                        }
                    }
                });
                if let Some(p) = pick {
                    self.select(p);
                }
            });
        self.diff_view(ui);
    }

    /// All changes on the branch; a picked one opens as its own page.
    fn history_page(&mut self, ui: &mut egui::Ui) {
        if self.selected.is_some() {
            return self.change_page(ui);
        }
        let mut pick = None;
        let shown: Vec<usize> = (0..self.history.len()).filter(|&i| self.shown(i)).collect();
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            ui.set_max_width(ui.available_width().min(1180.0));
            section(
                ui,
                shown.len(),
                &format!("Changes on {}", self.branch),
                |_| {},
                |ui| {
                    if shown.is_empty() {
                        empty_row(ui, "No changes yet");
                        return;
                    }
                    columns_header(ui);
                    for &i in &shown {
                        if self.change_row(ui, i).clicked() {
                            pick = Some(i);
                        }
                    }
                },
            );
        });
        if pick.is_some() {
            self.select(pick);
        }
    }

    /// Cursor's agent page: provenance on the left, the diff on the right.
    fn change_page(&mut self, ui: &mut egui::Ui) {
        let Some((id, c)) = self.selected.and_then(|i| self.history.get(i)).cloned() else { return };
        let mut resolve = false;
        egui::Panel::left("provenance")
            .resizable(true)
            .show_separator_line(false)
            .default_size(380.0)
            .frame(egui::Frame::new().inner_margin(Margin { right: 20, ..Margin::ZERO }))
            .show(ui, |ui| {
                egui::Panel::bottom("follow-up")
                    .frame(egui::Frame::new().inner_margin(Margin { top: 10, ..Margin::ZERO }))
                    .show(ui, |ui| {
                        follow_up(ui, &c);
                    });
                egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 6.0;
                    let agent = !c.meta.agent.is_empty();
                    let who = c.author.split(" <").next().unwrap_or("");
                    ui.horizontal(|ui| {
                        avatar(ui, if agent { &c.meta.agent } else { who }, agent, 32.0);
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 5.0;
                                if agent {
                                    ui.label(RichText::new(&c.meta.agent).family(semi()));
                                    ui.label(RichText::new(format!("for {who}")).color(MUTED));
                                } else {
                                    ui.label(RichText::new(who).family(semi()));
                                }
                            });
                            let mut sub = vec![];
                            if !c.meta.model.is_empty() {
                                sub.push(c.meta.model.clone());
                            }
                            sub.push(ago(now() - c.time));
                            sub.push(format!("on {}", self.branch));
                            ui.label(RichText::new(sub.join(" · ")).color(MUTED).size(12.0))
                                .on_hover_text(fmt_date(c.time));
                        });
                    });
                    let body = c.message.trim().lines().skip(1).collect::<Vec<_>>().join("\n");
                    if !body.trim().is_empty() {
                        ui.add_space(4.0);
                        ui.label(RichText::new(body.trim()).color(MUTED));
                    }

                    if !c.meta.intent.is_empty() {
                        heading(ui, "INTENT");
                        egui::Frame::new()
                            .fill(SURFACE)
                            .stroke(Stroke::new(1.0, BORDER))
                            .corner_radius(RADIUS + 2)
                            .inner_margin(Margin::same(12))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.label(RichText::new(&c.meta.intent).color(hex(0xd4d4da)));
                            });
                    }

                    heading(ui, "CHECKS");
                    egui::Frame::new().fill(SIDEBAR).stroke(Stroke::new(1.0, BORDER)).corner_radius(RADIUS + 2).show(
                        ui,
                        |ui| {
                            ui.set_width(ui.available_width());
                            ui.spacing_mut().item_spacing.y = 0.0;
                            let mut checks: Vec<(String, String)> =
                                c.meta.checks.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                            if agent && !c.meta.checks.contains_key("review") {
                                checks.push(("review".into(), "required by policy".into()));
                            }
                            if checks.is_empty() {
                                empty_row(ui, "No checks recorded");
                            }
                            let n = checks.len();
                            for (j, (k, v)) in checks.iter().enumerate() {
                                let (rect, _) =
                                    ui.allocate_exact_size(Vec2::new(ui.available_width(), 38.0), Sense::hover());
                                let p = ui.painter();
                                let (ic, fg, bg) = match check_state(v) {
                                    Some(true) => (Icon::Check, BG, ADD),
                                    Some(false) => (Icon::Cross, BG, DEL),
                                    None => (Icon::Warn, hex(0x1a1406), WARN),
                                };
                                let cpos = egui::pos2(rect.left() + 21.0, rect.center().y);
                                p.circle_filled(cpos, 9.0, bg);
                                if matches!(ic, Icon::Warn) {
                                    p.text(cpos, Align2::CENTER_CENTER, "!", FontId::new(12.0, semi()), fg);
                                } else {
                                    paint_icon(p, egui::Rect::from_center_size(cpos, Vec2::splat(11.0)), ic, fg);
                                }
                                let state = if check_state(v).is_some() { format!("{k}={v}") } else { k.clone() };
                                p.text(
                                    egui::pos2(rect.left() + 40.0, rect.center().y),
                                    Align2::LEFT_CENTER,
                                    state,
                                    FontId::monospace(12.5),
                                    TEXT,
                                );
                                let note = if check_state(v).is_some() { "" } else { v.as_str() };
                                p.text(
                                    egui::pos2(rect.right() - 12.0, rect.center().y),
                                    Align2::RIGHT_CENTER,
                                    note,
                                    FontId::proportional(12.0),
                                    hex(0xa1a1aa),
                                );
                                if j + 1 < n {
                                    p.line_segment(
                                        [rect.left_bottom(), rect.right_bottom()],
                                        Stroke::new(1.0, hex(0x1d1d21)),
                                    );
                                }
                            }
                        },
                    );

                    heading(ui, "SIGNATURE");
                    let (title, detail, fg, bg, border) = match &self.signature {
                        Some(Ok(who)) if !who.starts_with("untrusted") => {
                            ("Signed by a trusted key".to_string(), who.clone(), hex(0x7ee08a), ADD_BG, hex(0x1f4a2c))
                        }
                        Some(Ok(k)) => {
                            ("Signed, but not by a trusted key".to_string(), k.clone(), WARN, WARN_BG, hex(0x4a3c14))
                        }
                        Some(Err(e)) if e == "unsigned" => (
                            "Unsigned".to_string(),
                            "sprig keygen signs your changes".into(),
                            hex(0xa1a1aa),
                            SURFACE,
                            BORDER,
                        ),
                        Some(Err(e)) => ("Bad signature".to_string(), e.clone(), DEL, DEL_BG, hex(0x5a2427)),
                        None => (String::new(), String::new(), MUTED, SURFACE, BORDER),
                    };
                    egui::Frame::new()
                        .fill(bg)
                        .stroke(Stroke::new(1.0, border))
                        .corner_radius(RADIUS + 2)
                        .inner_margin(Margin::symmetric(12, 10))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                icon(ui, Icon::Shield, 15.0, fg);
                                ui.vertical(|ui| {
                                    ui.spacing_mut().item_spacing.y = 0.0;
                                    ui.label(RichText::new(title).family(semi()).color(fg));
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(detail).monospace().size(12.0).color(fg.gamma_multiply(0.8)),
                                        )
                                        .truncate(),
                                    );
                                });
                            });
                        });

                    heading(ui, "CHANGE");
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(short(&id)).monospace());
                        if ui.small_button("Copy id").on_hover_text(&id).clicked() {
                            ui.ctx().copy_text(id.clone());
                        }
                        if c.parents.len() > 1 {
                            pill(ui, "merge", PURPLE, hex(0x2a1f3d));
                        }
                        let parents: Vec<&str> = c.parents.iter().map(|p| short(p)).collect();
                        if !parents.is_empty() {
                            ui.label(RichText::new(format!("parent {}", parents.join(", "))).color(MUTED).size(12.0));
                        }
                    });
                    if !c.conflicts.is_empty() {
                        ui.horizontal(|ui| {
                            pill(ui, &plural(c.conflicts.len(), "conflict"), WARN, WARN_BG);
                            ui.label(RichText::new("recorded in this change").color(MUTED).size(12.0));
                            if id == self.head && button(ui, true, None, "Resolve", Btn::Warn).clicked() {
                                resolve = true;
                            }
                        });
                    }
                });
            });
        // the diff side: Cursor's tab bar over the diff
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let files = self.diff.counts.len();
            let tabs = [
                ("Diff".to_string(), true),
                (format!("Files {files}"), false),
                (format!("Conflicts {}", c.conflicts.len()), false),
                ("Raw".into(), false),
            ];
            for (name, on) in tabs {
                let b = egui::Button::new(RichText::new(name).color(if on { TEXT } else { MUTED }))
                    .fill(if on { RAISED } else { Color32::TRANSPARENT })
                    .stroke(Stroke::NONE)
                    .min_size(Vec2::new(0.0, 28.0));
                let r = ui.add_enabled(on, b);
                if !on {
                    r.on_disabled_hover_text("coming soon");
                }
            }
        });
        ui.add_space(6.0);
        self.diff_view(ui);
        if resolve {
            self.go(Tab::Conflicts, Some(0));
        }
    }

    fn conflicts_page(&mut self, ui: &mut egui::Ui) {
        let mut pick = None;
        ui.horizontal_wrapped(|ui| {
            for (i, cf) in self.conflicts.iter().enumerate() {
                let on = self.selected == Some(i);
                let b = egui::Button::new(RichText::new(&cf.path).monospace().color(if on { TEXT } else { MUTED }))
                    .fill(if on { SELECTED } else { SURFACE });
                if ui.add(b).clicked() {
                    pick = Some(i);
                }
            }
        });
        if pick.is_some() {
            self.select(pick);
        }
        ui.add_space(6.0);
        self.conflict_view(ui);
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

    /// The diff toolbar (files and line counts, Unified/Split) and the lines.
    fn diff_view(&mut self, ui: &mut egui::Ui) {
        if !self.detail_error.is_empty() {
            ui.colored_label(DEL, &self.detail_error);
        }
        if self.diff.unified.is_empty() {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("Nothing to show").color(MUTED));
            });
            return;
        }
        let (files, add, del) = self.diff.counts.values().fold((0, 0, 0), |(f, a, d), (x, y)| (f + 1, a + x, d + y));
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{} changed", plural(files, "file"))).family(semi()));
            counts(ui, add, del);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.split = segmented(ui, self.split, &[(false, "Unified"), (true, "Split")]);
            });
        });
        ui.add_space(4.0);
        let mut files: Vec<(&String, &(usize, usize))> = self.diff.counts.iter().collect();
        files.sort();
        let mut open = None;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
            for (path, (a, d)) in files.iter().take(12) {
                let mut job = egui::text::LayoutJob::default();
                let fmt = |size: f32, color| egui::TextFormat {
                    font_id: FontId::monospace(size),
                    color,
                    ..Default::default()
                };
                job.append(path, 0.0, fmt(12.0, TEXT));
                job.append(&format!("+{a}"), 8.0, fmt(11.5, ADD));
                job.append(&format!("−{d}"), 6.0, fmt(11.5, DEL));
                let b = egui::Button::new(job)
                    .fill(SURFACE)
                    .stroke(Stroke::new(1.0, BORDER))
                    .min_size(Vec2::new(0.0, 28.0));
                if ui.add(b).clicked() {
                    open = Some(path.to_string());
                }
            }
            if files.len() > 12 {
                ui.label(RichText::new(format!("+{} more", files.len() - 12)).color(MUTED).size(12.0));
            }
        });
        if let Some(p) = open.filter(|_| self.tab == Tab::Changes) {
            let i = self.changes.iter().position(|d| d.path == p);
            self.select(i);
            return;
        }
        ui.add_space(6.0);
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
        ui.label(RichText::new(&cf.path).monospace().family(semi()).size(14.0));
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
                            ui.label(RichText::new(*name).family(semi()).color(*color));
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
            ui.label(RichText::new("Merged result").family(semi()));
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

/// "3m", "2h", "5d": Graphite's Updated column.
fn ago_short(secs: i64) -> String {
    match secs.max(0) {
        s if s < 60 => "now".into(),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86400 => format!("{}h", s / 3600),
        s if s < 86400 * 30 => format!("{}d", s / 86400),
        s if s < 86400 * 365 => format!("{}mo", s / (86400 * 30)),
        s => format!("{}y", s / (86400 * 365)),
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

/// Widths of the Checks, Changes and Updated columns of a change list.
const COL_CHECKS: f32 = 230.0;
const COL_CHANGES: f32 = 96.0;
const COL_UPDATED: f32 = 56.0;

/// The column names over a change list, aligned with `change_row`.
fn columns_header(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 30.0), Sense::hover());
    let p = ui.painter();
    let f = FontId::proportional(12.0);
    let y = rect.center().y;
    let right = rect.right() - 14.0;
    p.text(egui::pos2(rect.left() + 14.0, y), Align2::LEFT_CENTER, "Change", f.clone(), MUTED);
    // change_row lays out right to left with 8 px between columns
    p.text(egui::pos2(right, y), Align2::RIGHT_CENTER, "Updated", f.clone(), MUTED);
    p.text(egui::pos2(right - COL_UPDATED - 8.0, y), Align2::RIGHT_CENTER, "Changes", f.clone(), MUTED);
    p.text(
        egui::pos2(right - COL_UPDATED - COL_CHANGES - 16.0 - COL_CHECKS, y),
        Align2::LEFT_CENTER,
        "Checks",
        f,
        MUTED,
    );
    p.line_segment([rect.left_bottom(), rect.right_bottom()], Stroke::new(1.0, hex(0x1d1d21)));
}

/// The composer under a change, for asking its agent for a follow-up.
/// Not wired up yet, so it is drawn disabled.
fn follow_up(ui: &mut egui::Ui, c: &Change) {
    egui::Frame::new()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(RADIUS + 4)
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new("Ask the agent for a follow-up change").color(MUTED).size(12.0));
            let mut hint = String::new();
            ui.add_enabled(
                false,
                egui::TextEdit::multiline(&mut hint)
                    .hint_text("e.g. also cover this in the tests")
                    .frame(egui::Frame::NONE)
                    .desired_rows(2)
                    .desired_width(f32::INFINITY),
            )
            .on_disabled_hover_text("coming soon");
            ui.horizontal(|ui| {
                let who = if c.meta.agent.is_empty() {
                    "agent".to_string()
                } else {
                    format!("{} {}", c.meta.agent, c.meta.model)
                };
                egui::Frame::new()
                    .stroke(Stroke::new(1.0, BORDER))
                    .corner_radius(RADIUS)
                    .inner_margin(Margin::symmetric(8, 3))
                    .show(ui, |ui| {
                        ui.label(RichText::new(who.trim()).color(hex(0xa1a1aa)).size(12.0));
                    });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (rect, r) = ui.allocate_exact_size(Vec2::splat(30.0), Sense::hover());
                    ui.painter().circle_filled(rect.center(), 15.0, TEXT.gamma_multiply(0.45));
                    paint_icon(ui.painter(), rect.shrink(8.0), Icon::Up, BG);
                    r.on_hover_text("coming soon");
                });
            });
        });
}

/// A small uppercase label over a group in the change page.
fn heading(ui: &mut egui::Ui, text: &str) {
    ui.add_space(10.0);
    ui.label(RichText::new(text).color(MUTED).size(11.0).family(semi()));
}

fn empty_row(ui: &mut egui::Ui, text: &str) {
    egui::Frame::new().inner_margin(Margin::symmetric(16, 10)).show(ui, |ui| {
        ui.label(RichText::new(text).color(MUTED));
    });
}

/// A working-tree file: kind, path, +N −N.
fn file_row(ui: &mut egui::Ui, d: &Delta, (a, del): (usize, usize), on: bool) -> egui::Response {
    row(ui, on, |ui| {
        ui.horizontal(|ui| {
            kind_badge(ui, d.kind);
            let path = if d.kind == 'R' { format!("{} → {}", d.from, d.path) } else { d.path.clone() };
            ui.add(egui::Label::new(RichText::new(path).monospace()).truncate());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                ui.label(RichText::new(format!("−{del}")).color(DEL).monospace());
                ui.label(RichText::new(format!("+{a}")).color(ADD).monospace());
            });
        });
    })
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
        egui::Panel::left("sidebar")
            .resizable(true)
            .default_size(264.0)
            .frame(egui::Frame::new().fill(SIDEBAR).inner_margin(Margin::same(12)).stroke(Stroke::new(1.0, BORDER)))
            .show(ui, |ui| self.sidebar(ui));
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
        egui::Panel::top("header")
            .frame(bar(BG).inner_margin(Margin::symmetric(20, 10)))
            .show(ui, |ui| self.header(ui));
        egui::CentralPanel::default().frame(egui::Frame::new().fill(BG).inner_margin(Margin::symmetric(20, 16))).show(
            ui,
            |ui| match self.tab {
                Tab::Inbox => self.inbox(ui),
                Tab::Changes => self.changes_page(ui),
                Tab::History => self.history_page(ui),
                Tab::Conflicts => self.conflicts_page(ui),
            },
        );
    }
}
