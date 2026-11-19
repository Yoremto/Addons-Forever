//! Configuration: a plain `.ini` file living next to the executable.
//! Everything the app needs to run is here; no registry, no app-data dirs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ini::Ini;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::flavor::Flavor;
use crate::types::Channel;
use crate::types::ProviderKind;

/// Public, community-shared CurseForge API key (the one shipped in open-source
/// managers). Fully overridable in `[network] curse_api_key` — the app never
/// requires any key to run.
pub const DEFAULT_CURSE_API_KEY: &str = "$2a$10$bL4bIL5pUWqfcO7KQtnMReakwtfHbNKh6v1uTpKlzhwoueEJQnPnm";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct AppConfig {
    pub app: AppSection,
    pub updates: UpdatesSection,
    pub network: NetworkSection,
    pub ui: UiSection,
    /// Per-addon overrides, keyed `provider:external_id`.
    pub addons: BTreeMap<String, AddonOverride>,
}


#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSection {
    /// Explicit AddOns directory (wins over auto-detection).
    pub addons_dir: Option<String>,
    /// Auto-detect the WoW install (Windows: registry-less well-known paths).
    pub auto_detect: bool,
    pub flavor: Flavor,
    /// Client version, e.g. "1.60.1". None = derive from the game's .toc data / default.
    pub game_version: Option<String>,
    /// GitHub `owner/repo` that hosts app releases (self-update), None = default.
    pub update_repo: Option<String>,
    /// Install app updates automatically when a check finds one.
    /// Off by default: updates are announced, and installed on user action.
    pub self_update: bool,
}

/// Default app-release repository (where the GitHub releases live).
pub const DEFAULT_UPDATE_REPO: &str = "Yoremto/Addons-Forever";

impl Default for AppSection {
    fn default() -> Self {
        AppSection {
            addons_dir: None,
            auto_detect: true,
            flavor: Flavor::default(),
            game_version: None,
            update_repo: Some(DEFAULT_UPDATE_REPO.to_string()),
            self_update: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdatesSection {
    /// Automatic updates enabled globally. Off by default — updates are checked
    /// and *announced* on a timer; installing them is opt-in per addon (`Auto`).
    pub auto_update: bool,
    /// Check for updates on app start.
    pub check_on_startup: bool,
    /// Recheck interval for the auto-update loop, minutes.
    pub interval_minutes: u32,
    /// Global default channel.
    pub channel: Channel,
    /// Keep one `.bak` copy of replaced addon folders.
    pub keep_backups: bool,
}

impl Default for UpdatesSection {
    fn default() -> Self {
        UpdatesSection {
            auto_update: false,
            check_on_startup: true,
            interval_minutes: 360,
            channel: Channel::Stable,
            keep_backups: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkSection {
    pub curse_api_key: String,
    /// Optional GitHub token: raises API limits, sent only to api.github.com.
    pub github_token: String,
    /// Optional Wago key, sent only to addons.wago.io.
    pub wago_api_key: String,
    /// Never touch the network (browse cached state only).
    pub offline: bool,
}

impl Default for NetworkSection {
    fn default() -> Self {
        NetworkSection {
            curse_api_key: DEFAULT_CURSE_API_KEY.to_string(),
            github_token: String::new(),
            wago_api_key: String::new(),
            offline: false,
        }
    }
}

/// Color theme: follow the OS appearance, or force one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    System,
    Dark,
    Light,
}

impl Theme {
    pub const ALL: [Theme; 3] = [Theme::System, Theme::Dark, Theme::Light];

    pub fn as_str(self) -> &'static str {
        match self {
            Theme::System => "system",
            Theme::Dark => "dark",
            Theme::Light => "light",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Theme::System => "Follow system",
            Theme::Dark => "Dark",
            Theme::Light => "Light",
        }
    }

    pub fn parse(s: &str) -> Option<Theme> {
        match s.trim().to_ascii_lowercase().as_str() {
            "system" | "auto" => Some(Theme::System),
            "dark" => Some(Theme::Dark),
            "light" => Some(Theme::Light),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSection {
    pub theme: Theme,
    /// UI scale multiplier applied on top of the OS DPI scaling (0.5 .. 2.5).
    pub scale: f32,
    pub minimize_to_tray: bool,
    pub scan_on_start: bool,
    pub window_w: f32,
    pub window_h: f32,
}

impl Default for UiSection {
    fn default() -> Self {
        UiSection {
            theme: Theme::System,
            scale: 1.0,
            minimize_to_tray: false,
            scan_on_start: true,
            window_w: 1080.0,
            window_h: 680.0,
        }
    }
}

/// `[addon <provider>:<external_id>]` sections — user-defined source pinning.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AddonOverride {
    pub provider: Option<ProviderKind>,
    pub external_id: Option<String>,
    pub name: Option<String>,
    pub folders: Vec<String>,
    pub channel: Option<Channel>,
    pub auto_update: Option<bool>,
    pub ignored: Option<bool>,
    /// AF-12: the user explicitly chose this source (source override, zip
    /// URL). Pins re-attach by folder on every scan, survive restarts, and
    /// outrank automatic provider detection.
    pub pinned: bool,
    /// AF-12: identity (file id) of what is installed, so "same file" is
    /// known across restarts.
    pub installed_file_id: Option<String>,
    /// Version string of the last installed file. Persisted so the app shows
    /// its installed addons (with versions) before any scan runs.
    #[serde(default)]
    pub installed_version: Option<String>,
}

impl AppConfig {
    pub fn config_path(dir: &Path) -> PathBuf {
        dir.join("addons-forever.ini")
    }

    /// Load, creating a default file on first run. Missing sections fall back to
    /// defaults; a corrupt file is an error (so the user notices).
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            let cfg = AppConfig::default();
            cfg.save(path)?;
            return Ok(cfg);
        }
        // Parse with `\` kept raw. With escape processing (the rust-ini
        // default) a Windows value like `C:\World of Warcraft\Interface\AddOns`
        // collapses to `C:World of WarcraftInterfaceAddOns`, silently losing
        // the configured path. load_from_file_* also strips a UTF-8 BOM.
        let ini = Ini::load_from_file_noescape(path)
            .map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
        cfg_from_ini(&ini)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = self.to_ini_text();
        crate::util::write_atomic(path, text.as_bytes())
    }

    pub fn to_ini_text(&self) -> String {
        let mut s = String::new();
        s.push_str("# addons-forever settings — portable, plain text. Safe to edit.\n\n");
        s.push_str("[app]\n");
        kv(&mut s, "flavor", self.app.flavor.as_str());
        kv_opt(&mut s, "addons_dir", self.app.addons_dir.as_deref());
        kv_bool(&mut s, "auto_detect", self.app.auto_detect);
        kv_opt(&mut s, "game_version", self.app.game_version.as_deref());
        kv_opt(&mut s, "update_repo", self.app.update_repo.as_deref());
        kv_bool(&mut s, "self_update", self.app.self_update);
        s.push('\n');
        s.push_str("[updates]\n");
        kv_bool(&mut s, "auto_update", self.updates.auto_update);
        kv_bool(&mut s, "check_on_startup", self.updates.check_on_startup);
        kv(&mut s, "interval_minutes", &self.updates.interval_minutes.to_string());
        kv(&mut s, "channel", self.updates.channel.as_str());
        kv_bool(&mut s, "keep_backups", self.updates.keep_backups);
        s.push('\n');
        s.push_str("[network]\n");
        kv(&mut s, "curse_api_key", &self.network.curse_api_key);
        kv(&mut s, "github_token", &self.network.github_token);
        kv(&mut s, "wago_api_key", &self.network.wago_api_key);
        kv_bool(&mut s, "offline", self.network.offline);
        s.push('\n');
        s.push_str("[ui]\n");
        kv(&mut s, "theme", self.ui.theme.as_str());
        kv(&mut s, "scale", &self.ui.scale.to_string());
        kv_bool(&mut s, "minimize_to_tray", self.ui.minimize_to_tray);
        kv_bool(&mut s, "scan_on_start", self.ui.scan_on_start);
        kv(&mut s, "window_w", &self.ui.window_w.to_string());
        kv(&mut s, "window_h", &self.ui.window_h.to_string());
        s.push('\n');
        for (key, o) in &self.addons {
            s.push_str(&format!("[addon {key}]\n"));
            if let Some(p) = o.provider {
                kv(&mut s, "provider", p.as_str());
            }
            kv_opt(&mut s, "external_id", o.external_id.as_deref());
            kv_opt(&mut s, "name", o.name.as_deref());
            if !o.folders.is_empty() {
                kv(&mut s, "folders", &o.folders.join(";"));
            }
            kv_opt(&mut s, "channel", o.channel.map(|c| c.as_str()));
            kv_bool_opt(&mut s, "auto_update", o.auto_update);
            kv_bool_opt(&mut s, "ignored", o.ignored);
            if o.pinned {
                kv(&mut s, "pinned", "true");
            }
            kv_opt(&mut s, "installed_file_id", o.installed_file_id.as_deref());
            kv_opt(&mut s, "installed_version", o.installed_version.as_deref());
            s.push('\n');
        }
        s
    }

    /// Effective AddOns directory. An explicit path wins outright: the app
    /// never silently substitutes an auto-detected installation for a
    /// configured one (a stale row must not end up mutating a different
    /// AddOns folder). Auto-detection runs only when no path is set.
    pub fn addons_dir(&self) -> Option<PathBuf> {
        if let Some(p) = self.app.addons_dir.as_deref().filter(|p| !p.trim().is_empty()) {
            return Some(PathBuf::from(p));
        }
        if self.app.auto_detect {
            return autodetect_addons_dir(self.app.flavor);
        }
        None
    }

    pub fn curse_key_header(&self) -> (String, String) {
        ("x-api-key".into(), self.network.curse_api_key.clone())
    }

    pub fn user_agent(&self, app_version: &str) -> String {
        format!("addons-forever/{app_version}")
    }
}

fn kv(s: &mut String, k: &str, v: &str) {
    s.push_str(&format!("{k} = {v}\n"));
}
fn kv_opt(s: &mut String, k: &str, v: Option<&str>) {
    if let Some(v) = v {
        kv(s, k, v);
    }
}
fn kv_bool(s: &mut String, k: &str, v: bool) {
    kv(s, k, if v { "true" } else { "false" });
}
fn kv_bool_opt(s: &mut String, k: &str, v: Option<bool>) {
    if let Some(v) = v {
        kv_bool(s, k, v);
    }
}

fn cfg_from_ini(ini: &Ini) -> Result<AppConfig> {
    let mut cfg = AppConfig::default();
    if let Some(s) = ini.section(Some("app")) {
        if let Some(v) = s.get("flavor").and_then(Flavor::parse) {
            cfg.app.flavor = v;
        }
        cfg.app.addons_dir = s.get("addons_dir").map(str::to_string);
        if let Some(v) = s.get("auto_detect") {
            cfg.app.auto_detect = parse_bool(v, cfg.app.auto_detect);
        }
        cfg.app.game_version = s.get("game_version").map(str::to_string);
        cfg.app.update_repo = s.get("update_repo").map(str::to_string);
        cfg.app.self_update = s.get("self_update").map(|v| parse_bool(v, false)).unwrap_or(cfg.app.self_update);
    }
    if let Some(s) = ini.section(Some("updates")) {
        cfg.updates.auto_update = s.get("auto_update").map(|v| parse_bool(v, true)).unwrap_or(cfg.updates.auto_update);
        cfg.updates.check_on_startup = s.get("check_on_startup").map(|v| parse_bool(v, true)).unwrap_or(cfg.updates.check_on_startup);
        if let Some(v) = s.get("interval_minutes").and_then(|v| v.trim().parse::<u32>().ok()) {
            cfg.updates.interval_minutes = v.clamp(5, 7 * 24 * 60);
        }
        if let Some(v) = s.get("channel").and_then(Channel::parse) {
            cfg.updates.channel = v;
        }
        cfg.updates.keep_backups = s.get("keep_backups").map(|v| parse_bool(v, true)).unwrap_or(cfg.updates.keep_backups);
    }
    if let Some(s) = ini.section(Some("network")) {
        if let Some(v) = s.get("curse_api_key").filter(|v| !v.trim().is_empty()) {
            cfg.network.curse_api_key = v.to_string();
        }
        cfg.network.github_token = s.get("github_token").unwrap_or("").to_string();
        cfg.network.wago_api_key = s.get("wago_api_key").unwrap_or("").to_string();
        cfg.network.offline = s.get("offline").map(|v| parse_bool(v, false)).unwrap_or(false);
    }
    if let Some(s) = ini.section(Some("ui")) {
        if let Some(v) = s.get("theme").and_then(Theme::parse) {
            cfg.ui.theme = v;
        }
        if let Some(v) = s.get("scale").and_then(|v| v.trim().parse::<f32>().ok()) {
            cfg.ui.scale = v.clamp(0.5, 2.5);
        }
        cfg.ui.minimize_to_tray = s.get("minimize_to_tray").map(|v| parse_bool(v, false)).unwrap_or(false);
        cfg.ui.scan_on_start = s.get("scan_on_start").map(|v| parse_bool(v, true)).unwrap_or(true);
        if let Some(v) = s.get("window_w").and_then(|v| v.trim().parse::<f32>().ok()) {
            cfg.ui.window_w = v.clamp(600.0, 8000.0);
        }
        if let Some(v) = s.get("window_h").and_then(|v| v.trim().parse::<f32>().ok()) {
            cfg.ui.window_h = v.clamp(400.0, 8000.0);
        }
    }
    for (name, s) in ini.iter() {
        let Some(rest) = name.filter(|n| n.starts_with("addon ")) else { continue };
        let key = rest["addon ".len()..].to_string();
        let (provider, external_id) = match key.split_once(':') {
            Some((p, id)) => (ProviderKind::parse(p), id.to_string()),
            None => (None, key.clone()),
        };
        let mut o = AddonOverride { provider, external_id: Some(external_id), ..Default::default() };
        if let Some(v) = s.get("provider").and_then(ProviderKind::parse) {
            o.provider = Some(v);
        }
        if let Some(v) = s.get("external_id") {
            o.external_id = Some(v.to_string());
        }
        o.name = s.get("name").map(str::to_string);
        if let Some(v) = s.get("folders") {
            o.folders = v.split(';').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        }
        o.channel = s.get("channel").and_then(Channel::parse);
        o.auto_update = s.get("auto_update").map(|v| parse_bool(v, true));
        o.ignored = s.get("ignored").map(|v| parse_bool(v, false));
        o.pinned = s.get("pinned").map(|v| parse_bool(v, false)).unwrap_or(false);
        o.installed_file_id = s
            .get("installed_file_id")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        o.installed_version = s
            .get("installed_version")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        cfg.addons.insert(key, o);
    }
    Ok(cfg)
}

fn parse_bool(v: &str, default: bool) -> bool {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => true,
        "0" | "false" | "no" | "off" => false,
        _ => default,
    }
}

/// Example AddOns path shown as placeholder text, per OS.
pub fn addons_dir_example() -> &'static str {
    if cfg!(windows) {
        r"C:\Program Files (x86)\World of Warcraft\_forever_\Interface\AddOns"
    } else if cfg!(target_os = "macos") {
        "~/Library/Application Support/World of Warcraft/_forever_/Interface/AddOns"
    } else {
        "~/.wine/drive_c/Program Files (x86)/World of Warcraft/_forever_/Interface/AddOns"
    }
}

/// Scan the standard installation locations for the flavor's AddOns dir.
pub fn autodetect_addons_dir(flavor: Flavor) -> Option<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    #[cfg(windows)]
    {
        let program_files = std::env::var("ProgramFiles(x86)")
            .or_else(|_| std::env::var("ProgramFiles"))
            .map(PathBuf::from)
            .ok();
        if let Some(pf) = program_files {
            roots.push(pf.join("World of Warcraft"));
        }
        if let Some(drive) = std::env::var("SystemDrive").ok() {
            roots.push(PathBuf::from(format!("{drive}\\Program Files (x86)\\World of Warcraft")));
            roots.push(PathBuf::from(format!("{drive}\\Games\\World of Warcraft")));
        }
    }
    #[cfg(not(windows))]
    {
        let home = dirs::home_dir();
        let mut bases: Vec<PathBuf> = Vec::new();
        // macOS: the Battle.net-managed client keeps its game data under
        // Application Support; classic installs sit in /Applications.
        if let Some(h) = &home {
            bases.push(h.join("Library/Application Support/World of Warcraft"));
        }
        bases.push(PathBuf::from("/Library/Application Support/World of Warcraft"));
        bases.push(PathBuf::from("/Applications/World of Warcraft"));
        bases.push(PathBuf::from("/Applications/WoW"));
        if let Some(h) = &home {
            bases.push(h.join("World of Warcraft"));
            bases.push(h.join("Applications/World of Warcraft"));
            bases.push(h.join("games"));
            bases.push(h.join("Games"));
            bases.push(h.join(".wine/drive_c/Program Files (x86)"));
            bases.push(h.join(".wine/drive_c/Program Files"));
            // CrossOver / Code Weavers Cask containers (common on macOS).
            bases.push(
                h.join("Library/Application Support/com.codeweavers.CrossOver/containers")
                    .join("cx_root/drive_c/Program Files (x86)"),
            );
            bases.push(
                h.join("Library/Application Support/com.codeweavers.CrossOver/containers")
                    .join("WoW/drive_c/Program Files (x86)"),
            );
            bases.push(h.join("Applications/CrossOver/World of Warcraft"));
        }
        for base in bases {
            let joined = base.join("World of Warcraft");
            // entries ending in "World of Warcraft" are already the root
            if base.file_name().map(|n| n == "World of Warcraft").unwrap_or(false) {
                roots.push(base);
            } else {
                roots.push(joined);
            }
        }
    }
    for root in roots {
        let addons = match flavor.edition_dir() {
            Some(ed) => root.join(ed).join("Interface").join("AddOns"),
            None => root.join("_retail_").join("Interface").join("AddOns"),
        };
        if addons.is_dir() {
            return Some(addons);
        }
        // flat layout (some installs): root/Interface/AddOns
        let flat = root.join("Interface").join("AddOns");
        if flat.is_dir() && flavor.edition_dir().is_none() {
            return Some(flat);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_roundtrip_and_fallback() {
        for t in Theme::ALL {
            let mut cfg = AppConfig::default();
            cfg.ui.theme = t;
            let ini = Ini::load_from_str(&cfg.to_ini_text()).unwrap();
            assert_eq!(cfg_from_ini(&ini).unwrap().ui.theme, t);
        }
        let ini = Ini::load_from_str("[ui]\ntheme = purple\n").unwrap();
        assert_eq!(cfg_from_ini(&ini).unwrap().ui.theme, Theme::System);
        let ini = Ini::load_from_str("[ui]\ntheme = Light\n").unwrap();
        assert_eq!(cfg_from_ini(&ini).unwrap().ui.theme, Theme::Light);
    }

    #[test]
    fn ini_roundtrip() {
        let mut cfg = AppConfig::default();
        cfg.app.addons_dir = Some("C:/Games/WoW/_forever_/Interface/AddOns".into());
        cfg.updates.channel = Channel::Beta;
        cfg.network.github_token = "ghp_x".into();
        cfg.addons.insert(
            "github:BigWigsMods/BigWigs".into(),
            AddonOverride {
                provider: Some(ProviderKind::GitHub),
                external_id: Some("BigWigsMods/BigWigs".into()),
                folders: vec!["BigWigs".into(), "BigWigs_Core".into()],
                auto_update: Some(false),
                pinned: true,
                installed_file_id: Some("release-42".into()),
                ..Default::default()
            },
        );
        let text = cfg.to_ini_text();
        let ini = Ini::load_from_str_noescape(&text).unwrap();
        let back = cfg_from_ini(&ini).unwrap();
        assert_eq!(cfg, back);

        // update repo ships a default; auto-install is off by default
        assert_eq!(AppConfig::default().app.update_repo.as_deref(), Some(DEFAULT_UPDATE_REPO));
        assert!(!AppConfig::default().app.self_update);
        let mut cfg2 = AppConfig::default();
        cfg2.app.self_update = true;
        let ini2 = Ini::load_from_str_noescape(&cfg2.to_ini_text()).unwrap();
        assert!(cfg_from_ini(&ini2).unwrap().app.self_update);
    }

    #[test]
    fn load_creates_default_file() {
        let dir = std::env::temp_dir().join(format!("fa-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = AppConfig::config_path(&dir);
        let _ = std::fs::remove_file(&p);
        let cfg = AppConfig::load(&p).unwrap();
        assert!(p.exists());
        assert_eq!(cfg.app.flavor, Flavor::Forever);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Regression: a Windows `addons_dir` must survive the ini roundtrip.
    /// rust-ini's default escape processing collapsed
    /// `C:\World of Warcraft\Interface\AddOns` to
    /// `C:World of WarcraftInterfaceAddOns`, so scans after a restart reported
    /// "no AddOns directory configured" (0.3.4 Windows bug report).
    #[test]
    fn load_windows_addons_dir_backslashes_and_bom() {
        let dir = std::env::temp_dir().join(format!("fa-cfg-win-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = AppConfig::config_path(&dir);
        let win_path = r"C:\World of Warcraft\Interface\AddOns";
        // UTF-8 BOM first: Notepad's "UTF-8" files carry one; load must cope.
        std::fs::write(
            &p,
            format!("\u{feff}[app]\nflavor = forever\naddons_dir = {win_path}\nauto_detect = false\n"),
        )
        .unwrap();
        let cfg = AppConfig::load(&p).unwrap();
        assert_eq!(cfg.app.addons_dir.as_deref(), Some(win_path));
        // fail-closed: the configured path is the effective dir even where
        // that path is not a directory (no silent auto-detect fallback)
        assert_eq!(
            cfg.addons_dir(),
            Some(std::path::PathBuf::from(win_path))
        );

        // full production roundtrip: save writes the path raw, load reads it back
        let mut cfg2 = AppConfig::default();
        cfg2.app.addons_dir = Some(win_path.into());
        cfg2.save(&p).unwrap();
        let cfg3 = AppConfig::load(&p).unwrap();
        assert_eq!(cfg3.app.addons_dir.as_deref(), Some(win_path));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A quoted path (common Windows ini habit) parses to the bare path when
    /// quote handling stays enabled and escapes stay off.
    #[test]
    fn load_quoted_addons_dir() {
        let dir = std::env::temp_dir().join(format!("fa-cfg-q-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = AppConfig::config_path(&dir);
        std::fs::write(
            &p,
            "[app]\naddons_dir = \"C:\\World of Warcraft\\Interface\\AddOns\"\nauto_detect = false\n",
        )
        .unwrap();
        let cfg = AppConfig::load(&p).unwrap();
        assert_eq!(
            cfg.app.addons_dir.as_deref(),
            Some(r"C:\World of Warcraft\Interface\AddOns")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
