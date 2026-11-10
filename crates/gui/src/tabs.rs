//! UI per tab: addon list, discovery, settings, log.
//!
//! All tabs collect deferred actions (Cmd / mutations of AppGui fields) and apply
//! them after table rows render, keeping egui's borrow model happy.

use egui_extras::{Column, TableBuilder};
use addons_forever_core::config::{AppConfig, Theme};
use addons_forever_core::flavor::Flavor;
use addons_forever_core::types::{Channel, ProviderKind};

use crate::app::{AppGui, Tab};
use crate::worker::Cmd;

/// Embedded artwork (docs/ART.md, assets/) — used for empty states, the update
/// badge and the About banner.
pub const ART_EMPTY_ADDONS: &[u8] = include_bytes!("../../../assets/empty-addons.png");
pub const ART_EMPTY_DISCOVER: &[u8] = include_bytes!("../../../assets/empty-discover.png");
pub const ART_EMPTY_LOG: &[u8] = include_bytes!("../../../assets/empty-log.png");
pub const ART_BADGE_UPDATE: &[u8] = include_bytes!("../../../assets/badge-update.png");

pub fn art(bytes: &'static [u8], uri: &'static str, max_w: f32, max_h: f32) -> egui::Image<'static> {
    egui::Image::from_bytes(uri, bytes).max_size(egui::vec2(max_w, max_h))
}

/// Status text colors, readable on the active theme's background (egui follows
/// the OS appearance, so light mode needs darker tones than dark mode).
pub enum Tone {
    Good,
    Warn,
    Bad,
}

pub fn tone(ui: &egui::Ui, tone: Tone) -> egui::Color32 {
    let dark = ui.visuals().dark_mode;
    match tone {
        Tone::Good if dark => egui::Color32::from_rgb(140, 230, 140),
        Tone::Good => egui::Color32::from_rgb(30, 125, 50),
        Tone::Warn if dark => egui::Color32::GOLD,
        Tone::Warn => egui::Color32::from_rgb(160, 100, 0),
        Tone::Bad if dark => egui::Color32::from_rgb(255, 130, 130),
        Tone::Bad => egui::Color32::from_rgb(185, 30, 30),
    }
}

enum Act {
    OpenMenu(String),
    ConfirmUninstall(String, String),
}

pub fn sort_addons(
    rows: &mut [addons_forever_core::store::AddonState],
    col: usize,
    asc: bool,
) {
    use addons_forever_core::store::AddonState;
    fn versionish(a: &AddonState, available: bool) -> (Vec<u64>, String) {
        let v = if available {
            a.pending_update
                .as_ref()
                .map(|p| p.version.clone())
                .or_else(|| {
                    a.cached_remote
                        .as_ref()
                        .and_then(|m| m.files.first().map(|f| f.version.clone()))
                })
        } else {
            a.installed_version.clone()
        }
        .unwrap_or_default();
        (addons_forever_core::util::version_key(&v), v.to_lowercase())
    }
    rows.sort_by(|a, b| {
        let ord = match col {
            1 => versionish(a, false).cmp(&versionish(b, false)),
            2 => versionish(a, true).cmp(&versionish(b, true)),
            3 => a.size_bytes.cmp(&b.size_bytes),
            4 => (a.provider.as_str(), a.name.to_lowercase())
                .cmp(&(b.provider.as_str(), b.name.to_lowercase())),
            _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        };
        if asc {
            ord
        } else {
            ord.reverse()
        }
    });
}

pub fn addons_tab(app: &mut AppGui, ui: &mut egui::Ui) {
    let rows = app.addons_filtered();
    let updates = app.updates.clone();

    if rows.is_empty() {
        ui.vertical_centered(|ui| {
            ui.add_space(40.0);
            if app.addons.is_empty() {
                ui.add(art(ART_EMPTY_ADDONS, "bytes://empty-addons.png", 96.0, 96.0));
                ui.add_space(6.0);
                ui.heading("No addons tracked yet");
                ui.label(
                    "Point addons-forever at your WoW AddOns folder (Settings, then press Save settings), then press Scan. \
                     Manually installed addons are fingerprint-matched to CurseForge, Tukui, GitHub, MMOUI and WowUp Hub.",
                );
                ui.add_space(8.0);
                if ui.button("Open Settings").clicked() {
                    app.tab = Tab::Settings;
                }
            } else {
                ui.label("No addons match the filter.");
            }
        });
        return;
    }

    let mut cmds: Vec<Cmd> = Vec::new();
    let mut acts: Vec<Act> = Vec::new();
    let mut sort_req: Option<usize> = None;
    let (sort_col, sort_dir) = (app.sort_col, app.sort_dir);
    let busy = app.busy.clone();
    let mut auto_all: Option<bool> = None;

    // Column separators: egui_extras paints an idle guide (noninteractive
    // stroke) and a full-height hover line (hovered stroke). Both are hidden;
    // the header cells draw their own short resize affordance (resize_strip).
    ui.visuals_mut().widgets.noninteractive.bg_stroke = egui::Stroke::NONE;
    ui.visuals_mut().widgets.hovered.bg_stroke = egui::Stroke::NONE;
    // Proportional layout with content floors: every column shows its text
    // unwrapped and un-truncated when the window allows, columns grow and
    // shrink with the window, and the table spans the full panel width.
    // The Auto column is a fixed-width control column.
    const AUTO_W: f32 = 58.0;
    const ACTIONS_W: f32 = 305.0;
    // floors (col 1..6): below these, labels start truncating
    const ADDONS_MIN: [f32; 6] = [120.0, 64.0, 64.0, 48.0, 56.0, ACTIONS_W];
    if app.addons_cols.is_empty() {
        app.addons_cols = vec![260.0, 78.0, 84.0, 64.0, 70.0, ACTIONS_W];
    }
    // content floor per column: widest visible label (cells use .small(),
    // headers the body font; + cell padding)
    let pad = CELL_PAD;
    let mut content: [f32; 6] = [120.0, 64.0, 64.0, 48.0, 56.0, ACTIONS_W];    for (slot, label) in content.iter_mut().zip(["Addon", "Installed", "Available", "Size", "Source", "Actions"]) {
        *slot = (*slot).max(est_text_w(ui, label, false, false) + pad);
    }
    for a in &rows {
        content[0] = content[0].max(est_text_w(ui, &a.name, false, true) + pad);
        content[1] = content[1].max(est_text_w(ui, a.installed_version.as_deref().unwrap_or("-"), true, false) + pad);
        let avail_v = a
            .pending_update
            .as_ref()
            .map(|p| p.version.clone())
            .or_else(|| a.cached_remote.as_ref().and_then(|m| m.files.first()).map(|f| f.version.clone()))
            .unwrap_or_default();
        content[2] = content[2].max(est_text_w(ui, &avail_v, true, true) + pad);
        content[3] = content[3].max(est_text_w(ui, &fmt_size(a.size_bytes), true, false) + pad);
        content[4] = content[4].max(est_text_w(ui, a.provider.label(), true, false) + pad);
    }

    let spacing = ui.spacing().item_spacing.x;
    let table_avail = ui.available_width() - AUTO_W - spacing;
    let (w, wscale) = fit_widths(&app.addons_cols, &content, &ADDONS_MIN, table_avail, spacing);
    let mut drag = [0.0f32; 6];

    TableBuilder::new(ui)
        .striped(true)
        .id_salt("addons-table")
        .column(Column::exact(AUTO_W))
        .column(Column::exact(w[0]))
        .column(Column::exact(w[1]))
        .column(Column::exact(w[2]))
        .column(Column::exact(w[3]))
        .column(Column::exact(w[4]))
        .column(Column::exact(w[5]))
        .header(22.0, |mut h| {
            h.col(|ui| {
                let all_on = !rows.is_empty() && rows.iter().all(|a| a.auto_update);
                let mut on = all_on;
                if ui.checkbox(&mut on, "Auto").on_hover_text("Toggle automatic updates for all addons").changed() {
                    auto_all = Some(on);
                }
                if let Some(dx) = resize_strip(ui) {
                    drag[0] = dx;
                }
            });
            h.col(|ui| {
                if header_cell(ui, sort_col, sort_dir, 0, "Addon").clicked() {
                    sort_req = Some(0);
                }
                if let Some(dx) = resize_strip(ui) {
                    drag[1] = dx;
                }
            });
            h.col(|ui| {
                if header_cell(ui, sort_col, sort_dir, 1, "Installed").clicked() {
                    sort_req = Some(1);
                }
                if let Some(dx) = resize_strip(ui) {
                    drag[2] = dx;
                }
            });
            h.col(|ui| {
                if header_cell(ui, sort_col, sort_dir, 2, "Available").clicked() {
                    sort_req = Some(2);
                }
                if let Some(dx) = resize_strip(ui) {
                    drag[3] = dx;
                }
            });
            h.col(|ui| {
                if header_cell(ui, sort_col, sort_dir, 3, "Size").clicked() {
                    sort_req = Some(3);
                }
                if let Some(dx) = resize_strip(ui) {
                    drag[4] = dx;
                }
            });
            h.col(|ui| {
                if header_cell(ui, sort_col, sort_dir, 4, "Source").clicked() {
                    sort_req = Some(4);
                }
                if let Some(dx) = resize_strip(ui) {
                    drag[5] = dx;
                }
            });
            h.col(|ui| { ui.weak("Actions"); });
        })
        .body(|body| {
            body.rows(38.0, rows.len(), |mut row| {
                let a = rows[row.index()].clone();
                let has_update = updates.contains(&a.key);

                row.col(|ui| {
                    let mut auto = a.auto_update;
                    if ui.checkbox(&mut auto, "").on_hover_text("Automatic updates for this addon").changed() {
                        cmds.push(Cmd::SetFlags { key: a.key.clone(), auto: Some(auto), ignored: None, channel: None });
                    }
                });

                row.col(|ui| {
                    let mut txt = egui::RichText::new(a.name.clone()).strong();
                    if a.ignored {
                        txt = txt.strikethrough().weak();
                    }
                    ui.add(egui::Label::new(txt).truncate().selectable(false));
                    let mut sub = a.folders.join(", ");
                    if a.manual {
                        if !sub.is_empty() {
                            sub.push_str("  -  ");
                        }
                        sub.push_str("manual source");
                    }
                    if !sub.is_empty() {
                        let sub = cap_chars(&sub, 96);
                        ui.add(egui::Label::new(egui::RichText::new(sub).small().weak()).truncate().selectable(false));
                    }
                });

                row.col(|ui| {
                    let v = a.installed_version.clone().unwrap_or_else(|| "-".into());
                    ui.add(egui::Label::new(egui::RichText::new(v).small()).truncate().selectable(false));
                });

                row.col(|ui| {
                    if has_update {
                        let v = a.pending_update.as_ref().map(|p| p.version.clone()).unwrap_or_default();
                        ui.add(
                            egui::Label::new(egui::RichText::new(v).color(tone(ui, Tone::Good)).small().strong())
                                .truncate(),
                        );
                    } else {
                        let latest = a
                            .cached_remote
                            .as_ref()
                            .and_then(|m| m.files.first())
                            .map(|f| f.version.clone())
                            .unwrap_or_default();
                        ui.add(egui::Label::new(egui::RichText::new(latest).weak().small()).truncate());
                    }
                });

                row.col(|ui| {
                    let sz = fmt_size(a.size_bytes);
                    ui.add(egui::Label::new(egui::RichText::new(sz).small().weak()).truncate().selectable(false));
                });

                row.col(|ui| {
                    ui.add(egui::Label::new(egui::RichText::new(a.provider.label()).small().weak()).truncate().selectable(false));
                });

                row.col(|ui| {
                    if let Some((phase, frac)) = busy.get(&a.key) {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            let txt = if *frac > 0.0 {
                                format!("{phase} {:.0}%", frac * 100.0)
                            } else {
                                phase.clone()
                            };
                            ui.add(egui::Label::new(egui::RichText::new(txt).small()).selectable(false));
                        });
                        return;
                    }
                    ui.horizontal(|ui| {
                        ui.spacing_mut().button_padding = egui::vec2(5.0, 2.0);
                        ui.spacing_mut().item_spacing = egui::vec2(3.0, 3.0);
                        if has_update
                            && ui.small_button("Update").on_hover_text(format!("Install update for {}", a.name)).clicked()
                        {
                            cmds.push(Cmd::ApplyKey(a.key.clone()));
                        }
                        if ui.small_button("Log").on_hover_text("Changelog").clicked() {
                            cmds.push(Cmd::Changelog { key: a.key.clone() });
                        }
                        if let Some(url) = a.site_url.clone().or(a.repo_url.clone()) {
                            if ui.small_button("Web").on_hover_text("Open project page").clicked() {
                                let _ = open::that(&url);
                            }
                        }
                        if ui.small_button("Options").on_hover_text("Options: channel / ignore / source").clicked() {
                            acts.push(Act::OpenMenu(a.key.clone()));
                        }
                        if ui.small_button("Remove").on_hover_text("Uninstall").clicked() {
                            acts.push(Act::ConfirmUninstall(a.key.clone(), a.name.clone()));
                        }
                    });
                });
            });
        });

    for (i, &dx) in drag.iter().enumerate() {
        if dx == 0.0 || wscale <= 0.0 {
            continue;
        }
        if i == 0 {
            // boundary against the fixed Auto column: only the Addon column changes
            let floor = ADDONS_MIN[0] / wscale;
            app.addons_cols[0] = (app.addons_cols[0] + dx / wscale).max(floor);
        } else {
            apply_boundary_drag(&mut app.addons_cols, &ADDONS_MIN, wscale, i - 1, dx);
        }
    }

    if let Some(on) = auto_all {
        cmds.push(Cmd::SetAutoAll(on));
    }
    for c in cmds {
        app.send(c);
    }
    for act in acts {
        match act {
            Act::OpenMenu(key) => app.addon_menu = Some(key),
            Act::ConfirmUninstall(k, n) => app.confirm_uninstall = Some((k, n)),
        }
    }
    if let Some(col) = sort_req {
        if app.sort_col == col {
            app.sort_dir = !app.sort_dir;
        } else {
            app.sort_col = col;
            app.sort_dir = col != 0; // newest version first, name A-Z
        }
    }
}

/// Short resize affordance drawn inside the header cell's right border while
/// hovered. The full-column guide lines are hidden; dragging the border itself
/// (which egui_extras tracks across the table height) resizes the column.
fn resize_strip(ui: &mut egui::Ui) -> Option<f32> {
    let r = ui.max_rect();
    let x = r.max.x + ui.spacing().item_spacing.x * 0.5;
    let strip = egui::Rect::from_min_max(
        egui::pos2(x - 5.0, r.min.y),
        egui::pos2(x + 5.0, r.max.y),
    );
    let resp = ui.interact(strip, ui.scope_id().with("hdr-resize-strip"), egui::Sense::drag());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeColumn);
        let stroke = egui::Stroke::new(1.0, egui::Color32::from_gray(140));
        ui.painter()
            .line_segment([egui::pos2(x, r.min.y), egui::pos2(x, r.max.y)], stroke);
    }
    if resp.dragged() {
        Some(resp.drag_delta().x)
    } else {
        None
    }
}

/// Width of `text` rendered in the table's cell fonts, used to measure the
/// content floor of each column.
fn est_text_w(ui: &egui::Ui, text: &str, small: bool, strong: bool) -> f32 {
    let style = if small { egui::TextStyle::Small } else { egui::TextStyle::Body };
    let font_id = style.resolve(ui.style());
    let w = ui.painter().layout_no_wrap(text.to_string(), font_id, egui::Color32::PLACEHOLDER).rect.width();
    if strong {
        w * 1.06
    } else {
        w
    }
}

/// Cell padding budget around measured text (cell margins + a little slack).
const CELL_PAD: f32 = 14.0;

/// Column widths for `avail` (inclusive of spacing): each column's display
/// width is at least its measured content width (nothing valuable is
/// truncated) and grows proportionally to `base` when the window has slack.
/// When the window is too narrow for content widths, the slack over content
/// is given up first, and only then do content columns compress toward their
/// `min` floors. Returns widths and the px-per-base scale for drag mapping.
fn fit_widths(base: &[f32], content: &[f32], min: &[f32], avail: f32, spacing: f32) -> (Vec<f32>, f32) {
    let n = base.len().max(1);
    let inner = (avail - spacing * (n - 1) as f32).max(40.0 * n as f32);
    let want: Vec<f32> = base.iter().zip(content).map(|(b, c)| b.max(*c)).collect();
    let sum_want: f32 = want.iter().sum::<f32>().max(1.0);
    let sum_base: f32 = base.iter().sum::<f32>().max(1.0);
    let scale = inner / sum_base;
    let deficit = sum_want - inner;
    let w = if deficit <= 0.0 {
        want.iter().map(|x| x * inner / sum_want).collect()
    } else {
        // slack first: shrink above the content floor, proportionally
        let slack: Vec<f32> = want.iter().zip(content).map(|(w, c)| (w - c).max(0.0)).collect();
        let sum_slack: f32 = slack.iter().sum();
        if sum_slack > deficit {
            want.iter().zip(&slack).map(|(w, s)| w - deficit * s / sum_slack).collect()
        } else {
            // then compress content toward the hard minimum, proportionally
            let room: Vec<f32> =
                want.iter().zip(min).map(|(w, m)| (w.max(*m) - m).max(0.0)).collect();
            let sum_room: f32 = room.iter().sum::<f32>().max(1.0);
            let extra = deficit - sum_slack;
            want.iter().zip(&room).zip(min)
                .map(|((w, r), m)| (w - extra * r / sum_room).max(*m))
                .collect()
        }
    };
    (w, scale)
}

/// Move a column boundary by a drag (px): the left column grows, the right
/// shrinks, clamped so neither goes under its minimum display width.
fn apply_boundary_drag(base: &mut [f32], min: &[f32], scale: f32, i: usize, dx: f32) {
    if scale <= 0.0 || i + 1 >= base.len() {
        return;
    }
    let lo = |i: usize| min.get(i).copied().unwrap_or(40.0) / scale;
    let mut a = base[i] + dx / scale;
    let mut b = base[i + 1] - dx / scale;
    if a < lo(i) {
        b += a - lo(i);
        a = lo(i);
    }
    if b < lo(i + 1) {
        a += b - lo(i + 1);
        b = lo(i + 1);
        a = a.max(lo(i));
    }
    base[i] = a;
    base[i + 1] = b;
}

/// Header label with a *drawn* sort caret (works with every font).
fn header_cell(ui: &mut egui::Ui, active_col: usize, asc: bool, col: usize, label: &str) -> egui::Response {
    let active = active_col == col;
    let mut text = egui::RichText::new(label).weak();
    if active {
        text = text.strong();
    }
    let w = ui.available_width();
    let resp = ui.add(
        egui::Button::new(text)
            .min_size(egui::vec2(w.max(40.0), 0.0))
            .frame(false)
            .small(),
    );
    if active {
        let r = resp.rect;
        let cx = r.right() - 8.0;
        let cy = r.center().y;
        let pts = if asc {
            [
                egui::pos2(cx - 4.0, cy + 2.5),
                egui::pos2(cx + 4.0, cy + 2.5),
                egui::pos2(cx, cy - 3.0),
            ]
        } else {
            [
                egui::pos2(cx - 4.0, cy - 2.5),
                egui::pos2(cx + 4.0, cy - 2.5),
                egui::pos2(cx, cy + 3.0),
            ]
        };
        let color = ui.visuals().text_color();
        ui.painter()
            .add(egui::Shape::convex_polygon(pts.to_vec(), color, egui::Stroke::NONE));
    }
    resp
}

pub fn fmt_size(b: u64) -> String {
    if b >= 1 << 30 {
        format!("{:.1} GB", b as f64 / (1u64 << 30) as f64)
    } else if b >= 1 << 20 {
        format!("{:.1} MB", b as f64 / (1u64 << 20) as f64)
    } else if b >= 1 << 10 {
        format!("{} KB", b / 1024)
    } else if b > 0 {
        format!("{b} B")
    } else {
        String::new()
    }
}

fn cap_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let cut: String = s.chars().take(n - 1).collect();
        format!("{cut}\u{2026}")
    }
}

pub fn discover_tab(app: &mut AppGui, ui: &mut egui::Ui) {
    let mut cmds: Vec<Cmd> = Vec::new();
    let installed: Vec<String> = app.addons.iter().map(|a| a.key.clone()).collect();
    let busy = app.busy.clone();

    {
        let d = &mut app.discover;

        ui.horizontal(|ui| {
            ui.style_mut().spacing.interact_size.y = 30.0;
            egui::ComboBox::from_id_salt("disc-provider")
                .width(160.0)
                .selected_text(d.provider.label())
                .show_ui(ui, |ui| {
                    ui.label(egui::RichText::new("searchable sources").weak().small());
                    for p in [ProviderKind::Curse, ProviderKind::WowUpHub, ProviderKind::Tukui] {
                        ui.selectable_value(&mut d.provider, p, p.label());
                    }
                });
            let bw = 84.0;
            let w = (ui.available_width() - bw - ui.spacing().item_spacing.x).max(160.0);
            let resp = ui.add_sized(
                [w, 30.0],
                egui::TextEdit::singleline(&mut d.query).hint_text("search addons…"),
            );
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let clicked = ui
                .add_sized([bw, 30.0], egui::Button::new(egui::RichText::new("Search").strong()))
                .clicked();
            if (clicked || enter) && d.query.trim().len() >= 2 {
                d.searching = true;
                d.page = 0;
                cmds.push(Cmd::Search { provider: d.provider, query: d.query.trim().to_string(), page: 0 });
            }
        });

        ui.horizontal(|ui| {
            ui.style_mut().spacing.interact_size.y = 30.0;
            ui.weak("Install from URL:");
            let bw = 74.0;
            let w = (ui.available_width() - bw - ui.spacing().item_spacing.x * 2.0).max(160.0);
            let resp = ui.add_sized(
                [w, 30.0],
                egui::TextEdit::singleline(&mut d.url).hint_text(
                    "github.com/owner/repo  ·  curseforge.com/wow/addons/<slug>  ·  wowinterface.com/downloads/info1234  ·  Tukui slug  ·  direct .zip URL",
                ),
            );
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (ui.add_sized([bw, 30.0], egui::Button::new("Install")).clicked() || enter)
                && !d.url.trim().is_empty()
            {
                cmds.push(Cmd::InstallUrl(d.url.trim().to_string()));
            }
        });

        ui.horizontal(|ui| {
            if d.searching {
                ui.spinner();
                ui.weak("loading all results…");
            } else if !d.items.is_empty() {
                let shown = d.items.len();
                ui.weak(format!(
                    "{} result{}. Click a column header to sort all of them.",
                    shown,
                    if shown == 1 { "" } else { "s" }
                ));
            }
        });

        let mut items = d.items.clone();
        {
            let col = d.sort_col;
            let asc = d.sort_dir;
            items.sort_by(|a, b| {
                let ord = match col {
                    1 => a.author.cmp(&b.author),
                    2 => a.size.cmp(&b.size).then(a.name.cmp(&b.name)),
                    3 => a.downloads.unwrap_or(0).cmp(&b.downloads.unwrap_or(0)),
                    4 => a.updated_at.cmp(&b.updated_at),
                    _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                };
                if asc { ord } else { ord.reverse() }
            });
        }
        let dcol = d.sort_col;
        let dasc = d.sort_dir;
        let mut dsort: Option<usize> = None;
        ui.visuals_mut().widgets.noninteractive.bg_stroke = egui::Stroke::NONE;
        ui.visuals_mut().widgets.hovered.bg_stroke = egui::Stroke::NONE;
        if items.is_empty() && !d.searching {
            ui.vertical_centered(|ui| {
                ui.add_space(28.0);
                ui.add(art(ART_EMPTY_DISCOVER, "bytes://empty-discover.png", 96.0, 96.0));
                ui.add_space(6.0);
                ui.label(if d.query.trim().is_empty() {
                    "Pick a source, type a name and press Search. Results are filtered to addons that support WoW: Forever."
                } else {
                    "No results. Try different keywords, a spellcheck pass, or another source."
                });
                ui.add_space(28.0);
            });
        }
        if !items.is_empty() || d.searching {
        const DISCOVER_INSTALL_W: f32 = 110.0;
        const DISCOVER_MIN: [f32; 5] = [150.0, 80.0, 52.0, 62.0, 60.0];
        if app.discover_cols.is_empty() {
            app.discover_cols = vec![360.0, 120.0, 70.0, 78.0, 80.0];
        }
        let pad = CELL_PAD;
        let mut content: [f32; 5] = [150.0, 80.0, 52.0, 62.0, 60.0];
        for (slot, label) in content.iter_mut().zip(["Addon", "Author", "Size", "Downloads", "Updated"]) {
            *slot = (*slot).max(est_text_w(ui, label, false, false) + pad);
        }
        for it in items.iter() {
            content[0] = content[0].max(est_text_w(ui, &it.name, false, true) + pad);
            content[1] = content[1].max(est_text_w(ui, it.author.as_deref().unwrap_or(""), true, false) + pad);
            content[2] = content[2].max(est_text_w(ui, &fmt_size(it.size.unwrap_or(0)), true, false) + pad);
            content[3] = content[3].max(est_text_w(ui, &it.downloads.map(fmt_count).unwrap_or_default(), true, false) + pad);
            content[4] = content[4].max(est_text_w(ui, &it.updated_at.map(fmt_date).unwrap_or_default(), true, false) + pad);
        }
        let spacing = ui.spacing().item_spacing.x;
        let table_avail = ui.available_width() - DISCOVER_INSTALL_W - spacing;
        let (dw, dscale) = fit_widths(&app.discover_cols, &content, &DISCOVER_MIN, table_avail, spacing);
        let mut ddrag = [0.0f32; 5];
        TableBuilder::new(ui)
            .striped(true)
            .id_salt("discover-table")
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(dw[0]))
            .column(Column::exact(dw[1]))
            .column(Column::exact(dw[2]))
            .column(Column::exact(dw[3]))
            .column(Column::exact(dw[4]))
            .column(Column::exact(DISCOVER_INSTALL_W))
            .header(22.0, |mut h| {
                h.col(|ui| {
                    if header_cell(ui, dcol, dasc, 0, "Addon").clicked() {
                        dsort = Some(0);
                    }
                    if let Some(dx) = resize_strip(ui) {
                        ddrag[0] = dx;
                    }
                });
                h.col(|ui| {
                    if header_cell(ui, dcol, dasc, 1, "Author").clicked() {
                        dsort = Some(1);
                    }
                    if let Some(dx) = resize_strip(ui) {
                        ddrag[1] = dx;
                    }
                });
                h.col(|ui| {
                    if header_cell(ui, dcol, dasc, 2, "Size").clicked() {
                        dsort = Some(2);
                    }
                    if let Some(dx) = resize_strip(ui) {
                        ddrag[2] = dx;
                    }
                });
                h.col(|ui| {
                    if header_cell(ui, dcol, dasc, 3, "Downloads").clicked() {
                        dsort = Some(3);
                    }
                    if let Some(dx) = resize_strip(ui) {
                        ddrag[3] = dx;
                    }
                });
                h.col(|ui| {
                    if header_cell(ui, dcol, dasc, 4, "Updated").clicked() {
                        dsort = Some(4);
                    }
                    if let Some(dx) = resize_strip(ui) {
                        ddrag[4] = dx;
                    }
                });
                h.col(|ui| { ui.weak("Install"); });
            })
            .body(|body| {
                body.rows(34.0, items.len(), |mut row| {
                    let it = &items[row.index()];
                    let key = format!("{}:{}", it.provider.as_str(), it.external_id);
                    let is_installed = installed.contains(&key);
                    row.col(|ui| {
                        ui.add(egui::Label::new(egui::RichText::new(&it.name).strong()).truncate().selectable(false));
                        if let Some(desc) = it.description.as_deref().filter(|s| !s.is_empty()) {
                            ui.add(egui::Label::new(egui::RichText::new(desc).small().weak()).truncate().selectable(false));
                        }
                    });
                    row.col(|ui| {
                        ui.add(egui::Label::new(egui::RichText::new(it.author.clone().unwrap_or_default()).small()).truncate().selectable(false));
                    });
                    row.col(|ui| {
                        ui.weak(fmt_size(it.size.unwrap_or(0)));
                    });
                    row.col(|ui| {
                        if let Some(n) = it.downloads {
                            ui.weak(fmt_count(n));
                        }
                    });
                    row.col(|ui| {
                        if let Some(t) = it.updated_at {
                            ui.weak(fmt_date(t));
                        }
                    });
                    row.col(|ui| {
                        if let Some((phase, frac)) = busy.get(&key) {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                if *frac > 0.0 {
                                    ui.weak(format!("{phase} {:.0}%", frac * 100.0));
                                } else {
                                    ui.weak(phase.clone());
                                }
                            });
                        } else {
                            ui.horizontal(|ui| {
                                if is_installed {
                                    ui.weak("installed");
                                } else if ui.button("Install").clicked() {
                                    cmds.push(Cmd::InstallNew {
                                        provider: it.provider,
                                        external_id: it.external_id.clone(),
                                        name: it.name.clone(),
                                        channel: None,
                                    });
                                }
                                if it.site_url.is_some() && ui.small_button("Web").clicked() {
                                    if let Some(u) = &it.site_url {
                                        let _ = open::that(u);
                                    }
                                }
                            });
                        }
                    });
                });
            });
        for (i, &dx) in ddrag.iter().enumerate() {
            if dx == 0.0 || dscale <= 0.0 {
                continue;
            }
            if i == DISCOVER_MIN.len() - 1 {
                // boundary against the fixed Install column: only Updated changes
                let floor = DISCOVER_MIN[i] / dscale;
                app.discover_cols[i] = (app.discover_cols[i] + dx / dscale).max(floor);
            } else {
                apply_boundary_drag(&mut app.discover_cols, &DISCOVER_MIN, dscale, i, dx);
            }
        }
        }
        if let Some(col) = dsort {
            if d.sort_col == col {
                d.sort_dir = !d.sort_dir;
            } else {
                d.sort_col = col;
                d.sort_dir = col == 0 || col == 1;
            }
        }
    }

    for c in cmds {
        app.send(c);
    }
}

pub fn settings_tab(app: &mut AppGui, ui: &mut egui::Ui) {
    if app.settings_form.is_none() {
        app.settings_form = Some(Box::new((*app.cfg).clone()));
    }
    // move the form out to avoid double-borrowing `app`
    let mut form = *app.settings_form.take().unwrap();
    let app_version = app.app_version.to_string();
    let data_dir = app.data_dir.clone();
    let update_repo = app.update_repo().unwrap_or_else(|| "not configured - set [app] update_repo".into());
    let self_update_info = app.self_update_info.clone();
    let app_update_available = app.self_update_available.clone();
    let mut cmds: Vec<Cmd> = Vec::new();
    let mut save_msg: Option<String> = None;
    let mut save_form: Option<AppConfig> = None;
    let cfg_snapshot = app.cfg.clone();

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
        ui.group(|ui| {
            ui.heading("WoW client");
            egui::Grid::new("set-paths").num_columns(3).spacing([8.0, 6.0]).show(ui, |ui| {
                ui.label("AddOns folder");
                let mut dir_txt = form.app.addons_dir.clone().unwrap_or_default();
                let r = ui.add(
                    egui::TextEdit::singleline(&mut dir_txt)
                        .desired_width(430.0)
                        .hint_text(addons_forever_core::config::addons_dir_example()),
                );
                if r.changed() {
                    form.app.addons_dir = Some(dir_txt);
                }
                if ui.button("Browse...").clicked() {
                    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                        form.app.addons_dir = Some(dir.display().to_string());
                    }
                }
                ui.end_row();

                // live preview of the effective source for this path
                ui.label("");
                match (
                    form.app.addons_dir.as_deref().filter(|d| !d.trim().is_empty()),
                    form.app.auto_detect,
                ) {
                    (Some(d), _) => {
                        // the field is the draft; say so until it is saved
                        let applied = cfg_snapshot
                            .app
                            .addons_dir
                            .as_deref()
                            .map(|p| p.trim())
                            .unwrap_or("");
                        let trimmed = d.trim();
                        if !trimmed.is_empty() && trimmed != applied {
                            ui.weak(
                                egui::RichText::new("explicit path - NOT SAVED: press Save settings below")
                                    .color(tone(ui, Tone::Warn)),
                            )
                        } else {
                            ui.weak("explicit path (used as-is)")
                        }
                    }
                    (None, true) => {
                        match addons_forever_core::config::autodetect_addons_dir(form.app.flavor) {
                            Some(p) => {
                                let txt = format!("auto-detected: {}", p.display());
                                ui.weak(txt)
                            }
                            None => ui.weak("nothing auto-detected on this system"),
                        }
                    }
                    (None, false) => ui.weak("auto-detection off, set a path above"),
                };
                ui.label("");
                ui.end_row();

                ui.label("Edition (flavor)");
                egui::ComboBox::from_id_salt("set-flavor")
                    .selected_text(form.app.flavor.label())
                    .show_ui(ui, |ui| {
                        for f in Flavor::ALL {
                            ui.selectable_value(&mut form.app.flavor, *f, f.label());
                        }
                    });
                ui.label(egui::RichText::new("Forever is the primary target; the engine is flavor-aware.").small().weak());
                ui.end_row();

                ui.label("Client version");
                let mut vtxt = form.app.game_version.clone().unwrap_or_default();
                let r = ui.add(
                    egui::TextEdit::singleline(&mut vtxt).desired_width(120.0).hint_text(
                        addons_forever_core::flavor::default_client_version(form.app.flavor),
                    ),
                );
                if r.changed() {
                    form.app.game_version = Some(vtxt);
                }
                ui.label(egui::RichText::new("Used for file compatibility (blank = current default).").small().weak());
                ui.end_row();
            });
        });

        ui.group(|ui| {
            ui.heading("Updates");
            ui.checkbox(&mut form.updates.auto_update, "Automatic updates (background timer)")
                .on_hover_text("Check and install updates automatically for addons with Auto enabled");
            ui.checkbox(&mut form.updates.check_on_startup, "Check for updates on startup");
            ui.checkbox(&mut form.ui.scan_on_start, "Scan AddOns folder on startup");
            ui.horizontal(|ui| {
                ui.label("Check interval");
                ui.add(egui::DragValue::new(&mut form.updates.interval_minutes).range(5..=10080).suffix(" min"));
                ui.label("Default channel");
                egui::ComboBox::from_id_salt("set-channel")
                    .selected_text(form.updates.channel.as_str())
                    .show_ui(ui, |ui| {
                        for c in [Channel::Stable, Channel::Beta, Channel::Alpha] {
                            ui.selectable_value(&mut form.updates.channel, c, c.as_str());
                        }
                    });
            });
            ui.checkbox(&mut form.updates.keep_backups, "Keep backups of replaced addon folders");
        });

        ui.group(|ui| {
            ui.heading("Providers & network");
            ui.horizontal(|ui| {
                ui.label("CurseForge API key");
                ui.add(egui::TextEdit::singleline(&mut form.network.curse_api_key).password(true).desired_width(360.0));
            });
            ui.label(
                egui::RichText::new(
                    "Pre-filled with the community key (same one open-source managers ship). You may apply for your own; PRIVACY.md explains exactly what is sent where.",
                )
                .small()
                .weak(),
            );
            ui.horizontal(|ui| {
                ui.label("GitHub token (optional)");
                ui.add(egui::TextEdit::singleline(&mut form.network.github_token).password(true).desired_width(360.0));
            });
            // No Wago provider exists yet: the old key field implied one did
            // (review M-01). The config key stays for forward compatibility.
            ui.checkbox(&mut form.network.offline, "Offline mode (never touch the network)").on_hover_text("Browse cached state only; no requests at all");
        });

        ui.group(|ui| {
            ui.heading("Interface");
            ui.horizontal(|ui| {
                ui.label("Theme");
                egui::ComboBox::from_id_salt("set-theme")
                    .selected_text(form.ui.theme.label())
                    .show_ui(ui, |ui| {
                        for t in Theme::ALL {
                            ui.selectable_value(&mut form.ui.theme, t, t.label());
                        }
                    });
                ui.label(egui::RichText::new("applies after Save").small().weak());
            });
            ui.checkbox(&mut form.ui.minimize_to_tray, "Close button minimizes to the system tray (Windows)")
                .on_hover_text("Available in the Windows build");
            ui.horizontal(|ui| {
                ui.label("UI scale");
                let mut sc = form.ui.scale;
                if ui.add(egui::Slider::new(&mut sc, 0.5..=2.5).text("x")).changed() {
                    form.ui.scale = (sc * 4.0).round() / 4.0;
                }
                ui.label(egui::RichText::new("applies after Save (on next frame)").small().weak());
            });
        });

        ui.group(|ui| {
            ui.heading("App");
            ui.label(format!("Version {app_version}  -  app updates from {update_repo}"));
            let mut cmds2: Vec<Cmd> = Vec::new();
            ui.horizontal(|ui| {
                if ui.button("Check for app updates").clicked() {
                    cmds2.push(Cmd::CheckAppUpdate);
                }
                if let Some(v) = &app_update_available {
                    ui.label(
                        egui::RichText::new(format!("Update available: {v}"))
                            .strong()
                            .color(ui.visuals().hyperlink_color),
                    );
                    if ui.button("Install update").clicked() {
                        cmds2.push(Cmd::SelfUpdate);
                    }
                }
            });
            cmds.extend(cmds2);
            ui.checkbox(
                &mut form.app.self_update,
                "Install app updates automatically (checked on startup)",
            )
            .on_hover_text("Off by default: updates are announced in the window title and here.");
            if let Some(info) = &self_update_info {
                ui.label(egui::RichText::new(info).small());
            }
            ui.horizontal(|ui| {
                if ui.button("Open data folder").clicked() {
                    let _ = open::that(&data_dir);
                }
                if ui.button("Open config file").clicked() {
                    let _ = open::that(AppConfig::config_path(&data_dir));
                }
            });
            ui.label(egui::RichText::new(
                "Everything the app knows lives in this folder: addons-forever.ini and the managed addons \
                 themselves. No logs are written to disk, and nothing is uploaded anywhere \
                 (see PRIVACY.md). Installs use a temporary .fa-tmp folder next to AddOns.",
            )
            .small()
            .weak());
        });

        ui.group(|ui| {
            ui.heading("Portable setup");
            ui.horizontal(|ui| {
                if ui.button("Export addon list...").clicked() {
                    if let Some(p) = rfd::FileDialog::new()
                        .set_file_name("addons-forever-export.json")
                        .save_file()
                    {
                        cmds.push(Cmd::ExportAddons(p));
                    }
                }
                if ui.button("Import addon list...").clicked() {
                    if let Some(p) = rfd::FileDialog::new()
                        .add_filter("Addons Forever export", &["json"])
                        .pick_file()
                    {
                        cmds.push(Cmd::ImportAddons(p));
                    }
                }
            });
            ui.label(
                egui::RichText::new(
                    "Export writes a JSON manifest of your addons (source, id, name, auto-update flags). \
                     Import installs each addon from its source, so a fresh setup ends up with the same set.",
                )
                .small()
                .weak(),
            );
        });


        ui.horizontal(|ui| {
            // make unsaved state unmistakable: the field previews above are
            // drafts, not applied settings (the whole config applies on Save)
            let dirty = form != *cfg_snapshot;
            let save_label = if dirty { "Save settings - unsaved changes" } else { "Save settings" };
            let save_text = egui::RichText::new(save_label)
                .strong()
                .color(if dirty { tone(ui, Tone::Warn) } else { ui.visuals().strong_text_color() });
            if ui.add(egui::Button::new(save_text)).clicked() {
                cmds.push(Cmd::SaveConfig(Box::new(form.clone())));
                save_msg = Some("Saved to addons-forever.ini.".to_string());
                save_form = Some(form.clone());
            }
            if ui.button("Revert").clicked() {
                form = (*cfg_snapshot).clone();
                save_msg = Some("Reverted to last saved settings.".to_string());
            }
        });
    });

    if let Some(f) = save_form {
        app.cfg = std::sync::Arc::new(f); // reflect immediately (scale etc.)
    }
    if let Some(m) = save_msg {
        app.settings_msg = Some(m);
    }
    app.settings_form = Some(Box::new(form));
    if let Some(m) = app.settings_msg.clone() {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(m).weak());
            if ui.small_button("✕").clicked() {
                app.settings_msg = None;
            }
        });
    }
    for c in cmds {
        app.send(c);
    }
}

pub fn log_tab(app: &mut AppGui, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        if ui.button("Clear").clicked() {
            app.logs.clear();
        }
        if ui.button("Copy log").clicked() {
            let text = app.logs.iter().cloned().collect::<Vec<_>>().join("\n");
            let n = text.lines().count();
            ui.ctx().copy_text(text);
            app.push_log(format!("log copied to clipboard ({n} lines)"));
        }
        if ui.button("Save log...").clicked() {
            if let Some(p) = rfd::FileDialog::new()
                .set_file_name("addons-forever-log.txt")
                .save_file()
            {
                let text = app.logs.iter().cloned().collect::<Vec<_>>().join("\n");
                match std::fs::write(&p, text) {
                    Ok(()) => app.push_log(format!("log saved to {}", p.display())),
                    Err(e) => app.push_log(format!("log save failed: {e}")),
                }
            }
        }
        ui.weak(format!("{} lines", app.logs.len()));
    });
    let copy_pressed =
        ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::C));
    egui::ScrollArea::vertical()
        .stick_to_bottom(true)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if app.logs.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space(24.0);
                    ui.add(art(ART_EMPTY_LOG, "bytes://empty-log.png", 96.0, 96.0));
                    ui.add_space(6.0);
                    ui.weak("Nothing logged yet. Scans, installs and updates are recorded here.");
                });
            }
            for line in app.logs.iter() {
                let text = egui::RichText::new(line.clone()).monospace().small();
                let resp = if line.contains("FAILED") || line.to_lowercase().contains("failed") {
                    ui.colored_label(tone(ui, Tone::Bad), text)
                } else {
                    ui.label(text)
                };
                if copy_pressed && resp.hovered() {
                    ui.ctx().copy_text(line.clone());
                    app.toast("copied to clipboard");
                }
            }
        });
}

fn fmt_count(n: i64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{:.0}k", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

fn fmt_date(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod layout_tests {
    use super::{apply_boundary_drag, fit_widths};

    const SP: f32 = 8.0;

    fn sum(w: &[f32]) -> f32 {
        w.iter().sum::<f32>() + SP * (w.len() - 1) as f32
    }

    #[test]
    fn widths_fill_the_available_space() {
        let base = vec![260.0, 78.0, 84.0, 64.0, 70.0, 280.0];
        let content = vec![120.0, 64.0, 64.0, 48.0, 56.0, 280.0];
        let min = vec![120.0, 64.0, 64.0, 48.0, 56.0, 280.0];
        for avail in [820.0, 1080.0, 1600.0, 2400.0] {
            let (w, _) = fit_widths(&base, &content, &min, avail, SP);
            assert!((sum(&w) - avail).abs() < 0.6, "avail {avail} -> sum {}", sum(&w));
        }
    }

    #[test]
    fn slack_is_given_up_before_content_is_compressed() {
        // wide user widths, narrow window: content floors must survive intact
        let base = vec![600.0, 400.0, 400.0];
        let content = vec![180.0, 90.0, 90.0];
        let min = vec![100.0, 60.0, 60.0];
        let avail = 420.0; // less than base, more than content + spacing
        let (w, _) = fit_widths(&base, &content, &min, avail, SP);
        assert!(w[0] >= 180.0, "content column truncated by shrinking: {w:?}");
        assert!(w[1] >= 90.0 && w[2] >= 90.0, "content column truncated: {w:?}");
        assert!((sum(&w) - avail).abs() < 0.6);
    }

    #[test]
    fn content_columns_compress_toward_mins_when_tiny() {
        let base = vec![260.0, 90.0, 90.0];
        let content = vec![260.0, 90.0, 90.0];
        let min = vec![120.0, 60.0, 60.0];
        let avail = 300.0; // below content sum (440 + spacing)
        let (w, _) = fit_widths(&base, &content, &min, avail, SP);
        assert!(w.iter().zip(&min).all(|(w, m)| *w >= *m), "below min: {w:?}");
        assert!((sum(&w) - avail).abs() < 0.6);
        // compression is proportional: the widest column loses the most px
        assert!(w[0] < 260.0 && w[1] <= 90.0);
    }

    #[test]
    fn growth_is_proportional() {
        let base = vec![260.0, 90.0];
        let content = vec![100.0, 90.0];
        let min = vec![100.0, 60.0];
        let (mid, _) = fit_widths(&base, &content, &min, 600.0, SP);
        let (big, _) = fit_widths(&base, &content, &min, 1200.0, SP);
        // with content floors met, both columns scale by ~the window factor
        let r_mid = mid[0] / (mid[0] + mid[1]);
        let r_big = big[0] / (big[0] + big[1]);
        assert!((r_mid - r_big).abs() < 0.05, "not proportional: {r_mid} vs {r_big}");
    }

    #[test]
    fn boundary_drag_moves_the_separator_and_keeps_mins() {
        let mut base = vec![100.0, 100.0, 100.0];
        let min = vec![40.0, 40.0, 40.0];
        // scale 2: display px = base * 2
        apply_boundary_drag(&mut base, &min, 2.0, 0, 20.0);
        assert_eq!(base[0], 110.0);
        assert_eq!(base[1], 90.0);
        // clamp: column 1 cannot go under its 40px minimum (20 base units)
        apply_boundary_drag(&mut base, &min, 2.0, 0, 200.0);
        assert_eq!(base[1], 20.0);
        assert_eq!(base[0], 180.0); // width conserved: 110 + 90 = 180 + 20
        // dragging past the neighbor's min stops
        apply_boundary_drag(&mut base, &min, 2.0, 0, 500.0);
        assert_eq!(base[1], 20.0);
        assert_eq!(base[0], 180.0);
    }
}
