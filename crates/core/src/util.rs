//! Small shared helpers: paths, version comparison, atomic file writes.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Directory the app keeps its data in: next to the executable (portable app).
/// Overridable with `ADDONS_FOREVER_DATA_DIR` (used by tests and CLI `--data-dir`).
pub fn data_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("ADDONS_FOREVER_DATA_DIR") {
        let p = PathBuf::from(dir);
        std::fs::create_dir_all(&p)?;
        return Ok(p);
    }
    let exe = std::env::current_exe().map_err(|e| Error::Config(format!("current_exe: {e}")))?;
    let dir = exe
        .parent()
        .ok_or_else(|| Error::Config("exe has no parent dir".into()))?
        .to_path_buf();
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Write `bytes` to `path` atomically (tmp file + rename).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Loose, provider-agnostic version comparison key: leading numbers.
/// `"1.60.1-beta.2" -> [1, 60, 1, 2]` (non-numeric segments dropped).
pub fn version_key(v: &str) -> Vec<u64> {
    let mut out = Vec::new();
    for token in v.split(|c: char| !c.is_ascii_digit()) {
        if let Ok(n) = token.parse::<u64>() {
            out.push(n);
        }
    }
    out
}

/// Trailing client tokens on published versions (`1.60.28-forever`,
/// `1.2.3_wotlk`) carry no ordering information; strip them before comparing.
pub fn strip_flavor_suffix(v: &str) -> String {
    const TOKENS: [&str; 9] = [
        "forever", "camelot", "mainline", "retail", "classic", "classic_beta", "tbc", "wotlk", "vanilla",
    ];
    let mut out = v.trim().to_lowercase();
    for tok in TOKENS {
        for sep in ["-", "_", " "] {
            if let Some(stripped) = out.strip_suffix(&format!("{sep}{tok}")) {
                out = stripped.to_string();
            }
        }
    }
    // parenthesized client labels: "OpenMap v0.8.13 (WoW Forever)"
    if let Some((head, tail)) = out.rsplit_once('(') {
        let inner = tail.strip_suffix(')');
        if let Some(inner) = inner {
            let words: Vec<&str> =
                inner.split(|c: char| !c.is_ascii_alphanumeric() && c != '_').filter(|w| !w.is_empty()).collect();
            let is_flavor = |w: &str| w == "wow" || TOKENS.contains(&w);
            if !words.is_empty() && words.iter().all(|w| is_flavor(w)) {
                out = head.trim_end().to_string();
            }
        }
    }
    out
}

/// Split into `(number, letter-suffix)` segments: `"1.0.0d"` ->
/// `[(1,""),(0,""),(0,"d")]`, so letter suffixes order (`d` > `c`).
pub fn version_parts(v: &str) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    let mut seen_number = false;
    for seg in strip_flavor_suffix(v).split(|c: char| !c.is_ascii_alphanumeric()) {
        if seg.is_empty() {
            continue;
        }
        // a leading letter prefix ("v1", "ver2") is a version tag, not
        // ordering data
        let num = seg.trim_start_matches(|c: char| c.is_ascii_alphabetic());
        let digits: String = num.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            // text-only segment: a product-name prefix ("OpenMap v0.8.13")
            // carries no ordering information and must not shift the
            // positional comparison; after the first number it is a real
            // qualifier ("1.0-beta") and orders by name
            if !seen_number {
                continue;
            }
            out.push((0, seg.to_lowercase()));
            continue;
        }
        seen_number = true;
        let letters: String = num[digits.len()..].to_lowercase();
        out.push((digits.parse::<u64>().unwrap_or(0), letters));
    }
    out
}

/// Compare two version strings with letter suffixes honored.
pub fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let (mut pa, mut pb) = (version_parts(a), version_parts(b));
    let n = pa.len().max(pb.len());
    while pa.len() < n {
        pa.push((0, String::new()));
    }
    while pb.len() < n {
        pb.push((0, String::new()));
    }
    pa.cmp(&pb)
}

/// `Some(Ordering)`-style compare: returns true if `a` is newer than `b`.
pub fn version_is_newer(a: &str, b: &str) -> bool {
    version_cmp(a, b) == std::cmp::Ordering::Greater
}

/// Case-insensitive path lookup: resolve `rel` (with `/` separators) under `base`
/// matching file/dir names ignoring case (TOC include paths are case-insensitive on Windows).
pub fn lookup_ci(base: &Path, rel: &str) -> Option<PathBuf> {
    let mut cur = base.to_path_buf();
    for seg in rel.split(['/', '\\']) {
        match seg {
            "" | "." => continue,
            _ => {}
        }
        let mut found = None;
        if cur.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&cur) {
                for e in entries.flatten() {
                    if e.file_name().to_string_lossy().eq_ignore_ascii_case(seg) {
                        found = Some(e.path());
                        break;
                    }
                }
            }
        } else {
            // single file?
            if cur.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.eq_ignore_ascii_case(seg)) {
                found = Some(cur.clone());
            }
        }
        cur = found?;
    }
    Some(cur)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_keys() {
        assert!(version_is_newer("1.60.2", "1.60.1"));
        assert!(version_is_newer("1.61", "1.60.9"));
        assert!(!version_is_newer("1.6.0", "1.60.1"));
        assert!(version_is_newer("v1.5.1-forever", "1.5.0"));
        assert_eq!(version_key("v427.8"), vec![427, 8]);
        // letter suffixes order
        assert!(version_is_newer("1.0.0d", "1.0.0c"));
        assert!(version_is_newer("1.0.1", "1.0.0d"));
        assert!(version_is_newer("1.0.0c", "1.0.0"));
        assert!(!version_is_newer("1.0.0c", "1.0.0d"));
        assert_eq!(version_cmp("1.60.28-forever", "1.60.28"), std::cmp::Ordering::Equal);
        assert_eq!(version_cmp("2.0.0b-WOTLK", "2.0.0b"), std::cmp::Ordering::Equal);
        // product-name prefixes and parenthesized client labels are the real
        // published version strings on GitHub/CurseForge (OpenMap report:
        // installed "v0.8.10" vs available "OpenMap v0.8.13 (WoW Forever)"
        // was judged OLDER by positional segment shift)
        assert!(version_is_newer("OpenMap v0.8.13 (WoW Forever)", "v0.8.10"));
        assert!(version_is_newer("OpenMap v0.8.13 (WoW Forever)", "OpenMap v0.8.10 (WoW Forever)"));
        assert_eq!(
            version_cmp("OpenMap v0.8.13 (WoW Forever)", "0.8.13"),
            std::cmp::Ordering::Equal
        );
        assert_eq!(
            version_cmp("OpenMap v0.8.10 (WOTLK)", "OpenMap v0.8.10 (WoW Forever)"),
            std::cmp::Ordering::Equal,
            "client labels are identity"
        );
        // a qualifier after the numbers still orders
        assert!(version_is_newer("1.0.0 stable2", "1.0.0 stable1"));
        assert!(version_is_newer("1.0.1", "1.0.0 beta"));
    }

    #[test]
    fn lookup_case_insensitive() {
        let dir = std::env::temp_dir().join("fa-lookup-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Locales")).unwrap();
        std::fs::write(dir.join("Locales").join("enUS.lua"), b"x").unwrap();
        std::fs::write(dir.join("Core.lua"), b"x").unwrap();
        assert!(lookup_ci(&dir, "core.lua").is_some());
        assert!(lookup_ci(&dir, "Locales/enus.lua").is_some());
        assert!(lookup_ci(&dir, "Locales\\enUS.lua").is_some());
        assert!(lookup_ci(&dir, "missing.lua").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
