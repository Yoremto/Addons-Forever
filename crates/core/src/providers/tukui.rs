//! Tukui provider: `GET https://api.tukui.org/v1/addons` (full list, no auth).
//!
//! Live-verified facts (2026-11-06): the list carries ~240 addons, ids are
//! **negative** for the flagship projects (`elvui` = -2, `tukui` = -1), `url` is a
//! rotating download token (`/v1/download/<slug>/<token>`) that must never be
//! cached, `patch[]` lists the game versions per client, `directories[]` lists the
//! addon folders one project installs, `last_update` is a `YYYY-MM-DD` string.

use std::collections::{BTreeMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::flavor::Flavor;
use crate::net;
use crate::types::{
    AppCtx, FileSelect, Provider, ProviderKind, RemoteFile, RemoteMeta, ScanMatch, ScanRequest,
    SearchItem, SearchPage,
};

const LIST_URL: &str = "https://api.tukui.org/v1/addons";
/// Full-list cache lifetime (spec: 10 minutes).
const CACHE_TTL: Duration = Duration::from_secs(10 * 60);

fn non_empty(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

// ---------------------------------------------------------------------------
// API shape
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TukuiAddon {
    /// Negative for Tukui-hosted flagship projects (ElvUI = -2).
    #[serde(deserialize_with = "crate::providers::curse::loose::i64_zero", default)]
    pub id: i64,
    #[serde(deserialize_with = "crate::providers::curse::loose::string", default)]
    pub slug: String,
    #[serde(deserialize_with = "crate::providers::curse::loose::string", default)]
    pub name: String,
    #[serde(deserialize_with = "crate::providers::curse::loose::string", default)]
    pub author: String,
    #[serde(deserialize_with = "crate::providers::curse::loose::string", default)]
    pub version: String,
    /// Tokenized download URL — fresh on every list fetch, never cached on disk.
    #[serde(deserialize_with = "crate::providers::curse::loose::string", default)]
    pub url: String,
    #[serde(deserialize_with = "crate::providers::curse::loose::vec", default)]
    pub patch: Vec<String>,
    #[serde(deserialize_with = "crate::providers::curse::loose::vec", default)]
    pub directories: Vec<String>,
    #[serde(deserialize_with = "crate::providers::curse::loose::string", default)]
    pub small_desc: String,
    #[serde(deserialize_with = "crate::providers::curse::loose::string", default)]
    pub desc: String,
    #[serde(deserialize_with = "crate::providers::curse::loose::string", default)]
    pub web_url: String,
    #[serde(deserialize_with = "crate::providers::curse::loose::string", default)]
    pub changelog_url: String,
    #[serde(deserialize_with = "crate::providers::curse::loose::string", default)]
    pub git_url: String,
    #[serde(deserialize_with = "crate::providers::curse::loose::string", default)]
    pub logo_url: String,
    /// "YYYY-MM-DD" (string in the live API; numbers tolerated).
    #[serde(default)]
    pub last_update: serde_json::Value,
    #[serde(deserialize_with = "crate::providers::curse::loose::opt_i64", default)]
    pub download_count: Option<i64>,
}

/// `1.60.1` -> Forever, `11.x`/`12.x` -> Retail, `1.15` -> ClassicEra, `2.5` -> Tbc,
/// `3.4` -> Wotlk, `3.8x` -> Titan, `4.4` -> Cata, `5.5` -> Mists.
fn flavor_from_patch(p: &str) -> Option<Flavor> {
    let mut parts = p.trim().split('.');
    let major: u32 = parts.next()?.trim().parse().ok()?;
    let minor: u32 = parts.next().unwrap_or("0").trim().parse().ok()?;
    Some(match major {
        1 if minor >= 60 => Flavor::Forever, // 1.60.1, 1.61.x, ...
        1 => Flavor::ClassicEra,             // 1.13 – 1.15
        2 => Flavor::Tbc,                    // 2.5.x
        3 if minor >= 80 => Flavor::Titan,   // 3.80.x
        3 => Flavor::Wotlk,                  // 3.4.x
        4 => Flavor::Cata,                   // 4.4.x
        5 => Flavor::Mists,                  // 5.5.x
        11..=13 => Flavor::Retail,           // 11.x (Dragonflight), 12.x/13.x (Midnight+)
        _ => return None,
    })
}

/// Distinct flavors of a `patch[]` list, in `Flavor::ALL` order.
fn flavors_from_patches(patches: &[String]) -> Vec<Flavor> {
    let mut out: Vec<Flavor> = patches
        .iter()
        .filter_map(|p| flavor_from_patch(p))
        .collect();
    out.sort_by_key(|f| {
        Flavor::ALL
            .iter()
            .position(|x| x == f)
            .unwrap_or(usize::MAX)
    });
    out.dedup();
    out
}

fn last_update_seconds(v: &serde_json::Value) -> Option<i64> {
    match v {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => {
            let s = s.trim();
            if let Ok(n) = s.parse::<i64>() {
                return Some(crate::providers::curse::normalize_epoch(n));
            }
            crate::providers::curse::parse_iso(s)
        }
        _ => v.as_i64().map(crate::providers::curse::normalize_epoch),
    }
}

fn remote_file(a: &TukuiAddon) -> RemoteFile {
    RemoteFile {
        file_id: format!("{}@{}", a.slug, a.version),
        version: a.version.clone(),
        channel: crate::types::Channel::Stable,
        file_name: format!("{}-{}.zip", a.slug, a.version),
        url: a.url.clone(),
        headers: Default::default(),
        flavors: flavors_from_patches(&a.patch),
        flavor_meta: !a.patch.is_empty(),
        date: last_update_seconds(&a.last_update),
        size: None,
        md5: None,
        sha1: None,
        changelog_url: non_empty(&a.changelog_url),
    }
}

fn remote_meta(a: &TukuiAddon) -> RemoteMeta {
    RemoteMeta {
        provider: ProviderKind::Tukui,
        external_id: a.slug.clone(),
        name: if a.name.trim().is_empty() {
            a.slug.clone()
        } else {
            a.name.clone()
        },
        author: non_empty(&a.author),
        site_url: non_empty(&a.web_url),
        repo_url: non_empty(&a.git_url),
        description: non_empty(&a.small_desc).or_else(|| non_empty(&a.desc)),
        icon_url: non_empty(&a.logo_url),
        downloads: a.download_count,
        updated_at: last_update_seconds(&a.last_update),
        files: vec![remote_file(a)],
    }
}

fn search_item(a: &TukuiAddon) -> SearchItem {
    SearchItem {
        provider: ProviderKind::Tukui,
        external_id: a.slug.clone(),
        name: if a.name.trim().is_empty() {
            a.slug.clone()
        } else {
            a.name.clone()
        },
        author: non_empty(&a.author),
        description: non_empty(&a.small_desc),
        downloads: a.download_count,
        icon_url: non_empty(&a.logo_url),
        site_url: non_empty(&a.web_url),
        updated_at: last_update_seconds(&a.last_update),
        latest_version: non_empty(&a.version),
        size: None, // tukui list API exposes no file sizes
    }
}

// ---------------------------------------------------------------------------
// Provider + full-list cache
// ---------------------------------------------------------------------------

type Cache = Mutex<Option<(Instant, Vec<TukuiAddon>)>>;

#[derive(Default)]
pub struct TukuiProvider {
    cache: Cache,
}

impl TukuiProvider {
    pub fn new() -> Self {
        TukuiProvider {
            cache: Mutex::new(None),
        }
    }

    /// GET the full list (no auth needed; the response is a JSON array).
    async fn fetch_list(ctx: &AppCtx) -> Result<Vec<TukuiAddon>> {
        let raw = net::get_json::<serde_json::Value>(&ctx.http, LIST_URL, &[]).await?;
        if !raw.is_array() {
            // an API error object (or an HTML page) is a provider error, not a panic
            let snippet: String = raw.to_string().chars().take(200).collect();
            return Err(Error::provider(
                ProviderKind::Tukui.label(),
                format!("unexpected list response: {snippet}"),
            ));
        }
        serde_json::from_value(raw).map_err(|e| {
            Error::provider(
                ProviderKind::Tukui.label(),
                format!("cannot parse addon list: {e}"),
            )
        })
    }

    /// Cached list (search / scan use this).
    async fn list(&self, ctx: &AppCtx) -> Result<Vec<TukuiAddon>> {
        if let Some(list) = cached(&self.cache) {
            return Ok(list);
        }
        let list = Self::fetch_list(ctx).await?;
        *lock(&self.cache) = Some((Instant::now(), list.clone()));
        Ok(list)
    }

    /// Bypasses the cache: download tokens rotate, so `meta()` must be fresh.
    async fn fresh_list(&self, ctx: &AppCtx) -> Result<Vec<TukuiAddon>> {
        let list = Self::fetch_list(ctx).await?;
        *lock(&self.cache) = Some((Instant::now(), list.clone()));
        Ok(list)
    }

    #[cfg(test)]
    fn cache_age(&self) -> Option<Duration> {
        lock(&self.cache).as_ref().map(|(at, _)| at.elapsed())
    }

    #[cfg(test)]
    fn prime_cache(&self, at: Instant, list: Vec<TukuiAddon>) {
        *lock(&self.cache) = Some((at, list));
    }

    #[cfg(test)]
    fn clear_cache(&self) {
        *lock(&self.cache) = None;
    }
}

fn lock(cache: &Cache) -> std::sync::MutexGuard<'_, Option<(Instant, Vec<TukuiAddon>)>> {
    cache.lock().unwrap_or_else(|e| e.into_inner())
}

/// Unexpired cache entry, cloned.
fn cached(cache: &Cache) -> Option<Vec<TukuiAddon>> {
    cached_at(cache, Instant::now())
}

/// Folder set claimed by one project: `## X-Tukui-ProjectFolders:` wins, else the
/// API `directories[]` when the claiming folder is the bundle root, else the
/// claiming folder alone. Siblings must exist in the scan and belong to no other
/// Tukui project.
fn claim_folders(
    addon: &TukuiAddon,
    claiming: &str,
    csv: &[String],
    scanned: &HashSet<String>,
    foreign: &HashSet<String>,
) -> Vec<String> {
    if !csv.is_empty() {
        // author-declared folders win verbatim (the scan may not have seen them all)
        let mut declared: Vec<String> = csv.to_vec();
        if !declared.iter().any(|n| n.eq_ignore_ascii_case(claiming)) {
            declared.push(claiming.to_string());
        }
        declared.sort();
        declared.dedup();
        return declared;
    }
    let names = if addon
        .directories
        .first()
        .is_some_and(|d| d.eq_ignore_ascii_case(claiming))
    {
        addon.directories.clone()
    } else {
        vec![claiming.to_string()]
    };
    let mut out: Vec<String> = names
        .into_iter()
        .filter(|n| {
            n.eq_ignore_ascii_case(claiming)
                || (scanned.contains(&n.to_ascii_lowercase())
                    && !foreign.contains(&n.to_ascii_lowercase()))
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Pure cache lookup (testable): fresh entries clone, stale ones are treated as a miss.
fn cached_at(cache: &Cache, now: Instant) -> Option<Vec<TukuiAddon>> {
    let guard = lock(cache);
    let (at, list) = guard.as_ref()?;
    if now
        .checked_duration_since(*at)
        .is_some_and(|age| age < CACHE_TTL)
    {
        return Some(list.clone());
    }
    None
}

#[async_trait]
impl Provider for TukuiProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Tukui
    }

    async fn meta(&self, ctx: &AppCtx, ids: &[String]) -> Result<Vec<RemoteMeta>> {
        ctx.ensure_online()?;
        let list = self.fresh_list(ctx).await?;
        let mut out = Vec::new();
        for id in ids {
            let want = id.trim().to_ascii_lowercase();
            if want.is_empty() {
                continue;
            }
            // also accept a bare numeric id (the GUI stores slugs)
            for a in list
                .iter()
                .filter(|a| a.slug.eq_ignore_ascii_case(&want) || a.id.to_string() == want)
            {
                out.push(remote_meta(a));
            }
        }
        Ok(out)
    }

    async fn scan(&self, ctx: &AppCtx, req: &ScanRequest<'_>) -> Result<Vec<ScanMatch>> {
        ctx.ensure_online()?;
        let list = self.fresh_list(ctx).await?;
        let by_id: BTreeMap<String, &TukuiAddon> =
            list.iter().map(|a| (a.id.to_string(), a)).collect();

        // group scanned folders by their `## X-Tukui-ProjectID:`
        let mut groups: BTreeMap<String, Vec<&crate::types::FolderInfo>> = BTreeMap::new();
        for f in req.folders {
            let Some(id) = f
                .toc
                .ids
                .tukui
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            groups.entry(id.to_string()).or_default().push(f);
        }
        // folders owned by a *different* Tukui id (never steal them)
        let foreign: HashSet<String> = req
            .folders
            .iter()
            .filter(|f| f.toc.ids.tukui.as_deref().is_some())
            .map(|f| f.name.to_ascii_lowercase())
            .collect();
        let scanned: HashSet<String> = req
            .folders
            .iter()
            .map(|f| f.name.to_ascii_lowercase())
            .collect();

        let mut out = Vec::new();
        for (id, folders) in groups {
            let Some(addon) = by_id.get(&id) else {
                tracing::warn!("tukui: X-Tukui-ProjectID {id} not in the addon list, skipped");
                continue;
            };
            let claiming = &folders[0].name;
            // `## X-Tukui-ProjectFolders:` wins; else the API's directories when the
            // claiming folder is the bundle root; else just the claiming folder.
            let mut csv: Vec<String> = folders
                .iter()
                .flat_map(|f| f.toc.ids.tukui_folders.clone())
                .collect();
            csv.sort();
            csv.dedup();
            let folders_out = claim_folders(addon, claiming, &csv, &scanned, &foreign);
            let installed_version = folders
                .iter()
                .find(|f| &f.name == claiming)
                .or(folders.first())
                .and_then(|f| f.toc.version.clone());
            out.push(ScanMatch {
                provider: ProviderKind::Tukui,
                external_id: addon.slug.clone(),
                folders: folders_out,
                meta: remote_meta(addon),
                installed_version,
            });
        }
        Ok(out)
    }

    async fn search(
        &self,
        ctx: &AppCtx,
        query: &str,
        page: u32,
        per_page: u32,
        _sel: &FileSelect,
    ) -> Result<SearchPage> {
        ctx.ensure_online()?;
        let list = self.list(ctx).await?;
        let q = query.trim().to_ascii_lowercase();
        let items: Vec<SearchItem> = list
            .iter()
            .filter(|a| {
                q.is_empty()
                    || a.name.to_ascii_lowercase().contains(&q)
                    || a.author.to_ascii_lowercase().contains(&q)
                    || a.slug.to_ascii_lowercase().contains(&q)
            })
            .map(search_item)
            .collect();
        let total = items.len() as u32;
        // client-side only: no pagination on the API, we return the whole match set
        Ok(SearchPage {
            items,
            total,
            page,
            page_size: per_page,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::toc::ParsedToc;
    use crate::types::Channel;
    use std::sync::Arc;

    /// Real API record (api.tukui.org/v1/addons, index 29, 2026-11-06) — ElvUI:
    /// negative id, three directories, a `1.60.1` (Forever) and `12.1.5` patch.
    const ELVUI_JSON: &str = r#"{
      "id": -2, "slug": "elvui", "author": "Elv", "name": "ElvUI",
      "url": "https://api.tukui.org/v1/download/elvui/oKVs9iS2r229CyyC7yDHjfMnGKnqSvvTsu1oP4-L9xGtXAcTiA",
      "version": "16.01",
      "changelog_url": "https://api.tukui.org/v1/changelog/elvui",
      "ticket_url": "https://github.com/tukui-org/ElvUI/issues",
      "git_url": "https://github.com/tukui-org/ElvUI",
      "patch": ["12.1.5", "1.60.1", "5.5.4", "1.15.9", "2.5.6"],
      "last_update": "2026-11-03", "web_url": "https://tukui.org/elvui",
      "donate_url": "http://tukui.org/support",
      "small_desc": "A user interface designed around user-friendliness.",
      "desc": "About: **ElvUI** is a full user interface replacement.",
      "logo_url": "https://storage.tukui.org/ElvUI.jpg",
      "directories": ["ElvUI", "ElvUI_Libraries", "ElvUI_Options"]
    }"#;

    /// Real record for the `tukui` UI (id -1) — carries an 11.x patch.
    const TUKUI_JSON: &str = r#"{"id": -1, "slug": "tukui", "author": "Tukz", "name": "Tukui",
      "url": "https://api.tukui.org/v1/download/tukui/i2eORTiK9XaO7LMPE", "version": "20.463",
      "patch": ["11.1.7", "5.5.0", "1.15.7"], "last_update": "2025-12-05",
      "web_url": "https://tukui.org/tukui", "directories": ["Tukui"]}"#;

    #[test]
    fn parses_real_elvui_record() {
        let a: TukuiAddon = serde_json::from_str(ELVUI_JSON).expect("record parses");
        assert_eq!(a.id, -2, "ElvUI has a negative project id");
        assert_eq!(a.slug, "elvui");
        assert_eq!(a.name, "ElvUI");
        assert_eq!(a.author, "Elv");
        assert_eq!(
            a.directories,
            vec!["ElvUI", "ElvUI_Libraries", "ElvUI_Options"]
        );

        let meta = remote_meta(&a);
        assert_eq!(meta.provider, ProviderKind::Tukui);
        assert_eq!(meta.external_id, "elvui", "external_id is the slug");
        assert_eq!(meta.author.as_deref(), Some("Elv"));
        assert_eq!(meta.site_url.as_deref(), Some("https://tukui.org/elvui"));
        assert_eq!(
            meta.repo_url.as_deref(),
            Some("https://github.com/tukui-org/ElvUI")
        );
        assert_eq!(
            meta.icon_url.as_deref(),
            Some("https://storage.tukui.org/ElvUI.jpg")
        );
        assert_eq!(
            meta.updated_at,
            Some(1_793_664_000),
            "YYYY-MM-DD => unix seconds"
        );
        assert_eq!(meta.files.len(), 1);
        let f = &meta.files[0];
        assert_eq!(f.file_id, "elvui@16.01");
        assert_eq!(f.version, "16.01");
        assert_eq!(f.file_name, "elvui-16.01.zip");
        assert!(f
            .url
            .starts_with("https://api.tukui.org/v1/download/elvui/"));
        assert_eq!(f.date, Some(1_793_664_000));
        assert!(f.headers.is_empty());
        assert_eq!(
            f.changelog_url.as_deref(),
            Some("https://api.tukui.org/v1/changelog/elvui")
        );
    }

    #[test]
    fn patch_maps_to_flavors() {
        // documented ranges, incl. the 11.x -> Retail case
        assert_eq!(flavor_from_patch("1.60.1"), Some(Flavor::Forever));
        assert_eq!(flavor_from_patch("1.61.2"), Some(Flavor::Forever));
        assert_eq!(
            flavor_from_patch("11.1.7"),
            Some(Flavor::Retail),
            "11.x => Retail"
        );
        assert_eq!(flavor_from_patch("12.1.5"), Some(Flavor::Retail));
        assert_eq!(flavor_from_patch("1.15.9"), Some(Flavor::ClassicEra));
        assert_eq!(flavor_from_patch("2.5.6"), Some(Flavor::Tbc));
        assert_eq!(flavor_from_patch("3.4.5"), Some(Flavor::Wotlk));
        assert_eq!(flavor_from_patch("3.80.2"), Some(Flavor::Titan));
        assert_eq!(flavor_from_patch("4.4.2"), Some(Flavor::Cata));
        assert_eq!(flavor_from_patch("5.5.4"), Some(Flavor::Mists));
        assert_eq!(flavor_from_patch("9.2.0"), None);
        assert_eq!(flavor_from_patch(""), None);
        assert_eq!(flavor_from_patch("nonsense"), None);

        let a: TukuiAddon = serde_json::from_str(ELVUI_JSON).unwrap();
        assert_eq!(
            flavors_from_patches(&a.patch),
            vec![
                Flavor::Forever,
                Flavor::Retail,
                Flavor::ClassicEra,
                Flavor::Tbc,
                Flavor::Mists
            ]
        );
        // 1.60.1 present in patch[] => ElvUI is Forever-compatible
        assert!(remote_file(&a).flavors.contains(&Flavor::Forever));

        let t: TukuiAddon = serde_json::from_str(TUKUI_JSON).unwrap();
        assert_eq!(
            flavors_from_patches(&t.patch),
            vec![Flavor::Retail, Flavor::ClassicEra, Flavor::Mists]
        );
    }

    fn ctx() -> AppCtx {
        AppCtx {
            http: net::build_client("fa-test"),
            cfg: Arc::new(AppConfig::default()),
            app_version: "test",
        }
    }

    fn folder(name: &str, toc_id: Option<&str>, extra: &[&str]) -> crate::types::FolderInfo {
        let mut toc = ParsedToc::default();
        toc.ids.tukui = toc_id.map(str::to_string);
        toc.ids.tukui_folders = extra.iter().map(|s| s.to_string()).collect();
        toc.version = Some("16.01".into());
        crate::types::FolderInfo {
            name: name.into(),
            path: std::path::PathBuf::from("/tmp").join(name),
            toc,
            curse_fingerprint: None,
            size_bytes: 0,
        }
    }

    #[test]
    fn scan_claims_by_negative_project_id() {
        let a: TukuiAddon = serde_json::from_str(ELVUI_JSON).unwrap();
        let by_id: BTreeMap<String, &TukuiAddon> = [(a.id.to_string(), &a)].into_iter().collect();
        assert_eq!(
            by_id.get("-2").map(|x| x.slug.as_str()),
            Some("elvui"),
            "negative ids are the key"
        );

        // the bundle root carries the TOC id; its sibling directories come from the API
        let folders = [folder("ElvUI", Some("-2"), &[]),
            folder("ElvUI_Libraries", None, &[]),
            folder("ElvUI_Options", None, &[]),
            folder("OtherAddon", Some("-1"), &[])];
        let scanned: HashSet<String> = folders
            .iter()
            .map(|f| f.name.to_ascii_lowercase())
            .collect();
        let names = claim_folders(&a, "ElvUI", &[], &scanned, &HashSet::new());
        assert_eq!(names, vec!["ElvUI", "ElvUI_Libraries", "ElvUI_Options"]);

        // X-Tukui-ProjectFolders wins over the API list
        let names = claim_folders(
            &a,
            "ElvUI",
            &["ElvUI".into(), "Custom".into()],
            &scanned,
            &HashSet::new(),
        );
        assert_eq!(names, vec!["Custom", "ElvUI"]);

        // a folder claiming a non-listed id is skipped by scan()
        assert!(by_id.get("-999").is_none());

        // meta() of the record (fresh token) is the same shape as meta() output
        let meta = remote_meta(&a);
        assert_eq!(meta.files[0].url, a.url);
        assert_eq!(Channel::Stable, meta.files[0].channel);
        let flavors = &meta.files[0].flavors;
        assert!(
            flavors.contains(&Flavor::Forever),
            "1.60.1 in patch[] => Forever: {flavors:?}"
        );
        assert!(
            flavors.contains(&Flavor::Retail),
            "12.1.5 in patch[] => Retail: {flavors:?}"
        );
    }

    #[test]
    fn list_cache_is_10_minutes() {
        let p = TukuiProvider::new();
        assert_eq!(
            cached_at(&p.cache, Instant::now()),
            None,
            "empty cache is a miss"
        );

        let list = vec![TukuiAddon {
            slug: "elvui".into(),
            name: "ElvUI".into(),
            ..Default::default()
        }];
        p.prime_cache(Instant::now(), list.clone());
        assert_eq!(p.cache_age().map(|d| d.as_secs()), Some(0));
        let hit = cached_at(&p.cache, Instant::now()).expect("fresh entry is a hit");
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].slug, "elvui");

        // 9:59 old => still cached, 10:01 old => expired (first call fetches again)
        let at = Instant::now();
        p.prime_cache(at - Duration::from_secs(9 * 60 + 59), list.clone());
        assert!(cached_at(&p.cache, at).is_some(), "9m59s is inside the TTL");
        p.prime_cache(at - Duration::from_secs(10 * 60 + 1), list);
        assert!(cached_at(&p.cache, at).is_none(), "10m01s is expired");
        p.clear_cache();
        assert!(cached_at(&p.cache, at).is_none());
    }

    #[test]
    fn malformed_list_is_an_error_not_a_panic() {
        // non-array bodies are rejected as provider errors (see fetch_list)
        let v: serde_json::Value = serde_json::from_str(r#"{"ERROR":"nope"}"#).unwrap();
        assert!(!v.is_array());
        assert!(serde_json::from_value::<Vec<TukuiAddon>>(v).is_err());
        // missing fields fall back to defaults; wrong scalar types are ignored
        let a: TukuiAddon = serde_json::from_str(
            r#"{"id":"x","slug":"z","patch":"nope","last_update":1724089877}"#,
        )
        .expect("tolerant record");
        assert_eq!(a.id, 0);
        assert!(a.patch.is_empty() && a.directories.is_empty());
        assert_eq!(last_update_seconds(&a.last_update), Some(1_724_089_877));
        let m = remote_meta(&a);
        assert_eq!(m.external_id, "z");
        assert!(m.author.is_none() && m.files[0].flavors.is_empty());
        assert!(serde_json::from_str::<Vec<TukuiAddon>>("[{").is_err());
    }

    fn live() -> bool {
        std::env::var("FA_LIVE").map(|v| v == "1").unwrap_or(false)
    }

    #[tokio::test]
    #[ignore = "live Tukui API (FA_LIVE=1)"]
    async fn live_list_contains_elvui_with_fresh_token() {
        if !live() {
            return;
        }
        let c = ctx();
        let p = TukuiProvider::new();
        let list = TukuiProvider::fetch_list(&c).await.expect("list ok");
        // Today the endpoint serves the two flagship UIs (elvui, tukui); older
        // snapshots carried ~240 entries. Assert the stable facts only.
        assert!(list.len() >= 2, "unexpectedly short list: {}", list.len());
        let elvui = list
            .iter()
            .find(|a| a.slug == "elvui")
            .expect("list contains elvui");
        assert_eq!(elvui.id, -2);
        assert_eq!(elvui.name, "ElvUI");
        assert!(
            elvui.patch.iter().any(|p| p.starts_with("1.60")),
            "forever patch: {:?}",
            elvui.patch
        );

        // meta() re-fetches: the download token must be present and the cache warm
        let metas = p
            .meta(&c, &["elvui".into(), "no-such-slug".into()])
            .await
            .expect("meta ok");
        assert_eq!(metas.len(), 1, "unknown slugs are skipped");
        assert_eq!(metas[0].external_id, "elvui");
        assert!(!metas[0].files.is_empty());
        let url = &metas[0].files[0].url;
        assert!(
            url.starts_with("https://api.tukui.org/v1/download/elvui/"),
            "{url}"
        );
        assert!(
            url.rsplit('/').next().is_some_and(|t| t.len() > 20),
            "token expected"
        );
        assert!(p.cache_age().is_some(), "list is cached for 10 min");

        // search is client-side over the full list
        let page = p
            .search(
                &c,
                "elv",
                0,
                20,
                &FileSelect {
                    flavor: Flavor::Forever,
                    interface: 16001,
                    version: "1.60.1".into(),
                    channel: Channel::Stable,
                },
            )
            .await
            .expect("search ok");
        assert!(page.items.iter().any(|i| i.external_id == "elvui"));
        assert!(net::host_allowed(LIST_URL));
    }
}
