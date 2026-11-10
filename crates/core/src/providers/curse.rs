//! CurseForge provider: catalog search, project metadata, file lists, fingerprint
//! scanning and changelogs, via `https://api.curseforge.com` (gameId 1 = WoW).
//!
//! Live-verified API facts (2026-11-06, see docs/SPEC-PROVIDERS.md):
//! - every request needs the `x-api-key` header (community key, `[network]`
//!   `curse_api_key` / `cfg.curse_key_header()`);
//! - since 2026-07 the CDN (`edge.forgecdn.net`) demands the same header, so every
//!   `RemoteFile.headers` carries it (the installer replays them on the download);
//! - dates are ISO-8601 strings (`fileDate`, `dateModified`), not unix seconds;
//! - `file.hashes[]` uses `algo` 1 = sha1, 2 = md5;
//! - flavor filtering is the numeric `gameVersionTypeId` (Forever = 88568);
//! - fingerprint scan is `POST /v1/fingerprints` (`/v1/apps/fingerprint` is gone);
//! - the changelog endpoint returns `{data: "<html>"}` (a bare string, not an object).

use std::collections::{BTreeMap, HashMap, HashSet};

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::flavor::Flavor;
use crate::net;
use crate::types::{
    AppCtx, Channel, FileSelect, Provider, ProviderKind, RemoteFile, RemoteMeta, ScanMatch,
    ScanRequest, SearchItem, SearchPage,
};

const BASE: &str = "https://api.curseforge.com";
const FILES_PAGE_SIZE: u32 = 50;
/// File pages fetched per meta() call (spec cap).
const FILES_MAX_PAGES: u32 = 4;
/// Download URLs resolved per call (one request each) — newest files first.
const DOWNLOAD_URL_CAP: usize = 10;
/// Mod ids per `POST /v1/mods` batch call.
const MODS_BATCH: usize = 50;

// ---------------------------------------------------------------------------
// JSON value helpers (tolerant: providers mix numbers, strings and nulls)
// ---------------------------------------------------------------------------

fn as_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => {
            let s = s.trim();
            s.parse::<i64>()
                .ok()
                .or_else(|| s.parse::<f64>().ok().map(|f| f as i64))
        }
        _ => None,
    }
}

fn as_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => {
            let s = s.trim();
            (!s.is_empty()).then(|| s.to_string())
        }
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Unix seconds from a provider date value: ISO-8601 string, unix seconds, or
/// unix milliseconds. None for null/garbage (never panics).
fn unix_seconds(v: &Value) -> Option<i64> {
    match v {
        Value::Null => None,
        Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                return None;
            }
            if let Ok(n) = s.parse::<i64>() {
                return Some(normalize_epoch(n));
            }
            parse_iso(s)
        }
        _ => as_i64(v).map(normalize_epoch),
    }
}

pub(crate) fn normalize_epoch(n: i64) -> i64 {
    // anything beyond ~5138 AD in seconds is really milliseconds
    if n.abs() >= 1_000_000_000_000 {
        n / 1000
    } else {
        n
    }
}

pub(crate) fn parse_iso(s: &str) -> Option<i64> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp());
    }
    // "2026-10-21T12:14:13" (no offset) and plain "2026-10-21"
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Some(dt.and_utc().timestamp());
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return d.and_hms_opt(0, 0, 0).map(|dt| dt.and_utc().timestamp());
    }
    None
}

fn flavor_of_type_id(id: i64) -> Option<Flavor> {
    Flavor::ALL
        .iter()
        .copied()
        .find(|f| f.curse_type_id() as i64 == id)
}

fn channel_of(release_type: i64) -> Channel {
    match release_type {
        2 => Channel::Beta,
        3 => Channel::Alpha,
        _ => Channel::Stable,
    }
}

fn is_not_found(e: &Error) -> bool {
    matches!(e, Error::Provider { message, .. } if message.contains("-> 404"))
}

/// Lenient field deserializers: the APIs send `null` for present fields, numbers as
/// strings and occasional type changes. Nothing here ever panics — a field that
/// cannot be interpreted falls back to its default.
pub(crate) mod loose {
    use serde::{Deserialize, Deserializer};
    use serde_json::Value;

    use super::{as_i64, as_string};

    fn value<'de, D>(d: D) -> std::result::Result<Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        Value::deserialize(d)
    }

    pub(crate) fn string<'de, D>(d: D) -> std::result::Result<String, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(as_string(&value(d)?).unwrap_or_default())
    }

    pub(crate) fn opt_string<'de, D>(d: D) -> std::result::Result<Option<String>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(as_string(&value(d)?))
    }

    pub(crate) fn i64_zero<'de, D>(d: D) -> std::result::Result<i64, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(as_i64(&value(d)?).unwrap_or(0))
    }

    pub(crate) fn opt_i64<'de, D>(d: D) -> std::result::Result<Option<i64>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(as_i64(&value(d)?))
    }

    pub(crate) fn opt_u64<'de, D>(d: D) -> std::result::Result<Option<u64>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(value(d)?.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64))
    }

    pub(crate) fn opt_bool<'de, D>(d: D) -> std::result::Result<Option<bool>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match value(d)? {
            Value::Bool(b) => Some(b),
            Value::Number(n) => n.as_i64().map(|i| i != 0),
            Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" => Some(true),
                "false" | "0" | "no" => Some(false),
                _ => None,
            },
            _ => None,
        })
    }

    /// Any sequence; a null / object / mismatching entry becomes an empty vec.
    pub(crate) fn vec<'de, D, T>(d: D) -> std::result::Result<Vec<T>, D::Error>
    where
        D: Deserializer<'de>,
        T: serde::de::DeserializeOwned,
    {
        Ok(serde_json::from_value(value(d)?).unwrap_or_default())
    }

    pub(crate) fn opt_vec<'de, D, T>(d: D) -> std::result::Result<Option<Vec<T>>, D::Error>
    where
        D: Deserializer<'de>,
        T: serde::de::DeserializeOwned,
    {
        Ok(serde_json::from_value(value(d)?).ok().flatten())
    }

    /// Any JSON value interpreted as `T`; a mismatching shape yields `T::default()`.
    pub(crate) fn structured<'de, D, T>(d: D) -> std::result::Result<T, D::Error>
    where
        D: Deserializer<'de>,
        T: serde::de::DeserializeOwned + Default,
    {
        Ok(serde_json::from_value(value(d)?).unwrap_or_default())
    }

    pub(crate) fn map<'de, D, K, V>(
        d: D,
    ) -> std::result::Result<std::collections::HashMap<K, V>, D::Error>
    where
        D: Deserializer<'de>,
        K: serde::de::DeserializeOwned + std::cmp::Eq + std::cmp::Ord + std::hash::Hash,
        V: serde::de::DeserializeOwned,
    {
        Ok(serde_json::from_value(value(d)?).unwrap_or_default())
    }
}

// ---------------------------------------------------------------------------
// API response shapes (all fields optional: unknown/missing keys are tolerated)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
#[allow(dead_code)] // pagination fields mirror the API shape
struct Pagination {
    #[serde(deserialize_with = "loose::i64_zero", default)]
    index: i64,
    #[serde(deserialize_with = "loose::i64_zero", default)]
    page_size: i64,
    #[serde(deserialize_with = "loose::i64_zero", default)]
    result_count: i64,
    #[serde(deserialize_with = "loose::i64_zero", default)]
    total_count: i64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Author {
    #[serde(deserialize_with = "loose::string", default)]
    name: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Links {
    #[serde(deserialize_with = "loose::string", default)]
    website_url: String,
    #[serde(deserialize_with = "loose::string", default)]
    source_url: String,
    #[serde(deserialize_with = "loose::string", default)]
    issues_url: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct IconUrl {
    #[serde(deserialize_with = "loose::string", default)]
    url: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Logo {
    #[serde(deserialize_with = "loose::string", default)]
    url: String,
    /// Current API: a plain string. Older docs: `thumbnailUrls: [{url}]`.
    #[serde(deserialize_with = "loose::string", default)]
    thumbnail_url: String,
    #[serde(deserialize_with = "loose::vec", default)]
    thumbnail_urls: Vec<IconUrl>,
}

impl Logo {
    fn best(&self) -> Option<String> {
        (!self.thumbnail_url.trim().is_empty())
            .then(|| self.thumbnail_url.clone())
            .or_else(|| {
                self.thumbnail_urls
                    .iter()
                    .map(|t| t.url.clone())
                    .find(|u| !u.trim().is_empty())
            })
            .or_else(|| (!self.url.trim().is_empty()).then(|| self.url.clone()))
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
#[allow(dead_code)] // game_version is informational; type ids drive flavor mapping
struct GameVersionRef {
    #[serde(deserialize_with = "loose::i64_zero", default)]
    game_version_type_id: i64,
    #[serde(deserialize_with = "loose::string", default)]
    game_version: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
#[allow(dead_code)] // values are read through the mapping helpers
struct Hash {
    #[serde(deserialize_with = "loose::string", default)]
    value: String,
    #[serde(deserialize_with = "loose::i64_zero", default)]
    algo: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
#[allow(dead_code)] // `name` documents the module shape; matching uses `fingerprint`
struct Module {
    #[serde(deserialize_with = "loose::string", default)]
    name: String,
    #[serde(deserialize_with = "loose::i64_zero", default)]
    fingerprint: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct CurseFile {
    #[serde(deserialize_with = "loose::i64_zero", default)]
    id: i64,
    #[serde(deserialize_with = "loose::i64_zero", default)]
    mod_id: i64,
    #[serde(deserialize_with = "loose::string", default)]
    file_name: String,
    #[serde(deserialize_with = "loose::string", default)]
    display_name: String,
    #[serde(deserialize_with = "loose::i64_zero", default)]
    release_type: i64,
    #[serde(deserialize_with = "loose::opt_bool", default)]
    is_available: Option<bool>,
    #[serde(deserialize_with = "loose::opt_string", default)]
    download_url: Option<String>,
    /// ISO-8601 string in the live API, unix seconds in the docs.
    #[serde(default)]
    file_date: Value,
    #[serde(deserialize_with = "loose::opt_u64", default)]
    file_length: Option<u64>,
    #[serde(deserialize_with = "loose::vec", default)]
    hashes: Vec<Hash>,
    #[serde(deserialize_with = "loose::vec", default)]
    modules: Vec<Module>,
    #[serde(deserialize_with = "loose::vec", default)]
    sortable_game_versions: Vec<GameVersionRef>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Mod {
    #[serde(deserialize_with = "loose::i64_zero", default)]
    id: i64,
    #[serde(deserialize_with = "loose::string", default)]
    name: String,
    #[serde(deserialize_with = "loose::string", default)]
    slug: String,
    #[serde(deserialize_with = "loose::string", default)]
    summary: String,
    #[serde(deserialize_with = "loose::i64_zero", default)]
    download_count: i64,
    #[serde(deserialize_with = "loose::vec", default)]
    authors: Vec<Author>,
    #[serde(deserialize_with = "loose::structured", default)]
    links: Links,
    #[serde(deserialize_with = "loose::structured", default)]
    logo: Logo,
    #[serde(deserialize_with = "loose::vec", default)]
    latest_files: Vec<CurseFile>,
    #[serde(default)]
    date_modified: Value,
    #[serde(deserialize_with = "loose::opt_bool", default)]
    is_available: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ModResp {
    #[serde(deserialize_with = "loose::structured", default)]
    data: Mod,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ModsResp {
    #[serde(deserialize_with = "loose::vec", default)]
    data: Vec<Mod>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FilesResp {
    #[serde(deserialize_with = "loose::vec", default)]
    data: Vec<CurseFile>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SearchResp {
    #[serde(deserialize_with = "loose::vec", default)]
    data: Vec<Mod>,
    #[serde(deserialize_with = "loose::structured", default)]
    pagination: Pagination,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FingerprintResp {
    #[serde(deserialize_with = "loose::structured", default)]
    data: FingerprintData,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
#[allow(dead_code)] // exact_fingerprints / unmatched are informational
struct FingerprintData {
    #[serde(deserialize_with = "loose::vec", default)]
    exact_matches: Vec<MatchEntry>,
    #[serde(deserialize_with = "loose::vec", default)]
    partial_matches: Vec<MatchEntry>,
    #[serde(deserialize_with = "loose::map", default)]
    partial_match_fingerprints: HashMap<String, Vec<i64>>,
    #[serde(deserialize_with = "loose::vec", default)]
    exact_fingerprints: Vec<i64>,
    /// `null` in live responses — Option so null deserializes cleanly.
    #[serde(deserialize_with = "loose::opt_vec", default)]
    unmatched_fingerprints: Option<Vec<i64>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct MatchEntry {
    #[serde(deserialize_with = "loose::i64_zero", default)]
    id: i64,
    #[serde(deserialize_with = "loose::structured", default)]
    file: CurseFile,
    #[serde(deserialize_with = "loose::vec", default)]
    latest_files: Vec<CurseFile>,
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

fn key_header(ctx: &AppCtx) -> Vec<(String, String)> {
    vec![ctx.cfg.curse_key_header()]
}

/// Build an API URL with percent-encoded query pairs.
fn url(path: &str, query: &[(&str, String)]) -> Result<String> {
    let mut u = reqwest::Url::parse(&format!("{BASE}{path}")).map_err(|e| {
        Error::provider(ProviderKind::Curse.label(), format!("bad url {path}: {e}"))
    })?;
    {
        let mut q = u.query_pairs_mut();
        for (k, v) in query {
            q.append_pair(k, v);
        }
    }
    Ok(u.to_string())
}

async fn get<T: DeserializeOwned>(ctx: &AppCtx, url: &str) -> Result<T> {
    net::get_json(&ctx.http, url, &key_header(ctx)).await
}

/// Resolve a CurseForge mod slug (from a `curseforge.com/wow/addons/<slug>` page URL)
/// to a mod id string.
pub async fn resolve_slug(ctx: &AppCtx, slug: &str) -> Result<Option<String>> {
    let u = url(
        "/v1/mods/search",
        &[("gameId", "1".into()), ("slug", slug.into()), ("pageSize", "1".into())],
    )?;
    let v: Value = get(ctx, &u).await?;
    Ok(v
        .get("data")
        .and_then(|d| d.as_array())
        .and_then(|a| a.first())
        .and_then(|m| m.get("id"))
        .and_then(|id| id.as_u64())
        .map(|id| id.to_string()))
}

/// JSON POST (only `/v1/fingerprints` and `/v1/mods` batches need it). Mirrors
/// `net::get_json`: allowlist enforced, non-2xx becomes a provider error.
async fn post<T: DeserializeOwned>(ctx: &AppCtx, path: &str, body: &Value) -> Result<T> {
    let full = url(path, &[])?;
    net::ensure_allowed(&full, &[])?;
    let (k, v) = ctx.cfg.curse_key_header();
    let resp = ctx
        .http
        .post(&full)
        .header(k.as_str(), v.as_str())
        .header("content-type", "application/json")
        .json(body)
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        let snippet: String = text.chars().take(300).collect();
        return Err(Error::provider(
            ProviderKind::Curse.label(),
            format!("POST {path} -> {status}: {snippet}"),
        ));
    }
    Ok(resp.json::<T>().await?)
}

// ---------------------------------------------------------------------------
// Mapping
// ---------------------------------------------------------------------------

fn non_empty(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

fn remote_file(ctx: &AppCtx, m: &Mod, f: &CurseFile) -> RemoteFile {
    // The CDN requires the api key header since 2026-07.
    let mut headers = BTreeMap::new();
    headers.insert(
        "x-api-key".to_string(),
        ctx.cfg.network.curse_api_key.clone(),
    );

    let mut flavors: Vec<Flavor> = f
        .sortable_game_versions
        .iter()
        .filter_map(|g| flavor_of_type_id(g.game_version_type_id))
        .collect();
    flavors.sort_by_key(|f| {
        Flavor::ALL
            .iter()
            .position(|x| x == f)
            .unwrap_or(usize::MAX)
    });
    flavors.dedup(); // no sortableGameVersions => empty => compatible-any

    let version = non_empty(&f.display_name)
        .or_else(|| non_empty(&f.file_name))
        .unwrap_or_else(|| f.id.to_string());
    let mut md5 = None;
    let mut sha1 = None;
    for h in &f.hashes {
        match h.algo {
            2 => md5 = non_empty(&h.value),
            1 => sha1 = non_empty(&h.value),
            _ => {}
        }
    }
    let changelog_url =
        non_empty(&m.slug).map(|s| format!("https://www.curseforge.com/wow/addons/{s}/changelog"));
    RemoteFile {
        file_id: f.id.to_string(),
        version,
        channel: channel_of(f.release_type),
        file_name: f.file_name.clone(),
        url: f.download_url.clone().unwrap_or_default(),
        headers,
        // sortableGameVersions present => the API told us which clients this
        // file targets; type ids we cannot map mean "not for our clients"
        flavor_meta: !f.sortable_game_versions.is_empty(),
        flavors,
        date: unix_seconds(&f.file_date),
        size: f.file_length,
        md5,
        sha1,
        changelog_url,
    }
}

fn meta_shell(m: &Mod, files: Vec<RemoteFile>) -> RemoteMeta {
    RemoteMeta {
        provider: ProviderKind::Curse,
        external_id: m.id.to_string(),
        name: if m.name.trim().is_empty() {
            format!("CurseForge mod {}", m.id)
        } else {
            m.name.clone()
        },
        author: m.authors.first().and_then(|a| non_empty(&a.name)),
        site_url: non_empty(&m.links.website_url),
        repo_url: non_empty(&m.links.source_url).or_else(|| non_empty(&m.links.issues_url)),
        description: non_empty(&m.summary),
        icon_url: m.logo.best(),
        downloads: Some(m.download_count),
        updated_at: unix_seconds(&m.date_modified),
        files,
    }
}

fn search_item(m: &Mod) -> SearchItem {
    SearchItem {
        provider: ProviderKind::Curse,
        external_id: m.id.to_string(),
        name: if m.name.trim().is_empty() {
            format!("CurseForge mod {}", m.id)
        } else {
            m.name.clone()
        },
        author: m.authors.first().and_then(|a| non_empty(&a.name)),
        description: non_empty(&m.summary),
        downloads: Some(m.download_count),
        icon_url: m.logo.best(),
        site_url: non_empty(&m.links.website_url),
        updated_at: unix_seconds(&m.date_modified),
        latest_version: m
            .latest_files
            .first()
            .and_then(|f| non_empty(&f.display_name)),
        size: m.latest_files.first().and_then(|f| f.file_length),
    }
}

/// Dedupe by file id, drop unavailable, newest first (date, then file id).
fn normalize_files(files: Vec<CurseFile>) -> Vec<CurseFile> {
    let mut seen: HashSet<i64> = HashSet::new();
    let mut out: Vec<CurseFile> = files
        .into_iter()
        .filter(|f| f.id != 0 && f.is_available != Some(false) && seen.insert(f.id))
        .collect();
    out.sort_by(|a, b| {
        let da = unix_seconds(&a.file_date).unwrap_or(0);
        let db = unix_seconds(&b.file_date).unwrap_or(0);
        db.cmp(&da).then(b.id.cmp(&a.id))
    });
    out
}

/// Fill in missing `downloadUrl` fields (one request each, capped, oldest first
/// skipped: we resolve the newest files only — that is what installs use).
async fn resolve_urls(ctx: &AppCtx, mod_id: &str, files: &mut [RemoteFile]) {
    let mut budget = DOWNLOAD_URL_CAP;
    for f in files.iter_mut() {
        if !f.url.trim().is_empty() || budget == 0 {
            continue;
        }
        budget -= 1;
        let path = format!("/v1/mods/{mod_id}/files/{}/download-url", f.file_id);
        match get::<Value>(ctx, &url(&path, &[]).unwrap_or_default()).await {
            Ok(v) => {
                if let Some(u) = v.get("data").and_then(as_string) {
                    f.url = u;
                }
            }
            Err(e) if is_not_found(&e) => {}
            Err(e) => {
                tracing::warn!("curse download-url {}/{}: {e}", mod_id, f.file_id);
            }
        }
    }
}

async fn files_for(ctx: &AppCtx, mod_id: &str, type_id: Option<u32>) -> Result<Vec<RemoteFile>> {
    let mut raw: Vec<CurseFile> = Vec::new();
    for page in 0..FILES_MAX_PAGES {
        let mut qs: Vec<(&str, String)> = vec![
            ("pageSize", FILES_PAGE_SIZE.to_string()),
            ("index", (page * FILES_PAGE_SIZE).to_string()),
        ];
        if let Some(t) = type_id {
            qs.push(("gameVersionTypeId", t.to_string()));
        }
        let path = format!("/v1/mods/{mod_id}/files");
        let resp: FilesResp = get(ctx, &url(&path, &qs)?).await?;
        let n = resp.data.len();
        raw.extend(resp.data);
        if !raw.is_empty() || n == 0 {
            break; // one page with results is enough
        }
    }
    if raw.is_empty() && type_id.is_some() {
        // The flavor filter can be over-restrictive (meta() has no FileSelect):
        // retry once unfiltered so the user still sees the project's files.
        return Box::pin(files_for(ctx, mod_id, None)).await;
    }
    let shell = Mod::default(); // no project record in the files response
    let mut files: Vec<RemoteFile> = normalize_files(raw)
        .into_iter()
        .map(|f| remote_file(ctx, &shell, &f))
        .collect();
    resolve_urls(ctx, mod_id, &mut files).await;
    Ok(files)
}

// ---------------------------------------------------------------------------
// Fingerprint scan (pure part, unit-testable)
// ---------------------------------------------------------------------------

/// One fingerprint match: mod id + the claimed folder names + files to build meta from.
#[derive(Debug)]
struct FingerprintHit {
    mod_id: i64,
    folders: Vec<String>,
    files: Vec<CurseFile>,
}

/// Map fingerprint matches to scanned folders. `exact_matches` are processed
/// first and a folder claimed once is never re-claimed.
fn fingerprint_hits(
    data: &FingerprintData,
    folders_by_fp: &BTreeMap<u32, Vec<String>>,
) -> Vec<FingerprintHit> {
    let mut claimed: HashSet<String> = HashSet::new();
    let mut hits: Vec<FingerprintHit> = Vec::new();
    for entry in data.exact_matches.iter().chain(data.partial_matches.iter()) {
        let mod_id = if entry.file.mod_id != 0 {
            entry.file.mod_id
        } else {
            entry.id
        };
        let key = mod_id.to_string();

        // primary: module fingerprints of the matched file that we actually scanned
        let mut fps: Vec<u32> = entry
            .file
            .modules
            .iter()
            .filter_map(|m| u32::try_from(m.fingerprint).ok())
            .filter(|fp| folders_by_fp.contains_key(fp))
            .collect();
        if fps.is_empty() {
            // fallback: every fingerprint the API assigned to this mod
            // (`partialMatchFingerprints`; the API has no per-mod exact list)
            fps.extend(
                data.partial_match_fingerprints
                    .get(&key)
                    .map(|v| v.as_slice())
                    .unwrap_or(&[])
                    .iter()
                    .filter_map(|f| u32::try_from(*f).ok()),
            );
        }
        let mut folders: Vec<String> = fps
            .iter()
            .flat_map(|fp| folders_by_fp.get(fp).cloned().unwrap_or_default())
            .filter(|name| claimed.insert(name.to_ascii_lowercase()))
            .collect();
        folders.sort();
        folders.dedup();
        if folders.is_empty() {
            continue;
        }
        let files = std::iter::once(&entry.file)
            .chain(entry.latest_files.iter())
            .cloned()
            .collect();
        hits.push(FingerprintHit {
            mod_id,
            folders,
            files,
        });
    }
    hits
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct CurseProvider;

impl CurseProvider {
    pub fn new() -> Self {
        CurseProvider
    }
}

#[async_trait]
impl Provider for CurseProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Curse
    }

    async fn meta(&self, ctx: &AppCtx, ids: &[String]) -> Result<Vec<RemoteMeta>> {
        ctx.ensure_online()?;
        let mut out = Vec::new();
        for raw in ids {
            let id = raw.trim();
            if id.is_empty() {
                continue;
            }
            let path = format!("/v1/mods/{id}");
            let resp: ModResp = match get(ctx, &url(&path, &[])?).await {
                Ok(r) => r,
                Err(e) if is_not_found(&e) => continue, // unknown id: skipped
                Err(e) => return Err(e),
            };
            let m = resp.data;
            if m.id == 0 {
                continue;
            }
            let type_id = ctx.cfg.app.flavor.curse_type_id();
            let mut files = match files_for(ctx, id, Some(type_id)).await {
                Ok(f) => f,
                Err(e) if is_not_found(&e) => Vec::new(),
                Err(e) => return Err(e),
            };
            // re-attach the project record (slug for changelog urls, name)
            for f in files.iter_mut() {
                if f.changelog_url.is_none() {
                    f.changelog_url = non_empty(&m.slug)
                        .map(|s| format!("https://www.curseforge.com/wow/addons/{s}/changelog"));
                }
            }
            out.push(meta_shell(&m, files));
        }
        Ok(out)
    }

    async fn scan(&self, ctx: &AppCtx, req: &ScanRequest<'_>) -> Result<Vec<ScanMatch>> {
        ctx.ensure_online()?;
        let mut out: Vec<ScanMatch> = Vec::new();
        let mut claimed: HashSet<String> = HashSet::new();

        // 1. folder fingerprints -> POST /v1/fingerprints
        let mut folders_by_fp: BTreeMap<u32, Vec<String>> = BTreeMap::new();
        for f in req.folders {
            if let Some(fp) = f.curse_fingerprint {
                folders_by_fp.entry(fp).or_default().push(f.name.clone());
            }
        }
        if !folders_by_fp.is_empty() {
            let fps: Vec<u32> = folders_by_fp.keys().copied().collect();
            let resp: FingerprintResp =
                post(ctx, "/v1/fingerprints", &json!({ "fingerprints": fps })).await?;
            let hits = fingerprint_hits(&resp.data, &folders_by_fp);

            // one batch call for the project records (names, authors, icons)
            let ids: Vec<i64> = hits.iter().map(|h| h.mod_id).collect();
            let mods: HashMap<i64, Mod> = batch_mods(ctx, &ids).await.unwrap_or_default();

            for hit in hits {
                let folders: Vec<String> = hit
                    .folders
                    .into_iter()
                    .filter(|n| claimed.insert(n.to_ascii_lowercase()))
                    .collect();
                if folders.is_empty() {
                    continue;
                }
                let empty = Mod::default();
                let shell = mods.get(&hit.mod_id).unwrap_or(&empty);
                let mut files: Vec<RemoteFile> = normalize_files(hit.files)
                    .into_iter()
                    .map(|f| remote_file(ctx, shell, &f))
                    .collect();
                let id_str = hit.mod_id.to_string();
                resolve_urls(ctx, &id_str, &mut files).await;
                let meta = meta_shell(shell, files);
                let installed_version = req
                    .folders
                    .iter()
                    .find(|f| folders.iter().any(|n| n.eq_ignore_ascii_case(&f.name)))
                    .and_then(|f| f.toc.version.clone());
                out.push(ScanMatch {
                    provider: ProviderKind::Curse,
                    external_id: id_str,
                    folders,
                    meta,
                    installed_version,
                });
            }
        }

        // 2. TOC `## X-Curse-Project-ID:` claims
        let mut by_id: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for f in req.folders {
            if claimed.iter().any(|c| c.eq_ignore_ascii_case(&f.name)) {
                continue;
            }
            if let Some(id) = f
                .toc
                .ids
                .curse
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                by_id
                    .entry(id.to_string())
                    .or_default()
                    .push(f.name.clone());
            }
        }
        for (id, folders) in &by_id {
            if out.iter().any(|m| m.external_id == *id) {
                continue;
            }
            let metas = match self.meta(ctx, std::slice::from_ref(id)).await {
                Ok(m) => m,
                Err(e) if is_not_found(&e) => continue,
                Err(e) => return Err(e),
            };
            for meta in metas {
                let installed_version = req
                    .folders
                    .iter()
                    .find(|f| folders.iter().any(|n| n.eq_ignore_ascii_case(&f.name)))
                    .and_then(|f| f.toc.version.clone());
                for n in folders {
                    claimed.insert(n.to_ascii_lowercase());
                }
                out.push(ScanMatch {
                    provider: ProviderKind::Curse,
                    external_id: meta.external_id.clone(),
                    folders: folders.clone(),
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
        let per = per_page.clamp(1, 50);
        // `page` is 0-based (the API `index` is an offset = page * per_page).
        let index = page.saturating_mul(per);
        let path = "/v1/mods/search";
        let qs: [(&str, String); 7] = [
            ("gameId", "1".to_string()),
            ("searchFilter", query.trim().to_string()),
            ("sortField", "2".to_string()), // popularity
            // the API defaults to ASCENDING, which surfaces zero-download
            // renames and spam at the top of the result list
            ("order", "desc".to_string()),
            ("gameVersionTypeId", sel.flavor.curse_type_id().to_string()),
            ("index", index.to_string()),
            ("pageSize", per.to_string()),
        ];
        let resp: SearchResp = get(ctx, &url(path, &qs)?).await?;
        let items = resp
            .data
            .iter()
            .filter(|m| m.is_available != Some(false))
            .map(search_item)
            .collect();
        Ok(SearchPage {
            items,
            total: resp.pagination.total_count.max(0) as u32,
            page,
            page_size: per,
        })
    }

    async fn changelog(
        &self,
        ctx: &AppCtx,
        meta: &RemoteMeta,
        file: &RemoteFile,
    ) -> Result<Option<String>> {
        ctx.ensure_online()?;
        if file.file_id.is_empty() || !file.file_id.bytes().all(|c| c.is_ascii_digit()) {
            return Ok(file
                .changelog_url
                .clone()
                .map(|u| format!("Changelog: {u}")));
        }
        let path = format!(
            "/v1/mods/{}/files/{}/changelog",
            meta.external_id, file.file_id
        );
        match get::<Value>(ctx, &url(&path, &[])?).await {
            // live API: `data` is the raw html string; docs show `{body: html}`
            Ok(v) => {
                let body = match &v["data"] {
                    Value::String(s) => Some(s.clone()),
                    Value::Object(o) => o.get("body").and_then(as_string),
                    _ => None,
                };
                match body.filter(|b| !b.trim().is_empty()) {
                    Some(b) => Ok(Some(b)),
                    None => Ok(file
                        .changelog_url
                        .clone()
                        .map(|u| format!("Changelog: {u}"))),
                }
            }
            Err(e) if is_not_found(&e) => Ok(file
                .changelog_url
                .clone()
                .map(|u| format!("Changelog: {u}"))),
            Err(e) => Err(e),
        }
    }
}

/// `POST /v1/mods {"modIds":[..]}` — project records for scan matches, chunked.
async fn batch_mods(ctx: &AppCtx, ids: &[i64]) -> Result<HashMap<i64, Mod>> {
    let mut out = HashMap::new();
    for chunk in ids.chunks(MODS_BATCH) {
        if chunk.is_empty() {
            continue;
        }
        let resp: ModsResp = post(ctx, "/v1/mods", &json!({ "modIds": chunk })).await?;
        for m in resp.data {
            if m.id != 0 {
                out.insert(m.id, m);
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use std::sync::Arc;

    fn ctx() -> AppCtx {
        AppCtx {
            http: net::build_client("fa-test"),
            cfg: Arc::new(AppConfig::default()),
            app_version: "test",
        }
    }

    fn sel() -> FileSelect {
        FileSelect {
            flavor: Flavor::Forever,
            interface: 16001,
            version: "1.60.1".into(),
            channel: Channel::Alpha,
        }
    }

    const SEARCH_JSON: &str = r#"{
      "pagination": {"index":0,"pageSize":2,"resultCount":2,"totalCount":45},
      "data": [
        {"id":2382,"name":"BigWigs (Boss Timers & Tools)","slug":"bigwigs",
         "summary":"A modular and extremely lightweight boss boss mod.",
         "downloadCount":208881290,"dateModified":"2026-11-05T17:44:15.263Z",
         "links":{"websiteUrl":"https://www.curseforge.com/wow/addons/bigwigs",
                  "sourceUrl":"https://github.com/BigWigsMods/BigWigs",
                  "issuesUrl":"https://github.com/BigWigsMods/BigWigs/issues"},
         "authors":[{"id":25,"name":"Funkeh"},{"name":"nebula169"}],
         "logo":{"id":854443,"thumbnailUrl":"https://media.forgecdn.net/avatars/thumbnails/854/443/a.jpeg",
                 "url":"https://media.forgecdn.net/avatars/854/443/a.jpeg"},
         "latestFiles":[{"id":9299959,"displayName":"v427.8","fileName":"BigWigs-v427.8.zip","releaseType":1}]},
        {"id":7,"name":"Details!","slug":"details","summary":"dmg","downloadCount":5,
         "authors":[],"links":{},"logo":{},"latestFiles":[],"isAvailable":true}
      ]}"#;

    #[test]
    fn search_fixture() {
        let resp: SearchResp = serde_json::from_str(SEARCH_JSON).expect("parses");
        let page_items: Vec<SearchItem> = resp.data.iter().map(search_item).collect();
        assert_eq!(page_items.len(), 2);
        let bw = &page_items[0];
        assert_eq!(bw.provider, ProviderKind::Curse);
        assert_eq!(bw.external_id, "2382");
        assert_eq!(bw.name, "BigWigs (Boss Timers & Tools)");
        assert_eq!(bw.author.as_deref(), Some("Funkeh"));
        assert_eq!(bw.downloads, Some(208_881_290));
        assert_eq!(
            bw.site_url.as_deref(),
            Some("https://www.curseforge.com/wow/addons/bigwigs")
        );
        assert_eq!(
            bw.icon_url.as_deref(),
            Some("https://media.forgecdn.net/avatars/thumbnails/854/443/a.jpeg")
        );
        assert_eq!(bw.latest_version.as_deref(), Some("v427.8"));
        assert_eq!(bw.updated_at, Some(1_793_900_655)); // 2026-11-05T17:44:15Z
        assert_eq!(resp.pagination.total_count, 45);

        // a mod with no authors/logo/links degrades to None, no panic
        let bare = &page_items[1];
        assert_eq!(bare.external_id, "7");
        assert!(bare.author.is_none() && bare.icon_url.is_none());
    }

    const MOD_JSON: &str = r#"{
      "data":{"id":2382,"name":"BigWigs","slug":"bigwigs","summary":"s","downloadCount":10,
        "authors":[{"name":"Funkeh"}],"links":{"websiteUrl":"https://www.curseforge.com/wow/addons/bigwigs"},
        "logo":{"thumbnailUrls":[{"url":"https://media.forgecdn.net/t/1.png"},{"url":""}]},"latestFiles":[],"isAvailable":true}}"#;

    const FILES_JSON: &str = r#"{
      "pagination":{"index":0,"pageSize":50,"resultCount":3,"totalCount":113},
      "data":[
        {"id":9456619,"modId":2382,"isAvailable":true,
         "displayName":"v427.8-2-g6eb1eac","fileName":"BigWigs-v427.8-2-g6eb1eac.zip","releaseType":3,
         "fileDate":"2026-11-05T17:43:04.58Z","fileLength":4637215,
         "downloadUrl":"https://edge.forgecdn.net/files/9456/619/BigWigs.zip",
         "hashes":[{"value":"503fcf21cb4728c9fc6a1bc2950826b7ffe89bad","algo":1},
                   {"value":"df51dbe6bdd52e5527fb97b01ccbdf85","algo":2}],
         "modules":[{"name":"BigWigs","fingerprint":3437473617}],
         "sortableGameVersions":[{"gameVersionTypeId":88568,"gameVersion":"1.60.1"},
                                 {"gameVersionTypeId":517,"gameVersion":"12.1.7"},
                                 {"gameVersionTypeId":99999,"gameVersion":"??"}]},
        {"id":9299959,"modId":2382,"isAvailable":true,"displayName":"v427.8","fileName":"BigWigs-v427.8.zip",
         "releaseType":1,"fileDate":"2026-10-23T16:33:14.003Z","fileLength":4635969,
         "hashes":[{"value":"d994af3bf3874df818e577909ff41296","algo":2}],
         "modules":[{"name":"BigWigs","fingerprint":3793833344}],
         "sortableGameVersions":[{"gameVersionTypeId":73713,"gameVersion":"3.4.5"},
                                 {"gameVersionTypeId":88568,"gameVersion":"1.60.1"}]},
        {"id":111,"modId":2382,"isAvailable":false,"displayName":"gone","fileName":"gone.zip","releaseType":2,
         "sortableGameVersions":[]}
      ]}"#;

    #[test]
    fn index_squatter_files_are_never_installable() {
        // Live incident (Nov 2026): the Forever search index lists mod 1774863
        // as "QuestieDB" (a stale name; the project was renamed to a
        // Supplementaries Squared file). Its files are tagged only with game
        // version type ids no client maps (73407, 88556 = Minecraft-era), the
        // flavor-filtered files endpoint returns nothing, and the unfiltered
        // retry returns them. "Tagged, nothing we know" must mean "incompatible
        // for us", never "compatible with any".
        let c = ctx();
        let m: ModResp = serde_json::from_str(MOD_JSON).expect("mod parses");
        const SQUAT: &str = r#"{
          "pagination":{"index":0,"pageSize":50,"resultCount":1,"totalCount":1},
          "data":[{"id":9386328,"modId":2382,"isAvailable":true,
                   "displayName":"Supplementaries Squared v1","fileName":"ss.zip","releaseType":1,
                   "fileDate":"2026-11-01T00:00:00.000Z","fileLength":1,
                   "downloadUrl":"https://edge.forgecdn.net/files/9386/328/ss.zip",
                   "sortableGameVersions":[{"gameVersionTypeId":73407,"gameVersion":"26.1"},
                                           {"gameVersionTypeId":88556,"gameVersion":"26.3"}]}]}"#;
        let fr: FilesResp = serde_json::from_str(SQUAT).expect("files parse");
        let files: Vec<RemoteFile> = normalize_files(fr.data)
            .into_iter()
            .map(|f| remote_file(&c, &m.data, &f))
            .collect();
        assert_eq!(files.len(), 1);
        assert!(files[0].flavors.is_empty(), "unmapped type ids drop out");
        assert!(files[0].flavor_meta, "tagged by the API: not compatible-any");
        let meta = meta_shell(&m.data, files);
        assert!(
            crate::types::pick_generic(&meta, &sel()).is_none(),
            "a file tagged for foreign clients must never be picked"
        );
    }

    #[test]
    fn meta_and_files_fixture() {
        let c = ctx();
        let m: ModResp = serde_json::from_str(MOD_JSON).expect("mod parses");
        let meta = meta_shell(&m.data, Vec::new());
        assert_eq!(meta.provider, ProviderKind::Curse);
        assert_eq!(meta.external_id, "2382");
        assert_eq!(
            meta.icon_url.as_deref(),
            Some("https://media.forgecdn.net/t/1.png")
        ); // thumbnailUrls[] shape
        assert_eq!(meta.repo_url, None);

        let fr: FilesResp = serde_json::from_str(FILES_JSON).expect("files parse");
        let files: Vec<RemoteFile> = normalize_files(fr.data)
            .into_iter()
            .map(|f| remote_file(&c, &m.data, &f))
            .collect();
        assert_eq!(files.len(), 2, "isAvailable:false filtered out");
        assert_eq!(files[0].file_id, "9456619", "newest first");
        assert_eq!(files[0].version, "v427.8-2-g6eb1eac");
        assert_eq!(files[0].channel, Channel::Alpha);
        assert_eq!(
            files[0].flavors,
            vec![Flavor::Forever, Flavor::Retail],
            "typeIds mapped, ordered, unknown dropped"
        );
        assert_eq!(
            files[0].url,
            "https://edge.forgecdn.net/files/9456/619/BigWigs.zip"
        );
        assert_eq!(
            files[0].headers.get("x-api-key").map(String::as_str),
            Some(AppConfig::default().network.curse_api_key.as_str())
        );
        assert_eq!(
            files[0].sha1.as_deref(),
            Some("503fcf21cb4728c9fc6a1bc2950826b7ffe89bad")
        );
        assert_eq!(
            files[0].md5.as_deref(),
            Some("df51dbe6bdd52e5527fb97b01ccbdf85")
        );
        assert_eq!(files[0].size, Some(4_637_215));
        assert_eq!(files[0].date, Some(1_793_900_584)); // ISO fileDate -> seconds
        assert_eq!(
            files[0].changelog_url.as_deref(),
            Some("https://www.curseforge.com/wow/addons/bigwigs/changelog")
        );
        assert_eq!(files[1].channel, Channel::Stable);
        assert_eq!(files[1].flavors, vec![Flavor::Forever, Flavor::Wotlk]);
        assert_eq!(
            files[1].url, "",
            "no downloadUrl field => empty until resolved"
        );

        // generic picker honours flavor + channel
        let picked = crate::types::pick_generic(
            &meta_shell(&m.data, files.clone()),
            &FileSelect {
                channel: Channel::Stable,
                ..sel()
            },
        )
        .expect("a Forever-compatible stable file");
        assert_eq!(picked.file_id, "9299959");
    }

    const FP_JSON: &str = r#"{
      "data":{"isCacheBuilt":true,
        "exactFingerprints":[1111111111],
        "exactMatches":[{"id":1756403,
          "file":{"id":9276708,"modId":1756403,"displayName":"3.2.12","fileName":"g.zip","releaseType":1,
                  "isAvailable":true,"downloadUrl":"https://edge.forgecdn.net/files/2135/562/g.zip",
                  "fileDate":"2026-10-21T12:14:13.213Z","fileLength":14360019,
                  "hashes":[{"value":"1c6f5f5f8b5c1d2d6c45fc7a8cc3aa99","algo":2}],
                  "modules":[{"name":"AzerothForgeGearscore","fingerprint":1111111111}],
                  "sortableGameVersions":[{"gameVersionTypeId":88568,"gameVersion":"1.60.1"}]},
          "latestFiles":[]}],
        "partialMatches":[{"id":2382,
          "file":{"id":9299959,"modId":2382,"displayName":"v427.8","fileName":"BigWigs-v427.8.zip","releaseType":1,
                  "isAvailable":true,"downloadUrl":"https://edge.forgecdn.net/files/9299/959/BigWigs-v427.8.zip",
                  "fileDate":"2026-10-23T16:33:14.003Z",
                  "modules":[{"name":"BigWigs","fingerprint":3793833344},{"name":"BigWigs_Options","fingerprint":3740840902}]},
          "latestFiles":[{"id":9377832,"modId":2382,"displayName":"v427.9","fileName":"bw2.zip","releaseType":1,"fileDate":"2026-11-01T00:00:00.000Z"}]}],
        "partialMatchFingerprints":{"2382":[3793833344]},
        "unmatchedFingerprints":null}}"#;

    #[test]
    fn fingerprint_scan_fixture() {
        let c = ctx();
        let r: FingerprintResp =
            serde_json::from_str(FP_JSON).expect("fingerprint response parses");
        // `unmatchedFingerprints: null` + the extra cache fields parse fine
        assert_eq!(r.data.unmatched_fingerprints, None);
        assert_eq!(r.data.exact_matches.len(), 1);
        assert_eq!(r.data.partial_matches.len(), 1);
        assert_eq!(r.data.exact_fingerprints, vec![1_111_111_111_i64]);

        let mut folders: BTreeMap<u32, Vec<String>> = BTreeMap::new();
        folders.insert(1_111_111_111, vec!["AzerothForgeGearscore".into()]);
        folders.insert(3_793_833_344, vec!["BigWigs".into()]);
        folders.insert(3_740_840_902, vec!["BigWigs_Options".into()]);
        folders.insert(999, vec!["UnknownThing".into()]);

        let hits = fingerprint_hits(&r.data, &folders);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].mod_id, 1756403, "exact matches first");
        assert_eq!(hits[0].folders, vec!["AzerothForgeGearscore".to_string()]);
        assert_eq!(hits[1].mod_id, 2382);
        assert_eq!(
            hits[1].folders,
            vec!["BigWigs".to_string(), "BigWigs_Options".to_string()]
        );

        // meta built from file + latestFiles
        let shell = Mod::default();
        let files: Vec<RemoteFile> = normalize_files(hits[1].files.clone())
            .into_iter()
            .map(|f| remote_file(&c, &shell, &f))
            .collect();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].file_id, "9377832", "latestFiles entry is newer");
        assert_eq!(
            files[1].url,
            "https://edge.forgecdn.net/files/9299/959/BigWigs-v427.8.zip"
        );

        // a folder claimed by an exact match is never re-claimed by a partial one
        let dup = r#"{"data":{"exactMatches":[{"id":1,"file":{"id":2,"modId":1,"modules":[{"name":"X","fingerprint":3793833344}]}}],
            "partialMatches":[{"id":2,"file":{"id":3,"modId":2,"modules":[{"name":"X","fingerprint":3793833344}]}}],
            "partialMatchFingerprints":{"2":[3793833344]},"unmatchedFingerprints":null}}"#;
        let d: FingerprintResp = serde_json::from_str(dup).expect("dup fixture");
        let mut only: BTreeMap<u32, Vec<String>> = BTreeMap::new();
        only.insert(3_793_833_344, vec!["X".into()]);
        let h = fingerprint_hits(&d.data, &only);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].mod_id, 1);
        assert_eq!(h[0].folders, vec!["X".to_string()]);

        // fallback path: matched file without modules[] uses partialMatchFingerprints
        const NO_MODULES: &str = r#"{"data":{"partialMatches":[{"id":2382,"file":{"id":9299959,"modId":2382,"modules":[]}}],
            "partialMatchFingerprints":{"2382":[3793833344,999]}}}"#;
        let n: FingerprintResp = serde_json::from_str(NO_MODULES).expect("no-modules fixture");
        let h = fingerprint_hits(&n.data, &only);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].folders, vec!["X".to_string()]);
    }

    #[test]
    fn malformed_json_degrades_gracefully() {
        // missing everything
        let s: SearchResp = serde_json::from_str("{}").expect("empty object parses");
        assert!(s.data.is_empty());
        assert_eq!(s.pagination.total_count, 0);
        let f: FilesResp = serde_json::from_str(r#"{"data":null}"#).expect("null data parses");
        assert!(f.data.is_empty());
        let m: ModResp = serde_json::from_str(r#"{"data":{"id":5,"name":123,"logo":"x"}}"#)
            .expect("wrong scalar types fall back to defaults");
        assert_eq!(m.data.id, 5);
        assert_eq!(m.data.name, "123", "numbers are stringified, not rejected");
        assert!(m.data.links.website_url.is_empty() && m.data.logo.best().is_none());
        let item = search_item(&m.data);
        assert_eq!(item.external_id, "5");
        assert!(item.icon_url.is_none());
        // broken json is an Err, never a panic
        assert!(serde_json::from_str::<SearchResp>("{\"data\":[{").is_err());
        // structurally wrong bodies parse to empty results (lenient fields)
        let empty_fp: FingerprintResp = serde_json::from_str("[]").expect("array body tolerated");
        assert!(empty_fp.data.exact_matches.is_empty() && empty_fp.data.partial_matches.is_empty());

        let c = ctx();
        let fr: FilesResp =
            serde_json::from_str(r#"{"data":[{"id":9,"modId":9,"fileDate":"garbage","hashes":[{"algo":7,"value":1}],"sortableGameVersions":[{"gameVersionTypeId":"88568"}]}]}"#)
                .expect("odd types parse");
        let files: Vec<RemoteFile> = fr
            .data
            .iter()
            .map(|f| remote_file(&c, &Mod::default(), f))
            .collect();
        assert_eq!(files[0].date, None, "unparsable date => None");
        assert!(files[0].md5.is_none() && files[0].sha1.is_none());
        assert_eq!(
            files[0].flavors,
            vec![Flavor::Forever],
            "numeric strings in typeId position are tolerated"
        );
        assert_eq!(files[0].version, "9", "no displayName => file id fallback");
    }

    #[test]
    fn date_and_flavor_helpers() {
        assert_eq!(
            unix_seconds(&json!("2026-10-21T12:14:13.213Z")),
            Some(1_792_584_853)
        );
        assert_eq!(unix_seconds(&json!("2026-10-21")), Some(1_792_540_800));
        assert_eq!(
            unix_seconds(&json!(1_724_089_877_000_i64)),
            Some(1_724_089_877),
            "ms => s"
        );
        assert_eq!(unix_seconds(&json!(1_724_089_877_i64)), Some(1_724_089_877));
        assert_eq!(unix_seconds(&json!("1724089877")), Some(1_724_089_877));
        assert_eq!(unix_seconds(&Value::Null), None);
        assert_eq!(unix_seconds(&json!("")), None);
        assert_eq!(flavor_of_type_id(88568), Some(Flavor::Forever));
        assert_eq!(flavor_of_type_id(517), Some(Flavor::Retail));
        assert_eq!(flavor_of_type_id(1), None);
        assert_eq!(channel_of(1), Channel::Stable);
        assert_eq!(channel_of(2), Channel::Beta);
        assert_eq!(channel_of(3), Channel::Alpha);
        assert!(is_not_found(&Error::provider(
            "CurseForge",
            "GET https://api.curseforge.com/v1/mods/9 -> 404 Not Found: nope"
        )));
        assert!(!is_not_found(&Error::provider("CurseForge", "-> 500")));
    }

    #[test]
    fn url_building_encodes_query() {
        let u = url(
            "/v1/mods/search",
            &[("gameId", "1".into()), ("searchFilter", "d&d test".into())],
        )
        .unwrap();
        assert!(
            u.starts_with("https://api.curseforge.com/v1/mods/search?"),
            "{u}"
        );
        assert!(
            u.contains("searchFilter=d%26d+test") || u.contains("searchFilter=d%26d%20test"),
            "{u}"
        );
        assert!(net::ensure_allowed(&u, &[]).is_ok());
    }

    fn live() -> bool {
        std::env::var("FA_LIVE").map(|v| v == "1").unwrap_or(false)
    }

    #[tokio::test]
    #[ignore = "live CurseForge API (FA_LIVE=1)"]
    async fn live_search_meta_fingerprint() {
        if !live() {
            return;
        }
        let c = ctx();
        let s = CurseProvider::new();
        let page = s
            .search(&c, "details", 0, 10, &sel())
            .await
            .expect("search ok");
        assert!(!page.items.is_empty(), "search 'details' returned nothing");
        assert!(page.total > 0);
        assert!(page.items.iter().any(|i| !i.name.trim().is_empty()));

        let metas = s.meta(&c, &["2382".into()]).await.expect("meta ok");
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].external_id, "2382");
        assert!(metas[0].name.contains("BigWigs"), "name: {}", metas[0].name);
        assert!(
            !metas[0].files.is_empty(),
            "BigWigs files must not be empty"
        );
        let f = &metas[0].files[0];
        assert!(!f.file_id.is_empty());
        assert_eq!(
            f.headers.get("x-api-key").expect("cdn needs the key"),
            &c.cfg.network.curse_api_key
        );
        assert!(f.date.unwrap_or(0) > 1_700_000_000, "date: {:?}", f.date);
        let log = s.changelog(&c, &metas[0], f).await.expect("changelog ok");
        assert!(log.is_some(), "changelog body expected");

        // fingerprint scan: a real BigWigs folder fingerprint => mod 2382
        let resp: FingerprintResp = post(
            &c,
            "/v1/fingerprints",
            &json!({"fingerprints": [3793833344_u32]}),
        )
        .await
        .expect("fingerprint post");
        let mut folders: BTreeMap<u32, Vec<String>> = BTreeMap::new();
        folders.insert(3_793_833_344, vec!["BigWigs".into()]);
        let hits = fingerprint_hits(&resp.data, &folders);
        assert!(
            hits.iter().any(|h| h.mod_id == 2382),
            "expected mod 2382, got {hits:?}"
        );
        assert_eq!(
            hits.iter().find(|h| h.mod_id == 2382).unwrap().folders,
            vec!["BigWigs".to_string()]
        );
    }

    #[tokio::test]
    #[ignore = "live CurseForge API (FA_LIVE=1)"]
    async fn live_forever_index_squatter_never_picks() {
        // Curse mod 1774863 appears in the Forever search index as "QuestieDB"
        // (stale name from before a renames-as-recycling spam wave; the real
        // project is a supplement file tagged only for unmapped game version
        // ids). An install resolved to it must refuse: no compatible file.
        if !live() {
            return;
        }
        let c = ctx();
        let s = CurseProvider::new();
        let metas = s
            .meta(&c, &["1774863".into()])
            .await
            .expect("meta ok (may be an empty file list)");
        let meta = metas.into_iter().next().expect("one meta");
        assert!(
            crate::types::pick_generic(&meta, &sel()).is_none(),
            "squatter files (unmapped flavor tags) must not be picked: {:?}",
            meta.files
                .iter()
                .map(|f| (&f.file_id, &f.flavors, f.flavor_meta))
                .collect::<Vec<_>>()
        );
    }
}
