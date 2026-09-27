//! Repository ingest: scan a folder, normalise headers, rename files to a
//! descriptive name, file them into the repository tree and catalogue them.
//!
//! Port of `FileProcessor.registerFitsImage`, restructured for speed: header
//! parsing, header fixes and SHA-256 hashing run in parallel across all cores,
//! and database inserts happen in a single transaction.

use crate::config::Config;
use crate::db::{self, FitsFile, Mapping};
use crate::fits::{self, Header, Value};
use crate::masters;
use crate::progress::Progress;
use crate::util::{self, sanitize, FrameKind};
use anyhow::{anyhow, bail, Context, Result};
use rayon::prelude::*;
use rusqlite::Connection;
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use walkdir::WalkDir;

/// What happens to files being loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Rename and move into the repository (the original's behaviour).
    Move,
    /// Put renamed copies into the repository and leave the originals untouched.
    Copy,
    /// Catalogue files where they are (used for the repository itself).
    InPlace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IngestOptions {
    pub placement: Placement,
    /// Work out where everything would go without touching files or catalogue.
    pub dry_run: bool,
}

impl IngestOptions {
    pub const MOVE: Self = IngestOptions {
        placement: Placement::Move,
        dry_run: false,
    };
    pub const COPY: Self = IngestOptions {
        placement: Placement::Copy,
        dry_run: false,
    };
    pub const IN_PLACE: Self = IngestOptions {
        placement: Placement::InPlace,
        dry_run: false,
    };
}

#[derive(Debug, Default)]
pub struct IngestReport {
    pub registered: usize,
    pub masters: usize,
    /// Files deliberately not imported (e.g. DWARF `failed_*` frames).
    pub skipped: usize,
    /// Files whose content is already in the catalogue (e.g. loaded before).
    pub already_catalogued: usize,
    pub duplicates: Vec<(PathBuf, String)>,
    pub errors: Vec<(PathBuf, String)>,
    pub new_ids: Vec<String>,
    /// Input path -> final (or, in a dry run, planned) path of each filed file.
    pub placed: Vec<(PathBuf, PathBuf)>,
    pub dry_run: bool,
}

impl IngestReport {
    pub fn summary(&self) -> String {
        let verb = if self.dry_run {
            "would be filed"
        } else {
            "registered"
        };
        format!(
            "{} {verb}, {} masters, {} already in catalogue, {} duplicate copies, {} rejected frames skipped, {} errors",
            self.registered,
            self.masters,
            self.already_catalogued,
            self.duplicates.len() - self.already_catalogued,
            self.skipped,
            self.errors.len()
        )
    }

    /// Write the input -> destination plan as CSV.
    pub fn write_plan_csv(&self, path: &Path) -> Result<()> {
        use std::io::Write;
        let q = |p: &Path| format!("\"{}\"", p.display().to_string().replace('"', "\"\""));
        let mut w = std::io::BufWriter::new(std::fs::File::create(path)?);
        writeln!(w, "status,source,destination")?;
        for (a, b) in &self.placed {
            writeln!(w, "file,{},{}", q(a), q(b))?;
        }
        for (a, b) in &self.duplicates {
            writeln!(w, "duplicate,{},\"{}\"", q(a), b.replace('"', "\"\""))?;
        }
        for (a, e) in &self.errors {
            writeln!(w, "error,{},\"{}\"", q(a), e.replace('"', "\"\""))?;
        }
        Ok(())
    }
}

/// Recursively collect importable files under `dir`.
/// Folders the repository manages itself.
pub const MANAGED_DIRS: &[&str] = &["Light", "Calibrate", "Stacked", "Masters", "Archive"];

pub fn collect_files(dir: &Path, exclude: &[PathBuf]) -> Vec<PathBuf> {
    WalkDir::new(dir)
        .follow_links(true)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !(exclude.iter().any(|x| e.path() == x) || name.starts_with(".astrofiler-work"))
        })
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| util::is_supported_file(p) && !p.to_string_lossy().ends_with(".astrofiler-tmp"))
        .collect()
}

/// Load every supported file under `source` (e.g. an incoming folder or an
/// existing archive on a NAS).
pub fn ingest_folder(
    conn: &mut Connection,
    cfg: &Config,
    source: &Path,
    opts: IngestOptions,
    progress: &dyn Progress,
) -> Result<IngestReport> {
    if !source.is_dir() {
        bail!("source folder {} does not exist", source.display());
    }
    progress.update(0, 0, "Scanning for files...");
    // Never re-read the repository's own organised folders as input
    // (the repository may live inside the folder being loaded).
    let excluded: Vec<PathBuf> = if opts.placement == Placement::InPlace {
        vec![]
    } else if cfg.repo.starts_with(source) && !paths_equal(source, &cfg.repo) {
        vec![cfg.repo.clone()]
    } else {
        MANAGED_DIRS.iter().map(|d| cfg.repo.join(d)).collect()
    };
    let files = collect_files(source, &excluded);
    log::info!(
        "Found {} candidate files in {}",
        files.len(),
        source.display()
    );
    ingest_files(conn, cfg, files, opts, progress)
}

/// Remove scratch folders left behind by an interrupted load (older than
/// ten minutes, so a load running in another window is left alone).
fn remove_stale_work_dirs(repo: &Path) {
    let Ok(rd) = std::fs::read_dir(repo) else {
        return;
    };
    for e in rd.flatten() {
        let stale = e
            .file_name()
            .to_string_lossy()
            .starts_with(".astrofiler-work-")
            && e.metadata().and_then(|m| m.modified()).is_ok_and(|t| {
                t.elapsed()
                    .is_ok_and(|age| age > std::time::Duration::from_secs(600))
            });
        if stale {
            std::fs::remove_dir_all(e.path()).ok();
        }
    }
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

/// A FITS file ready to register. `temp` marks files we produced ourselves
/// (unzipped, decompressed, converted from XISF) that can always be moved.
struct Staged {
    input: PathBuf,
    path: PathBuf,
    temp: bool,
}

enum Prepared {
    Frame {
        staged: Staged,
        header: Header,
        new_name: String,
        dest_dir: PathBuf,
        hash: String,
        rewrite: bool,
    },
    Master {
        staged: Staged,
        header: Header,
    },
}

pub fn ingest_files(
    conn: &mut Connection,
    cfg: &Config,
    files: Vec<PathBuf>,
    opts: IngestOptions,
    progress: &dyn Progress,
) -> Result<IngestReport> {
    let mut report = IngestReport {
        dry_run: opts.dry_run,
        ..Default::default()
    };
    let before = files.len();
    let files: Vec<PathBuf> = files
        .into_iter()
        .filter(|p| !crate::telescope::skip_file(p))
        .collect();
    report.skipped = before - files.len();
    let total = files.len();
    // Scratch space for converted files, so sources are never written to
    // when copying or doing a dry run.
    remove_stale_work_dirs(&cfg.repo);
    let work = cfg.repo.join(format!(
        ".astrofiler-work-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let result = ingest_inner(conn, cfg, files, opts, progress, &work, &mut report, total);
    if work.exists() {
        std::fs::remove_dir_all(&work).ok();
    }
    result?;
    progress.update(total, total, "Done");
    log::info!("Ingest finished: {}", report.summary());
    Ok(report)
}

/// Files are processed in batches: unpack, hash and file one batch, commit it,
/// then move on. This keeps scratch space small (only one batch of converted
/// XISF files exists at a time), makes progress visible to other readers of
/// the catalogue, and means an interrupted load loses at most one batch.
const BATCH: usize = 64;

struct FileState {
    seen: HashSet<String>,
    planned: HashSet<PathBuf>,
}

#[allow(clippy::too_many_arguments)]
fn ingest_inner(
    conn: &mut Connection,
    cfg: &Config,
    files: Vec<PathBuf>,
    opts: IngestOptions,
    progress: &dyn Progress,
    work: &Path,
    report: &mut IngestReport,
    total: usize,
) -> Result<()> {
    let mappings = db::mappings(conn)?;
    let mut state = FileState {
        seen: HashSet::new(),
        planned: HashSet::new(),
    };
    let mut done = 0usize;
    for chunk in files.chunks(BATCH) {
        if progress.cancelled() {
            bail!("cancelled");
        }
        let label = |p: &Path| {
            p.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
        };
        progress.update(done, total, &format!("Reading {}", label(&chunk[0])));

        // Unpack containers (zip, xisf, gz) into plain FITS files.
        let unpacked: Vec<(PathBuf, Result<Vec<Staged>>)> = chunk
            .par_iter()
            .map(|p| (p.clone(), unpack(p, cfg, opts, work)))
            .collect();
        let mut staged: Vec<Staged> = Vec::new();
        for (input, r) in unpacked {
            match r {
                Ok(list) => staged.extend(list),
                Err(e) => report.errors.push((input, format!("{e:#}"))),
            }
        }

        // Parse, normalise and hash in parallel.
        let prepared: Vec<(PathBuf, Result<Prepared>)> = staged
            .into_par_iter()
            .map(|st| (st.input.clone(), prepare(st, cfg, &mappings)))
            .collect();

        // File and catalogue this batch in one transaction.
        progress.update(done, total, &format!("Filing {}", label(&chunk[0])));
        if !opts.dry_run {
            conn.execute_batch("BEGIN IMMEDIATE")?;
        }
        let mut result = Ok(());
        for (input, prep) in prepared {
            match prep {
                Ok(p) => {
                    if let Err(e) = file_prepared(conn, cfg, opts, p, report, &mut state) {
                        result = Err(e);
                        break;
                    }
                }
                Err(e) => {
                    log::warn!("{}: {e:#}", input.display());
                    report.errors.push((input, format!("{e:#}")));
                }
            }
        }
        if !opts.dry_run {
            // Keep whatever was filed, even if the batch stopped early, so
            // the catalogue always matches the files on disk.
            conn.execute_batch("COMMIT")?;
        }
        result?;
        if work.exists() {
            std::fs::remove_dir_all(work).ok();
        }
        done += chunk.len();
        progress.update(done, total, &format!("{} of {total} files processed", done));
    }
    Ok(())
}

fn file_prepared(
    conn: &Connection,
    cfg: &Config,
    opts: IngestOptions,
    prep: Prepared,
    report: &mut IngestReport,
    state: &mut FileState,
) -> Result<()> {
    match prep {
        Prepared::Master { staged, header } => {
            let placement = if staged.temp {
                Placement::Move
            } else {
                opts.placement
            };
            if opts.dry_run {
                let dest = match placement {
                    Placement::InPlace => staged.path.clone(),
                    _ => cfg
                        .masters_dir()
                        .join(staged.input.file_name().unwrap_or_default()),
                };
                report.masters += 1;
                report.placed.push((staged.input, dest));
                return Ok(());
            }
            match masters::register_master(conn, cfg, &staged.path, &header, placement) {
                Ok((_, final_path)) => {
                    report.masters += 1;
                    report.placed.push((staged.input, final_path));
                }
                Err(e) => report.errors.push((staged.input, format!("{e:#}"))),
            }
        }
        Prepared::Frame {
            staged,
            header,
            new_name,
            dest_dir,
            hash,
            rewrite,
        } => {
            if let Some(existing) = db::hash_exists(conn, &hash)? {
                // Already catalogued. When syncing in place this is the same file.
                if !paths_equal(Path::new(&existing), &staged.path) {
                    report.already_catalogued += 1;
                    report.duplicates.push((staged.input, existing));
                }
                return Ok(());
            }
            if !state.seen.insert(hash.clone()) {
                report
                    .duplicates
                    .push((staged.input, "another file in this batch".into()));
                return Ok(());
            }
            let placement = if staged.temp && opts.placement == Placement::Copy {
                Placement::Move
            } else {
                opts.placement
            };
            let target = dest_dir.join(&new_name);
            // A previous, interrupted run may already have put this exact
            // file in place: adopt it instead of making a `_001` copy.
            let adopt = placement != Placement::InPlace
                && target != staged.path
                && target.exists()
                && util::sha256_file(&target).is_ok_and(|h| h == hash);
            if opts.dry_run {
                let dest = match placement {
                    Placement::InPlace => staged.path.clone(),
                    _ if adopt || target == staged.path => target,
                    _ => {
                        // Mirror unique_path() against files planned in this run.
                        let stem = Path::new(&new_name)
                            .file_stem()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_string();
                        let mut d = util::unique_path(&target);
                        let mut k = 1;
                        while state.planned.contains(&d) {
                            d = dest_dir.join(format!("{stem}_{k:03}.fits"));
                            k += 1;
                        }
                        state.planned.insert(d.clone());
                        d
                    }
                };
                report.registered += 1;
                report.placed.push((staged.input, dest));
                return Ok(());
            }
            let final_path = if placement == Placement::InPlace || target == staged.path {
                staged.path.clone()
            } else if adopt {
                if placement == Placement::Move || staged.temp {
                    std::fs::remove_file(&staged.path).ok();
                }
                target
            } else {
                let dest = util::unique_path(&target);
                let r = if placement == Placement::Move {
                    util::move_file(&staged.path, &dest)
                } else {
                    std::fs::create_dir_all(&dest_dir)
                        .map_err(anyhow::Error::from)
                        .and_then(|_| {
                            std::fs::copy(&staged.path, &dest)?;
                            Ok(())
                        })
                };
                if let Err(e) = r {
                    report
                        .errors
                        .push((staged.input, format!("filing into repository: {e:#}")));
                    return Ok(());
                }
                if rewrite {
                    if let Err(e) = fits::rewrite_primary_header(&dest, &header) {
                        log::warn!(
                            "could not save modified header for {}: {e:#}",
                            dest.display()
                        );
                    }
                }
                dest
            };
            let record = file_record(&header, &final_path, hash);
            record.insert(conn)?;
            report.new_ids.push(record.id);
            report.placed.push((staged.input, final_path));
            report.registered += 1;
        }
    }
    Ok(())
}

/// Turn one input file into the FITS file(s) to register.
fn unpack(path: &Path, cfg: &Config, opts: IngestOptions, work: &Path) -> Result<Vec<Staged>> {
    let lower = path.to_string_lossy().to_lowercase();
    let moving = opts.placement == Placement::Move && !opts.dry_run;
    // Where intermediate files go: next to the source when we own it (moving
    // or syncing in place), otherwise into the scratch folder.
    let scratch = |name: &std::ffi::OsStr| -> Result<(PathBuf, bool)> {
        if moving || (opts.placement == Placement::InPlace && !opts.dry_run) {
            Ok((util::unique_path(&path.with_file_name(name)), false))
        } else {
            let dir = work.join(uuid::Uuid::new_v4().simple().to_string());
            std::fs::create_dir_all(&dir)?;
            Ok((dir.join(name), true))
        }
    };
    let staged = |p: PathBuf, temp: bool| Staged {
        input: path.to_path_buf(),
        path: p,
        temp,
    };

    if lower.ends_with(".zip") {
        let mut archive = zip::ZipArchive::new(std::fs::File::open(path)?)?;
        let mut out = Vec::new();
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i)?;
            let Some(name) = entry.enclosed_name() else {
                continue;
            };
            if !util::is_fits_name(&name) {
                continue;
            }
            let (dest, temp) = scratch(name.file_name().unwrap_or_default())?;
            std::io::copy(&mut entry, &mut std::fs::File::create(&dest)?)?;
            out.push(staged(dest, temp));
        }
        if moving {
            std::fs::remove_file(path).ok();
        }
        return Ok(out);
    }
    if lower.ends_with(".xisf") {
        let sibling = path.with_extension("fits");
        if opts.placement == Placement::InPlace && sibling.exists() {
            return Ok(vec![]); // already converted in an earlier sync
        }
        let fname = sibling.file_name().unwrap_or_default().to_os_string();
        let (fits_path, temp) = scratch(&fname)?;
        crate::xisf::convert_to_fits(path, &fits_path)
            .with_context(|| format!("converting {}", path.display()))?;
        if moving {
            // Keep the original XISF, out of the way.
            let archive = util::unique_path(
                &cfg.repo
                    .join("Archive")
                    .join("XISF")
                    .join(path.file_name().unwrap()),
            );
            util::move_file(path, &archive)?;
        }
        return Ok(vec![staged(fits_path, temp)]);
    }
    if lower.ends_with(".gz") && opts.placement != Placement::InPlace {
        let name = path.file_stem().unwrap_or_default().to_os_string();
        let (out, temp) = scratch(&name)?;
        let mut dec = flate2::read::GzDecoder::new(std::fs::File::open(path)?);
        std::io::copy(&mut dec, &mut std::fs::File::create(&out)?)?;
        if moving {
            std::fs::remove_file(path)?;
        }
        return Ok(vec![staged(out, temp)]);
    }
    Ok(vec![staged(path.to_path_buf(), false)])
}

fn is_master_path(path: &Path) -> bool {
    if path
        .components()
        .any(|c| matches!(c, Component::Normal(s) if s == "Masters"))
    {
        return true;
    }
    path.file_name()
        .map(|n| n.to_string_lossy().to_lowercase().contains("master"))
        .unwrap_or(false)
}

fn prepare(st: Staged, cfg: &Config, mappings: &[Mapping]) -> Result<Prepared> {
    let mut header = fits::read_primary_header(&st.path)?;
    let imagetyp = header
        .get_str("IMAGETYP")
        .unwrap_or_default()
        .to_uppercase();
    if is_master_path(&st.input) || imagetyp.contains("MASTER") {
        if masters::determine_master_type(&st.input, &header).is_some() {
            return Ok(Prepared::Master { staged: st, header });
        }
        // e.g. PixInsight `masterLight_*.xisf` or DWARF `*-AstroWizard.fits`:
        // an integrated light, filed with the stacked results.
        header.set("IMAGETYP", Value::Str("Master Light".into()));
    }
    // Header fixes read folder names, so use the original location.
    let modified = normalize_header(&mut header, &st.input, mappings)?;
    let (new_name, dest_dir) = destination(&header, &cfg.repo)?;
    let rewrite = modified && cfg.save_modified_headers && !fits::is_gzip(&st.path);
    // The stored hash is of the file as it will be written, so re-loading the
    // same original later is still recognised as a duplicate.
    let hash = if rewrite {
        fits::sha256_with_header(&st.path, &header)?
    } else {
        util::sha256_file(&st.path)?
    };
    Ok(Prepared::Frame {
        staged: st,
        header,
        new_name,
        dest_dir,
        hash,
        rewrite,
    })
}

/// Apply header fixes (Celestron Origin, DWARF, mappings, FRAME→IMAGETYP,
/// calibration OBJECT names, CD matrix) and validate required keywords.
/// Returns whether anything changed.
pub fn normalize_header(h: &mut Header, path: &Path, mappings: &[Mapping]) -> Result<bool> {
    let mut modified = false;

    if h.get_str("CREATOR")
        .unwrap_or_default()
        .to_lowercase()
        .contains("origin")
    {
        if h.get_truthy("TELESCOP").is_none() {
            h.set("TELESCOP", Value::Str("Celestron Origin".into()));
            modified = true;
        }
        if h.get_truthy("INSTRUME").is_none() {
            if let Some(cam) = h.get_truthy("CAMERA") {
                h.set("INSTRUME", Value::Str(cam.trim().to_string()));
                modified = true;
            }
        }
    }

    // Telescope-specific fixes (e.g. DWARF files carry no IMAGETYP).
    if crate::telescope::normalize_header(h, path)? {
        modified = true;
    }

    if apply_mappings(h, mappings) {
        modified = true;
    }

    if h.get_truthy("IMAGETYP").is_none() {
        match h.get_truthy("FRAME") {
            Some(frame) => {
                h.set("IMAGETYP", Value::Str(frame));
                modified = true;
            }
            None => bail!("missing required IMAGETYP or FRAME keyword"),
        }
    }
    if h.get("EXPTIME").or_else(|| h.get("EXPOSURE")).is_none() {
        bail!("missing required EXPTIME/EXPOSURE keyword");
    }
    let imagetyp = h.get_str("IMAGETYP").unwrap_or_default();
    match FrameKind::classify(&imagetyp) {
        Some(FrameKind::Light) | None => {}
        Some(kind) => {
            h.set("OBJECT", Value::Str(kind.object_name().into()));
            modified = true;
        }
    }
    if h.get_truthy("DATE-OBS").is_none() {
        bail!("missing required DATE-OBS keyword");
    }
    parse_date_obs(&h.get_str("DATE-OBS").unwrap())?;

    if FrameKind::classify(&imagetyp) == Some(FrameKind::Light) {
        if !h.contains("CD1_1") {
            if let (Some(c1), Some(c2), Some(rot)) = (
                h.get_f64("CDELT1"),
                h.get_f64("CDELT2"),
                h.get_f64("CROTA2"),
            ) {
                let r = rot.to_radians();
                h.set_with_comment("CD1_1", Value::Float(c1 * r.cos()), "Rotation Matrix");
                h.set_with_comment("CD1_2", Value::Float(-c2 * r.sin()), "Rotation Matrix");
                h.set_with_comment("CD2_1", Value::Float(c1 * r.sin()), "Rotation Matrix");
                h.set_with_comment("CD2_2", Value::Float(c2 * r.cos()), "Rotation Matrix");
                modified = true;
            }
        }
        if h.get_truthy("OBJECT").is_none() {
            bail!("light frame has no OBJECT keyword");
        }
    }
    Ok(modified)
}

/// Apply Mapping table rules. Semantics follow the original download path:
/// an empty `current` is a default applied to missing/empty/"Unknown" values.
pub fn apply_mappings(h: &mut Header, mappings: &[Mapping]) -> bool {
    let mut changed = false;
    for m in mappings {
        let Some(replace) = m.replace.as_deref().filter(|r| !r.is_empty()) else {
            continue;
        };
        let card = m.card.to_uppercase();
        let current = m.current.as_deref().unwrap_or("").trim();
        match h.get_str(&card) {
            Some(value) => {
                let value = value.trim().to_string();
                let hit = if current.is_empty() {
                    matches!(
                        value.to_uppercase().as_str(),
                        "" | "NONE" | "NULL" | "UNKNOWN"
                    )
                } else {
                    value.eq_ignore_ascii_case(current)
                };
                if hit && value != replace {
                    h.set(&card, Value::Str(replace.to_string()));
                    changed = true;
                }
            }
            None if current.is_empty() || current.eq_ignore_ascii_case("UNKNOWN") => {
                h.set_with_comment(
                    &card,
                    Value::Str(replace.to_string()),
                    "Added via AstroFiler mapping",
                );
                changed = true;
            }
            None => {}
        }
    }
    changed
}

/// Parse DATE-OBS into (compact yyyymmddHHMMSS, yyyymmdd).
pub fn parse_date_obs(s: &str) -> Result<(String, String)> {
    let s = s.trim().replace('T', " ");
    let s = s.split('.').next().unwrap_or(&s);
    let dt = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
        .or_else(|_| {
            chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .map(|d| d.and_hms_opt(0, 0, 0).unwrap())
        })
        .map_err(|_| anyhow!("invalid DATE-OBS '{s}'"))?;
    Ok((
        dt.format("%Y%m%d%H%M%S").to_string(),
        dt.format("%Y%m%d").to_string(),
    ))
}

fn val(h: &Header, key: &str, default: &str) -> String {
    h.get_str(key).unwrap_or_else(|| default.to_string())
}

/// Stacked results (Seestar `Stacked_*.fit`, anything with STACKCNT/NCOMBINE > 1
/// that is a light) are filed separately from sub-frames.
pub fn is_stacked(h: &Header) -> bool {
    let light = FrameKind::classify(&val(h, "IMAGETYP", "")) == Some(FrameKind::Light);
    light
        && (h.get_i64("STACKCNT").unwrap_or(0) > 1
            || h.get_i64("NCOMBINE").unwrap_or(0) > 1
            || val(h, "IMAGETYP", "").to_uppercase().contains("MASTER"))
}

/// Descriptive file name and repository folder (same scheme as the original).
pub fn destination(h: &Header, repo: &Path) -> Result<(String, PathBuf)> {
    let imagetyp = val(h, "IMAGETYP", "");
    let (stamp, day) = parse_date_obs(&val(h, "DATE-OBS", ""))?;
    let exposure = h
        .get("EXPTIME")
        .or_else(|| h.get("EXPOSURE"))
        .map(|v| v.to_py_string())
        .unwrap_or_default();
    let telescope = sanitize(&val(h, "TELESCOP", "Unknown"));
    let instrument = sanitize(&val(h, "INSTRUME", "Unknown"));
    let xbin = val(h, "XBINNING", "1");
    let ybin = val(h, "YBINNING", "1");
    let temp = val(h, "CCD-TEMP", "0");
    let filter = sanitize(&h.get_truthy("FILTER").unwrap_or_else(|| "OSC".into()));
    let kind = FrameKind::classify(&imagetyp)
        .ok_or_else(|| anyhow!("unrecognised IMAGETYP '{imagetyp}'"))?;
    if is_stacked(h) {
        let object = sanitize(&val(h, "OBJECT", "Unknown"));
        let count = h
            .get_i64("STACKCNT")
            .or_else(|| h.get_i64("NCOMBINE"))
            .unwrap_or(0);
        let name = format!(
            "Stacked-{object}-{telescope}-{instrument}-{filter}-{stamp}-{count}x{exposure}s.fits"
        );
        return Ok((
            name,
            repo.join("Stacked")
                .join(&object)
                .join(&telescope)
                .join(&instrument),
        ));
    }
    let name = match kind {
        FrameKind::Light => format!(
            "{}-{telescope}-{instrument}-{filter}-{stamp}-{exposure}s-{xbin}x{ybin}-t{temp}.fits",
            sanitize(&val(h, "OBJECT", ""))
        ),
        FrameKind::Flat => format!(
            "Flat-{telescope}-{instrument}-{filter}-{stamp}-{exposure}s-{xbin}x{ybin}-t{temp}.fits"
        ),
        FrameKind::FlatDark => format!(
            "FlatDark-{telescope}-{instrument}-{stamp}-{exposure}s-{xbin}x{ybin}-t{temp}.fits"
        ),
        FrameKind::Dark => {
            format!("Dark-{telescope}-{instrument}-{stamp}-{exposure}s-{xbin}x{ybin}-t{temp}.fits")
        }
        FrameKind::Bias => {
            format!("Bias-{telescope}-{instrument}-{stamp}-{xbin}x{ybin}-t{temp}.fits")
        }
    };
    let dir = match kind {
        FrameKind::Light => repo
            .join("Light")
            .join(sanitize(&val(h, "OBJECT", "Unknown")))
            .join(&telescope)
            .join(&instrument)
            .join(day),
        other => repo
            .join("Calibrate")
            .join(other.object_name().to_uppercase())
            .join(&telescope)
            .join(&instrument),
    };
    Ok((name, dir))
}

fn file_record(h: &Header, path: &Path, hash: String) -> FitsFile {
    let imagetyp = val(h, "IMAGETYP", "").to_uppercase();
    let telescope = val(h, "TELESCOP", "Unknown");
    let instrument = val(h, "INSTRUME", "Unknown");
    let precalibrated = telescope.to_lowercase().contains("itelescope")
        || instrument.to_lowercase().contains("seestar");
    let no_filter = imagetyp.contains("DARK") || imagetyp.contains("BIAS");
    FitsFile {
        id: uuid::Uuid::new_v4().to_string(),
        name: util::normalize_path(path),
        date: h.get_str("DATE-OBS"),
        object: h
            .get_truthy("OBJECT")
            .or_else(|| Some(val(h, "IMAGETYP", ""))),
        image_type: Some(imagetyp),
        exptime: h
            .get("EXPTIME")
            .or_else(|| h.get("EXPOSURE"))
            .map(|v| v.to_py_string()),
        xbin: Some(val(h, "XBINNING", "1")),
        ybin: Some(val(h, "YBINNING", "1")),
        ccd_temp: Some(val(h, "CCD-TEMP", "0")),
        telescope: Some(telescope),
        instrument: Some(instrument),
        gain: h.get_str("GAIN"),
        offset: h.get_str("OFFSET"),
        filter: if no_filter {
            None
        } else {
            h.get_truthy("FILTER")
        },
        observer: h.get_truthy("OBSERVER"),
        notes: None,
        hash: Some(hash),
        session: None,
        calibrated: precalibrated,
        soft_delete: false,
        stacked: is_stacked(h),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::fits::{write_image, ImageShape, OutType};

    #[allow(clippy::too_many_arguments)]

    pub fn make_frame(
        dir: &Path,
        name: &str,
        typ: &str,
        object: Option<&str>,
        date: &str,
        exp: f64,
        filter: Option<&str>,
        seed: f32,
    ) -> PathBuf {
        let mut h = Header::default();
        h.set("IMAGETYP", Value::Str(typ.into()));
        if let Some(o) = object {
            h.set("OBJECT", Value::Str(o.into()));
        }
        h.set("DATE-OBS", Value::Str(date.into()));
        h.set("EXPTIME", Value::Float(exp));
        h.set("TELESCOP", Value::Str("RedCat 51".into()));
        h.set("INSTRUME", Value::Str("ZWO ASI2600MM".into()));
        h.set("XBINNING", Value::Int(1));
        h.set("YBINNING", Value::Int(1));
        h.set("CCD-TEMP", Value::Float(-10.0));
        if let Some(f) = filter {
            h.set("FILTER", Value::Str(f.into()));
        }
        let shape = ImageShape {
            width: 16,
            height: 8,
            planes: 1,
        };
        let data: Vec<f32> = (0..shape.len())
            .map(|i| 1000.0 + seed + (i % 7) as f32)
            .collect();
        let p = dir.join(name);
        write_image(&p, &h, shape, &data, OutType::U16).unwrap();
        p
    }

    #[test]
    fn ingests_and_files() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("incoming");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&src).unwrap();
        make_frame(
            &src,
            "a.fits",
            "Light Frame",
            Some("M 31"),
            "2024-10-01T21:00:00.123",
            300.0,
            Some("Ha"),
            1.0,
        );
        make_frame(
            &src,
            "b.fit",
            "Dark Frame",
            None,
            "2024-10-02T08:00:00",
            300.0,
            None,
            2.0,
        );
        make_frame(
            &src,
            "c.fits",
            "Light Frame",
            Some("M 31"),
            "2024-10-01T21:00:00.123",
            300.0,
            Some("Ha"),
            1.0,
        ); // duplicate content
        std::fs::write(src.join("junk.fits"), b"not fits").unwrap();

        let cfg = Config {
            repo: repo.clone(),
            source: src.clone(),
            ..Default::default()
        };
        let mut conn = db::open(&tmp.path().join("t.db")).unwrap();
        let r = ingest_folder(
            &mut conn,
            &cfg,
            &src,
            IngestOptions::MOVE,
            &crate::progress::NoProgress,
        )
        .unwrap();
        assert_eq!(r.registered, 2, "{r:?}");
        assert_eq!(r.duplicates.len(), 1);
        assert_eq!(r.errors.len(), 1);
        let light = repo.join("Light/M_31/RedCat_51/ZWO_ASI2600MM/20241001/M_31-RedCat_51-ZWO_ASI2600MM-Ha-20241001210000-300.0s-1x1-t-10.0.fits");
        assert!(light.exists());
        assert!(repo.join("Calibrate/DARK/RedCat_51/ZWO_ASI2600MM/Dark-RedCat_51-ZWO_ASI2600MM-20241002080000-300.0s-1x1-t-10.0.fits").exists());
        let files = db::all_files(&conn, false).unwrap();
        let dark = files
            .iter()
            .find(|f| f.object.as_deref() == Some("Dark"))
            .unwrap();
        assert_eq!(dark.exptime.as_deref(), Some("300.0"));
        assert_eq!(dark.filter, None);
        assert_eq!(dark.image_type.as_deref(), Some("DARK FRAME"));
    }

    #[test]
    fn mappings_apply() {
        let mut h = Header::default();
        h.set("TELESCOP", Value::Str("redcat".into()));
        let maps = vec![
            Mapping {
                id: 1,
                card: "TELESCOP".into(),
                current: Some("REDCAT".into()),
                replace: Some("RedCat 51".into()),
            },
            Mapping {
                id: 2,
                card: "OBSERVER".into(),
                current: None,
                replace: Some("Me".into()),
            },
        ];
        assert!(apply_mappings(&mut h, &maps));
        assert_eq!(h.get_str("TELESCOP").as_deref(), Some("RedCat 51"));
        assert_eq!(h.get_str("OBSERVER").as_deref(), Some("Me"));
    }
}
