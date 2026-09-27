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

#[derive(Debug, Default)]
pub struct SeestarMigration {
    /// Old path -> new path of every file moved (or, in a dry run, to move).
    pub moved: Vec<(PathBuf, PathBuf)>,
    /// Catalogue rows updated.
    pub catalogued: usize,
    pub errors: Vec<(PathBuf, String)>,
}

/// Seestar files filed before the layout dropped the serial-number level:
/// `<top>/<object>/S50_1a2b3c4d/Seestar_S50/...` and names with
/// `-S50_1a2b3c4d-Seestar_S50-` in them. Returns (old, new) paths.
pub fn old_seestar_layout(repo: &Path) -> Vec<(PathBuf, PathBuf)> {
    let dirs = |p: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(p)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .collect()
            })
            .unwrap_or_default()
    };
    let name = |p: &Path| {
        p.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    };
    let mut out = Vec::new();
    for top in ["Light", "Stacked", "Calibrate"] {
        for object in dirs(&repo.join(top)) {
            for telescope in dirs(&object) {
                for camera in dirs(&telescope) {
                    let (tel, cam) = (name(&telescope), name(&camera));
                    if !ingest::is_seestar(&cam) || ingest::is_seestar(&tel) {
                        continue;
                    }
                    let old_part = format!("-{tel}-{cam}-");
                    let new_part = format!("-{cam}-");
                    for e in walkdir::WalkDir::new(&camera)
                        .into_iter()
                        .filter_map(|e| e.ok())
                    {
                        if !e.file_type().is_file() {
                            continue;
                        }
                        let rel = e.path().strip_prefix(&camera).unwrap_or(e.path());
                        let mut new = object.join(&cam).join(rel);
                        new.set_file_name(
                            e.file_name()
                                .to_string_lossy()
                                .replace(&old_part, &new_part),
                        );
                        out.push((e.path().to_path_buf(), new));
                    }
                }
            }
        }
    }
    out
}

/// Move Seestar files (and their previews and other companions) out of the
/// old serial-number folders into the current layout, updating the catalogue.
/// A file whose new name is already taken is left where it is.
pub fn migrate_seestar_layout(
    conn: &mut Connection,
    cfg: &Config,
    dry_run: bool,
    progress: &dyn Progress,
) -> Result<SeestarMigration> {
    let mut report = SeestarMigration::default();
    let plan = old_seestar_layout(&cfg.repo);
    let tx = conn.transaction()?;
    for (i, (old, new)) in plan.iter().enumerate() {
        progress.update(i + 1, plan.len(), "Moving Seestar files");
        if progress.cancelled() {
            break;
        }
        if new.exists() {
            report
                .errors
                .push((old.clone(), format!("{} already exists", new.display())));
            continue;
        }
        if dry_run {
            report.catalogued += tx.query_row(
                "SELECT COUNT(*) FROM fitsFile WHERE fitsFileName=?1",
                [util::normalize_path(old)],
                |r| r.get::<_, usize>(0),
            )?;
        } else {
            if let Err(e) = util::move_file(old, new) {
                report.errors.push((old.clone(), format!("{e:#}")));
                continue;
            }
            report.catalogued += tx.execute(
                "UPDATE fitsFile SET fitsFileName=?1 WHERE fitsFileName=?2",
                params![util::normalize_path(new), util::normalize_path(old)],
            )?;
        }
        report.moved.push((old.clone(), new.clone()));
    }
    tx.commit()?;
    if !dry_run {
        for top in ["Light", "Stacked", "Calibrate"] {
            remove_empty_dirs(&cfg.repo.join(top));
        }
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
                    util::copy_file(&src, &target)?;
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
        && !crate::util::is_stack_preview_name(&p.file_name().unwrap_or_default().to_string_lossy())
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
    use crate::ingest::tests::{make_frame, make_seestar};
    use crate::progress::NoProgress;

    #[test]
    fn seestar_layout_migration() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let old_dir = repo.join("Light/M_2/S50_1a2b3c4d/Seestar_S50/20240801");
        std::fs::create_dir_all(&old_dir).unwrap();
        let name = "M_2-S50_1a2b3c4d-Seestar_S50-IRCUT-20240801220000-10.0s-1x1-t-10.0";
        let p = make_frame(
            &old_dir,
            &format!("{name}.fits"),
            "Light",
            Some("M 2"),
            "2024-08-01T22:00:00",
            10.0,
            Some("IRCUT"),
            1.0,
        );
        make_seestar(&p, None);
        std::fs::write(old_dir.join(format!("{name}.jpg")), b"jpg").unwrap();
        let cfg = Config {
            repo: repo.clone(),
            source: tmp.path().join("incoming"),
            ..Default::default()
        };
        let mut conn = db::open(&tmp.path().join("t.db")).unwrap();
        ingest::ingest_folder(
            &mut conn,
            &cfg,
            &repo,
            ingest::IngestOptions::IN_PLACE,
            &NoProgress,
        )
        .unwrap();

        let dry = migrate_seestar_layout(&mut conn, &cfg, true, &NoProgress).unwrap();
        assert_eq!((dry.moved.len(), dry.catalogued), (2, 1), "{dry:?}");
        assert!(p.exists());

        let r = migrate_seestar_layout(&mut conn, &cfg, false, &NoProgress).unwrap();
        assert_eq!(
            (r.moved.len(), r.catalogued, r.errors.len()),
            (2, 1, 0),
            "{r:?}"
        );
        let new_dir = repo.join("Light/M_2/Seestar_S50/20240801");
        let new = new_dir.join("M_2-Seestar_S50-IRCUT-20240801220000-10.0s-1x1-t-10.0.fits");
        assert!(new.exists());
        assert!(new_dir
            .join("M_2-Seestar_S50-IRCUT-20240801220000-10.0s-1x1-t-10.0.jpg")
            .exists());
        assert!(!repo.join("Light/M_2/S50_1a2b3c4d").exists());
        let files = db::all_files(&conn, false).unwrap();
        assert_eq!(files[0].name, util::normalize_path(&new));
        assert!(old_seestar_layout(&repo).is_empty());
    }

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
        assert_eq!(found.len(), 2, "stacked previews are kept");
        assert!(d.join("img.png").exists(), "dry run must not delete");
        clean_previews(tmp.path(), false).unwrap();
        assert!(d.join("a.fits").exists() && d.join("shotsInfo.json").exists());
        assert!(d.join("stacked.jpg").exists(), "stacked preview kept");
        assert!(!d.join("img.png").exists() && !d.join("Thumbnail").exists());
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
