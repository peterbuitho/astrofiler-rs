//! DWARFLAB DWARF (DWARF 3, DWARF II).
//!
//! Wi-Fi: anonymous FTP (192.168.88.1 in hotspot mode).
//! USB-C: shows up as a drive; files may sit under `Astronomy/`.
//!
//! Layout: `DWARF_RAW_<CAM>_<OBJECT>_EXP_<s>_GAIN_<g>_<date>/*.fits` (lights,
//! next to `Thumbnail/`, `stacked*.jpg`, PNGs and `shotsInfo.json`, none of
//! which are transferred), `CALI_FRAME/<bias|dark|flat>/<cam_0|cam_1>/` and
//! `DWARF_DARK/tele_*.fits` calibration libraries.
//!
//! DWARF FITS files have no IMAGETYP, so [`Dwarf::normalize_header`] derives
//! the missing keywords from the folder and file names.

use super::transport::{is_fits, join, FtpTransport, Transport};
use super::{RemoteFile, ScanOptions, Telescope};
use crate::config::Config;
use crate::fits::{Header, Value};
use anyhow::{anyhow, bail, Result};
use std::path::{Component, Path};

pub struct Dwarf;

const ROOTS: &[&str] = &["", "Astronomy", "DWARF3/Astronomy"];

fn is_dwarf_dir(name: &str) -> bool {
    name.starts_with("DWARF_RAW") || name == "CALI_FRAME" || name == "DWARF_DARK"
}

impl Telescope for Dwarf {
    fn id(&self) -> &'static str {
        "dwarf"
    }
    fn name(&self) -> &'static str {
        "DWARF"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["dwarf3", "dwarf 3", "dwarf2", "dwarfii"]
    }
    fn port(&self) -> u16 {
        21
    }
    fn default_host(&self, cfg: &Config) -> String {
        cfg.dwarf_host.clone()
    }
    fn connect(&self, _cfg: &Config, host: &str) -> Result<Box<dyn Transport>> {
        Ok(Box::new(FtpTransport::connect(
            host,
            "anonymous",
            "anonymous@",
        )?))
    }
    /// DWARF has no distinctive host name, so confirm by its folder layout.
    fn identify_network(&self, cfg: &Config, ip: &str, _hostname: &str) -> bool {
        self.connect(cfg, ip)
            .is_ok_and(|mut t| find_root(t.as_mut()).is_some())
    }
    fn detect_usb(&self, root: &Path) -> bool {
        ROOTS.iter().any(|sub| {
            std::fs::read_dir(root.join(sub))
                .map(|rd| {
                    rd.flatten()
                        .any(|e| is_dwarf_dir(&e.file_name().to_string_lossy()))
                })
                .unwrap_or(false)
        })
    }

    fn scan(&self, t: &mut dyn Transport, opts: &ScanOptions) -> Result<Vec<RemoteFile>> {
        let (root, entries) = find_root(t)
            .ok_or_else(|| anyhow!("no DWARF_RAW / CALI_FRAME folders found; is this a DWARF?"))?;
        let rel = |p: &str| {
            p.strip_prefix(root)
                .unwrap_or(p)
                .trim_start_matches('/')
                .to_string()
        };
        let mut files = Vec::new();
        for dir in entries
            .iter()
            .filter(|e| e.is_dir && e.name.starts_with("DWARF_RAW"))
        {
            let dpath = join(root, &dir.name);
            for f in t.list(&dpath)? {
                if f.is_dir || !is_fits(&f.name) || f.name.starts_with("failed_") {
                    continue;
                }
                // stacked-<n>_*.fits is the telescope's own live stack.
                let stacked = f.name.to_lowercase().starts_with("stacked");
                if stacked && !opts.include_stacked {
                    continue;
                }
                let kind = if stacked { "stacked" } else { "light" };
                files.push(RemoteFile {
                    path: join(&dpath, &f.name),
                    name: f.name,
                    size: f.size,
                    folder: dir.name.clone(),
                    local_dir: dir.name.clone(),
                    kind,
                });
            }
        }
        if entries.iter().any(|e| e.is_dir && e.name == "CALI_FRAME") {
            for frame in ["bias", "dark", "flat"] {
                for cam in ["cam_0", "cam_1"] {
                    let dpath = join(root, &format!("CALI_FRAME/{frame}/{cam}"));
                    let Ok(list) = t.list(&dpath) else { continue };
                    for f in list.into_iter().filter(|f| !f.is_dir && is_fits(&f.name)) {
                        files.push(RemoteFile {
                            path: join(&dpath, &f.name),
                            name: f.name,
                            size: f.size,
                            folder: cam.into(),
                            local_dir: rel(&dpath),
                            kind: "master",
                        });
                    }
                }
            }
        }
        if entries.iter().any(|e| e.is_dir && e.name == "DWARF_DARK") {
            let dpath = join(root, "DWARF_DARK");
            for f in t
                .list(&dpath)?
                .into_iter()
                .filter(|f| !f.is_dir && is_fits(&f.name) && f.name.starts_with("tele_"))
            {
                files.push(RemoteFile {
                    path: join(&dpath, &f.name),
                    name: f.name,
                    size: f.size,
                    folder: "DWARF_DARK".into(),
                    local_dir: "DWARF_DARK".into(),
                    kind: "dark library",
                });
            }
        }
        Ok(files)
    }

    /// Frames the telescope itself rejected.
    fn skip_file(&self, path: &Path) -> bool {
        path.file_name().is_some_and(|n| n.to_string_lossy().starts_with("failed_"))
            && path.components().any(|c| matches!(c, Component::Normal(s) if s.to_string_lossy().starts_with("DWARF_RAW")))
    }

    fn claims(&self, h: &Header, path: &Path) -> bool {
        let telescop = h.get_str("TELESCOP").unwrap_or_default().to_uppercase();
        let in_raw = path.components().any(
            |c| matches!(c, Component::Normal(s) if s.to_string_lossy().starts_with("DWARF_RAW")),
        );
        telescop.starts_with("DWARF")
            || (h.get_truthy("IMAGETYP").is_none() && h.get_truthy("FRAME").is_none() && in_raw)
    }

    /// Port of the original `dwarfFixHeader`, plus a fallback for files that
    /// are no longer inside the telescope's folder layout.
    fn normalize_header(&self, h: &mut Header, path: &Path) -> Result<()> {
        let file = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        if file.starts_with("failed_") {
            bail!("ignoring failed DWARF image");
        }
        let stem = Path::new(&file)
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let comps: Vec<String> = path
            .parent()
            .unwrap_or(Path::new(""))
            .components()
            .filter_map(|c| match c {
                Component::Normal(s) => Some(s.to_string_lossy().to_string()),
                _ => None,
            })
            .collect();
        let f = |s: &str| s.parse::<f64>().ok();
        let i = |s: &str| s.parse::<i64>().ok();
        let now = || chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();

        // DWARF 3 records the sensor temperature as DET-TEMP.
        if !h.contains("CCD-TEMP") {
            if let Some(t) = h.get("DET-TEMP").cloned() {
                h.set("CCD-TEMP", t);
            }
        }

        if let Some(folder) = comps.iter().rev().find(|c| c.starts_with("DWARF_RAW")) {
            // DWARF_RAW_<INSTRUMENT>_<OBJECT>_EXP_<EXPTIME>_GAIN_<GAIN>_<DATE-OBS>
            let parts: Vec<&str> = folder.split('_').collect();
            if parts.len() < 8 {
                bail!("unrecognised DWARF_RAW folder name '{folder}'");
            }
            h.set("INSTRUME", Value::Str(parts[2].into()));
            h.set("OBJECT", Value::Str(parts[3].into()));
            // A stacked result's EXPTIME is the total integration; keep it.
            if !(h.contains("STACKCNT") && h.contains("EXPTIME")) {
                h.set(
                    "EXPTIME",
                    Value::Float(
                        f(parts[5]).ok_or_else(|| anyhow!("bad DWARF exposure in '{folder}'"))?,
                    ),
                );
            }
            h.set(
                "GAIN",
                Value::Float(f(parts[7]).ok_or_else(|| anyhow!("bad DWARF gain in '{folder}'"))?),
            );
            for (k, v) in [
                ("XBINNING", Value::Int(1)),
                ("YBINNING", Value::Int(1)),
                ("CCD-TEMP", Value::Float(-10.0)),
            ] {
                if !h.contains(k) {
                    h.set(k, v);
                }
            }
            if !h.contains("DATE-OBS") && parts.len() > 8 {
                h.set("DATE-OBS", Value::Str(parts[8..].join("_")));
            }
            h.set("IMAGETYP", Value::Str("LIGHT".into()));
            if !h.contains("TELESCOP") {
                h.set("TELESCOP", Value::Str("DWARF".into()));
            }
        } else if let Some(idx) = comps.iter().position(|c| c == "CALI_FRAME") {
            let (Some(frame), Some(cam)) = (comps.get(idx + 1), comps.get(idx + 2)) else {
                bail!("unrecognised DWARF CALI_FRAME layout");
            };
            h.set("IMAGETYP", Value::Str(frame.to_uppercase()));
            h.set(
                "OBJECT",
                Value::Str(format!("MASTER{}", frame.to_uppercase())),
            );
            if !h.contains("DATE-OBS") {
                h.set("DATE-OBS", Value::Str(now()));
            }
            match cam.as_str() {
                "cam_0" => h.set("INSTRUME", Value::Str("TELE".into())),
                "cam_1" => h.set("INSTRUME", Value::Str("WIDE".into())),
                _ => {}
            }
            let parts: Vec<&str> = stem.split('_').collect();
            match frame.to_lowercase().as_str() {
                "bias" | "flat" if parts.len() >= 5 && parts[1] == "gain" => {
                    if let (Some(g), Some(b)) = (f(parts[2]), i(parts[4])) {
                        h.set("GAIN", Value::Float(g));
                        h.set("XBINNING", Value::Int(b));
                        h.set("YBINNING", Value::Int(b));
                    }
                    if !h.contains("EXPTIME") {
                        h.set(
                            "EXPTIME",
                            Value::Float(if frame.eq_ignore_ascii_case("bias") {
                                0.0
                            } else {
                                1.0
                            }),
                        );
                    }
                    if !h.contains("CCD-TEMP") {
                        h.set("CCD-TEMP", Value::Float(-10.0));
                    }
                    if frame.eq_ignore_ascii_case("flat") && !h.contains("FILTER") {
                        h.set("FILTER", Value::Str("UNKNOWN".into()));
                    }
                }
                "dark" if parts.len() >= 8 && parts[1] == "exp" => {
                    if let (Some(e), Some(g), Some(b), Some(t)) =
                        (f(parts[2]), f(parts[4]), i(parts[6]), f(parts[7]))
                    {
                        h.set("EXPTIME", Value::Float(e));
                        h.set("GAIN", Value::Float(g));
                        h.set("XBINNING", Value::Int(b));
                        h.set("YBINNING", Value::Int(b));
                        h.set("CCD-TEMP", Value::Float(t));
                    }
                }
                _ => {}
            }
        } else if comps.iter().any(|c| c == "DWARF_DARK") && stem.starts_with("tele_exp_") {
            let parts: Vec<&str> = stem.split('_').collect();
            if parts.len() >= 7 {
                h.set("INSTRUME", Value::Str("TELE".into()));
                h.set("IMAGETYP", Value::Str("DARKMASTER".into()));
                h.set("OBJECT", Value::Str("DARKMASTER".into()));
                if let (Some(e), Some(g), Some(b)) = (f(parts[2]), f(parts[4]), i(parts[6])) {
                    h.set("EXPTIME", Value::Float(e));
                    h.set("GAIN", Value::Float(g));
                    h.set("XBINNING", Value::Int(b));
                    h.set("YBINNING", Value::Int(b));
                }
                if !h.contains("DATE-OBS") {
                    h.set(
                        "DATE-OBS",
                        Value::Str(if parts.len() > 7 {
                            parts[7..].join("_")
                        } else {
                            now()
                        }),
                    );
                }
            }
        } else {
            // Outside the telescope's layout (e.g. already filed in the
            // repository): DWARF raw FITS carry no IMAGETYP but are lights.
            if h.get_truthy("IMAGETYP").is_none() && h.get_truthy("FRAME").is_none() {
                h.set("IMAGETYP", Value::Str("LIGHT".into()));
            }
            if let Some(cam) = h.get_truthy("CAMERA") {
                h.set("INSTRUME", Value::Str(cam.trim().to_string()));
            }
        }
        Ok(())
    }
}

fn find_root(t: &mut dyn Transport) -> Option<(&'static str, Vec<super::Entry>)> {
    ROOTS.iter().find_map(|r| {
        let entries = t.list(r).ok()?;
        entries
            .iter()
            .any(|e| e.is_dir && is_dwarf_dir(&e.name))
            .then_some((*r, entries))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Header as written by DWARF 3 firmware 1.5 (from a real file).
    fn dwarf3_header() -> Header {
        let mut h = Header::default();
        h.set("DATE-OBS", Value::Str("2026-09-27T01:47:46.616".into()));
        h.set("EXPTIME", Value::Float(15.0));
        h.set("GAIN", Value::Int(60));
        h.set("FILTER", Value::Str("Astro".into()));
        h.set("CAMERA", Value::Str("TELE".into()));
        h.set("DET-TEMP", Value::Int(25));
        h.set("OBJECT", Value::Str("M 39".into()));
        h.set("TELESCOP", Value::Str("DWARF 3".into()));
        h.set("INSTRUME", Value::Str("DWARF 3".into()));
        h
    }

    #[test]
    fn raw_folder() {
        let mut h = dwarf3_header();
        let p = Path::new("/x/DWARF_RAW_TELE_M 39_EXP_15_GAIN_60_2026-09-27-01-46-25-216/M 39_15s60_Astro_20260927-014746616_25C.fits");
        assert!(Dwarf.claims(&h, p));
        Dwarf.normalize_header(&mut h, p).unwrap();
        assert_eq!(h.get_str("OBJECT").as_deref(), Some("M 39"));
        assert_eq!(h.get_str("IMAGETYP").as_deref(), Some("LIGHT"));
        assert_eq!(h.get_str("INSTRUME").as_deref(), Some("TELE"));
        assert_eq!(h.get_i64("CCD-TEMP"), Some(25));
    }

    #[test]
    fn stacked_keeps_total_exposure() {
        let mut h = dwarf3_header();
        h.set("EXPTIME", Value::Float(2895.0));
        h.set("STACKCNT", Value::Int(193));
        let p = Path::new("/x/DWARF_RAW_TELE_M 52_EXP_15_GAIN_60_2026-09-27-03-55-25-891/stacked-16_M 52_15s60_Astro_20260927-035629162.fits");
        Dwarf.normalize_header(&mut h, p).unwrap();
        assert_eq!(h.get_f64("EXPTIME"), Some(2895.0));
        assert!(Dwarf.skip_file(Path::new(
            "/x/DWARF_RAW_TELE_M 52_EXP_15_GAIN_60_x/failed_1.fits"
        )));
        assert!(!Dwarf.skip_file(Path::new("/x/other/failed_1.fits")));
    }

    #[test]
    fn filed_outside_layout() {
        let mut h = dwarf3_header();
        let p = Path::new("/repo/Light/M_39/DWARF_3/TELE/20260927/M_39-DWARF_3-TELE-Astro-20260927014746-15.0s-1x1-t25.fits");
        assert!(Dwarf.claims(&h, p));
        Dwarf.normalize_header(&mut h, p).unwrap();
        assert_eq!(h.get_str("IMAGETYP").as_deref(), Some("LIGHT"));
        assert_eq!(h.get_str("INSTRUME").as_deref(), Some("TELE"));
    }
}
