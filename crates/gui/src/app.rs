//! egui application: state, event pump, layout, dialogs.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;

use addons_forever_core::config::{AppConfig, Theme};
use addons_forever_core::flavor::Flavor;
use addons_forever_core::store::AddonState;
use addons_forever_core::types::{Channel, ProviderKind, SearchItem};

use crate::tabs;
use crate::worker::{Cmd, Ev, WorkerHandle};

#[derive(PartialEq, Clone, Copy)]
pub enum Tab {
    Addons,
    Discover,
    Settings,
    Log,
}

#[derive(Clone)]
pub struct Job {
    pub label: String,
    pub phase: String,
    pub addon: String, // shown in the status bar
    pub frac: Option<f32>,
}

pub struct DiscoverState {
    pub provider: ProviderKind,
    pub query: String,
    pub sort_col: usize,
    pub sort_dir: bool,
    pub page: u32,
    pub items: Vec<SearchItem>,
    pub total: u32,
    pub searching: bool,
    pub url: String,
}

impl Default for DiscoverState {
    fn default() -> Self {
        DiscoverState {
            provider: ProviderKind::Curse,
            query: String::new(),
            sort_col: 3,
            sort_dir: false, // default: most downloads first
            page: 0,
            items: Vec::new(),
            total: 0,
            searching: false,
            url: String::new(),
        }
    }
}

pub struct ChangelogView {
    pub title: String,
    pub text: String,
    pub open: bool,
}

/// One row of the dependency prompt.
#[derive(Clone)]
pub struct DepEntry {
    pub name: String,
    pub required: bool,
    /// Installable provider source, when a provider has this addon.
    pub source: Option<(ProviderKind, String)>,
    pub checked: bool,
}

/// Prompt shown after an install whose addon declares missing dependencies.
#[derive(Clone)]
pub struct DepDialog {
    pub addon: String,
    pub entries: Vec<DepEntry>,
}

pub struct AppGui {
    pub cfg: Arc<AppConfig>,
    pub data_dir: PathBuf,
    pub app_version: &'static str,

    tx: mpsc::UnboundedSender<Cmd>,
    rx: mpsc::UnboundedReceiver<Ev>,

    pub addons: Vec<AddonState>,
    pub updates: Vec<String>, // keys with pending updates
    pub tab: Tab,
    pub filter: String,
    pub sort_col: usize,
    pub sort_dir: bool, // true = ascending
    pub job: Option<Job>,
    pub logs: VecDeque<String>,
    pub toast: Option<(String, std::time::Instant)>,
    pub discover: DiscoverState,
    pub changelog: Option<ChangelogView>,
    pub dep_dialog: Option<DepDialog>,
    pub confirm_uninstall: Option<(String, String)>, // key, name
    pub source_editor: Option<SourceEditor>,
    pub settings_form: Option<Box<AppConfig>>,
    pub settings_msg: Option<String>,
    pub self_update_info: Option<String>,
    /// Latest app version known to be available (drives title + Settings badge).
    pub self_update_available: Option<String>,
    pub last_check: Option<i64>,
    /// AF-18: true when the last check reached every source online (drives the
    /// status bar's "up to date"; failures and offline runs never set it)
    pub check_clean: bool,
    pub quit: bool,
    pub addon_menu: Option<String>,
    /// key -> (phase, fraction 0..1) for addons currently downloading/installing.
    pub busy: std::collections::BTreeMap<String, (String, f32)>,
    /// value of [ui] scale currently applied via egui zoom_factor
    pub applied_scale: f32,
    /// value of [ui] theme currently applied via egui's theme preference
    pub applied_theme: Option<Theme>,
    /// top bar busy states (set on click, cleared by ScanDone/CheckDone/Failed)
    pub scanning: bool,
    pub checking: bool,
    pub window_size: Option<egui::Vec2>,
    size_save: u32,
    /// Proportional column widths (base units) for the addon table.
    pub addons_cols: Vec<f32>,
    /// Proportional column widths (base units) for the discover table.
    pub discover_cols: Vec<f32>,
    /// last title string applied to the OS window (change-detect for badge text)
    window_title: String,

    pub tray: Option<crate::tray::TrayCtl>,
}

#[derive(Clone)]
pub struct SourceEditor {
    pub key: String,
    pub provider: ProviderKind,
    pub external_id: String,
}

impl AppGui {
    pub fn new(
        _cc: &eframe::CreationContext<'_>,
        cfg: Arc<AppConfig>,
        data_dir: PathBuf,
        worker: WorkerHandle,
        app_version: &'static str,
    ) -> Self {
        let mut app = AppGui {
            cfg,
            data_dir,
            app_version,
            tx: worker.tx,
            rx: worker.rx,
            addons: Vec::new(),
            updates: Vec::new(),
            tab: Tab::Addons,
            filter: String::new(),
            sort_col: 0,
            sort_dir: true,
            job: None,
            logs: VecDeque::new(),
            toast: None,
            discover: DiscoverState::default(),
            changelog: None,
            dep_dialog: None,
            confirm_uninstall: None,
            source_editor: None,
            settings_form: None,
            settings_msg: None,
            self_update_info: None,
            self_update_available: None,
            window_title: "Addons Forever".into(),
            last_check: None,
            check_clean: false,
            quit: false,
            addon_menu: None,
            busy: Default::default(),
            applied_scale: -1.0,
            applied_theme: None,
            scanning: false,
            checking: false,
            window_size: None,
            size_save: 0,
            addons_cols: Vec::new(),
            discover_cols: Vec::new(),
            tray: None,
        };
        app.push_log(format!("addons-forever {app_version} starting"));
        app.tray = match crate::tray::TrayCtl::setup(app_version) {
            Ok(t) => Some(t),
            Err(e) => {
                tracing::debug!("tray unavailable: {e}");
                None
            }
        };
        app
    }

    pub fn send(&self, cmd: Cmd) {
        if self.tx.send(cmd).is_err() {
            self.toast("worker not running".to_string());
        }
    }

    pub fn toast(&self, msg: impl Into<String>) {
        tracing::warn!("{}", msg.into());
    }

    pub fn push_log(&mut self, line: impl Into<String>) {
        let line = line.into();
        tracing::info!("{line}");
        if self.logs.len() >= 800 {
            self.logs.pop_front();
        }
        self.logs.push_back(line);
    }

    fn drain_events(&mut self, ctx: &egui::Context) {
        let mut n = 0;
        while let Ok(ev) = self.rx.try_recv() {
            n += 1;
            self.handle_event(ev);
            if n >= 64 {
                ctx.request_repaint();
                break;
            }
        }
        if n > 0 {
            ctx.request_repaint();
        }
    }

    fn handle_event(&mut self, ev: Ev) {
        match ev {
            Ev::State { addons } => {
                let mut addons = addons;
                addons.sort_by(|a, b| {
                    let pa = !self.updates.contains(&a.key);
                    let pb = !self.updates.contains(&b.key);
                    pb.cmp(&pa).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                });
                self.addons = addons;
            }
            Ev::ScanProgress { folder, done, total } => {
                self.job = Some(Job {
                    label: "Scanning AddOns folder".into(),
                    phase: folder,
                    addon: String::new(),
                    frac: Some(done as f32 / total.max(1) as f32),
                });
            }
            Ev::ScanDone { matched, unmatched } => {
                self.scanning = false;
                self.checking = false;
                self.job = None;
                self.push_log(format!("Scan: {matched} matched, {unmatched} unmanaged folders"));
            }
            Ev::CheckProgress { name, done, total } => {
                self.job = Some(Job {
                    label: "Checking for updates".into(),
                    phase: name,
                    addon: String::new(),
                    frac: Some(done as f32 / total.max(1) as f32),
                });
            }
            Ev::CheckDone { updates, failed, offline } => {
                self.scanning = false;
                self.checking = false;
                self.job = None;
                self.last_check = Some(chrono::Local::now().timestamp());
                self.updates = updates.iter().map(|u| u.key.clone()).collect();
                // AF-18: the status bar's "up to date" only follows a clean
                // check; a partial/offline one is reported truthfully
                self.check_clean = !offline && failed.is_empty();
                if offline {
                    self.push_log("Check: offline mode, results are cache-only");
                } else if !failed.is_empty() {
                    let labels = failed.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>().join(", ");
                    self.push_log(format!(
                        "Check: {} source(s) failed ({labels}); showing cached data",
                        failed.len()
                    ));
                } else if updates.is_empty() {
                    self.push_log("Check: all addons are up to date");
                } else {
                    self.push_log(format!("Updates available: {}", updates.len()));
                }
            }
            Ev::InstallProgress { key, name, phase, bytes, total } => {
                let frac = total.map(|t| (bytes as f64 / t as f64) as f32).unwrap_or(0.0);
                self.busy.insert(key, (phase.clone(), frac));
                self.job = Some(Job {
                    label: "Installing".into(),
                    phase,
                    addon: name,
                    frac: Some(frac),
                });
            }
            Ev::InstallDone { key, name, version } => {
                self.job = None;
                self.busy.remove(&key);
                self.updates.retain(|k| k != &key);
                self.push_log(format!("{name}: installed {}", version.as_deref().unwrap_or("?")));
            }
            Ev::Failed { key, msg } => {
                self.scanning = false;
                self.checking = false;
                self.job = None;
                if !key.is_empty() {
                    self.busy.remove(&key);
                }
                if key == "search" {
                    // AF-26: a failed search must end the operation
                    self.discover.searching = false;
                }
                self.toast = Some((msg.clone(), std::time::Instant::now()));
                self.push_log(format!("FAILED {key}: {msg}"));
            }
            Ev::SearchDone { provider: _, query, page, items, total } => {
                self.discover.searching = false;
                self.discover.query = query;
                self.discover.page = page;
                self.discover.items = items;
                self.discover.total = total;
            }
            Ev::ChangelogDone { key, title, text } => {
                let _ = key;
                self.changelog = Some(ChangelogView { title, text: strip_html(&text), open: true });
            }
            Ev::DepsFound { addon, deps } => {
                self.dep_dialog = Some(DepDialog {
                    addon,
                    entries: deps
                        .into_iter()
                        .map(|d| DepEntry {
                            name: d.name,
                            required: d.required,
                            checked: d.source.is_some(),
                            source: d.source,
                        })
                        .collect(),
                });
            }
            Ev::Log { line } => {
                tracing::info!(target: "worker", "{line}");
                self.logs.push_back(line);
                if self.logs.len() >= 800 {
                    self.logs.pop_front();
                }
            }
            Ev::SelfUpdateDone { info } => {
                self.self_update_info = Some(info.clone());
                self.self_update_available = None;
                self.push_log(format!("self-update: {info}"));
            }
            Ev::SelfUpdateAvailable { version } => {
                self.self_update_available = Some(version.clone());
                self.push_log(format!("app update available: {version} (see Settings)"));
            }
        }
    }

    pub fn top_bar(&mut self, ui: &mut egui::Ui) {
        let mut cmds: Vec<Cmd> = Vec::new();
        // Uniform control height across the bar (matches the discover toolbar),
        // so buttons, the filter field and the flavor combo share a centerline.
        ui.style_mut().spacing.interact_size.y = 30.0;

        ui.horizontal(|ui| {
            let scanning = self.scanning;
            let scan_lbl = if scanning { "Scanning\u{2026}" } else { "Scan" };
            if ui
                .add_enabled(!scanning, egui::Button::new(scan_lbl).min_size(egui::vec2(0.0, 30.0)))
                .on_hover_text("Re-scan the AddOns folder")
                .clicked()
            {
                self.scanning = true;
                cmds.push(Cmd::Rescan);
            }
            let checking = self.checking;
            let chk_lbl = if checking { "Checking\u{2026}" } else { "Check updates" };
            if ui
                .add_enabled(!checking, egui::Button::new(chk_lbl).min_size(egui::vec2(0.0, 30.0)))
                .on_hover_text("Check for addon updates")
                .clicked()
            {
                self.checking = true;
                cmds.push(Cmd::Check);
            }
            if scanning || checking {
                ui.spinner();
            }
            let n = self.updates.len();
            let label = if n > 0 { format!("Update all ({n})") } else { "Update all".into() };
            if ui
                .add_enabled(n > 0, egui::Button::new(label).min_size(egui::vec2(0.0, 30.0)))
                .on_hover_text("Install all available updates")
                .clicked()
            {
                cmds.push(Cmd::ApplyAll);
            }

            ui.add_space(6.0);
            ui.add_sized(
                [240.0, 30.0],
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text("filter addons...")
                    .margin(egui::Margin::symmetric(8, 8)),
            );
            if !self.filter.is_empty() && ui.button("✕").clicked() {
                self.filter.clear();
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.cfg.network.offline {
                    ui.colored_label(tabs::tone(ui, tabs::Tone::Warn), "[offline]");
                }
                let flavor_txt = self.cfg.app.flavor.label().to_string();
                egui::ComboBox::from_id_salt("flavor")
                    .selected_text(flavor_txt)
                    .show_ui(ui, |ui| {
                        for f in Flavor::ALL {
                            if ui
                                .selectable_label(self.cfg.app.flavor == *f, f.label())
                                .clicked()
                                && self.cfg.app.flavor != *f
                            {
                                let mut c = (*self.cfg).clone();
                                c.app.flavor = *f;
                                self.cfg = Arc::new(c);
                                // the worker persists, restores that flavor's
                                // list from cache and rescans
                                self.updates.clear();
                                self.busy.clear();
                                self.job = None;
                                self.send(Cmd::SetFlavor(*f));
                            }
                        }
                    });
            });
        });

        for c in cmds {
            self.send(c);
        }
    }

    pub fn status_bar(&mut self, ui: &mut egui::Ui) {
        let job = self.job.clone();
        let addons_n = self.addons.len();
        let upd_n = self.updates.len();
        let last = self.last_check;
        ui.horizontal(|ui| {
            ui.label(format!("{} addon{}", addons_n, if addons_n == 1 { "" } else { "s" }));
            if upd_n > 0 {
                ui.add(crate::tabs::art(crate::tabs::ART_BADGE_UPDATE, "bytes://badge-update.png", 16.0, 16.0));
                ui.strong(format!("{upd_n} update(s)"));
            } else if self.check_clean && last.is_some() {
                ui.weak("up to date");
            } else if last.is_some() {
                ui.weak("check incomplete")
                    .on_hover_text("the last update check did not reach every source (or ran offline); see the Log tab");
            }
            ui.separator();
            if let Some(j) = job {
                if !j.addon.is_empty() {
                    ui.strong(j.addon);
                }
                ui.label(j.label);
                ui.label(egui::RichText::new(&j.phase).weak().small());
                match j.frac {
                    Some(f) => {
                        ui.add(egui::ProgressBar::new(f).desired_width(140.0));
                    }
                    None => {
                        ui.spinner();
                    }
                }
            } else if self.cfg.network.offline {
                ui.weak("offline mode, cached data only");
            } else if let Some(ts) = last {
                ui.weak(format!("last check {}", fmt_ts(ts)));
            }
            if let Some((msg, at)) = self.toast.clone() {
                if at.elapsed().as_secs() < 8 {
                    ui.colored_label(tabs::tone(ui, tabs::Tone::Bad), format!("! {msg}"));
                } else {
                    self.toast = None;
                }
            }
        });
    }

    pub fn tab_bar(&mut self, ui: &mut egui::Ui) {
        let badge = if self.self_update_available.is_some() { "Settings (1)" } else { "Settings" };
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.tab, Tab::Addons, "My Addons");
            ui.selectable_value(&mut self.tab, Tab::Discover, "Discover");
            ui.selectable_value(&mut self.tab, Tab::Settings, badge);
            ui.selectable_value(&mut self.tab, Tab::Log, "Log");
        });
    }

    pub fn render_tab(&mut self, ui: &mut egui::Ui) {
        match self.tab {
            Tab::Addons => tabs::addons_tab(self, ui),
            Tab::Discover => tabs::discover_tab(self, ui),
            Tab::Settings => tabs::settings_tab(self, ui),
            Tab::Log => tabs::log_tab(self, ui),
        }
    }

    pub fn dialogs(&mut self, ctx: &egui::Context) {
        // dependency prompt: a fresh install declared addons that are not installed
        if let Some(dlg) = self.dep_dialog.clone() {
            let mut entries = dlg.entries;
            let mut cmds: Vec<Cmd> = Vec::new();
            let mut close = false;
            egui::Window::new("Dependencies")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(format!(
                        "“{}” declares addons it depends on that are not installed:",
                        dlg.addon
                    ));
                    ui.separator();
                    egui::ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
                        for e in entries.iter_mut() {
                            let mut label = e.name.clone();
                            label.push_str(if e.required { "  (required)" } else { "  (optional)" });
                            match &e.source {
                                None => label.push_str("  -  not found in providers"),
                                // name the resolved project: provider ids are
                                // recycled (a search hit is not a guarantee)
                                Some((provider, id)) => {
                                    label.push_str(&format!("  -  {} #{id}", provider.label()))
                                }
                            }
                            ui.add_enabled_ui(e.source.is_some(), |ui| {
                                ui.checkbox(&mut e.checked, label);
                            });
                        }
                    });
                    ui.separator();
                    ui.horizontal(|ui| {
                        let any = entries.iter().any(|e| e.checked && e.source.is_some());
                        if ui
                            .add_enabled(any, egui::Button::new("Install selected"))
                            .clicked()
                        {
                            for e in entries.iter().filter(|e| e.checked) {
                                if let Some((provider, id)) = &e.source {
                                    cmds.push(Cmd::InstallNew {
                                        provider: *provider,
                                        external_id: id.clone(),
                                        name: e.name.clone(),
                                        channel: None,
                                    });
                                }
                            }
                            close = true;
                        }
                        if ui.button("Skip").clicked() {
                            close = true;
                        }
                    });
                });
            for c in cmds {
                self.send(c);
            }
            if close {
                self.dep_dialog = None;
            } else if let Some(cur) = &mut self.dep_dialog {
                cur.entries = entries;
            }
        }

        // per-addon options (channel / ignore / source)
        if let Some(key) = self.addon_menu.clone() {
            let Some(a) = self.addons.iter().find(|x| x.key == key).cloned() else {
                self.addon_menu = None;
                return;
            };
            let mut close = false;
            let mut cmds: Vec<Cmd> = Vec::new();
            egui::Window::new(a.name.clone())
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(format!("Folders: {}", if a.folders.is_empty() { "none".into() } else { a.folders.join(", ") }));
                    ui.label(format!("Source: {} - {}", a.provider.label(), a.external_id));
                    ui.separator();
                    let mut ignored = a.ignored;
                    if ui.checkbox(&mut ignored, "Ignore this addon (skip update checks)").changed() {
                        cmds.push(Cmd::SetFlags { key: a.key.clone(), auto: None, ignored: Some(ignored), channel: None });
                    }
                    let mut auto = a.auto_update;
                    if ui.checkbox(&mut auto, "Automatic updates").changed() {
                        cmds.push(Cmd::SetFlags { key: a.key.clone(), auto: Some(auto), ignored: None, channel: None });
                    }
                    ui.horizontal(|ui| {
                        ui.label("Channel");
                        egui::ComboBox::from_id_salt("menu-channel")
                            .selected_text(a.channel.as_str())
                            .show_ui(ui, |ui| {
                                for c in [Channel::Stable, Channel::Beta, Channel::Alpha] {
                                    if ui.selectable_value(&mut { a.channel }, c, c.as_str()).changed() {
                                        cmds.push(Cmd::SetFlags { key: a.key.clone(), auto: None, ignored: None, channel: Some(c) });
                                    }
                                }
                            });
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Set source…").clicked() {
                            self.source_editor = Some(SourceEditor {
                                key: a.key.clone(),
                                provider: a.provider,
                                external_id: a.external_id.clone(),
                            });
                            close = true;
                        }
                        if ui.button("Install / re-install now").clicked() {
                            cmds.push(Cmd::InstallExisting(a.key.clone()));
                            close = true;
                        }
                    });
                    if ui.button("Close").clicked() {
                        close = true;
                    }
                });
            for c in cmds {
                self.send(c);
            }
            if close {
                self.addon_menu = None;
            }
        }

        if let Some((key, name)) = self.confirm_uninstall.clone() {
            egui::Window::new("Remove addon?")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(format!(
                        "Delete the folders of “{name}” from the AddOns directory?"
                    ));
                    ui.horizontal(|ui| {
                        if ui.button("Remove").clicked() {
                            self.send(Cmd::Uninstall(key.clone()));
                            self.confirm_uninstall = None;
                        }
                        if ui.button("Cancel").clicked() {
                            self.confirm_uninstall = None;
                        }
                    });
                });
        }

        if let Some(ed) = self.source_editor.clone() {
            egui::Window::new("Set addon source")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label("Manually bind this addon to a provider id:\nCurseForge: numeric mod id  |  GitHub: owner/repo  |  Tukui: slug\nMMOUI: file id  |  WowUp Hub: numeric id  |  Zip: download URL");
                    let mut provider = ed.provider;
                    egui::ComboBox::from_id_salt("src-provider")
                        .selected_text(provider.label())
                        .show_ui(ui, |ui| {
                            for p in [
                                ProviderKind::Curse,
                                ProviderKind::GitHub,
                                ProviderKind::Tukui,
                                ProviderKind::WowInterface,
                                ProviderKind::WowUpHub,
                                ProviderKind::Zip,
                            ] {
                                ui.selectable_value(&mut provider, p, p.label());
                            }
                        });
                    let mut id = ed.external_id.clone();
                    ui.add(egui::TextEdit::singleline(&mut id).desired_width(320.0).hint_text("e.g. 2382 / BigWigsMods/BigWigs / elvui"));
                    ui.horizontal(|ui| {
                        if ui.button("Apply").clicked() {
                            if !id.trim().is_empty() {
                                self.send(Cmd::Override {
                                    key: ed.key.clone(),
                                    provider,
                                    external_id: id.trim().to_string(),
                                });
                            }
                            self.source_editor = None;
                        }
                        if ui.button("Cancel").clicked() {
                            self.source_editor = None;
                        }
                    });
                    // store edited id back
                    if let Some(e) = self.source_editor.as_mut() {
                        e.external_id = id;
                        e.provider = provider;
                    }
                });
        }

        if let Some(cl) = &mut self.changelog {
            let mut open = cl.open;
            egui::Window::new(format!("Changelog: {}", cl.title))
                .open(&mut open)
                .default_size([640.0, 420.0])
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        ui.label(egui::RichText::new(&cl.text).small());
                    });
                });
            cl.open = open;
            if !open {
                self.changelog = None;
            }
        }
    }

    pub fn addons_filtered(&self) -> Vec<AddonState> {
        let f = self.filter.trim().to_lowercase();
        let mut rows: Vec<AddonState> = self
            .addons
            .iter()
            .filter(|a| {
                f.is_empty()
                    || a.name.to_lowercase().contains(&f)
                    || a.folders.iter().any(|d| d.to_lowercase().contains(&f))
            })
            .cloned()
            .collect();
        crate::tabs::sort_addons(&mut rows, self.sort_col, self.sort_dir);
        rows
    }

    pub fn update_repo(&self) -> Option<String> {
        self.cfg
            .app
            .update_repo
            .clone()
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty())
    }
}

pub fn fmt_ts(ts: i64) -> String {
    chrono::DateTime::<chrono::Local>::from(
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(ts.max(0) as u64),
    )
    .format("%H:%M")
    .to_string()
}

/// Crude HTML -> text for changelog display (curse returns HTML).
pub fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push('\n');
            }
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    while out.contains("\n\n\n") {
        out = out.replace("\n\n\n", "\n\n");
    }
    out.trim().to_string()
}

impl AppGui {
    /// Apply `[ui] scale` as egui's zoom_factor — the supported way to scale
    /// *on top of* the OS DPI factor (egui multiplies it by native pixels_per_point).
    pub fn apply_scale(&mut self, ctx: &egui::Context) {
        let want = self.cfg.ui.scale.clamp(0.5, 2.5);
        if (ctx.zoom_factor() - want).abs() > 0.001 {
            ctx.set_zoom_factor(want);
        }
        self.applied_scale = self.cfg.ui.scale;
    }

    /// Apply `[ui] theme`. Both egui styles are configured in `setup_style`;
    /// this only picks which one shows (System follows the OS appearance).
    pub fn apply_theme(&mut self, ctx: &egui::Context) {
        let theme = self.cfg.ui.theme;
        ctx.set_theme(match theme {
            Theme::System => egui::ThemePreference::System,
            Theme::Dark => egui::ThemePreference::Dark,
            Theme::Light => egui::ThemePreference::Light,
        });
        self.applied_theme = Some(theme);
    }

    /// Ctrl+= / Ctrl+- live zoom, persisted to the ini.
    pub fn zoom_shortcuts(&mut self, ctx: &egui::Context) {
        let delta = ctx.input_mut(|i| {
            let cmd = egui::Modifiers::COMMAND;
            if i.consume_key(cmd, egui::Key::Equals) || i.consume_key(cmd, egui::Key::Plus) {
                Some(0.05f32)
            } else if i.consume_key(cmd, egui::Key::Minus) {
                Some(-0.05)
            } else {
                None
            }
        });
        let Some(d) = delta else { return };
        let mut c = (*self.cfg).clone();
        c.ui.scale = ((c.ui.scale + d) * 20.0).round() / 20.0;
        c.ui.scale = c.ui.scale.clamp(0.5, 2.5);
        self.cfg = Arc::new(c.clone());
        self.apply_scale(ctx);
        self.toast = Some((format!("zoom {:.0}%", c.ui.scale * 100.0), std::time::Instant::now()));
        let path = AppConfig::config_path(&self.data_dir);
        let _ = c.save(&path);
    }

    fn show_window(&self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    fn maybe_save_window_size(&mut self) {
        self.size_save += 1;
        if self.size_save % 120 != 0 {
            return; // every ~minute
        }
        let Some(w) = self.window_size else { return };
        let mut c = (*self.cfg).clone();
        c.ui.window_w = w.x.max(820.0);
        c.ui.window_h = w.y.max(540.0);
        self.cfg = Arc::new(c.clone());
        let path = AppConfig::config_path(&self.data_dir);
        let _ = c.save(&path);
    }
}

impl eframe::App for AppGui {
    /// Runs before every UI pass, and also while the window is hidden (tray).
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events(ctx);
        let want_title = match &self.self_update_available {
            Some(v) => format!("Addons Forever  (update {v} available)"),
            None => "Addons Forever".into(),
        };
        if want_title != self.window_title {
            self.window_title = want_title.clone();
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(want_title));
        }
        if (self.applied_scale - self.cfg.ui.scale).abs() > 0.001 {
            self.apply_scale(ctx);
        }
        if self.applied_theme != Some(self.cfg.ui.theme) {
            self.apply_theme(ctx);
        }

        if let Some(tray) = &self.tray {
            match tray.pump() {
                crate::tray::TrayAction::Show => self.show_window(ctx),
                crate::tray::TrayAction::Check => self.send(Cmd::Check),
                crate::tray::TrayAction::Quit => {
                    self.quit = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                crate::tray::TrayAction::None => {}
            }
        }

        // close button -> hide to tray (when configured)
        if !self.quit
            && self.tray.is_some()
            && self.cfg.ui.minimize_to_tray
            && ctx.input(|i| i.viewport().close_requested())
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }

        if let Some(rect) = ctx.input(|i| i.viewport().inner_rect) {
            self.window_size = Some(rect.size());
        }
        self.maybe_save_window_size();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.zoom_shortcuts(&ui.ctx().clone());
        egui::Panel::top("top").show(ui, |ui| {
            self.top_bar(ui);
            ui.add_space(2.0);
            self.tab_bar(ui);
        });
        egui::Panel::bottom("status").show(ui, |ui| {
            self.status_bar(ui);
        });
        egui::CentralPanel::default().show(ui, |ui| {
            self.render_tab(ui);
        });
        let ctx = ui.ctx().clone();
        self.dialogs(&ctx);

        ctx.request_repaint_after(std::time::Duration::from_millis(
            if self.job.is_some() { 120 } else { 600 },
        ));
    }
}