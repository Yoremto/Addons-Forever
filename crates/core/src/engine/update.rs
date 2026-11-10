//! Update checking + auto-update queue: batch metadata fetch per provider,
//! file picking, update-needed logic, sequential apply, backup pruning.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use futures::future::BoxFuture;

use crate::error::Result;
use crate::providers::Registry;
use crate::store::{AddonState, PendingUpdate, StateStore};
use crate::types::{
    AppCtx, FileSelect, ProviderKind, RemoteFile, RemoteMeta,
};
use crate::util::{version_cmp, version_is_newer, version_key};

use super::install::{self, InstallProgress};

/// An addon with a newer remote file available.
#[derive(Clone, Debug)]
pub struct UpdateInfo {
    /// Store key (`provider:external_id`).
    pub key: String,
    pub name: String,
    pub current: Option<String>,
    /// The file to install.
    pub target: RemoteFile,
    /// Version of `target`.
    pub version: String,
}

/// Result of an `apply_updates` run.
#[derive(Clone, Debug, Default)]
pub struct ApplySummary {
    pub updated: usize,
    /// `(key, new version)` per successfully updated addon.
    pub updated_keys: Vec<(String, Option<String>)>,
    /// `(key, error message)` per failed addon.
    pub failed: Vec<(String, String)>,
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Minimal meta for the install pipeline (name/provider/id; files not needed).
pub(crate) fn meta_stub_for(state: &AddonState) -> RemoteMeta {
    RemoteMeta {
        provider: state.provider,
        external_id: state.external_id.clone(),
        name: state.name.clone(),
        author: None,
        site_url: state.site_url.clone(),
        repo_url: state.repo_url.clone(),
        description: None,
        icon_url: state.icon_url.clone(),
        downloads: None,
        updated_at: None,
        files: vec![],
    }
}

/// Is `target` an update for this state?
///
/// - never installed by us (no `installed_file_id`) → yes
/// - same file id → no
/// - different file id: newer version → yes; incomparable versions → newer date
/// - different file id, not newer (and comparable) → no
pub(crate) fn update_needed(state: &AddonState, target: &RemoteFile) -> bool {
    let Some(installed) = state.installed_file_id.as_deref() else {
        // Never installed by us (freshly ingested addon): we only know the TOC
        // version. Only offer files that are strictly NEWER: an equal version
        // is up to date, and an older published version is a downgrade, not
        // an update (AF-16: a provider lagging behind the user's 2.0 install
        // must not offer 1.0).
        return state
            .installed_version
            .as_deref()
            // Identity counts as up to date: "1.60.28" matches published
            // "1.60.28-forever" (client suffix carries no ordering info), while
            // "1.0.0c" vs "1.0.0d" (letter suffixes) is a real update.
            .map(|v| version_cmp(v.trim(), target.version.trim()) == std::cmp::Ordering::Less)
            .unwrap_or(true);
    };
    if installed == target.file_id {
        return false;
    }
    let Some(current) = state.installed_version.as_deref() else {
        return true;
    };
    if version_key(&target.version).is_empty() || version_key(current).is_empty() {
        // versions incomparable: trust the publish date
        target.date.unwrap_or(0) > 0
    } else {
        version_is_newer(&target.version, current)
    }
}

/// Result of a check: the updates found plus which provider sources failed,
/// so the caller never presents cached/failing data as a fresh complete check
/// (AF-18).
#[derive(Clone, Debug, Default)]
pub struct CheckOutcome {
    pub updates: Vec<UpdateInfo>,
    /// (provider label, error message) for sources that could not be reached
    pub failed: Vec<(String, String)>,
    /// true when the check ran in offline mode (cache only, never "up to date")
    pub offline: bool,
}

impl CheckOutcome {
    pub fn is_clean(&self) -> bool {
        !self.offline && self.failed.is_empty()
    }
}

/// Check for updates with the provider map injected (testable core of
/// [`check_updates`]).
pub(crate) async fn check_updates_inner(
    ctx: &AppCtx,
    store: &mut StateStore,
    providers: &BTreeMap<ProviderKind, &dyn crate::types::Provider>,
    sel: &FileSelect,
    on_progress: &mut (dyn FnMut(&str, usize, usize) + Send),
) -> Result<CheckOutcome> {
    let now = now_unix();
    let offline = ctx.cfg.network.offline;
    let mut failed: Vec<(String, String)> = Vec::new();

    // group managed addons by provider
    let mut groups: BTreeMap<ProviderKind, Vec<String>> = BTreeMap::new();
    for st in store.addons.values() {
        if st.provider == ProviderKind::Zip && st.external_id.starts_with("local:") {
            continue; // local pseudo-addons have no remote
        }
        if st.ignored {
            continue;
        }
        groups.entry(st.provider).or_default().push(st.key.clone());
    }
    let total: usize = groups.values().map(|v| v.len()).sum();
    let mut done = 0usize;
    let mut updates = Vec::new();

    for (kind, keys) in groups {
        let Some(provider) = providers.get(&kind).copied() else {
            done += keys.len();
            continue;
        };
        // one batched meta() per provider; offline → cache only
        let mut fresh = true;
        let metas: Vec<RemoteMeta> = if offline {
            fresh = false;
            keys.iter().filter_map(|k| store.addons.get(k)?.cached_remote.clone()).collect()
        } else {
            let ids: Vec<String> =
                keys.iter().filter_map(|k| store.addons.get(k).map(|s| s.external_id.clone())).collect();
            match provider.meta(ctx, &ids).await {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(provider = kind.as_str(), "update check meta failed: {e}");
                    failed.push((kind.label().to_string(), e.to_string()));
                    fresh = false;
                    keys.iter().filter_map(|k| store.addons.get(k)?.cached_remote.clone()).collect()
                }
            }
        };
        let mut by_id: BTreeMap<String, RemoteMeta> =
            metas.into_iter().map(|m| (m.external_id.clone(), m)).collect();

        for key in keys {
            let label = store.addons.get(&key).map(|s| s.name.clone()).unwrap_or_else(|| key.clone());
            done += 1;
            on_progress(&label, done, total);
            let Some(state) = store.addons.get_mut(&key) else { continue };
            let Some(meta) = by_id.remove(&state.external_id).or_else(|| state.cached_remote.clone())
            else {
                continue; // provider doesn't know it and we have no cache
            };
            state.cached_remote = Some(meta.clone());
            // AF-18: cache-sourced metadata keeps its original fetch time;
            // only a fresh remote answer stamps cached_at
            if fresh {
                state.cached_at = Some(now);
            }
            state.last_checked = Some(now);

            let sel = FileSelect { channel: state.channel, ..sel.clone() };
            let Some(target) = provider.pick(&meta, &sel) else {
                state.pending_update = None;
                continue;
            };
            if update_needed(state, &target) {
                state.pending_update =
                    Some(PendingUpdate { file_id: target.file_id.clone(), version: target.version.clone() });
                updates.push(UpdateInfo {
                    key: key.clone(),
                    name: state.name.clone(),
                    current: state.installed_version.clone(),
                    version: target.version.clone(),
                    target,
                });
            } else {
                state.pending_update = None;
            }
        }
    }

    store.last_check = Some(now);
    Ok(CheckOutcome { updates, failed, offline })
}

/// Check every managed addon (batched `meta()` per provider, respecting
/// `network.offline`), cache the metadata, flag `pending_update`, and return
/// the list of available updates.
pub async fn check_updates(
    ctx: &AppCtx,
    store: &mut StateStore,
    reg: &Registry,
    sel: &FileSelect,
    on_progress: &mut (dyn FnMut(&str, usize, usize) + Send),
) -> Result<CheckOutcome> {
    let providers: BTreeMap<ProviderKind, &dyn crate::types::Provider> = [
        ProviderKind::Curse,
        ProviderKind::GitHub,
        ProviderKind::Tukui,
        ProviderKind::WowInterface,
        ProviderKind::WowUpHub,
        ProviderKind::Zip,
    ]
    .into_iter()
    .map(|k| (k, reg.get(k)))
    .collect();
    check_updates_inner(ctx, store, &providers, sel, on_progress).await
}

/// Apply updates sequentially (concurrency 1: no zip storms). `fetch` supplies
/// the downloaded archive bytes for an addon (network in production, in-memory
/// in tests); on success the store state is updated, on failure the addon is
/// recorded and the run continues.
#[allow(clippy::type_complexity)] // the progress sink needs the inline dyn type:
// a type alias with a named lifetime erases the higher-ranked FnMut(InstallProgress).
pub(crate) async fn apply_updates_with(
    ctx: &AppCtx,
    store: &mut StateStore,
    infos: &[UpdateInfo],
    addons_dir: &Path,
    on_progress: &mut (dyn FnMut(&str, &InstallProgress) + Send),
    fetch: &mut dyn FnMut(
        &AddonState,
        &UpdateInfo,
        Box<dyn FnMut(InstallProgress) + Send>,
    ) -> BoxFuture<'static, Result<Vec<u8>>>,
) -> Result<ApplySummary> {
    let mut summary = ApplySummary::default();
    // ownership snapshot for AF-06/AF-08, taken before any mutation
    let owned = store.folder_owners();
    for info in infos {
        let Some(state) = store.addons.get(&info.key) else {
            summary.failed.push((info.key.clone(), "not found in store".into()));
            continue;
        };
        let meta = meta_stub_for(state);
        let key = info.key.clone();
        let bytes = {
            // progress events flow through a channel; the fetch future is polled
            // with a short timeout so events can be forwarded while downloading.
            let (tx, rx) = std::sync::mpsc::channel::<InstallProgress>();
            let fut = fetch(state, info, Box::new(move |p| {
                let _ = tx.send(p);
            }));
            tokio::pin!(fut);
            let result = loop {
                match tokio::time::timeout(std::time::Duration::from_millis(100), &mut fut).await {
                    Ok(r) => break r,
                    Err(_elapsed) => {
                        while let Ok(p) = rx.try_recv() {
                            on_progress(&key, &p);
                        }
                    }
                }
            };
            while let Ok(p) = rx.try_recv() {
                on_progress(&key, &p);
            }
            match result {
                Ok(b) => b,
                Err(e) => {
                    summary.failed.push((info.key.clone(), format!("download: {e}")));
                    continue;
                }
            }
        };
        let mut progress = |p: InstallProgress| on_progress(&info.key, &p);
        match install::install_from_bytes(ctx, addons_dir, &meta, &info.target, bytes, &owned, &info.key, &mut progress).await {
            Ok(outcome) => {
                let old_folders = store
                    .addons
                    .get(&info.key)
                    .map(|s| s.folders.clone())
                    .unwrap_or_default();
                if let Some(state) = store.addons.get_mut(&info.key) {
                    state.installed_file_id = Some(info.target.file_id.clone());
                    state.installed_version =
                        outcome.version.clone().or_else(|| Some(info.version.clone()));
                    state.folders = outcome.folders.clone();
                    state.pending_update = None;
                }
                // AF-08: folders the update dropped are removed (ownership map
                // is the pre-update one, so foreign folders are left alone)
                for r in install::cleanup_dropped_roots(addons_dir, &old_folders, &outcome.folders, &owned, &info.key) {
                    tracing::info!("removed folder {r}, dropped by the update of {}", info.key);
                }
                summary.updated += 1;
                summary.updated_keys.push((info.key.clone(), outcome.version.clone()));
            }
            Err(e) => summary.failed.push((info.key.clone(), e.to_string())),
        }
    }
    Ok(summary)
}

/// Download + install every listed update.
pub async fn apply_updates(
    ctx: &AppCtx,
    store: &mut StateStore,
    _reg: &Registry,
    _sel: &FileSelect,
    infos: &[UpdateInfo],
    addons_dir: &Path,
    on_progress: &mut (dyn FnMut(&str, &InstallProgress) + Send),
) -> Result<ApplySummary> {
    let mut fetch = |state: &AddonState,
                      info: &UpdateInfo,
                      mut prog: Box<dyn FnMut(InstallProgress) + Send>|
     -> BoxFuture<'static, Result<Vec<u8>>> {
        let ctx = ctx.clone();
        let meta = meta_stub_for(state);
        let file = info.target.clone();
        let dir = addons_dir.to_path_buf();
        // route download progress through the caller's per-addon callback
        Box::pin(async move {
            install::download_zip(&ctx, &dir, &meta, &file, &mut move |b, t| {
                prog(InstallProgress::Downloading { bytes: b, total: t });
            })
            .await
        })
    };
    apply_updates_with(ctx, store, infos, addons_dir, on_progress, &mut fetch).await
}

/// The subset of available updates the user allowed to auto-update.
pub fn auto_update_targets(store: &StateStore, updates: &[UpdateInfo]) -> Vec<UpdateInfo> {
    updates
        .iter()
        .filter(|u| store.addons.get(&u.key).is_some_and(|s| s.auto_update && !s.ignored))
        .cloned()
        .collect()
}

/// Scheduler helper: how long until the next auto-check is due.
/// `None` last-check → due immediately.
pub fn seconds_until_next_check(last: Option<i64>, interval_min: u32) -> Duration {
    let now = now_unix();
    let due = match last {
        None => return Duration::ZERO,
        // clock skew (last-check in the future): check now
        Some(l) if l > now => return Duration::ZERO,
        Some(l) => l.saturating_add(i64::from(interval_min) * 60),
    };
    if due <= now {
        Duration::ZERO
    } else {
        Duration::from_secs((due - now) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::error::Error;
    use crate::types::{Channel, Provider, ScanMatch, ScanRequest, SearchPage};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn ctx() -> AppCtx {
        AppCtx {
            http: crate::net::build_client("test/0"),
            cfg: std::sync::Arc::new(AppConfig::default()),
            app_version: "test",
        }
    }

    fn sel() -> FileSelect {
        FileSelect {
            flavor: crate::flavor::Flavor::Forever,
            interface: 16001,
            version: "1.60.1".into(),
            channel: Channel::Stable,
        }
    }

    fn state(provider: ProviderKind, id: &str, folders: &[&str]) -> AddonState {
        AddonState {
            key: AddonState::key(provider, id),
            provider,
            external_id: id.into(),
            name: format!("addon {id}"),
            site_url: None,
            repo_url: None,
            icon_url: None,
            folders: folders.iter().map(|f| f.to_string()).collect(),
            fingerprints: vec![],
            installed_version: Some("1.0.0".into()),
            installed_file_id: Some("old-file".into()),
            size_bytes: 0,
            channel: Channel::Stable,
            auto_update: true,
            ignored: false,
            manual: false,
            cached_remote: None,
            cached_at: None,
            last_checked: None,
            provider_data: serde_json::Value::Null,
            pending_update: None,
        }
    }

    fn file(id: &str, version: &str, date: i64) -> RemoteFile {
        RemoteFile {
            file_id: id.into(),
            version: version.into(),
            channel: Channel::Stable,
            file_name: format!("x-{id}.zip"),
            url: format!("https://example.invalid/{id}.zip"),
            headers: Default::default(),
            flavors: vec![],
            flavor_meta: false,
            date: Some(date),
            size: None,
            md5: None,
            sha1: None,
            changelog_url: None,
        }
    }

    fn meta_with(provider: ProviderKind, id: &str, files: Vec<RemoteFile>) -> RemoteMeta {
        RemoteMeta {
            provider,
            external_id: id.into(),
            name: format!("meta {id}"),
            author: None,
            site_url: None,
            repo_url: None,
            description: None,
            icon_url: None,
            downloads: None,
            updated_at: None,
            files,
        }
    }

    /// Mock provider serving fixed metas, counting meta() calls.
    struct MetaProvider {
        kind: ProviderKind,
        metas: Vec<RemoteMeta>,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Provider for MetaProvider {
        fn kind(&self) -> ProviderKind {
            self.kind
        }
        async fn meta(&self, _c: &AppCtx, _ids: &[String]) -> Result<Vec<RemoteMeta>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.metas.clone())
        }
        async fn scan(&self, _c: &AppCtx, _r: &ScanRequest<'_>) -> Result<Vec<ScanMatch>> {
            Ok(vec![])
        }
        async fn search(
            &self,
            _c: &AppCtx,
            _q: &str,
            _p: u32,
            _n: u32,
            _s: &FileSelect,
        ) -> Result<SearchPage> {
            Err(Error::Unsupported("n/a".into()))
        }
    }

    #[tokio::test]
    async fn apply_updates_forwards_download_progress() {
        use std::io::Write as _;
        use zip::write::SimpleFileOptions;
        use zip::ZipWriter;

        fn build_zip(name: &str, version: &str) -> Vec<u8> {
            let mut z = ZipWriter::new(std::io::Cursor::new(Vec::new()));
            let opts = SimpleFileOptions::default();
            z.start_file(format!("{name}/{name}.toc"), opts).unwrap();
            z.write_all(format!("## Title: {name}\n## Version: {version}\n## Interface: 16001\n").as_bytes())
                .unwrap();
            z.finish().unwrap().into_inner()
        }

        let tmp = tempfile::tempdir().unwrap();
        let addons = tmp.path().join("AddOns");
        std::fs::create_dir_all(&addons).unwrap();

        let mut store = StateStore::default();
        store.upsert(state(ProviderKind::Curse, "1", &["Test"]));
        let infos = vec![UpdateInfo {
            key: "curse:1".into(),
            name: "Test".into(),
            current: Some("1.0.0".into()),
            target: file("f2", "1.2.0", 100),
            version: "1.2.0".into(),
        }];

        let zip_bytes = build_zip("Test", "1.2.0");
        // fetch forwards two download-progress events, then returns the zip
        let mut fetch = move |_: &AddonState,
                             _: &UpdateInfo,
                             mut prog: Box<dyn FnMut(InstallProgress) + Send>|
         -> BoxFuture<'static, Result<Vec<u8>>> {
            let bytes = zip_bytes.clone();
            Box::pin(async move {
                prog(InstallProgress::Downloading { bytes: 0, total: Some(100) });
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                prog(InstallProgress::Downloading { bytes: 100, total: Some(100) });
                Ok(bytes)
            })
        };

        let seen = Arc::new(std::sync::Mutex::new(Vec::<u64>::new()));
        let seen2 = seen.clone();
        let mut on_progress = move |key: &str, p: &InstallProgress| {
            if let InstallProgress::Downloading { bytes, .. } = p {
                assert_eq!(key, "curse:1");
                seen2.lock().unwrap().push(*bytes);
            }
        };

        let ctx = ctx();
        let summary =
            apply_updates_with(&ctx, &mut store, &infos, &addons, &mut on_progress, &mut fetch)
                .await
                .unwrap();
        assert_eq!(summary.updated, 1);
        let seen = seen.lock().unwrap();
        assert!(seen.contains(&0), "start event forwarded: {seen:?}");
        assert!(seen.contains(&100), "final event forwarded: {seen:?}");
    }

    #[test]
    fn update_needed_ignores_flavor_suffix_versions() {
        let mut st = state(ProviderKind::Curse, "7", &["Leatrix_Maps"]);
        st.installed_version = Some("1.60.28".into());
        // freshly ingested: no installed_file_id
        let mut f = file("f7", "1.60.28-forever", 10);
        assert!(!update_needed(&st, &f), "1.60.28 vs 1.60.28-forever is the same version");
        f.version = "1.60.29-forever".into();
        assert!(update_needed(&st, &f), "1.60.29-forever is newer than 1.60.28");
        // letter-suffix versions are real updates
        st.installed_version = Some("1.0.0c".into());
        f.version = "1.0.0d".into();
        assert!(update_needed(&st, &f), "1.0.0d is newer than 1.0.0c");
        f.version = "1.0.0c".into();
        assert!(!update_needed(&st, &f), "1.0.0c is the same as 1.0.0c");
    }

    #[test]
    fn update_needed_matrix() {
        let mut st = state(ProviderKind::Curse, "1", &["A"]);
        // never installed by us → update
        st.installed_file_id = None;
        assert!(update_needed(&st, &file("f2", "1.0.1", 5)), "no install proof → update");
        assert!(!update_needed(&st, &file("f2", "1.0.0", 5)), "ingested addon, same version → up to date");
        // AF-16: provider lagging behind the user's install is not an update
        assert!(!update_needed(&st, &file("f0", "0.9.0", 5)), "older version → no update (no install proof)");
        st.installed_file_id = Some("old-file".into());
        // same file id → no update
        assert!(!update_needed(&st, &file("old-file", "9.9.9", 9)), "same file id → up to date");
        // different id, newer version → update
        assert!(update_needed(&st, &file("f2", "1.0.1", 5)), "newer version → update");
        // different id, older/equal version → no update
        assert!(!update_needed(&st, &file("f2", "1.0.0", 5)), "equal version → no update");
        assert!(!update_needed(&st, &file("f0", "0.9.0", 5)), "older version → no update");
        // incomparable versions: fall back to date
        assert!(update_needed(&st, &file("f2", "nightly", 100)), "incomparable + dated → update");
        assert!(!update_needed(&st, &file("f2", "nightly", 0)), "incomparable + undated → no update");
        st.installed_version = None;
        assert!(update_needed(&st, &file("f2", "1.0.0", 5)), "version unknown + new file → update");
        st.installed_version = Some("1.0.0".into());
        assert!(update_needed(&st, &file("f2", "1.1", 5)));
        assert!(!update_needed(&st, &file("old-file", "1.1", 5)), "id match wins over version");
    }

    fn offline_cfg() -> std::sync::Arc<AppConfig> {
        let mut cfg = AppConfig::default();
        cfg.network.offline = true;
        Arc::new(cfg)
    }

    #[tokio::test]
    async fn check_updates_flow_with_mock_provider() {
        let mut store = StateStore::default();
        // curse addon: newer file available
        store.upsert(state(ProviderKind::Curse, "2382", &["BigWigs"]));
        // github addon: installed file is the picked one
        let mut gh = state(ProviderKind::GitHub, "o/r", &["Lib"]);
        gh.installed_file_id = Some("new".into());
        gh.installed_version = Some("2.0".into());
        store.upsert(gh);
        // ignored addon → never checked
        let mut ign = state(ProviderKind::Tukui, "aura", &["Aura"]);
        ign.ignored = true;
        store.upsert(ign);
        // local zip pseudo-addon → skipped
        let mut local = state(ProviderKind::Zip, "local:Junk", &["Junk"]);
        local.installed_file_id = None;
        store.upsert(local);

        let calls = Arc::new(AtomicUsize::new(0));
        let curse = MetaProvider {
            kind: ProviderKind::Curse,
            metas: vec![meta_with(
                ProviderKind::Curse,
                "2382",
                vec![file("new", "1.2.0", 100), file("old-file", "1.0.0", 50)],
            )],
            calls: calls.clone(),
        };
        let gh = MetaProvider {
            kind: ProviderKind::GitHub,
            metas: vec![meta_with(ProviderKind::GitHub, "o/r", vec![file("new", "2.0", 10)])],
            calls: calls.clone(),
        };
        let providers: BTreeMap<ProviderKind, &dyn Provider> =
            [(ProviderKind::Curse, &curse as &dyn Provider), (ProviderKind::GitHub, &gh as &dyn Provider)]
                .into_iter()
                .collect();

        let app = ctx();
        let mut progress = Vec::new();
        let updates = check_updates_inner(
            &app,
            &mut store,
            &providers,
            &sel(),
            &mut |n, d, t| progress.push((n.to_string(), d, t)),
        )
        .await
        .unwrap();
        assert!(updates.is_clean() && updates.failed.is_empty());
        let updates = updates.updates;

        assert_eq!(updates.len(), 1, "only the outdated addon: {updates:?}");
        let u = &updates[0];
        assert_eq!(u.key, "curse:2382");
        assert_eq!(u.version, "1.2.0");
        assert_eq!(u.current.as_deref(), Some("1.0.0"));
        assert_eq!(u.target.file_id, "new");
        let s = store.get(ProviderKind::Curse, "2382").unwrap();
        assert_eq!(s.pending_update.as_ref().unwrap().file_id, "new");
        assert!(s.cached_remote.is_some(), "meta cached");
        assert!(s.cached_at.is_some() && s.last_checked.is_some());
        assert!(store.get(ProviderKind::GitHub, "o/r").unwrap().pending_update.is_none());
        assert!(store.get(ProviderKind::Zip, "local:Junk").unwrap().last_checked.is_none(), "local skipped");
        assert!(store.get(ProviderKind::Tukui, "aura").unwrap().last_checked.is_none(), "ignored skipped");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "one meta() per provider with work");
        assert!(store.last_check.is_some());
        assert_eq!(progress.len(), 2);
        assert_eq!(progress[1].2, 2, "total counts checked addons only");

        // offline: cached metas drive the result, zero meta() calls
        let mut app_off = ctx();
        app_off.cfg = offline_cfg();
        let calls2 = Arc::new(AtomicUsize::new(0));
        let curse2 = MetaProvider { kind: ProviderKind::Curse, metas: vec![], calls: calls2.clone() };
        let gh2 = MetaProvider { kind: ProviderKind::GitHub, metas: vec![], calls: calls2.clone() };
        let providers2: BTreeMap<ProviderKind, &dyn Provider> =
            [(ProviderKind::Curse, &curse2 as &dyn Provider), (ProviderKind::GitHub, &gh2 as &dyn Provider)]
                .into_iter()
                .collect();
        // simulate: reset the curse state to outdated so the cache flags it again
        store.get_mut(ProviderKind::Curse, "2382").unwrap().installed_file_id = Some("old-file".into());
        let outcome_off =
            check_updates_inner(&app_off, &mut store, &providers2, &sel(), &mut |_, _, _| {}).await.unwrap();
        assert!(outcome_off.offline && !outcome_off.is_clean(), "offline is never a clean check");
        assert_eq!(outcome_off.updates.len(), 1);
        assert_eq!(outcome_off.updates[0].target.file_id, "new", "offline uses cached_remote");
        assert_eq!(calls2.load(Ordering::SeqCst), 0, "offline makes no network meta calls");
    }

    #[test]
    fn auto_update_targets_filters() {
        let mut store = StateStore::default();
        let on = state(ProviderKind::Curse, "on", &["A"]);
        let off = {
            let mut s = state(ProviderKind::Curse, "off", &["B"]);
            s.auto_update = false;
            s
        };
        let ign = {
            let mut s = state(ProviderKind::Curse, "ign", &["C"]);
            s.ignored = true;
            s.auto_update = true;
            s
        };
        for s in [on, off, ign] {
            store.upsert(s);
        }
        let upd = |key: &str| UpdateInfo {
            key: key.into(),
            name: "n".into(),
            current: None,
            target: file("f", "1.1", 1),
            version: "1.1".into(),
        };
        let all = vec![upd("curse:on"), upd("curse:off"), upd("curse:ign"), upd("curse:missing")];
        let targets = auto_update_targets(&store, &all);
        assert_eq!(targets.iter().map(|u| u.key.as_str()).collect::<Vec<_>>(), ["curse:on"]);
    }

    #[test]
    fn seconds_until_next_check_math() {
        assert_eq!(seconds_until_next_check(None, 360), Duration::ZERO, "never checked → now");
        let now = chrono::Utc::now().timestamp();
        assert_eq!(seconds_until_next_check(Some(now - 10_000), 60), Duration::ZERO, "overdue");
        let d = seconds_until_next_check(Some(now), 60);
        assert!(d > Duration::from_secs(3540) && d <= Duration::from_secs(3600), "{d:?}");
        assert_eq!(seconds_until_next_check(Some(now + 1000), 1), Duration::ZERO, "clock skew → now");
    }

    #[tokio::test]
    async fn apply_updates_injects_downloads_and_updates_state() {
        use std::io::Write as _;
        use zip::write::SimpleFileOptions;
        use zip::ZipWriter;

        fn build_zip(name: &str, version: &str) -> Vec<u8> {
            let mut z = ZipWriter::new(std::io::Cursor::new(Vec::new()));
            let opts = SimpleFileOptions::default();
            z.start_file(format!("{name}/{name}.toc"), opts).unwrap();
            z.write_all(format!("## Title: {name}\n## Version: {version}\n## Interface: 16001\n").as_bytes())
                .unwrap();
            z.finish().unwrap().into_inner()
        }

        let tmp = tempfile::tempdir().unwrap();
        let addons = tmp.path().join("AddOns");
        std::fs::create_dir_all(&addons).unwrap();
        std::fs::create_dir_all(addons.join("Test")).unwrap();
        std::fs::write(addons.join("Test").join("Test.toc"), b"## Version: 1.0.0\n").unwrap();

        let mut store = StateStore::default();
        store.upsert(state(ProviderKind::Curse, "1", &["Test"]));

        let infos = vec![
            UpdateInfo {
                key: "curse:1".into(),
                name: "Test".into(),
                current: Some("1.0.0".into()),
                target: file("f2", "1.2.0", 100),
                version: "1.2.0".into(),
            },
            UpdateInfo {
                key: "github:missing".into(),
                name: "ghost".into(),
                current: None,
                target: file("fx", "9", 1),
                version: "9".into(),
            },
        ];

        let zip_bytes = build_zip("Test", "1.2.0");
        let mut fetch = |_: &AddonState,
                         _: &UpdateInfo,
                         _: Box<dyn FnMut(InstallProgress) + Send>|
         -> BoxFuture<'static, Result<Vec<u8>>> {
            let bytes = zip_bytes.clone();
            Box::pin(async move { Ok(bytes) })
        };
        let ctx = ctx();
        let summary = apply_updates_with(
            &ctx,
            &mut store,
            &infos,
            &addons,
            &mut |_, _| {},
            &mut fetch,
        )
        .await
        .unwrap();

        assert_eq!(summary.updated, 1);
        assert_eq!(summary.failed.len(), 1, "the ghost key failed");
        assert_eq!(summary.failed[0].0, "github:missing");
        let toc = std::fs::read_to_string(addons.join("Test").join("Test.toc")).unwrap();
        assert!(toc.contains("1.2.0"), "new version installed: {toc}");
        let st = store.get(ProviderKind::Curse, "1").unwrap();
        assert_eq!(st.installed_file_id.as_deref(), Some("f2"));
        assert_eq!(st.installed_version.as_deref(), Some("1.2.0"));
        assert_eq!(st.folders, vec!["Test".to_string()]);
        assert!(st.pending_update.is_none());
        // backups pruned to keep=3 (default keep_backups=true): at most 3 snapshots
        let backup_root = install::fa_tmp_dir(&addons).join("backup");
        if backup_root.is_dir() {
            assert!(std::fs::read_dir(&backup_root).unwrap().count() <= 3);
        }
    }
}
