//! Game flavors (client editions), interface-number math.

use serde::{Deserialize, Serialize};

/// Which World of Warcraft client the manager (and its file selection) targets.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum Flavor {
    /// World of Warcraft: Forever (`_forever_`, interface 16001, Curse typeId 88568)
    #[default]
    Forever,
    /// Retail (Midnight, `_classic_`-style `Interface` root, typeId 517)
    Retail,
    ClassicEra,
    Tbc,
    Wotlk,
    Cata,
    Mists,
    Titan,
}


impl Flavor {
    pub const ALL: &'static [Flavor] = &[
        Flavor::Forever,
        Flavor::Retail,
        Flavor::ClassicEra,
        Flavor::Tbc,
        Flavor::Wotlk,
        Flavor::Cata,
        Flavor::Mists,
        Flavor::Titan,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Flavor::Forever => "forever",
            Flavor::Retail => "retail",
            Flavor::ClassicEra => "classic",
            Flavor::Tbc => "bcc",
            Flavor::Wotlk => "wotlk",
            Flavor::Cata => "cata",
            Flavor::Mists => "mists",
            Flavor::Titan => "titan",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase();
        Some(match s.as_str() {
            "forever" | "camelot" => Flavor::Forever,
            "retail" | "standard" | "midnight" => Flavor::Retail,
            "classic" | "era" | "classicera" | "vanilla" => Flavor::ClassicEra,
            "bcc" | "tbc" => Flavor::Tbc,
            "wotlk" | "wotlkc" => Flavor::Wotlk,
            "cata" | "cataclysm" => Flavor::Cata,
            "mists" | "mop" => Flavor::Mists,
            "titan" => Flavor::Titan,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Flavor::Forever => "WoW: Forever",
            Flavor::Retail => "Retail (Midnight)",
            Flavor::ClassicEra => "Classic Era",
            Flavor::Tbc => "Burning Crusade Classic",
            Flavor::Wotlk => "WotLK Classic",
            Flavor::Cata => "Cataclysm Classic",
            Flavor::Mists => "MoP Classic",
            Flavor::Titan => "Titan Reforged",
        }
    }

    /// CurseForge `gameVersionTypeId` (authoritative list: /v1/games/1/version-types).
    /// Map a dotted client version ("1.60.1", "5.5.4", "2.5.6") to its flavor.
    pub fn from_version(p: &str) -> Option<Flavor> {
        let mut parts = p.trim().split('.');
        let major: u32 = parts.next()?.trim().parse().ok()?;
        let minor: u32 = parts.next().unwrap_or("0").trim().parse().ok()?;
        Some(match major {
            1 if minor >= 60 => Flavor::Forever, // 1.60.1, 1.61.x, ...
            1 => Flavor::ClassicEra,             // 1.13 - 1.15
            2 => Flavor::Tbc,                    // 2.5.x
            3 if minor >= 80 => Flavor::Titan,   // 3.80.x
            3 => Flavor::Wotlk,                  // 3.4.x
            4 => Flavor::Cata,                   // 4.4.x
            5 => Flavor::Mists,                  // 5.5.x
            11..=13 => Flavor::Retail,           // 11.x (Dragonflight), 12.x/13.x (Midnight+)
            _ => return None,
        })
    }

    pub fn curse_type_id(self) -> u32 {
        match self {
            Flavor::Forever => 88568,
            Flavor::Retail => 517,
            Flavor::ClassicEra => 67408,
            Flavor::Tbc => 73246,
            Flavor::Wotlk => 73713,
            Flavor::Cata => 77522,
            Flavor::Mists => 79434,
            Flavor::Titan => 81212,
        }
    }

    /// Edition folder inside a shared install; None for retail (root `_retail_`).
    pub fn edition_dir(self) -> Option<&'static str> {
        match self {
            Flavor::Forever => Some("_forever_"),
            Flavor::Retail => None,
            Flavor::ClassicEra => Some("_classic_"),
            Flavor::Tbc => Some("_classic_"),
            Flavor::Wotlk => Some("_classic_"),
            Flavor::Cata => Some("_classic_"),
            Flavor::Mists => Some("_classic_"),
            Flavor::Titan => Some("_classic_"),
        }
    }

    /// Suffix tokens recognized on TOC file names, most specific first.
    /// e.g. Forever: `Name_Forever.toc`, `Name_Camelot.toc`, `Name_Mainline.toc`.
    pub fn toc_suffixes(self) -> &'static [&'static str] {
        match self {
            Flavor::Forever => &["forever", "camelot", "mainline"],
            Flavor::Retail => &["mainline", "standard"],
            Flavor::ClassicEra => &["classic", "vanilla", "era", "mainline"],
            Flavor::Tbc => &["bcc", "tbc", "classic", "mainline"],
            Flavor::Wotlk => &["wotlkc", "wrath", "classic", "mainline"],
            Flavor::Cata => &["cata", "classic", "mainline"],
            Flavor::Mists => &["mists", "classic", "mainline"],
            Flavor::Titan => &["titan", "classic", "mainline"],
        }
    }

    /// WowUp Hub `gameType` path segment.
    pub fn hub_game_type(self) -> &'static str {
        match self {
            Flavor::Forever => "forever",
            Flavor::Retail => "retail",
            Flavor::ClassicEra => "classic",
            Flavor::Tbc => "burningCrusade",
            Flavor::Wotlk => "wotlk",
            Flavor::Cata => "cata",
            Flavor::Mists => "mists",
            Flavor::Titan => "titan",
        }
    }

    /// BigWigs-packager / release.json flavor token.
    pub fn packager_flavor(self) -> &'static str {
        match self {
            Flavor::Forever => "forever",
            Flavor::Retail => "mainline",
            Flavor::ClassicEra => "classic",
            Flavor::Tbc => "bcc",
            Flavor::Wotlk => "wrath",
            Flavor::Cata => "cata",
            Flavor::Mists => "mists",
            Flavor::Titan => "titan",
        }
    }
}

impl std::fmt::Display for Flavor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `16001 -> "1.60.1"`, `120105 -> "12.1.5"`.
pub fn interface_to_version(iface: u32) -> String {
    let major = iface / 10000;
    let minor = (iface / 100) % 100;
    let patch = iface % 100;
    format!("{major}.{minor}.{patch}")
}

/// Known-current client version per flavor (used until the user pins one in the
/// `[app] game_version` setting; also the basis for interface numbers).
pub fn default_client_version(flavor: Flavor) -> String {
    match flavor {
        Flavor::Forever => "1.60.1",
        Flavor::Retail => "12.1.5",
        Flavor::ClassicEra => "1.15.9",
        Flavor::Tbc => "2.5.6",
        Flavor::Wotlk => "3.4.5",
        Flavor::Cata => "4.4.2",
        Flavor::Mists => "5.5.4",
        Flavor::Titan => "3.80.2",
    }
    .to_string()
}

/// `"1.60.1" -> Some(16001)`. Accepts `major.minor` (patch 0).
pub fn version_to_interface(v: &str) -> Option<u32> {
    let mut parts = v.trim().split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next().unwrap_or("0").parse().ok()?;
    let patch: u32 = parts.next().unwrap_or("0").parse().ok()?;
    Some(major * 10000 + minor * 100 + patch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interface_math() {
        assert_eq!(interface_to_version(16001), "1.60.1");
        assert_eq!(interface_to_version(120105), "12.1.5");
        assert_eq!(version_to_interface("1.60.1"), Some(16001));
        assert_eq!(version_to_interface("12.1.5"), Some(120105));
        assert_eq!(version_to_interface("1.60"), Some(16000));
        assert_eq!(version_to_interface("garbage"), None);
    }

    #[test]
    fn flavor_roundtrip() {
        for f in Flavor::ALL {
            assert_eq!(Flavor::parse(f.as_str()), Some(*f), "roundtrip {}", f.as_str());
        }
        assert_eq!(Flavor::parse("camelot"), Some(Flavor::Forever));
        assert_eq!(Flavor::Forever.curse_type_id(), 88568);
    }
}
