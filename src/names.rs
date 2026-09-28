//! Common names of well-known deep-sky objects, so repository folders read
//! `M_76_Barbell_Nebula` rather than just `M_76`. The user can add names or
//! replace these in the settings (`[object_names]` in the config file).

use std::collections::BTreeMap;
use std::path::Path;

/// Catalogue designation -> common name. Only names in wide use; objects
/// known mainly by their number are left out.
const BUILTIN: &[(&str, &str)] = &[
    // Messier
    ("M 1", "Crab Nebula"),
    ("M 6", "Butterfly Cluster"),
    ("M 7", "Ptolemy Cluster"),
    ("M 8", "Lagoon Nebula"),
    ("M 11", "Wild Duck Cluster"),
    ("M 13", "Hercules Cluster"),
    ("M 16", "Eagle Nebula"),
    ("M 17", "Omega Nebula"),
    ("M 20", "Trifid Nebula"),
    ("M 22", "Sagittarius Cluster"),
    ("M 24", "Sagittarius Star Cloud"),
    ("M 27", "Dumbbell Nebula"),
    ("M 31", "Andromeda Galaxy"),
    ("M 33", "Triangulum Galaxy"),
    ("M 42", "Orion Nebula"),
    ("M 43", "De Mairan's Nebula"),
    ("M 44", "Beehive Cluster"),
    ("M 45", "Pleiades"),
    ("M 51", "Whirlpool Galaxy"),
    ("M 57", "Ring Nebula"),
    ("M 63", "Sunflower Galaxy"),
    ("M 64", "Black Eye Galaxy"),
    ("M 74", "Phantom Galaxy"),
    ("M 76", "Barbell Nebula"),
    ("M 81", "Bode's Galaxy"),
    ("M 82", "Cigar Galaxy"),
    ("M 83", "Southern Pinwheel Galaxy"),
    ("M 87", "Virgo A"),
    ("M 97", "Owl Nebula"),
    ("M 101", "Pinwheel Galaxy"),
    ("M 104", "Sombrero Galaxy"),
    // NGC
    ("NGC 40", "Bow-Tie Nebula"),
    ("NGC 104", "47 Tucanae"),
    ("NGC 246", "Skull Nebula"),
    ("NGC 253", "Sculptor Galaxy"),
    ("NGC 281", "Pacman Nebula"),
    ("NGC 457", "Owl Cluster"),
    ("NGC 869", "Double Cluster"),
    ("NGC 884", "Double Cluster"),
    ("NGC 1499", "California Nebula"),
    ("NGC 1514", "Crystal Ball Nebula"),
    ("NGC 1535", "Cleopatra's Eye"),
    ("NGC 1579", "Northern Trifid Nebula"),
    ("NGC 1952", "Crab Nebula"),
    ("NGC 1976", "Orion Nebula"),
    ("NGC 1977", "Running Man Nebula"),
    ("NGC 2024", "Flame Nebula"),
    ("NGC 2070", "Tarantula Nebula"),
    ("NGC 2174", "Monkey Head Nebula"),
    ("NGC 2237", "Rosette Nebula"),
    ("NGC 2244", "Rosette Cluster"),
    ("NGC 2261", "Hubble's Variable Nebula"),
    ("NGC 2264", "Christmas Tree Cluster"),
    ("NGC 2359", "Thor's Helmet"),
    ("NGC 2392", "Eskimo Nebula"),
    ("NGC 3132", "Southern Ring Nebula"),
    ("NGC 3242", "Ghost of Jupiter"),
    ("NGC 3372", "Carina Nebula"),
    ("NGC 3628", "Hamburger Galaxy"),
    ("NGC 4038", "Antennae Galaxies"),
    ("NGC 4039", "Antennae Galaxies"),
    ("NGC 4565", "Needle Galaxy"),
    ("NGC 4631", "Whale Galaxy"),
    ("NGC 4656", "Hockey Stick Galaxy"),
    ("NGC 4755", "Jewel Box"),
    ("NGC 5128", "Centaurus A"),
    ("NGC 5139", "Omega Centauri"),
    ("NGC 6302", "Bug Nebula"),
    ("NGC 6334", "Cat's Paw Nebula"),
    ("NGC 6543", "Cat's Eye Nebula"),
    ("NGC 6822", "Barnard's Galaxy"),
    ("NGC 6826", "Blinking Planetary"),
    ("NGC 6888", "Crescent Nebula"),
    ("NGC 6946", "Fireworks Galaxy"),
    ("NGC 6960", "Western Veil Nebula"),
    ("NGC 6992", "Eastern Veil Nebula"),
    ("NGC 7000", "North America Nebula"),
    ("NGC 7009", "Saturn Nebula"),
    ("NGC 7023", "Iris Nebula"),
    ("NGC 7293", "Helix Nebula"),
    ("NGC 7380", "Wizard Nebula"),
    ("NGC 7635", "Bubble Nebula"),
    ("NGC 7662", "Blue Snowball Nebula"),
    ("NGC 7789", "Caroline's Rose Cluster"),
    // IC
    ("IC 63", "Ghost of Cassiopeia"),
    ("IC 342", "Hidden Galaxy"),
    ("IC 405", "Flaming Star Nebula"),
    ("IC 410", "Tadpoles Nebula"),
    ("IC 434", "Horsehead Nebula"),
    ("IC 443", "Jellyfish Nebula"),
    ("IC 1318", "Butterfly Nebula"),
    ("IC 1396", "Elephant's Trunk Nebula"),
    ("IC 1795", "Fishhead Nebula"),
    ("IC 1805", "Heart Nebula"),
    ("IC 1848", "Soul Nebula"),
    ("IC 2118", "Witch Head Nebula"),
    ("IC 2177", "Seagull Nebula"),
    ("IC 2602", "Southern Pleiades"),
    ("IC 2944", "Running Chicken Nebula"),
    ("IC 4592", "Blue Horsehead Nebula"),
    ("IC 5070", "Pelican Nebula"),
    ("IC 5146", "Cocoon Nebula"),
    ("B 33", "Horsehead Nebula"),
    // Caldwell
    ("C 2", "Bow-Tie Nebula"),
    ("C 4", "Iris Nebula"),
    ("C 5", "Hidden Galaxy"),
    ("C 6", "Cat's Eye Nebula"),
    ("C 9", "Cave Nebula"),
    ("C 11", "Bubble Nebula"),
    ("C 12", "Fireworks Galaxy"),
    ("C 13", "Owl Cluster"),
    ("C 14", "Double Cluster"),
    ("C 15", "Blinking Planetary"),
    ("C 19", "Cocoon Nebula"),
    ("C 20", "North America Nebula"),
    ("C 22", "Blue Snowball Nebula"),
    ("C 24", "Perseus A"),
    ("C 27", "Crescent Nebula"),
    ("C 31", "Flaming Star Nebula"),
    ("C 32", "Whale Galaxy"),
    ("C 33", "Eastern Veil Nebula"),
    ("C 34", "Western Veil Nebula"),
    ("C 38", "Needle Galaxy"),
    ("C 39", "Eskimo Nebula"),
    ("C 41", "Hyades"),
    ("C 46", "Hubble's Variable Nebula"),
    ("C 49", "Rosette Nebula"),
    ("C 53", "Spindle Galaxy"),
    ("C 55", "Saturn Nebula"),
    ("C 56", "Skull Nebula"),
    ("C 57", "Barnard's Galaxy"),
    ("C 59", "Ghost of Jupiter"),
    ("C 60", "Antennae Galaxies"),
    ("C 61", "Antennae Galaxies"),
    ("C 63", "Helix Nebula"),
    ("C 65", "Sculptor Galaxy"),
    ("C 69", "Bug Nebula"),
    ("C 74", "Southern Ring Nebula"),
    ("C 77", "Centaurus A"),
    ("C 80", "Omega Centauri"),
    ("C 92", "Carina Nebula"),
    ("C 94", "Jewel Box"),
    ("C 99", "Coalsack Nebula"),
    ("C 100", "Running Chicken Nebula"),
    ("C 102", "Southern Pleiades"),
    ("C 103", "Tarantula Nebula"),
    ("C 106", "47 Tucanae"),
];

/// Comparison key for a designation: "M 76", "M76", "m_76" and "Messier 76"
/// all become "M76".
pub fn key(object: &str) -> String {
    let up = object.trim().to_uppercase();
    let up = up
        .strip_prefix("MESSIER")
        .map(|r| format!("M{r}"))
        .or_else(|| up.strip_prefix("CALDWELL").map(|r| format!("C{r}")))
        .unwrap_or(up);
    up.chars()
        .filter(|c| !matches!(c, ' ' | '_' | '-'))
        .collect()
}

/// The common name for `object`, from the user's names first, then the
/// built-in list.
pub fn common_name(object: &str, custom: &BTreeMap<String, String>) -> Option<String> {
    let k = key(object);
    if k.is_empty() {
        return None;
    }
    // A custom entry with an empty name switches the built-in name off.
    if let Some((_, n)) = custom.iter().find(|(o, _)| key(o) == k) {
        return Some(n.trim().to_string()).filter(|n| !n.is_empty());
    }
    BUILTIN
        .iter()
        .find(|(o, _)| key(o) == k)
        .map(|(_, n)| n.to_string())
}

/// Folder name for an object: "M 76" -> "M_76_Barbell_Nebula"; objects
/// without a common name keep just their designation.
pub fn object_folder(object: &str, custom: &BTreeMap<String, String>) -> String {
    let base = crate::util::sanitize(object);
    match common_name(object, custom) {
        Some(name) => format!("{base}_{}", crate::util::sanitize(&name)),
        None => base,
    }
}

/// Catalogue prefixes recognised at the start of a folder name.
const CATALOGUES: &[&str] = &[
    "MESSIER", "CALDWELL", "BARNARD", "ABELL", "SH2", "NGC", "IC", "SH", "LDN", "LBN", "VDB",
    "ARP", "UGC", "PGC", "MEL", "CR", "TR", "HCG", "CED", "GUM", "RCW", "M", "C", "B",
];

/// Folders that stacking programs and archives put results in; the target
/// is named by the folder above.
const OUTPUT_FOLDERS: &[&str] = &[
    "stacked",
    "stack",
    "stacks",
    "light",
    "lights",
    "master",
    "masters",
    "masterlight",
    "integration",
    "output",
    "processed",
    "pixinsight",
    "wbpp",
    "fbpp",
    "fits",
    "xisf",
    "registered",
    "calibrated",
];

/// Folder names that say nothing about the target: looking further up would
/// only find unrelated folders.
const GENERIC: &[&str] = &[
    "images",
    "astro",
    "astrophotography",
    "photo",
    "photos",
    "pictures",
    "archive",
    "data",
    "logs",
    "new",
    "tmp",
    "temp",
];

/// The catalogue designation a folder name starts with, as written:
/// "NGC 1333 Embryo Nebula" -> "NGC 1333", "Sh 2-132_Lion Nebula_EXP_30" ->
/// "Sh 2-132", "SH2-158 Northern Lagoon" -> "SH2-158", "C4 Iris" -> "C4".
fn designation(name: &str) -> Option<String> {
    let name = name.trim();
    let prefix_len = name.chars().take_while(|c| c.is_ascii_alphabetic()).count();
    let prefix = &name[..prefix_len];
    if !CATALOGUES.iter().any(|c| c.eq_ignore_ascii_case(prefix)) {
        return None;
    }
    let rest = &name[prefix_len..];
    let gap = rest.len() - rest.trim_start_matches([' ', '_']).len();
    let digits = |s: &str| s.chars().take_while(|c| c.is_ascii_digit()).count();
    let num = &rest[gap..];
    let mut end = digits(num);
    if end == 0 {
        return None;
    }
    // Sharpless numbers carry the catalogue edition: "Sh 2-132".
    if num[end..].starts_with('-') && digits(&num[end + 1..]) > 0 {
        end += 1 + digits(&num[end + 1..]);
    }
    // The designation must end there: "M 13 Hercules" but not "M13x".
    if num[end..]
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
    {
        return None;
    }
    Some(format!(
        "{prefix}{}{}",
        &rest[..gap].replace('_', " "),
        &num[..end]
    ))
}

fn is_generic(name: &str) -> bool {
    let n = name.trim().to_lowercase();
    GENERIC.contains(&n.as_str())
        || n.is_empty()
        || n.chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '-' | '_' | ' '))
}

/// The target of an image in `path`, from the folders it is in, for stacks
/// whose header does not name it (PixInsight master lights). Output folders
/// such as "master" or "Stacked" are skipped; the next folder up decides,
/// by its catalogue designation or else its whole name. None when that
/// folder is a generic one ("Astro", a year).
pub fn object_from_folders(path: &Path) -> Option<String> {
    let folder = path
        .ancestors()
        .skip(1)
        .take(3)
        .filter_map(|p| p.file_name())
        .map(|n| n.to_string_lossy())
        .find(|n| !OUTPUT_FOLDERS.contains(&n.trim().to_lowercase().as_str()))?;
    if let Some(d) = designation(&folder) {
        return Some(d);
    }
    (!is_generic(&folder)).then(|| folder.replace('_', " ").trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn objects_from_folder_names() {
        let obj = |p: &str| object_from_folders(Path::new(p));
        let cases = [
            (
                "/a/NGC 1333 Embryo Nebula/masterLight.xisf",
                Some("NGC 1333"),
            ),
            (
                "/a/Sh 2-132_Lion Nebula_EXP_30_GAIN_60_2026-07-15-00-03-13-569_XISF/m.xisf",
                Some("Sh 2-132"),
            ),
            (
                "/a/SH2-158 Northern Lagoon Nebula SH 2-158/m.xisf",
                Some("SH2-158"),
            ),
            ("/a/SH 2-157 Lobster Claw Nebula/m.xisf", Some("SH 2-157")),
            (
                "/a/NGC 4631 (Whale Galaxy) + NGC 4656 (Crowbar Galaxy)/m.xisf",
                Some("NGC 4631"),
            ),
            ("/a/NGC 4449 C 21 Box Galaxy/m.xisf", Some("NGC 4449")),
            ("/a/C4 Iris Nebula/m.xisf", Some("C4")),
            (
                "/a/M 13 Hercules Globular Cluster 2026/master/m.xisf",
                Some("M 13"),
            ),
            ("/a/Markarian's Chain/m.xisf", Some("Markarian's Chain")),
            ("/a/Messier_31/Stacked/m.xisf", Some("Messier 31")),
            ("/a/Medusa Nebula/m.xisf", Some("Medusa Nebula")),
            ("/mnt/nas/photo/Astro/Stacked/m.xisf", None),
            ("/data/2025/m.xisf", None),
            ("/data/2025/master/m.xisf", None),
            ("/a/NGC 7000/2025/m.xisf", None),
        ];
        for (path, want) in cases {
            assert_eq!(obj(path).as_deref(), want, "{path}");
        }
    }

    #[test]
    fn folders() {
        let none = BTreeMap::new();
        assert_eq!(object_folder("M 76", &none), "M_76_Barbell_Nebula");
        assert_eq!(
            object_folder("Messier 31", &none),
            "Messier_31_Andromeda_Galaxy"
        );
        assert_eq!(
            object_folder("NGC7000", &none),
            "NGC7000_North_America_Nebula"
        );
        assert_eq!(object_folder("M 2", &none), "M_2");
        assert_eq!(
            object_folder("M 76 Barbell Nebula", &none),
            "M_76_Barbell_Nebula"
        );
        let custom = BTreeMap::from([
            ("M 76".to_string(), "Little Dumbbell".to_string()),
            ("M2".to_string(), "Aquarius Globular".to_string()),
            ("M 31".to_string(), String::new()),
        ]);
        assert_eq!(object_folder("M 76", &custom), "M_76_Little_Dumbbell");
        assert_eq!(object_folder("M 2", &custom), "M_2_Aquarius_Globular");
        // An empty custom name switches the built-in one off.
        assert_eq!(object_folder("M 31", &custom), "M_31");
    }
}
