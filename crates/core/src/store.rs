//! In-memory addon state. The single source of truth for what is installed
//! during a session. Nothing is written to disk: user intent (auto-update,
//! channel, ignored, source pinning) lives in `[addon ...]` sections of
//! `addons-forever.ini`; facts like installed versions are re-derived by the
//! fingerprint scan at startup. Legacy files from pre-0.2.7 layouts are folded
//! into the ini once by [`StateStore::migrate_into_cfg`] and deleted.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::types::{ProviderKind, RemoteMeta};
use crate::types::Channel;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StateStore {
    pub schema_version: u32,
    /// Key: `provider:external_id` (stable across scans).
    pub addons: BTreeMap<String, AddonState>,
    pub last_scan: Option<i64>,
    /// Free-form: "installed manually" addons from zip URLs the user added.
    pub last_check: Option<i64>,
}

/// One managed addon.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AddonState {
    pub key: String,
    pub provider: ProviderKind,
    pub external_id: String,
    pub name: String,
    pub site_url: Option<String>,
    pub repo_url: Option<String>,
    pub icon_url: Option<String>,
    /// Folder names inside AddOns owned by this addon.
    pub folders: Vec<String>,
    /// Curse fingerprints of those folders (for re-matching; also the ingest proof).
    pub fingerprints: Vec<u32>,
    pub installed_version: Option<String>,
    /// Size of the installed folders on disk, bytes (0 = unknown).
    #[serde(default)]
    pub size_bytes: u64,
    /// file_id of the file we last installed.
    pub installed_file_id: Option<String>,
    pub channel: Channel,
    pub auto_update: bool,
    pub ignored: bool,
    /// True when the provider binding was set by hand (never auto-rebound).
    pub manual: bool,
    /// Cache of the last fetched remote metadata (drives offline update check).
    pub cached_remote: Option<RemoteMeta>,
    pub cached_at: Option<i64>,
    pub last_checked: Option<i64>,
    /// Provider-specific data (mod id, asset ids, hashes...).
    #[serde(default)]
    pub provider_data: serde_json::Value,
    /// Update detected on last check: file id, version.
    pub pending_update: Option<PendingUpdate>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingUpdate {
    pub file_id: String,
    pub version: String,
}

impl AddonState {
    pub fn key(provider: ProviderKind, external_id: &str) -> String {
        format!("{}:{}", provider.as_str(), external_id)
    }
}

impl StateStore {
    /// Map of lowercased AddOns folder name -> store key of the addon that
    /// owns it. The install pipeline uses it to refuse replacing a folder that
    /// belongs to a different managed addon.
    pub fn folder_owners(&self) -> BTreeMap<String, String> {
        let mut map = BTreeMap::new();
        for st in self.addons.values() {
            for f in &st.folders {
                map.entry(f.to_lowercase()).or_insert_with(|| st.key.clone());
            }
        }
        map
    }

    /// Persist the user-intent part of the store into the config's
    /// `[addon ...]` override sections (auto-update, channel, ignored, name,
    /// folders, provider pinning). Returns true if the config changed.
    pub fn write_overrides(&self, cfg: &mut crate::config::AppConfig) -> bool {
        use crate::config::AddonOverride;
        let mut changed = false;
        for a in self.addons.values() {
            let e = cfg.addons.entry(a.key.clone()).or_insert_with(AddonOverride::default);
            let next = AddonOverride {
                provider: Some(a.provider),
                external_id: Some(a.external_id.clone()),
                name: Some(a.name.clone()),
                folders: a.folders.clone(),
                channel: Some(a.channel),
                auto_update: Some(a.auto_update),
                ignored: Some(a.ignored),
                pinned: a.manual,
                installed_file_id: a.installed_file_id.clone(),
                installed_version: a.installed_version.clone(),
            };
            if *e != next {
                *e = next;
                changed = true;
            }
        }
        // drop sections for addons no longer in the store (uninstalled,
        // re-keyed sources); they can only reappear via a scan
        let live: std::collections::HashSet<&String> = self.addons.keys().collect();
        let stale: Vec<String> = cfg.addons.keys().filter(|k| !live.contains(k)).cloned().collect();
        for k in stale {
            cfg.addons.remove(&k);
            changed = true;
        }
        changed
    }

    /// Seed an install list from the config's `[addon ...]` sections: the ini
    /// records what this app installed (source, folders, version), so the
    /// addon list is populated at startup before any scan. The scan stays the
    /// ground truth for folders present on disk; entries whose folders are
    /// gone are dropped by [`crate::engine::scan::apply_scan_report`] (and
    /// their sections pruned on the next save).
    pub fn seed_from_cfg(cfg: &crate::config::AppConfig) -> StateStore {
        let mut store = StateStore::default();
        for ov in cfg.addons.values() {
            let Some(provider) = ov.provider else { continue };
            let Some(external_id) = ov.external_id.as_deref() else { continue };
            let external_id = external_id.trim().to_string();
            if external_id.is_empty() {
                continue;
            }
            let state_key = AddonState::key(provider, &external_id);
            store.addons.insert(
                state_key.clone(),
                AddonState {
                    key: state_key,
                    provider,
                    external_id,
                    name: ov.name.clone().unwrap_or_default(),
                    site_url: None,
                    repo_url: None,
                    icon_url: None,
                    folders: ov.folders.clone(),
                    fingerprints: vec![],
                    installed_version: ov.installed_version.clone(),
                    size_bytes: 0,
                    installed_file_id: ov.installed_file_id.clone(),
                    channel: ov.channel.unwrap_or(cfg.updates.channel),
                    auto_update: ov.auto_update.unwrap_or(cfg.updates.auto_update),
                    ignored: ov.ignored.unwrap_or(false),
                    manual: ov.pinned,
                    cached_remote: None,
                    cached_at: None,
                    last_checked: None,
                    provider_data: serde_json::Value::Null,
                    pending_update: None,
                },
            );
        }
        store
    }

    /// One-time cleanup of the pre-0.2.7 on-disk layout: folds a legacy
    /// `state.json` into the config overrides, then removes `state.json`,
    /// `egui-ui.toml` and the `logs/` folder. Returns true if the config
    /// changed (caller should save the ini).
    pub fn migrate_into_cfg(data_dir: &Path, cfg: &mut crate::config::AppConfig) -> bool {
        let mut changed = false;
        let legacy = data_dir.join("state.json");
        if legacy.exists() {
            // AF-15: the input is only retired after a clean fold, and it is
            // renamed (kept as `state.json.migrated`), never deleted: a
            // corrupt, unreadable, or uncommitted migration must not destroy
            // the last copy of the user's old state.
            let folded = std::fs::read_to_string(&legacy)
                .map_err(|e| tracing::warn!("legacy state unreadable: {e}; keeping state.json"))
                .and_then(|text| {
                    serde_json::from_str::<StateStore>(&text)
                        .map_err(|e| tracing::warn!("legacy state corrupt: {e}; keeping state.json"))
                });
            if let Ok(store) = folded {
                changed |= store.write_overrides(cfg);
                let kept = data_dir.join("state.json.migrated");
                match std::fs::rename(&legacy, &kept) {
                    Ok(()) => tracing::info!("legacy state folded into the ini; original kept as state.json.migrated"),
                    Err(e) => tracing::warn!("could not retire state.json: {e}"),
                }
            }
        }
        let _ = std::fs::remove_dir_all(data_dir.join("logs"));
        let _ = std::fs::remove_file(data_dir.join("egui-ui.toml"));
        changed
    }

    pub fn get(&self, provider: ProviderKind, external_id: &str) -> Option<&AddonState> {
        self.addons.get(&AddonState::key(provider, external_id))
    }

    pub fn get_mut(&mut self, provider: ProviderKind, external_id: &str) -> Option<&mut AddonState> {
        self.addons.get_mut(&AddonState::key(provider, external_id))
    }

    pub fn upsert(&mut self, state: AddonState) {
        self.addons.insert(state.key.clone(), state);
    }

    /// All addons whose installed folder set includes `folder`.
    pub fn find_by_folder(&self, folder: &str) -> Option<&AddonState> {
        self.addons
            .values()
            .find(|a| a.folders.iter().any(|f| f.eq_ignore_ascii_case(folder)))
    }

    pub fn managed_folders(&self) -> Vec<String> {
        self.addons.values().flat_map(|a| a.folders.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;

    fn sample() -> StateStore {
        let mut store = StateStore { schema_version: SCHEMA_VERSION, ..Default::default() };
        store.upsert(AddonState {
            key: AddonState::key(ProviderKind::Curse, "2382"),
            provider: ProviderKind::Curse,
            external_id: "2382".into(),
            name: "BigWigs".into(),
            site_url: None,
            repo_url: None,
            icon_url: None,
            folders: vec!["BigWigs".into(), "BigWigs_Core".into()],
            fingerprints: vec![3793833344],
            installed_version: Some("427.8".into()),
            installed_file_id: Some("9299959".into()),
            size_bytes: 0,
            channel: Channel::Beta,
            auto_update: true,
            ignored: false,
            manual: false,
            cached_remote: None,
            cached_at: None,
            last_checked: None,
            provider_data: serde_json::Value::Null,
            pending_update: None,
        });
        store
    }

    #[test]
    fn write_overrides_persists_user_intent() {
        let mut cfg = AppConfig::default();
        assert!(sample().write_overrides(&mut cfg));
        let ov = &cfg.addons["curse:2382"];
        assert_eq!(ov.auto_update, Some(true));
        assert_eq!(ov.channel, Some(Channel::Beta));
        assert_eq!(ov.folders, vec!["BigWigs", "BigWigs_Core"]);
        // idempotent second pass reports no change
        assert!(!sample().write_overrides(&mut cfg));
        // and the ini round-trips the sections
        let text = cfg.to_ini_text();
        assert!(text.contains("[addon curse:2382]"));
    }

    #[test]
    fn migrate_folds_legacy_state_into_cfg_and_cleans_up() {
        let dir = std::env::temp_dir().join(format!("fa-mig-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("logs")).unwrap();
        std::fs::write(dir.join("state.json"), serde_json::to_string(&sample()).unwrap()).unwrap();
        std::fs::write(dir.join("egui-ui.toml"), "x").unwrap();
        let mut cfg = AppConfig::default();
        assert!(StateStore::migrate_into_cfg(&dir, &mut cfg));
        assert_eq!(cfg.addons["curse:2382"].auto_update, Some(true));
        assert!(!dir.join("state.json").exists());
        assert!(dir.join("state.json.migrated").exists(), "original retained, not deleted");
        assert!(!dir.join("egui-ui.toml").exists());
        assert!(!dir.join("logs").exists());
        // second run is a no-op
        assert!(!StateStore::migrate_into_cfg(&dir, &mut cfg));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn migrate_keeps_corrupt_legacy_file() {
        // AF-15: a legacy file that fails to parse must never be destroyed;
        // nothing is folded and the input stays in place for recovery.
        let dir = std::env::temp_dir().join(format!("fa-mig-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("state.json"), b"{ not json").unwrap();
        let mut cfg = AppConfig::default();
        assert!(!StateStore::migrate_into_cfg(&dir, &mut cfg));
        assert!(dir.join("state.json").exists(), "corrupt input is not deleted");
        assert!(cfg.addons.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn find_by_folder_is_case_insensitive() {
        assert_eq!(sample().find_by_folder("bigwigs_Core").unwrap().name, "BigWigs");
    }
}
