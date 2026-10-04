//! Nicknames of objects ("C 7" = "Spiral Galaxy"), kept in the catalogue
//! (table objectNickname) and
//! learned from the names the user gives folders and processed pictures.
//! They count as common names: in the folder name, the search and the lists.

use crate::config::Config;
use crate::names;
use rusqlite::{params, Connection};
use std::collections::BTreeMap;
use std::path::Path;

/// Words that end a nickname: what telescopes and processing add to a name.
const NOT_NAMES: &[&str] = &[
    "exp",
    "gain",
    "xisf",
    "fits",
    "fit",
    "stacked",
    "stack",
    "final",
    "crop",
    "cropped",
    "edit",
    "edited",
    "processed",
    "copy",
    "labelled",
    "labeled",
    "annotated",
    "starless",
    "astrowizard",
    "manual",
    "mosaic",
    "new",
    "test",
    "raw",
    "master",
    "masterlight",
    "drizzle",
    "rgb",
    "lrgb",
    "hoo",
    "sub",
    "subs",
    "light",
    "lights",
    "darks",
    "flats",
    "bias",
    "stacks",
    "video",
    "sho",
    "ha",
    "oiii",
    "sii",
    "lp",
    "ircut",
    "astro",
    "tele",
    "wide",
    "and",
];

fn is_catalogue(word: &str) -> bool {
    names::CATALOGUES
        .iter()
        .any(|c| c.eq_ignore_ascii_case(word))
}

/// Splits a name that starts with a catalogue number into that number and
/// the rest: "Sh 2-132_Lion Nebula" -> ("Sh 2-132", "_Lion Nebula").
fn designation(name: &str) -> Option<(String, &str)> {
    let prefix_len = name.chars().take_while(|c| c.is_ascii_alphabetic()).count();
    let prefix = &name[..prefix_len];
    if !is_catalogue(prefix) {
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
    if num[end..]
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
    {
        return None;
    }
    let object = format!("{prefix}{}{}", rest[..gap].replace('_', " "), &num[..end]);
    Some((object, &num[end..]))
}

/// The object and nickname in a folder or picture name:
/// "C 7 Spiral Galaxy" -> ("C 7", "Spiral Galaxy"). None when the name does
/// not start with a catalogue number or nothing name-like follows it.
pub fn in_name(name: &str) -> Option<(String, String)> {
    let name = name.trim();
    // DWARF folders: "DWARF_RAW_TELE_NGC281 - Pacman Nebula_EXP_30...".
    let name = ["DWARF_RAW_TELE_", "DWARF_RAW_WIDE_"]
        .iter()
        .find_map(|p| name.strip_prefix(p))
        .unwrap_or(name);
    let (object, rest) = designation(name)?;
    let tokens: Vec<&str> = rest
        .split([' ', '_'])
        .map(|t| t.trim_matches(|c: char| "()[],.".contains(c)))
        .filter(|t| !t.is_empty())
        .collect();
    let mut words: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < tokens.len() && words.len() < 5 {
        let t = tokens[i];
        let next_is_number = tokens
            .get(i + 1)
            .is_some_and(|n| n.starts_with(|c: char| c.is_ascii_digit()));
        if words.is_empty() && t == "-" {
            i += 1;
        } else if words.is_empty() && is_catalogue(t) && next_is_number {
            // A second number for the same object: "NGC 4449 C 21 Box Galaxy".
            i += 2;
        } else if t
            .chars()
            .all(|c| c.is_alphabetic() || c == '\'' || c == '-')
            && t.chars().any(|c| c.is_alphabetic())
            && !NOT_NAMES.contains(&t.to_lowercase().as_str())
            && !(is_catalogue(t) && next_is_number)
        {
            words.push(t);
            i += 1;
        } else {
            break;
        }
    }
    let nick = words.join(" ");
    (nick.chars().filter(|c| c.is_alphabetic()).count() >= 3).then_some((object, nick))
}

fn ensure(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS objectNickname (objectKey TEXT NOT NULL PRIMARY KEY, \
         object TEXT NOT NULL, nickname TEXT NOT NULL)",
        [],
    )?;
    Ok(())
}

/// Object -> nickname, as stored. Empty when the table does not exist yet.
pub fn load(conn: &Connection) -> BTreeMap<String, String> {
    let read = || -> rusqlite::Result<BTreeMap<String, String>> {
        conn.prepare("SELECT object, nickname FROM objectNickname")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect()
    };
    read().unwrap_or_default()
}

/// Replaces the stored nicknames (the Settings page).
pub fn replace(conn: &mut Connection, nicknames: &[(String, String)]) -> rusqlite::Result<()> {
    ensure(conn)?;
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM objectNickname", [])?;
    for (object, nick) in nicknames {
        tx.execute(
            "INSERT OR REPLACE INTO objectNickname (objectKey, object, nickname) VALUES (?1, ?2, ?3)",
            params![names::key(object), object, nick],
        )?;
    }
    tx.commit()
}

/// Adds the nicknames to the object names of `cfg`. The names set on the
/// Settings page win, also an empty one that switches a name off.
pub fn merge(cfg: &mut Config, nicknames: BTreeMap<String, String>) {
    for (object, nick) in nicknames {
        let k = names::key(&object);
        if !cfg.object_names.keys().any(|o| names::key(o) == k) {
            cfg.object_names.insert(object, nick);
        }
    }
}

/// Learns nicknames from folder and picture names, for objects that have no
/// common name yet, and adds them to `cfg`. Of several spellings the most
/// used wins, then the shortest. Stored unless `dry_run`.
pub fn learn(
    conn: &Connection,
    cfg: &mut Config,
    found: impl IntoIterator<Item = String>,
    dry_run: bool,
) -> Vec<(String, String)> {
    // key -> (object, nickname -> times seen)
    let mut seen: BTreeMap<String, (String, BTreeMap<String, usize>)> = BTreeMap::new();
    for name in found {
        if let Some((object, nick)) = in_name(&name) {
            let e = seen
                .entry(names::key(&object))
                .or_insert((object, BTreeMap::new()));
            *e.1.entry(nick).or_default() += 1;
        }
    }
    let mut learned = Vec::new();
    for (key, (object, nicks)) in seen {
        let known = names::common_name(&object, &cfg.object_names).is_some()
            || cfg.object_names.keys().any(|o| names::key(o) == key);
        let best = nicks
            .into_iter()
            .max_by(|a, b| a.1.cmp(&b.1).then(b.0.len().cmp(&a.0.len())));
        if let (false, Some((nick, _))) = (known, best) {
            learned.push((object, nick));
        }
    }
    for (object, nick) in &learned {
        if !dry_run {
            let stored = ensure(conn).and_then(|()| {
                conn.execute(
                    "INSERT OR IGNORE INTO objectNickname (objectKey, object, nickname) VALUES (?1, ?2, ?3)",
                    params![names::key(object), object, nick],
                )
            });
            if let Err(e) = stored {
                log::warn!("Nickname of {object} not saved: {e}");
            }
        }
        log::info!("Nickname: {object} = {nick}");
        cfg.object_names.insert(object.clone(), nick.clone());
    }
    learned
}

/// The settings with the stored nicknames among the object names.
pub fn effective(conn: &Connection, cfg: &Config) -> Config {
    let mut cfg = cfg.clone();
    merge(&mut cfg, load(conn));
    cfg
}

/// The names nicknames are learned from under `dir`: the folders and the
/// processed pictures. `depth` limits how far down it looks.
pub fn names_under(dir: &Path, depth: usize, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
        let name = if is_dir {
            path.file_name()
        } else {
            path.file_stem()
        };
        let Some(name) = name.map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        if is_dir {
            out.push(name);
            if depth > 0 {
                names_under(&path, depth - 1, out);
            }
        } else if crate::pictures::is_picture(&path) {
            out.push(name);
        }
    }
}

/// Learns nicknames from `dir`: its own name, its folders and its pictures.
pub fn learn_from(
    conn: &Connection,
    cfg: &mut Config,
    dir: &Path,
    dry_run: bool,
) -> Vec<(String, String)> {
    let mut found: Vec<String> = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .into_iter()
        .collect();
    names_under(dir, 4, &mut found);
    learn(conn, cfg, found, dry_run)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nicknames_in_names() {
        let nick = |s: &str| in_name(s).map(|(o, n)| format!("{o} = {n}"));
        let cases = [
            ("C 7 Spiral Galaxy", Some("C 7 = Spiral Galaxy")),
            ("C4 Iris Nebula", Some("C4 = Iris Nebula")),
            (
                "IC 2574 Coddington's Nebula",
                Some("IC 2574 = Coddington's Nebula"),
            ),
            (
                "IC 5146 Cocoon Nebula 2026",
                Some("IC 5146 = Cocoon Nebula"),
            ),
            (
                "M 35 Shoe-buckle Cluster",
                Some("M 35 = Shoe-buckle Cluster"),
            ),
            ("NGC 4449 C 21 Box Galaxy", Some("NGC 4449 = Box Galaxy")),
            ("NGC 7331 C 30", None),
            (
                "NGC 4631 (Whale Galaxy) + NGC 4656 (Crowbar Galaxy)",
                Some("NGC 4631 = Whale Galaxy"),
            ),
            (
                "SH2-158 Northern Lagoon Nebula SH 2-158",
                Some("SH2-158 = Northern Lagoon Nebula"),
            ),
            (
                "Sh 2-132_Lion Nebula_EXP_30_GAIN_60_2026-07-15-00-03-13-569_XISF",
                Some("Sh 2-132 = Lion Nebula"),
            ),
            (
                "DWARF_RAW_TELE_NGC281 - Pacman Nebula_EXP_30_GAIN_60_2026-10-02",
                Some("NGC281 = Pacman Nebula"),
            ),
            (
                "NGC 869 Double Clusters_labelled",
                Some("NGC 869 = Double Clusters"),
            ),
            ("DWARF_RAW_TELE_M 67_EXP_30_GAIN_60", None),
            ("DWARF_RAW_TELE_NGC6997_57Cyg_Manual", None),
            ("M 31 final", None),
            // Seestar keeps the sub-frames of M 2 in "M 2_sub".
            ("M 2_sub", None),
            ("M 2_mosaic_sub", None),
            ("Markarian's Chain", None),
            ("Stacked_47_M 39_10.0s_IRCUT", None),
        ];
        for (name, want) in cases {
            assert_eq!(nick(name).as_deref(), want, "{name}");
        }
    }

    #[test]
    fn learns_only_what_has_no_name_yet() {
        let conn = Connection::open_in_memory().unwrap();
        let mut cfg = Config::default();
        cfg.object_names.insert("C 36".into(), String::new());
        let found = [
            "C 7 Spiral Galaxy",
            "C7 Spiral Galaxy crop",
            "C 7 Spiral Galaxy in Camelopardalis",
            "M 76 Little Dumbbell",
            "C 36 Koi Fish Galaxy",
        ];
        let learned = learn(&conn, &mut cfg, found.map(String::from), false);
        // M 76 has a built-in name and C 36 was switched off in Settings.
        assert_eq!(
            learned,
            vec![("C 7".to_string(), "Spiral Galaxy".to_string())]
        );
        assert_eq!(
            names::object_folder("C 7", &cfg.object_names),
            "C_7_Spiral_Galaxy"
        );
        assert_eq!(
            load(&conn).get("C 7").map(String::as_str),
            Some("Spiral Galaxy")
        );
    }
}
