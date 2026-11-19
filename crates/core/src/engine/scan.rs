//! Folder scanning + ingest: enumerate AddOns, let providers claim folders,
//! merge results into the state store, and watch the directory for changes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use notify_debouncer_full::{
    new_debouncer,
    notify::{
        event::ModifyKind,
        EventKind, RecommendedWatcher, RecursiveMode,
    },
    DebounceEventResult, Debouncer, RecommendedCache,
};

use crate::config::{AppConfig, AddonOverride};
use crate::error::Result;
use crate::flavor::Flavor;
use crate::fingerprint;
use crate::providers::Registry;
use crate::store::{AddonState, StateStore};
use crate::toc::{self, ParsedToc};
use crate::types::{
    AppCtx, FileSelect, FolderInfo, ProviderKind, Provider, ScanMatch, ScanRequest,
};

/// Result of one full AddOns scan: provider-matched addons, folders no
/// provider claimed, and the scan timestamp (unix seconds).
#[derive(Debug, Clone)]
pub struct ScanReport {
    pub matched: Vec<ScanMatch>,
    pub unmatched: Vec<FolderInfo>,
    pub scanned_at: i64,
    /// Folder name -> bytes on disk (for state sizes).
    pub folder_sizes: std::collections::BTreeMap<String, u64>,
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Enumerate addon folders in `addons_dir` (skipping dot-directories and plain
/// files), parse each folder's best TOC for `flavor` and compute its Curse
/// fingerprint. Folders without a TOC get an empty `ParsedToc` and no
/// fingerprint so they can ingest as unmanaged local addons.
pub fn scan_folders(addons_dir: &Path, flavor: Flavor) -> Result<Vec<FolderInfo>> {
    let mut dirs: Vec<(String, PathBuf)> = std::fs::read_dir(addons_dir)?
        .filter_map(|e| e.ok())
        .map(|e| (e.file_name().to_string_lossy().to_string(), e.path()))
        .filter(|(name, path)| !name.starts_with('.') && path.is_dir())
        .collect();
    dirs.sort();

    let mut out = Vec::with_capacity(dirs.len());
    for (name, path) in dirs {
        let parsed = match toc::find_toc(&path, &name, flavor) {
            Some(toc_path) => {
                let raw = std::fs::read(&toc_path)?;
                toc::parse(&toc::decode_text(&raw))
            }
            None => ParsedToc::default(),
        };
        let curse_fingerprint = fingerprint::folder_fingerprint(&path)?;
        out.push(FolderInfo { size_bytes: crate::engine::install::dir_size(&path), name, path, toc: parsed, curse_fingerprint });
    }
    Ok(out)
}

/// Full scan: enumerate folders, then walk providers in
/// `Registry::scan_order()`. A folder claimed by one provider is never offered
/// to later ones. Provider failures are logged and skipped (one dead provider
/// must not break the scan).
pub async fn scan_addons(
    ctx: &AppCtx,
    addons_dir: &Path,
    sel: &FileSelect,
    on_progress: &mut (dyn FnMut(&str, usize, usize) + Send),
) -> Result<ScanReport> {
    // AF-24: the local walk is blocking filesystem work; run it off the
    // async worker so timeouts, the UI event loop, and other jobs are not
    // stalled by a slow disk
    let dir = addons_dir.to_path_buf();
    let flavor = sel.flavor;
    let folders = tokio::task::spawn_blocking(move || scan_folders(&dir, flavor))
        .await
        .map_err(|e| crate::error::Error::provider("scan", format!("scan task failed: {e}")))??;
    let reg = Registry::new(ctx);
    let providers: Vec<&dyn Provider> =
        Registry::scan_order().iter().map(|k| reg.get(*k)).collect();
    scan_with_providers(ctx, &providers, folders, sel, on_progress).await
}

/// Provider loop, split out of [`scan_addons`] so tests can inject mocks.
pub(crate) async fn scan_with_providers(
    ctx: &AppCtx,
    providers: &[&dyn Provider],
    folders: Vec<FolderInfo>,
    sel: &FileSelect,
    on_progress: &mut (dyn FnMut(&str, usize, usize) + Send),
) -> Result<ScanReport> {
    let total = folders.len();
    let folder_sizes: std::collections::BTreeMap<String, u64> =
        folders.iter().map(|f| (f.name.clone(), f.size_bytes)).collect();
    let mut remaining = folders;
    let mut matched: Vec<ScanMatch> = Vec::new();

    for provider in providers {
        if remaining.is_empty() {
            break;
        }
        if ctx.cfg.network.offline {
            // offline mode: keep the local inventory, make zero requests
            break;
        }
        let req = ScanRequest { folders: &remaining, sel };
        let found = match provider.scan(ctx, &req).await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(provider = provider.kind().as_str(), "scan failed: {e}");
                continue;
            }
        };

        // Only accept claims for folders we actually saw, deduped.
        let mut claimed: Vec<String> = Vec::new();
        for m in found {
            let mut folders = Vec::new();
            for f in m.folders {
                let known = remaining.iter().any(|r| r.name.eq_ignore_ascii_case(&f));
                let dup = folders.iter().any(|x: &String| x.eq_ignore_ascii_case(&f));
                if known && !dup {
                    folders.push(f);
                }
            }
            if folders.is_empty() {
                continue; // a match claiming no real folder is useless
            }
            claimed.extend(folders.iter().cloned());
            matched.push(ScanMatch { folders, ..m });
        }

        // Claimed folders are removed from the request for later providers.
        remaining.retain(|f| !claimed.iter().any(|c| c.eq_ignore_ascii_case(&f.name)));
        on_progress(provider.kind().label(), total - remaining.len(), total);
    }

    Ok(ScanReport { matched, unmatched: remaining, scanned_at: now_unix(), folder_sizes })
}

/// Merge a scan report into the store.
///
/// - Matched addons keep the user's settings (auto_update/channel/ignored/
///   manual/installed file id) from the previous state; fresh addons take the
///   global defaults. `cfg.addons` `[addon provider:id]` overrides win over
///   everything.
/// - Unclaimed folders become `zip:local:<folder>` pseudo-addons (manual,
///   never auto-updated), named from the TOC title when present.
/// - Managed addons whose folders are all gone from disk are dropped.
pub fn apply_scan_report(store: &mut StateStore, report: &ScanReport, cfg: &AppConfig) {
    let mut next: BTreeMap<String, AddonState> = BTreeMap::new();

    // AF-12: user-pinned sources are indexed by their installed folders, so
    // a pin survives provider re-detection and restarts (the pin wins over
    // whatever provider id the scan derived).
    let mut pins: std::collections::HashMap<String, &AddonOverride> = Default::default();
    for ov in cfg.addons.values().filter(|o| o.pinned) {
        for f in &ov.folders {
            pins.insert(f.to_lowercase(), ov);
        }
    }

    for m in &report.matched {
        let base_key = AddonState::key(m.provider, &m.external_id);
        let ov: Option<&AddonOverride> = cfg
            .addons
            .get(&base_key)
            .or_else(|| m.folders.iter().find_map(|f| pins.get(&f.to_lowercase()).copied()));
        let provider = ov.and_then(|o| o.provider).unwrap_or(m.provider);
        let external_id = ov
            .and_then(|o| o.external_id.clone())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| m.external_id.clone());
        let key = AddonState::key(provider, &external_id);
        let old = store.addons.get(&key);

        let name = ov
            .and_then(|o| o.name.clone())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| m.meta.name.clone());
        let folders = match ov.map(|o| &o.folders).filter(|f| !f.is_empty()) {
            Some(f) => f.clone(),
            None => m.folders.clone(),
        };
        let size_bytes = next_size(&report.folder_sizes, &folders);
        next.insert(
            key.clone(),
            AddonState {
                key,
                provider,
                external_id,
                name,
                site_url: m.meta.site_url.clone().or_else(|| old.and_then(|o| o.site_url.clone())),
                repo_url: m.meta.repo_url.clone().or_else(|| old.and_then(|o| o.repo_url.clone())),
                icon_url: m.meta.icon_url.clone().or_else(|| old.and_then(|o| o.icon_url.clone())),
                folders,
                fingerprints: old.map(|o| o.fingerprints.clone()).unwrap_or_default(),
                installed_version: m
                    .installed_version
                    .clone()
                    .or_else(|| old.and_then(|o| o.installed_version.clone()))
                    .or_else(|| ov.and_then(|o| o.installed_version.clone())),
                installed_file_id: ov
                    .and_then(|o| o.installed_file_id.clone())
                    .or_else(|| old.and_then(|o| o.installed_file_id.clone())),
                channel: ov
                    .and_then(|o| o.channel)
                    .or_else(|| old.map(|o| o.channel))
                    .unwrap_or(cfg.updates.channel),
                auto_update: ov
                    .and_then(|o| o.auto_update)
                    .or_else(|| old.map(|o| o.auto_update))
                    .unwrap_or(cfg.updates.auto_update),
                ignored: ov
                    .and_then(|o| o.ignored)
                    .or_else(|| old.map(|o| o.ignored))
                    .unwrap_or(false),
                manual: ov.map(|o| o.pinned).unwrap_or(false)
                    || old.map(|o| o.manual).unwrap_or(false),
                cached_remote: Some(m.meta.clone()),
                cached_at: Some(report.scanned_at),
                last_checked: old.and_then(|o| o.last_checked),
                provider_data: old.map(|o| o.provider_data.clone()).unwrap_or(serde_json::Value::Null),
                pending_update: old.and_then(|o| o.pending_update.clone()),
                size_bytes,
            },
        );
    }

    for f in &report.unmatched {
        let external_id = format!("local:{}", f.name);
        let key = AddonState::key(ProviderKind::Zip, &external_id);
        let old = store.addons.get(&key);
        let name = f.toc.title.clone().filter(|t| !t.trim().is_empty()).unwrap_or(f.name.clone());
        next.insert(
            key.clone(),
            AddonState {
                key,
                provider: ProviderKind::Zip,
                external_id,
                name,
                site_url: None,
                repo_url: None,
                icon_url: None,
                folders: vec![f.name.clone()],
                fingerprints: f.curse_fingerprint.map(|fp| vec![fp]).unwrap_or_default(),
                size_bytes: f.size_bytes,
                installed_version: f.toc.version.clone(),
                installed_file_id: None,
                channel: old.map(|o| o.channel).unwrap_or(cfg.updates.channel),
                auto_update: false, // local addons are never auto-updated
                ignored: old.map(|o| o.ignored).unwrap_or(false),
                manual: true,
                cached_remote: None,
                cached_at: None,
                last_checked: None,
                provider_data: serde_json::Value::Null,
                pending_update: None,
            },
        );
    }

    // Folders present on disk according to this report (case-insensitive).
    let disk: Vec<String> = report
        .matched
        .iter()
        .flat_map(|m| m.folders.iter())
        .chain(report.unmatched.iter().map(|f| &f.name))
        .map(|f| f.to_lowercase())
        .collect();

    let previous = std::mem::take(&mut store.addons);
    for (key, state) in previous {
        if next.contains_key(&key) {
            continue; // refreshed from this report
        }
        let alive = state.folders.iter().any(|f| disk.contains(&f.to_lowercase()));
        if state.folders.is_empty() || alive {
            next.insert(key, state); // folders not covered by this scan: keep
        }
        // folders gone from disk → drop from the store
    }

    store.addons = next;
    store.last_scan = Some(report.scanned_at);
}

/// Filesystem watcher on the AddOns dir: debounced (1.5 s) create/modify/
/// delete notifications, used by the GUI to trigger a rescan.
pub struct WatcherHandle {
    _debouncer: Debouncer<RecommendedWatcher, RecommendedCache>,
}

/// Start watching `addons_dir`; `on_change` fires (debounced) whenever the
/// folder set changes. Dropping the returned handle stops the watcher.
pub fn spawn_watcher(
    addons_dir: PathBuf,
    on_change: impl Fn() + Send + 'static,
) -> Result<WatcherHandle> {
    let mut debouncer = new_debouncer(
        Duration::from_millis(1500),
        None,
        move |result: DebounceEventResult| match result {
            Ok(events) => {
                // Only react to real changes: scans/open dialogs emit Access(Open)
                // events, and reacting to those would rescan forever.
                let changed = events.iter().any(|de| {
                    matches!(
                        de.event.kind,
                        EventKind::Create(_)
                            | EventKind::Remove(_)
                            | EventKind::Modify(ModifyKind::Name(_))
                            | EventKind::Modify(ModifyKind::Metadata(_))
                    )
                });
                if changed {
                    on_change();
                }
            }
            Err(errors) => {
                for e in errors {
                    tracing::warn!("addon watcher: {e}");
                }
            }
        },
    )
    .map_err(|e| crate::error::Error::Other(format!("watcher: {e}")))?;
    debouncer
        .watch(&addons_dir, RecursiveMode::NonRecursive)
        .map_err(|e| crate::error::Error::Other(format!("watch {}: {e}", addons_dir.display())))?;
    Ok(WatcherHandle { _debouncer: debouncer })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{RemoteMeta, SearchPage};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc, Mutex};

    fn ctx() -> AppCtx {
        AppCtx {
            http: crate::net::build_client("test/0"),
            cfg: std::sync::Arc::new(AppConfig::default()),
            app_version: "test",
        }
    }

    fn offline_ctx() -> AppCtx {
        let mut cfg = AppConfig::default();
        cfg.network.offline = true;
        AppCtx {
            http: crate::net::build_client("test/0"),
            cfg: std::sync::Arc::new(cfg),
            app_version: "test",
        }
    }

    #[tokio::test]
    async fn offline_scan_makes_zero_provider_calls() {
        // AF-01 regression: with offline mode on, a scan keeps the local
        // inventory and touches no provider at all.
        let (_tmp, addons) = fixture();
        let folders = scan_folders(&addons, Flavor::Forever).unwrap();
        let n = folders.len();
        let calls = Arc::new(AtomicUsize::new(0));
        let mock = MockProvider {
            kind: ProviderKind::Curse,
            claims: vec!["Alpha".into()],
            requests: Arc::new(Mutex::new(vec![])),
            calls: calls.clone(),
        };
        let report = scan_with_providers(
            &offline_ctx(),
            &[&mock],
            folders,
            &sel(),
            &mut |_, _, _| {},
        )
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0, "offline must not call providers");
        assert!(report.matched.is_empty());
        assert_eq!(report.unmatched.len(), n, "folders stay as local inventory");
    }

    #[test]
    fn ensure_online_gate() {
        assert!(ctx().ensure_online().is_ok());
        assert!(offline_ctx().ensure_online().is_err());
    }

    fn sel() -> FileSelect {
        FileSelect {
            flavor: Flavor::Forever,
            interface: 16001,
            version: "1.60.1".into(),
            channel: crate::types::Channel::Stable,
        }
    }

    fn write_addon(root: &Path, folder: &str, title: &str, version: &str) {
        let dir = root.join(folder);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{folder}.toc")),
            format!("## Title: {title}\n## Version: {version}\n## Interface: 16001\n{folder}.lua\n"),
        )
        .unwrap();
        std::fs::write(dir.join(format!("{folder}.lua")), format!("-- {folder}\n")).unwrap();
    }

    /// A missing AddOns folder (flavor not installed) must fail fast: the GUI
    /// depends on scan completion to leave the "Scanning..." state.
    #[test]
    fn scan_missing_dir_errors_fast_never_hangs() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let c = ctx();
            let missing = std::path::Path::new("/nonexistent-addon-dir-xyz/AddOns");
            let res = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                scan_addons(&c, missing, &sel(), &mut |_, _, _| {}),
            )
            .await;
            assert!(res.as_ref().is_ok_and(|r| r.is_err()), "missing dir must error, not hang: {res:?}");
        });
    }

    /// An empty folder succeeds with an empty report (switching to an
    /// installed-but-empty client must rescan cleanly).
    #[test]
    fn scan_empty_dir_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let c = ctx();
            let rep = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                scan_addons(&c, tmp.path(), &sel(), &mut |_, _, _| {}),
            )
            .await
            .expect("empty scan must not hang")
            .expect("empty dir must scan ok");
            assert!(rep.matched.is_empty() && rep.unmatched.is_empty());
        });
    }

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let addons = tmp.path().join("AddOns");
        std::fs::create_dir_all(&addons).unwrap();
        write_addon(&addons, "Alpha", "Alpha Addon", "1.0.0");
        // two-folder bundle sharing a lib folder
        write_addon(&addons, "Bundle", "Bundle", "2.0");
        write_addon(&addons, "Bundle_Libs", "Bundle Libs", "2.0");
        // junk: no TOC at all
        std::fs::create_dir_all(addons.join("Junk")).unwrap();
        std::fs::write(addons.join("Junk").join("plain.txt"), b"hi").unwrap();
        // hidden dir, ignored
        std::fs::create_dir_all(addons.join(".dotdir")).unwrap();
        std::fs::write(addons.join(".dotdir").join("x.toc"), b"## Title: Nope\n").unwrap();
        (tmp, addons)
    }

    #[test]
    fn manual_pin_survives_provider_redetection_and_restart() {
        // AF-12: a user-pinned source (folder-based) wins over fresh provider
        // detection, keeps the installed file identity, and has exactly one
        // owner row; the pin and identity persist through the ini.
        let mut cfg = AppConfig::default();
        cfg.addons.insert(
            "github:BigWigsMods/BigWigs".into(),
            AddonOverride {
                provider: Some(ProviderKind::GitHub),
                external_id: Some("BigWigsMods/BigWigs".into()),
                name: Some("BigWigs".into()),
                folders: vec!["BigWigs".into()],
                pinned: true,
                installed_file_id: Some("rel-9".into()),
                ..Default::default()
            },
        );
        let report = ScanReport {
            matched: vec![ScanMatch {
                provider: ProviderKind::Curse,
                external_id: "2382".into(),
                folders: vec!["BigWigs".into()],
                meta: RemoteMeta {
                    provider: ProviderKind::Curse,
                    external_id: "2382".into(),
                    name: "BigWigs (Curse)".into(),
                    author: None,
                    site_url: None,
                    repo_url: None,
                    description: None,
                    icon_url: None,
                    downloads: None,
                    updated_at: None,
                    files: vec![],
                },
                installed_version: Some("1.0".into()),
            }],
            unmatched: vec![],
            scanned_at: 1,
            folder_sizes: Default::default(),
        };
        let mut store = StateStore::default();
        apply_scan_report(&mut store, &report, &cfg);

        let st = store.get(ProviderKind::GitHub, "BigWigsMods/BigWigs").expect("pin survives redetection");
        assert!(st.manual);
        assert_eq!(st.installed_file_id.as_deref(), Some("rel-9"), "file id restored from ini");
        assert_eq!(st.provider, ProviderKind::GitHub, "pin wins over detection");
        assert!(store.get(ProviderKind::Curse, "2382").is_none(), "exactly one owner row");

        let mut cfg2 = AppConfig::default();
        store.write_overrides(&mut cfg2);
        let ov = &cfg2.addons["github:BigWigsMods/BigWigs"];
        assert!(ov.pinned, "pin persists");
        assert_eq!(ov.installed_file_id.as_deref(), Some("rel-9"), "file id persists");
    }

    #[test]
    fn scan_folders_fixture() {
        let (_tmp, addons) = fixture();
        let infos = scan_folders(&addons, Flavor::Forever).unwrap();
        let names: Vec<&str> = infos.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["Alpha", "Bundle", "Bundle_Libs", "Junk"]);

        let alpha = infos.iter().find(|f| f.name == "Alpha").unwrap();
        assert_eq!(alpha.toc.title.as_deref(), Some("Alpha Addon"));
        assert_eq!(alpha.toc.version.as_deref(), Some("1.0.0"));
        assert_eq!(alpha.toc.interface, vec![16001]);
        assert!(alpha.curse_fingerprint.is_some(), "toc closure must fingerprint");

        let junk = infos.iter().find(|f| f.name == "Junk").unwrap();
        assert_eq!(junk.toc, ParsedToc::default());
        assert!(junk.curse_fingerprint.is_none());
    }

    /// Mock provider: claims a fixed folder list, records each scan request.
    struct MockProvider {
        kind: ProviderKind,
        claims: Vec<String>,
        requests: Arc<Mutex<Vec<Vec<String>>>>,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Provider for MockProvider {
        fn kind(&self) -> ProviderKind {
            self.kind
        }
        async fn meta(&self, _c: &AppCtx, _ids: &[String]) -> Result<Vec<RemoteMeta>> {
            Ok(vec![])
        }
        async fn scan(&self, _c: &AppCtx, req: &ScanRequest<'_>) -> Result<Vec<ScanMatch>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let seen: Vec<String> = req.folders.iter().map(|f| f.name.clone()).collect();
            self.requests.lock().unwrap().push(seen.clone());
            let mine: Vec<String> =
                self.claims.iter().filter(|c| seen.iter().any(|s| s.eq_ignore_ascii_case(c))).cloned().collect();
            if mine.is_empty() {
                return Ok(vec![]);
            }
            Ok(vec![ScanMatch {
                provider: self.kind,
                external_id: "42".into(),
                folders: mine,
                meta: RemoteMeta {
                    provider: self.kind,
                    external_id: "42".into(),
                    name: "Mock".into(),
                    author: None,
                    site_url: None,
                    repo_url: None,
                    description: None,
                    icon_url: None,
                    downloads: None,
                    updated_at: None,
                    files: vec![],
                },
                installed_version: Some("1.0.0".into()),
            }])
        }
        async fn search(
            &self,
            _c: &AppCtx,
            _q: &str,
            _p: u32,
            _n: u32,
            _s: &FileSelect,
        ) -> Result<SearchPage> {
            Ok(SearchPage::default())
        }
    }

    #[tokio::test]
    async fn scan_with_providers_claims_folders_once() {
        let (_tmp, addons) = fixture();
        let folders = scan_folders(&addons, Flavor::Forever).unwrap();
        let reqs = Arc::new(Mutex::new(Vec::new()));

        let curse = MockProvider {
            kind: ProviderKind::Curse,
            claims: vec!["Alpha".into(), "Ghost".into()], // Ghost not on disk
            requests: reqs.clone(),
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let gh = MockProvider {
            kind: ProviderKind::GitHub,
            claims: vec!["Bundle".into(), "Bundle_Libs".into(), "Alpha".into()],
            requests: reqs.clone(),
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let ctx = ctx();
        let mut events = Vec::new();
        let report = scan_with_providers(
            &ctx,
            &[&curse, &gh],
            folders,
            &sel(),
            &mut |f, d, t| events.push((f.to_string(), d, t)),
        )
        .await
        .unwrap();

        assert_eq!(report.matched.len(), 2);
        let m = report.matched.iter().find(|m| m.provider == ProviderKind::Curse).unwrap();
        assert_eq!(m.folders, vec!["Alpha".to_string()], "ghost claims dropped");
        let g = report.matched.iter().find(|m| m.provider == ProviderKind::GitHub).unwrap();
        assert_eq!(g.folders, vec!["Bundle", "Bundle_Libs"], "claimed folder not re-offered");
        assert_eq!(report.unmatched.len(), 1);
        assert_eq!(report.unmatched[0].name, "Junk");
        // progress: after curse 1/4 resolved, after github 3/4
        assert_eq!(events, vec![("CurseForge".to_string(), 1, 4), ("GitHub".to_string(), 3, 4)]);
        // github request excluded curse's Alpha
        assert_eq!(reqs.lock().unwrap()[1], vec!["Bundle", "Bundle_Libs", "Junk"]);
    }

    #[tokio::test]
    async fn scan_survives_provider_failure() {
        let (_tmp, addons) = fixture();
        let folders = scan_folders(&addons, Flavor::Forever).unwrap();
        struct Failing;
        #[async_trait]
        impl Provider for Failing {
            fn kind(&self) -> ProviderKind {
                ProviderKind::Tukui
            }
            async fn meta(&self, _c: &AppCtx, _ids: &[String]) -> Result<Vec<RemoteMeta>> {
                Ok(vec![])
            }
            async fn scan(&self, _c: &AppCtx, _r: &ScanRequest<'_>) -> Result<Vec<ScanMatch>> {
                Err(crate::error::Error::provider("tukui", "boom"))
            }
            async fn search(
                &self,
                _c: &AppCtx,
                _q: &str,
                _p: u32,
                _n: u32,
                _s: &FileSelect,
            ) -> Result<SearchPage> {
                Ok(SearchPage::default())
            }
        }
        let ctx = ctx();
        let report = scan_with_providers(
            &ctx,
            &[&Failing],
            folders,
            &sel(),
            &mut |_, _, _| {},
        )
        .await
        .unwrap();
        assert!(report.matched.is_empty());
        assert_eq!(report.unmatched.len(), 4, "all folders fall through to unmatched");
    }

    #[test]
    fn apply_scan_report_merges_state() {
        let (_tmp, addons) = fixture();
        let folder_infos = scan_folders(&addons, Flavor::Forever).unwrap();
        let junk = folder_infos.iter().find(|f| f.name == "Junk").unwrap();
        let report = ScanReport {
            matched: vec![ScanMatch {
                provider: ProviderKind::Curse,
                external_id: "2382".into(),
                folders: vec!["Alpha".into()],
                meta: RemoteMeta {
                    provider: ProviderKind::Curse,
                    external_id: "2382".into(),
                    name: "Alpha".into(),
                    author: None,
                    site_url: Some("https://example/2382".into()),
                    repo_url: None,
                    description: None,
                    icon_url: None,
                    downloads: None,
                    updated_at: None,
                    files: vec![],
                },
                installed_version: Some("1.0.0".into()),
            }],
            unmatched: vec![junk.clone()],
            scanned_at: 1_700_000_000,
            folder_sizes: Default::default(),
        };

        let mut store = StateStore::default();
        let mut existing = AddonState {
            key: AddonState::key(ProviderKind::Curse, "2382"),
            provider: ProviderKind::Curse,
            external_id: "2382".into(),
            name: "Alpha".into(),
            site_url: None,
            repo_url: None,
            icon_url: None,
            folders: vec!["Alpha".into()],
            fingerprints: vec![7],
            installed_version: Some("0.9".into()),
            installed_file_id: Some("file-9".into()),
            size_bytes: 0,
            channel: crate::types::Channel::Beta,
            auto_update: false,
            ignored: true,
            manual: true,
            cached_remote: None,
            cached_at: None,
            last_checked: Some(123),
            provider_data: serde_json::json!({"k": 1}),
            pending_update: None,
        };
        store.upsert(existing.clone());
        // managed addon whose folder vanished from disk → must be dropped
        existing.key = AddonState::key(ProviderKind::GitHub, "o/gone");
        existing.provider = ProviderKind::GitHub;
        existing.external_id = "o/gone".into();
        existing.folders = vec!["Gone".into()];
        store.upsert(existing);
        // a state with no folders on disk (installed-by-url) → kept
        let mut url_only = store.addons.values().next().unwrap().clone();
        url_only.key = AddonState::key(ProviderKind::Zip, "https://example.com/x.zip");
        url_only.provider = ProviderKind::Zip;
        url_only.external_id = "https://example.com/x.zip".into();
        url_only.folders = vec![];
        store.upsert(url_only);

        let mut cfg = AppConfig::default();
        cfg.updates.auto_update = true;
        // pin folders + auto_update for the curse addon
        cfg.addons.insert(
            "curse:2382".into(),
            AddonOverride { folders: vec!["Alpha".into()], auto_update: Some(false), ..Default::default() },
        );

        apply_scan_report(&mut store, &report, &cfg);

        let alpha = store.get(ProviderKind::Curse, "2382").unwrap();
        assert!(!alpha.auto_update, "existing/user-override auto_update preserved");
        assert!(alpha.ignored, "user setting preserved");
        assert_eq!(alpha.channel, crate::types::Channel::Beta, "user channel preserved");
        assert!(alpha.manual);
        assert_eq!(alpha.installed_file_id.as_deref(), Some("file-9"), "install proof kept");
        assert_eq!(alpha.installed_version.as_deref(), Some("1.0.0"), "TOC version wins");
        assert_eq!(alpha.fingerprints, vec![7]);
        assert_eq!(alpha.last_checked, Some(123));
        assert!(alpha.cached_remote.is_some(), "scan meta cached");

        let junk = store.get(ProviderKind::Zip, "local:Junk").unwrap();
        assert_eq!(junk.folders, vec!["Junk".to_string()]);
        assert!(junk.manual);
        assert!(!junk.auto_update, "local pseudo-addons never auto-update");
        assert_eq!(junk.name, "Junk", "no TOC → folder name");

        assert!(store.get(ProviderKind::GitHub, "o/gone").is_none(), "gone folder dropped");
        assert!(store
            .addons
            .contains_key(&AddonState::key(ProviderKind::Zip, "https://example.com/x.zip")));
        assert_eq!(store.last_scan, Some(1_700_000_000));
        assert!(alpha.folders == vec!["Alpha".to_string()]);
    }

    #[test]
    fn apply_scan_report_defaults_for_new_addons() {
        let tmp = tempfile::tempdir().unwrap();
        let addons = tmp.path().join("AddOns");
        write_addon(&addons, "Fresh", "Fresh Addon", "3.1");
        let report = ScanReport {
            matched: vec![ScanMatch {
                provider: ProviderKind::GitHub,
                external_id: "me/fresh".into(),
                folders: vec!["Fresh".into()],
                meta: RemoteMeta {
                    provider: ProviderKind::GitHub,
                    external_id: "me/fresh".into(),
                    name: "Fresh".into(),
                    author: None,
                    site_url: None,
                    repo_url: None,
                    description: None,
                    icon_url: None,
                    downloads: None,
                    updated_at: None,
                    files: vec![],
                },
                installed_version: Some("3.1".into()),
            }],
            unmatched: vec![],
            scanned_at: 5,
            folder_sizes: Default::default(),
        };
        let mut store = StateStore::default();
        let mut cfg = AppConfig::default();
        cfg.updates.auto_update = false;
        cfg.updates.channel = crate::types::Channel::Beta;
        apply_scan_report(&mut store, &report, &cfg);
        let a = store.get(ProviderKind::GitHub, "me/fresh").unwrap();
        assert!(!a.auto_update, "new addon takes global default");
        assert_eq!(a.channel, crate::types::Channel::Beta);
        assert!(!a.manual);
        assert_eq!(a.name, "Fresh");
    }

    #[test]
    fn watcher_fires_on_new_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let addons = tmp.path().join("AddOns");
        std::fs::create_dir_all(&addons).unwrap();
        let (tx, rx) = mpsc::channel::<()>();
        let handle = spawn_watcher(addons.clone(), move || {
            let _ = tx.send(());
        })
        .unwrap();

        std::fs::create_dir(addons.join("DroppedIn")).unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(()) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    drop(handle);
                    panic!("watcher dropped without firing")
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    assert!(std::time::Instant::now() < deadline, "watcher callback never fired");
                }
            }
        }
        drop(handle); // Drop = stop, must not hang/panic
    }
}

fn next_size(sizes: &std::collections::BTreeMap<String, u64>, folders: &[String]) -> u64 {
    folders
        .iter()
        .map(|f| {
            sizes
                .get(f)
                .or_else(|| sizes.iter().find(|(k, _)| k.eq_ignore_ascii_case(f)).map(|(_, v)| v))
                .copied()
                .unwrap_or(0)
        })
        .sum()
}
