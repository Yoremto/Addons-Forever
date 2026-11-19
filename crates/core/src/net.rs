//! HTTP layer with the privacy guarantee baked in: every request URL's host must
//! be on the addon-provider allowlist (redirects included). No other outbound
//! traffic is possible through this module.

use std::sync::Once;
use std::time::Duration;

use serde::de::DeserializeOwned;

use crate::error::{Error, Result};

/// The complete set of hosts the app will ever talk to, plus "zip URL" installs
/// which are user-added per addon (checked at call site, not here).
pub const ALLOWED_HOSTS: &[&str] = &[
    "api.curseforge.com",
    "edge.forgecdn.net",
    "mediafilez.forgecdn.net",
    "api.github.com",
    "github.com",
    "codeload.github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
    "raw.githubusercontent.com",
    "media.githubusercontent.com",
    "api.tukui.org",
    "downloads.tukui.org",
    "api.mmoui.com",
    "cdn.wowinterface.com",
    "hub.wowup.io",
    "addons.wago.io",
];

/// Largest initial allocation the downloader will ever reserve (the real
/// bound is enforced per received byte while streaming).
const MAX_ALLOC_PREFLIGHT: u64 = 4 * 1024 * 1024 * 1024;

pub fn host_allowed(url: &str) -> bool {
    let Some(host) = https_host(url) else {
        return false;
    };
    ALLOWED_HOSTS.iter().any(|h| host == *h)
}

fn https_host(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    if parsed.scheme() != "https" {
        return None;
    }
    let host = parsed.host_str()?;
    Some(host.trim_end_matches('.').to_ascii_lowercase())
}

static AUTHORIZED_HOSTS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    std::sync::OnceLock::new();

/// Authorize a user-added host (zip URL installs) for the process lifetime,
/// including for redirects. The user typing the URL is the authorization;
/// requests for provider traffic never widen this set.
pub fn authorize_host(host: &str) {
    let set = AUTHORIZED_HOSTS.get_or_init(Default::default);
    if let Ok(mut s) = set.lock() {
        s.insert(host.trim_end_matches('.').to_ascii_lowercase());
    }
}

fn host_authorized(host: &str) -> bool {
    AUTHORIZED_HOSTS
        .get()
        .and_then(|s| s.lock().ok().map(|s| s.contains(host)))
        .unwrap_or(false)
}

/// Allowlist plus per-process user authorizations (zip installs).
fn host_allowed_ext(url: &str) -> bool {
    match https_host(url) {
        Some(h) => ALLOWED_HOSTS.iter().any(|a| *a == h) || host_authorized(&h),
        None => false,
    }
}

/// Enforce the allowlist for provider traffic. `extra_hosts` carries the
/// user-added hosts (zip URL installs), which are checked case-insensitively.
pub fn ensure_allowed(url: &str, extra_hosts: &[String]) -> Result<()> {
    if host_allowed(url) {
        return Ok(());
    }
    if !extra_hosts.is_empty() {
        if let Ok(parsed) = reqwest::Url::parse(url) {
            if parsed.scheme() == "https" {
                if let Some(host) = parsed.host_str() {
                    if extra_hosts.iter().any(|h| h.eq_ignore_ascii_case(host)) {
                        return Ok(());
                    }
                }
            }
        }
    }
    let host = reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_else(|| url.to_string());
    Err(Error::Blocked { host })
}

static CRYPTO_INIT: Once = Once::new();

/// reqwest is built `rustls-no-provider` for a slim, portable dep tree (ring
/// works on Windows GNU cross builds; aws-lc-rs does not) — install ring here.
fn init_crypto() {
    CRYPTO_INIT.call_once(|| {
        rustls::crypto::ring::default_provider()
            .install_default()
            .expect("install rustls ring provider");
    });
}

pub fn build_client(user_agent: &str) -> reqwest::Client {
    init_crypto();
    reqwest::Client::builder()
        .user_agent(user_agent.to_string())
        .timeout(Duration::from_secs(600))
        .connect_timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            // privacy: never follow a redirect off the allowlist
            if attempt.previous().len() >= 10 {
                return attempt.error("too many redirects");
            }
            if host_allowed_ext(attempt.url().as_str()) {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .build()
        .expect("build http client")
}

/// GET and parse JSON with the allowlist enforced.
pub async fn get_json<T: DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
) -> Result<T> {
    ensure_allowed(url, &[])?;
    let mut req = client.get(url);
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let resp = req.send().await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let snippet: String = body.chars().take(300).collect();
        return Err(Error::provider(
            "http",
            format!("GET {url} -> {status}: {snippet}"),
        ));
    }
    Ok(resp.json::<T>().await?)
}

/// Stream a download to disk with progress callbacks (bytes, total).
/// `max_bytes`, when set, is enforced while streaming: the transfer aborts as
/// soon as more than that many bytes have been received, so the cap is a real
/// bound and not a post-hoc check.
pub async fn download_to(
    client: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
    dest: &std::path::Path,
    extra_hosts: &[String],
    max_bytes: Option<u64>,
    on_progress: &mut (dyn FnMut(u64, Option<u64>) + Send),
) -> Result<Vec<u8>> {
    ensure_allowed(url, extra_hosts)?;
    if let Some(size) = max_bytes {
        // a huge declared Content-Length must not drive a huge reservation
        // before a single body byte arrives
        if size > MAX_ALLOC_PREFLIGHT {
            return Err(Error::provider("http", format!("{url}: declared size {size} exceeds the limit")));
        }
    }
    let mut req = client.get(url);
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let resp = req.send().await?;
    let status = resp.status();
    if !status.is_success() {
        return Err(Error::provider("http", format!("GET {url} -> {status}")));
    }
    let total = resp.content_length();
    if let (Some(cap), Some(t)) = (max_bytes, total) {
        if t > cap {
            return Err(Error::provider("http", format!("{url}: content length {t} exceeds the {cap} byte cap")));
        }
    }
    // never reserve remote-supplied capacity beyond a modest floor
    let mut buf = Vec::with_capacity(total.unwrap_or(0).min(4 * 1024 * 1024) as usize);
    let stream = resp.bytes_stream();
    use futures::StreamExt;
    let mut last_report = std::time::Instant::now();
    futures::pin_mut!(stream);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        buf.extend_from_slice(&chunk);
        if let Some(cap) = max_bytes {
            if buf.len() as u64 > cap {
                return Err(Error::provider("http", format!("{url}: exceeded the {cap} byte cap during transfer")));
            }
        }
        if last_report.elapsed() >= Duration::from_millis(200) {
            on_progress(buf.len() as u64, total);
            last_report = std::time::Instant::now();
        }
    }
    on_progress(buf.len() as u64, total);
    tokio::fs::write(dest, &buf).await?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist() {
        assert!(host_allowed("https://api.curseforge.com/v1/mods/1"));
        assert!(host_allowed("https://edge.forgecdn.net/files/9299/959/x.zip"));
        assert!(host_allowed("https://api.github.com/repos/a/b"));
        assert!(!host_allowed("https://evil.com/x"));
        assert!(!host_allowed("http://api.curseforge.com/v1")); // no plaintext
        assert!(!host_allowed("https://sub.api.github.com.evil.net/"));
        assert!(ensure_allowed("https://evil.com/x.zip", &["evil.com".into()]).is_ok());
        assert!(ensure_allowed("https://evil.com/x.zip", &[]).is_err());
    }
}
