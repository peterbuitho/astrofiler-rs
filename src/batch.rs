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
use std::collections::{BTreeMap, HashMap};
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
    let mappings = db::mappings(&tx)?;
    // Folders the files left, and where they went.
    let mut moved_dirs: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();
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
            match edit_file(cfg, &path, &card, value, opts, &mappings) {
                Ok(edited) => {
                    if edited.path != path {
                        report.moved += 1;
                        if let (Some(old), Some(new)) = (path.parent(), edited.path.parent()) {
                            if old != new {
                                moved_dirs.insert(old.to_path_buf(), new.to_path_buf());
                            }
                        }
                    }
                    new_path = edited.path;
                    if let Some(h) = edited.hash {
                        new_hash = Some(h);
                    }
                    // The header is rewritten, so the catalogue follows.
                    if let Some(e) = edited.not_moved {
                        report.errors.push((file.name.clone(), e));
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
    for (old, new) in &moved_dirs {
        move_sidecars(old, new);
    }
    Ok(report)
}

/// Move the companion files (session info, stack previews) of a folder whose
/// frames have all been re-filed to where the frames went.
fn move_sidecars(old: &Path, new: &Path) {
    let files: Vec<PathBuf> = match std::fs::read_dir(old) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect(),
        Err(_) => return,
    };
    if files
        .iter()
        .any(|p| util::is_supported_file(p) && !util::is_sidecar(p))
    {
        return;
    }
    for p in files.iter().filter(|p| util::is_sidecar(p)) {
        let Some(name) = p.file_name() else {
            continue;
        };
        if let Err(e) = util::move_file(p, &util::unique_path(&new.join(name))) {
            log::warn!("Not moved: {} ({e:#})", p.display());
        }
    }
}

struct Edited {
    path: PathBuf,
    /// New checksum, when the header was rewritten.
    hash: Option<String>,
    /// Why the file stayed where it was after its header was rewritten.
    not_moved: Option<String>,
}

/// Rewrite the header and/or move one file. Nothing is changed when the
/// file's new place can't be worked out.
fn edit_file(
    cfg: &Config,
    path: &Path,
    card: &str,
    value: &str,
    opts: EditOptions,
    mappings: &[db::Mapping],
) -> Result<Edited> {
    let mut header = fits::read_primary_header(path)?;
    header.set(card, Value::Str(value.to_string()));
    if opts.update_headers && fits::is_gzip(path) {
        bail!("can't rewrite the header of a gzip-compressed file");
    }
    let mut target = None;
    if opts.refile && path.starts_with(&cfg.repo) {
        // File by the header as ingest sees it (e.g. DWARF files carry no
        // IMAGETYP); the edited value wins over the mappings.
        let mut filed = header.clone();
        ingest::normalize_header(&mut filed, path, mappings)?;
        filed.set(card, Value::Str(value.to_string()));
        let (mut name, dir) = ingest::destination(&filed, cfg)?;
        if ingest::is_stacked(&filed) {
            name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
        }
        target = Some(dir.join(name)).filter(|t| t != path);
    }
    let mut edited = Edited {
        path: path.to_path_buf(),
        hash: None,
        not_moved: None,
    };
    if opts.update_headers {
        fits::rewrite_primary_header(path, &header)?;
        edited.hash = Some(util::sha256_file(path)?);
    }
    if let Some(target) = target {
        let target = util::unique_path(&target);
        match util::move_file(path, &target) {
            Ok(()) => edited.path = target,
            Err(e) if opts.update_headers => edited.not_moved = Some(format!("{e:#}")),
            Err(e) => return Err(e),
        }
    }
    Ok(edited)
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
        remove_empty_dirs(&cfg.repo.join("Stacked"));
    }
    Ok(report)
}

#[derive(Debug, Default)]
pub struct LayoutMigration {
    /// Old path -> new path of every file moved (or, in a dry run, to move).
    pub moved: Vec<(PathBuf, PathBuf)>,
    /// Catalogue rows updated.
    pub catalogued: usize,
    pub errors: Vec<(PathBuf, String)>,
}

/// The name a stacked result is kept under: its original file name, with
/// converted XISF files ending in .fits and gzip files unpacked.
fn kept_name(original: &str) -> Option<String> {
    let name = original.rsplit(['/', '\\']).next()?;
    let lower = name.to_lowercase();
    if lower.ends_with(".zip") || name.is_empty() {
        return None; // the name inside the archive wasn't recorded
    }
    Some(if lower.ends_with(".xisf") {
        format!("{}.fits", &name[..name.len() - 5])
    } else if lower.ends_with(".gz") {
        name[..name.len() - 3].to_string()
    } else {
        name.to_string()
    })
}

/// Files filed under an older repository layout, with where they belong now
/// (companions such as previews move with their frames):
/// - Seestar files under `<object>/S50_1a2b3c4d/Seestar_S50/`, with the serial
///   number in their names too;
/// - object folders without the object's common name (`M_76` becomes
///   `M_76_Barbell_Nebula`), or with a common name that has since changed;
/// - stacked results renamed by earlier versions (`Stacked-M_2-…fits`), which
///   get their original name back.
pub fn layout_plan(conn: &Connection, cfg: &Config) -> Result<Vec<(PathBuf, PathBuf)>> {
    let repo = &cfg.repo;
    let prefix = format!("{}/", util::normalize_path(repo));
    let files = db::files_where(conn, "substr(fitsFileName, 1, length(?1)) = ?1", &[&prefix])?;
    // Object of each object folder, and new names of stacked results and of
    // the companions that share their name.
    let mut objects: HashMap<PathBuf, String> = HashMap::new();
    let mut renames: HashMap<PathBuf, String> = HashMap::new();
    let mut stem_renames: HashMap<(PathBuf, String), String> = HashMap::new();
    for f in &files {
        let path = PathBuf::from(&f.name);
        let Ok(rel) = path.strip_prefix(repo) else {
            continue;
        };
        let mut comps = rel.components();
        if let (Some(top), Some(object_dir), Some(object)) = (comps.next(), comps.next(), &f.object)
        {
            if matches!(top.as_os_str().to_str(), Some("Light" | "Stacked")) {
                objects
                    .entry(repo.join(top).join(object_dir))
                    .or_insert_with(|| object.clone());
            }
        }
        if !f.stacked {
            continue;
        }
        let Some(kept) = f.original.as_deref().and_then(kept_name) else {
            continue;
        };
        let (Some(dir), Some(old_stem), Some(new_stem)) = (
            path.parent(),
            path.file_stem(),
            Path::new(&kept).file_stem(),
        ) else {
            continue;
        };
        stem_renames.insert(
            (dir.to_path_buf(), old_stem.to_string_lossy().into_owned()),
            new_stem.to_string_lossy().into_owned(),
        );
        renames.insert(path.clone(), kept);
    }

    let mut out = Vec::new();
    for top in ["Light", "Stacked", "Calibrate"] {
        let top_dir = repo.join(top);
        let entries = walkdir::WalkDir::new(&top_dir)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file());
        for e in entries {
            let old = e.path().to_path_buf();
            let Ok(rel) = old.strip_prefix(&top_dir) else {
                continue;
            };
            let mut parts: Vec<String> = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            let Some(mut name) = parts.pop() else {
                continue;
            };
            if parts.is_empty() {
                continue;
            }
            let object_dir = top_dir.join(&parts[0]);
            let stem_key = |p: &Path| -> Option<(PathBuf, String)> {
                Some((
                    p.parent()?.to_path_buf(),
                    p.file_stem()?.to_string_lossy().into_owned(),
                ))
            };
            let renamed = if let Some(kept) = renames.get(&old) {
                name = kept.clone();
                true
            } else if let Some(stem) = stem_key(&old).and_then(|k| stem_renames.get(&k)) {
                name = match old.extension() {
                    Some(ext) => format!("{stem}.{}", ext.to_string_lossy()),
                    None => stem.clone(),
                };
                true
            } else {
                false
            };
            // Seestar: <object>/<serial>/<Seestar model>/... -> <object>/<model>/...
            if parts.len() >= 3 && ingest::is_seestar(&parts[2]) && !ingest::is_seestar(&parts[1]) {
                let (serial, model) = (parts[1].clone(), parts[2].clone());
                if !renamed {
                    name = name.replace(&format!("-{serial}-{model}-"), &format!("-{model}-"));
                }
                parts.remove(1);
            }
            if top != "Calibrate" {
                let object = objects
                    .get(&object_dir)
                    .cloned()
                    .unwrap_or_else(|| parts[0].clone());
                let base = util::sanitize(&object);
                // Only add or update the common name; never move files to a
                // different object.
                if parts[0] == base || parts[0].starts_with(&format!("{base}_")) {
                    parts[0] = crate::names::object_folder(&object, &cfg.object_names);
                }
            }
            let new = top_dir.join(parts.iter().collect::<PathBuf>()).join(name);
            if new != old {
                out.push((old, new));
            }
        }
    }
    Ok(out)
}

/// Move files filed under an older layout (see [`layout_plan`]) to where
/// they belong now, updating the catalogue. A file whose new name is already
/// taken is left where it is.
pub fn migrate_layout(
    conn: &mut Connection,
    cfg: &Config,
    dry_run: bool,
    progress: &dyn Progress,
) -> Result<LayoutMigration> {
    let mut report = LayoutMigration::default();
    let plan = layout_plan(conn, cfg)?;
    let tx = conn.transaction()?;
    for (i, (old, new)) in plan.iter().enumerate() {
        progress.update(i + 1, plan.len(), "Moving files to the current layout");
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
    let first = files
        .first()
        .map(|f| PathBuf::from(&f.name))
        .unwrap_or_default();
    let results: Vec<(String, Result<PathBuf>)> = util::with_io_pool(&[&first, dest], || {
        files
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
            .collect()
    });
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
pub struct FillReport {
    pub filled: usize,
    /// Files that turned out to be copies of other catalogued files.
    pub duplicates: usize,
    pub errors: Vec<(String, String)>,
}

impl FillReport {
    pub fn summary(&self) -> String {
        let mut s = format!("{} checksums filled in", self.filled);
        if self.duplicates > 0 {
            s.push_str(&format!(
                ", {} files are copies of others (see Duplicates)",
                self.duplicates
            ));
        }
        if !self.errors.is_empty() {
            s.push_str(&format!(
                ", {} could not be read (see Log)",
                self.errors.len()
            ));
        }
        s
    }
}

/// Compute the checksums a quick move skipped. Until then those files
/// can't be recognised as duplicates, so any copies found are reported.
pub fn fill_checksums(conn: &mut Connection, progress: &dyn Progress) -> Result<FillReport> {
    let files = db::files_where(
        conn,
        "fitsFileHash IS NULL AND COALESCE(fitsFileSoftDelete,0)=0",
        &[],
    )?;
    let done = AtomicUsize::new(0);
    let total = files.len();
    let first = files
        .first()
        .map(|f| PathBuf::from(&f.name))
        .unwrap_or_default();
    let hashes: Vec<(&FitsFile, Option<Result<String>>)> = util::with_io_pool(&[&first], || {
        files
            .par_iter()
            .map(|f| {
                if progress.cancelled() {
                    return (f, None);
                }
                let h = util::sha256_file(Path::new(&f.name));
                let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                progress.update(d, total, "Filling in checksums");
                (f, Some(h))
            })
            .collect()
    });
    let mut r = FillReport::default();
    let tx = conn.transaction()?;
    for (f, h) in hashes {
        match h {
            Some(Ok(h)) => {
                tx.execute(
                    "UPDATE fitsFile SET fitsFileHash=?1 WHERE fitsFileId=?2",
                    params![h, f.id],
                )?;
                let copies: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM fitsFile WHERE fitsFileHash=?1 \
                     AND COALESCE(fitsFileSoftDelete,0)=0",
                    [&h],
                    |row| row.get(0),
                )?;
                if copies > 1 {
                    r.duplicates += 1;
                }
                r.filled += 1;
            }
            Some(Err(e)) => {
                log::warn!("{}: {e:#}", f.name);
                r.errors.push((f.name.clone(), format!("{e:#}")));
            }
            None => {}
        }
    }
    tx.commit()?;
    Ok(r)
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
    let first = files
        .first()
        .map(|f| PathBuf::from(&f.name))
        .unwrap_or_default();
    let states: Vec<(FitsFile, u8)> = util::with_io_pool(&[&first], || {
        files
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
                let name = p.file_name().unwrap_or_default().to_string_lossy();
                progress.update(d, total, &format!("Verifying {name}"));
                (f, state)
            })
            .collect()
    });
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
    fn layout_migration() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let cfg = Config {
            repo: repo.clone(),
            source: tmp.path().join("incoming"),
            ..Default::default()
        };
        let mut conn = db::open(&tmp.path().join("t.db")).unwrap();

        // A Seestar sub filed by 0.2.0-: serial-number folder, no common name.
        let old_dir = repo.join("Light/M_76/S50_1a2b3c4d/Seestar_S50/20240801");
        std::fs::create_dir_all(&old_dir).unwrap();
        let name = "M_76-S50_1a2b3c4d-Seestar_S50-IRCUT-20240801220000-10.0s-1x1-t-10.0";
        let p = make_frame(
            &old_dir,
            &format!("{name}.fits"),
            "Light",
            Some("M 76"),
            "2024-08-01T22:00:00",
            10.0,
            Some("IRCUT"),
            1.0,
        );
        make_seestar(&p, None);
        std::fs::write(old_dir.join(format!("{name}.jpg")), b"jpg").unwrap();
        ingest::ingest_folder(
            &mut conn,
            &cfg,
            &repo,
            ingest::IngestOptions::IN_PLACE,
            &NoProgress,
        )
        .unwrap();

        // A stacked result renamed by 0.2.0, loaded from the telescope's drive.
        let drive = tmp.path().join("MyWorks/M 2");
        std::fs::create_dir_all(&drive).unwrap();
        let original = "Stacked_2_M 2_10.0s_IRCUT_20240801-221000";
        let st = make_frame(
            &drive,
            &format!("{original}.fit"),
            "Light",
            Some("M 2"),
            "2024-08-01T22:10:00",
            10.0,
            Some("IRCUT"),
            9.0,
        );
        make_seestar(&st, Some(2));
        ingest::ingest_folder(
            &mut conn,
            &cfg,
            &drive,
            ingest::IngestOptions::COPY,
            &NoProgress,
        )
        .unwrap();
        let stacked_dir = repo.join("Stacked/M_2/Seestar_S50");
        let kept = stacked_dir.join(format!("{original}.fit"));
        assert!(kept.exists(), "stacked results keep their name");
        let renamed = "Stacked-M_2-Seestar_S50-IRCUT-20240801221000-2x10.0s";
        let renamed_fits = stacked_dir.join(format!("{renamed}.fits"));
        std::fs::rename(&kept, &renamed_fits).unwrap();
        std::fs::write(stacked_dir.join(format!("{renamed}.jpg")), b"jpg").unwrap();
        conn.execute(
            "UPDATE fitsFile SET fitsFileName=?1 WHERE fitsFileName=?2",
            params![
                util::normalize_path(&renamed_fits),
                util::normalize_path(&kept)
            ],
        )
        .unwrap();

        let dry = migrate_layout(&mut conn, &cfg, true, &NoProgress).unwrap();
        assert_eq!((dry.moved.len(), dry.catalogued), (4, 2), "{dry:?}");
        assert!(p.exists());

        let r = migrate_layout(&mut conn, &cfg, false, &NoProgress).unwrap();
        assert_eq!(
            (r.moved.len(), r.catalogued, r.errors.len()),
            (4, 2, 0),
            "{r:?}"
        );
        let new_dir = repo.join("Light/M_76_Barbell_Nebula/Seestar_S50/20240801");
        let new = new_dir.join("M_76-Seestar_S50-IRCUT-20240801220000-10.0s-1x1-t-10.0.fits");
        assert!(new.exists());
        assert!(new_dir
            .join("M_76-Seestar_S50-IRCUT-20240801220000-10.0s-1x1-t-10.0.jpg")
            .exists());
        assert!(!repo.join("Light/M_76").exists());
        assert!(kept.exists());
        assert!(stacked_dir.join(format!("{original}.jpg")).exists());
        let names: Vec<String> = db::all_files(&conn, false)
            .unwrap()
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert!(names.contains(&util::normalize_path(&new)), "{names:?}");
        assert!(names.contains(&util::normalize_path(&kept)), "{names:?}");
        assert!(layout_plan(&conn, &cfg).unwrap().is_empty());
    }

    #[test]
    fn merge_refiles_files_whose_header_needs_normalising() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config {
            repo: tmp.path().join("repo"),
            ..Default::default()
        };
        // A filed DWARF light: no IMAGETYP, camera in CAMERA.
        let dir = cfg
            .repo
            .join("Light/NGC281_-_Pacman_Nebula/DWARF_3/TELE/20261002");
        std::fs::create_dir_all(&dir).unwrap();
        let p = make_frame(
            &dir,
            "1.fits",
            "Light",
            Some("NGC281 - Pacman Nebula"),
            "2026-10-02T20:24:36",
            30.0,
            Some("Duo-Band"),
            1.0,
        );
        let mut h = fits::read_primary_header(&p).unwrap();
        h.remove("IMAGETYP");
        h.set("TELESCOP", Value::Str("DWARF 3".into()));
        h.set("INSTRUME", Value::Str("DWARF 3".into()));
        h.set("CAMERA", Value::Str("TELE".into()));
        fits::rewrite_primary_header(&p, &h).unwrap();
        std::fs::write(dir.join("shotsInfo.json"), "{}").unwrap();
        let mut conn = db::open(&tmp.path().join("t.db")).unwrap();
        ingest::ingest_folder(
            &mut conn,
            &cfg,
            &cfg.repo,
            crate::ingest::IngestOptions::IN_PLACE,
            &NoProgress,
        )
        .unwrap();
        let r = merge_objects(
            &mut conn,
            &cfg,
            "NGC281 - Pacman Nebula",
            "NGC 281",
            EditOptions {
                update_headers: true,
                refile: true,
            },
            &NoProgress,
        )
        .unwrap();
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!((r.updated, r.moved), (1, 1));
        let files = db::all_files(&conn, false).unwrap();
        let f = &files[0];
        assert_eq!(f.object.as_deref(), Some("NGC 281"));
        let new_dir = cfg
            .repo
            .join("Light/NGC_281_Pacman_Nebula/DWARF_3/TELE/20261002");
        assert_eq!(Path::new(&f.name).parent(), Some(new_dir.as_path()));
        assert_eq!(
            util::sha256_file(Path::new(&f.name)).unwrap(),
            f.hash.clone().unwrap()
        );
        // The header keeps what the telescope wrote, apart from the edit.
        let h = fits::read_primary_header(Path::new(&f.name)).unwrap();
        assert_eq!(h.get_str("OBJECT").as_deref(), Some("NGC 281"));
        assert!(!h.contains("IMAGETYP"));
        assert!(new_dir.join("shotsInfo.json").exists());
        assert!(!cfg.repo.join("Light/NGC281_-_Pacman_Nebula").exists());
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
            assert!(
                f.name.contains("/Light/M_31_Andromeda_Galaxy/"),
                "{}",
                f.name
            );
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
