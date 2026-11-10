//! GUI ↔ worker bridge: commands, events, and the tokio runtime loop.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::mpsc;

use addons_forever_core::config::AppConfig;
use addons_forever_core::engine::deps::{self, DepStatus};
use addons_forever_core::engine::install::{self, InstallProgress};
use addons_forever_core::engine::scan;
use addons_forever_core::engine::update::{self, UpdateInfo};
use addons_forever_core::providers::Registry;
use addons_forever_core::store::{AddonState, StateStore};
use addons_forever_core::types::{AppCtx, Channel, FileSelect, ProviderKind, SearchItem};

/// Self-update source: a GitHub `owner/repo` (optional, set via `[app] update_repo`).

#[derive(Debug, Clone)]
pub enum Cmd {
    Rescan,
    /// Switch client flavor: drop the running scan, restore that flavor's
    /// list from the session cache (or clear), and rescan its folder.
    SetFlavor(addons_forever_core::flavor::Flavor),
    Check,
    ApplyAll,
    /// Install updates for a single addon.
    ApplyKey(String),
    /// Re-install/update a known addon.
    InstallExisting(String),
    /// Fresh install of a searched addon.
    InstallNew { provider: ProviderKind, external_id: String, name: String, channel: Option<Channel> },
    Uninstall(String),
    Search { provider: ProviderKind, query: String, page: u32 },
    Changelog { key: String },
    SetFlags { key: String, auto: Option<bool>, ignored: Option<bool>, channel: Option<Channel> },
    /// Turn auto-update on/off for every tracked addon at once.
    SetAutoAll(bool),
    /// Write a portable JSON manifest of the tracked addons.
    ExportAddons(std::path::PathBuf),
    /// Install every addon listed in a portable JSON manifest.
    ImportAddons(std::path::PathBuf),
    /// Manual provider binding ("set source").
    Override { key: String, provider: ProviderKind, external_id: String },
    /// Install by pasting a URL (github repo, curse slug page, mmoui file, zip URL).
    InstallUrl(String),
    SaveConfig(Box<AppConfig>),
    /// Query the app release feed (no install).
    CheckAppUpdate,
    /// Install the app update (swap + restart-on-next-launch).
    SelfUpdate,
    /// Result of a background scan; dropped when a newer scan has started.
    ScanFinished { gen: u64, report: Box<scan::ScanReport> },
}

#[derive(Debug, Clone)]
pub enum Ev {
    /// Full store snapshot after every mutation (GUI renders from this).
    State { addons: Vec<AddonState> },
    ScanProgress { folder: String, done: usize, total: usize },
    ScanDone { matched: usize, unmatched: usize },
    CheckProgress { name: String, done: usize, total: usize },
    CheckDone { updates: Vec<UpdateInfo>, failed: Vec<(String, String)>, offline: bool },
    InstallProgress { key: String, name: String, phase: String, bytes: u64, total: Option<u64> },
    InstallDone { key: String, name: String, version: Option<String> },
    Failed { key: String, msg: String },
    SearchDone {
        #[allow(dead_code)] // reserved for UI filtering
        provider: ProviderKind,
        query: String,
        page: u32,
        items: Vec<SearchItem>,
        total: u32,
    },
    ChangelogDone { key: String, title: String, text: String },
    /// A newer app release exists (the update flows live in Settings).
    SelfUpdateAvailable { version: String },
    /// A just-installed addon declares dependencies that are missing from the
    /// AddOns folder; the GUI offers to install them.
    DepsFound { addon: String, deps: Vec<DepStatus> },
    Log { line: String },
    SelfUpdateDone { info: String },
}

pub struct WorkerHandle {
    pub tx: mpsc::UnboundedSender<Cmd>,
    pub rx: mpsc::UnboundedReceiver<Ev>,
}

/// Spawn the worker thread with its own tokio runtime; it owns the StateStore.
pub const EXPORT_FORMAT: &str = "addons-forever-export";

#[derive(serde::Serialize, serde::Deserialize)]
pub struct ExportEntry {
    pub provider: String,
    pub external_id: String,
    pub name: String,
    pub auto_update: bool,
    pub channel: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct ExportManifest {
    pub format: String,
    pub version: u32,
    pub exported_at: String,
    pub addons: Vec<ExportEntry>,
}

pub fn spawn(cfg: Arc<AppConfig>, data_dir: PathBuf, app_version: &'static str) -> WorkerHandle {
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<Cmd>();
    let handle_tx = cmd_tx.clone();
    let (ev_tx, ev_rx) = mpsc::unbounded_channel::<Ev>();

    std::thread::Builder::new()
        .name("fa-worker".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("tokio runtime");
            rt.block_on(async move {
                run(cmd_tx.clone(), &mut cmd_rx, &ev_tx, cfg, data_dir, app_version).await;
            });
        })
        .expect("spawn worker thread");

    WorkerHandle { tx: handle_tx, rx: ev_rx }
}

struct Worker {
    ev: mpsc::UnboundedSender<Ev>,
    /// Sends ScanFinished back into the command loop (spawned scans).
    cmd_tx: mpsc::UnboundedSender<Cmd>,
    cfg: Arc<AppConfig>,
    ctx: AppCtx,
    data_dir: PathBuf,
    store: StateStore,
    /// Bumped on every scan start / flavor switch: running scans carry the
    /// generation they started with and are dropped when it goes stale.
    gen: u64,
    /// Scanned lists per flavor for this session: switching flavors shows the
    /// known state instantly, then the rescan refreshes it.
    flavor_cache: std::collections::HashMap<addons_forever_core::flavor::Flavor, Vec<AddonState>>,
    _watcher: Option<scan::WatcherHandle>,
    watcher_tx: mpsc::UnboundedSender<()>,
    watcher_rx: mpsc::UnboundedReceiver<()>,
}

impl Worker {
    fn log(&self, line: impl AsRef<str>) {
        let ts = chrono_now();
        let _ = self.ev.send(Ev::Log { line: format!("[{ts}] {}", line.as_ref()) });
    }

    fn addons_dir(&self) -> Option<PathBuf> {
        self.cfg.addons_dir()
    }

    fn sel(&self, channel: Channel) -> FileSelect {
        FileSelect::from_cfg_channel(&self.cfg, channel)
    }

    /// Store is in-memory; user intent (auto/channel/ignored pins) goes to the
    /// ini's `[addon ...]` sections. Nothing else is written to disk.
    fn save_state(&mut self) {
        let mut c = (*self.cfg).clone();
        if self.store.write_overrides(&mut c) {
            let path = addons_forever_core::config::AppConfig::config_path(&self.data_dir);
            if let Err(e) = c.save(&path) {
                self.log(format!("config save failed: {e}"));
            } else {
                self.cfg = Arc::new(c);
            }
        }
        let addons: Vec<AddonState> = self.store.addons.values().cloned().collect();
        let _ = self.ev.send(Ev::State { addons });
    }

    fn emit_state(&self) {
        let addons: Vec<AddonState> = self.store.addons.values().cloned().collect();
        let _ = self.ev.send(Ev::State { addons });
    }

    fn setup_watcher(&mut self) {
        self._watcher = None;
        let Some(dir) = self.addons_dir() else { return };
        match scan::spawn_watcher(dir.clone(), {
            let tx = self.watcher_tx.clone();
            move || {
                let _ = tx.send(());
            }
        }) {
            Ok(h) => {
                self._watcher = Some(h);
                self.log(format!("watching {}", dir.display()));
            }
            Err(e) => self.log(format!("watcher unavailable: {e}")),
        }
    }

    /// Background scan with a hard timeout, so an unreachable folder
    /// (flavor not installed, network share asleep) can never hang.
    /// The result is delivered as Cmd::ScanFinished and dropped if a newer
    /// scan (or a flavor switch) superseded it.
    fn start_scan(&mut self) {
        let Some(dir) = self.addons_dir() else {
            self.log("scan: no AddOns directory configured");
            let _ = self.ev.send(Ev::ScanDone { matched: 0, unmatched: 0 });
            return;
        };
        self.gen += 1;
        let gen = self.gen;
        let ctx = self.ctx.clone();
        let sel = self.sel(self.cfg.updates.channel);
        let ev = self.ev.clone();
        let tx = self.cmd_tx.clone();
        tokio::spawn(async move {
            let mut progress = |folder: &str, done: usize, total: usize| {
                let _ = ev.send(Ev::ScanProgress { folder: folder.to_string(), done, total });
            };
            let fut = scan::scan_addons(&ctx, &dir, &sel, &mut progress);
            match tokio::time::timeout(std::time::Duration::from_secs(120), fut).await {
                Ok(Ok(report)) => {
                    let _ = tx.send(Cmd::ScanFinished { gen, report: Box::new(report) });
                }
                Ok(Err(e)) => {
                    let _ = ev.send(Ev::Failed { key: String::new(), msg: format!("Scan: {e}") });
                }
                Err(_) => {
                    let _ = ev.send(Ev::Failed {
                        key: String::new(),
                        msg: "Scan timed out after 120s (folder unreadable or provider unreachable)".into(),
                    });
                }
            }
        });
    }

    /// Remember the live store under `flavor` for instant switching back.
    fn cache_flavor_store(&mut self, flavor: addons_forever_core::flavor::Flavor) {
        let cur: Vec<AddonState> = self.store.addons.values().cloned().collect();
        self.flavor_cache.insert(flavor, cur);
    }

    /// Swap the live store to `flavor`'s cached list, or clear it when the
    /// flavor was not scanned this session.
    fn restore_flavor_store(&mut self, flavor: addons_forever_core::flavor::Flavor) {
        self.store = match self.flavor_cache.get(&flavor) {
            Some(list) => {
                let mut st = StateStore::default();
                for a in list {
                    st.upsert(a.clone());
                }
                st
            }
            None => StateStore::default(),
        };
    }

    /// Switch the client flavor: cancel any running scan, swap to that
    /// flavor's cached list (or clear the view), persist, and rescan.
    async fn do_set_flavor(&mut self, flavor: addons_forever_core::flavor::Flavor) {
        if self.cfg.app.flavor == flavor {
            return;
        }
        let old = self.cfg.app.flavor;
        self.cache_flavor_store(old);
        self.gen += 1; // supersede running scans
        let mut c = (*self.cfg).clone();
        c.app.flavor = flavor;
        self.cfg = Arc::new(c);
        self.ctx = AppCtx::new(self.cfg.clone(), self.ctx.app_version);
        let path = addons_forever_core::config::AppConfig::config_path(&self.data_dir);
        if let Err(e) = self.cfg.save(&path) {
            self.log(format!("config save failed: {e}"));
        }
        self.restore_flavor_store(flavor);
        self.setup_watcher();
        self.log(format!("switched to {}; rescanning", flavor.label()));
        self.emit_state();
        self.start_scan();
    }

    async fn do_scan(&mut self) {
        // AF-24: a direct scan is its own generation: results that race a
        // later config/flavor change are dropped, not committed
        self.gen += 1;
        let my_gen = self.gen;
        let Some(dir) = self.addons_dir() else {
            let set = self
                .cfg
                .app
                .addons_dir
                .as_deref()
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty());
            match set {
                Some(p) => self.log(format!(
                    "scan: configured AddOns directory not found: {p} (check [app] addons_dir in the ini; auto-detection is {})",
                    if self.cfg.app.auto_detect { "on (found nothing)" } else { "off" }
                )),
                None => self.log("scan: no AddOns directory configured (set the AddOns folder in Settings and press Save settings)"),
            }
            let _ = self.ev.send(Ev::ScanDone { matched: 0, unmatched: 0 });
            return;
        };
        let ev = self.ev.clone();
        let sel = self.sel(self.cfg.updates.channel);
        self.log(format!("scanning {}", dir.display()));
        let mut progress = |folder: &str, done: usize, total: usize| {
            let _ = ev.send(Ev::ScanProgress { folder: folder.to_string(), done, total });
        };
        let fut = scan::scan_addons(&self.ctx, &dir, &sel, &mut progress);
        match tokio::time::timeout(std::time::Duration::from_secs(120), fut).await {
            Ok(Ok(report)) => {
                if my_gen != self.gen {
                    self.log("scan superseded by a newer one; result dropped");
                    return;
                }
                let matched = report.matched.len();
                let unmatched = report.unmatched.len();
                scan::apply_scan_report(&mut self.store, &report, &self.cfg);
                self.save_state();
                self.log(format!("scan done: {matched} matched, {unmatched} unmanaged"));
                // common mistake: pointing the app at the game install root
                // (which has an Interface folder) instead of Interface\AddOns
                if matched == 0 && dir.join("Interface").is_dir() && (unmatched <= 2) {
                    self.log(format!(
                        "hint: {} contains an Interface folder, so it looks like a game install root; the addons live in {}/AddOns",
                        dir.display(),
                        dir.join("Interface").display()
                    ));
                }
                let _ = self.ev.send(Ev::ScanDone { matched, unmatched });
            }
            Ok(Err(e)) => {
                self.log(format!("scan failed: {e}"));
                let _ = self.ev.send(Ev::Failed { key: String::new(), msg: format!("Scan: {e}") });
            }
            Err(_) => {
                self.log("scan timed out");
                let _ = self.ev.send(Ev::Failed {
                    key: String::new(),
                    msg: "Scan timed out after 120s (folder unreadable or provider unreachable)".into(),
                });
            }
        }
    }

    async fn do_check(&mut self) -> update::CheckOutcome {
        let reg = Registry::new(&self.ctx);
        let sel = self.sel(self.cfg.updates.channel);
        let ev = self.ev.clone();
        let outcome = match update::check_updates(
            &self.ctx,
            &mut self.store,
            &reg,
            &sel,
            &mut |name, done, total| {
                let _ = ev.send(Ev::CheckProgress { name: name.to_string(), done, total });
            },
        )
        .await
        {
            Ok(o) => o,
            Err(e) => {
                self.log(format!("check failed: {e}"));
                let _ = self.ev.send(Ev::Failed { key: String::new(), msg: format!("Check: {e}") });
                return Default::default();
            }
        };
        self.save_state();
        if !outcome.updates.is_empty() {
            self.log(format!("{} update(s) available", outcome.updates.len()));
        }
        // AF-18: partial failures must never read as a complete success
        for (label, msg) in &outcome.failed {
            self.log(format!("check: {label} failed: {msg} (cached data used)"));
        }
        if outcome.offline {
            self.log("check: offline mode, evaluated from cache only");
        }
        let _ = self.ev.send(Ev::CheckDone {
            updates: outcome.updates.clone(),
            failed: outcome.failed.clone(),
            offline: outcome.offline,
        });
        outcome
    }

    async fn do_apply(&mut self, infos: Vec<UpdateInfo>) {
        if infos.is_empty() {
            self.log("nothing to update");
            return;
        }
        let Some(dir) = self.addons_dir() else {
            self.log("apply: no AddOns directory configured");
            return;
        };
        let reg = Registry::new(&self.ctx);
        let sel = self.sel(self.cfg.updates.channel);
        let ev = self.ev.clone();
        let names: std::collections::HashMap<String, String> =
            self.store.addons.iter().map(|(k, a)| (k.clone(), a.name.clone())).collect();
        self.log(format!("updating {} addon(s)", infos.len()));
        let summary = update::apply_updates(
            &self.ctx,
            &mut self.store,
            &reg,
            &sel,
            &infos,
            &dir,
            &mut |key, prog| {
                let name = names.get(key).cloned().unwrap_or_default();
                let (phase, bytes, total) = match prog {
                    InstallProgress::Downloading { bytes, total } => ("download", *bytes, *total),
                    other => (phase_str(other), 0, None),
                };
                let _ = ev.send(Ev::InstallProgress {
                    key: key.to_string(),
                    name,
                    phase: phase.into(),
                    bytes,
                    total,
                });
            },
        )
        .await;
        match summary {
            Ok(s) => {
                for (key, version) in &s.updated_keys {
                    let name = names.get(key).cloned().unwrap_or_default();
                    let _ = ev.send(Ev::InstallDone {
                        key: key.clone(),
                        name,
                        version: version.clone(),
                    });
                }
                for (key, msg) in &s.failed {
                    let _ = ev.send(Ev::Failed { key: key.clone(), msg: msg.clone() });
                }
                self.save_state();
                self.log(format!("apply done: {} updated, {} failed", s.updated, s.failed.len()));
            }
            Err(e) => {
                self.log(format!("apply failed: {e}"));
                let _ = ev.send(Ev::Failed { key: String::new(), msg: format!("Apply: {e}") });
            }
        }
    }

    /// After a successful install: parse the installed folders' TOC dependency
    /// headers, resolve the missing ones against the providers, and ask the
    /// GUI to offer them. Never fails the install: resolution errors are logs.
    async fn check_install_deps(&self, folders: &[String], addon_name: &str) {
        let Some(dir) = self.addons_dir() else { return };
        let declared = deps::declared_dependencies(&dir, folders);
        if declared.is_empty() {
            return;
        }
        // Embedded libraries and bundle siblings satisfy deps at load time.
        let present = deps::embedded_names(&dir, folders);
        let mut unmet = deps::unmet_dependencies(&dir, &declared, &present);
        if unmet.is_empty() {
            return;
        }
        let reg = Registry::new(&self.ctx);
        let sel = self.sel(self.cfg.updates.channel);
        for dep in &mut unmet {
            dep.source = deps::resolve_source(&self.ctx, &reg, &sel, &dep.name).await;
            match &dep.source {
                Some((provider, id)) => self.log(format!(
                    "dependency of {addon_name}: {} ({}, resolved to {}#{id})",
                    dep.name, "found", provider.label()
                )),
                None => self.log(format!(
                    "dependency of {addon_name}: {} (not found in providers)",
                    dep.name
                )),
            }
        }
        let _ = self.ev.send(Ev::DepsFound { addon: addon_name.to_string(), deps: unmet });
    }

    async fn do_install_existing(&mut self, key: String) {
        let Some(st) = self.store.addons.get(&key).cloned() else { return };
        let reg = Registry::new(&self.ctx);
        let Some(dir) = self.addons_dir() else { return };
        let ev = self.ev.clone();
        let name = st.name.clone();
        let sel = self.sel(st.channel);
        let provider = reg.get(st.provider);
        let owned = self.store.folder_owners();
        let old_folders = st.folders.clone();
        let res: Result<(), String> = async {
            let metas = provider.meta(&self.ctx, std::slice::from_ref(&st.external_id)).await.map_err(|e| e.to_string())?;
            let meta = metas.into_iter().next().ok_or("provider returned no data")?;
            let file = provider
                .pick(&meta, &sel)
                .ok_or("no compatible file for this client/channel")?;
            let mut prog = {
                let ev = ev.clone();
                let key = key.clone();
                let name = name.clone();
                move |p: InstallProgress| {
                    let (phase, bytes, total) = match p {
                        InstallProgress::Downloading { bytes, total } => ("download", bytes, total),
                        other => (phase_str(&other), 0, None),
                    };
                    let _ = ev.send(Ev::InstallProgress {
                        key: key.clone(),
                        name: name.clone(),
                        phase: phase.into(),
                        bytes,
                        total,
                    });
                }
            };
            let outcome = install::install(&self.ctx, &dir, &meta, &file, &owned, &key, &mut prog)
                .await
                .map_err(|e| e.to_string())?;
            if let Some(st) = self.store.addons.get_mut(&key) {
                st.installed_file_id = Some(file.file_id.clone());
                st.installed_version = outcome.version.clone();
                st.folders = outcome.folders.clone();
                st.size_bytes = outcome.size_bytes;
                st.pending_update = None;
                st.cached_remote = Some(meta.clone());
            }
            for r in install::cleanup_dropped_roots(&dir, &old_folders, &outcome.folders, &owned, &key) {
                self.log(format!("removed folder {r}, dropped by the update of {name}"));
            }
            let _ = ev.send(Ev::InstallDone { key: key.clone(), name: name.clone(), version: outcome.version });
            self.check_install_deps(&outcome.folders, &name).await;
            Ok(())
        }
        .await;
        if let Err(msg) = res {
            self.log(format!("install {name} failed: {msg}"));
            let _ = ev.send(Ev::Failed { key, msg });
        }
        self.save_state();
    }

    async fn do_install_new(&mut self, provider: ProviderKind, external_id: String, name: String, channel: Option<Channel>) {
        let reg = Registry::new(&self.ctx);
        let Some(dir) = self.addons_dir() else {
            self.log("install: no AddOns directory configured (set it in Settings and press Save settings)");
            return;
        };
        let ev = self.ev.clone();
        let key = AddonState::key(provider, &external_id);
        // AF-20: pick the artifact under the requested channel (imported
        // entries carry one), not always the global default
        let channel = channel.unwrap_or(self.cfg.updates.channel);
        let sel = self.sel(channel);
        let prov = reg.get(provider);
        let owned = self.store.folder_owners();
        let res: Result<(), String> = async {
            let metas = prov.meta(&self.ctx, std::slice::from_ref(&external_id)).await.map_err(|e| e.to_string())?;
            let meta = metas.into_iter().next().ok_or("provider returned no data")?;
            // Provider records get renamed and recycled (observed on CurseForge:
            // an addon id found by exact-name search resolving to an unrelated
            // project). Say so in the log instead of silently installing it.
            let want = name.trim();
            let have = meta.name.trim();
            if !want.is_empty() && !have.is_empty() && !want.eq_ignore_ascii_case(have) {
                self.log(format!(
                    "note: {}:{external_id} is published as \"{have}\" (searched as \"{want}\")",
                    provider.label()
                ));
            }
            let file = prov.pick(&meta, &sel).ok_or("no compatible file for this client/channel")?;
            let mut prog = {
                let ev = ev.clone();
                let key = key.clone();
                let name = name.clone();
                move |p: InstallProgress| {
                    let (phase, bytes, total) = match p {
                        InstallProgress::Downloading { bytes, total } => ("download", bytes, total),
                        other => (phase_str(&other), 0, None),
                    };
                    let _ = ev.send(Ev::InstallProgress {
                        key: key.clone(),
                        name: name.clone(),
                        phase: phase.into(),
                        bytes,
                        total,
                    });
                }
            };
            let outcome = install::install(&self.ctx, &dir, &meta, &file, &owned, &key, &mut prog)
                .await
                .map_err(|e| e.to_string())?;
            let mut st = AddonState {
                key: key.clone(),
                provider,
                external_id: external_id.clone(),
                name: meta.name.clone(),
                site_url: meta.site_url.clone(),
                repo_url: meta.repo_url.clone(),
                icon_url: meta.icon_url.clone(),
                folders: outcome.folders.clone(),
                fingerprints: vec![],
                installed_version: outcome.version.clone(),
                installed_file_id: Some(file.file_id.clone()),
                size_bytes: outcome.size_bytes,
                channel,
                auto_update: self.cfg.updates.auto_update,
                ignored: false,
                manual: provider == ProviderKind::Zip,
                cached_remote: Some(meta.clone()),
                cached_at: Some(now_unix()),
                last_checked: Some(now_unix()),
                provider_data: serde_json::Value::Null,
                pending_update: None,
            };
            if st.name.is_empty() {
                st.name = name.clone();
            }
            self.store.upsert(st);
            let _ = ev.send(Ev::InstallDone { key: key.clone(), name: name.clone(), version: outcome.version });
            self.check_install_deps(&outcome.folders, &name).await;
            Ok(())
        }
        .await;
        if let Err(msg) = res {
            self.log(format!("install {name} failed: {msg}"));
            let _ = ev.send(Ev::Failed { key, msg });
        }
        self.save_state();
    }

    async fn do_uninstall(&mut self, key: String) {
        let Some(st) = self.store.addons.get(&key).cloned() else { return };
        let Some(dir) = self.addons_dir() else { return };
        match install::uninstall(&self.ctx, &dir, &st).await {
            Ok(()) => {
                self.log(format!("removed {}", st.name));
                self.store.addons.remove(&key);
                self.save_state();
            }
            Err(e) => {
                let _ = self.ev.send(Ev::Failed { key, msg: format!("Uninstall: {e}") });
            }
        }
    }

    fn do_export(&mut self, path: std::path::PathBuf) {
        let mut addons: Vec<ExportEntry> = self
            .store
            .addons
            .values()
            .map(|a| ExportEntry {
                provider: a.provider.as_str().to_string(),
                external_id: a.external_id.clone(),
                name: a.name.clone(),
                auto_update: a.auto_update,
                channel: a.channel.as_str().to_string(),
            })
            .collect();
        addons.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        let manifest = ExportManifest {
            format: EXPORT_FORMAT.to_string(),
            version: 1,
            exported_at: chrono::Local::now().to_rfc3339(),
            addons,
        };
        match serde_json::to_string_pretty(&manifest) {
            Ok(json) => {
                let n = manifest.addons.len();
                match std::fs::write(&path, json) {
                    Ok(()) => self.log(format!("exported {n} addon(s) to {}", path.display())),
                    Err(e) => self.log(format!("export failed: {e}")),
                }
            }
            Err(e) => self.log(format!("export failed: {e}")),
        }
    }

    async fn do_import(&mut self, path: std::path::PathBuf) {
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(e) => {
                self.log(format!("import failed: {e}"));
                return;
            }
        };
        let manifest: ExportManifest = match serde_json::from_str::<ExportManifest>(&raw) {
            Ok(m) if m.format == EXPORT_FORMAT => m,
            Ok(_) => {
                self.log("import failed: not an addons-forever export file");
                return;
            }
            Err(e) => {
                self.log(format!("import failed: {e}"));
                return;
            }
        };
        if manifest.version > 1 {
            self.log(format!("import failed: unsupported manifest version {}", manifest.version));
            let _ = self.ev.send(Ev::Failed {
                key: "import".into(),
                msg: format!("unsupported manifest version {} (this build reads version 1)", manifest.version),
            });
            return;
        }
        let n = manifest.addons.len();
        self.log(format!("importing {n} addon(s) from {}", path.display()));
        let mut ok = 0usize;
        for e in manifest.addons {
            let Some(provider) = ProviderKind::from_key(&e.provider) else {
                self.log(format!("import skip `{}`: unknown provider `{}`", e.name, e.provider));
                continue;
            };
            let entry_channel = Channel::from_key(&e.channel);
            self.do_install_new(provider, e.external_id.clone(), e.name.clone(), entry_channel).await;
            let key = addons_forever_core::store::AddonState::key(provider, &e.external_id);
            if let Some(st) = self.store.addons.get_mut(&key) {
                st.auto_update = e.auto_update;
                if let Some(ch) = entry_channel {
                    st.channel = ch;
                }
                ok += 1;
            }
        }
        self.save_state();
        self.log(format!("import done: {ok}/{n} addon(s) in your set"));
    }

    /// Search fetches ALL result pages (bounded) so the UI can sort the complete
    /// result list, not just one server page.
    async fn do_search(&mut self, provider: ProviderKind, query: String, _page: u32) {
        const PER: usize = 50;
        const CAP: usize = 500;
        let reg = Registry::new(&self.ctx);
        let sel = self.sel(self.cfg.updates.channel);
        let mut all: Vec<SearchItem> = Vec::new();
        let mut total = 0usize;
        let mut page = 0u32;
        let mut err = None;
        loop {
            match reg.get(provider).search(&self.ctx, &query, page, PER as u32, &sel).await {
                Ok(p) => {
                    total = p.total as usize;
                    if p.items.is_empty() {
                        break;
                    }
                    let n = p.items.len();
                    all.extend(p.items);
                    if all.len() >= total.min(CAP) || n < PER {
                        break;
                    }
                    page += 1;
                }
                Err(e) => {
                    err = Some(e.to_string());
                    break;
                }
            }
        }
        if let Some(e) = err {
            if all.is_empty() {
                let _ = self
                    .ev
                    .send(Ev::Failed { key: "search".into(), msg: format!("{} search: {e}", provider.label()) });
                return;
            }
        }
        // The search API ignores `order=desc` for popularity sorts and returns
        // ascending, i.e. zero-download renames and spam first. We fetched all
        // pages; show the popular projects first.
        all.sort_by(|a, b| {
            b.downloads
                .unwrap_or(0)
                .cmp(&a.downloads.unwrap_or(0))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        let _ = self.ev.send(Ev::SearchDone { provider, query, page: 0, items: all, total: total as u32 });
    }

    async fn do_changelog(&mut self, key: String) {
        let Some(st) = self.store.addons.get(&key).cloned() else { return };
        let reg = Registry::new(&self.ctx);
        let prov = reg.get(st.provider);
        let meta = match st.cached_remote.clone() {
            Some(m) => m,
            None => {
                let ev = self.ev.clone();
                let _ = ev.send(Ev::ChangelogDone { key, title: st.name, text: "No cached metadata. Update first.".into() });
                return;
            }
        };
        let sel = self.sel(st.channel);
        let title = st.name.clone();
        let file = prov.pick(&meta, &sel).or_else(|| meta.files.first().cloned());
        let text = match file {
            Some(f) => match prov.changelog(&self.ctx, &meta, &f).await {
                Ok(Some(t)) => t,
                Ok(None) => "No changelog provided by the source.".into(),
                Err(e) => format!("Failed: {e}"),
            },
            None => "No file available.".into(),
        };
        let _ = self.ev.send(Ev::ChangelogDone { key, title, text });
    }

    fn do_save_config(&mut self, mut new_cfg: AppConfig, path: &Path) {
        // AF-13: the settings form carries a GUI-side snapshot; the worker's
        // [addon ...] intent (auto-update, channel, ignored, pins written
        // since the form opened) is newer and must survive the whole-config
        // replacement.
        for (k, v) in self.cfg.addons.clone() {
            new_cfg.addons.insert(k, v);
        }
        if let Err(e) = new_cfg.save(path) {
            // a read-only location (Program Files without elevation) is the
            // classic reason settings never take effect
            let _ = self.ev.send(Ev::Failed {
                key: "config".into(),
                msg: format!(
                    "Save: {e} - the settings file {} is not writable; move the app folder somewhere writable (e.g. your user profile) or run it once as administrator",
                    path.display()
                ),
            });
            return;
        }
        let old_flavor = self.cfg.app.flavor;
        let old_dir = (
            self.cfg.app.addons_dir.clone().unwrap_or_default(),
            self.cfg.app.auto_detect,
        );
        self.cfg = Arc::new(new_cfg);
        self.ctx = AppCtx::new(self.cfg.clone(), self.ctx.app_version);
        self.setup_watcher();
        self.log("settings saved");
        self.emit_state();
        // flavor changed via the settings form: cache swap + rescan like the
        // top-bar switch
        if self.cfg.app.flavor != old_flavor {
            self.cache_flavor_store(old_flavor);
            self.gen += 1;
            let f = self.cfg.app.flavor;
            self.restore_flavor_store(f);
            self.log(format!("switched to {}; rescanning", f.label()));
            self.emit_state();
            self.start_scan();
        } else {
            // same flavor: a changed AddOns target is a different installation.
            // Drop the old rows so an operation can never act on the previous
            // folder set, then rescan the new one.
            let new_dir = (
                self.cfg.app.addons_dir.clone().unwrap_or_default(),
                self.cfg.app.auto_detect,
            );
            if new_dir != old_dir {
                self.gen += 1; // supersede in-flight scans of the old directory
                self.store = StateStore::default();
                self.log("AddOns directory changed; inventory cleared, rescanning");
                self.emit_state();
                self.start_scan();
            }
        }
    }

    /// Query the app release feed. On startup (`manual == false`) a found
    /// update only announces; with `[app] self_update = true` it installs.
    async fn do_check_app_update(&mut self, manual: bool) {
        let Some(repo) = self.update_repo() else {
            if manual {
                let _ = self.ev.send(Ev::SelfUpdateDone {
                    info: "app self-update is not configured. Set `update_repo = owner/repo` under [app] in the .ini (GitHub Releases)".into(),
                });
            }
            return;
        };
        if self.cfg.network.offline {
            if manual {
                let _ = self.ev.send(Ev::SelfUpdateDone { info: "offline mode: no update check performed".into() });
            }
            return;
        }
        let ctx = self.ctx.clone();
        let ev = self.ev.clone();
        let auto = self.cfg.app.self_update;
        match crate::selfupdate::check(&ctx, &repo).await {
            Ok(Some(version)) => {
                self.log(format!("app update available: {}", version));
                let _ = ev.send(Ev::SelfUpdateAvailable { version: version.clone() });
                if auto {
                    if cfg!(windows) {
                        let r = repo.clone();
                        let out = tokio::task::spawn_blocking(move || crate::selfupdate::run(&r)).await;
                        let info = match out {
                            Ok(Ok(msg)) => msg,
                            Ok(Err(e)) => format!("app update failed: {e}"),
                            Err(e) => format!("app update job panicked: {e}"),
                        };
                        self.log(info.clone());
                        let _ = ev.send(Ev::SelfUpdateDone { info });
                    } else {
                        let info = format!("app update {version} available; self-install is a Windows feature, see Settings");
                        self.log(info.clone());
                        let _ = ev.send(Ev::SelfUpdateDone { info });
                    }
                }
            }
            Ok(None) => {
                if manual {
                    let _ = ev.send(Ev::SelfUpdateDone {
                        info: format!("App is up to date ({}).", env!("CARGO_PKG_VERSION")),
                    });
                }
            }
            Err(e) => {
                self.log(format!("app update check failed: {e}"));
                if manual {
                    let _ = ev.send(Ev::SelfUpdateDone { info: format!("app update check failed: {e}") });
                }
            }
        }
    }

    fn update_repo(&self) -> Option<String> {
        self.cfg
            .app
            .update_repo
            .clone()
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty())
    }

    fn do_self_update(&mut self) {
        if self.cfg.network.offline {
            let _ = self.ev.send(Ev::SelfUpdateDone {
                info: "offline mode: the app will not download anything, including its own update".into(),
            });
            return;
        }
        let ev = self.ev.clone();
        let Some(repo) = self
            .cfg
            .app
            .update_repo
            .clone()
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty())
        else {
            let _ = ev.send(Ev::SelfUpdateDone {
                info: "app self-update is not configured. Set `update_repo = owner/repo` under [app] in the .ini (GitHub Releases)".into(),
            });
            return;
        };
        tokio::spawn(async move {
            let out = tokio::task::spawn_blocking(move || crate::selfupdate::run(&repo)).await;
            let info = match out {
                Ok(Ok(msg)) => msg,
                Ok(Err(e)) => format!("update failed: {e}"),
                Err(e) => format!("update job panicked: {e}"),
            };
            let _ = ev.send(Ev::SelfUpdateDone { info });
        });
    }
}

fn phase_str(p: &InstallProgress) -> &'static str {
    match p {
        InstallProgress::Extracting => "extract",
        InstallProgress::BackingUp => "backup",
        InstallProgress::Swapping => "install",
        InstallProgress::Done => "done",
        InstallProgress::Downloading { .. } => "download",
    }
}

async fn run(
    cmd_tx: mpsc::UnboundedSender<Cmd>,
    cmd_rx: &mut mpsc::UnboundedReceiver<Cmd>,
    ev: &mpsc::UnboundedSender<Ev>,
    cfg: Arc<AppConfig>,
    data_dir: PathBuf,
    app_version: &'static str,
) {
    // Fold legacy state.json/egui-ui.toml/logs into the ini, then delete them.
    let mut cfg = (*cfg).clone();
    if StateStore::migrate_into_cfg(&data_dir, &mut cfg) {
        let _ = cfg.save(&addons_forever_core::config::AppConfig::config_path(&data_dir));
    }
    let cfg = Arc::new(cfg);
    let store = StateStore::default();
    let (watcher_tx, watcher_rx) = mpsc::unbounded_channel();
    let mut w = Worker {
        ev: ev.clone(),
        cmd_tx,
        cfg: cfg.clone(),
        ctx: AppCtx::new(cfg.clone(), app_version),
        data_dir: data_dir.clone(),
        store,
        gen: 0,
        flavor_cache: std::collections::HashMap::new(),
        _watcher: None,
        watcher_tx,
        watcher_rx,
    };
    w.setup_watcher();
    w.emit_state();

    // startup jobs
    let cfg0 = w.cfg.clone();
    let mut startup_check_done = false;
    if cfg0.app.addons_dir.is_some() || cfg0.app.auto_detect {
        if cfg0.ui.scan_on_start {
            w.do_scan().await;
        } else {
            w.log("startup scan disabled ([ui] scan_on_start = false)");
        }
        if cfg0.updates.check_on_startup {
            let outcome = w.do_check().await;
            if cfg0.updates.auto_update {
                let targets = update::auto_update_targets(&w.store, &outcome.updates);
                if !targets.is_empty() {
                    w.log(format!("auto-update: {} addon(s)", targets.len()));
                    w.do_apply(targets).await;
                }
            }
            // AF-11: a startup check counts as a completed check, so the
            // periodic scheduler is armed without needing a manual Check
            startup_check_done = true;
        }
    } else {
        w.log("AddOns folder not configured. Open Settings.");
    }
    w.do_check_app_update(false).await;

    // AF-11: arm the periodic cycle from the startup check (a run check set
    // `last_check` before, which is why auto-update only ever worked after a
    // manual Check)
    let mut last_check: i64 = if startup_check_done { now_unix() } else { 0 };
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break };
                match cmd {
                    Cmd::Rescan => w.start_scan(),
                    Cmd::SetFlavor(f) => w.do_set_flavor(f).await,
                    Cmd::ScanFinished { gen, report } => {
                        if gen != w.gen {
                            w.log("scan superseded, result discarded");
                        } else {
                            let matched = report.matched.len();
                            let unmatched = report.unmatched.len();
                            scan::apply_scan_report(&mut w.store, &report, &w.cfg);
                            w.save_state();
                            w.log(format!("scan done: {matched} matched, {unmatched} unmanaged"));
                            let _ = w.ev.send(Ev::ScanDone { matched, unmatched });
                        }
                    }
                    Cmd::Check => { w.do_check().await; last_check = now_unix(); }
                    Cmd::ApplyAll => {
                        let outcome = w.do_check().await;
                        w.do_apply(outcome.updates).await;
                        last_check = now_unix(); // AF-11: any completed check arms the scheduler
                    }
                    Cmd::ApplyKey(key) => {
                        let info = w.store.addons.get(&key).and_then(|st| {
                            let p = st.pending_update.clone()?;
                            let target = st.cached_remote.as_ref()?.files.iter().find(|f| f.file_id == p.file_id)?.clone();
                            Some(UpdateInfo {
                                key: key.clone(),
                                name: st.name.clone(),
                                current: st.installed_version.clone(),
                                target,
                                version: p.version,
                            })
                        });
                        match info {
                            Some(i) => w.do_apply(vec![i]).await,
                            None => w.do_install_existing(key).await,
                        }
                    }
                    Cmd::InstallExisting(key) => w.do_install_existing(key).await,
                    Cmd::InstallNew { provider, external_id, name, channel } => {
                        w.do_install_new(provider, external_id, name, channel).await
                    }
                    Cmd::InstallUrl(url) => match parse_install_url(&w.ctx, &url).await {
                        Ok((provider, external_id, name)) => {
                            w.do_install_new(provider, external_id, name, None).await
                        }
                        Err(msg) => {
                            let _ = w.ev.send(Ev::Failed { key: "install-url".into(), msg: msg.clone() });
                            w.log(format!("bad install URL {url}: {msg}"));
                        }
                    },
                    Cmd::Uninstall(key) => w.do_uninstall(key).await,
                    Cmd::Search { provider, query, page } => w.do_search(provider, query, page).await,
                    Cmd::Changelog { key } => w.do_changelog(key).await,
                    Cmd::ExportAddons(p) => w.do_export(p),
                    Cmd::ImportAddons(p) => w.do_import(p).await,
                    Cmd::SetAutoAll(on) => {
                        for a in w.store.addons.values_mut() {
                            a.auto_update = on;
                        }
                        w.save_state();
                        w.log(format!("auto-update {} for all addons", if on { "enabled" } else { "disabled" }));
                    }
                    Cmd::SetFlags { key, auto, ignored, channel } => {
                        if let Some(st) = w.store.addons.get_mut(&key) {
                            if let Some(a) = auto { st.auto_update = a; }
                            if let Some(i) = ignored { st.ignored = i; }
                            if let Some(c) = channel { st.channel = c; }
                        }
                        w.save_state();
                    }
                    Cmd::Override { key, provider, external_id } => {
                        if let Some(mut st) = w.store.addons.remove(&key) {
                            st.provider = provider;
                            st.external_id = external_id.clone();
                            st.key = AddonState::key(provider, &external_id);
                            st.manual = true;
                            st.cached_remote = None;
                            w.store.upsert(st);
                            w.save_state();
                            w.log(format!("source set to {}/{}", provider.label(), external_id));
                        }
                    }
                    Cmd::SaveConfig(new_cfg) => {
                        let path = AppConfig::config_path(&w.data_dir);
                        w.do_save_config(*new_cfg, &path);
                    }
                    Cmd::CheckAppUpdate => w.do_check_app_update(true).await,
                    Cmd::SelfUpdate => w.do_self_update(),
                }
            }
            _ = w.watcher_rx.recv() => {
                // debounce bursts (the debouncer already does 1.5s)
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                while w.watcher_rx.try_recv().is_ok() {}
                w.log("AddOns folder changed, rescanning");
                w.do_scan().await;
            }
            _ = tick.tick() => {
                let c = w.cfg.clone();
                if c.updates.auto_update
                    && !c.network.offline
                    && last_check > 0
                    && now_unix() - last_check >= (c.updates.interval_minutes as i64) * 60
                {
                    w.log("scheduled update check");
                    let outcome = w.do_check().await;
                    let targets = update::auto_update_targets(&w.store, &outcome.updates);
                    w.do_apply(targets).await;
                    last_check = now_unix();
                }
            }
        }
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Parse a user-pasted install target into (provider, external_id, name).
pub(crate) async fn parse_install_url(ctx: &AppCtx, raw: &str) -> Result<(ProviderKind, String, String), String> {
    let s = raw
        .trim()
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_start_matches("www.");

    // github.com/owner/repo[/...]
    if let Some(rest) = s.strip_prefix("github.com/") {
        let segs: Vec<&str> = rest.split('/').collect();
        if segs.len() >= 2 && !segs[0].is_empty() && !segs[1].is_empty() {
            let owner = segs[0].trim_end_matches(".git");
            let repo = segs[1];
            return Ok((ProviderKind::GitHub, format!("{owner}/{repo}"), repo.to_string()));
        }
        return Err("expected github.com/owner/repo".into());
    }

    // curseforge.com/wow/addons/<slug>
    if let Some(rest) = s
        .strip_prefix("curseforge.com/wow/addons/")
        .or(s.strip_prefix("www.curseforge.com/wow/addons/"))
    {
        let slug = rest.split(['/', '?', '#']).next().unwrap_or("");
        if slug.is_empty() {
            return Err("no addon slug in CurseForge URL".into());
        }
        let id = addons_forever_core::providers::curse::resolve_slug(ctx, slug)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("mod slug not found on CurseForge")?;
        return Ok((ProviderKind::Curse, id, slug.to_string()));
    }

    // wowinterface.com/downloads/info<id>-Name.html
    if let Some(pos) = s.find("/downloads/info") {
        let digits: String = s[pos + "/downloads/info".len()..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if !digits.is_empty() {
            return Ok((ProviderKind::WowInterface, digits.clone(), digits));
        }
    }

    // plain https zip URL
    let lower = raw.to_ascii_lowercase();
    if (raw.starts_with("https://") || raw.starts_with("http://"))
        && lower.split('?').next().unwrap_or("").ends_with(".zip")
    {
        let file = raw.split('?').next().unwrap_or("addon.zip");
        let name = file.rsplit('/').next().unwrap_or("addon").trim_end_matches(".zip").to_string();
        return Ok((ProviderKind::Zip, raw.trim().to_string(), name));
    }

    // bare owner/repo
    if let Some((o, r)) = s.split_once('/') {
        let ok = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
        if !o.is_empty() && !r.is_empty() && o.chars().all(ok) && r.chars().all(ok) {
            return Ok((ProviderKind::GitHub, format!("{o}/{r}"), r.to_string()));
        }
    }

    // bare numeric CurseForge mod id
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()) {
        return Ok((ProviderKind::Curse, s.to_string(), s.to_string()));
    }

    // bare Tukui slug (elvui, tukui, ...)
    if s.len() >= 3
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Ok((ProviderKind::Tukui, s.to_string(), s.to_string()));
    }

    Err("paste a github.com/owner/repo, CurseForge addon page, WoWInterface file URL, Tukui slug, numeric CurseForge id, or a direct .zip URL".into())
}

fn chrono_now() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

/// CLI entry point into the GUI's URL parser.
pub async fn parse_install_url_pub(ctx: &AppCtx, raw: &str) -> Result<(ProviderKind, String, String), String> {
    parse_install_url(ctx, raw).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn parses_install_targets_offline() {
        // github repo URLs & bare owner/repo
        let ctx = test_ctx();
        let (p, id, _) = parse_install_url(&ctx, "https://github.com/BigWigsMods/BigWigs/releases").await.unwrap();
        assert_eq!((p, id.as_str()), (ProviderKind::GitHub, "BigWigsMods/BigWigs"));
        let (p, id, _) = parse_install_url(&ctx, "johnnystarr/some-addon").await.unwrap();
        assert_eq!((p, id.as_str()), (ProviderKind::GitHub, "johnnystarr/some-addon"));
        // direct zip
        let (p, id, _) =
            parse_install_url(&ctx, "https://cdn.example.org/files/my-addon-1.2.3.zip?v=2").await.unwrap();
        assert_eq!((p, id.as_str()), (ProviderKind::Zip, "https://cdn.example.org/files/my-addon-1.2.3.zip?v=2"));
        // numeric curse id, bare tukui slug, wowinterface URL
        let (p, id, _) = parse_install_url(&ctx, "2382").await.unwrap();
        assert_eq!((p, id.as_str()), (ProviderKind::Curse, "2382"));
        let (p, id, _) = parse_install_url(&ctx, "elvui").await.unwrap();
        assert_eq!((p, id.as_str()), (ProviderKind::Tukui, "elvui"));
        let (p, id, _) =
            parse_install_url(&ctx, "https://www.wowinterface.com/downloads/info19508-Foo.html").await.unwrap();
        assert_eq!((p, id.as_str()), (ProviderKind::WowInterface, "19508"));
        // nonsense rejected
        assert!(parse_install_url(&ctx, "?!??").await.is_err());
    }

    fn test_ctx() -> AppCtx {
        AppCtx::new(std::sync::Arc::new(AppConfig::default()), "test")
    }
}
