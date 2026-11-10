//! MMOUI / WoWInterface provider.
//!
//! Live-verified facts (2026-11-06):
//! - v4 batch: `GET https://api.mmoui.com/v4/game/WOW/filedetails/{ids}.json`
//!   (comma joined) returns a JSON **array**; ids it has no record for are simply
//!   absent, and if none resolve the body is `{"ERROR":"No AddOn found."}`;
//! - v4 has holes, so every missing id falls back to the v3 endpoint
//!   `GET https://api.mmoui.com/v3/game/WOW/filedetails/{id}.json` (array of one
//!   `UID/…` record, `UIDate` in unix **milliseconds**, `UIHitCount` a string);
//! - the provider publishes no per-file flavor info (`flavors` stays empty) and
//!   has **no search endpoint** — addons are matched by `## X-WoWI-ID:`.

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use crate::error::{Error, Result};
use crate::net;
use crate::types::{
    AppCtx, FileSelect, FolderInfo, Provider, ProviderKind, RemoteFile, RemoteMeta, ScanMatch,
    ScanRequest, SearchPage,
};

const V4_BASE: &str = "https://api.mmoui.com/v4/game/WOW/filedetails";
const V3_BASE: &str = "https://api.mmoui.com/v3/game/WOW/filedetails";
/// Ids per batched v4 request.
const BATCH: usize = 25;

fn non_empty(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// Unix seconds from a provider date value (ms/s numbers, numeric strings, ISO).
fn unix_seconds(v: &Value) -> Option<i64> {
    match v {
        Value::Null => None,
        Value::String(s) => {
            let s = s.trim();
            match s.parse::<i64>() {
                Ok(n) => Some(crate::providers::curse::normalize_epoch(n)),
                Err(_) => crate::providers::curse::parse_iso(s),
            }
        }
        _ => v.as_i64().map(crate::providers::curse::normalize_epoch),
    }
}

fn file_id_of(v: &Value) -> Option<String> {
    match v {
        Value::Number(n) => n.as_i64().filter(|i| *i > 0).map(|i| i.to_string()),
        Value::String(s) => {
            let s = s.trim().to_string();
            (!s.is_empty() && s.bytes().all(|c| c.is_ascii_digit())).then_some(s)
        }
        _ => None,
    }
}

fn site_url(id: &str) -> String {
    format!("https://www.wowinterface.com/downloads/info{id}.html")
}

// ---------------------------------------------------------------------------
// API shapes
// ---------------------------------------------------------------------------

/// v4 record: `{id,title,author,version,downloadUri,changeLog,downloads,lastUpdate,checksum,fileName}`.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
#[allow(dead_code)]
struct V4File {
    #[serde(default)]
    id: Value,
    #[serde(deserialize_with = "crate::providers::curse::loose::string", default)]
    title: String,
    #[serde(deserialize_with = "crate::providers::curse::loose::string", default)]
    author: String,
    #[serde(deserialize_with = "crate::providers::curse::loose::string", default)]
    version: String,
    #[serde(deserialize_with = "crate::providers::curse::loose::string", default)]
    download_uri: String,
    #[serde(default)]
    change_log: Value,
    #[serde(default)]
    downloads: Value,
    #[serde(default)]
    last_update: Value,
    #[serde(deserialize_with = "crate::providers::curse::loose::string", default)]
    checksum: String,
    #[serde(deserialize_with = "crate::providers::curse::loose::string", default)]
    file_name: String,
    #[serde(deserialize_with = "crate::providers::curse::loose::string", default)]
    description: String,
}

/// v3 record: `{UID,UIName,UIAuthorName,UIVersion,UIDownload,UIFileName,UIHitCount,UIDate,UIMD5,…}`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
#[allow(dead_code)]
struct V3File {
    #[serde(rename = "UID", default)]
    uid: Value,
    #[serde(
        rename = "UIName",
        deserialize_with = "crate::providers::curse::loose::string",
        default
    )]
    ui_name: String,
    #[serde(
        rename = "UIAuthorName",
        deserialize_with = "crate::providers::curse::loose::string",
        default
    )]
    ui_author_name: String,
    #[serde(
        rename = "UIVersion",
        deserialize_with = "crate::providers::curse::loose::string",
        default
    )]
    ui_version: String,
    #[serde(
        rename = "UIDownload",
        deserialize_with = "crate::providers::curse::loose::string",
        default
    )]
    ui_download: String,
    #[serde(
        rename = "UIFileName",
        deserialize_with = "crate::providers::curse::loose::string",
        default
    )]
    ui_file_name: String,
    #[serde(rename = "UIHitCount", default)]
    ui_hit_count: Value,
    #[serde(rename = "UIDate", default)]
    ui_date: Value,
    #[serde(
        rename = "UIMD5",
        deserialize_with = "crate::providers::curse::loose::string",
        default
    )]
    ui_md5: String,
    #[serde(
        rename = "UIChangeLog",
        deserialize_with = "crate::providers::curse::loose::string",
        default
    )]
    ui_change_log: String,
    #[serde(
        rename = "UIDescription",
        deserialize_with = "crate::providers::curse::loose::string",
        default
    )]
    ui_description: String,
}

fn build_meta(
    id: &str,
    name: String,
    author: Option<String>,
    description: Option<String>,
    downloads: Option<i64>,
    updated_at: Option<i64>,
    file: RemoteFile,
) -> RemoteMeta {
    RemoteMeta {
        provider: ProviderKind::WowInterface,
        external_id: id.to_string(),
        name,
        author,
        site_url: Some(site_url(id)),
        repo_url: None,
        description,
        icon_url: None,
        downloads,
        updated_at,
        files: vec![file],
    }
}

fn remote_file(
    id: &str,
    version: &str,
    url: &str,
    file_name: &str,
    md5: &str,
    date: Option<i64>,
    changelog: Option<String>,
) -> RemoteFile {
    RemoteFile {
        file_id: id.to_string(),
        version: version.to_string(),
        channel: crate::types::Channel::Stable,
        file_name: file_name.to_string(),
        url: url.to_string(),
        headers: Default::default(),
        flavors: Vec::new(), // the API publishes no flavor info
        flavor_meta: false,
        date,
        size: None,
        md5: non_empty(md5),
        sha1: None,
        changelog_url: changelog,
    }
}

fn v4_meta(f: &V4File) -> Option<RemoteMeta> {
    let id = file_id_of(&f.id)?;
    let file = remote_file(
        &id,
        f.version.trim(),
        f.download_uri.trim(),
        &f.file_name,
        &f.checksum,
        unix_seconds(&f.last_update),
        f.change_log.as_str().and_then(non_empty),
    );
    Some(build_meta(
        &id,
        non_empty(&f.title).unwrap_or_else(|| format!("WoWInterface file {id}")),
        non_empty(&f.author),
        non_empty(&f.description),
        f.downloads
            .as_i64()
            .or_else(|| f.downloads.as_str().and_then(|s| s.trim().parse().ok())),
        unix_seconds(&f.last_update),
        file,
    ))
}

fn v3_meta(f: &V3File) -> Option<RemoteMeta> {
    let id = file_id_of(&f.uid)?;
    let file = remote_file(
        &id,
        f.ui_version.trim(),
        f.ui_download.trim(),
        &f.ui_file_name,
        &f.ui_md5,
        unix_seconds(&f.ui_date),
        non_empty(&f.ui_change_log),
    );
    Some(build_meta(
        &id,
        non_empty(&f.ui_name).unwrap_or_else(|| format!("WoWInterface file {id}")),
        non_empty(&f.ui_author_name),
        non_empty(&f.ui_description),
        f.ui_hit_count
            .as_i64()
            .or_else(|| f.ui_hit_count.as_str().and_then(|s| s.trim().parse().ok())),
        unix_seconds(&f.ui_date),
        file,
    ))
}

/// v4 bodies are arrays; `{"ERROR":…}` bodies (and anything else) are "no records".
fn parse_v4(body: &Value) -> Vec<V4File> {
    match serde_json::from_value::<Vec<V4File>>(body.clone()) {
        Ok(v) => v,
        Err(e) => {
            let snippet: String = body.to_string().chars().take(160).collect();
            tracing::debug!("mmoui v4 body not a record array ({e}): {snippet}");
            Vec::new()
        }
    }
}

fn parse_v3(body: &Value) -> Vec<V3File> {
    match serde_json::from_value::<Vec<V3File>>(body.clone()) {
        Ok(v) => v,
        Err(e) => {
            let snippet: String = body.to_string().chars().take(160).collect();
            tracing::debug!("mmoui v3 body not a record array ({e}): {snippet}");
            Vec::new()
        }
    }
}

/// Numeric ids only (the endpoints 404 on anything else); deduplicated, in order.
fn clean_ids(ids: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for raw in ids {
        let id = raw.trim();
        if id.is_empty() || !id.bytes().all(|c| c.is_ascii_digit()) {
            tracing::debug!("mmoui: ignoring non-numeric file id {raw:?}");
            continue;
        }
        if seen.insert(id.to_string()) {
            out.push(id.to_string());
        }
    }
    out
}

/// Which ids are missing from a v4 batch answer (they get the v3 fallback).
fn missing_ids(all: &[String], found: &BTreeMap<String, RemoteMeta>) -> Vec<String> {
    all.iter()
        .filter(|id| !found.contains_key(id.as_str()))
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct MmouiProvider;

impl MmouiProvider {
    pub fn new() -> Self {
        MmouiProvider
    }
}

#[async_trait]
impl Provider for MmouiProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::WowInterface
    }

    async fn meta(&self, ctx: &AppCtx, ids: &[String]) -> Result<Vec<RemoteMeta>> {
        ctx.ensure_online()?;
        let wanted = clean_ids(ids);
        let mut found: BTreeMap<String, RemoteMeta> = BTreeMap::new();

        // v4, batched; holes are expected so failures are not fatal
        for chunk in wanted.chunks(BATCH) {
            let url = format!("{V4_BASE}/{}.json", chunk.join(","));
            match net::get_json::<Value>(&ctx.http, &url, &[]).await {
                Ok(body) => {
                    for rec in parse_v4(&body) {
                        if let Some(m) = v4_meta(&rec) {
                            found.insert(m.external_id.clone(), m);
                        }
                    }
                }
                Err(e) => {
                    tracing::debug!("mmoui v4 batch failed ({url}): {e}");
                }
            }
        }

        // v3 per-id fallback
        for id in missing_ids(&wanted, &found) {
            let url = format!("{V3_BASE}/{id}.json");
            match net::get_json::<Value>(&ctx.http, &url, &[]).await {
                Ok(body) => {
                    if let Some(rec) = parse_v3(&body).into_iter().find_map(|f| v3_meta(&f)) {
                        found.insert(id.clone(), rec);
                    }
                }
                Err(e) if is_not_found(&e) => {
                    tracing::debug!("mmoui: file id {id} not found (v4 and v3)");
                }
                Err(e) => return Err(e),
            }
        }

        Ok(wanted
            .into_iter()
            .filter_map(|id| found.remove(id.as_str()))
            .collect())
    }

    async fn scan(&self, ctx: &AppCtx, req: &ScanRequest<'_>) -> Result<Vec<ScanMatch>> {
        ctx.ensure_online()?;
        let mut by_id = wowi_folders(req.folders);
        if by_id.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<String> = by_id.keys().cloned().collect();
        let metas = self.meta(ctx, &ids).await?;
        let mut out = Vec::new();
        for meta in metas {
            let Some(folders) = by_id.remove(&meta.external_id) else {
                continue;
            };
            let installed_version = req
                .folders
                .iter()
                .find(|f| folders.iter().any(|n| n.eq_ignore_ascii_case(&f.name)))
                .and_then(|f| f.toc.version.clone());
            out.push(ScanMatch {
                provider: ProviderKind::WowInterface,
                external_id: meta.external_id.clone(),
                folders,
                meta,
                installed_version,
            });
        }
        Ok(out)
    }

    async fn search(
        &self,
        ctx: &AppCtx,
        _query: &str,
        _page: u32,
        _per_page: u32,
        _sel: &FileSelect,
    ) -> Result<SearchPage> {
        ctx.ensure_online()?;
        Err(Error::Unsupported(
            "MMOUI/WoWInterface has no search API. Install by pasting a wowinterface.com \
             file id (e.g. 8814) or download URL; addons declaring ## X-WoWI-ID: are \
             detected by the folder scan."
                .to_string(),
        ))
    }

    async fn changelog(
        &self,
        ctx: &AppCtx,
        _meta: &RemoteMeta,
        file: &RemoteFile,
    ) -> Result<Option<String>> {
        ctx.ensure_online()?;
        // MMOUI ships the changelog text (not a url) inside the file record
        match file.changelog_url.as_deref().map(str::trim) {
            Some(t) if !t.is_empty() && !t.starts_with("http") => Ok(Some(t.to_string())),
            other => Ok(other.map(|u| format!("Changelog: {u}"))),
        }
    }
}

fn is_not_found(e: &Error) -> bool {
    matches!(e, Error::Provider { message, .. } if message.contains("-> 404"))
}

/// Folder names declaring `## X-WoWI-ID:` (pure part of scan, unit-tested).
fn wowi_folders(folders: &[FolderInfo]) -> BTreeMap<String, Vec<String>> {
    let mut by_id: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for f in folders {
        if let Some(id) = f
            .toc
            .ids
            .wowi
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
    by_id
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::toc::ParsedToc;
    use std::sync::Arc;

    /// Real v4 record (api.mmoui.com/v4/game/WOW/filedetails/3826.json).
    const V4_JSON: &str = r#"[{
      "id": 3826, "categoryId": 19, "version": "11.0.2.3", "lastUpdate": 1724089877000,
      "checksum": "801d2daf73ad195c02c58d50be7f1b22", "fileName": "CTMod 11.0.2.3c.zip",
      "downloadUri": "https://cdn.wowinterface.com/downloads/getfile.php?id=3826&d=1724089877&minion",
      "pendingUpdate": 0, "title": "CTMod", "author": "DahkCeles",
      "description": "CTMod is one of the longest-running collections of addons.",
      "changeLog": "CTMod 11.0.2.3 (2024-08-19): further updates for The War Within",
      "downloads": 598626, "downloadsMonthly": 38, "favorites": 695, "images": []
    }]"#;

    /// Real v3 record (api.mmoui.com/v3/game/WOW/filedetails/8814.json — DBM).
    const V3_JSON: &str = r#"[{
      "UID": "8814", "UICATID": "45", "UIVersion": "11.2.15", "UIDate": 1757932228000,
      "UIMD5": "e81dc2d50729669d0b7ff8a3f4ab4db1", "UIFileName": "DBM-Core-11.2.15.zip",
      "UIDownload": "https://cdn.wowinterface.com/downloads/getfile.php?id=8814&d=1757932228&minion",
      "UIPending": "0", "UIName": "Deadly Boss Mods", "UIAuthorName": "MysticalOS",
      "UIDescription": "The ultimate encounter helper.",
      "UIChangeLog": "[size=5]DBM - Core[/size]\r\n[size=4]11.2.15[/size]",
      "UIHitCount": "2996133", "UIHitCountMonthly": "100", "UIFavoriteTotal": "5"
    }]"#;

    #[test]
    fn parses_v4_record() {
        let body: Value = serde_json::from_str(V4_JSON).unwrap();
        let recs = parse_v4(&body);
        assert_eq!(recs.len(), 1);
        let m = v4_meta(&recs[0]).expect("v4 record maps");
        assert_eq!(m.provider, ProviderKind::WowInterface);
        assert_eq!(m.external_id, "3826", "file id is the external id");
        assert_eq!(m.name, "CTMod");
        assert_eq!(m.author.as_deref(), Some("DahkCeles"));
        assert_eq!(m.downloads, Some(598_626));
        assert_eq!(
            m.site_url.as_deref(),
            Some("https://www.wowinterface.com/downloads/info3826.html")
        );
        assert_eq!(m.updated_at, Some(1_724_089_877), "unix ms => seconds");
        assert_eq!(m.files.len(), 1);
        let f = &m.files[0];
        assert_eq!(f.file_id, "3826");
        assert_eq!(f.version, "11.0.2.3");
        assert_eq!(f.file_name, "CTMod 11.0.2.3c.zip");
        assert_eq!(
            f.url,
            "https://cdn.wowinterface.com/downloads/getfile.php?id=3826&d=1724089877&minion"
        );
        assert!(net::host_allowed(&f.url), "cdn host is on the allowlist");
        assert_eq!(f.md5.as_deref(), Some("801d2daf73ad195c02c58d50be7f1b22"));
        assert_eq!(f.date, Some(1_724_089_877));
        assert!(
            f.flavors.is_empty(),
            "the provider publishes no flavor info"
        );
        assert!(f.headers.is_empty());
    }

    #[test]
    fn parses_v3_record() {
        let body: Value = serde_json::from_str(V3_JSON).unwrap();
        let recs = parse_v3(&body);
        assert_eq!(recs.len(), 1);
        let m = v3_meta(&recs[0]).expect("v3 record maps");
        assert_eq!(m.external_id, "8814");
        assert_eq!(m.name, "Deadly Boss Mods");
        assert_eq!(m.author.as_deref(), Some("MysticalOS"));
        assert_eq!(m.downloads, Some(2_996_133), "UIHitCount is a string");
        assert_eq!(
            m.description.as_deref(),
            Some("The ultimate encounter helper.")
        );
        let f = &m.files[0];
        assert_eq!(f.file_id, "8814");
        assert_eq!(f.version, "11.2.15");
        assert_eq!(f.file_name, "DBM-Core-11.2.15.zip");
        assert!(f
            .url
            .starts_with("https://cdn.wowinterface.com/downloads/getfile.php?id=8814"));
        assert_eq!(f.md5.as_deref(), Some("e81dc2d50729669d0b7ff8a3f4ab4db1"));
        assert_eq!(f.date, Some(1_757_932_228), "UIDate ms => s");
        assert_eq!(m.updated_at, Some(1_757_932_228));
        assert!(f.flavors.is_empty());
    }

    #[test]
    fn v4_holes_fall_back_to_v3() {
        let all = clean_ids(&[
            "8814".into(),
            "3826".into(),
            " 8814 ".into(),
            "abc".into(),
            "".into(),
        ]);
        assert_eq!(
            all,
            vec!["8814".to_string(), "3826".to_string()],
            "dedup + numeric only"
        );

        let body: Value = serde_json::from_str(V4_JSON).unwrap();
        let mut found: BTreeMap<String, RemoteMeta> = BTreeMap::new();
        for rec in parse_v4(&body) {
            if let Some(m) = v4_meta(&rec) {
                found.insert(m.external_id.clone(), m);
            }
        }
        assert_eq!(found.len(), 1);
        assert_eq!(
            missing_ids(&all, &found),
            vec!["8814".to_string()],
            "8814 is a v4 hole"
        );

        // v4 "no add-on" body is not an array => no records, no panic
        let holes: Value = serde_json::from_str(r#"{"ERROR":"No AddOn found."}"#).unwrap();
        assert!(parse_v4(&holes).is_empty());
        assert!(parse_v3(&holes).is_empty());
        let empty: Value = serde_json::from_str("[]").unwrap();
        assert!(parse_v4(&empty).is_empty());

        // in-order output of the merged results
        let mut merged = found.clone();
        let v3body: Value = serde_json::from_str(V3_JSON).unwrap();
        let v3 = v3_meta(&parse_v3(&v3body)[0]).unwrap();
        merged.insert(v3.external_id.clone(), v3);
        let out: Vec<RemoteMeta> = all
            .into_iter()
            .filter_map(|id| merged.remove(id.as_str()))
            .collect();
        assert_eq!(
            out.iter()
                .map(|m| m.external_id.as_str())
                .collect::<Vec<_>>(),
            vec!["8814", "3826"]
        );
    }

    fn folder(name: &str, wowi: Option<&str>, version: Option<&str>) -> FolderInfo {
        let mut toc = ParsedToc::default();
        toc.ids.wowi = wowi.map(str::to_string);
        toc.version = version.map(str::to_string);
        FolderInfo {
            name: name.into(),
            path: std::path::PathBuf::from("/tmp").join(name),
            toc,
            curse_fingerprint: None,
            size_bytes: 0,
        }
    }

    #[test]
    fn scan_groups_by_wowi_id() {
        let folders = vec![
            folder("DBM-Core", Some("8814"), Some("11.2.15")),
            folder("DBM-AuraWatch", Some(" 8814 "), None),
            folder("TukuiOnly", None, None),
            folder("Broken", Some(""), None),
        ];
        let groups = wowi_folders(&folders);
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups["8814"],
            vec!["DBM-Core".to_string(), "DBM-AuraWatch".to_string()]
        );
        assert!(wowi_folders(&[folder("Plain", None, None)]).is_empty());
    }

    #[test]
    fn changelog_prefers_embedded_text() {
        let p = MmouiProvider::new();
        let mut f = remote_file(
            "1",
            "1.0",
            "https://cdn.wowinterface.com/x.zip",
            "x.zip",
            "",
            None,
            Some("### 1.0\n- fixes".into()),
        );
        let got = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                p.changelog(&ctx(), &RemoteMeta::default_meta("1"), &f)
                    .await
            })
            .expect("changelog");
        assert_eq!(got.as_deref(), Some("### 1.0\n- fixes"));

        f.changelog_url = Some("https://www.wowinterface.com/downloads/info1.html".into());
        let got = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                p.changelog(&ctx(), &RemoteMeta::default_meta("1"), &f)
                    .await
            })
            .expect("changelog");
        assert_eq!(
            got.as_deref(),
            Some("Changelog: https://www.wowinterface.com/downloads/info1.html")
        );

        let none = remote_file(
            "1",
            "1.0",
            "https://cdn.wowinterface.com/x.zip",
            "x.zip",
            "",
            None,
            None,
        );
        assert_eq!(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    p.changelog(&ctx(), &RemoteMeta::default_meta("1"), &none)
                        .await
                })
                .unwrap(),
            None
        );
    }

    #[test]
    fn malformed_json_degrades_gracefully() {
        let junk: Vec<Value> = vec![
            serde_json::from_str("[]").unwrap(),
            serde_json::from_str(r#"{"ERROR":"x"}"#).unwrap(),
            serde_json::from_str("null").unwrap(),
            serde_json::from_str(r#"[{"id":"abc","title":5}]"#).unwrap(),
            serde_json::from_str(r#"[{}]"#).unwrap(),
        ];
        for body in junk {
            for m in parse_v4(&body).iter().filter_map(v4_meta) {
                assert!(!m.external_id.is_empty());
            }
            for m in parse_v3(&body).iter().filter_map(v3_meta) {
                assert!(!m.external_id.is_empty());
            }
        }
        // non-numeric ids never reach the API
        assert!(clean_ids(&["<script>".into(), "8814; DROP".into()]).is_empty());
        // broken json is an Err, never a panic
        assert!(serde_json::from_str::<Value>("{\"data\":[{").is_err());
    }

    fn ctx() -> AppCtx {
        AppCtx {
            http: net::build_client("fa-test"),
            cfg: Arc::new(AppConfig::default()),
            app_version: "test",
        }
    }

    impl RemoteMeta {
        fn default_meta(id: &str) -> RemoteMeta {
            RemoteMeta {
                provider: ProviderKind::WowInterface,
                external_id: id.into(),
                name: "x".into(),
                author: None,
                site_url: None,
                repo_url: None,
                description: None,
                icon_url: None,
                downloads: None,
                updated_at: None,
                files: Vec::new(),
            }
        }
    }

    fn live() -> bool {
        std::env::var("FA_LIVE").map(|v| v == "1").unwrap_or(false)
    }

    #[tokio::test]
    #[ignore = "live MMOUI API (FA_LIVE=1)"]
    async fn live_v4_batch_and_v3_fallback() {
        if !live() {
            return;
        }
        let c = ctx();
        let p = MmouiProvider::new();
        // 8814 (DBM) is a known v4 hole: it must resolve through the v3 fallback
        let metas = p
            .meta(&c, &["3826".into(), "8814".into(), "999999999".into()])
            .await
            .expect("meta ok");
        let ids: Vec<&str> = metas.iter().map(|m| m.external_id.as_str()).collect();
        assert!(ids.contains(&"3826"), "v4 record expected, got {ids:?}");
        let dbm = metas
            .iter()
            .find(|m| m.external_id == "8814")
            .expect("v3 fallback for 8814");
        assert!(dbm.name.contains("Boss Mods"), "name: {}", dbm.name);
        assert_eq!(dbm.files.len(), 1);
        assert!(
            dbm.files[0]
                .url
                .starts_with("https://cdn.wowinterface.com/"),
            "{:?}",
            dbm.files[0].url
        );
        assert!(dbm.files[0].md5.is_some(), "v3 publishes an md5");
        assert!(dbm.files[0].date.unwrap_or(0) > 1_700_000_000);
        assert!(
            p.search(
                &c,
                "dbm",
                0,
                10,
                &FileSelect {
                    flavor: crate::flavor::Flavor::Forever,
                    interface: 16001,
                    version: "1.60.1".into(),
                    channel: crate::types::Channel::Stable,
                }
            )
            .await
            .is_err(),
            "no search endpoint"
        );
    }
}
