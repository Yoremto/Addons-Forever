//! GitHub Releases provider (spec §2).
//!
//! external_id is `owner/repo` (full repo URLs with a trailing `.git` are accepted,
//! which is what the TOC `## X-Source:` scan parses). File selection follows the
//! BigWigs-packager convention: a `release.json` metadata asset wins when present
//! (flavor tokens per entry, `camelot` → Forever, `nolib` entries excluded), asset
//! name suffix tokens are the fallback. Prereleases are gated by channel at pick
//! time; downloads use `browser_download_url` with the octet-stream asset endpoint
//! as fallback. Search is not supported (noisy) — users paste a repo URL.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use tracing::warn;

use crate::error::{Error, Result};
use crate::flavor::Flavor;
use crate::net;
use crate::toc::ParsedToc;
use crate::types::{
    AppCtx, Channel, FileSelect, Provider, ProviderKind, RemoteFile, RemoteMeta, ScanMatch,
    ScanRequest, SearchPage,
};

const API: &str = "https://api.github.com";
const ACCEPT_API: &str = "application/vnd.github+json";
const ACCEPT_OCTET: &str = "application/octet-stream";
/// Releases considered per repo when building the file list.
const SCAN_RELEASES: usize = 5;
const RATE_LIMIT_MSG: &str = "GitHub API rate limit exceeded (x-ratelimit-remaining: 0) \
    — set [network] github_token in addons-forever.ini to raise the limit (60 req/h unauthenticated)";

#[derive(Default)]
pub struct GitHubProvider;

impl GitHubProvider {
    pub fn new() -> Self {
        GitHubProvider
    }
}

// ---------------------------------------------------------------- serde models

#[derive(Debug, Deserialize, Default)]
pub struct GhRepo {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub full_name: String,
    #[serde(default)]
    pub html_url: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub owner: Option<GhOwner>,
}

#[derive(Debug, Deserialize, Default)]
pub struct GhOwner {
    #[serde(default)]
    pub login: String,
    #[serde(default)]
    pub avatar_url: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct GhRelease {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub tag_name: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub html_url: Option<String>,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub assets: Vec<GhAsset>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct GhAsset {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub content_type: String,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub browser_download_url: Option<String>,
}

/// `release.json` BigWigs-packager metadata asset:
/// `{"releases":[{"name","version","filename","nolib","metadata":[{"flavor","interface"}]}]}`.
#[derive(Debug, Deserialize, Default)]
pub struct ReleaseJson {
    #[serde(default)]
    pub releases: Vec<ReleaseJsonEntry>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ReleaseJsonEntry {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub nolib: bool,
    #[serde(default)]
    pub metadata: Vec<ReleaseJsonMeta>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ReleaseJsonMeta {
    #[serde(default)]
    pub flavor: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub interface: Option<u32>,
}

// ------------------------------------------------------------------- helpers

fn rate_limit_error() -> Error {
    Error::provider("GitHub", RATE_LIMIT_MSG)
}

fn is_rate_limit(err: &Error) -> bool {
    matches!(err, Error::Provider { message, .. } if message.contains("rate limit"))
}

/// Parse `owner/repo`, `github.com/owner/repo`, `.../repo.git`, trailing path
/// segments (`/releases`) and trailing slashes included.
pub fn parse_repo_id(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let path = if raw.contains("://") {
        let url = reqwest::Url::parse(raw).ok()?;
        let host = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();
        if host != "github.com" && host != "www.github.com" {
            return None;
        }
        url.path().to_string()
    } else {
        raw.to_string()
    };
    let mut segs = path.trim_matches('/').split('/');
    let owner = clean_repo_seg(segs.next()?)?;
    let repo = clean_repo_seg(segs.next()?)?;
    Some(format!("{owner}/{repo}"))
}

fn clean_repo_seg(seg: &str) -> Option<String> {
    let seg = seg.trim().trim_end_matches(".git").trim_end_matches('/');
    if seg.is_empty() || seg.len() > 100 {
        return None;
    }
    if !seg
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return None;
    }
    Some(seg.to_string())
}

/// The `## X-Source:` / `## X-Repo:` / `## X-GitHub:` TOC line → repo id.
fn repo_from_toc(toc: &ParsedToc) -> Option<String> {
    toc.ids.github.as_deref().and_then(parse_repo_id)
}

fn parse_iso(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s.trim())
        .ok()
        .map(|d| d.timestamp())
}

fn release_date(rel: &GhRelease) -> Option<i64> {
    rel.published_at
        .as_deref()
        .and_then(parse_iso)
        .or_else(|| rel.created_at.as_deref().and_then(parse_iso))
}

fn strip_version(tag: &str) -> String {
    let t = tag.trim();
    if t.len() > 1
        && (t.as_bytes()[0] == b'v' || t.as_bytes()[0] == b'V')
        && t.as_bytes()[1].is_ascii_digit()
    {
        t[1..].to_string()
    } else {
        t.to_string()
    }
}

/// Packager `release.json` flavor token → Flavor (`mainline`→Retail, `camelot`→Forever).
fn packager_flavor(token: &str) -> Option<Flavor> {
    match token.trim().to_ascii_lowercase().as_str() {
        "mainline" | "standard" => Some(Flavor::Retail),
        other => Flavor::parse(other),
    }
}

/// Asset-name suffix tokens, e.g. `Name-1.5.1-forever.zip`, `Name_Classic.zip`.
const ASSET_FLAVOR_TOKENS: &[(&str, Flavor)] = &[
    ("forever", Flavor::Forever),
    ("camelot", Flavor::Forever),
    ("mainline", Flavor::Retail),
    ("standard", Flavor::Retail),
    ("classic", Flavor::ClassicEra),
    ("vanilla", Flavor::ClassicEra),
    ("era", Flavor::ClassicEra),
    ("bcc", Flavor::Tbc),
    ("tbc", Flavor::Tbc),
    ("wrath", Flavor::Wotlk),
    ("wotlkc", Flavor::Wotlk),
    ("cata", Flavor::Cata),
    ("mists", Flavor::Mists),
    ("titan", Flavor::Titan),
];

/// Flavors declared by client-suffix tokens in an asset name; empty = no suffix.
fn asset_flavor_tokens(name: &str) -> Vec<Flavor> {
    let lower = name.to_ascii_lowercase();
    let stem = lower.strip_suffix(".zip").unwrap_or(&lower);
    let mut out: Vec<Flavor> = Vec::new();
    for part in stem.split(['-', '_']) {
        if let Some((_, f)) = ASSET_FLAVOR_TOKENS.iter().find(|(t, _)| *t == part) {
            if !out.contains(f) {
                out.push(*f);
            }
        }
    }
    out
}

fn is_zip_asset(a: &GhAsset) -> bool {
    a.content_type.eq_ignore_ascii_case("application/zip")
        || a.name.to_ascii_lowercase().ends_with(".zip")
}

fn is_nolib_name(name: &str) -> bool {
    name.to_ascii_lowercase().contains("-nolib")
}

/// (download url, Accept header) for an asset: public browser URL when present,
/// the API asset endpoint + octet-stream dance otherwise.
fn asset_endpoint(repo_id: &str, asset: &GhAsset) -> (String, &'static str) {
    match &asset.browser_download_url {
        Some(u) if !u.trim().is_empty() => (u.clone(), ACCEPT_API),
        _ => (
            format!("{API}/repos/{repo_id}/releases/assets/{}", asset.id),
            ACCEPT_OCTET,
        ),
    }
}

/// Files of one release. `release.json` (when present) drives selection:
/// non-nolib entries mapped to assets by `filename`, flavors from metadata.
/// Fallback: zip assets, `-nolib` excluded, flavors from name-suffix tokens.
pub fn release_files(
    repo_id: &str,
    rel: &GhRelease,
    release_json: Option<&ReleaseJson>,
) -> Vec<RemoteFile> {
    if rel.draft {
        return Vec::new();
    }
    let version = strip_version(&rel.tag_name);
    let channel = if rel.prerelease {
        Channel::Beta
    } else {
        Channel::Stable
    };
    let date = release_date(rel);
    let make = |asset: &GhAsset, flavors: Vec<Flavor>, flavor_meta: bool| -> RemoteFile {
        let (url, accept) = asset_endpoint(repo_id, asset);
        let mut headers = BTreeMap::new();
        if accept == ACCEPT_OCTET {
            headers.insert("Accept".into(), ACCEPT_OCTET.into());
        }
        RemoteFile {
            file_id: format!("{}:{}", rel.id, asset.id),
            version: version.clone(),
            channel,
            file_name: asset.name.clone(),
            url,
            headers,
            // raw metadata presence, not the mapped result: a release.json
            // entry tagged only for unknown flavors is foreign, not unknown
            flavor_meta,
            flavors,
            date,
            size: asset.size,
            md5: None,
            sha1: None,
            changelog_url: rel.html_url.clone(),
        }
    };

    match release_json {
        Some(rj) => {
            let mut out = Vec::new();
            for entry in &rj.releases {
                if entry.nolib {
                    continue;
                }
                if entry.filename.trim().is_empty() {
                    continue;
                }
                let Some(asset) = rel
                    .assets
                    .iter()
                    .find(|a| a.name.eq_ignore_ascii_case(&entry.filename))
                else {
                    warn!(target: "fa::github", "release.json filename {:?} has no matching asset (release {})", entry.filename, rel.id);
                    continue;
                };
                let mut flavors: Vec<Flavor> = Vec::new();
                for m in &entry.metadata {
                    if let Some(f) = packager_flavor(&m.flavor) {
                        if !flavors.contains(&f) {
                            flavors.push(f);
                        }
                    }
                }
                let tagged = !entry.metadata.is_empty();
                out.push(make(asset, flavors, tagged));
            }
            out
        }
        None => rel
            .assets
            .iter()
            .filter(|a| is_zip_asset(a) && !is_nolib_name(&a.name))
            .map(|a| {
                let flavors = asset_flavor_tokens(&a.name);
                let tagged = !flavors.is_empty();
                make(a, flavors, tagged)
            })
            .collect(),
    }
}

/// Build the RemoteMeta for one repo from its releases (+ optional release.json
/// per release, same order). Pure: unit-testable without network.
pub fn build_meta(
    repo_id: &str,
    repo: &GhRepo,
    releases: &[(GhRelease, Option<ReleaseJson>)],
) -> RemoteMeta {
    let mut files: Vec<RemoteFile> = Vec::new();
    for (rel, rj) in releases {
        files.extend(release_files(repo_id, rel, rj.as_ref()));
    }
    let name = {
        let n = repo.name.trim();
        if n.is_empty() {
            repo_id.rsplit('/').next().unwrap_or(repo_id).to_string()
        } else {
            n.to_string()
        }
    };
    RemoteMeta {
        provider: ProviderKind::GitHub,
        external_id: repo_id.to_string(),
        name,
        author: repo
            .owner
            .as_ref()
            .map(|o| o.login.clone())
            .filter(|o| !o.is_empty()),
        site_url: Some(repo.html_url.clone()).filter(|u| !u.is_empty()),
        repo_url: Some(repo.html_url.clone()).filter(|u| !u.is_empty()),
        description: repo.description.clone().filter(|d| !d.trim().is_empty()),
        icon_url: repo.owner.as_ref().and_then(|o| o.avatar_url.clone()),
        downloads: None,
        updated_at: repo.updated_at.as_deref().and_then(parse_iso),
        files,
    }
}

/// Releases ready for file building: non-draft, published desc,
/// stable before prerelease, capped at SCAN_RELEASES.
fn select_releases(mut rels: Vec<GhRelease>) -> Vec<GhRelease> {
    rels.retain(|r| !r.draft);
    rels.sort_by(|a, b| {
        release_date(b)
            .unwrap_or(0)
            .cmp(&release_date(a).unwrap_or(0))
            .then(a.prerelease.cmp(&b.prerelease))
    });
    rels.truncate(SCAN_RELEASES);
    rels
}

/// Pick score: 0 = excluded (incompatible flavor), 1 = flavor-agnostic,
/// 2 = explicitly labeled for `sel.flavor`. Retail prefers the no-client-suffix
/// asset, so unlabeled files score 2 there (spec §2.2).
/// Rank key: (score, date desc, stable-before-prerelease, version key).
fn file_rank(f: &RemoteFile, sel: &FileSelect) -> Option<(u8, i64, u8, Vec<u64>)> {
    if f.channel > sel.channel {
        return None;
    }
    let score = if f.flavors.is_empty() {
        if sel.flavor == Flavor::Retail {
            2
        } else {
            1
        }
    } else if f.flavors.contains(&sel.flavor) {
        2
    } else {
        return None;
    };
    let stable = if f.channel == Channel::Stable { 1 } else { 0 };
    Some((
        score,
        f.date.unwrap_or(0),
        stable,
        crate::util::version_key(&f.version),
    ))
}

// --------------------------------------------------------------------- http

async fn gh_send(ctx: &AppCtx, url: &str, accept: &str) -> Result<reqwest::Response> {
    net::ensure_allowed(url, &[])?;
    let mut req = ctx.http.get(url).header("Accept", accept);
    // AF-03: the bearer token goes to the API host only (PRIVACY.md), never
    // to asset download hosts
    let to_api = reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.eq_ignore_ascii_case("api.github.com")))
        .unwrap_or(false);
    let token = ctx.cfg.network.github_token.trim();
    if !token.is_empty() && to_api {
        req = req.bearer_auth(token);
    }
    let resp = req.send().await?;
    let status = resp.status();
    let remaining = resp
        .headers()
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    // AF-27: only treat the quota as an error for non-success responses; a
    // 200 that consumes the last request is valid data
    if remaining == "0" && !status.is_success() {
        return Err(rate_limit_error());
    }
    if status.as_u16() == 403 || status.as_u16() == 429 {
        let body = resp.text().await.unwrap_or_default();
        let snippet: String = body.chars().take(300).collect();
        if snippet.to_ascii_lowercase().contains("rate limit") {
            return Err(rate_limit_error());
        }
        return Err(Error::provider(
            "GitHub",
            format!("GET {url} -> {status}: {snippet}"),
        ));
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let snippet: String = body.chars().take(300).collect();
        return Err(Error::provider(
            "GitHub",
            format!("GET {url} -> {status}: {snippet}"),
        ));
    }
    Ok(resp)
}

async fn gh_json<T: DeserializeOwned>(ctx: &AppCtx, url: &str) -> Result<T> {
    let resp = gh_send(ctx, url, ACCEPT_API).await?;
    resp.json::<T>().await.map_err(|e| {
        warn!(target: "fa::github", "malformed JSON from {url}: {e}");
        Error::provider("GitHub", format!("malformed JSON response from {url}: {e}"))
    })
}

async fn fetch_release_json(
    ctx: &AppCtx,
    repo_id: &str,
    rel: &GhRelease,
) -> Result<Option<ReleaseJson>> {
    let Some(asset) = rel
        .assets
        .iter()
        .find(|a| a.name.eq_ignore_ascii_case("release.json"))
    else {
        return Ok(None);
    };
    let (url, accept) = asset_endpoint(repo_id, asset);
    let resp = gh_send(ctx, &url, accept).await?;
    let bytes = resp.bytes().await?;
    match serde_json::from_slice::<ReleaseJson>(&bytes) {
        Ok(rj) => Ok(Some(rj)),
        Err(e) => {
            warn!(target: "fa::github", "{repo_id}: unparsable release.json ({}): {e}", bytes.len());
            Ok(None)
        }
    }
}

// ------------------------------------------------------------------ provider

#[async_trait]
impl Provider for GitHubProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::GitHub
    }

    async fn meta(&self, ctx: &AppCtx, ids: &[String]) -> Result<Vec<RemoteMeta>> {
        ctx.ensure_online()?;
        let mut out = Vec::new();
        for raw in ids {
            let Some(repo_id) = parse_repo_id(raw) else {
                warn!(target: "fa::github", "skipping unparseable repo id {raw:?}");
                continue;
            };
            let repo: GhRepo = match gh_json(ctx, &format!("{API}/repos/{repo_id}")).await {
                Ok(r) => r,
                Err(e) => {
                    if is_rate_limit(&e) {
                        return Err(e);
                    }
                    warn!(target: "fa::github", "repo {repo_id}: {e}");
                    continue;
                }
            };
            let rels: Vec<GhRelease> =
                match gh_json(ctx, &format!("{API}/repos/{repo_id}/releases?per_page=10")).await {
                    Ok(r) => r,
                    Err(e) => {
                        if is_rate_limit(&e) {
                            return Err(e);
                        }
                        warn!(target: "fa::github", "releases of {repo_id}: {e}");
                        Vec::new()
                    }
                };
            let mut items: Vec<(GhRelease, Option<ReleaseJson>)> = Vec::new();
            for rel in select_releases(rels) {
                let rj = match fetch_release_json(ctx, &repo_id, &rel).await {
                    Ok(rj) => rj,
                    Err(e) => {
                        if is_rate_limit(&e) {
                            return Err(e);
                        }
                        warn!(target: "fa::github", "{repo_id}: release.json fetch failed: {e}");
                        None
                    }
                };
                items.push((rel, rj));
            }
            out.push(build_meta(&repo_id, &repo, &items));
        }
        Ok(out)
    }

    async fn scan(&self, ctx: &AppCtx, req: &ScanRequest<'_>) -> Result<Vec<ScanMatch>> {
        ctx.ensure_online()?;
        let mut by_repo: BTreeMap<String, Vec<&crate::types::FolderInfo>> = BTreeMap::new();
        for folder in req.folders {
            if let Some(repo) = repo_from_toc(&folder.toc) {
                by_repo.entry(repo).or_default().push(folder);
            }
        }
        if by_repo.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<String> = by_repo.keys().cloned().collect();
        let metas = match self.meta(ctx, &ids).await {
            Ok(m) => m,
            Err(e) => {
                if is_rate_limit(&e) {
                    return Err(e);
                }
                warn!(target: "fa::github", "scan meta failed: {e}");
                Vec::new()
            }
        };
        let mut out = Vec::new();
        for (repo, folders) in by_repo {
            let Some(meta) = metas.iter().find(|m| m.external_id == repo).cloned() else {
                continue;
            };
            let mut names: Vec<String> = folders.iter().map(|f| f.name.clone()).collect();
            names.sort();
            names.dedup();
            let installed_version = folders
                .iter()
                .find_map(|f| f.toc.version.clone().filter(|v| !v.trim().is_empty()));
            out.push(ScanMatch {
                provider: ProviderKind::GitHub,
                external_id: repo,
                folders: names,
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
            "GitHub has no reliable addon catalog — paste a repository URL \
             (https://github.com/owner/repo) to install from GitHub"
                .into(),
        ))
    }

    fn pick(&self, meta: &RemoteMeta, sel: &FileSelect) -> Option<RemoteFile> {
        // Tie-break on file order (newest release first): earliest entry wins.
        meta.files
            .iter()
            .enumerate()
            .filter_map(|(idx, f)| file_rank(f, sel).map(|k| (k, std::cmp::Reverse(idx), f)))
            .max_by_key(|(k, idx, _)| (k.clone(), *idx))
            .map(|(_, _, f)| f.clone())
    }
}

// -------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(flavor: Flavor, channel: Channel) -> FileSelect {
        FileSelect {
            flavor,
            interface: 16001,
            version: "1.60.1".into(),
            channel,
        }
    }

    fn release_fixture() -> GhRelease {
        serde_json::from_str(
            r#"{
              "id": 9001,
              "tag_name": "v1.5.1",
              "name": "1.5.1",
              "html_url": "https://github.com/BigWigsMods/BigWigs/releases/tag/v1.5.1",
              "draft": false,
              "prerelease": false,
              "published_at": "2026-11-01T10:00:00Z",
              "assets": [
                {"id": 1, "name": "Name-1.5.1-forever.zip", "content_type": "application/zip", "size": 111,
                 "browser_download_url": "https://github.com/o/r/releases/download/v1.5.1/Name-1.5.1-forever.zip"},
                {"id": 2, "name": "Name.zip", "content_type": "application/zip", "size": 222,
                 "browser_download_url": "https://github.com/o/r/releases/download/v1.5.1/Name.zip"},
                {"id": 3, "name": "Name-nolib.zip", "content_type": "application/zip", "size": 333,
                 "browser_download_url": "https://github.com/o/r/releases/download/v1.5.1/Name-nolib.zip"},
                {"id": 4, "name": "release.json", "content_type": "application/json", "size": 444,
                 "browser_download_url": "https://github.com/o/r/releases/download/v1.5.1/release.json"},
                {"id": 5, "name": "Source-code.zip", "content_type": "application/zip", "size": 555,
                 "browser_download_url": "https://github.com/o/r/releases/download/v1.5.1/Source-code.zip"}
              ]
            }"#,
        )
        .unwrap()
    }

    fn release_json_fixture() -> ReleaseJson {
        serde_json::from_str(
            r#"{"releases": [
               {"name": "BigWigs", "version": "1.5.1", "filename": "Name-1.5.1-forever.zip",
                "metadata": [{"flavor": "forever", "interface": 16001}]},
               {"name": "BigWigs", "version": "1.5.1", "filename": "Name.zip",
                "metadata": [{"flavor": "mainline", "interface": 120105}]},
               {"name": "BigWigs (camelot)", "version": "1.5.1", "filename": "Source-code.zip",
                "metadata": [{"flavor": "camelot", "interface": 16001}]},
               {"name": "BigWigs (no libs)", "version": "1.5.1", "filename": "Name-nolib.zip", "nolib": true,
                "metadata": [{"flavor": "forever", "interface": 16001}]}
             ]}"#,
        )
        .unwrap()
    }

    #[test]
    fn parse_repo_id_variants() {
        assert_eq!(
            parse_repo_id("BigWigsMods/BigWigs").as_deref(),
            Some("BigWigsMods/BigWigs")
        );
        assert_eq!(
            parse_repo_id("https://github.com/BigWigsMods/BigWigs").as_deref(),
            Some("BigWigsMods/BigWigs")
        );
        assert_eq!(
            parse_repo_id("https://github.com/BigWigsMods/BigWigs.git").as_deref(),
            Some("BigWigsMods/BigWigs")
        );
        assert_eq!(
            parse_repo_id("https://github.com/BigWigsMods/BigWigs/releases").as_deref(),
            Some("BigWigsMods/BigWigs")
        );
        assert_eq!(
            parse_repo_id("https://www.GitHub.com/o/r/").as_deref(),
            Some("o/r")
        );
        assert_eq!(parse_repo_id("https://gitlab.com/o/r"), None);
        assert_eq!(parse_repo_id("just-one-segment"), None);
        assert_eq!(parse_repo_id("bad chars/here"), None);
        assert_eq!(parse_repo_id(""), None);
    }

    #[test]
    fn repo_from_toc_uses_x_source() {
        let toc = ParsedToc {
            ids: crate::toc::TocIds {
                github: Some("https://github.com/BigWigsMods/BigWigs.git".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(repo_from_toc(&toc).as_deref(), Some("BigWigsMods/BigWigs"));
    }

    #[test]
    fn release_json_parsing_rules() {
        let rel = release_fixture();
        let rj = release_json_fixture();
        let files = release_files("BigWigsMods/BigWigs", &rel, Some(&rj));
        // nolib entry excluded; release.json entry matched by filename.
        assert_eq!(files.len(), 3, "nolib excluded: {files:?}");
        assert!(files.iter().all(|f| !f.file_name.contains("nolib")));

        let forever = files
            .iter()
            .find(|f| f.file_name == "Name-1.5.1-forever.zip")
            .unwrap();
        assert_eq!(forever.flavors, vec![Flavor::Forever]);
        assert_eq!(forever.version, "1.5.1"); // tag v1.5.1, leading v stripped
        assert_eq!(forever.file_id, "9001:1");
        assert_eq!(forever.channel, Channel::Stable);
        assert_eq!(forever.date, Some(1_793_527_200)); // 2026-11-01T10:00:00Z
        let mainline = files.iter().find(|f| f.file_name == "Name.zip").unwrap();
        assert_eq!(mainline.flavors, vec![Flavor::Retail]); // mainline → Retail
        let camelot = files
            .iter()
            .find(|f| f.file_name == "Source-code.zip")
            .unwrap();
        assert_eq!(camelot.flavors, vec![Flavor::Forever]); // camelot → Forever
        assert!(camelot.headers.is_empty()); // browser_download_url present
    }

    #[test]
    fn asset_name_fallback_matrix() {
        // suffix-token classification
        assert_eq!(
            asset_flavor_tokens("Name-1.5.1-forever.zip"),
            vec![Flavor::Forever]
        );
        assert_eq!(
            asset_flavor_tokens("Name_Classic.zip"),
            vec![Flavor::ClassicEra]
        );
        assert_eq!(asset_flavor_tokens("x-bcc.zip"), vec![Flavor::Tbc]);
        assert_eq!(asset_flavor_tokens("x-tbc.zip"), vec![Flavor::Tbc]);
        assert_eq!(asset_flavor_tokens("x-wrath.zip"), vec![Flavor::Wotlk]);
        assert_eq!(asset_flavor_tokens("x-wotlkc.zip"), vec![Flavor::Wotlk]);
        assert_eq!(asset_flavor_tokens("x-cata.zip"), vec![Flavor::Cata]);
        assert_eq!(asset_flavor_tokens("x-mists.zip"), vec![Flavor::Mists]);
        assert_eq!(asset_flavor_tokens("x-titan.zip"), vec![Flavor::Titan]);
        assert_eq!(asset_flavor_tokens("x-mainline.zip"), vec![Flavor::Retail]);
        assert!(asset_flavor_tokens("Name.zip").is_empty());
        assert!(asset_flavor_tokens("Name-1.5.1.zip").is_empty());

        // fallback file building: no release.json
        let mut rel = release_fixture();
        rel.assets
            .iter_mut()
            .for_each(|a| a.browser_download_url = None); // octet dance
        let files = release_files("o/r", &rel, None);
        let names: Vec<&str> = files.iter().map(|f| f.file_name.as_str()).collect();
        // -nolib excluded; all zip assets kept with token flavors
        assert!(
            !names.contains(&"Name-nolib.zip"),
            "nolib must be excluded: {names:?}"
        );
        assert_eq!(files.len(), 3, "{names:?}"); // forever, plain, source-code (release.json is not a zip)
        let forever = files
            .iter()
            .find(|f| f.file_name == "Name-1.5.1-forever.zip")
            .unwrap();
        assert_eq!(forever.flavors, vec![Flavor::Forever]);
        assert!(files
            .iter()
            .find(|f| f.file_name == "Name.zip")
            .unwrap()
            .flavors
            .is_empty());
        // no browser_download_url → API octet-stream endpoint + Accept header
        assert_eq!(forever.url, format!("{API}/repos/o/r/releases/assets/1"));
        assert_eq!(
            forever.headers.get("Accept").map(|s| s.as_str()),
            Some(ACCEPT_OCTET)
        );
    }

    #[test]
    fn asset_endpoint_fallback() {
        let repo = "o/r";
        let with = GhAsset {
            id: 7,
            name: "a.zip".into(),
            browser_download_url: Some("https://github.com/o/r/releases/download/v1/a.zip".into()),
            ..Default::default()
        };
        let (url, accept) = asset_endpoint(repo, &with);
        assert_eq!(url, "https://github.com/o/r/releases/download/v1/a.zip");
        assert_eq!(accept, ACCEPT_API);
        let without = GhAsset {
            id: 8,
            name: "b.zip".into(),
            content_type: "application/zip".into(),
            ..Default::default()
        };
        let (url, accept) = asset_endpoint(repo, &without);
        assert_eq!(url, format!("{API}/repos/o/r/releases/assets/8"));
        assert_eq!(accept, ACCEPT_OCTET);
    }

    #[test]
    fn pick_forever_with_release_json() {
        let rel = release_fixture();
        let files = release_files("BigWigsMods/BigWigs", &rel, Some(&release_json_fixture()));
        let mut meta = build_meta("BigWigsMods/BigWigs", &GhRepo::default(), &[]);
        meta.files = files;

        let picked = GitHubProvider
            .pick(&meta, &sel(Flavor::Forever, Channel::Stable))
            .unwrap();
        assert_eq!(picked.file_name, "Name-1.5.1-forever.zip"); // over plain Name.zip

        let retail = GitHubProvider
            .pick(&meta, &sel(Flavor::Retail, Channel::Stable))
            .unwrap();
        assert_eq!(retail.file_name, "Name.zip"); // mainline entry, prefers no suffix
    }

    #[test]
    fn pick_forever_fallback_mode() {
        let rel = release_fixture();
        let files = release_files("o/r", &rel, None); // no release.json
        let mut meta = build_meta("o/r", &GhRepo::default(), &[]);
        meta.files = files;

        let picked = GitHubProvider
            .pick(&meta, &sel(Flavor::Forever, Channel::Stable))
            .unwrap();
        assert_eq!(picked.file_name, "Name-1.5.1-forever.zip"); // preferred over plain Name.zip
        let retail = GitHubProvider
            .pick(&meta, &sel(Flavor::Retail, Channel::Stable))
            .unwrap();
        assert_eq!(retail.file_name, "Name.zip"); // no client suffix wins
    }

    #[test]
    fn pick_retail_excludes_other_flavor_suffixes() {
        let mut rel = release_fixture();
        rel.assets.extend_from_slice(&[
            GhAsset {
                id: 10,
                name: "Name-classic.zip".into(),
                content_type: "application/zip".into(),
                ..Default::default()
            },
            GhAsset {
                id: 11,
                name: "Name-titan.zip".into(),
                content_type: "application/zip".into(),
                ..Default::default()
            },
        ]);
        let mut meta = build_meta("o/r", &GhRepo::default(), &[]);
        meta.files = release_files("o/r", &rel, None);
        let retail = GitHubProvider
            .pick(&meta, &sel(Flavor::Retail, Channel::Stable))
            .unwrap();
        assert_eq!(retail.file_name, "Name.zip");
        let picked_ids: Vec<String> = meta
            .files
            .iter()
            .filter_map(|f| {
                file_rank(f, &sel(Flavor::Retail, Channel::Stable)).map(|_| f.file_name.clone())
            })
            .collect();
        assert!(!picked_ids.contains(&"Name-classic.zip".to_string()));
        assert!(!picked_ids.contains(&"Name-titan.zip".to_string()));
        assert!(!picked_ids.contains(&"Name-1.5.1-forever.zip".to_string()));
    }

    #[test]
    fn pick_prerelease_gating() {
        let mut rel = release_fixture();
        rel.prerelease = true;
        let mut files = release_files("o/r", &rel, None);
        let mut stable = release_fixture();
        stable.id = 9000;
        stable.tag_name = "v1.5.0".into();
        stable.prerelease = false;
        stable.published_at = Some("2026-10-01T10:00:00Z".into());
        files.extend(release_files("o/r", &stable, None));
        let mut meta = build_meta("o/r", &GhRepo::default(), &[]);
        meta.files = files;

        // stable channel → prerelease (v1.5.1, newer) excluded
        let picked = GitHubProvider
            .pick(&meta, &sel(Flavor::Forever, Channel::Stable))
            .unwrap();
        assert_eq!(picked.version, "1.5.0");
        assert_eq!(picked.channel, Channel::Stable);
        // beta channel → prerelease allowed (newer wins)
        let picked = GitHubProvider
            .pick(&meta, &sel(Flavor::Forever, Channel::Beta))
            .unwrap();
        assert_eq!(picked.version, "1.5.1");
        assert_eq!(picked.channel, Channel::Beta);
    }

    #[test]
    fn draft_releases_produce_no_files_and_sorting() {
        let mut rel = release_fixture();
        rel.draft = true;
        assert!(release_files("o/r", &rel, None).is_empty());

        let mut older = release_fixture();
        older.id = 1;
        older.published_at = Some("2026-01-01T00:00:00Z".into());
        let mut pre = release_fixture();
        pre.id = 2;
        pre.prerelease = true;
        pre.published_at = Some("2026-06-01T00:00:00Z".into());
        let mut stable_new = release_fixture();
        stable_new.id = 3;
        stable_new.published_at = Some("2026-06-01T00:00:00Z".into());
        let sorted = select_releases(vec![
            older,
            pre,
            stable_new,
            release_fixture(),
            release_fixture_with_draft(),
        ]);
        assert_eq!(
            sorted.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![9001, 3, 2, 1]
        ); // newest first, stable before prerelease on tie, draft dropped
    }

    fn release_fixture_with_draft() -> GhRelease {
        let mut r = release_fixture();
        r.id = 9999;
        r.draft = true;
        r
    }

    #[test]
    fn rate_limit_error_is_actionable() {
        let err = rate_limit_error();
        let msg = err.to_string();
        assert!(msg.contains("GitHub"), "{msg}");
        assert!(msg.contains("github_token"), "{msg}");
        assert!(is_rate_limit(&err));
        assert!(!is_rate_limit(&Error::provider("GitHub", "GET x -> 404")));
    }

    #[test]
    fn build_meta_fields() {
        let repo: GhRepo = serde_json::from_str(
            r#"{"name":"BigWigs","full_name":"BigWigsMods/BigWigs","html_url":"https://github.com/BigWigsMods/BigWigs",
                "description":"The best boss mod","updated_at":"2026-11-02T08:00:00Z",
                "owner":{"login":"BigWigsMods","avatar_url":"https://avatars.githubusercontent.com/u/1"},"extra":1}"#,
        )
        .unwrap();
        let rel = release_fixture();
        let meta = build_meta("BigWigsMods/BigWigs", &repo, &[(rel, None)]);
        assert_eq!(meta.provider, ProviderKind::GitHub);
        assert_eq!(meta.external_id, "BigWigsMods/BigWigs");
        assert_eq!(meta.name, "BigWigs");
        assert_eq!(meta.author.as_deref(), Some("BigWigsMods"));
        assert_eq!(
            meta.site_url.as_deref(),
            Some("https://github.com/BigWigsMods/BigWigs")
        );
        assert_eq!(
            meta.icon_url.as_deref(),
            Some("https://avatars.githubusercontent.com/u/1")
        );
        assert!(meta.updated_at.unwrap() > 0);
        assert!(!meta.files.is_empty());
    }

    // ---- live smoke (network) ------------------------------------------------

    fn live_ctx() -> AppCtx {
        AppCtx {
            http: net::build_client("addons-forever-test"),
            cfg: std::sync::Arc::new(crate::config::AppConfig::default()),
            app_version: "test",
        }
    }

    #[tokio::test]
    #[ignore = "live GitHub API (FA_LIVE=1)"]
    async fn live_bigwigs_meta() {
        if std::env::var("FA_LIVE").map(|v| v != "1").unwrap_or(true) {
            return;
        }
        let ctx = live_ctx();
        let metas = GitHubProvider
            .meta(&ctx, &["BigWigsMods/BigWigs".to_string()])
            .await
            .expect("BigWigs meta");
        assert_eq!(metas.len(), 1);
        let meta = &metas[0];
        assert_eq!(meta.external_id, "BigWigsMods/BigWigs");
        assert!(!meta.files.is_empty(), "BigWigs has release assets");
        // tolerate repos without a forever release; assert consistency when present
        if meta
            .files
            .iter()
            .any(|f| f.flavors.contains(&Flavor::Forever))
        {
            let picked = GitHubProvider.pick(meta, &sel(Flavor::Forever, Channel::Stable));
            assert!(picked.is_some());
        }
    }
}
