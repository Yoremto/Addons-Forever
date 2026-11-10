//! Addon dependency support: parse `## Dependencies:` / `## OptionalDependencies:`
//! from installed TOC files, detect which are missing from the AddOns folder,
//! and resolve missing ones to an installable source by exact name search.

use std::path::Path;

use crate::providers::Registry;
use crate::types::{AppCtx, FileSelect, ProviderKind};

/// One dependency as declared by the addon.
#[derive(Clone, Debug)]
pub struct DeclaredDep {
    pub name: String,
    pub required: bool,
}

/// A declared dependency with its resolution state against the folder.
#[derive(Clone, Debug)]
pub struct DepStatus {
    pub name: String,
    pub required: bool,
    /// True when a folder with this name already exists in the AddOns dir.
    pub installed: bool,
    /// Provider source for installing it, when a provider has an exact-name match.
    pub source: Option<(ProviderKind, String)>,
}

/// Parse dependency headers from all `.toc` files directly in the given
/// addon folders (a bundle declares deps in its main TOC; parsing all root
/// TOCs is safe: duplicates are deduped, preserving order).
/// Handles the four TOC keys: `Dependencies`, `RequiredDeps` (required) and
/// `OptionalDependencies`, `OptionalDeps` (optional; the short alias is what
/// most addons ship, e.g. Questie).
pub fn declared_dependencies(addons_dir: &Path, folders: &[String]) -> Vec<DeclaredDep> {
    let mut out: Vec<DeclaredDep> = Vec::new();
    for folder in folders {
        let dir = addons_dir.join(folder);
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let mut tocs: Vec<_> = entries
            .flatten()
            .filter(|e| e.path().extension().map(|x| x.eq_ignore_ascii_case("toc")).unwrap_or(false))
            .map(|e| e.path())
            .collect();
        tocs.sort();
        for toc in tocs {
            let Ok(text) = std::fs::read_to_string(&toc) else { continue };
            for line in text.lines().take(80) {
                let Some(rest) = line.strip_prefix("## ") else { continue };
                let (header, required) =
                    if let Some(h) = strip_prefix_ci(rest, "Dependencies:").or_else(|| strip_prefix_ci(rest, "RequiredDeps:")) {
                        (h, true)
                    } else if let Some(h) = strip_prefix_ci(rest, "OptionalDependencies:")
                        .or_else(|| strip_prefix_ci(rest, "OptionalDeps:"))
                    {
                        (h, false)
                    } else {
                        continue;
                    };
                for dep in header.split(',') {
                    let dep = dep.trim();
                    if dep.is_empty() || dep.eq_ignore_ascii_case("Blizzard") {
                        continue;
                    }
                    // dedup case-insensitively; a later required declaration
                    // upgrades an earlier optional one
                    match out.iter_mut().find(|d| d.name.eq_ignore_ascii_case(dep)) {
                        Some(existing) => existing.required |= required,
                        None => out.push(DeclaredDep { name: dep.to_string(), required }),
                    }
                }
            }
        }
    }
    out
}

/// Names satisfied from inside the bundle itself: sub-folder names (embedded
/// libraries live in `Libs/<Name>/` and match by folder name at load time)
/// plus every `## Title:` found in TOCs within two directory levels
/// (color codes stripped). Used to keep embedded libraries from being
/// offered as separate installs.
pub fn embedded_names(addons_dir: &Path, folders: &[String]) -> Vec<String> {
    fn titles_in(dir: &Path, out: &mut Vec<String>) {
        titles_in_depth(dir, out, 0);
    }
    // bounded depth (the doc promises two levels) and never follows symlinks,
    // so a directory loop cannot blow the stack
    fn titles_in_depth(dir: &Path, out: &mut Vec<String>, depth: usize) {
        if depth > 2 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let path = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                out.push(name.clone());
                titles_in_depth(&path, out, depth + 1);
            } else if path.extension().map(|x| x.eq_ignore_ascii_case("toc")).unwrap_or(false) {
                let Ok(text) = std::fs::read_to_string(&path) else { continue };
                for line in text.lines().take(40) {
                    if let Some(t) = strip_prefix_ci(line.strip_prefix("## ").unwrap_or(line), "Title:") {
                        out.push(strip_color_codes(t));
                    }
                }
            }
        }
    }
    let mut out = Vec::new();
    for folder in folders {
        out.push(folder.clone());
        titles_in(&addons_dir.join(folder), &mut out);
    }
    out
}

/// Remove WoW rich-text color codes (`|cAARRGGBB`, `|r`) from TOC titles.
fn strip_color_codes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '|' {
            match chars.peek() {
                Some('c') => {
                    chars.next();
                    for _ in 0..8 {
                        if chars.peek().map(|x| x.is_ascii_alphanumeric()).unwrap_or(false) {
                            chars.next();
                        }
                    }
                }
                Some('r') => {
                    chars.next();
                }
                _ => out.push(c),
            }
        } else {
            out.push(c);
        }
    }
    out.trim().to_string()
}

/// Which declared deps have no folder in the AddOns dir (case-insensitive),
/// excluding names present in `satisfied_extra` (bundle-internal names).
pub fn unmet_dependencies(addons_dir: &Path, deps: &[DeclaredDep], satisfied_extra: &[String]) -> Vec<DepStatus> {
    let present = |name: &str| -> bool {
        satisfied_extra.iter().any(|s| s.eq_ignore_ascii_case(name))
            || {
                let Ok(entries) = std::fs::read_dir(addons_dir) else { return false };
                entries.flatten().any(|e| {
                    e.path().is_dir() && e.file_name().to_string_lossy().eq_ignore_ascii_case(name)
                })
            }
    };
    deps.iter()
        .map(|d| DepStatus {
            name: d.name.clone(),
            required: d.required,
            installed: present(&d.name),
            source: None,
        })
        .filter(|s| !s.installed)
        .collect()
}

/// Exact, case-insensitive name match against each searchable provider's
/// top results, in order: CurseForge, WowUp Hub, Tukui, WoWInterface.
pub async fn resolve_source(ctx: &AppCtx, reg: &Registry, sel: &FileSelect, name: &str) -> Option<(ProviderKind, String)> {
    for provider in [
        ProviderKind::Curse,
        ProviderKind::WowUpHub,
        ProviderKind::Tukui,
        ProviderKind::WowInterface,
    ] {
        // Collect all result pages (the popularity search is served ascending
        // and ignores `order`, so an exact-name hit can sit on any page).
        const PER: u32 = 50;
        const CAP: usize = 500;
        let mut items: Vec<crate::types::SearchItem> = Vec::new();
        let mut page_no = 0u32;
        loop {
            let Ok(page) = reg.get(provider).search(ctx, name, page_no, PER, sel).await else {
                break;
            };
            let total = page.total as usize;
            let n = page.items.len();
            items.extend(page.items);
            if n == 0 || items.len() >= total.min(CAP) || (items.len() < total && n < PER as usize) {
                break;
            }
            page_no += 1;
        }
        // Exact-name search hits are not ranked by the API (provider order can
        // put a renamed, low-download squatter of the same name first; live
        // case: "QuestieDB" returned a 54k-download rename ahead of the real
        // 6.4M-download project). Prefer the most-downloaded exact match.
        let mut hits: Vec<_> = items
            .into_iter()
            .filter(|i| clean_name(&i.name).eq_ignore_ascii_case(&clean_name(name)))
            .collect();
        hits.sort_by_key(|i| std::cmp::Reverse(i.downloads.unwrap_or(0)));
        if let Some(item) = hits.into_iter().next() {
            return Some((item.provider, item.external_id));
        }
    }
    None
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let bytes = s.as_bytes();
    // ASCII byte comparison: on a match the prefix occupies whole bytes,
    // so the slice boundary is a char boundary (TOC lines are UTF-8 and real
    // addons ship Cyrillic characters, e.g. Details!).
    if bytes.len() >= prefix.len() && bytes[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes()) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

/// Strip marketing suffixes from provider result names for matching
/// ("Details! [Ex]" -> "Details!").
fn clean_name(s: &str) -> String {
    let mut out = s;
    for c in ['[', '('] {
        out = out.split(c).next().unwrap_or(out);
    }
    out.trim().trim_end_matches(['!', '-', ':']).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toc(dir: &Path, content: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("X.toc"), content).unwrap();
    }

    #[test]
    fn parses_dependency_headers() {
        let tmp = tempfile::tempdir().unwrap();
        let addons = tmp.path();
        toc(
            &addons.join("CoolAddon"),
            "## Title: Cool\n## Interface: 16001\n## Dependencies: BigWigs, details, Blizzard\n## OptionalDependencies: LibStub\n",
        );
        let deps = declared_dependencies(addons, &["CoolAddon".into()]);
        assert_eq!(deps.len(), 3);
        assert_eq!(deps[0].name, "BigWigs");
        assert!(deps[0].required);
        assert_eq!(deps[1].name, "details");
        assert_eq!(deps[2].name, "LibStub");
        assert!(!deps[2].required);
    }

    #[test]
    fn unmet_checks_folders_case_insensitively() {
        let tmp = tempfile::tempdir().unwrap();
        let addons = tmp.path();
        std::fs::create_dir_all(addons.join("bigwigs_core")).unwrap();
        std::fs::create_dir_all(addons.join("Details")).unwrap();
        let deps = vec![
            DeclaredDep { name: "BigWigs_Core".into(), required: true },
            DeclaredDep { name: "Details".into(), required: true },
            DeclaredDep { name: "MissingLib".into(), required: false },
        ];
        let unmet = unmet_dependencies(addons, &deps, &[]);
        assert_eq!(unmet.len(), 1);
        assert_eq!(unmet[0].name, "MissingLib");
    }

    #[test]
    fn non_ascii_toc_lines_do_not_panic() {
        // The header probe slices every TOC line at a byte offset; a multi-byte
        // UTF-8 char (Cyrillic ships in real addons, e.g. Details!) straddling
        // that offset must not panic.
        let tmp = tempfile::tempdir().unwrap();
        let addons = tmp.path();
        std::fs::create_dir_all(addons.join("Details")).unwrap();
        std::fs::write(
            addons.join("Details/Details.toc"),
            "## Title: Details!\n## Author: Arak\u{410}, Team\u{410}rk\n## Dependencies: Details_Lib, CoolAddon\n",
        )
        .unwrap();
        let deps = declared_dependencies(addons, &["Details".into()]);
        assert_eq!(deps.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["Details_Lib", "CoolAddon"]);
    }

    #[test]
    fn questie_shape_optionaldeps_and_embedded_libs() {
        // Questie declares `## OptionalDeps: QuestieDB, LibStub, Ace3` and
        // embeds LibStub as `Libs/LibStub/`: embedded names must not be
        // offered for install; real missing addons must.
        let tmp = tempfile::tempdir().unwrap();
        let addons = tmp.path();
        toc(&addons.join("Questie"), "## Title: Questie\n## OptionalDeps: QuestieDB, LibStub, Ace3\n");
        toc(&addons.join("Questie/Libs/LibStub"), "## Title: Lib: |cFF00FF00LibStub|r\n");
        std::fs::create_dir_all(addons.join("Questie/Libs/Ace3")).unwrap();
        let declared = declared_dependencies(addons, &["Questie".into()]);
        assert_eq!(declared.len(), 3);
        assert!(declared.iter().all(|d| !d.required));
        let embedded = embedded_names(addons, &["Questie".into()]);
        assert!(embedded.iter().any(|n| n == "LibStub"));
        assert!(embedded.iter().any(|n| n == "Ace3"));
        let unmet = unmet_dependencies(addons, &declared, &embedded);
        assert_eq!(unmet.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["QuestieDB"]);
    }

    #[test]
    fn color_codes_stripped_from_titles() {
        assert_eq!(strip_color_codes("Questie|cFF00FF00 v12|r"), "Questie v12");
        assert_eq!(strip_color_codes("Lib: |cFF00FF00LibStub|r"), "Lib: LibStub");
        assert_eq!(strip_color_codes("Plain"), "Plain");
    }

    #[test]
    fn clean_name_strips_bracket_suffixes() {
        assert_eq!(clean_name("Details! [Ex]"), "Details");
        assert_eq!(clean_name("BigWigs (Classic)"), "BigWigs");
        assert_eq!(clean_name("Cool addon"), "Cool addon");
    }

    #[test]
    fn bundle_deps_satisfied_absent_deps_prompt() {
        // A BigWigs module declares `## Dependencies: BigWigs`.
        let tmp = tempfile::tempdir().unwrap();
        let addons = tmp.path();
        toc(&addons.join("BigWigs_Core"), "## Title: BigWigs Core\n## Dependencies: BigWigs\n");
        toc(&addons.join("BigWigs"), "## Title: BigWigs\n");

        // Core present: dependency is satisfied, nothing to prompt.
        let declared = declared_dependencies(addons, &["BigWigs_Core".into()]);
        assert_eq!(declared.len(), 1);
        assert!(unmet_dependencies(addons, &declared, &[]).is_empty());

        // Core missing (fresh module install): it surfaces as unmet.
        std::fs::remove_dir_all(addons.join("BigWigs")).unwrap();
        let unmet = unmet_dependencies(addons, &declared, &[]);
        assert_eq!(unmet.len(), 1);
        assert_eq!(unmet[0].name, "BigWigs");
        assert!(unmet[0].required);
    }

    /// Live provider resolution (FA_LIVE=1 cargo test -- --include-ignored).
    #[test]
    #[ignore]
    fn resolve_source_finds_real_addons() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let cfg = std::sync::Arc::new(crate::config::AppConfig::default());
            let ctx = crate::types::AppCtx::new(cfg.clone(), "test");
            let reg = crate::providers::Registry::new(&ctx);
            let sel = crate::types::FileSelect::from_cfg(&cfg);
            // AishCore is an established Curse addon published for the Forever
            // flavor (gameVersionTypeId 88568), so the exact-name search hits.
            let src = resolve_source(&ctx, &reg, &sel, "AishCore").await;
            assert!(src.is_some(), "AishCore must resolve on the forever flavor");
            // Live incident (Nov 2026): the Forever search index lists a
            // 54k-download renamed squatter named "QuestieDB" (1774863, a
            // supplement file tagged only for unmapped game versions) ahead
            // of the real 6.4M-download QuestieDB (1696795). Exact-name
            // resolution must pick the real project, never the squatter.
            let src = resolve_source(&ctx, &reg, &sel, "QuestieDB").await;
            assert_eq!(
                src,
                Some((crate::types::ProviderKind::Curse, "1696795".to_string())),
                "QuestieDB must resolve to the real project, not the index squatter"
            );
            // a made-up addon resolves to nothing
            let src = resolve_source(&ctx, &reg, &sel, "Zzznotarealaddon918273645").await;
            assert!(src.is_none(), "bogus name must not resolve");
        });
    }
}
