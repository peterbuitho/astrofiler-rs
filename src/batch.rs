//! Batch file management: bulk header/catalogue edits (including the original's
//! "Merge Objects"), deleting, exporting, verifying, duplicate handling and
//! rebuilding the catalogue from the repository.

use crate::config::Config;
use crate::db::{self, FitsFile};
use crate::fits::{self, Value};
use crate::ingest;
use crate::progress::Progress;
use crate::util::{self, FrameKind};
use anyhow::{bail, Result};
use rayon::prelude::*;
use rusqlite::{params, Connection};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Header keywords that can be edited in bulk, with their catalogue columns.
pub const EDITABLE: &[(&str, &str, Option<&str>)] = &[
    ("OBJECT", "fitsFileObject", Some("fitsSessionObjectName")),
    ("FILTER", "fitsFileFilter", Some("fitsSessionFilter")),
    ("TELESCOP", "fitsFileTelescop", Some("fitsSessionTelescope")),
    ("INSTRUME", "fitsFileInstrument", Some("fitsSessionImager")),
    ("OBSERVER", "fitsFileObserver", None),
    ("NOTES", "fitsFileNotes", None),
];

fn column_for(card: &str) -> Result<(&'static str, Option<&'static str>)> {
    EDITABLE
        .iter()
        .find(|(c, _, _)| c.eq_ignore_ascii_case(card))
        .map(|(_, f, s)| (*f, *s))
        .ok_or_else(|| anyhow::anyhow!("{card} can't be edited; editable keywords: OBJECT, FILTER, TELESCOP, INSTRUME, OBSERVER, NOTES"))
}

#[derive(Debug, Clone, Copy, Default)]
pub struct EditOptions {
    /// Rewrite the keyword inside the FITS files too.
    pub update_headers: bool,
    /// Re-name and re-file the files to match the new value.
    pub refile: bool,
}

#[derive(Debug, Default)]
pub struct EditReport {
    pub updated: usize,
    pub moved: usize,
    pub errors: Vec<(String, String)>,
}

/// Set `card` = `value` on the given files.
pub fn set_field(
    conn: &mut Connection,
    cfg: &Config,
    ids: &[String],
    card: &str,
    value: &str,
    opts: EditOptions,
    progress: &dyn Progress,
) -> Result<EditReport> {
    let (column, _) = column_for(card)?;
    let card = card.to_uppercase();
    let mut report = EditReport::default();
    let tx = conn.transaction()?;
    for (i, id) in ids.iter().enumerate() {
        progress.update(i + 1, ids.len(), &format!("Updating {card}"));
        let Some(file) = db::file_by_id(&tx, id)? else {
            continue;
        };
        // Calibration frames get fixed OBJECT names; don't rename them.
        if card == "OBJECT"
            && FrameKind::classify(file.image_type.as_deref().unwrap_or(""))
                != Some(FrameKind::Light)
        {
            continue;
        }
        let path = PathBuf::from(&file.name);
        let mut new_path = path.clone();
        let mut new_hash = file.hash.clone();
        if (opts.update_headers || opts.refile) && path.exists() {
            match edit_file(cfg, &path, &card, value, opts) {
                Ok((p, h)) => {
                    if p != path {
                        report.moved += 1;
                    }
                    new_path = p;
                    if let Some(h) = h {
                        new_hash = Some(h);
                    }
                }
                Err(e) => {
                    report.errors.push((file.name.clone(), format!("{e:#}")));
                    continue;
                }
            }
        }
        tx.execute(
            &format!("UPDATE fitsFile SET \"{column}\"=?1, fitsFileName=?2, fitsFileHash=?3 WHERE fitsFileId=?4"),
            params![value, util::normalize_path(&new_path), new_hash, id],
        )?;
        report.updated += 1;
    }
    tx.commit()?;
    Ok(report)
}

/// Rewrite the header and/or move one file. Returns the new path and new hash.
fn edit_file(
    cfg: &Config,
    path: &Path,
    card: &str,
    value: &str,
    opts: EditOptions,
) -> Result<(PathBuf, Option<String>)> {
    let mut header = fits::read_primary_header(path)?;
    header.set(card, Value::Str(value.to_string()));
    let mut hash = None;
    if opts.update_headers {
        if fits::is_gzip(path) {
            bail!("can't rewrite the header of a gzip-compressed file");
        }
        fits::rewrite_primary_header(path, &header)?;
        hash = Some(util::sha256_file(path)?);
    }
    let mut new_path = path.to_path_buf();
    if opts.refile && path.starts_with(&cfg.repo) {
        let (name, dir) = ingest::destination(&header, &cfg.repo)?;
        let target = dir.join(name);
        if target != path {
            let target = util::unique_path(&target);
            util::move_file(path, &target)?;
            new_path = target;
        }
    }
    Ok((new_path, hash))
}

/// Rename an object across the catalogue (the original's "Merge Objects").
pub fn merge_objects(
    conn: &mut Connection,
    cfg: &Config,
    from: &str,
    to: &str,
    opts: EditOptions,
    progress: &dyn Progress,
) -> Result<EditReport> {
    let ids: Vec<String> = db::files_where(conn, "fitsFileObject=?1", &[&from])?
        .into_iter()
        .map(|f| f.id)
        .collect();
    if ids.is_empty() {
        bail!("no files with object '{from}'");
    }
    let report = set_field(conn, cfg, &ids, "OBJECT", to, opts, progress)?;
    conn.execute(
        "UPDATE fitsSession SET fitsSessionObjectName=?1 WHERE fitsSessionObjectName=?2",
        params![to, from],
    )?;
    if opts.refile {
        remove_empty_dirs(&cfg.repo.join("Light"));
    }
    Ok(report)
}

/// Delete files from the catalogue, and optionally from disk.
pub fn delete_files(
    conn: &mut Connection,
    ids: &[String],
    from_disk: bool,
) -> Result<(usize, Vec<(String, String)>)> {
    let tx = conn.transaction()?;
    let mut n = 0;
    let mut errors = Vec::new();
    for id in ids {
        let Some(f) = db::file_by_id(&tx, id)? else {
            continue;
        };
        if from_disk && Path::new(&f.name).exists() {
            if let Err(e) = std::fs::remove_file(&f.name) {
                errors.push((f.name.clone(), e.to_string()));
                continue;
            }
        }
        tx.execute("DELETE FROM fitsFile WHERE fitsFileId=?1", [id])?;
        n += 1;
    }
    tx.commit()?;
    Ok((n, errors))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportLayout {
    /// All files in one folder.
    Flat,
    /// `<dest>/<Object>/<Filter or frame type>/`.
    ByObject,
}

/// Copy (or move) files out of the repository, e.g. to prepare a session for
/// stacking in another program.
pub fn export_files(
    conn: &mut Connection,
    ids: &[String],
    dest: &Path,
    layout: ExportLayout,
    move_files: bool,
    progress: &dyn Progress,
) -> Result<usize> {
    let files: Vec<FitsFile> = ids
        .iter()
        .filter_map(|id| db::file_by_id(conn, id).ok().flatten())
        .collect();
    let done = AtomicUsize::new(0);
    let results: Vec<(String, Result<PathBuf>)> = files
        .par_iter()
        .map(|f| {
            let src = PathBuf::from(&f.name);
            let dir = match layout {
                ExportLayout::Flat => dest.to_path_buf(),
                ExportLayout::ByObject => {
                    let kind = FrameKind::classify(f.image_type.as_deref().unwrap_or(""));
                    let sub = match kind {
                        Some(FrameKind::Light) | None => {
                            f.filter.clone().unwrap_or_else(|| "OSC".into())
                        }
                        Some(k) => k.object_name().to_string(),
                    };
                    dest.join(util::sanitize(f.object.as_deref().unwrap_or("Unknown")))
                        .join(util::sanitize(&sub))
                }
            };
            let target = util::unique_path(&dir.join(src.file_name().unwrap_or_default()));
            let r = (|| -> Result<PathBuf> {
                std::fs::create_dir_all(&dir)?;
                if move_files {
                    util::move_file(&src, &target)?;
                } else {
                    std::fs::copy(&src, &target)?;
                }
                Ok(target)
            })();
            let d = done.fetch_add(1, Ordering::Relaxed) + 1;
            progress.update(d, files.len(), &f.file_name());
            (f.id.clone(), r)
        })
        .collect();
    let mut n = 0;
    for (id, r) in results {
        match r {
            Ok(p) => {
                n += 1;
                if move_files {
                    conn.execute(
                        "UPDATE fitsFile SET fitsFileName=?1 WHERE fitsFileId=?2",
                        params![util::normalize_path(&p), id],
                    )?;
                }
            }
            Err(e) => log::warn!("export failed: {e:#}"),
        }
    }
    Ok(n)
}

#[derive(Debug, Default)]
pub struct VerifyReport {
    pub checked: usize,
    pub missing: Vec<FitsFile>,
    pub mismatched: Vec<FitsFile>,
}

/// Check catalogued files still exist and (optionally) still match their hash.
pub fn verify(
    conn: &Connection,
    check_hash: bool,
    progress: &dyn Progress,
) -> Result<VerifyReport> {
    let files = db::all_files(conn, false)?;
    let done = AtomicUsize::new(0);
    let total = files.len();
    let states: Vec<(FitsFile, u8)> = files
        .into_par_iter()
        .map(|f| {
            let p = Path::new(&f.name);
            let state = if !p.exists() {
                1
            } else if check_hash
                && f.hash
                    .as_deref()
                    .is_some_and(|h| util::sha256_file(p).map(|x| x != h).unwrap_or(true))
            {
                2
            } else {
                0
            };
            let d = done.fetch_add(1, Ordering::Relaxed) + 1;
            if d % 64 == 0 || d == total {
                progress.update(d, total, "Verifying files");
            }
            (f, state)
        })
        .collect();
    let mut r = VerifyReport {
        checked: total,
        ..Default::default()
    };
    for (f, s) in states {
        match s {
            1 => r.missing.push(f),
            2 => r.mismatched.push(f),
            _ => {}
        }
    }
    Ok(r)
}

/// Drop catalogue rows whose files no longer exist.
pub fn remove_missing(conn: &mut Connection, progress: &dyn Progress) -> Result<usize> {
    let missing: Vec<String> = verify(conn, false, progress)?
        .missing
        .into_iter()
        .map(|f| f.id)
        .collect();
    Ok(delete_files(conn, &missing, false)?.0)
}

/// Rebuild the catalogue from the files in the repository (the original's
/// "Regenerate"). Existing file and session rows are replaced.
pub fn regenerate(
    conn: &mut Connection,
    cfg: &Config,
    progress: &dyn Progress,
) -> Result<ingest::IngestReport> {
    {
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM fitsFile", [])?;
        tx.execute("DELETE FROM fitsSession", [])?;
        tx.commit()?;
    }
    ingest::ingest_folder(
        conn,
        cfg,
        &cfg.repo,
        ingest::IngestOptions::IN_PLACE,
        progress,
    )
}

pub fn remove_empty_dirs(root: &Path) -> usize {
    let mut removed = 0;
    let dirs: Vec<PathBuf> = walkdir::WalkDir::new(root)
        .contents_first(true)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_dir() && e.path() != root)
        .map(|e| e.into_path())
        .collect();
    for d in dirs {
        if std::fs::remove_dir(&d).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Preview files written by Seestar / DWARF for their app album.
fn is_preview(p: &Path) -> bool {
    let ext = p
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    matches!(ext.as_str(), "jpg" | "jpeg" | "png")
}

/// Find (and unless `dry_run`, delete) JPG/PNG preview images under `root`,
/// then remove any `Thumbnail` folders left empty. FITS and JSON files are
/// never touched. Returns the preview files and their total size.
pub fn clean_previews(root: &Path, dry_run: bool) -> Result<(Vec<PathBuf>, u64)> {
    let previews: Vec<PathBuf> = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file() && is_preview(e.path()))
        .map(|e| e.into_path())
        .collect();
    let bytes = previews
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    if !dry_run {
        for p in &previews {
            std::fs::remove_file(p)?;
        }
        let thumbs: Vec<PathBuf> = walkdir::WalkDir::new(root)
            .contents_first(true)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_dir() && e.file_name() == "Thumbnail")
            .map(|e| e.into_path())
            .collect();
        for d in thumbs {
            std::fs::remove_dir(&d).ok(); // only succeeds when empty
        }
    }
    Ok((previews, bytes))
}

/// Groups of catalogued files that share the same SHA-256.
pub fn duplicate_groups(conn: &Connection) -> Result<Vec<Vec<FitsFile>>> {
    let files = db::files_where(
        conn,
        "fitsFileHash IN (SELECT fitsFileHash FROM fitsFile WHERE fitsFileHash IS NOT NULL \
         AND COALESCE(fitsFileSoftDelete,0)=0 GROUP BY fitsFileHash HAVING count(*) > 1) \
         AND COALESCE(fitsFileSoftDelete,0)=0 ORDER BY fitsFileHash, fitsFileName",
        &[],
    )?;
    let mut groups: BTreeMap<String, Vec<FitsFile>> = BTreeMap::new();
    for f in files {
        groups
            .entry(f.hash.clone().unwrap_or_default())
            .or_default()
            .push(f);
    }
    Ok(groups.into_values().collect())
}

/// Delete every duplicate except one kept copy per group (the first that
/// exists on disk). Returns (files removed, bytes freed).
pub fn remove_duplicates(conn: &mut Connection) -> Result<(usize, u64)> {
    let mut to_delete = Vec::new();
    let mut freed = 0;
    for group in duplicate_groups(conn)? {
        let keep = group
            .iter()
            .position(|f| Path::new(&f.name).exists())
            .unwrap_or(0);
        for (i, f) in group.iter().enumerate() {
            if i != keep {
                // Never delete the kept file even if two rows point at the same path.
                if f.name == group[keep].name {
                    conn.execute("DELETE FROM fitsFile WHERE fitsFileId=?1", [&f.id])?;
                    continue;
                }
                freed += std::fs::metadata(&f.name).map(|m| m.len()).unwrap_or(0);
                to_delete.push(f.id.clone());
            }
        }
    }
    let (n, _) = delete_files(conn, &to_delete, true)?;
    Ok((n, freed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::tests::make_frame;
    use crate::progress::NoProgress;

    #[test]
    fn merge_refiles_and_rewrites_headers() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("in");
        std::fs::create_dir_all(&src).unwrap();
        make_frame(
            &src,
            "1.fits",
            "Light",
            Some("M 31"),
            "2024-10-01T21:00:00",
            60.0,
            Some("L"),
            1.0,
        );
        make_frame(
            &src,
            "2.fits",
            "Light",
            Some("Andromeda"),
            "2024-10-01T22:00:00",
            60.0,
            Some("L"),
            2.0,
        );
        let cfg = Config {
            repo: tmp.path().join("repo"),
            ..Default::default()
        };
        let mut conn = db::open(&tmp.path().join("t.db")).unwrap();
        ingest::ingest_folder(
            &mut conn,
            &cfg,
            &src,
            crate::ingest::IngestOptions::MOVE,
            &NoProgress,
        )
        .unwrap();
        let r = merge_objects(
            &mut conn,
            &cfg,
            "Andromeda",
            "M 31",
            EditOptions {
                update_headers: true,
                refile: true,
            },
            &NoProgress,
        )
        .unwrap();
        assert_eq!(r.updated, 1);
        assert_eq!(r.moved, 1);
        let files = db::all_files(&conn, false).unwrap();
        assert!(files.iter().all(|f| f.object.as_deref() == Some("M 31")));
        for f in &files {
            assert!(f.name.contains("/Light/M_31/"), "{}", f.name);
            assert_eq!(
                fits::read_primary_header(Path::new(&f.name))
                    .unwrap()
                    .get_str("OBJECT")
                    .as_deref(),
                Some("M 31")
            );
            assert_eq!(
                util::sha256_file(Path::new(&f.name)).unwrap(),
                f.hash.clone().unwrap()
            );
        }
        assert!(!cfg.repo.join("Light/Andromeda").exists());
        let v = verify(&conn, true, &NoProgress).unwrap();
        assert!(v.missing.is_empty() && v.mismatched.is_empty());
    }

    #[test]
    fn cleans_previews_only() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("DWARF_RAW_x");
        std::fs::create_dir_all(d.join("Thumbnail")).unwrap();
        for f in [
            "a.fits",
            "Thumbnail/a.jpg",
            "stacked.jpg",
            "img.png",
            "shotsInfo.json",
        ] {
            std::fs::write(d.join(f), b"x").unwrap();
        }
        let (found, _) = clean_previews(tmp.path(), true).unwrap();
        assert_eq!(found.len(), 3);
        assert!(d.join("stacked.jpg").exists(), "dry run must not delete");
        clean_previews(tmp.path(), false).unwrap();
        assert!(d.join("a.fits").exists() && d.join("shotsInfo.json").exists());
        assert!(!d.join("stacked.jpg").exists() && !d.join("Thumbnail").exists());
    }

    #[test]
    fn finds_and_removes_duplicates() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join("a")).unwrap();
        std::fs::create_dir_all(repo.join("b")).unwrap();
        make_frame(
            &repo.join("a"),
            "x.fits",
            "Light",
            Some("M1"),
            "2024-10-01T21:00:00",
            60.0,
            None,
            1.0,
        );
        std::fs::copy(repo.join("a/x.fits"), repo.join("b/x.fits")).unwrap();
        let cfg = Config {
            repo: repo.clone(),
            ..Default::default()
        };
        let mut conn = db::open(&tmp.path().join("t.db")).unwrap();
        // Sync mode registers in place; the second copy is reported, not catalogued.
        let r = ingest::ingest_folder(
            &mut conn,
            &cfg,
            &repo,
            ingest::IngestOptions::IN_PLACE,
            &NoProgress,
        )
        .unwrap();
        assert_eq!((r.registered, r.duplicates.len()), (1, 1));
        // Force a duplicate row, as an older database might contain.
        let mut f = db::all_files(&conn, false).unwrap().remove(0);
        f.id = "dup".into();
        let other = if f.name.ends_with("a/x.fits") {
            "b/x.fits"
        } else {
            "a/x.fits"
        };
        f.name = util::normalize_path(&repo.join(other));
        f.insert(&conn).unwrap();
        assert_eq!(duplicate_groups(&conn).unwrap().len(), 1);
        let (n, _) = remove_duplicates(&mut conn).unwrap();
        assert_eq!(n, 1);
        assert_eq!(db::all_files(&conn, false).unwrap().len(), 1);
        assert_eq!(
            repo.join("a/x.fits").exists() as u8 + repo.join("b/x.fits").exists() as u8,
            1
        );
    }
}
