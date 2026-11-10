//! WowUp Hub provider (spec §5).
//!
//! No-auth JSON API at `https://hub.wowup.io`: `GET /addons/search/{game_type}`
//! (query required) and `GET /addons/{id}`. Releases carry direct `download_url`s,
//! `game_type` → Flavor, and `addonFolders` — which is what the folder scan claims
//! (name-similarity matching over a search of the folder name, threshold 0.85).
//! This is our GitHub-addon discovery source, so `search()` is fully supported.

use std::collections::HashSet;

use async_trait::async_trait;
use serde::Deserialize;
use tracing::warn;

use crate::error::{Error, Result};
use crate::flavor::Flavor;
use crate::net;
use crate::types::{
    AppCtx, Channel, FileSelect, Provider, ProviderKind, RemoteFile, RemoteMeta, ScanMatch,
    ScanRequest, SearchPage,
};

const HUB: &str = "https://hub.wowup.io";
const SIMILARITY_THRESHOLD: f64 = 0.85;

#[derive(Default)]
pub struct HubProvider;

impl HubProvider {
    pub fn new() -> Self {
        HubProvider
    }
}

// ---------------------------------------------------------------- serde models

/// `null` (and missing) deserialize as an empty Vec — hub fields are flaky.
fn de_vec_null_default<'de, D, T>(d: D) -> std::result::Result<Vec<T>, D::Error>
where
    T: serde::de::Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Ok(<Option<Vec<T>> as serde::Deserialize>::deserialize(d)?.unwrap_or_default())
}

/// Numeric ids in the wild; strings tolerated.
#[derive(Debug, Deserialize, Default)]
#[serde(untagged)]
pub(crate) enum JsonId {
    #[default]
    Empty,
    Num(i64),
    Text(String),
}

impl JsonId {
    fn as_str(&self) -> String {
        match self {
            JsonId::Num(n) => n.to_string(),
            JsonId::Text(s) => s.clone(),
            JsonId::Empty => String::new(),
        }
    }
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct HubAddon {
    #[serde(default)]
    pub(crate) id: JsonId,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub repository_name: Option<String>,
    /// some endpoints use plain `name`
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    #[allow(dead_code)] // "github"/"curse"/... provenance note, not mapped into RemoteMeta
    pub source: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub image_url: Option<String>,
    #[serde(default)]
    pub owner_name: Option<String>,
    #[serde(default)]
    pub total_download_count: Option<i64>,
    #[serde(default, deserialize_with = "de_vec_null_default")]
    pub releases: Vec<HubRelease>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub(crate) struct HubRelease {
    #[serde(default)]
    pub download_url: Option<String>,
    /// asset file name (e.g. `ElvUI_MerathilisUI-7.44.zip`)
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub tag_name: Option<String>,
    #[serde(default)]
    pub game_type: Option<String>,
    /// per-game-type metadata; authoritative flavor list when present
    #[serde(default, deserialize_with = "de_vec_null_default")]
    pub game_versions: Vec<HubGameVersion>,
    #[serde(default)]
    #[allow(dead_code)]
    pub game_version: Option<String>,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(
        default,
        rename = "addonFolders",
        deserialize_with = "de_vec_null_default"
    )]
    pub addon_folders: Vec<HubFolder>,
    #[serde(default)]
    pub toc_version: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub(crate) struct HubGameVersion {
    #[serde(default)]
    pub game_type: Option<String>,
}

/// Live hub sends `addonFolders` as objects (`{folder_name, …}`); the older
/// spec shape is plain strings. Accept both.
#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub(crate) enum HubFolder {
    Name(String),
    Entry {
        #[serde(default, rename = "folder_name")]
        folder_name: String,
    },
}

impl HubFolder {
    pub(crate) fn name(&self) -> &str {
        match self {
            HubFolder::Name(s) => s,
            HubFolder::Entry { folder_name } => folder_name,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum HubSearchResp {
    Wrapped {
        #[serde(default, deserialize_with = "de_vec_null_default")]
        addons: Vec<HubAddon>,
    },
    List(Vec<HubAddon>),
}

/// `GET /addons/{id}` wraps in `{"addon": {…}}` (live-verified); bare object tolerated.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum HubSingleResp {
    Wrapped { addon: HubAddon },
    Addon(HubAddon),
}

// ------------------------------------------------------------------- helpers

fn game_type_flavor(gt: &str) -> Option<Flavor> {
    match gt.trim().to_ascii_lowercase().as_str() {
        "burningcrusade" | "burning-crusade" => Some(Flavor::Tbc),
        "wotlkc" | "wotlkclassic" => Some(Flavor::Wotlk),
        "cataclysm" => Some(Flavor::Cata),
        other => Flavor::parse(other),
    }
}

fn parse_iso(s: &str) -> Option<i64> {
    let s = s.trim();
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.timestamp())
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
                .ok()
                .map(|d| d.and_utc().timestamp())
        })
        .or_else(|| {
            chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .ok()
                .map(|d| {
                    d.and_hms_opt(0, 0, 0)
                        .map(|t| t.and_utc().timestamp())
                        .unwrap_or(0)
                })
        })
}

fn nonempty(s: Option<&String>) -> Option<String> {
    let s = s?.trim();
    (!s.is_empty()).then(|| s.to_string())
}

fn url_file_name(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| {
            u.path()
                .rsplit('/')
                .next()
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        })
        .unwrap_or_default()
}

/// Strip HTML tags and decode the handful of entities the hub emits.
pub fn strip_html(s: &str) -> String {
    const ENTITIES: &[(&str, &str)] = &[
        ("amp;", "&"),
        ("lt;", "<"),
        ("gt;", ">"),
        ("quot;", "\""),
        ("#39;", "'"),
        ("apos;", "'"),
        ("nbsp;", " "),
    ];
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '<' {
            // drop the tag, keep a word separator
            while let Some(&n) = chars.peek() {
                chars.next();
                if n == '>' {
                    break;
                }
            }
            out.push(' ');
            continue;
        }
        if c == '&' {
            let lower: String = chars
                .clone()
                .flat_map(|c| c.to_lowercase())
                .take(6)
                .collect();
            if let Some((entity, decoded)) = ENTITIES.iter().find(|(e, _)| lower.starts_with(e)) {
                for _ in 0..entity.chars().count() {
                    chars.next();
                }
                out.push_str(decoded);
                continue;
            }
        }
        out.push(c);
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Normalized lowercase name: non-alphanumerics → single spaces.
pub fn normalize_name(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn tokenize(norm: &str) -> Vec<&str> {
    norm.split(' ').filter(|t| !t.is_empty()).collect()
}

/// 1.0 equal · 0.9 word-containment (needle tokens inside hay) · Jaccard of
/// token sets otherwise. Simple, no deps (spec §5).
pub fn name_similarity(a: &str, b: &str) -> f64 {
    let (na, nb) = (normalize_name(a), normalize_name(b));
    if na.is_empty() || nb.is_empty() {
        return 0.0;
    }
    if na == nb {
        return 1.0;
    }
    let (ta, tb): (HashSet<&str>, HashSet<&str>) = (
        tokenize(&na).into_iter().collect(),
        tokenize(&nb).into_iter().collect(),
    );
    let inter = ta.intersection(&tb).count();
    if inter > 0 {
        let smaller_first = ta.len() <= tb.len();
        let (needle, hay) = if smaller_first {
            (&ta, &tb)
        } else {
            (&tb, &ta)
        };
        if needle.is_subset(hay) {
            return 0.9;
        }
    }
    let union = ta.union(&tb).count();
    if union == 0 {
        return 0.0;
    }
    inter as f64 / union as f64
}

pub(crate) fn build_files(releases: &[HubRelease]) -> Vec<RemoteFile> {
    let mut files: Vec<RemoteFile> = Vec::new();
    for (idx, r) in releases.iter().enumerate() {
        let Some(url) = nonempty(r.download_url.as_ref()) else {
            continue;
        };
        let tag = nonempty(r.tag_name.as_ref());
        let toc = nonempty(r.toc_version.as_ref());
        let version = toc
            .or(tag.clone())
            .unwrap_or_else(|| format!("release-{idx}"));
        let file_id = tag.unwrap_or_else(|| url.clone());
        let mut flavors: Vec<Flavor> = Vec::new();
        for gt in r
            .game_versions
            .iter()
            .filter_map(|g| g.game_type.as_deref())
            .chain(r.game_type.as_deref())
        {
            if let Some(f) = game_type_flavor(gt) {
                if !flavors.contains(&f) {
                    flavors.push(f);
                }
            }
        }
        files.push(RemoteFile {
            file_id,
            version,
            channel: if r.prerelease {
                Channel::Beta
            } else {
                Channel::Stable
            },
            file_name: nonempty(r.name.as_ref()).unwrap_or_else(|| url_file_name(&url)),
            url,
            headers: Default::default(),
            flavor_meta: !flavors.is_empty(),
            flavors,
            date: r.published_at.as_deref().and_then(parse_iso),
            size: None,
            md5: None,
            sha1: None,
            changelog_url: None,
        });
    }
    // newest first
    files.sort_by_key(|f| std::cmp::Reverse(f.date.unwrap_or(0)));
    files
}

pub(crate) fn build_meta(a: &HubAddon) -> Option<RemoteMeta> {
    let external_id = a.id.as_str();
    if external_id.is_empty() {
        return None;
    }
    let name = nonempty(a.repository_name.as_ref())
        .or_else(|| nonempty(a.name.as_ref()))
        .unwrap_or_else(|| format!("addon {external_id}"));
    let files = build_files(&a.releases);
    let updated_at = files.iter().filter_map(|f| f.date).max();
    Some(RemoteMeta {
        provider: ProviderKind::WowUpHub,
        external_id: external_id.to_string(),
        name,
        author: nonempty(a.owner_name.as_ref()),
        site_url: nonempty(a.homepage.as_ref()),
        repo_url: nonempty(a.repository.as_ref()),
        description: a
            .description
            .as_deref()
            .map(strip_html)
            .filter(|d| !d.is_empty()),
        icon_url: nonempty(a.image_url.as_ref()),
        downloads: a.total_download_count,
        updated_at,
        files,
    })
}

fn search_url(game_type: &str, query: &str, limit: u32) -> Result<String> {
    let url = reqwest::Url::parse(&format!("{HUB}/addons/search/{game_type}"))
        .map_err(|e| Error::provider("WowUp Hub", format!("bad url: {e}")))?;
    reqwest::Url::parse_with_params(
        url.as_str(),
        &[("query", query), ("limit", &limit.to_string())][..],
    )
    .map(|u| u.to_string())
    .map_err(|e| Error::provider("WowUp Hub", format!("bad query: {e}")))
}

/// Sanitize an external id for URL-path use (hub ids are numeric strings).
fn clean_id(id: &str) -> Option<&str> {
    let id = id.trim();
    if id.is_empty()
        || id.len() > 64
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return None;
    }
    Some(id)
}

// ------------------------------------------------------------------ provider

#[async_trait]
impl Provider for HubProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::WowUpHub
    }

    async fn meta(&self, ctx: &AppCtx, ids: &[String]) -> Result<Vec<RemoteMeta>> {
        ctx.ensure_online()?;
        let mut out = Vec::new();
        for raw in ids {
            let Some(id) = clean_id(raw) else {
                warn!(target: "fa::hub", "skipping unparseable hub id {raw:?}");
                continue;
            };
            let resp: HubSingleResp =
                match net::get_json(&ctx.http, &format!("{HUB}/addons/{id}"), &[]).await {
                    Ok(a) => a,
                    Err(e) => {
                        warn!(target: "fa::hub", "hub addon {id}: {e}");
                        continue;
                    }
                };
            let addon = match resp {
                HubSingleResp::Wrapped { addon } => addon,
                HubSingleResp::Addon(a) => a,
            };
            match build_meta(&addon) {
                Some(m) => out.push(m),
                None => warn!(target: "fa::hub", "hub addon {id}: missing id in response"),
            }
        }
        Ok(out)
    }

    async fn scan(&self, ctx: &AppCtx, req: &ScanRequest<'_>) -> Result<Vec<ScanMatch>> {
        ctx.ensure_online()?;
        let scanned: HashSet<String> = req.folders.iter().map(|f| f.name.clone()).collect();
        // repo/folder-name → best addon match (cached per name)
        let mut claims: Vec<(HubAddon, String, f64)> = Vec::new(); // addon, folder name, score
        for folder in req.folders {
            let query = folder.name.trim();
            if query.chars().count() < 2 {
                continue;
            }
            let url = match search_url(req.sel.flavor.hub_game_type(), query, 20) {
                Ok(u) => u,
                Err(e) => {
                    warn!(target: "fa::hub", "scan url for {query:?}: {e}");
                    continue;
                }
            };
            let resp: HubSearchResp = match net::get_json(&ctx.http, &url, &[]).await {
                Ok(r) => r,
                Err(e) => {
                    warn!(target: "fa::hub", "scan search for {query:?}: {e}");
                    continue;
                }
            };
            let addons = match resp {
                HubSearchResp::Wrapped { addons } => addons,
                HubSearchResp::List(v) => v,
            };
            let mut best: Option<(HubAddon, f64)> = None;
            for addon in addons {
                let display = nonempty(addon.repository_name.as_ref())
                    .or_else(|| nonempty(addon.name.as_ref()))
                    .unwrap_or_default();
                let score = name_similarity(query, &display);
                if score >= SIMILARITY_THRESHOLD && best.as_ref().is_none_or(|(_, s)| score > *s) {
                    best = Some((addon, score));
                }
            }
            if let Some((addon, score)) = best {
                claims.push((addon, folder.name.clone(), score));
            }
        }
        // group by addon id; folders = release.addonFolders ∩ scanned names
        let mut out: Vec<ScanMatch> = Vec::new();
        for (addon, folder_name, _score) in claims {
            let Some(meta) = build_meta(&addon) else {
                continue;
            };
            let mut folders: Vec<String> = addon
                .releases
                .iter()
                .flat_map(|r| r.addon_folders.iter().map(|f| f.name().to_string()))
                .filter(|f| scanned.contains(f.as_str()))
                .collect();
            folders.sort();
            folders.dedup();
            if folders.is_empty() {
                folders.push(folder_name.clone());
            }
            let installed_version = req
                .folders
                .iter()
                .find(|f| f.name == folder_name)
                .and_then(|f| f.toc.version.clone().filter(|v| !v.trim().is_empty()));
            if let Some(existing) = out.iter_mut().find(|m| m.external_id == meta.external_id) {
                for f in folders {
                    if !existing.folders.contains(&f) {
                        existing.folders.push(f);
                    }
                }
                existing.folders.sort();
            } else {
                out.push(ScanMatch {
                    provider: ProviderKind::WowUpHub,
                    external_id: meta.external_id.clone(),
                    folders,
                    meta,
                    installed_version,
                });
            }
        }
        Ok(out)
    }

    async fn search(
        &self,
        ctx: &AppCtx,
        query: &str,
        page: u32,
        per_page: u32,
        sel: &FileSelect,
    ) -> Result<SearchPage> {
        ctx.ensure_online()?;
        let q = query.trim();
        if q.chars().count() < 2 {
            return Err(Error::provider(
                "WowUp Hub",
                "search query needs at least 2 characters",
            ));
        }
        let limit = per_page.clamp(1, 50);
        let url = search_url(sel.flavor.hub_game_type(), q, limit)?;
        let resp: HubSearchResp = net::get_json(&ctx.http, &url, &[]).await?;
        let addons = match resp {
            HubSearchResp::Wrapped { addons } => addons,
            HubSearchResp::List(v) => v,
        };
        let mut items = Vec::with_capacity(addons.len());
        for addon in addons {
            let Some(meta) = build_meta(&addon) else {
                continue;
            };
            items.push(crate::types::SearchItem {
                provider: ProviderKind::WowUpHub,
                external_id: meta.external_id,
                name: meta.name,
                author: meta.author,
                description: meta.description,
                downloads: meta.downloads,
                icon_url: meta.icon_url,
                site_url: meta.site_url,
                updated_at: meta.updated_at,
                latest_version: meta.files.first().map(|f| f.version.clone()),
                size: meta.files.first().and_then(|f| f.size),
            });
        }
        let total = items.len() as u32;
        Ok(SearchPage {
            items,
            total,
            page,
            page_size: limit,
        })
    }
}

// -------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn search_fixture() -> &'static str {
        r#"{
          "count": 1,
          "addons": [
            {"id": 123,
             "repository": "https://github.com/ElvUI/ElvUI",
             "repository_name": "ElvUI",
             "source": "github",
             "description": "<p>A <b>full</b> UI &amp; more &#39;stuff&#39;</p>",
             "homepage": "https://www.tukui.org",
             "image_url": "https://hub.wowup.io/img/elvui.png",
             "owner_name": "Simpyx",
             "total_download_count": 1234567,
             "topics": ["ui"],
             "extra_field": {"ignored": true},
             "releases": [
               {"download_url": "https://hub.wowup.io/dl/1.zip", "name": "ElvUI-12.0.1.zip",
                "tag_name": "v1.1", "game_type": "retail", "game_version": "12.0", "prerelease": false,
                "published_at": "2026-10-05T10:00:00Z", "addonFolders": ["ElvUI"], "toc_version": "12.0.1"},
               {"download_url": "https://hub.wowup.io/dl/2.zip", "name": "ElvUI.zip", "tag_name": "v1.0-forever",
                "game_type": "forever", "prerelease": true,
                "published_at": "2026-11-05T05:19:48.000Z",
                "game_versions": [{"game_type": "retail", "interface": "120105"}, {"game_type": "forever", "interface": "16001"}],
                "addonFolders": [{"fingerprint": "dbbbd3", "folder_name": "ElvUI_Core", "id": 42}, {"folder_name": "NotScanned"}]},
               {"tag_name": "no-url", "game_type": "forever"}
             ]}
          ]
        }"#
    }

    #[test]
    fn parse_search_response_fixture() {
        let resp: HubSearchResp = serde_json::from_str(search_fixture()).unwrap();
        let addons = match resp {
            HubSearchResp::Wrapped { addons } => addons,
            HubSearchResp::List(_) => panic!("expected wrapped"),
        };
        assert_eq!(addons.len(), 1);
        let meta = build_meta(&addons[0]).unwrap();
        assert_eq!(meta.provider, ProviderKind::WowUpHub);
        assert_eq!(meta.external_id, "123"); // numeric id stringified
        assert_eq!(meta.name, "ElvUI");
        assert_eq!(meta.author.as_deref(), Some("Simpyx"));
        assert_eq!(
            meta.description.as_deref(),
            Some("A full UI & more 'stuff'")
        );
        assert_eq!(meta.downloads, Some(1234567));
        assert_eq!(meta.site_url.as_deref(), Some("https://www.tukui.org"));
        assert_eq!(
            meta.repo_url.as_deref(),
            Some("https://github.com/ElvUI/ElvUI")
        );

        // releases → files, newest first, release without download_url skipped
        assert_eq!(meta.files.len(), 2);
        assert_eq!(meta.files[0].file_id, "v1.0-forever");
        // game_versions[] is the authoritative flavor list when present
        assert_eq!(meta.files[0].flavors, vec![Flavor::Retail, Flavor::Forever]);
        assert_eq!(meta.files[0].channel, Channel::Beta); // prerelease
        assert_eq!(meta.files[0].url, "https://hub.wowup.io/dl/2.zip");
        assert_eq!(meta.files[0].file_name, "ElvUI.zip"); // release.name (asset file name)
                                                          // ms-precision ISO ("…T05:19:48.000Z") parses
        assert!(meta.files[0].date.unwrap() > 1_793_000_000);
        assert_eq!(meta.files[1].version, "12.0.1"); // toc_version preferred
        assert_eq!(meta.files[1].flavors, vec![Flavor::Retail]); // game_type fallback, single
        assert_eq!(meta.files[1].file_name, "ElvUI-12.0.1.zip");
        assert!(meta.files[0].date.unwrap() > meta.files[1].date.unwrap());
        assert_eq!(meta.updated_at, meta.files[0].date);
        // addonFolders: plain strings and {folder_name} objects both parse
        assert_eq!(addons[0].releases[0].addon_folders[0].name(), "ElvUI");
        assert_eq!(addons[0].releases[1].addon_folders[0].name(), "ElvUI_Core");
        assert_eq!(addons[0].releases[1].addon_folders[1].name(), "NotScanned");
    }

    #[test]
    fn parse_single_addon_and_list_shapes() {
        // /addons/{id} returns {"addon": {…}} (live); bare object tolerated
        let wrapped: HubSingleResp = serde_json::from_str(
            r#"{"addon":{"id":123,"repository_name":"ElvUI","releases":[{"download_url":"https://hub.wowup.io/dl/1.zip","tag_name":"v1","game_type":"forever"}]}}"#,
        )
        .unwrap();
        let addon = match wrapped {
            HubSingleResp::Wrapped { addon } => addon,
            HubSingleResp::Addon(a) => a,
        };
        let meta = build_meta(&addon).unwrap();
        assert_eq!(meta.external_id, "123");
        assert_eq!(meta.files.len(), 1);
        let bare: HubSingleResp =
            serde_json::from_str(r#"{"id":5,"repository_name":"B"}"#).unwrap();
        match bare {
            HubSingleResp::Addon(a) => assert_eq!(build_meta(&a).unwrap().external_id, "5"),
            HubSingleResp::Wrapped { .. } => panic!("bare addon must not match the wrapper"),
        }
        // search endpoints: {addons:[..]} and bare arrays both parse
        let wrapped: HubSearchResp = serde_json::from_str(search_fixture()).unwrap();
        let addons = match wrapped {
            HubSearchResp::Wrapped { addons } => addons,
            HubSearchResp::List(v) => v,
        };
        assert_eq!(addons.len(), 1);
        let list: HubSearchResp =
            serde_json::from_str(r#"[{"id":7,"repository_name":"X","releases":[]}]"#).unwrap();
        let addons = match list {
            HubSearchResp::List(v) => v,
            HubSearchResp::Wrapped { addons } => addons,
        };
        assert_eq!(addons.len(), 1);
        assert_eq!(build_meta(&addons[0]).unwrap().external_id, "7");
        // missing id → no meta; extra/missing fields tolerated
        let empty: HubAddon = serde_json::from_str("{}").unwrap();
        assert!(build_meta(&empty).is_none());
        assert!(serde_json::from_str::<HubAddon>("{\"id\":\"abc123\",\"releases\":null}").is_ok());
    }

    #[test]
    fn game_type_flavor_map() {
        assert_eq!(game_type_flavor("forever"), Some(Flavor::Forever));
        assert_eq!(game_type_flavor("retail"), Some(Flavor::Retail));
        assert_eq!(game_type_flavor("classic"), Some(Flavor::ClassicEra));
        assert_eq!(game_type_flavor("burningCrusade"), Some(Flavor::Tbc));
        assert_eq!(game_type_flavor("wotlk"), Some(Flavor::Wotlk));
        assert_eq!(game_type_flavor("cata"), Some(Flavor::Cata));
        assert_eq!(game_type_flavor("mists"), Some(Flavor::Mists));
        assert_eq!(game_type_flavor("titan"), Some(Flavor::Titan));
        assert_eq!(game_type_flavor("nonsense"), None);
    }

    #[test]
    fn name_similarity_rules() {
        assert!((name_similarity("ElvUI", "elvui") - 1.0).abs() < f64::EPSILON);
        assert!(name_similarity("BigWigs_Core", "BigWigs") >= SIMILARITY_THRESHOLD);
        assert!(name_similarity("Details", "Details! Damage Meter") >= SIMILARITY_THRESHOLD);
        assert!(name_similarity("bagnon", "Bagnon") >= SIMILARITY_THRESHOLD);
        assert!(name_similarity("foo", "myfoo") < SIMILARITY_THRESHOLD);
        assert!(name_similarity("auctioneer", "bagginsed") < SIMILARITY_THRESHOLD);
        assert!((name_similarity("", "x")).abs() < f64::EPSILON);
        assert!(name_similarity("DBM-Core", "Deadly Boss Mods") < SIMILARITY_THRESHOLD);
    }

    #[test]
    fn strip_html_rules() {
        assert_eq!(strip_html("<p>Hello <b>World</b></p>"), "Hello World");
        assert_eq!(strip_html("a &amp; b &#39;c&#39;"), "a & b 'c'");
        assert_eq!(strip_html("no html"), "no html");
        assert_eq!(
            strip_html("<em>Multi<br>line\n  text</em>"),
            "Multi line text"
        );
    }

    #[test]
    fn clean_ids() {
        assert_eq!(clean_id("123"), Some("123"));
        assert_eq!(clean_id(" ab-12_x "), Some("ab-12_x"));
        assert_eq!(clean_id("../etc/passwd"), None);
        assert_eq!(clean_id(""), None);
    }

    #[test]
    fn iso_dates_tolerant() {
        assert_eq!(parse_iso("2026-11-01T10:00:00Z"), Some(1_793_527_200));
        assert_eq!(parse_iso("2026-11-01T10:00:00"), Some(1_793_527_200)); // naive → UTC
        assert_eq!(parse_iso("2026-11-01"), Some(1_793_491_200));
        assert!(parse_iso("garbage").is_none());
        assert!(parse_iso("").is_none());
    }

    #[test]
    fn search_query_min_length() {
        // the search() guard is at its top; here test the URL builder
        let url = search_url(Flavor::Forever.hub_game_type(), "elvui", 20).unwrap();
        assert!(url.starts_with("https://hub.wowup.io/addons/search/forever?"));
        assert!(url.contains("query=elvui"));
        assert!(url.contains("limit=20"));
    }

    // ---- live smoke (network) ------------------------------------------------

    fn live_ctx() -> AppCtx {
        AppCtx {
            http: net::build_client("addons-forever-test"),
            cfg: std::sync::Arc::new(crate::config::AppConfig::default()),
            app_version: "test",
        }
    }

    fn test_sel() -> FileSelect {
        FileSelect {
            flavor: Flavor::Forever,
            interface: 16001,
            version: "1.60.1".into(),
            channel: Channel::Stable,
        }
    }

    #[tokio::test]
    #[ignore = "live WowUp Hub API (FA_LIVE=1)"]
    async fn live_hub_search_elvui() {
        if std::env::var("FA_LIVE").map(|v| v != "1").unwrap_or(true) {
            return;
        }
        let ctx = live_ctx();
        let page = HubProvider
            .search(&ctx, "elvui", 0, 20, &test_sel())
            .await
            .expect("hub search");
        assert!(
            !page.items.is_empty(),
            "hub search forever/elvui returned nothing"
        );
        // search results may omit releases; the meta endpoint fills them in
        let ids: Vec<String> = page.items.iter().map(|i| i.external_id.clone()).collect();
        let metas = HubProvider.meta(&ctx, &ids).await.expect("hub meta");
        assert!(!metas.is_empty(), "hub meta for search hits");
        assert!(
            metas.iter().any(|m| !m.files.is_empty()),
            "some hub elvui addon has releases"
        );
    }
}
