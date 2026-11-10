//! Install pipeline: download → verify → sanitize-extract → layout detection →
//! backup / swap / rollback into the AddOns directory.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use md5::Md5;
use sha1::Sha1;

use crate::error::{Error, Result};
use crate::net;
use crate::toc;
use crate::types::{AppCtx, ProviderKind, RemoteFile, RemoteMeta};
use crate::store::AddonState;

/// Hard cap for downloaded archives (addon zips are tiny; guards zip bombs).
pub const MAX_DOWNLOAD_BYTES: u64 = 200 * 1024 * 1024;
/// Cap for a single unpacked entry.
const MAX_ENTRY_BYTES: u64 = 512 * 1024 * 1024;
/// Cap for the total unpacked size.
const MAX_UNPACKED_TOTAL: u128 = 2 * 1024 * 1024 * 1024;
/// Cap for the number of archive entries (inode/CPU exhaustion guard).
const MAX_ENTRIES: usize = 20_000;

/// Install progress notifications.
#[derive(Clone, Debug, PartialEq)]
pub enum InstallProgress {
    Downloading { bytes: u64, total: Option<u64> },
    Extracting,
    BackingUp,
    Swapping,
    Done,
}

/// What landed on disk after a successful install.
#[derive(Clone, Debug, Default)]
pub struct InstallOutcome {
    /// Folder names inside AddOns (the addon roots that were applied).
    pub folders: Vec<String>,
    /// Version parsed from the installed TOCs.
    pub version: Option<String>,
    /// Total bytes of the installed addon folders.
    pub size_bytes: u64,
    /// `## Interface:` numbers found in the installed TOCs (sorted, unique).
    pub interfaces: Vec<u32>,
}

fn zip_err(e: impl std::fmt::Display) -> Error {
    Error::Zip(e.to_string())
}

/// Scratch dir the app owns: `<AddOns>/../.fa-tmp` (downloads, extraction,
/// timestamped backups).
pub fn fa_tmp_dir(addons_dir: &Path) -> PathBuf {
    addons_dir.parent().unwrap_or(addons_dir).join(".fa-tmp")
}

fn url_host(url: &str) -> Option<String> {
    reqwest::Url::parse(url).ok().and_then(|u| u.host_str().map(|h| h.to_string()))
}

/// Case-insensitive hex digest check.
pub(crate) fn verify_hashes(bytes: &[u8], md5_hex: Option<&str>, sha1_hex: Option<&str>) -> Result<()> {
    if let Some(want) = md5_hex.map(str::trim).filter(|s| !s.is_empty()) {
        use md5::Digest;
        let mut h = Md5::new();
        h.update(bytes);
        let got = format!("{:x}", h.finalize());
        if !got.eq_ignore_ascii_case(want) {
            return Err(Error::provider("install", format!("md5 mismatch: expected {want}, got {got}")));
        }
    }
    if let Some(want) = sha1_hex.map(str::trim).filter(|s| !s.is_empty()) {
        use sha1::Digest;
        let mut h = Sha1::new();
        h.update(bytes);
        let got = format!("{:x}", h.finalize());
        if !got.eq_ignore_ascii_case(want) {
            return Err(Error::provider("install", format!("sha1 mismatch: expected {want}, got {got}")));
        }
    }
    Ok(())
}

/// True when `name` is safe as a direct child of AddOns: a single path
/// component, never `.`, `..`, empty, or containing separators.
pub fn is_safe_folder_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
}

/// Shrink a zip entry/file name into a safe single folder-name component.
fn sanitize_folder_name(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let mut s: String = base.trim().chars().filter(|c| !matches!(c, '/' | '\\' | '\0')).collect();
    if s.is_empty() || s.chars().all(|c| c == '.') {
        s = "addon".to_string();
    }
    if s.len() > 120 {
        let mut cut = 120;
        while cut > 0 && !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
    s
}

fn zip_stem(file_name: &str) -> String {
    let base = file_name.rsplit(['/', '\\']).next().unwrap_or(file_name);
    let stem = Path::new(base)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| base.to_string());
    sanitize_folder_name(&stem)
}

fn dir_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

/// Hard rejection of hostile raw entry names (before the zip crate's own
/// sanitization, which strips drive/absolute prefixes instead of refusing):
/// NUL, absolute paths, drive prefixes (`C:`), UNC roots, any `..` component.
fn is_unsafe_entry_name(raw: &str) -> bool {
    if raw.contains('\0') || raw.starts_with('/') || raw.starts_with('\\') {
        return true;
    }
    let b = raw.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        return true;
    }
    raw.split(['/', '\\']).any(|c| c == "..")
}

/// Extract with full path sanitization (CVE-2026-115905 hardening): local vs
/// central header consistency enforced, entry names must resolve inside the
/// destination (`ZipFile::enclosed_name` — rejects `..`, absolute and drive
/// paths), symlinks refused, sizes capped.
fn extract_zip(bytes: &[u8], dest: &Path) -> Result<()> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(zip_err)?;
    archive
        .validate_header_consistency()
        .map_err(|e| zip_err(format!("header/local entry mismatch (corrupt archive): {e}")))?;
    if let Some(total) = archive.decompressed_size() {
        if total > MAX_UNPACKED_TOTAL {
            return Err(zip_err("archive unpacks beyond the size limit"));
        }
    }
    if archive.len() > MAX_ENTRIES {
        return Err(zip_err(format!(
            "archive has too many entries ({} > {})",
            archive.len(),
            MAX_ENTRIES
        )));
    }

    let mut entries: Vec<(usize, PathBuf, bool)> = Vec::new();
    for i in 0..archive.len() {
        let zf = archive.by_index(i).map_err(zip_err)?;
        let raw_name = zf.name().map_err(zip_err)?.into_owned();
        if is_unsafe_entry_name(&raw_name) {
            return Err(zip_err(format!("unsafe entry path: {raw_name}")));
        }
        let rel = zf
            .enclosed_name()
            .ok_or_else(|| zip_err(format!("unsafe entry path: {raw_name}")))?;
        if rel.as_os_str().is_empty() {
            continue; // archive-root marker
        }
        if zf.size() > MAX_ENTRY_BYTES {
            return Err(zip_err(format!("entry unpacks beyond the size limit: {raw_name}")));
        }
        if zf.unix_mode().map(|m| m & 0o170_000) == Some(0o120_000) {
            return Err(zip_err(format!("symlink entries are not allowed: {raw_name}")));
        }
        entries.push((i, rel, zf.is_dir()));
    }

    std::fs::create_dir_all(dest)?;
    let dest = dest.canonicalize()?;
    for (i, rel, is_dir) in entries {
        let target = dest.join(&rel);
        let parent = target.parent().unwrap_or(dest.as_path());
        std::fs::create_dir_all(parent)?;
        // defense in depth: parent must resolve under the extraction root
        if !parent.canonicalize()?.starts_with(&dest) {
            return Err(zip_err(format!("entry escapes the extraction root: {}", rel.display())));
        }
        if is_dir {
            std::fs::create_dir_all(&target)?;
            continue;
        }
        let mut zf = archive.by_index(i).map_err(zip_err)?;
        let mut out = std::fs::File::create(&target)?;
        // real received bytes, not the header's declared size
        let mut limited = std::io::Read::take(&mut zf, MAX_ENTRY_BYTES + 1);
        let n = std::io::copy(&mut limited, &mut out)?;
        if n > MAX_ENTRY_BYTES {
            let _ = std::fs::remove_file(&target);
            return Err(zip_err(format!("entry unpacks beyond the size limit: {}", rel.display())));
        }
        out.flush()?;
    }
    Ok(())
}

/// Detect addon roots in an extraction directory.
///
/// - Single top-level dir containing a `.toc` → that dir is the addon.
/// - Every top-level dir that directly contains a `.toc` matching its name is
///   an addon folder (bundles); loose extras (READMEs, non-toc dirs) dropped.
/// - Otherwise all top-level content becomes one root named after the zip.
///
/// Returns `(src_dir, target_folder_name)` pairs.
fn detect_roots(extract: &Path, zip_name: &str, flavor: crate::flavor::Flavor) -> Result<Vec<(PathBuf, String)>> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut files: Vec<PathBuf> = Vec::new();
    for e in std::fs::read_dir(extract)? {
        let e = e?;
        let p = e.path();
        if p.is_dir() {
            dirs.push(p);
        } else {
            files.push(p);
        }
    }
    dirs.sort();
    files.sort();
    if dirs.is_empty() && files.is_empty() {
        return Err(zip_err("archive is empty"));
    }

    if dirs.len() == 1 && files.is_empty() {
        let d = &dirs[0];
        if toc::find_toc(d, "", flavor).is_some() {
            return Ok(vec![(d.clone(), dir_name(d))]);
        }
    }

    let named: Vec<&PathBuf> = dirs
        .iter()
        .filter(|d| toc::find_toc(d, &dir_name(d), flavor).is_some())
        .collect();
    if !named.is_empty() {
        return Ok(named.into_iter().map(|d| (d.clone(), dir_name(d))).collect());
    }

    // no recognizable addon dirs: pack everything into a root named after the zip
    let root_name = sanitize_folder_name(zip_name);
    let root = extract.join(&root_name);
    std::fs::create_dir_all(&root)?;
    for p in dirs.iter().chain(files.iter()) {
        if p == &root {
            continue;
        }
        move_dir(p, &root.join(dir_name(p)))?;
    }
    Ok(vec![(root, root_name)])
}

/// Copy a tree, skipping symlinks (never following them out of the tree).
fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let from = e.path();
        let to = dst.join(e.file_name());
        let ft = e.file_type()?; // does not follow symlinks
        if ft.is_dir() {
            copy_dir_all(&from, &to)?;
        } else if ft.is_file() {
            std::fs::copy(&from, &to)?;
        } // symlinks and specials: skipped by design
    }
    Ok(())
}

/// Move a directory, falling back to copy+remove across filesystems.
fn move_dir(src: &Path, dst: &Path) -> Result<()> {
    match std::fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(_) => {
            copy_dir_all(src, dst)?;
            std::fs::remove_dir_all(src)?;
            Ok(())
        }
    }
}

/// Move installed folders back out of AddOns after a swap failed. Returns the
/// names that could not be restored (caller must surface them).
fn restore_backups(backup_dir: &Path, names: &[String], addons_dir: &Path) -> Vec<String> {
    let mut failed = Vec::new();
    for name in names {
        let dst = addons_dir.join(name);
        let src = backup_dir.join(name);
        let _ = std::fs::remove_dir_all(&dst); // partial new install
        if src.is_dir()
            && std::fs::rename(&src, &dst).is_err()
            && copy_dir_all(&src, &dst).is_err()
        {
            failed.push(name.clone());
        }
    }
    failed
}

/// Backup-then-swap. Existing folders are moved (renamed when possible) into
/// `backup_dir` first, so an interrupted swap never leaves a folder half
/// deleted: the previous content is recoverable from the backup directory.
fn do_swap(addons_dir: &Path, roots: &[(PathBuf, String)], names: &[String], backup_dir: &Path) -> Result<()> {
    let mut backed_up: Vec<String> = Vec::new();
    for name in names {
        let existing = addons_dir.join(name);
        if existing.exists() {
            std::fs::create_dir_all(backup_dir)?;
            if let Err(e) = move_dir(&existing, &backup_dir.join(name)) {
                // undo the moves we already made, then bail
                let _ = restore_backups(backup_dir, &backed_up, addons_dir);
                return Err(e);
            }
            backed_up.push(name.clone());
        }
    }
    for (src, name) in roots {
        if let Err(e) = move_dir(src, &addons_dir.join(name)) {
            let mut undo = backed_up.clone();
            if !undo.contains(name) {
                undo.push(name.clone());
            }
            let failed = restore_backups(backup_dir, &undo, addons_dir);
            let mut msg = format!("install failed: {e}");
            if !failed.is_empty() {
                msg.push_str(&format!(
                    "; ROLLBACK FAILED for [{}, restore manually from {}]",
                    failed.join(", "),
                    backup_dir.display()
                ));
            }
            return Err(Error::provider("install", msg));
        }
    }
    Ok(())
}

/// Keep the newest `keep` snapshots under `backup_root/<key>/`, delete older
/// ones. Snapshots are named by millisecond timestamps.
pub(crate) fn prune_backups(backup_root: &Path, key: &str, keep: usize) -> Result<()> {
    let key_dir = backup_root.join(snapshot_slug(key));
    if !key_dir.is_dir() {
        return Ok(());
    }
    let mut snaps: Vec<i64> = std::fs::read_dir(&key_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().to_string_lossy().parse::<i64>().ok())
        .collect();
    snaps.sort_unstable();
    snaps.reverse();
    for old in snaps.iter().skip(keep) {
        let _ = std::fs::remove_dir_all(key_dir.join(old.to_string()));
    }
    Ok(())
}

/// Filesystem-safe slug for a store key in backup paths.
fn snapshot_slug(key: &str) -> String {
    let s: String = key
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if s.is_empty() { "unknown".into() } else { s }
}

/// Per-transaction backup snapshot dir: `backup/<addon key>/<millis>`.
fn snapshot_dir(tmp: &Path, key: &str) -> PathBuf {
    tmp.join("backup")
        .join(snapshot_slug(key))
        .join(chrono::Utc::now().timestamp_millis().to_string())
}

/// Download an archive into the `.fa-tmp` scratch dir and return its bytes
/// (allowlist enforced by net, 200 MiB cap, provider headers attached).
pub(crate) async fn download_zip(
    ctx: &AppCtx,
    addons_dir: &Path,
    meta: &RemoteMeta,
    file: &RemoteFile,
    on_progress: &mut (dyn FnMut(u64, Option<u64>) + Send),
) -> Result<Vec<u8>> {
    let tmp = fa_tmp_dir(addons_dir);
    let dl_dir = tmp.join("downloads");
    std::fs::create_dir_all(&dl_dir)?;
    let zip_path = dl_dir.join(format!("{}.zip", uuid::Uuid::new_v4()));

    let headers: Vec<(String, String)> =
        file.headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    // zip-URL installs point at user-added hosts; authorize exactly those hosts.
    let mut extra_hosts: Vec<String> = Vec::new();
    if meta.provider == ProviderKind::Zip {
        for u in [&meta.external_id, &file.url] {
            if let Some(h) = url_host(u) {
                extra_hosts.push(h);
            }
        }
    }
    // pre-flight: a provider-announced size over the cap never starts
    if file.size.is_some_and(|s| s > MAX_DOWNLOAD_BYTES) {
        return Err(Error::provider("install", "download exceeds the 200 MiB cap"));
    }

    let downloaded = net::download_to(
        &ctx.http,
        &file.url,
        &headers,
        &zip_path,
        &extra_hosts,
        Some(MAX_DOWNLOAD_BYTES),
        &mut |bytes, total| on_progress(bytes, total),
    )
    .await;
    let bytes = match downloaded {
        Ok(b) => b,
        Err(e) => {
            let _ = std::fs::remove_file(&zip_path);
            return Err(e);
        }
    };
    let _ = std::fs::remove_file(&zip_path);
    Ok(bytes)
}

/// Install one remote file: download (allowlisted, capped), hash-verify,
/// sanitized extract, layout detection, backup + swap with rollback.
/// `owned` maps lowercased AddOns folder names to the store key that owns
/// them; `my_key` is the addon being installed. Folders owned by a *different*
/// addon are never replaced (AF-06).
pub async fn install(
    ctx: &AppCtx,
    addons_dir: &Path,
    meta: &RemoteMeta,
    file: &RemoteFile,
    owned: &std::collections::BTreeMap<String, String>,
    my_key: &str,
    on_progress: &mut (dyn FnMut(InstallProgress) + Send),
) -> Result<InstallOutcome> {
    ctx.ensure_online()?;
    let bytes = download_zip(ctx, addons_dir, meta, file, &mut |bytes, total| {
        on_progress(InstallProgress::Downloading { bytes, total });
    })
    .await?;
    install_from_bytes(ctx, addons_dir, meta, file, bytes, owned, my_key, on_progress).await
}

/// The network-free half of [`install`]: verify, extract, detect, apply.
pub(crate) async fn install_from_bytes(
    ctx: &AppCtx,
    addons_dir: &Path,
    _meta: &RemoteMeta,
    file: &RemoteFile,
    bytes: Vec<u8>,
    owned: &std::collections::BTreeMap<String, String>,
    my_key: &str,
    on_progress: &mut (dyn FnMut(InstallProgress) + Send),
) -> Result<InstallOutcome> {
    verify_hashes(&bytes, file.md5.as_deref(), file.sha1.as_deref())?;

    let tmp = fa_tmp_dir(addons_dir);
    let extract = tmp.join("extract").join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir_all(&extract)?;

    on_progress(InstallProgress::Extracting);
    let flavor = ctx.cfg.app.flavor;
    let roots = match extract_zip(&bytes, &extract)
        .and_then(|()| detect_roots(&extract, &zip_stem(&file.file_name), flavor))
    {
        Ok(r) => r,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&extract);
            return Err(e);
        }
    };

    let names: Vec<String> = roots.iter().map(|(_, n)| n.clone()).collect();
    for n in &names {
        if !is_safe_folder_name(n) {
            let _ = std::fs::remove_dir_all(&extract);
            return Err(Error::provider("install", format!("unsafe addon folder name: {n}")));
        }
    }
    for (i, a) in names.iter().enumerate() {
        for b in &names[i + 1..] {
            if a.eq_ignore_ascii_case(b) {
                let _ = std::fs::remove_dir_all(&extract);
                return Err(Error::provider("install", format!("archive contains duplicate folders: {a}")));
            }
        }
    }
    // AF-06: never clobber folders that belong to a different managed addon.
    for n in &names {
        if let Some(owner) = owned.get(&n.to_lowercase()) {
            if !owner.eq_ignore_ascii_case(my_key) {
                let _ = std::fs::remove_dir_all(&extract);
                return Err(Error::provider(
                    "install",
                    format!("archive contains folder {n}, which is owned by {owner}; remove that addon first"),
                ));
            }
        }
    }

    on_progress(InstallProgress::BackingUp);
    let backup_dir = snapshot_dir(&tmp, my_key);
    let existing_any = names.iter().any(|n| addons_dir.join(n).exists());

    on_progress(InstallProgress::Swapping);
    if let Err(e) = do_swap(addons_dir, &roots, &names, &backup_dir) {
        let _ = std::fs::remove_dir_all(&extract);
        return Err(e);
    }
    let _ = std::fs::remove_dir_all(&extract);

    // Re-parse the installed TOCs for the authoritative installed state.
    // A read failure here is not fatal: the install already succeeded, the
    // version is simply unknown until the next scan.
    let mut version = None;
    let mut interfaces: Vec<u32> = Vec::new();
    let mut size_bytes = 0u64;
    for name in &names {
        let dir = addons_dir.join(name);
        size_bytes += dir_size(&dir);
        if let Some(toc_path) = toc::find_toc(&dir, name, flavor) {
            match std::fs::read(&toc_path) {
                Ok(raw) => {
                    let parsed = toc::parse(&toc::decode_text(&raw));
                    if version.is_none() {
                        version = parsed.version.clone();
                    }
                    for i in parsed.interface {
                        if !interfaces.contains(&i) {
                            interfaces.push(i);
                        }
                    }
                }
                Err(e) => tracing::warn!("installed TOC unreadable ({name}): {e}"),
            }
        }
    }
    interfaces.sort_unstable();

    // retention: keep the newest N snapshots for this addon (0 = none)
    if existing_any {
        let keep = if ctx.cfg.updates.keep_backups { 3 } else { 0 };
        let _ = prune_backups(&tmp.join("backup"), my_key, keep);
    }

    on_progress(InstallProgress::Done);
    Ok(InstallOutcome { folders: names, version, interfaces, size_bytes })
}

/// After a successful update, remove folders the old version owned and the
/// new one no longer ships (AF-08). Folders currently owned by a *different*
/// addon are left alone. `owned` is the pre-update ownership map. Returns the
/// removed names.
pub fn cleanup_dropped_roots(
    addons_dir: &Path,
    old_folders: &[String],
    new_folders: &[String],
    owned: &std::collections::BTreeMap<String, String>,
    my_key: &str,
) -> Vec<String> {
    let mut removed = Vec::new();
    for f in old_folders {
        if new_folders.iter().any(|n| n.eq_ignore_ascii_case(f)) {
            continue;
        }
        let owned_by_me = owned
            .get(&f.to_lowercase())
            .map(|k| k.eq_ignore_ascii_case(my_key))
            .unwrap_or(false);
        if !owned_by_me {
            continue; // not ours to remove
        }
        if !is_safe_folder_name(f) {
            continue;
        }
        let path = addons_dir.join(f);
        if path.is_dir() && std::fs::remove_dir_all(&path).is_ok() {
            removed.push(f.clone());
        }
    }
    removed
}

/// Total size in bytes of a directory tree (missing/erroneous entries ignored).
pub fn dir_size(dir: &std::path::Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(meta) = e.metadata() else { continue };
            if meta.is_dir() {
                stack.push(p);
            } else if meta.is_file() {
                total += meta.len();
            }
        }
    }
    total
}

/// Remove every folder owned by a state from AddOns. Safety: each entry must
/// be a plain folder name (never a path, never `.`/`..`).
pub async fn uninstall(_ctx: &AppCtx, addons_dir: &Path, state: &AddonState) -> Result<()> {
    for folder in &state.folders {
        if !is_safe_folder_name(folder) {
            return Err(Error::provider(
                "install",
                format!("refusing to remove unsafe folder name: {folder:?}"),
            ));
        }
        let path = addons_dir.join(folder);
        if path.is_dir() {
            std::fs::remove_dir_all(&path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::types::Channel;
    use std::collections::BTreeMap;
    use std::io::Cursor;
    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;

    fn ctx() -> AppCtx {
        AppCtx {
            http: net::build_client("test/0"),
            cfg: std::sync::Arc::new(AppConfig::default()),
            app_version: "test",
        }
    }

    fn build_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut z = ZipWriter::new(Cursor::new(Vec::new()));
        let opts = SimpleFileOptions::default();
        for (name, data) in entries {
            z.start_file(*name, opts).unwrap();
            z.write_all(data).unwrap();
        }
        z.finish().unwrap().into_inner()
    }

    fn hex_md5(bytes: &[u8]) -> String {
        use md5::Digest;
        let mut h = Md5::new();
        h.update(bytes);
        format!("{:x}", h.finalize())
    }

    fn hex_sha1(bytes: &[u8]) -> String {
        use sha1::Digest;
        let mut h = Sha1::new();
        h.update(bytes);
        format!("{:x}", h.finalize())
    }

    fn remote(bytes: &[u8]) -> RemoteFile {
        RemoteFile {
            file_id: "f-1".into(),
            version: "1.2.0".into(),
            channel: Channel::Stable,
            file_name: "test-addon.zip".into(),
            url: "https://example.invalid/test-addon.zip".into(),
            headers: BTreeMap::new(),
            flavors: vec![],
            flavor_meta: false,
            date: Some(1_700_000_000),
            size: Some(bytes.len() as u64),
            md5: Some(hex_md5(bytes)),
            sha1: Some(hex_sha1(bytes)),
            changelog_url: None,
        }
    }

    fn meta() -> RemoteMeta {
        RemoteMeta {
            provider: ProviderKind::Curse,
            external_id: "1".into(),
            name: "Test".into(),
            author: None,
            site_url: None,
            repo_url: None,
            description: None,
            icon_url: None,
            downloads: None,
            updated_at: None,
            files: vec![],
        }
    }

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let addons = tmp.path().join("Interface").join("AddOns");
        std::fs::create_dir_all(&addons).unwrap();
        (tmp, addons)
    }

    fn toc_body(name: &str, version: &str) -> Vec<u8> {
        format!("## Title: {name}\n## Version: {version}\n## Interface: 16001\nCore.lua\n").into_bytes()
    }

    #[tokio::test]
    async fn install_roundtrip_replaces_and_rolls_back() {
        let (_tmp, addons) = fixture();
        let ctx = ctx();
        let mut noop = |_| {};

        // v1: top-level addon dir + toc + lua
        let z1 = build_zip(&[
            ("Test/Test.toc", &toc_body("Test", "1.2.0")),
            ("Test/Core.lua", b"-- v1\n".as_slice()),
        ]);
        let mut f1 = remote(&z1);
        f1.version = "1.2.0".into();
        let empty_owned = Default::default();
        let out = install_from_bytes(&ctx, &addons, &meta(), &f1, z1.clone(), &empty_owned, "curse:1", &mut noop).await.unwrap();
        assert_eq!(out.folders, vec!["Test".to_string()]);
        assert_eq!(out.version.as_deref(), Some("1.2.0"));
        assert_eq!(out.interfaces, vec![16001]);
        assert_eq!(std::fs::read(addons.join("Test").join("Core.lua")).unwrap(), b"-- v1\n");
        // the zip download scratch is gone; no stray extract dirs
        assert!(!fa_tmp_dir(&addons).join("downloads").join("x").exists());

        // v2 replaces v1, backup of v1 is kept
        let z2 = build_zip(&[
            ("Test/Test.toc", &toc_body("Test", "1.3.0")),
            ("Test/Core.lua", b"-- v2\n".as_slice()),
        ]);
        let mut f2 = remote(&z2);
        f2.version = "1.3.0".into();
        f2.file_id = "f-2".into();
        let out2 = install_from_bytes(&ctx, &addons, &meta(), &f2, z2.clone(), &empty_owned, "curse:1", &mut noop).await.unwrap();
        assert_eq!(out2.version.as_deref(), Some("1.3.0"));
        assert_eq!(std::fs::read(addons.join("Test").join("Core.lua")).unwrap(), b"-- v2\n");
        let backups = fa_tmp_dir(&addons).join("backup").join("curse_1");
        assert!(backups.is_dir(), "per-addon backup snapshots were created");
        assert_eq!(std::fs::read_dir(&backups).unwrap().count(), 1);

        // corrupt zip (truncated) → error, installed v2 stays intact
        let mut truncated = z2.clone();
        truncated.truncate(z2.len() / 2);
        let mut fb = remote(&truncated); // hashes recomputed on truncated bytes
        fb.md5 = Some(hex_md5(&truncated));
        fb.sha1 = Some(hex_sha1(&truncated));
        let err = install_from_bytes(&ctx, &addons, &meta(), &fb, truncated, &empty_owned, "curse:1", &mut noop).await;
        assert!(err.is_err(), "corrupt archive must fail: {err:?}");
        assert_eq!(std::fs::read(addons.join("Test").join("Core.lua")).unwrap(), b"-- v2\n");
    }

    #[tokio::test]
    async fn hash_mismatch_is_rejected_and_touches_nothing() {
        let (_tmp, addons) = fixture();
        let ctx = ctx();
        let z = build_zip(&[("X/X.toc", &toc_body("X", "1.0"))]);
        let mut f = remote(&z);
        f.md5 = Some("00000000000000000000000000000000".into());
        let empty_owned = Default::default();
        let err = install_from_bytes(&ctx, &addons, &meta(), &f, z.clone(), &empty_owned, "curse:1", &mut |_| {})
            .await
            .expect_err("md5 mismatch must fail");
        assert!(err.to_string().contains("md5"));
        let mut f2 = remote(&z);
        f2.sha1 = Some("0000000000000000000000000000000000000000".into());
        assert!(install_from_bytes(&ctx, &addons, &meta(), &f2, z, &empty_owned, "curse:1", &mut |_| {}).await.is_err());
        assert_eq!(std::fs::read_dir(&addons).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn malicious_entry_names_are_rejected() {
        let (_tmp, addons) = fixture();
        let ctx = ctx();
        for bad in ["../evil.txt", "/etc/evil.txt", "C:/Windows/evil.txt", "a/../../evil.lua"] {
            let z = build_zip(&[("Good/Good.toc", &toc_body("Good", "1")), (bad, b"x".as_slice())]);
            let f = remote(&z);
            let empty_owned = Default::default();
            let err = install_from_bytes(&ctx, &addons, &meta(), &f, z, &empty_owned, "curse:1", &mut |_| {})
                .await
                .expect_err(&format!("{bad} must be rejected"));
            assert!(err.to_string().contains("unsafe"), "{bad}: {err}");
        }
        assert_eq!(std::fs::read_dir(&addons).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn layout_detection_bundles_and_loose_files() {
        let (_tmp, addons) = fixture();
        let ctx = ctx();

        // multi-folder bundle (+ loose README): both toc dirs land, README dropped
        let z = build_zip(&[
            ("Bundle/Bundle.toc", &toc_body("Bundle", "2.0")),
            ("Bundle_Libs/Bundle_Libs.toc", &toc_body("Bundle Libs", "2.0")),
            ("README.md", b"readme".as_slice()),
        ]);
        let mut f = remote(&z);
        f.file_name = "bundle.zip".into();
        let empty_owned = Default::default();
        let out = install_from_bytes(&ctx, &addons, &meta(), &f, z, &empty_owned, "curse:1", &mut |_| {}).await.unwrap();
        assert_eq!(out.folders, vec!["Bundle", "Bundle_Libs"]);
        assert!(!addons.join("README.md").exists());

        // loose top-level files → root named after the zip
        let z2 = build_zip(&[("Loose.toc", &toc_body("Loose", "0.1")), ("Loose.lua", b"--\n".as_slice())]);
        let mut f2 = remote(&z2);
        f2.file_name = "loose-thing.zip".into();
        let out2 = install_from_bytes(&ctx, &addons, &meta(), &f2, z2, &empty_owned, "curse:1", &mut |_| {}).await.unwrap();
        assert_eq!(out2.folders, vec!["loose-thing".to_string()]);
        assert!(!addons.join("loose-thing").join("Loose.toc").is_dir());
        assert!(addons.join("loose-thing").join("Loose.toc").is_file());
    }

    #[test]
    fn swap_failure_restores_backups() {
        // Direct test of the backup/swap/restore machinery: two folders are
        // backed up, the swap fails part-way, and restore brings both originals
        // back.
        let (_tmp, addons) = fixture();
        std::fs::create_dir_all(addons.join("A")).unwrap();
        std::fs::write(addons.join("A").join("a.txt"), b"old A").unwrap();
        std::fs::create_dir_all(addons.join("B")).unwrap();
        std::fs::write(addons.join("B").join("b.txt"), b"old B").unwrap();

        let backup = fa_tmp_dir(&addons).join("backup").join("k").join("999");

        let extract = fa_tmp_dir(&addons).join("extract").join("t");
        std::fs::create_dir_all(extract.join("A")).unwrap();
        std::fs::write(extract.join("A").join("a.txt"), b"new A").unwrap();
        std::fs::create_dir_all(extract.join("B")).unwrap();
        std::fs::write(extract.join("B").join("b.txt"), b"new B").unwrap();
        // remove the staged B dir to make the swap fail part-way
        std::fs::remove_dir_all(extract.join("B")).unwrap();

        let roots = vec![
            (extract.join("A"), "A".to_string()),
            (extract.join("B"), "B".to_string()),
        ];
        let names: Vec<String> = vec!["A".into(), "B".into()];
        let err = do_swap(&addons, &roots, &names, &backup).expect_err("swap must fail");
        // AF-07: restore failures are reported, success is silent
        assert!(!err.to_string().contains("ROLLBACK FAILED"), "{err}");
        assert_eq!(std::fs::read(addons.join("A").join("a.txt")).unwrap(), b"old A");
        assert_eq!(std::fs::read(addons.join("B").join("b.txt")).unwrap(), b"old B");
    }

    #[test]
    fn prune_backups_keeps_newest() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("backup");
        for ts in [1, 2, 3, 4, 5] {
            std::fs::create_dir_all(root.join("curse_1").join(ts.to_string())).unwrap();
        }
        prune_backups(&root, "curse:1", 3).unwrap();
        let mut left: Vec<String> = std::fs::read_dir(&root.join("curse_1"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        left.sort();
        assert_eq!(left, ["3", "4", "5"]);
    }

    #[tokio::test]
    async fn uninstall_removes_folders_safely() {
        let (_tmp, addons) = fixture();
        let ctx = ctx();
        std::fs::create_dir_all(addons.join("Del")).unwrap();
        std::fs::write(addons.join("Del").join("x.lua"), b"x").unwrap();
        let mut state = AddonState {
            key: "curse:1".into(),
            provider: ProviderKind::Curse,
            external_id: "1".into(),
            name: "Del".into(),
            site_url: None,
            repo_url: None,
            icon_url: None,
            folders: vec!["Del".into()],
            fingerprints: vec![],
            installed_version: None,
            installed_file_id: None,
            size_bytes: 0,
            channel: Channel::Stable,
            auto_update: false,
            ignored: false,
            manual: false,
            cached_remote: None,
            cached_at: None,
            last_checked: None,
            provider_data: serde_json::Value::Null,
            pending_update: None,
        };
        uninstall(&ctx, &addons, &state).await.unwrap();
        assert!(!addons.join("Del").exists());

        // path-like folder names are refused
        state.folders = vec!["../Interface".into()];
        assert!(uninstall(&ctx, &addons, &state).await.is_err());
        state.folders = vec![".".into()];
        assert!(uninstall(&ctx, &addons, &state).await.is_err());
        assert!(addons.exists());
    }

    #[test]
    fn hash_vectors() {
        // known digest vectors (empty string + classic)
        assert_eq!(hex_md5(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex_md5(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(hex_sha1(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(hex_sha1(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert!(verify_hashes(b"abc", Some("900150983CD24FB0D6963F7D28E17F72"), None).is_ok(), "case-insensitive");
        assert!(verify_hashes(b"abc", None, Some("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef")).is_err());
    }

    #[test]
    fn folder_name_safety() {
        assert!(is_safe_folder_name("Big_Wigs"));
        assert!(!is_safe_folder_name(""));
        assert!(!is_safe_folder_name(".."));
        assert!(!is_safe_folder_name("a/b"));
        assert!(!is_safe_folder_name("a\\b"));
        assert_eq!(zip_stem("my-addon-1.2.3.zip"), "my-addon-1.2.3");
        assert_eq!(zip_stem("path/to/weird..name.zip"), "weird..name");
        assert_eq!(zip_stem("/"), "addon");
    }

    #[tokio::test]
    async fn archive_cannot_clobber_a_foreign_owned_folder() {
        // AF-06: an archive shipping a folder owned by another addon is
        // rejected; the victim's bytes stay untouched.
        let (_tmp, addons) = fixture();
        let ctx = ctx();
        std::fs::create_dir_all(addons.join("Victim")).unwrap();
        std::fs::write(addons.join("Victim").join("v.toc"), toc_body("Victim", "9.9")).unwrap();

        let owned: std::collections::BTreeMap<String, String> =
            [("victim".to_string(), "curse:777".to_string())].into_iter().collect();
        let z = build_zip(&[
            ("Attacker/Attacker.toc", &toc_body("Attacker", "1.0")),
            ("Victim/Victim.toc", &toc_body("Victim", "1.0")),
        ]);
        let f = remote(&z);
        let err = install_from_bytes(&ctx, &addons, &meta(), &f, z, &owned, "curse:1", &mut |_| {})
            .await
            .expect_err("foreign folder must be refused");
        assert!(err.to_string().contains("owned by curse:777"), "{err}");
        assert_eq!(
            std::fs::read(addons.join("Victim").join("v.toc")).unwrap(),
            toc_body("Victim", "9.9")
        );
        assert!(!addons.join("Attacker").exists(), "nothing installs on refusal");
    }

    #[test]
    fn dropped_roots_cleanup_respects_ownership() {
        // AF-08: old-owned, new-dropped folders go away; foreign-owned stay.
        let (_tmp, addons) = fixture();
        for (dir, content) in [("Old_Mod", b"mine".as_slice()), ("Shared_Libs", b"theirs".as_slice())] {
            std::fs::create_dir_all(addons.join(dir)).unwrap();
            std::fs::write(addons.join(dir).join("f.lua"), content).unwrap();
        }
        let owned: std::collections::BTreeMap<String, String> = [
            ("old_mod".to_string(), "curse:1".to_string()),
            ("shared_libs".to_string(), "curse:2".to_string()),
        ]
        .into_iter()
        .collect();
        let old = vec!["Old_Mod".to_string(), "Shared_Libs".to_string(), "Gone".to_string()];
        let new = vec!["Main".to_string()];
        let removed = cleanup_dropped_roots(&addons, &old, &new, &owned, "curse:1");
        assert_eq!(removed, vec!["Old_Mod".to_string()]);
        assert!(!addons.join("Old_Mod").exists());
        assert!(addons.join("Shared_Libs").exists(), "foreign folder untouched");
    }

    #[test]
    fn backups_pruned_per_addon_key() {
        // AF-09: retention is per addon key; keep=0 drops all snapshots.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("backup");
        for (key, ts) in [("curse:1", 10), ("curse:1", 20), ("curse:1", 30), ("github:a_b", 15)] {
            std::fs::create_dir_all(root.join(snapshot_slug(key)).join(ts.to_string())).unwrap();
        }
        prune_backups(&root, "curse:1", 2).unwrap();
        let c1: Vec<String> = std::fs::read_dir(root.join("curse_1"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(c1.len(), 2);
        assert!(root.join("github_a_b").join("15").exists(), "other keys untouched");
        prune_backups(&root, "curse:1", 0).unwrap();
        assert_eq!(std::fs::read_dir(root.join("curse_1")).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn entry_count_cap_rejects() {
        // R-03: absurd entry counts are refused before extraction.
        let (_tmp, addons) = fixture();
        let ctx = ctx();
        let mut z = ZipWriter::new(Cursor::new(Vec::new()));
        let opts = SimpleFileOptions::default();
        for i in 0..(MAX_ENTRIES + 1) {
            z.start_file(format!("Big/Pad{i}.lua"), opts).unwrap();
            z.write_all(b"x").unwrap();
        }
        let bytes = z.finish().unwrap().into_inner();
        let f = remote(&bytes);
        let empty_owned = Default::default();
        let err = install_from_bytes(&ctx, &addons, &meta(), &f, bytes, &empty_owned, "curse:1", &mut |_| {})
            .await
            .expect_err("entry cap must reject");
        assert!(err.to_string().contains("too many entries"), "{err}");
    }
}
