//! Master calibration frame management: registering existing masters,
//! building new masters from calibration sessions (sigma-clipped stacking),
//! validating file integrity and cleaning up missing files.
//!
//! Stacking processes the image in row bands across all cores, so memory use
//! stays bounded no matter how many frames are combined.

use crate::config::Config;
use crate::db::{self, Master};
use crate::fits::{self, FitsFile, Header, ImageShape, OutType, Value};
use crate::ingest::Placement;
use crate::progress::Progress;
use crate::util::{self, sanitize, FrameKind};
use anyhow::{anyhow, bail, Context, Result};
use rayon::prelude::*;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Work out the master type from the file name, IMAGETYP and OBJECT
/// (port of `_determine_master_type`).
pub fn determine_master_type(path: &Path, h: &Header) -> Option<FrameKind> {
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

fn unique_master_id(conn: &Connection, kind: FrameKind, telescope: &str) -> Result<String> {
    let base = format!(
        "{}_{}_{}",
        kind.master_type(),
        telescope.replace(' ', "_"),
        chrono::Local::now().format("%Y%m%d_%H%M%S")
    );
    let mut id = base.clone();
    let mut n = 1;
    while conn
        .query_row(
            "SELECT 1 FROM Masters WHERE master_id=?1",
            [&id],
            |_| Ok(()),
        )
        .optional()?
        .is_some()
    {
        id = format!("{base}_{n}");
        n += 1;
    }
    Ok(id)
}

/// Register an existing master file. Returns (master_id, final path).
pub fn register_master(
    conn: &Connection,
    cfg: &Config,
    path: &Path,
    h: &Header,
    placement: Placement,
) -> Result<(String, PathBuf)> {
    let kind =
        determine_master_type(path, h).ok_or_else(|| anyhow!("could not determine master type"))?;
    let hash = util::md5_file(path)?;
    let mut final_path = path.to_path_buf();
    let masters_dir = cfg.masters_dir();
    if placement != Placement::InPlace && !path.starts_with(&masters_dir) {
        final_path = util::unique_path(&masters_dir.join(path.file_name().unwrap()));
        if placement == Placement::Move {
            util::move_file(path, &final_path)?;
        } else {
            std::fs::create_dir_all(&masters_dir)?;
            util::copy_file(path, &final_path)?;
        }
    }
    let final_str = util::normalize_path(&final_path);

    if let Some((id, master_id)) = conn
        .query_row(
            "SELECT id, master_id FROM Masters WHERE hash_value=?1 AND COALESCE(soft_delete,0)=0",
            [&hash],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()?
    {
        conn.execute(
            "UPDATE Masters SET master_path=?1 WHERE id=?2",
            params![final_str, id],
        )?;
        return Ok((master_id, final_path));
    }

    let telescope = h.get_str("TELESCOP").unwrap_or_else(|| "Unknown".into());
    let file_count = h
        .get_i64("NCOMBINE")
        .or_else(|| h.get_i64("NIMAGES"))
        .unwrap_or(0);
    let master = Master {
        master_id: unique_master_id(conn, kind, &telescope)?,
        master_type: kind.master_type().into(),
        path: final_str,
        creation_date: db::now_str(),
        instrument: Some(h.get_str("INSTRUME").unwrap_or_else(|| "Unknown".into())),
        telescope: Some(telescope),
        exposure: matches!(kind, FrameKind::Dark | FrameKind::FlatDark)
            .then(|| {
                h.get("EXPTIME")
                    .or_else(|| h.get("EXPOSURE"))
                    .map(|v| v.to_py_string())
            })
            .flatten(),
        xbin: Some(h.get_str("XBINNING").unwrap_or_else(|| "1".into())),
        ybin: Some(h.get_str("YBINNING").unwrap_or_else(|| "1".into())),
        ccd_temp: h.get_str("CCD-TEMP"),
        gain: h.get_str("GAIN"),
        offset: h.get_str("OFFSET"),
        filter: (kind == FrameKind::Flat)
            .then(|| h.get_str("FILTER"))
            .flatten(),
        source_session: None,
        file_count,
        file_size: std::fs::metadata(&final_path).ok().map(|m| m.len() as i64),
        hash: Some(hash),
        validated: true,
        ..Default::default()
    };
    master.insert(conn)?;
    log::info!(
        "Registered {} master {}",
        master.master_type,
        final_path.display()
    );
    Ok((master.master_id, final_path))
}

/// Scan a folder for master frames and register them.
pub fn register_folder(
    conn: &mut Connection,
    cfg: &Config,
    folder: &Path,
    move_files: bool,
    progress: &dyn Progress,
) -> Result<(usize, Vec<(PathBuf, String)>)> {
    let files: Vec<PathBuf> = crate::ingest::collect_files(folder, &[])
        .into_iter()
        .filter(|p| util::is_fits_name(p))
        .collect();
    let headers: Vec<(PathBuf, Result<Header>)> = files
        .into_par_iter()
        .map(|p| {
            let h = fits::read_primary_header(&p);
            (p, h)
        })
        .collect();
    let tx = conn.transaction()?;
    let mut count = 0;
    let mut errors = Vec::new();
    let total = headers.len();
    for (i, (path, h)) in headers.into_iter().enumerate() {
        progress.update(
            i + 1,
            total,
            &path.file_name().unwrap_or_default().to_string_lossy(),
        );
        let h = match h {
            Ok(h) => h,
            Err(e) => {
                errors.push((path, format!("{e:#}")));
                continue;
            }
        };
        let imagetyp = h.get_str("IMAGETYP").unwrap_or_default().to_uppercase();
        let looks_master = imagetyp.contains("MASTER")
            || path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_lowercase()
                .contains("master");
        if !looks_master || determine_master_type(&path, &h).is_none() {
            continue;
        }
        match register_master(
            &tx,
            cfg,
            &path,
            &h,
            if move_files {
                Placement::Move
            } else {
                Placement::InPlace
            },
        ) {
            Ok(_) => count += 1,
            Err(e) => errors.push((path, format!("{e:#}"))),
        }
    }
    tx.commit()?;
    Ok((count, errors))
}

/// Per-pixel kappa-sigma clipped mean. `v` is scratch space and is reordered.
fn clipped_mean(v: &mut [f32], kappa: f32) -> (f32, usize) {
    let mut n = v.len();
    if n == 0 {
        return (f32::NAN, 0);
    }
    if n >= 3 {
        for _ in 0..5 {
            let s = &mut v[..n];
            s.sort_unstable_by(|a, b| a.total_cmp(b));
            let median = if n % 2 == 1 {
                s[n / 2]
            } else {
                0.5 * (s[n / 2 - 1] + s[n / 2])
            };
            let mean = s.iter().sum::<f32>() / n as f32;
            let var = s.iter().map(|x| (x - mean) * (x - mean)).sum::<f32>() / n as f32;
            let limit = kappa * var.sqrt();
            if limit <= 0.0 {
                break;
            }
            let mut kept = 0;
            for i in 0..n {
                if (s[i] - median).abs() <= limit {
                    s.swap(kept, i);
                    kept += 1;
                }
            }
            if kept == n || kept < 2 {
                break;
            }
            n = kept;
        }
    }
    let mean = v[..n].iter().sum::<f32>() / n as f32;
    (mean, v.len() - n)
}

/// Approximate median from up to ~1M evenly spaced samples.
fn sample_median(data: &[f32]) -> f32 {
    let step = (data.len() / 1_000_000).max(1);
    let mut s: Vec<f32> = data
        .iter()
        .step_by(step)
        .copied()
        .filter(|v| v.is_finite())
        .collect();
    if s.is_empty() {
        return 1.0;
    }
    let mid = s.len() / 2;
    *s.select_nth_unstable_by(mid, |a, b| a.total_cmp(b)).1
}

pub struct StackResult {
    pub header: Header,
    pub shape: ImageShape,
    pub data: Vec<f32>,
    pub rejected_pct: f64,
    pub frames: usize,
}

/// Sigma-clipped combine. Flats are normalised by their median before
/// combining, then rescaled to the mean median so the result stays in ADU.
pub fn stack(
    paths: &[PathBuf],
    kind: FrameKind,
    kappa: f32,
    progress: &dyn Progress,
) -> Result<StackResult> {
    if paths.is_empty() {
        bail!("no frames to stack");
    }
    let first = FitsFile::open(&paths[0])?;
    let hdu0 = first.image_hdu()?;
    let shape = first.shape(hdu0);
    let header = first.hdus[hdu0].header.clone();
    drop(first);

    // Validate geometry; gzip files are decompressed once into temp files so
    // they can be read band by band.
    let tmpdir = std::env::temp_dir().join(format!("astrofiler-stack-{}", uuid::Uuid::new_v4()));
    let mut usable: Vec<(PathBuf, usize)> = Vec::new();
    for p in paths {
        let src = if fits::is_gzip(p) {
            std::fs::create_dir_all(&tmpdir)?;
            let out = tmpdir.join(format!("{}.fits", usable.len()));
            let mut dec = flate2::read::GzDecoder::new(std::fs::File::open(p)?);
            std::io::copy(&mut dec, &mut std::fs::File::create(&out)?)?;
            out
        } else {
            p.clone()
        };
        match FitsFile::open(&src).and_then(|f| {
            let h = f.image_hdu()?;
            Ok((f.shape(h), h))
        }) {
            Ok((s, h)) if s == shape => usable.push((src, h)),
            Ok((s, _)) => log::warn!(
                "skipping {}: size {}x{} differs from {}x{}",
                p.display(),
                s.width,
                s.height,
                shape.width,
                shape.height
            ),
            Err(e) => log::warn!("skipping {}: {e:#}", p.display()),
        }
    }
    let result = stack_inner(&usable, shape, kind, kappa, progress);
    std::fs::remove_dir_all(&tmpdir).ok();
    let (data, rejected_pct) = result?;
    Ok(StackResult {
        header,
        shape,
        data,
        rejected_pct,
        frames: usable.len(),
    })
}

fn stack_inner(
    frames: &[(PathBuf, usize)],
    shape: ImageShape,
    kind: FrameKind,
    kappa: f32,
    progress: &dyn Progress,
) -> Result<(Vec<f32>, f64)> {
    let n = frames.len();
    if n < 1 {
        bail!("no usable frames");
    }
    let medians: Vec<f32> = if kind == FrameKind::Flat {
        progress.update(0, n, "Measuring flat frame levels...");
        frames
            .par_iter()
            .map(|(p, _)| fits::read_image(p).map(|img| sample_median(&img.data)))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .map(|m| if m.abs() < 1e-6 { 1.0 } else { m })
            .collect()
    } else {
        vec![1.0; n]
    };
    let scale = if kind == FrameKind::Flat {
        medians.iter().sum::<f32>() / n as f32
    } else {
        1.0
    };

    let width = shape.width;
    let rows = shape.rows();
    let band = (4_000_000 / (width * n).max(1)).clamp(1, rows);
    let bands = rows.div_ceil(band);
    let mut out = vec![0f32; shape.len()];
    let rejected = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);

    out.par_chunks_mut(band * width)
        .enumerate()
        .try_for_each(|(b, chunk)| -> Result<()> {
            let row0 = b * band;
            let nrows = chunk.len() / width;
            let mut inputs = Vec::with_capacity(n);
            for (i, (p, hdu)) in frames.iter().enumerate() {
                let mut f = FitsFile::open(p)?;
                let mut rows = f
                    .read_rows(*hdu, row0, nrows)
                    .with_context(|| format!("reading {}", p.display()))?;
                if kind == FrameKind::Flat {
                    let inv = 1.0 / medians[i];
                    rows.iter_mut().for_each(|v| *v *= inv);
                }
                inputs.push(rows);
            }
            let mut scratch = Vec::with_capacity(n);
            let mut rej = 0;
            for (px, dst) in chunk.iter_mut().enumerate() {
                scratch.clear();
                scratch.extend(inputs.iter().map(|r| r[px]).filter(|v| v.is_finite()));
                let (m, r) = clipped_mean(&mut scratch, kappa);
                rej += r;
                *dst = if m.is_finite() { m * scale } else { 0.0 };
            }
            rejected.fetch_add(rej, Ordering::Relaxed);
            let d = done.fetch_add(1, Ordering::Relaxed) + 1;
            progress.update(d, bands, &format!("Stacking {n} frames"));
            if progress.cancelled() {
                bail!("cancelled");
            }
            Ok(())
        })?;
    let pct = rejected.load(Ordering::Relaxed) as f64 * 100.0 / (shape.len() * n) as f64;
    Ok((out, pct))
}

/// Build a master from one calibration session.
pub fn create_from_session(
    conn: &Connection,
    cfg: &Config,
    session_id: &str,
    progress: &dyn Progress,
) -> Result<Master> {
    let s = db::all_sessions(conn)?
        .into_iter()
        .find(|s| s.id == session_id)
        .ok_or_else(|| anyhow!("session {session_id} not found"))?;
    let kind = s
        .object
        .as_deref()
        .and_then(|o| FrameKind::from_master_type(o))
        .ok_or_else(|| anyhow!("session {session_id} is not a calibration session"))?;
    let files = db::files_where(
        conn,
        "fitsFileSession=?1 AND COALESCE(fitsFileSoftDelete,0)=0",
        &[&session_id],
    )?;
    if files.len() < cfg.min_master_files {
        bail!(
            "session has {} frames; at least {} are needed",
            files.len(),
            cfg.min_master_files
        );
    }
    let paths: Vec<PathBuf> = files
        .iter()
        .map(|f| PathBuf::from(&f.name))
        .filter(|p| p.exists())
        .collect();
    if paths.len() < cfg.min_master_files {
        bail!("only {} of the session's files exist on disk", paths.len());
    }

    let stamp = s
        .date
        .as_deref()
        .map(|d| {
            d.chars()
                .filter(|c| c.is_ascii_digit())
                .take(8)
                .collect::<String>()
        })
        .filter(|d| d.len() == 8)
        .unwrap_or_else(|| chrono::Local::now().format("%Y%m%d").to_string());
    let tel = sanitize(s.telescope.as_deref().unwrap_or("Unknown")).replace('@', "_");
    let cam = sanitize(s.imager.as_deref().unwrap_or("Unknown")).replace('@', "_");
    let exp = s.exposure.clone().unwrap_or_default();
    let bins = format!(
        "{}x{}",
        s.xbin.as_deref().unwrap_or("1"),
        s.ybin.as_deref().unwrap_or("1")
    );
    let temp = s.ccd_temp.clone().unwrap_or_default();
    let type_title = kind.object_name();
    let name = if kind == FrameKind::Flat {
        let filter = sanitize(s.filter.as_deref().unwrap_or("OSC"));
        format!("Master-Flat-{tel}-{cam}-{filter}-{stamp}-{exp}s-{bins}-t{temp}.fits")
    } else {
        format!("Master-{type_title}-{tel}-{cam}-{stamp}-{exp}s-{bins}-t{temp}.fits")
    };
    let out_path = util::unique_path(&cfg.masters_dir().join(name));
    std::fs::create_dir_all(cfg.masters_dir())?;

    let started = std::time::Instant::now();
    let result = stack(&paths, kind, cfg.sigma_clip, progress)?;
    let mut header = result.header;
    header.set("IMAGETYP", Value::Str(format!("Master {type_title}")));
    header.set_with_comment(
        "NCOMBINE",
        Value::Int(result.frames as i64),
        "Number of frames combined",
    );
    header.set_with_comment(
        "CREATOR",
        Value::Str("AstroFiler-rs".into()),
        "Software that created this master",
    );
    header.set(
        "METHOD",
        Value::Str(format!("Sigma-clipped mean (kappa={})", cfg.sigma_clip)),
    );
    header.set(
        "REJECTED",
        Value::Str(format!("{:.3}%", result.rejected_pct)),
    );
    header.set(
        "DATE",
        Value::Str(chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string()),
    );
    header.add_history(&format!(
        "Master {} created from {} files",
        kind.master_type(),
        result.frames
    ));
    let out_type = if kind == FrameKind::Flat {
        OutType::F32
    } else {
        OutType::U16
    };
    fits::write_image(&out_path, &header, result.shape, &result.data, out_type)?;
    log::info!(
        "Created {} in {:.1}s ({} frames, {:.3}% rejected)",
        out_path.display(),
        started.elapsed().as_secs_f64(),
        result.frames,
        result.rejected_pct
    );

    let master = Master {
        master_id: unique_master_id(conn, kind, s.telescope.as_deref().unwrap_or("unknown"))?,
        master_type: kind.master_type().into(),
        path: util::normalize_path(&out_path),
        creation_date: db::now_str(),
        telescope: s.telescope.clone(),
        instrument: s.imager.clone(),
        exposure: matches!(kind, FrameKind::Dark | FrameKind::FlatDark)
            .then(|| s.exposure.clone())
            .flatten(),
        xbin: s.xbin.clone(),
        ybin: s.ybin.clone(),
        ccd_temp: s.ccd_temp.clone(),
        gain: s.gain.clone(),
        offset: s.offset.clone(),
        filter: (kind == FrameKind::Flat)
            .then(|| s.filter.clone())
            .flatten(),
        source_session: Some(s.id.clone()),
        file_count: result.frames as i64,
        file_size: std::fs::metadata(&out_path).ok().map(|m| m.len() as i64),
        hash: Some(util::md5_file(&out_path)?),
        validated: true,
        ..Default::default()
    };
    master.insert(conn)?;
    // Like the original, source frames are soft-deleted once folded into a master.
    conn.execute(
        "UPDATE fitsFile SET fitsFileSoftDelete=1 WHERE fitsFileSession=?1",
        [session_id],
    )?;
    Ok(master)
}

/// Create masters for every calibration session that doesn't have one yet.
pub fn create_missing(
    conn: &Connection,
    cfg: &Config,
    progress: &dyn Progress,
) -> Result<(Vec<Master>, Vec<(String, String)>)> {
    let existing: std::collections::HashSet<String> = db::masters(conn, true)?
        .into_iter()
        .filter_map(|m| m.source_session)
        .collect();
    let order = |o: &str| match o {
        "Bias" => 0,
        "Dark" => 1,
        "FlatDark" => 2,
        _ => 3,
    };
    let mut todo: Vec<_> = db::all_sessions(conn)?
        .into_iter()
        .filter(|s| {
            s.is_calibration()
                && !existing.contains(&s.id)
                && s.file_count as usize >= cfg.min_master_files
        })
        .collect();
    todo.sort_by_key(|s| order(s.object.as_deref().unwrap_or("")));
    let mut created = Vec::new();
    let mut errors = Vec::new();
    for s in todo {
        if progress.cancelled() {
            break;
        }
        match create_from_session(conn, cfg, &s.id, progress) {
            Ok(m) => created.push(m),
            Err(e) => errors.push((s.id.clone(), format!("{e:#}"))),
        }
    }
    Ok((created, errors))
}

#[derive(Debug, Default)]
pub struct ValidationReport {
    pub ok: usize,
    pub missing: Vec<String>,
    pub corrupt: Vec<String>,
}

/// Check every master's file exists and matches its stored MD5 (in parallel).
pub fn validate(conn: &Connection, progress: &dyn Progress) -> Result<ValidationReport> {
    let list = db::masters(conn, false)?;
    let done = AtomicUsize::new(0);
    let total = list.len();
    let results: Vec<(i64, String, Result<bool>)> = list
        .par_iter()
        .map(|m| {
            let p = Path::new(&m.path);
            let r = if !p.exists() {
                Ok(false)
            } else {
                match &m.hash {
                    Some(h) => util::md5_file(p).map(|x| &x == h),
                    None => Ok(true),
                }
            };
            let d = done.fetch_add(1, Ordering::Relaxed) + 1;
            progress.update(d, total, &m.master_id);
            (m.id, m.path.clone(), r)
        })
        .collect();
    let mut report = ValidationReport::default();
    for (id, path, r) in results {
        match r {
            Ok(true) => {
                conn.execute(
                    "UPDATE Masters SET is_validated=1, validation_date=?1 WHERE id=?2",
                    params![db::now_str(), id],
                )?;
                report.ok += 1;
            }
            Ok(false) if !Path::new(&path).exists() => report.missing.push(path),
            _ => {
                conn.execute("UPDATE Masters SET is_validated=0 WHERE id=?1", [id])?;
                report.corrupt.push(path);
            }
        }
    }
    Ok(report)
}

/// Soft-delete masters whose files no longer exist.
pub fn cleanup_missing(conn: &Connection) -> Result<Vec<String>> {
    let mut removed = Vec::new();
    for m in db::masters(conn, false)? {
        if !Path::new(&m.path).exists() {
            conn.execute("UPDATE Masters SET soft_delete=1 WHERE id=?1", [m.id])?;
            removed.push(m.master_id);
        }
    }
    Ok(removed)
}

/// Remove a master from the catalogue, optionally deleting its file.
pub fn delete(conn: &Connection, id: i64, delete_file: bool) -> Result<()> {
    if delete_file {
        if let Some(path) = conn
            .query_row("SELECT master_path FROM Masters WHERE id=?1", [id], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
        {
            if Path::new(&path).exists() {
                std::fs::remove_file(&path)?;
            }
        }
    }
    conn.execute("UPDATE Masters SET soft_delete=1 WHERE id=?1", [id])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipping_rejects_outliers() {
        let mut v = vec![10.0, 10.5, 9.5, 10.2, 9.8, 10.1, 9.9, 1000.0];
        let (m, r) = clipped_mean(&mut v, 2.0);
        assert_eq!(r, 1);
        assert!((m - 10.0).abs() < 0.1, "{m}");
    }

    #[test]
    fn builds_master_from_session() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("in");
        std::fs::create_dir_all(&src).unwrap();
        for i in 0..5 {
            crate::ingest::tests::make_frame(
                &src,
                &format!("d{i}.fits"),
                "Dark Frame",
                None,
                &format!("2024-10-02T08:0{i}:00"),
                60.0,
                None,
                i as f32,
            );
        }
        let cfg = Config {
            repo: tmp.path().join("repo"),
            ..Default::default()
        };
        let mut conn = db::open(&tmp.path().join("t.db")).unwrap();
        crate::ingest::ingest_folder(
            &mut conn,
            &cfg,
            &src,
            crate::ingest::IngestOptions::MOVE,
            &crate::progress::NoProgress,
        )
        .unwrap();
        crate::sessions::create_all(&mut conn, &crate::progress::NoProgress).unwrap();
        let (made, errs) = create_missing(&conn, &cfg, &crate::progress::NoProgress).unwrap();
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(made.len(), 1);
        let img = fits::read_image(Path::new(&made[0].path)).unwrap();
        // pixel 0 of frame i is 1000 + i -> mean 1002
        assert_eq!(img.data[0], 1002.0);
        assert_eq!(img.header.get_i64("NCOMBINE"), Some(5));
        let v = validate(&conn, &crate::progress::NoProgress).unwrap();
        assert_eq!(v.ok, 1);
    }
}
