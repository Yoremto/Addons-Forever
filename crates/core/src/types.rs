//! Core shared types: flavors, providers, remote addon metadata, provider trait.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::config::AppConfig;
use crate::error::Result;
use crate::flavor::Flavor;
use crate::toc::ParsedToc;

/// Which addon provider an addon comes from.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Curse,
    GitHub,
    Tukui,
    WowInterface,
    WowUpHub,
    Zip,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderKind::Curse => "curse",
            ProviderKind::GitHub => "github",
            ProviderKind::Tukui => "tukui",
            ProviderKind::WowInterface => "mmoui",
            ProviderKind::WowUpHub => "hub",
            ProviderKind::Zip => "zip",
        }
    }

    /// Inverse of [`Self::as_str`] (used by the portable export format).
    pub fn from_key(s: &str) -> Option<Self> {
        Some(match s {
            "curse" => ProviderKind::Curse,
            "github" => ProviderKind::GitHub,
            "tukui" => ProviderKind::Tukui,
            "mmoui" => ProviderKind::WowInterface,
            "hub" => ProviderKind::WowUpHub,
            "zip" => ProviderKind::Zip,
            _ => return None,
        })
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "curse" | "curseforge" => Some(ProviderKind::Curse),
            "github" | "gh" => Some(ProviderKind::GitHub),
            "tukui" => Some(ProviderKind::Tukui),
            "mmoui" | "wowi" | "wowinterface" => Some(ProviderKind::WowInterface),
            "hub" | "wowup" | "wowuphub" => Some(ProviderKind::WowUpHub),
            "zip" | "url" => Some(ProviderKind::Zip),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ProviderKind::Curse => "CurseForge",
            ProviderKind::GitHub => "GitHub",
            ProviderKind::Tukui => "Tukui",
            ProviderKind::WowInterface => "MMOUI",
            ProviderKind::WowUpHub => "WowUp Hub",
            ProviderKind::Zip => "Zip URL",
        }
    }
}

/// Release channel of a file; ordered so `file.channel <= user_channel` filters.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    #[default]
    Stable,
    Beta,
    Alpha,
}

impl Channel {
    /// Inverse of [`Self::as_str`] (used by the portable export format).
    pub fn from_key(s: &str) -> Option<Self> {
        Some(match s {
            "stable" => Channel::Stable,
            "beta" => Channel::Beta,
            "alpha" => Channel::Alpha,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Beta => "beta",
            Channel::Alpha => "alpha",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "stable" | "release" => Some(Channel::Stable),
            "beta" => Some(Channel::Beta),
            "alpha" => Some(Channel::Alpha),
            _ => None,
        }
    }
}

impl std::fmt::Display for Channel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One distributable file (release asset) of a remote addon.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RemoteFile {
    /// Provider-internal file/release id (stringified).
    pub file_id: String,
    /// Version string as published by the author.
    pub version: String,
    pub channel: Channel,
    pub file_name: String,
    /// Direct download URL (already authenticated, provider-resolved).
    pub url: String,
    /// Extra headers required for the download (e.g. CurseForge api key).
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Flavors this file declares compatibility with. Empty = unknown/any,
    /// but only when the provider publishes no flavor metadata at all; see
    /// `flavor_meta`.
    #[serde(default)]
    pub flavors: Vec<Flavor>,
    /// True when the provider attached flavor metadata to this file, even if
    /// none of it mapped to a known client (e.g. a Curse file tagged only for
    /// other game versions). Such a file is never "compatible with any":
    /// an empty `flavors` list then means "not for this client", not "unknown".
    #[serde(default)]
    pub flavor_meta: bool,
    /// Publish date, unix seconds.
    pub date: Option<i64>,
    pub size: Option<u64>,
    /// md5 of the zip, if the provider publishes it (MMOUI, curse hashes).
    pub md5: Option<String>,
    pub sha1: Option<String>,
    pub changelog_url: Option<String>,
}

/// Remote metadata for one addon (project) from one provider.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RemoteMeta {
    pub provider: ProviderKind,
    /// Curse: mod id. GitHub: `owner/repo`. Tukui: slug. MMOUI: file id. Hub: id. Zip: url.
    pub external_id: String,
    pub name: String,
    pub author: Option<String>,
    pub site_url: Option<String>,
    pub repo_url: Option<String>,
    pub description: Option<String>,
    pub icon_url: Option<String>,
    pub downloads: Option<i64>,
    pub updated_at: Option<i64>,
    /// Newest first.
    pub files: Vec<RemoteFile>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchItem {
    pub provider: ProviderKind,
    pub external_id: String,
    pub name: String,
    pub author: Option<String>,
    pub description: Option<String>,
    pub downloads: Option<i64>,
    pub icon_url: Option<String>,
    pub site_url: Option<String>,
    pub updated_at: Option<i64>,
    pub latest_version: Option<String>,
    /// Size of the latest file in bytes, when the provider reports it.
    #[serde(default)]
    pub size: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct SearchPage {
    pub items: Vec<SearchItem>,
    pub total: u32,
    pub page: u32,
    pub page_size: u32,
}

/// Runtime context shared by all providers.
#[derive(Clone)]
pub struct AppCtx {
    pub http: reqwest::Client,
    pub cfg: Arc<AppConfig>,
    pub app_version: &'static str,
}

impl AppCtx {
    pub fn new(cfg: Arc<AppConfig>, app_version: &'static str) -> Self {
        let ua = cfg.user_agent(app_version);
        AppCtx { http: crate::net::build_client(&ua), cfg, app_version }
    }

    /// `[network] offline` enforcement. Every code path that can touch the
    /// network calls this first, so the checkbox is a real boundary: with it
    /// on, requests fail before connecting (the GUI's local features keep
    /// working, including a local-only scan).
    pub fn ensure_online(&self) -> crate::error::Result<()> {
        if self.cfg.network.offline {
            return Err(crate::error::Error::provider(
                "offline",
                "offline mode is on, network access denied (turn it off in Settings)",
            ));
        }
        Ok(())
    }
}

/// What "compatible with my client" means when picking files.
#[derive(Clone, Debug)]
pub struct FileSelect {
    pub flavor: Flavor,
    /// e.g. 16001
    pub interface: u32,
    /// e.g. "1.60.1"
    pub version: String,
    pub channel: Channel,
}

impl FileSelect {
    /// Selection derived from config: flavor, client version (explicit or the
    /// known-current default), interface number, global channel.
    pub fn from_cfg(cfg: &AppConfig) -> Self {
        Self::from_cfg_channel(cfg, cfg.updates.channel)
    }

    pub fn from_cfg_channel(cfg: &AppConfig, channel: Channel) -> Self {
        let version = cfg
            .app
            .game_version
            .clone()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| crate::flavor::default_client_version(cfg.app.flavor));
        let interface = crate::flavor::version_to_interface(&version).unwrap_or(0);
        FileSelect { flavor: cfg.app.flavor, interface, version, channel }
    }
}

/// A scanned addon folder on disk.
#[derive(Clone, Debug)]
pub struct FolderInfo {
    /// Folder name inside AddOns.
    pub name: String,
    pub path: PathBuf,
    /// Best-matching TOC for the current flavor.
    pub toc: ParsedToc,
    /// CurseForge folder fingerprint (MurmurHash2), None if no TOC closure.
    pub curse_fingerprint: Option<u32>,
    /// Total size of the folder on disk, bytes.
    pub size_bytes: u64,
}

/// One provider-matched addon: the folders on disk that belong to it.
#[derive(Clone, Debug)]
pub struct ScanMatch {
    pub provider: ProviderKind,
    pub external_id: String,
    /// Folder names (in AddOns) claimed by this addon.
    pub folders: Vec<String>,
    pub meta: RemoteMeta,
    /// Version parsed from the installed TOC, if any.
    pub installed_version: Option<String>,
}

#[derive(Debug)]
pub struct ScanRequest<'a> {
    pub folders: &'a [FolderInfo],
    pub sel: &'a FileSelect,
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn kind(&self) -> ProviderKind;

    /// Batch-fetch metadata for known external ids. Missing/unknown ids are skipped.
    async fn meta(&self, ctx: &AppCtx, ids: &[String]) -> Result<Vec<RemoteMeta>>;

    /// Claim scanned folders that this provider recognizes.
    async fn scan(&self, ctx: &AppCtx, req: &ScanRequest<'_>) -> Result<Vec<ScanMatch>>;

    /// Search the catalog for installation. Providers without search return
    /// `Error::Unsupported`.
    async fn search(
        &self,
        ctx: &AppCtx,
        query: &str,
        page: u32,
        per_page: u32,
        sel: &FileSelect,
    ) -> Result<SearchPage>;

    /// Changelog text for a file (markdown/html as provided).
    async fn changelog(
        &self,
        _ctx: &AppCtx,
        _meta: &RemoteMeta,
        file: &RemoteFile,
    ) -> Result<Option<String>> {
        Ok(file.changelog_url.clone().map(|u| format!("Changelog: {u}")))
    }

    /// Pick the best file for a selection. Overridable for providers with rich
    /// asset conventions (GitHub); default filters by flavor + channel, newest first.
    fn pick(&self, meta: &RemoteMeta, sel: &FileSelect) -> Option<RemoteFile> {
        pick_generic(meta, sel)
    }
}

/// Generic file selection: compatible flavor, channel, newest date then highest version.
pub fn pick_generic(meta: &RemoteMeta, sel: &FileSelect) -> Option<RemoteFile> {
    let best = |files: &[&RemoteFile]| -> Option<RemoteFile> {
        files
            .iter()
            .copied()
            .max_by_key(|f| (f.date.unwrap_or(0), crate::util::version_key(&f.version)))
            .cloned()
    };
    let candidates: Vec<&RemoteFile> = meta
        .files
        .iter()
        .filter(|f| f.channel <= sel.channel)
        .collect();
    // an explicit tag for the selected flavor always wins; only then fall
    // back to files the provider left untagged (GitHub releases without
    // metadata, plain zips). A file tagged for other clients is never a match.
    let matched: Vec<&RemoteFile> = candidates
        .iter()
        .copied()
        .filter(|f| f.flavors.contains(&sel.flavor))
        .collect();
    if let Some(f) = best(&matched) {
        return Some(f);
    }
    // compatible-any: providers with no flavor metadata at all
    best(&
        candidates
            .into_iter()
            .filter(|f| f.flavors.is_empty() && !f.flavor_meta)
            .collect::<Vec<_>>())
}

/// Parse a loose version string into a comparable numeric key.
pub use crate::util::version_key;

#[cfg(test)]
mod pick_tests {
    use super::*;

    fn rf(id: &str, flavors: &[Flavor], flavor_meta: bool, date: i64) -> RemoteFile {
        RemoteFile {
            file_id: id.into(),
            version: "1.0.0".into(),
            channel: Channel::Stable,
            file_name: format!("{id}.zip"),
            url: format!("https://example.invalid/{id}.zip"),
            headers: Default::default(),
            flavors: flavors.to_vec(),
            flavor_meta,
            date: Some(date),
            size: None,
            md5: None,
            sha1: None,
            changelog_url: None,
        }
    }

    fn meta_of(files: Vec<RemoteFile>) -> RemoteMeta {
        RemoteMeta {
            provider: ProviderKind::Curse,
            external_id: "1".into(),
            name: "m".into(),
            site_url: None,
            repo_url: None,
            icon_url: None,
            author: None,
            description: None,
            downloads: None,
            updated_at: None,
            files,
        }
    }

    fn sel(flavor: Flavor) -> FileSelect {
        FileSelect {
            flavor,
            interface: 16001,
            version: "1.60.1".into(),
            channel: Channel::Stable,
        }
    }

    #[test]
    fn tagged_file_never_compatible_any() {
        // A Curse file tagged only for other clients (all game version type
        // ids unmapped, so flavors is empty) must not be installed on Forever.
        let meta = meta_of(vec![rf("mists-other", &[], true, 100)]);
        assert!(pick_generic(&meta, &sel(Flavor::Forever)).is_none());
    }

    #[test]
    fn untagged_provider_files_stay_compatible_any() {
        // GitHub releases and plain zips publish no flavor metadata: they must
        // keep installing (the catchall is their only path).
        let meta = meta_of(vec![rf("zip-1", &[], false, 100)]);
        assert_eq!(pick_generic(&meta, &sel(Flavor::Forever)).unwrap().file_id, "zip-1");
    }

    #[test]
    fn flavor_match_beats_foreign_tags_and_untagged() {
        let meta = meta_of(vec![
            rf("foreign", &[Flavor::Mists], true, 300),
            rf("untagged", &[], false, 200),
            rf("forever", &[Flavor::Forever], true, 100),
        ]);
        assert_eq!(pick_generic(&meta, &sel(Flavor::Forever)).unwrap().file_id, "forever");
    }
}
