//! Zip-URL provider (spec §6).
//!
//! The trivial escape hatch: an addon *is* a download URL. external_id = the https
//! URL; `meta` HEADs (GET fallback) the URL to learn size/date and yields a single
//! RemoteFile with the query-less filename as version. No catalog: scan and search
//! are unsupported/no-op. The privacy allowlist is enforced by `net`; user-added
//! hosts (extra_hosts) are checked by the engine at download time.

use async_trait::async_trait;

use crate::error::{Error, Result};
use crate::net;
use crate::types::{
    AppCtx, Channel, FileSelect, Provider, ProviderKind, RemoteFile, RemoteMeta, ScanMatch,
    ScanRequest, SearchPage,
};

#[derive(Default)]
pub struct ZipProvider;

impl ZipProvider {
    pub fn new() -> Self {
        ZipProvider
    }
}

/// (file_name, version) of a URL: the path's file name, query stripped,
/// a trailing `.zip` removed from the version.
pub fn file_name_version(url: &str) -> Option<(String, String)> {
    let parsed = reqwest::Url::parse(url.trim()).ok()?;
    if parsed.scheme() != "https" {
        return None;
    }
    let name = parsed
        .path()
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())?
        .to_string();
    let version = match name.len() {
        n if n > 4 && name[n - 4..].eq_ignore_ascii_case(".zip") => name[..n - 4].to_string(),
        _ => name.clone(),
    };
    Some((name, version))
}

/// RemoteFile from a URL + probe headers (etag / last-modified / content-length),
/// with the documented id preference: etag, then date, then the URL itself.
pub fn build_file(
    url: &str,
    etag: Option<&str>,
    last_modified: Option<&str>,
    len: Option<u64>,
) -> Result<RemoteFile> {
    let (file_name, version) = file_name_version(url)
        .ok_or_else(|| Error::provider("Zip URL", format!("invalid https URL: {url}")))?;
    let date = last_modified.and_then(|d| {
        chrono::DateTime::parse_from_rfc2822(d)
            .ok()
            .map(|t| t.timestamp())
    });
    let etag_clean = etag.map(|e| {
        e.trim()
            .trim_start_matches("W/")
            .trim_matches('"')
            .to_string()
    });
    let file_id = etag_clean
        .filter(|e| !e.is_empty())
        .or_else(|| {
            last_modified
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| url.to_string());
    Ok(RemoteFile {
        file_id,
        version,
        channel: Channel::Stable,
        file_name,
        url: url.trim().to_string(),
        headers: Default::default(),
        flavors: Vec::new(),
        flavor_meta: false,
        date,
        size: len,
        md5: None,
        sha1: None,
        changelog_url: None,
    })
}

fn header_str(resp: &reqwest::Response, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

/// HEAD the URL, falling back to GET when the server rejects HEAD. Allowlist
/// enforced via `net` (user-added hosts are checked at download by the engine).
async fn probe(ctx: &AppCtx, url: &str) -> Result<reqwest::Response> {
    // AF-19: the URL the user supplied is itself the authorization; authorize
    // the host for probe, download, and redirects on this exact origin.
    let host = reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .ok_or_else(|| Error::provider("Zip URL", format!("cannot parse URL: {url}")))?;
    net::ensure_allowed(url, &[host.clone()])?;
    net::authorize_host(&host);
    let head = ctx.http.head(url).send().await?;
    if head.status().is_success() {
        return Ok(head);
    }
    let get = ctx.http.get(url).send().await?;
    if get.status().is_success() {
        return Ok(get);
    }
    Err(Error::provider(
        "Zip URL",
        format!("HEAD/GET {url} -> {}/{}", head.status(), get.status()),
    ))
}

#[async_trait]
impl Provider for ZipProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Zip
    }

    async fn meta(&self, ctx: &AppCtx, ids: &[String]) -> Result<Vec<RemoteMeta>> {
        ctx.ensure_online()?;
        let mut out = Vec::new();
        for raw in ids {
            let url = raw.trim();
            let Some((file_name, _)) = file_name_version(url) else {
                tracing::warn!(target: "fa::zip", "skipping non-https url {url:?}");
                continue;
            };
            let resp = probe(ctx, url).await?;
            let file = build_file(
                url,
                header_str(&resp, "etag").as_deref(),
                header_str(&resp, "last-modified").as_deref(),
                resp.content_length(),
            )?;
            out.push(RemoteMeta {
                provider: ProviderKind::Zip,
                external_id: url.to_string(),
                name: file_name,
                author: None,
                site_url: Some(url.to_string()),
                repo_url: None,
                description: None,
                icon_url: None,
                downloads: None,
                updated_at: file.date,
                files: vec![file],
            });
        }
        Ok(out)
    }

    async fn scan(&self, _ctx: &AppCtx, _req: &ScanRequest<'_>) -> Result<Vec<ScanMatch>> {
        Ok(Vec::new())
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
            "Zip URL installs have no catalog — paste a direct download URL".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_parsing() {
        let (name, version) =
            file_name_version("https://cdn.example.com/path/MyAddon-1.2.3.zip?token=abc").unwrap();
        assert_eq!(name, "MyAddon-1.2.3.zip"); // query stripped from the file name
        assert_eq!(version, "MyAddon-1.2.3");
        let (name, version) = file_name_version("https://x.io/a/addon.ZIP").unwrap();
        assert_eq!((name.as_str(), version.as_str()), ("addon.ZIP", "addon"));
        let (name, version) = file_name_version("https://x.io/files/v1.0").unwrap();
        assert_eq!((name.as_str(), version.as_str()), ("v1.0", "v1.0"));
        assert_eq!(file_name_version("https://x.io/"), None);
        assert_eq!(file_name_version("http://x.io/a.zip"), None); // plaintext rejected
        assert_eq!(file_name_version("not a url"), None);
        assert_eq!(file_name_version(""), None);
    }

    #[test]
    fn build_file_id_preference_and_meta_shape() {
        let url = "https://github.com/o/r/releases/download/v2/AddOn-2.0.zip";
        // etag wins (weak marker + quotes stripped)
        let f = build_file(
            url,
            Some("W/\"abc123\""),
            Some("Fri, 31 Oct 2025 10:00:00 GMT"),
            Some(1234),
        )
        .unwrap();
        assert_eq!(f.file_id, "abc123");
        assert_eq!(f.version, "AddOn-2.0");
        assert_eq!(f.file_name, "AddOn-2.0.zip");
        assert_eq!(f.url, url);
        assert_eq!(f.size, Some(1234));
        assert_eq!(f.flavors, Vec::new());
        assert_eq!(f.channel, Channel::Stable);
        assert_eq!(f.date, Some(1_761_904_800)); // RFC2822 Last-Modified parsed
                                                 // no etag → date; no date → url
        let f = build_file(url, None, Some("Fri, 31 Oct 2025 10:00:00 GMT"), None).unwrap();
        assert_eq!(f.file_id, "Fri, 31 Oct 2025 10:00:00 GMT");
        let f = build_file(url, None, None, None).unwrap();
        assert_eq!(f.file_id, url);
        // bad url → provider error, no panic
        assert!(build_file("http://x/y.zip", None, None, None).is_err());
    }

    #[test]
    fn scan_is_noop_and_search_unsupported() {
        // scan never runs for zip (not in scan_order) and must not error
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let ctx = AppCtx {
                http: net::build_client("addons-forever-test"),
                cfg: std::sync::Arc::new(crate::config::AppConfig::default()),
                app_version: "test",
            };
            let sel = FileSelect {
                flavor: crate::flavor::Flavor::Forever,
                interface: 16001,
                version: "1.60.1".into(),
                channel: Channel::Stable,
            };
            let req = ScanRequest {
                folders: &[],
                sel: &sel,
            };
            assert!(ZipProvider.scan(&ctx, &req).await.unwrap().is_empty());
            let err = ZipProvider
                .search(&ctx, "x", 0, 20, &sel)
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Unsupported(_)), "{err}");
            // AF-19: the URL the user supplied is the authorization; the
            // probe for a plain https host outside the provider list is no
            // longer blocked (it reaches the network: a transport error, not
            // Error::Blocked, proves the authorization)
            let ids = ["https://evil.example/addon.zip".to_string()];
            let err = ZipProvider.meta(&ctx, &ids).await.unwrap_err();
            assert!(!matches!(err, Error::Blocked { .. }), "user host must be authorized: {err}");
            // plaintext is still refused (meta skips non-https URLs; the
            // privacy layer refuses them outright)
            assert!(
                crate::net::ensure_allowed("http://evil.example/addon.zip", &["evil.example".into()]).is_err(),
                "http must never be allowed, even for a user host"
            );
        });
    }
}
