//! Repository ingest: scan a folder, normalise headers, rename files to a
//! descriptive name, file them into the repository tree and catalogue them.
//!
//! Port of `FileProcessor.registerFitsImage`, restructured for speed: header
//! parsing, header fixes and SHA-256 hashing run in parallel across all cores,
//! and database inserts happen in a single transaction.

use crate::config::Config;
use crate::db::{self, FitsFile, Mapping};
use crate::fits::{self, Header, Value};
use crate::progress::Progress;
use crate::util::{self, sanitize, FrameKind};
use anyhow::{anyhow, bail, Result};
use rayon::prelude::*;
use rusqlite::Connection;
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
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

/// What happens when a *different* file already exists under the name a file
/// is being filed as. Empty or partly written leftovers of an interrupted copy
/// are always replaced, and identical files are always adopted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnConflict {
    /// Leave both files alone; the new one is not filed.
    #[default]
    Skip,
    /// Replace the existing file (and its catalogue entry).
    Overwrite,
    /// File the new one as `name_001.fits`.
    KeepBoth,
}

impl OnConflict {
    pub const ALL: [Self; 3] = [Self::Skip, Self::Overwrite, Self::KeepBoth];

    pub fn key(self) -> &'static str {
        match self {
            Self::Skip => "skip",
            Self::Overwrite => "overwrite",
            Self::KeepBoth => "keep-both",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|c| c.key().eq_ignore_ascii_case(s.trim()))
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Skip => "Skip the new file",
            Self::Overwrite => "Overwrite the existing file",
            Self::KeepBoth => "Keep both (new one gets a _001 suffix)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IngestOptions {
    pub placement: Placement,
    /// Work out where everything would go without touching files or catalogue.
    pub dry_run: bool,
    pub on_conflict: OnConflict,
    /// Skip the checksum of files that are only renamed into place (a Move
    /// within one drive or share), so nothing but their header is read. The
    /// checksums are filled in later by [`crate::batch::fill_checksums`].
    pub quick: bool,
}

impl IngestOptions {
    pub const MOVE: Self = IngestOptions {
        placement: Placement::Move,
        dry_run: false,
        on_conflict: OnConflict::Skip,
        quick: false,
    };
    pub const COPY: Self = IngestOptions {
        placement: Placement::Copy,
        dry_run: false,
        on_conflict: OnConflict::Skip,
        quick: false,
    };
    pub const IN_PLACE: Self = IngestOptions {
        placement: Placement::InPlace,
        dry_run: false,
        on_conflict: OnConflict::Skip,
        quick: false,
    };

    pub fn quick(self, quick: bool) -> Self {
        IngestOptions { quick, ..self }
    }

    pub fn with_conflict(self, on_conflict: OnConflict) -> Self {
        IngestOptions {
            on_conflict,
            ..self
        }
    }
}

#[derive(Debug, Default)]
pub struct IngestReport {
    pub registered: usize,
    /// Files a telescope module asked to skip.
    pub skipped: usize,
    /// Files whose content is already in the catalogue (e.g. loaded before).
    pub already_catalogued: usize,
    /// Companion files (stack previews, DWARF shotsInfo.json) placed next to their frames.
    pub sidecars: usize,
    pub duplicates: Vec<(PathBuf, String)>,
    /// Input -> existing file with the same name but different content, left
    /// alone because of `OnConflict::Skip`.
    pub conflicts: Vec<(PathBuf, PathBuf)>,
    /// Existing files replaced because of `OnConflict::Overwrite`.
    pub overwritten: usize,
    /// Empty or partly written files (from an interrupted copy) replaced.
    pub repaired: usize,
    /// Files filed without a checksum (quick move); fill them in afterwards.
    pub unhashed: usize,
    pub errors: Vec<(PathBuf, String)>,
    pub new_ids: Vec<String>,
    /// Input path -> final (or, in a dry run, planned) path of each filed file.
    pub placed: Vec<(PathBuf, PathBuf)>,
    /// Processed pictures filed with the frames.
    pub pictures: usize,
    /// Folders deleted after a move because nothing was left in them.
    pub folders_removed: usize,
    /// Object nicknames learned from folder and picture names.
    pub nicknames: Vec<(String, String)>,
    /// Files that were catalogued in place under the source and are now filed.
    pub refiled: usize,
    pub dry_run: bool,
}

impl IngestReport {
    pub fn summary(&self) -> String {
        let verb = if self.dry_run {
            "would be filed"
        } else {
            "registered"
        };
        let mut out = format!(
            "{} {verb}, {} already in catalogue, {} duplicate copies",
            self.registered,
            self.already_catalogued,
            self.duplicates.len() - self.already_catalogued,
        );
        if self.sidecars > 0 {
            out.push_str(&format!(", {} previews/session info files", self.sidecars));
        }
        if self.skipped > 0 {
            out.push_str(&format!(", {} skipped", self.skipped));
        }
        let would = if self.dry_run { "would be " } else { "" };
        if self.pictures > 0 {
            out.push_str(&format!(
                ", {} processed pictures {would}filed",
                self.pictures
            ));
        }
        if self.folders_removed > 0 {
            out.push_str(&format!(", {} empty folders removed", self.folders_removed));
        }
        if !self.nicknames.is_empty() {
            out.push_str(&format!(", {} nicknames learned", self.nicknames.len()));
        }
        if !self.conflicts.is_empty() {
            out.push_str(&format!(
                ", {} {would}skipped because a different file already has that name",
                self.conflicts.len()
            ));
        }
        if self.overwritten > 0 {
            out.push_str(&format!(
                ", {} existing files {would}overwritten",
                self.overwritten
            ));
        }
        if self.repaired > 0 {
            out.push_str(&format!(
                ", {} empty/incomplete files {would}replaced",
                self.repaired
            ));
        }
        if self.unhashed > 0 {
            out.push_str(&format!(
                ", {} checksums {}to fill in",
                self.unhashed,
                if self.dry_run {
                    "would be left "
                } else {
                    "left "
                }
            ));
        }
        out.push_str(&format!(", {} errors", self.errors.len()));
        out
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
        for (a, b) in &self.conflicts {
            writeln!(w, "conflict,{},{}", q(a), q(b))?;
        }
        for (a, e) in &self.errors {
            writeln!(w, "error,{},\"{}\"", q(a), e.replace('"', "\"\""))?;
        }
        Ok(())
    }
}

/// Recursively collect importable files under `dir`.
/// Folders the repository manages itself.
pub const MANAGED_DIRS: &[&str] = &["Light", "Calibrate", "Stacked", "Archive"];

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
    let fast = util::prefer_kernel_mount(source);
    if fast != source {
        log::info!(
            "Reading {} through the kernel mount {} instead of GNOME's network view",
            source.display(),
            fast.display()
        );
    }
    let source = fast.as_path();
    if !source.is_dir() {
        bail!("source folder {} does not exist", source.display());
    }
    // The folder and the pictures in it may give an object a nickname, which
    // then goes into the name of its folder.
    let mut cfg = crate::nick::effective(conn, cfg);
    let nicknames = crate::nick::learn_from(conn, &mut cfg, source, opts.dry_run);
    let cfg = &cfg;
    // Files a sync catalogued where they lie would be taken for "loaded
    // before" and left there. Forget them first, so a move files them.
    let mut refiled = 0;
    if opts.placement == Placement::Move && !opts.dry_run {
        refiled = forget_unfiled(conn, cfg, source)?;
        if refiled > 0 {
            log::info!(
                "{refiled} files were catalogued in place under {}",
                source.display()
            );
        }
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
    let mut files = collect_files(source, &excluded);
    log::info!(
        "Found {} candidate files in {}",
        files.len(),
        source.display()
    );
    let companions = seestar_companions(source);
    for other in &companions {
        let more = collect_files(other, &excluded);
        log::info!(
            "Also loading {} files from {} (the other half of this Seestar target)",
            more.len(),
            other.display()
        );
        files.extend(more);
    }
    let mut report = ingest_files(conn, cfg, files, opts, progress)?;
    report.nicknames = nicknames;
    report.refiled = refiled;
    // Pictures the user made from the frames go where the frames went.
    if opts.placement != Placement::InPlace {
        let mut filed = report.placed.clone();
        filed.extend(
            report
                .duplicates
                .iter()
                .map(|(a, b)| (a.clone(), PathBuf::from(b)))
                .filter(|(_, b)| b.is_absolute()),
        );
        report.pictures = crate::pictures::file(cfg, source, &filed, opts);
    }
    // A folder that was moved out of is not needed any more.
    if opts.placement == Placement::Move && !opts.dry_run {
        for dir in std::iter::once(&source.to_path_buf()).chain(&companions) {
            report.folders_removed += crate::batch::prune_source(cfg, dir);
        }
    }
    Ok(report)
}

/// Drop the catalogue rows of files under `src` that are not in the
/// repository's own folders yet. The files are not touched.
fn forget_unfiled(conn: &Connection, cfg: &Config, src: &Path) -> Result<usize> {
    let under = |p: &Path| format!("{}/", util::normalize_path(p).trim_end_matches('/'));
    let n = conn.execute(
        "DELETE FROM fitsFile WHERE substr(fitsFileName, 1, length(?1)) = ?1 \
         AND substr(fitsFileName, 1, length(?2)) <> ?2 \
         AND substr(fitsFileName, 1, length(?3)) <> ?3 \
         AND substr(fitsFileName, 1, length(?4)) <> ?4",
        rusqlite::params![
            under(src),
            under(&cfg.repo.join("Light")),
            under(&cfg.repo.join("Stacked")),
            under(&cfg.repo.join("Calibrate")),
        ],
    )?;
    Ok(n)
}

/// A Seestar keeps each target in two sibling folders: `M 2` (stacked
/// results and previews) and `M 2_sub` (sub-frames), plus `M 2_mosaic_sub`
/// for mosaics. Loading any one of them brings in the others.
pub fn seestar_companions(source: &Path) -> Vec<PathBuf> {
    let (Some(parent), Some(name)) = (source.parent(), source.file_name()) else {
        return vec![];
    };
    let name = name.to_string_lossy();
    let base = name
        .strip_suffix("_mosaic_sub")
        .or_else(|| name.strip_suffix("_sub"))
        .unwrap_or(&name);
    if base.is_empty() {
        return vec![];
    }
    [
        base.to_string(),
        format!("{base}_sub"),
        format!("{base}_mosaic_sub"),
    ]
    .into_iter()
    .filter(|n| *n != name)
    .map(|n| parent.join(n))
    .filter(|p| p.is_dir())
    .collect()
}

/// Where the stacked results of the sub-frames in `light_dir` belong: the
/// same folders under Stacked, without the one for the day.
pub(crate) fn stacked_dir(repo: &Path, light_dir: &Path) -> Option<PathBuf> {
    let rel = light_dir.strip_prefix(repo.join("Light")).ok()?;
    let mut parts: Vec<Component> = rel.components().collect();
    let day = parts.last().is_some_and(|c| {
        let s = c.as_os_str().to_string_lossy();
        s.len() == 8 && s.chars().all(|c| c.is_ascii_digit())
    });
    if day {
        parts.pop();
    }
    Some(repo.join("Stacked").join(parts.iter().collect::<PathBuf>()))
}

/// Place companion files next to the frames that came from the same folder:
/// session info (DWARF `shotsInfo.json`) beside the light frames, stack
/// previews (Seestar/DWARF stacked JPG/PNG) beside the stacked FITS. A preview
/// with the same name as a stacked FITS takes over that file's new name;
/// others are prefixed with their original folder name so sessions filed into
/// one directory don't collide. A preview without a stacked FITS goes to the
/// Stacked folder of the folder's sub-frames. Frames filed in an earlier run
/// are found through their recorded original path.
fn place_sidecars(
    conn: &Connection,
    repo: &Path,
    sidecars: &[PathBuf],
    opts: IngestOptions,
    report: &mut IngestReport,
) -> Result<()> {
    if opts.placement == Placement::InPlace {
        return Ok(()); // frames stay where they are, and so do their companions
    }
    for sidecar in sidecars {
        let (Some(kind), Some(src_dir)) = (util::sidecar_kind(sidecar), sidecar.parent()) else {
            continue;
        };
        let want_stacked = kind == util::SidecarKind::StackPreview;
        let is_stacked_dest = |d: &Path| d.components().any(|c| c.as_os_str() == "Stacked");
        // (original path, filed path) of this folder's frames: sub-frames or
        // stacked results.
        let find = |stacked: bool| -> Result<Vec<(PathBuf, PathBuf)>> {
            let frames: Vec<(PathBuf, PathBuf)> = report
                .placed
                .iter()
                .filter(|(i, d)| {
                    i.parent() == Some(src_dir)
                        && util::is_fits_name(d)
                        && is_stacked_dest(d) == stacked
                })
                .cloned()
                .collect();
            if !frames.is_empty() {
                return Ok(frames);
            }
            let prefix = format!("{}/", util::normalize_path(src_dir));
            let rows = db::files_where(
                conn,
                "substr(fitsFileOriginalFile, 1, length(?1)) = ?1 AND COALESCE(fitsFileStacked,0) = ?2",
                &[&prefix, &(stacked as i64)],
            )?;
            Ok(rows
                .into_iter()
                .map(|f| {
                    (
                        PathBuf::from(f.original.unwrap_or_default()),
                        PathBuf::from(f.name),
                    )
                })
                .collect())
        };
        let mut frames = find(want_stacked)?;
        // A preview whose stacked FITS did not come along (deleted, or never
        // downloaded) goes where that would have gone.
        let beside_subs = want_stacked && frames.is_empty();
        if beside_subs {
            frames = find(false)?;
        }
        if frames.is_empty() && kind == util::SidecarKind::SessionInfo {
            // Frames catalogued without an original path (older runs, or a
            // database from the Python app): match the session by target
            // and start time instead.
            if let Some((target, start, end)) = sidecar_session(sidecar) {
                let rows = db::files_where(
                    conn,
                    "fitsFileObject = ?1 AND fitsFileDate >= ?2 AND fitsFileDate <= ?3 \
                     AND COALESCE(fitsFileStacked,0)=0 AND COALESCE(fitsFileSoftDelete,0)=0",
                    &[&target, &start, &end],
                )?;
                frames = rows
                    .into_iter()
                    .map(|f| (PathBuf::new(), PathBuf::from(f.name)))
                    .collect();
            }
        }
        // Most common destination directory.
        let mut counts: std::collections::HashMap<PathBuf, usize> =
            std::collections::HashMap::new();
        for (_, d) in &frames {
            let dir = match d.parent() {
                Some(p) if beside_subs => stacked_dir(repo, p),
                p => p.map(Path::to_path_buf),
            };
            if let Some(p) = dir {
                *counts.entry(p).or_default() += 1;
            }
        }
        let Some((dest_dir, _)) = counts.into_iter().max_by_key(|(_, n)| *n) else {
            log::info!(
                "{}: no matching frames from this folder were filed; left in place",
                sidecar.display()
            );
            continue;
        };
        let name = sidecar
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let folder = src_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let ext = sidecar
            .extension()
            .map(|e| e.to_string_lossy().to_string())
            .unwrap_or_default();
        let twin = frames.iter().find(|(i, _)| {
            !beside_subs && i.file_stem().is_some() && i.file_stem() == sidecar.file_stem()
        });
        let file_name = match twin {
            Some((_, filed)) => format!(
                "{}.{ext}",
                filed.file_stem().unwrap_or_default().to_string_lossy()
            ),
            None if folder.is_empty() => name,
            None => format!("{folder}_{name}"),
        };
        let mut target = dest_dir.join(file_name);
        let existing = if !target.exists() {
            Existing::Free
        } else if std::fs::read(&target).ok() == std::fs::read(sidecar).ok() {
            Existing::Identical
        } else if std::fs::metadata(&target).is_ok_and(|m| m.len() == 0)
            || is_prefix_of(&target, sidecar).unwrap_or(false)
        {
            Existing::Incomplete
        } else {
            Existing::Different
        };
        let action = match (existing, opts.on_conflict) {
            (Existing::Free, _) => Action::Place,
            (Existing::Identical, _) => Action::Adopt,
            (Existing::Incomplete, _) | (Existing::Different, OnConflict::Overwrite) => {
                Action::Replace
            }
            (Existing::Different, OnConflict::KeepBoth) => Action::KeepBoth,
            (Existing::Different, OnConflict::Skip) => Action::Skip,
        };
        match action {
            Action::Skip => {
                report.conflicts.push((sidecar.clone(), target));
                continue;
            }
            Action::Replace if existing == Existing::Incomplete => report.repaired += 1,
            Action::Replace => report.overwritten += 1,
            Action::KeepBoth => target = util::unique_path(&target),
            _ => {}
        }
        let moving = opts.placement == Placement::Move;
        if !opts.dry_run && action != Action::Adopt {
            let r = match action {
                Action::Replace => replace_file(sidecar, &target, moving),
                _ if moving => util::move_file(sidecar, &target),
                _ => util::copy_file(sidecar, &target).map(|_| ()),
            };
            if let Err(e) = r {
                report
                    .errors
                    .push((sidecar.clone(), format!("placing companion file: {e:#}")));
                continue;
            }
        } else if !opts.dry_run && moving {
            std::fs::remove_file(sidecar).ok();
        }
        report.sidecars += 1;
        report.placed.push((sidecar.clone(), target));
    }
    Ok(())
}

/// Target name and session time window for a DWARF `shotsInfo.json`: the
/// target comes from the file, the start time from its folder name
/// (`..._2026-09-26-23-37-24-900`). The window runs 18 hours from the start.
fn sidecar_session(sidecar: &Path) -> Option<(String, String, String)> {
    let json: serde_json::Value = serde_json::from_slice(&std::fs::read(sidecar).ok()?).ok()?;
    let target = json.get("target")?.as_str()?.trim().to_string();
    let folder = sidecar.parent()?.file_name()?.to_string_lossy().to_string();
    let start = folder.split('_').find_map(|t| {
        t.get(..19)
            .and_then(|s| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d-%H-%M-%S").ok())
    })?;
    let end = start + chrono::Duration::hours(18);
    let fmt = "%Y-%m-%dT%H:%M:%S";
    Some((
        target,
        start.format(fmt).to_string(),
        end.format(fmt).to_string(),
    ))
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
/// (unzipped, decompressed) that can always be moved.
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
        /// None when skipped for a quick move.
        hash: Option<String>,
        rewrite: bool,
    },
}

pub fn ingest_files(
    conn: &mut Connection,
    cfg: &Config,
    files: Vec<PathBuf>,
    opts: IngestOptions,
    progress: &dyn Progress,
) -> Result<IngestReport> {
    let cfg = &crate::nick::effective(conn, cfg);
    let mut report = IngestReport {
        dry_run: opts.dry_run,
        ..Default::default()
    };
    let (sidecars, files): (Vec<PathBuf>, Vec<PathBuf>) =
        files.into_iter().partition(|p| util::is_sidecar(p));
    let before = files.len();
    let files: Vec<PathBuf> = files
        .into_iter()
        .filter(|p| !crate::telescope::skip_file(p))
        .collect();
    report.skipped = before - files.len();
    let total = files.len();
    // Scratch space for converted files, so sources are never written to
    // when copying or doing a dry run. It is inside the repository so that
    // filing is a rename; converting there happens in parallel, which on a
    // NAS measured faster than converting locally and copying one by one.
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
    place_sidecars(conn, &cfg.repo, &sidecars, opts, &mut report)?;
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
    // Fewer files at once when reading from or writing to a NAS.
    let mut io_paths: Vec<&Path> = vec![&cfg.repo];
    io_paths.extend(files.first().and_then(|f| f.parent()));
    let threads = util::io_threads(&io_paths);
    log::info!("Reading {threads} files at a time");
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()?;
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
        // Progress moves per file: the first half of a batch while files are
        // read and hashed, the second half while they are filed.
        let tick = |step: usize, steps: usize, half: usize, msg: String| {
            let within = (half * steps + step) * chunk.len() / (2 * steps.max(1));
            progress.update(done + within, total, &msg);
        };

        // Unpack containers (zip, gz) into plain FITS files, then parse,
        // normalise and hash them, one input file per task.
        let read = AtomicUsize::new(0);
        let steps = chunk.len();
        type Read = (PathBuf, Result<Vec<(PathBuf, Result<Prepared>)>>);
        let results: Vec<Read> = pool.install(|| {
            chunk
                .par_iter()
                .map(|p| {
                    let started = read.load(Ordering::Relaxed);
                    tick(started, steps, 0, format!("Reading {}", label(p)));
                    let r = unpack(p, opts, work).map(|staged| {
                        staged
                            .into_par_iter()
                            .map(|st| (st.input.clone(), prepare(st, cfg, opts, &mappings)))
                            .collect()
                    });
                    let n = read.fetch_add(1, Ordering::Relaxed) + 1;
                    tick(n, steps, 0, format!("Reading {}", label(p)));
                    (p.clone(), r)
                })
                .collect()
        });
        let mut prepared: Vec<(PathBuf, Result<Prepared>)> = Vec::new();
        for (input, r) in results {
            match r {
                Ok(list) => prepared.extend(list),
                Err(e) => report.errors.push((input, format!("{e:#}"))),
            }
        }

        // File and catalogue this batch in one transaction.
        let steps = prepared.len();
        if !opts.dry_run {
            conn.execute_batch("BEGIN IMMEDIATE")?;
        }
        let mut result = Ok(());
        for (i, (input, prep)) in prepared.into_iter().enumerate() {
            tick(i, steps, 1, format!("Filing {}", label(&input)));
            match prep {
                Ok(p) => {
                    if let Err(e) = file_prepared(conn, opts, p, report, &mut state) {
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
    opts: IngestOptions,
    prep: Prepared,
    report: &mut IngestReport,
    state: &mut FileState,
) -> Result<()> {
    match prep {
        Prepared::Frame {
            staged,
            header,
            new_name,
            dest_dir,
            hash,
            rewrite,
        } => {
            let target = dest_dir.join(&new_name);
            // A skipped checksum is needed after all when the name is taken:
            // that is how a file loaded before is recognised.
            let hash = match hash {
                None if target.exists() || state.planned.contains(&target) => {
                    Some(util::sha256_file(&staged.path)?)
                }
                h => h,
            };
            if let Some(hash) = &hash {
                if let Some(existing) = db::hash_exists(conn, hash)? {
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
            }
            let placement = if staged.temp && opts.placement == Placement::Copy {
                Placement::Move
            } else {
                opts.placement
            };
            let existing = if placement == Placement::InPlace || target == staged.path {
                Existing::Free
            } else if state.planned.contains(&target) {
                // Filed earlier in this run: two different frames that map to
                // the same name are both new data, so always keep both.
                Existing::Different
            } else {
                existing_file(&target, &staged.path, hash.as_deref().unwrap_or_default())
            };
            if existing == Existing::Identical && !opts.dry_run {
                // The same file, catalogued by a quick move whose checksum
                // isn't filled in yet: it is already catalogued, not a
                // leftover to adopt. Record the checksum while we have it.
                let name = util::normalize_path(&target);
                let updated = conn.execute(
                    "UPDATE fitsFile SET fitsFileHash=?1 WHERE fitsFileName=?2 AND fitsFileHash IS NULL",
                    rusqlite::params![hash, name],
                )?;
                let known: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM fitsFile WHERE fitsFileName=?1)",
                    [&name],
                    |r| r.get(0),
                )?;
                if updated > 0 || known {
                    if staged.temp {
                        std::fs::remove_file(&staged.path).ok();
                    }
                    report.already_catalogued += 1;
                    report.duplicates.push((staged.input, name));
                    return Ok(());
                }
            }
            let action = match existing {
                Existing::Free => Action::Place,
                // A previous, interrupted run already put this exact file in place.
                Existing::Identical => Action::Adopt,
                Existing::Incomplete => Action::Replace,
                Existing::Different if state.planned.contains(&target) => Action::KeepBoth,
                Existing::Different => match opts.on_conflict {
                    OnConflict::Skip => Action::Skip,
                    OnConflict::Overwrite => Action::Replace,
                    OnConflict::KeepBoth => Action::KeepBoth,
                },
            };
            if action == Action::Skip {
                log::info!(
                    "{}: not filed, a different file already exists at {}",
                    staged.input.display(),
                    target.display()
                );
                report.conflicts.push((staged.input, target));
                if staged.temp {
                    std::fs::remove_file(&staged.path).ok();
                }
                return Ok(());
            }
            if action == Action::Replace {
                if existing == Existing::Incomplete {
                    report.repaired += 1;
                } else {
                    report.overwritten += 1;
                }
            }
            if opts.dry_run {
                let dest = match (placement, action) {
                    (Placement::InPlace, _) => staged.path.clone(),
                    (_, Action::KeepBoth) => {
                        // Mirror unique_path() against files planned in this run.
                        let stem = Path::new(&new_name)
                            .file_stem()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_string();
                        let ext = Path::new(&new_name)
                            .extension()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_string();
                        let mut d = util::unique_path(&target);
                        let mut k = 1;
                        while state.planned.contains(&d) {
                            d = dest_dir.join(format!("{stem}_{k:03}.{ext}"));
                            k += 1;
                        }
                        d
                    }
                    _ => target,
                };
                state.planned.insert(dest.clone());
                report.registered += 1;
                report.placed.push((staged.input, dest));
                return Ok(());
            }
            let final_path = if placement == Placement::InPlace || target == staged.path {
                staged.path.clone()
            } else if action == Action::Adopt {
                if placement == Placement::Move || staged.temp {
                    std::fs::remove_file(&staged.path).ok();
                }
                target
            } else {
                let dest = if action == Action::KeepBoth {
                    util::unique_path(&target)
                } else {
                    target
                };
                let r = if action == Action::Replace {
                    replace_file(&staged.path, &dest, placement == Placement::Move)
                } else if placement == Placement::Move {
                    util::move_file(&staged.path, &dest)
                } else {
                    util::copy_file(&staged.path, &dest).map(|_| ())
                };
                if let Err(e) = r {
                    report
                        .errors
                        .push((staged.input, format!("filing into repository: {e:#}")));
                    return Ok(());
                }
                if action == Action::Replace {
                    // The old file is gone; so is whatever the catalogue knew about it.
                    conn.execute(
                        "DELETE FROM fitsFile WHERE fitsFileName=?1",
                        [util::normalize_path(&dest)],
                    )?;
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
            state.planned.insert(final_path.clone());
            if hash.is_none() {
                report.unhashed += 1;
            }
            let record = file_record(&header, &final_path, hash, &staged.input);
            record.insert(conn)?;
            report.new_ids.push(record.id);
            report.placed.push((staged.input, final_path));
            report.registered += 1;
        }
    }
    Ok(())
}

/// What is already at the path a file is about to be filed under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Existing {
    Free,
    /// Same content as the file being filed.
    Identical,
    /// Empty, or the beginning of the file being filed: left by a copy that
    /// failed or was interrupted.
    Incomplete,
    Different,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Place,
    Adopt,
    Replace,
    KeepBoth,
    Skip,
}

/// `hash` is the content hash the filed file will have (after any header
/// rewrite); `source` is the file as it is now.
fn existing_file(target: &Path, source: &Path, hash: &str) -> Existing {
    let Ok(meta) = std::fs::metadata(target) else {
        return Existing::Free;
    };
    if meta.len() == 0 {
        return Existing::Incomplete;
    }
    let source_len = std::fs::metadata(source).map(|m| m.len()).unwrap_or(0);
    if meta.len() < source_len && is_prefix_of(target, source).unwrap_or(false) {
        return Existing::Incomplete;
    }
    if util::sha256_file(target).is_ok_and(|h| h == hash) {
        Existing::Identical
    } else {
        Existing::Different
    }
}

/// Whether the whole of `short` equals the start of `long`.
fn is_prefix_of(short: &Path, long: &Path) -> Result<bool> {
    use std::io::Read;
    let mut a = std::fs::File::open(short)?;
    let mut b = std::fs::File::open(long)?;
    let (mut x, mut y) = (vec![0u8; 1 << 20], vec![0u8; 1 << 20]);
    loop {
        let n = a.read(&mut x)?;
        if n == 0 {
            return Ok(true);
        }
        b.read_exact(&mut y[..n])?;
        if x[..n] != y[..n] {
            return Ok(false);
        }
    }
}

/// Put `from` at `to`, replacing the file there. The new content is written
/// next to it first, so the existing file survives a failed copy.
fn replace_file(from: &Path, to: &Path, moving: bool) -> Result<()> {
    let mut tmp = to.as_os_str().to_owned();
    tmp.push(".astrofiler-tmp");
    let tmp = PathBuf::from(tmp);
    if moving {
        util::move_file(from, &tmp)?;
    } else {
        util::copy_file(from, &tmp)?;
    }
    if std::fs::rename(&tmp, to).is_err() {
        // Some network filesystems refuse to rename over an existing file.
        std::fs::remove_file(to)?;
        std::fs::rename(&tmp, to)?;
    }
    Ok(())
}

/// Turn one input file into the FITS file(s) to register.
fn unpack(path: &Path, opts: IngestOptions, work: &Path) -> Result<Vec<Staged>> {
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
    if lower.ends_with(".xisf")
        && opts.placement == Placement::InPlace
        && path.with_extension("fits").exists()
    {
        // Converted by an earlier version: its FITS twin is the catalogued file.
        return Ok(vec![]);
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

/// A descriptive name ends in .fits; an XISF file keeps its own extension.
pub(crate) fn keep_format(name: String, source: &Path) -> String {
    let xisf = source
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("xisf"));
    match name.strip_suffix(".fits") {
        Some(stem) if xisf => format!("{stem}.xisf"),
        _ => name,
    }
}

/// Whether a master frame is a bias, dark or flat, from its file name,
/// IMAGETYP or OBJECT. None for a master light.
fn master_calibration_kind(path: &Path, h: &Header) -> Option<FrameKind> {
    let from = |s: &str| {
        let n = util::normalize_image_type(s);
        if n.contains("BIAS") {
            Some(FrameKind::Bias)
        } else if n.contains("FLATDARK") || n.contains("DARKFLAT") {
            Some(FrameKind::FlatDark)
        } else if n.contains("DARK") {
            Some(FrameKind::Dark)
        } else if n.contains("FLAT") {
            Some(FrameKind::Flat)
        } else {
            None
        }
    };
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    from(&name)
        .or_else(|| from(&h.get_str("IMAGETYP").unwrap_or_default()))
        .or_else(|| from(&h.get_str("OBJECT").unwrap_or_default()))
}

/// A processed result that carries no IMAGETYP: a StackingWizard or
/// PixInsight stack (it says how many frames it holds), or a file with no
/// observation keywords at all (a stack converted from XISF by an older
/// version) in a folder that names its target.
fn is_processed_result(h: &Header, path: &Path) -> bool {
    if h.get_truthy("IMAGETYP").is_some() || h.get_truthy("FRAME").is_some() {
        return false;
    }
    let frames = ["NCOMBINE", "STACKCNT", "WZNSUBS"]
        .iter()
        .any(|k| h.get_i64(k).unwrap_or(0) > 1);
    let bare = ["OBJECT", "DATE-OBS", "EXPTIME", "EXPOSURE"]
        .iter()
        .all(|k| h.get_truthy(k).is_none());
    frames || (bare && crate::names::object_from_folders(path).is_some())
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

fn prepare(
    st: Staged,
    cfg: &Config,
    opts: IngestOptions,
    mappings: &[Mapping],
) -> Result<Prepared> {
    let mut header = fits::read_primary_header(&st.path)?;
    if is_processed_result(&header, &st.input) {
        log::info!(
            "{}: no IMAGETYP, filed as a stacked result",
            st.input.display()
        );
        header.set("IMAGETYP", Value::Str("Master Light".into()));
    }
    let imagetyp = header
        .get_str("IMAGETYP")
        .unwrap_or_default()
        .to_uppercase();
    if is_master_path(&st.input) || imagetyp.contains("MASTER") {
        match master_calibration_kind(&st.input, &header) {
            // Master bias, darks and flats are catalogued like the frames
            // they were made from.
            Some(kind) => {
                if FrameKind::classify(&imagetyp) != Some(kind) {
                    let t = format!("Master {}", kind.object_name());
                    header.set("IMAGETYP", Value::Str(t));
                }
            }
            // e.g. PixInsight `masterLight_*.xisf` or DWARF `*-AstroWizard.fits`:
            // an integrated light, filed with the stacked results.
            None => header.set("IMAGETYP", Value::Str("Master Light".into())),
        }
    }
    // Header fixes read folder names, so use the original location.
    let modified = normalize_header(&mut header, &st.input, mappings)?;
    let (mut new_name, dest_dir) = destination(&header, cfg)?;
    if is_stacked(&header) {
        // Stacked results keep the name the telescope or stacking program
        // gave them.
        if let Some(n) = st.path.file_name() {
            new_name = n.to_string_lossy().into_owned();
        }
    }
    let new_name = keep_format(new_name, &st.path);
    let rewrite = modified && cfg.save_modified_headers && !fits::is_gzip(&st.path);
    // The stored hash is of the file as it will be written, so re-loading the
    // same original later is still recognised as a duplicate.
    // A quick move renames files without reading them; their checksums are
    // filled in afterwards. Anything that has to be copied is read anyway.
    let renamed = (opts.placement == Placement::Move || st.temp)
        && !opts.dry_run
        && util::same_filesystem(&st.path, &cfg.repo);
    let hash = if opts.quick && renamed && !rewrite {
        None
    } else if rewrite {
        Some(fits::sha256_with_header(&st.path, &header)?)
    } else {
        Some(util::sha256_file(&st.path)?)
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
    let imagetyp = h.get_str("IMAGETYP").unwrap_or_default();
    // A stacked result may not say when or how long; its file does.
    let master_light = imagetyp.to_uppercase().contains("MASTER")
        && FrameKind::classify(&imagetyp) == Some(FrameKind::Light);
    if master_light && h.get("EXPTIME").or_else(|| h.get("EXPOSURE")).is_none() {
        let total = h.get_f64("LIVETIME").unwrap_or(0.0);
        h.set("EXPTIME", Value::Float(total));
    }
    if master_light && h.get_truthy("DATE-OBS").is_none() {
        if let Some(t) = path
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .map(chrono::DateTime::<chrono::Local>::from)
        {
            h.set(
                "DATE-OBS",
                Value::Str(t.format("%Y-%m-%dT%H:%M:%S").to_string()),
            );
        }
    }
    if h.get("EXPTIME").or_else(|| h.get("EXPOSURE")).is_none() {
        bail!("missing required EXPTIME/EXPOSURE keyword");
    }
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
        // PixInsight master lights do not name their target; the folder
        // they were saved in usually does.
        if h.get_truthy("OBJECT").is_none() && imagetyp.to_uppercase().contains("MASTER") {
            if let Some(o) = crate::names::object_from_folders(path) {
                log::info!(
                    "{}: no OBJECT keyword, using \"{o}\" from the folder name",
                    path.display()
                );
                h.set("OBJECT", Value::Str(o));
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

/// Whether a camera name is a Seestar smart telescope.
pub fn is_seestar(instrument: &str) -> bool {
    instrument.to_lowercase().starts_with("seestar")
}

/// The telescope/camera part of file names ("RedCat_51-ZWO_ASI2600MM") and
/// folders (`RedCat_51/ZWO_ASI2600MM`). A Seestar is one unit that writes its
/// serial number as TELESCOP ("S50_1a2b3c4d") and its model as INSTRUME
/// ("Seestar S50"), so it gets just the model: "Seestar_S50".
fn device_folder(h: &Header) -> (String, PathBuf) {
    let telescope = sanitize(&val(h, "TELESCOP", "Unknown"));
    let instrument = sanitize(&val(h, "INSTRUME", "Unknown"));
    if is_seestar(&instrument) {
        return (instrument.clone(), PathBuf::from(instrument));
    }
    (
        format!("{telescope}-{instrument}"),
        Path::new(&telescope).join(instrument),
    )
}

/// Descriptive file name and repository folder (same scheme as the original).
pub fn destination(h: &Header, cfg: &Config) -> Result<(String, PathBuf)> {
    let repo = &cfg.repo;
    // The panels of a mosaic share the mosaic's folder and get one of their
    // own below the telescope: Light/HD_199479/DWARF_3/TELE/Panel_1/<day>.
    let (mosaic, panel) = crate::names::mosaic(&val(h, "OBJECT", "Unknown"));
    let object_dir = || crate::names::object_folder(&mosaic, &cfg.object_names);
    let imagetyp = val(h, "IMAGETYP", "");
    let (stamp, day) = parse_date_obs(&val(h, "DATE-OBS", ""))?;
    let exposure = h
        .get("EXPTIME")
        .or_else(|| h.get("EXPOSURE"))
        .map(|v| v.to_py_string())
        .unwrap_or_default();
    let (device, device_dir) = device_folder(h);
    let device_dir = match panel {
        Some(n) => device_dir.join(crate::names::panel_folder(n)),
        None => device_dir,
    };
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
        let name = format!("Stacked-{object}-{device}-{filter}-{stamp}-{count}x{exposure}s.fits");
        return Ok((
            name,
            repo.join("Stacked").join(object_dir()).join(&device_dir),
        ));
    }
    let name = match kind {
        FrameKind::Light => format!(
            "{}-{device}-{filter}-{stamp}-{exposure}s-{xbin}x{ybin}-t{temp}.fits",
            sanitize(&val(h, "OBJECT", ""))
        ),
        FrameKind::Flat => {
            format!("Flat-{device}-{filter}-{stamp}-{exposure}s-{xbin}x{ybin}-t{temp}.fits")
        }
        FrameKind::FlatDark => {
            format!("FlatDark-{device}-{stamp}-{exposure}s-{xbin}x{ybin}-t{temp}.fits")
        }
        FrameKind::Dark => {
            format!("Dark-{device}-{stamp}-{exposure}s-{xbin}x{ybin}-t{temp}.fits")
        }
        FrameKind::Bias => {
            format!("Bias-{device}-{stamp}-{xbin}x{ybin}-t{temp}.fits")
        }
    };
    let dir = match kind {
        FrameKind::Light => repo
            .join("Light")
            .join(object_dir())
            .join(&device_dir)
            .join(day),
        other => repo
            .join("Calibrate")
            .join(other.object_name().to_uppercase())
            .join(&device_dir),
    };
    Ok((name, dir))
}

fn file_record(h: &Header, path: &Path, hash: Option<String>, original: &Path) -> FitsFile {
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
        hash,
        session: None,
        calibrated: precalibrated,
        soft_delete: false,
        stacked: is_stacked(h),
        original: Some(util::normalize_path(original)),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::fits::{write_image, ImageShape, OutType};
    use crate::progress::NoProgress;

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

    /// Turn a test frame into one written by a Seestar S50.
    pub fn make_seestar(path: &Path, stack: Option<i64>) {
        let mut h = fits::read_primary_header(path).unwrap();
        h.set("TELESCOP", Value::Str("S50_1a2b3c4d".into()));
        h.set("INSTRUME", Value::Str("Seestar S50".into()));
        if let Some(n) = stack {
            h.set("STACKCNT", Value::Int(n));
        }
        fits::rewrite_primary_header(path, &h).unwrap();
    }

    /// Rewrite a test frame's header: remove IMAGETYP and the given keys, add others.
    fn processed(path: &Path, drop: &[&str], add: &[(&str, Value)]) {
        let mut h = fits::read_primary_header(path).unwrap();
        for k in drop.iter().chain(["IMAGETYP"].iter()) {
            h.remove(k);
        }
        for (k, v) in add {
            h.set(k, v.clone());
        }
        fits::write_image(
            path,
            &h,
            ImageShape {
                width: 16,
                height: 8,
                planes: 1,
            },
            &(0..128).map(|i| 500.0 + i as f32).collect::<Vec<f32>>(),
            OutType::U16,
        )
        .unwrap();
    }

    #[test]
    fn an_emptied_source_folder_is_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let inbox = tmp.path().join("inbox");
        let emptied = inbox.join("M 31 night 1");
        let kept = inbox.join("M 31 night 2");
        for (dir, name, date) in [
            (&emptied, "a.fits", "2026-01-20T01:00:00"),
            (&kept, "b.fits", "2026-01-22T01:00:00"),
        ] {
            std::fs::create_dir_all(dir.join("sub")).unwrap();
            make_frame(
                &dir.join("sub"),
                name,
                "Light",
                Some("M 31"),
                date,
                30.0,
                None,
                1.0,
            );
        }
        // What a NAS leaves behind does not count; a telescope thumbnail does.
        std::fs::create_dir_all(emptied.join("sub/@eaDir/a.fits")).unwrap();
        std::fs::write(emptied.join(".DS_Store"), b"x").unwrap();
        std::fs::write(kept.join("sub/thumb.jpg"), b"x").unwrap();
        let cfg = Config {
            repo: tmp.path().join("repo"),
            source: inbox.clone(),
            ..Default::default()
        };
        let mut conn = db::open(&tmp.path().join("t.db")).unwrap();
        let r = ingest_folder(&mut conn, &cfg, &emptied, IngestOptions::MOVE, &NoProgress).unwrap();
        assert_eq!((r.registered, r.folders_removed), (1, 2), "{r:?}");
        assert!(!emptied.exists());
        // A dry run, a copy and the incoming folder itself stay.
        let other = inbox.join("M 31 night 3");
        std::fs::create_dir_all(&other).unwrap();
        make_frame(
            &other,
            "c.fits",
            "Light",
            Some("M 31"),
            "2026-01-21T01:00:00",
            30.0,
            None,
            5.0,
        );
        let r = ingest_folder(&mut conn, &cfg, &other, IngestOptions::COPY, &NoProgress).unwrap();
        assert_eq!(r.folders_removed, 0);
        let dry = IngestOptions {
            dry_run: true,
            ..IngestOptions::MOVE
        };
        ingest_folder(&mut conn, &cfg, &kept, dry, &NoProgress).unwrap();
        assert!(kept.join("sub/b.fits").exists() && other.exists());
        std::fs::remove_file(kept.join("sub/thumb.jpg")).unwrap();
        let r = ingest_folder(&mut conn, &cfg, &inbox, IngestOptions::MOVE, &NoProgress).unwrap();
        assert!(inbox.is_dir(), "{r:?}");
        let left: Vec<_> = walkdir::WalkDir::new(&kept)
            .into_iter()
            .flatten()
            .map(|e| e.into_path())
            .collect();
        assert!(!kept.exists(), "{left:?} {r:?}");
        // c.fits was copied before, so its original is left (it is a
        // duplicate now) and so is its folder.
        assert!(other.exists());
        // The repository is never removed, even when it is what was loaded.
        std::fs::create_dir_all(tmp.path().join("repo/Light/Empty")).unwrap();
        assert_eq!(
            crate::batch::prune_source(&cfg, &tmp.path().join("repo")),
            0
        );
        assert!(tmp.path().join("repo/Light/Empty").exists());
    }

    #[test]
    fn stacked_results_without_imagetyp_are_filed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("incoming/C4 Iris Nebula");
        std::fs::create_dir_all(&dir).unwrap();
        // A StackingWizard result: says what it is, but not IMAGETYP.
        let a = make_frame(
            &dir,
            "wizardstack.fits",
            "Light",
            Some("NGC 7023"),
            "2026-03-22T22:03:46",
            10.0,
            Some("IRCUT"),
            1.0,
        );
        processed(&a, &[], &[("NCOMBINE", Value::Int(684))]);
        // A stack converted from XISF long ago: no observation keywords at all.
        let b = make_frame(
            &dir,
            "C4 Iris Nebula.fits",
            "Light",
            Some("X"),
            "2026-03-22T22:03:46",
            10.0,
            None,
            2.0,
        );
        processed(
            &b,
            &[
                "OBJECT", "DATE-OBS", "EXPTIME", "TELESCOP", "INSTRUME", "XBINNING", "YBINNING",
                "CCD-TEMP",
            ],
            &[],
        );
        // A picture of the first.
        std::fs::write(dir.join("wizardstack.png"), b"png").unwrap();
        // An ordinary file without IMAGETYP and without anything to say what
        // it is is still refused.
        std::fs::create_dir_all(tmp.path().join("incoming/other")).unwrap();
        let c = make_frame(
            &tmp.path().join("incoming/other"),
            "x.fits",
            "Light",
            Some("M 1"),
            "2026-03-22T22:03:46",
            10.0,
            None,
            3.0,
        );
        processed(&c, &[], &[]);

        let repo = tmp.path().join("repo");
        let cfg = Config {
            repo: repo.clone(),
            source: tmp.path().join("incoming"),
            ..Default::default()
        };
        let mut conn = db::open(&tmp.path().join("t.db")).unwrap();
        let r = ingest_folder(&mut conn, &cfg, &dir, IngestOptions::MOVE, &NoProgress).unwrap();
        assert_eq!((r.registered, r.errors.len()), (2, 0), "{r:?}");
        assert_eq!(r.pictures, 1);
        let files = db::all_files(&conn, false).unwrap();
        assert!(files.iter().all(|f| f.stacked), "{files:?}");
        assert!(files
            .iter()
            .any(|f| f.object.as_deref() == Some("NGC 7023")));
        assert!(files.iter().any(|f| f.object.as_deref() == Some("C4")));
        assert!(files.iter().all(|f| f.name.contains("/Stacked/")));
        let r = ingest_folder(
            &mut conn,
            &cfg,
            &tmp.path().join("incoming/other"),
            IngestOptions::MOVE,
            &NoProgress,
        )
        .unwrap();
        assert_eq!(r.errors.len(), 1, "{r:?}");
    }

    #[test]
    fn seestar_target_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let works = tmp.path().join("MyWorks");
        let (stacked, subs) = (works.join("M 2"), works.join("M 2_sub"));
        std::fs::create_dir_all(&stacked).unwrap();
        std::fs::create_dir_all(&subs).unwrap();
        std::fs::create_dir_all(works.join("M 3_sub")).unwrap();
        for (i, t) in ["22:00:00", "22:00:10"].iter().enumerate() {
            let p = make_frame(
                &subs,
                &format!("Light_{i}.fit"),
                "Light",
                Some("M 2"),
                &format!("2024-08-01T{t}"),
                10.0,
                Some("IRCUT"),
                i as f32,
            );
            make_seestar(&p, None);
        }
        let p = make_frame(
            &stacked,
            "Stacked_2_M 2_10.0s_IRCUT_20240801-221000.fit",
            "Light",
            Some("M 2"),
            "2024-08-01T22:10:00",
            10.0,
            Some("IRCUT"),
            9.0,
        );
        make_seestar(&p, Some(2));

        assert_eq!(seestar_companions(&stacked), vec![subs.clone()]);
        assert_eq!(seestar_companions(&subs), vec![stacked.clone()]);
        assert!(seestar_companions(&works).is_empty());

        let repo = tmp.path().join("repo");
        let cfg = Config {
            repo: repo.clone(),
            source: tmp.path().join("incoming"),
            ..Default::default()
        };
        let mut conn = db::open(&tmp.path().join("t.db")).unwrap();
        // Pointing at the subs folder brings in the stacked result too.
        let r = ingest_folder(&mut conn, &cfg, &subs, IngestOptions::COPY, &NoProgress).unwrap();
        assert_eq!(r.registered, 3, "{r:?}");
        assert!(repo
            .join("Light/M_2/Seestar_S50/20240801/M_2-Seestar_S50-IRCUT-20240801220000-10.0s-1x1-t-10.0.fits")
            .exists());
        assert!(repo
            .join("Stacked/M_2/Seestar_S50/Stacked_2_M 2_10.0s_IRCUT_20240801-221000.fit")
            .exists());
    }

    #[test]
    fn master_calibration_frames_are_catalogued_as_calibration() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("in");
        std::fs::create_dir_all(&src).unwrap();
        let date = "2024-10-02T08:00:00";
        make_frame(
            &src,
            "masterDark_300s.fits",
            "Master Dark",
            None,
            date,
            300.0,
            None,
            1.0,
        );
        // IMAGETYP says nothing useful; the file name says flat.
        make_frame(
            &src,
            "masterFlat_L.fits",
            "Master",
            None,
            date,
            1.0,
            Some("L"),
            2.0,
        );
        let cfg = Config {
            repo: tmp.path().join("repo"),
            source: src.clone(),
            ..Default::default()
        };
        let mut conn = db::open(&tmp.path().join("t.db")).unwrap();
        let r = ingest_folder(&mut conn, &cfg, &src, IngestOptions::COPY, &NoProgress).unwrap();
        assert_eq!((r.registered, r.errors.len()), (2, 0), "{r:?}");
        let mut kinds: Vec<Option<FrameKind>> = db::all_files(&conn, false)
            .unwrap()
            .iter()
            .map(|f| {
                assert!(f.name.contains("/Calibrate/"), "{}", f.name);
                FrameKind::classify(f.image_type.as_deref().unwrap_or(""))
            })
            .collect();
        kinds.sort_by_key(|k| format!("{k:?}"));
        assert_eq!(kinds, [Some(FrameKind::Dark), Some(FrameKind::Flat)]);
    }

    #[test]
    fn progress_moves_per_file() {
        struct Record(std::sync::Mutex<Vec<usize>>);
        impl Progress for Record {
            fn update(&self, done: usize, _: usize, _: &str) {
                self.0.lock().unwrap().push(done);
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("in");
        std::fs::create_dir_all(&src).unwrap();
        for i in 0..70 {
            make_frame(
                &src,
                &format!("f{i}.fits"),
                "Light",
                Some("M 2"),
                &format!("2024-08-01T22:{:02}:{:02}", i / 60, i % 60),
                10.0,
                None,
                i as f32,
            );
        }
        let cfg = Config {
            repo: tmp.path().join("repo"),
            source: src.clone(),
            ..Default::default()
        };
        let mut conn = db::open(&tmp.path().join("t.db")).unwrap();
        let rec = Record(Default::default());
        let r = ingest_folder(&mut conn, &cfg, &src, IngestOptions::COPY, &rec).unwrap();
        assert_eq!(r.registered, 70, "{r:?}");
        let mut seen = rec.0.into_inner().unwrap();
        seen.sort();
        seen.dedup();
        // Every count within the first batch shows up, not just 64 and 70.
        assert!((1..=64).all(|n| seen.contains(&n)), "{seen:?}");
        assert_eq!(seen.last(), Some(&70));
    }

    #[test]
    fn quick_move_fills_checksums_later() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("archive");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&src).unwrap();
        let frame = |dir: &Path, name: &str, t: &str, seed: f32| {
            make_frame(
                dir,
                name,
                "Light",
                Some("M 2"),
                t,
                10.0,
                Some("IRCUT"),
                seed,
            )
        };
        frame(&src, "a.fits", "2024-08-01T22:00:00", 1.0);
        let b = frame(&src, "b.fits", "2024-08-01T22:00:10", 2.0);
        let b_copy = std::fs::read(&b).unwrap();
        let cfg = Config {
            repo: repo.clone(),
            source: src.clone(),
            ..Default::default()
        };
        let mut conn = db::open(&tmp.path().join("t.db")).unwrap();
        let quick = IngestOptions::MOVE.quick(true);
        let r = ingest_folder(&mut conn, &cfg, &src, quick, &NoProgress).unwrap();
        assert_eq!((r.registered, r.unhashed), (2, 2), "{r:?}");
        let files = db::all_files(&conn, false).unwrap();
        assert!(files
            .iter()
            .all(|f| f.hash.is_none() && Path::new(&f.name).exists()));

        // The same frame again, before the checksums are filled in: it is
        // recognised by its name and content, not catalogued twice.
        std::fs::write(src.join("b-again.fits"), &b_copy).unwrap();
        let r = ingest_folder(&mut conn, &cfg, &src, quick, &NoProgress).unwrap();
        assert_eq!((r.registered, r.already_catalogued), (0, 1), "{r:?}");
        assert_eq!(db::all_files(&conn, false).unwrap().len(), 2);

        let f = crate::batch::fill_checksums(&mut conn, &NoProgress).unwrap();
        assert_eq!((f.filled, f.duplicates, f.errors.len()), (1, 0, 0), "{f:?}");
        for file in db::all_files(&conn, false).unwrap() {
            assert_eq!(
                file.hash.as_deref(),
                Some(util::sha256_file(Path::new(&file.name)).unwrap().as_str())
            );
        }

        // Copies are read anyway, so they always get a checksum.
        let src2 = tmp.path().join("more");
        std::fs::create_dir_all(&src2).unwrap();
        frame(&src2, "c.fits", "2024-08-01T22:00:20", 3.0);
        let r = ingest_folder(
            &mut conn,
            &cfg,
            &src2,
            IngestOptions::COPY.quick(true),
            &NoProgress,
        )
        .unwrap();
        assert_eq!((r.registered, r.unhashed), (1, 0), "{r:?}");
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
        let light = repo.join("Light/M_31_Andromeda_Galaxy/RedCat_51/ZWO_ASI2600MM/20241001/M_31-RedCat_51-ZWO_ASI2600MM-Ha-20241001210000-300.0s-1x1-t-10.0.fits");
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
    fn name_conflicts() {
        let light_rel = "Light/M_31_Andromeda_Galaxy/RedCat_51/ZWO_ASI2600MM/20241001/M_31-RedCat_51-ZWO_ASI2600MM-Ha-20241001210000-300.0s-1x1-t-10.0.fits";
        let frame = |dir: &Path, name: &str, seed: f32| {
            std::fs::create_dir_all(dir).unwrap();
            let p = make_frame(
                dir,
                name,
                "Light Frame",
                Some("M 31"),
                "2024-10-01T21:00:00",
                300.0,
                Some("Ha"),
                seed,
            );
            std::fs::read(p).unwrap()
        };
        // Copies `new` (seed 1) into a repo whose destination already holds
        // `existing`: None = a catalogued different frame, Some(bytes) = raw bytes.
        let run = |existing: Option<Vec<u8>>, on_conflict: OnConflict| {
            let tmp = tempfile::tempdir().unwrap();
            let repo = tmp.path().join("repo");
            let light = repo.join(light_rel);
            let cfg = Config {
                repo: repo.clone(),
                ..Default::default()
            };
            let mut conn = db::open(&tmp.path().join("t.db")).unwrap();
            let opts = IngestOptions::COPY.with_conflict(on_conflict);
            match existing {
                Some(bytes) => {
                    std::fs::create_dir_all(light.parent().unwrap()).unwrap();
                    std::fs::write(&light, bytes).unwrap();
                }
                None => {
                    let old = tmp.path().join("old");
                    frame(&old, "x.fits", 9.0);
                    let r = ingest_folder(&mut conn, &cfg, &old, opts, &NoProgress).unwrap();
                    assert_eq!(r.registered, 1);
                }
            }
            let src = tmp.path().join("incoming");
            let new = frame(&src, "a.fits", 1.0);
            let r = ingest_folder(&mut conn, &cfg, &src, opts, &NoProgress).unwrap();
            let names: Vec<String> = db::all_files(&conn, false)
                .unwrap()
                .into_iter()
                .map(|f| f.name)
                .collect();
            let kept_both = light.with_file_name(
                light_rel
                    .rsplit('/')
                    .next()
                    .unwrap()
                    .replace(".fits", "_001.fits"),
            );
            let content = std::fs::read(&light).unwrap();
            (r, new, content, kept_both.exists(), names, tmp)
        };
        let other = frame(
            &tempfile::tempdir().unwrap().path().join("o"),
            "o.fits",
            5.0,
        );

        // Leftovers of a failed copy are replaced whatever the setting.
        for c in OnConflict::ALL {
            let (r, new, content, both, names, _t) = run(Some(vec![]), c);
            assert_eq!(
                (r.registered, r.repaired, both),
                (1, 1, false),
                "{c:?} {r:?}"
            );
            assert_eq!(content, new);
            assert_eq!(names.len(), 1);
        }
        let (r, new, content, both, _, _t) = run(Some(other[..4000].to_vec()), OnConflict::Skip);
        assert_eq!(
            content[..4000],
            other[..4000],
            "prefix of a different file is not ours"
        );
        assert_eq!((r.registered, r.conflicts.len(), both), (0, 1, false));
        let (r, _, content, _, _, _t) = run(Some(new[..4000].to_vec()), OnConflict::Skip);
        assert_eq!((r.registered, r.repaired), (1, 1));
        assert_eq!(content, new);

        // A different, uncatalogued file.
        let (r, _, content, both, names, _t) = run(Some(other.clone()), OnConflict::Skip);
        assert_eq!((r.registered, r.conflicts.len(), both), (0, 1, false));
        assert_eq!(content, other, "existing file untouched");
        assert!(names.is_empty());
        let (r, new, content, both, names, _t) = run(Some(other.clone()), OnConflict::Overwrite);
        assert_eq!((r.registered, r.overwritten, both), (1, 1, false));
        assert_eq!(content, new);
        assert_eq!(names.len(), 1);
        let (r, _, content, both, _, _t) = run(Some(other.clone()), OnConflict::KeepBoth);
        assert_eq!((r.registered, both), (1, true));
        assert_eq!(content, other);

        // A different, catalogued file: overwriting drops its old catalogue entry.
        let (r, new, content, _, names, _t) = run(None, OnConflict::Overwrite);
        assert_eq!((r.registered, r.overwritten), (1, 1));
        assert_eq!(content, new);
        assert_eq!(names.len(), 1, "{names:?}");
        let (r, _, _, both, names, _t) = run(None, OnConflict::Skip);
        assert_eq!(
            (r.registered, r.conflicts.len(), both, names.len()),
            (0, 1, false, 1)
        );

        // Dry runs report the same decisions without touching anything.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let light = repo.join(light_rel);
        std::fs::create_dir_all(light.parent().unwrap()).unwrap();
        std::fs::write(&light, &other).unwrap();
        let src = tmp.path().join("incoming");
        frame(&src, "a.fits", 1.0);
        let cfg = Config {
            repo,
            ..Default::default()
        };
        let mut conn = db::open(&tmp.path().join("t.db")).unwrap();
        let dry = IngestOptions {
            dry_run: true,
            ..IngestOptions::COPY
        };
        let r = ingest_folder(&mut conn, &cfg, &src, dry, &NoProgress).unwrap();
        assert_eq!((r.registered, r.conflicts.len()), (0, 1));
        let dry = dry.with_conflict(OnConflict::Overwrite);
        let r = ingest_folder(&mut conn, &cfg, &src, dry, &NoProgress).unwrap();
        assert_eq!((r.registered, r.overwritten), (1, 1));
        assert_eq!(r.placed[0].1, light);
        assert_eq!(std::fs::read(&light).unwrap(), other);
    }

    #[test]
    fn companion_conflicts() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp
            .path()
            .join("DWARF_RAW_TELE_M 31_EXP_300_GAIN_60_2024-10-01-21-00-00-000");
        std::fs::create_dir_all(&folder).unwrap();
        make_frame(
            &folder,
            "a.fits",
            "Light Frame",
            Some("M 31"),
            "2024-10-01T21:00:00",
            300.0,
            Some("Ha"),
            1.0,
        );
        std::fs::write(folder.join("shotsInfo.json"), b"{\"target\":\"M 31\"}").unwrap();
        let cfg = Config {
            repo: tmp.path().join("repo"),
            ..Default::default()
        };
        let mut conn = db::open(&tmp.path().join("t.db")).unwrap();
        let r = ingest_folder(&mut conn, &cfg, &folder, IngestOptions::COPY, &NoProgress).unwrap();
        assert_eq!(r.sidecars, 1, "{r:?}");
        let json = r
            .placed
            .iter()
            .find(|(i, _)| i.ends_with("shotsInfo.json"))
            .unwrap()
            .1
            .clone();
        std::fs::write(&json, b"something else").unwrap();
        let mut again = |c: OnConflict| {
            ingest_folder(
                &mut conn,
                &cfg,
                &folder,
                IngestOptions::COPY.with_conflict(c),
                &NoProgress,
            )
            .unwrap()
        };
        let r = again(OnConflict::Skip);
        assert_eq!((r.sidecars, r.conflicts.len()), (0, 1), "{r:?}");
        assert_eq!(std::fs::read(&json).unwrap(), b"something else");
        let r = again(OnConflict::Overwrite);
        assert_eq!((r.sidecars, r.overwritten), (1, 1), "{r:?}");
        assert_eq!(std::fs::read(&json).unwrap(), b"{\"target\":\"M 31\"}");
        std::fs::write(&json, b"").unwrap();
        let r = again(OnConflict::Skip);
        assert_eq!((r.sidecars, r.repaired), (1, 1), "{r:?}");
    }

    #[test]
    fn dwarf_stacks_without_a_frame_count_are_stacked_results() {
        let tmp = tempfile::tempdir().unwrap();
        let night = |n: &str| {
            let dir = tmp.path().join(format!(
                "in/DWARF_RAW_TELE_C 9_EXP_60_GAIN_60_2025-10-0{n}-20-25-05-997"
            ));
            std::fs::create_dir_all(&dir).unwrap();
            dir
        };
        let frame = |dir: &Path, name: &str, date: &str, exp: f64, seed: f32| {
            let p = make_frame(dir, name, "Light", Some("C 9"), date, exp, None, seed);
            processed(&p, &[], &[]);
        };
        // The telescope's stack, its previews and a sub-frame.
        let (first, second) = (night("1"), night("2"));
        let stack = "stacked-16_C 9_60s60_Duo-Band_20251001-202510704";
        frame(
            &first,
            "C 9_60s60_0001.fits",
            "2025-10-01T20:26:00",
            60.0,
            1.0,
        );
        frame(
            &first,
            &format!("{stack}.fits"),
            "2025-10-01T23:00:00",
            11040.0,
            2.0,
        );
        std::fs::write(first.join(format!("{stack}.png")), b"png").unwrap();
        std::fs::write(first.join("stacked.jpg"), b"jpg").unwrap();
        // A night whose stacked FITS was not kept.
        frame(
            &second,
            "C 9_60s60_0001.fits",
            "2025-10-02T20:26:00",
            60.0,
            3.0,
        );
        std::fs::write(second.join("stacked.jpg"), b"jpg 2").unwrap();

        let repo = tmp.path().join("repo");
        let cfg = Config {
            repo: repo.clone(),
            ..Default::default()
        };
        let mut conn = db::open(&tmp.path().join("t.db")).unwrap();
        let source = tmp.path().join("in");
        let r = ingest_folder(&mut conn, &cfg, &source, IngestOptions::MOVE, &NoProgress).unwrap();
        assert_eq!(
            (r.registered, r.sidecars, r.errors.len()),
            (3, 3, 0),
            "{r:?}"
        );
        let stacked = repo.join("Stacked/C_9_Cave_Nebula/RedCat_51/TELE");
        let folder = |d: &Path| d.file_name().unwrap().to_string_lossy().into_owned();
        for name in [
            format!("{stack}.fits"),
            format!("{stack}.png"),
            format!("{}_stacked.jpg", folder(&first)),
            format!("{}_stacked.jpg", folder(&second)),
        ] {
            assert!(stacked.join(&name).exists(), "{name} {r:?}");
        }
        let files = db::all_files(&conn, false).unwrap();
        assert_eq!(files.iter().filter(|f| f.stacked).count(), 1, "{files:?}");
        assert!(!source.exists(), "nothing is left behind");
    }

    #[test]
    fn shots_info_follows_its_frames() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = "DWARF_RAW_TELE_M 39_EXP_15_GAIN_60_2026-09-27-01-46-25-216";
        let src = tmp.path().join("in").join(folder);
        std::fs::create_dir_all(&src).unwrap();
        make_frame(
            &src,
            "a.fits",
            "Light",
            Some("M 39"),
            "2026-09-27T01:47:46",
            15.0,
            Some("Astro"),
            1.0,
        );
        make_frame(
            &src,
            "b.fits",
            "Light",
            Some("M 39"),
            "2026-09-27T01:48:01",
            15.0,
            Some("Astro"),
            2.0,
        );
        let cfg = Config {
            repo: tmp.path().join("repo"),
            ..Default::default()
        };
        let mut conn = db::open(&tmp.path().join("t.db")).unwrap();
        let src_root = tmp.path().join("in");

        // First load: frames only (as if the json had been added later).
        let r = ingest_folder(
            &mut conn,
            &cfg,
            &src_root,
            IngestOptions::COPY,
            &crate::progress::NoProgress,
        )
        .unwrap();
        assert_eq!((r.registered, r.sidecars), (2, 0));

        // Second load finds the json and puts it beside the frames filed earlier.
        std::fs::write(
            src.join("shotsInfo.json"),
            br#"{"target": "M 39", "shotsTaken": 204}"#,
        )
        .unwrap();
        let r = ingest_folder(
            &mut conn,
            &cfg,
            &src_root,
            IngestOptions::COPY,
            &crate::progress::NoProgress,
        )
        .unwrap();
        assert_eq!(
            (r.registered, r.sidecars, r.errors.len()),
            (0, 1, 0),
            "{r:?}"
        );
        let frame_dir = cfg.repo.join("Light/M_39/RedCat_51/ZWO_ASI2600MM/20260927");
        let placed = frame_dir.join(format!("{folder}_shotsInfo.json"));
        assert!(placed.exists(), "{placed:?}");
        assert!(
            src.join("shotsInfo.json").exists(),
            "copy mode keeps the original"
        );

        // Running again doesn't create a second copy.
        ingest_folder(
            &mut conn,
            &cfg,
            &src_root,
            IngestOptions::COPY,
            &crate::progress::NoProgress,
        )
        .unwrap();
        let jsons = std::fs::read_dir(&frame_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .count();
        assert_eq!(jsons, 1);
    }

    #[test]
    fn shots_info_found_by_target_and_time() {
        // Frames catalogued without an original path (older runs, or the
        // Python app's database) are matched by target and start time.
        let tmp = tempfile::tempdir().unwrap();
        let folder = "DWARF_RAW_TELE_C 13_EXP_15_GAIN_60_2026-09-26-23-37-24-900";
        let src = tmp.path().join("in").join(folder);
        std::fs::create_dir_all(&src).unwrap();
        make_frame(
            &src,
            "a.fits",
            "Light",
            Some("C 13"),
            "2026-09-26T23:38:00",
            15.0,
            Some("Astro"),
            1.0,
        );
        let cfg = Config {
            repo: tmp.path().join("repo"),
            ..Default::default()
        };
        let mut conn = db::open(&tmp.path().join("t.db")).unwrap();
        ingest_folder(
            &mut conn,
            &cfg,
            &tmp.path().join("in"),
            IngestOptions::MOVE,
            &crate::progress::NoProgress,
        )
        .unwrap();
        conn.execute("UPDATE fitsFile SET fitsFileOriginalFile = NULL", [])
            .unwrap();
        // The folder went with its frames.
        assert!(!src.exists());
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("shotsInfo.json"), br#"{"target": "C 13"}"#).unwrap();
        let r = ingest_folder(
            &mut conn,
            &cfg,
            &tmp.path().join("in"),
            IngestOptions::MOVE,
            &crate::progress::NoProgress,
        )
        .unwrap();
        assert_eq!(r.sidecars, 1, "{r:?}");
        let placed = cfg
            .repo
            .join("Light/C_13_Owl_Cluster/RedCat_51/ZWO_ASI2600MM/20260926")
            .join(format!("{folder}_shotsInfo.json"));
        assert!(placed.exists(), "{placed:?}");
        assert!(!src.join("shotsInfo.json").exists(), "moved");
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
