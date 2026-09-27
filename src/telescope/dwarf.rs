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
                // shotsInfo.json: the session summary (target, exposure, frames, temperatures).
                let info = crate::util::is_sidecar(std::path::Path::new(&f.name));
                if f.is_dir || !(is_fits(&f.name) || info) {
                    continue;
                }
                // stacked-<n>_*.fits is the telescope's own live stack.
                let stacked = f.name.to_lowercase().starts_with("stacked");
                if stacked && !opts.include_stacked {
                    continue;
                }
                let kind = if info {
                    "session info"
                } else if stacked {
                    "stacked"
                } else {
                    "light"
                };
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
            // Header values win; the folder name only fills gaps. Folder
            // names vary (manual mode has no EXP, users append "_a" etc.).
            let info = RawFolder::parse(folder);
            let cam = h
                .get_truthy("CAMERA")
                .map(|c| c.trim().to_string())
                .or(info.camera);
            if let Some(cam) = cam {
                h.set("INSTRUME", Value::Str(cam));
            }
            if h.get_truthy("OBJECT").is_none() {
                if let Some(o) = info.object {
                    h.set("OBJECT", Value::Str(o));
                }
            }
            if !h.contains("EXPTIME") && !h.contains("EXPOSURE") {
                if let Some(e) = info.exposure {
                    h.set("EXPTIME", Value::Float(e));
                }
            }
            if !h.contains("GAIN") {
                if let Some(g) = info.gain {
                    h.set("GAIN", Value::Float(g));
                }
            }
            if h.get_truthy("DATE-OBS").is_none() {
                if let Some(d) = info.date {
                    h.set("DATE-OBS", Value::Str(d));
                }
            }
            for (k, v) in [
                ("XBINNING", Value::Int(1)),
                ("YBINNING", Value::Int(1)),
                ("CCD-TEMP", Value::Float(-10.0)),
            ] {
                if !h.contains(k) {
                    h.set(k, v);
                }
            }
            if h.get_truthy("IMAGETYP").is_none() {
                h.set("IMAGETYP", Value::Str("LIGHT".into()));
            }
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

/// What a `DWARF_RAW_<CAM>_<OBJECT>[_Manual][_EXP_<s>][_GAIN_<g>]_<date>[_suffix]`
/// folder name tells us.
#[derive(Debug, Default, PartialEq)]
struct RawFolder {
    camera: Option<String>,
    object: Option<String>,
    exposure: Option<f64>,
    gain: Option<f64>,
    date: Option<String>,
}

impl RawFolder {
    fn parse(name: &str) -> Self {
        let tokens: Vec<&str> = name.split('_').collect();
        let mut info = RawFolder::default();
        if let Some(cam) = tokens
            .get(2)
            .filter(|c| c.eq_ignore_ascii_case("TELE") || c.eq_ignore_ascii_case("WIDE"))
        {
            info.camera = Some(cam.to_uppercase());
        }
        let is_date = |t: &str| {
            t.len() >= 10 && t.as_bytes()[4] == b'-' && t[..4].chars().all(|c| c.is_ascii_digit())
        };
        let is_keyword =
            |t: &str| ["EXP", "GAIN", "MANUAL"].contains(&t.to_uppercase().as_str()) || is_date(t);
        let start = if info.camera.is_some() { 3 } else { 2 };
        let object: Vec<&str> = tokens
            .iter()
            .skip(start)
            .take_while(|t| !is_keyword(t))
            .copied()
            .collect();
        if !object.is_empty() {
            info.object = Some(object.join("_"));
        }
        for (i, t) in tokens.iter().enumerate() {
            let next = tokens.get(i + 1).and_then(|v| v.parse::<f64>().ok());
            match t.to_uppercase().as_str() {
                "EXP" => info.exposure = info.exposure.or(next),
                "GAIN" => info.gain = info.gain.or(next),
                _ if info.date.is_none() && is_date(t) => {
                    // 2026-09-27-01-46-25-216 -> 2026-09-27T01:46:25.216
                    let p: Vec<&str> = t.split('-').collect();
                    info.date = Some(if p.len() >= 6 && p[3..6].iter().all(|x| x.len() == 2) {
                        format!("{}-{}-{}T{}:{}:{}", p[0], p[1], p[2], p[3], p[4], p[5])
                    } else {
                        t[..10].to_string()
                    });
                }
                _ => {}
            }
        }
        info
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
        // Frames the telescope marked failed_ are imported like any other.
        let failed = Path::new("/x/DWARF_RAW_TELE_M 52_EXP_15_GAIN_60_x/failed_1.fits");
        assert!(!Dwarf.skip_file(failed));
        let mut h = dwarf3_header();
        Dwarf.normalize_header(&mut h, failed).unwrap();
        assert_eq!(h.get_str("IMAGETYP").as_deref(), Some("LIGHT"));
    }

    #[test]
    fn folder_name_variants() {
        // Real folder names from a DWARF 3 archive.
        let p = RawFolder::parse("DWARF_RAW_TELE_M 39_EXP_15_GAIN_60_2026-09-27-01-46-25-216");
        assert_eq!(
            (p.object.as_deref(), p.exposure, p.gain),
            (Some("M 39"), Some(15.0), Some(60.0))
        );
        assert_eq!(p.date.as_deref(), Some("2026-09-27T01:46:25"));
        let p = RawFolder::parse("DWARF_RAW_TELE_LDN 935_GAIN_60_2026-08-08");
        assert_eq!(
            (p.object.as_deref(), p.exposure, p.gain),
            (Some("LDN 935"), None, Some(60.0))
        );
        assert_eq!(p.date.as_deref(), Some("2026-08-08"));
        let p = RawFolder::parse("DWARF_RAW_TELE_IC1396_Manual_GAIN_60_2026-08-01-XISF");
        assert_eq!(p.object.as_deref(), Some("IC1396"));
        let p = RawFolder::parse(
            "DWARF_RAW_TELE_NGC6997_57Cyg_Manual_EXP_60_GAIN_60_2026-08-08-00-15-02-876",
        );
        assert_eq!(
            (p.object.as_deref(), p.exposure),
            (Some("NGC6997_57Cyg"), Some(60.0))
        );
        let p =
            RawFolder::parse("DWARF_RAW_TELE_NGC 1491_EXP_60_GAIN_60_2026-03-05-20-33-46-329_a");
        assert_eq!(
            (p.object.as_deref(), p.camera.as_deref()),
            (Some("NGC 1491"), Some("TELE"))
        );

        // Header values take precedence over the folder name.
        let mut h = dwarf3_header();
        Dwarf
            .normalize_header(
                &mut h,
                Path::new("/x/DWARF_RAW_TELE_LDN 935_GAIN_60_2026-08-08/f.fits"),
            )
            .unwrap();
        assert_eq!(h.get_str("OBJECT").as_deref(), Some("M 39"));
        assert_eq!(h.get_f64("EXPTIME"), Some(15.0));
        assert_eq!(h.get_str("IMAGETYP").as_deref(), Some("LIGHT"));
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
