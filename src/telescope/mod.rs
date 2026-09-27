//! Smart telescope import, over Wi-Fi or USB-C.
//!
//! Each telescope model is a module implementing [`Telescope`] and listed in
//! [`all`]. The shared pipeline here does the rest: connect → scan → download
//! into the source folder → header fixes → import into the repository →
//! optional delete on the telescope. Only FITS files are ever transferred;
//! the JPG/PNG previews telescopes write for their app album are skipped.
//!
//! ## Adding a telescope
//!
//! 1. Create `src/telescope/<model>.rs` with a unit struct implementing
//!    [`Telescope`]: how to connect over Wi-Fi (usually by returning one of the
//!    ready-made transports in [`transport`]), how to recognise its USB drive,
//!    where its FITS files live ([`Telescope::scan`]) and, if its headers need
//!    help, [`Telescope::claims`] + [`Telescope::normalize_header`].
//! 2. Add `mod <model>;` below and the struct to [`all`].
//!
//! The CLI, GUI, discovery and import pipeline pick it up automatically.

mod dwarf;
mod seestar;
pub mod transport;

use crate::config::Config;
use crate::fits::Header;
use crate::ingest::{self, IngestReport};
use crate::progress::Progress;
use anyhow::{anyhow, bail, Result};
use rayon::prelude::*;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::time::Duration;
pub use transport::{Entry, Transport};

/// Every supported telescope model.
pub fn all() -> &'static [&'static dyn Telescope] {
    &[&seestar::Seestar, &dwarf::Dwarf]
}

/// Look up a telescope by id or alias (case-insensitive).
pub fn find(name: &str) -> Option<&'static dyn Telescope> {
    let n = name.trim().to_lowercase();
    all()
        .iter()
        .copied()
        .find(|t| t.id() == n || t.aliases().iter().any(|a| *a == n))
}

pub struct ScanOptions {
    /// Also include stacked results the telescope produced (where supported).
    pub include_stacked: bool,
}

/// One telescope model.
pub trait Telescope: Sync {
    /// Short lowercase id used on the command line and in config keys.
    fn id(&self) -> &'static str;
    /// Display name.
    fn name(&self) -> &'static str;
    /// Other accepted names for [`find`].
    fn aliases(&self) -> &'static [&'static str] {
        &[]
    }
    /// TCP port probed during network discovery.
    fn port(&self) -> u16;
    /// Wi-Fi host name or IP to try first.
    fn default_host(&self, cfg: &Config) -> String;
    /// Open a Wi-Fi connection.
    fn connect(&self, cfg: &Config, host: &str) -> Result<Box<dyn Transport>>;
    /// During a network scan: is the host with this open port one of ours?
    fn identify_network(&self, cfg: &Config, ip: &str, hostname: &str) -> bool;
    /// Does this mounted drive / folder hold this telescope's storage?
    fn detect_usb(&self, root: &Path) -> bool;
    /// List importable FITS files.
    fn scan(&self, t: &mut dyn Transport, opts: &ScanOptions) -> Result<Vec<RemoteFile>>;
    /// Adjust a freshly downloaded file (e.g. write metadata only present in
    /// folder names into the header).
    fn after_download(&self, _local: &Path, _file: &RemoteFile) -> Result<()> {
        Ok(())
    }
    /// During ingest: should this file be skipped (e.g. frames the telescope
    /// itself marked as failed)?
    fn skip_file(&self, _path: &Path) -> bool {
        false
    }
    /// During ingest: is this file from this telescope and in need of
    /// [`Telescope::normalize_header`]?
    fn claims(&self, _h: &Header, _path: &Path) -> bool {
        false
    }
    /// Fill in / correct header keywords before the file is filed.
    fn normalize_header(&self, _h: &mut Header, _path: &Path) -> Result<()> {
        Ok(())
    }
}

/// Whether any telescope module wants this file skipped on import.
pub fn skip_file(path: &Path) -> bool {
    all().iter().any(|t| t.skip_file(path))
}

/// Apply telescope-specific header fixes during ingest. Returns whether any
/// telescope handled the file.
pub fn normalize_header(h: &mut Header, path: &Path) -> Result<bool> {
    for t in all() {
        if t.claims(h, path) {
            t.normalize_header(h, path)?;
            return Ok(true);
        }
    }
    Ok(false)
}

/// How to reach the telescope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// Wi-Fi: host name or IP address.
    Network(String),
    /// USB-C: folder where the telescope's storage is mounted.
    Usb(PathBuf),
}

impl std::fmt::Display for Link {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Link::Network(h) => write!(f, "Wi-Fi {h}"),
            Link::Usb(p) => write!(f, "USB {}", p.display()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RemoteFile {
    /// Path relative to the transport root, `/`-separated.
    pub path: String,
    pub name: String,
    pub size: u64,
    /// Folder the file lives in (often carries the target name).
    pub folder: String,
    /// Relative directory recreated locally, so folder-based header fixes still work.
    pub local_dir: String,
    /// "light", "stacked", "master", ...
    pub kind: &'static str,
}

pub struct Session {
    pub telescope: &'static dyn Telescope,
    pub link: Link,
    transport: Box<dyn Transport>,
}

pub fn connect(cfg: &Config, telescope: &'static dyn Telescope, link: &Link) -> Result<Session> {
    let transport: Box<dyn Transport> = match link {
        Link::Usb(root) => {
            if !root.is_dir() {
                bail!("{} is not a folder", root.display());
            }
            Box::new(transport::LocalTransport { root: root.clone() })
        }
        Link::Network(host) => telescope.connect(cfg, host)?,
    };
    Ok(Session {
        telescope,
        link: link.clone(),
        transport,
    })
}

impl Session {
    pub fn scan(&mut self, include_stacked: bool) -> Result<Vec<RemoteFile>> {
        self.telescope
            .scan(self.transport.as_mut(), &ScanOptions { include_stacked })
    }

    /// Download `files` into `dest`, import them into the repository and, if
    /// requested, delete the originals from the telescope once catalogued
    /// (or already present in the catalogue).
    pub fn import(
        &mut self,
        conn: &mut rusqlite::Connection,
        cfg: &Config,
        files: &[RemoteFile],
        dest: &Path,
        delete_on_scope: bool,
        progress: &dyn Progress,
    ) -> Result<ImportReport> {
        let mut report = ImportReport::default();
        let mut downloaded: Vec<(RemoteFile, PathBuf)> = Vec::new();
        let total_bytes = files.iter().map(|f| f.size).sum::<u64>().max(1);
        let mut bytes = 0u64;
        for (i, f) in files.iter().enumerate() {
            if progress.cancelled() {
                break;
            }
            progress.update(
                i,
                files.len(),
                &format!(
                    "Downloading {} ({:.0}%)",
                    f.name,
                    bytes as f64 * 100.0 / total_bytes as f64
                ),
            );
            let dir = dest.join(&f.local_dir);
            let local = dir.join(&f.name);
            let fetched = std::fs::create_dir_all(&dir)
                .map_err(anyhow::Error::from)
                .and_then(|_| {
                    let tmp = dir.join(format!("{}.part", f.name));
                    self.transport.fetch(&f.path, &tmp)?;
                    std::fs::rename(&tmp, &local)?;
                    Ok(())
                });
            match fetched {
                Ok(()) => {
                    bytes += f.size;
                    if let Err(e) = self.telescope.after_download(&local, f) {
                        log::warn!("{}: {e:#}", local.display());
                    }
                    downloaded.push((f.clone(), local));
                }
                Err(e) => report.failed.push((f.path.clone(), format!("{e:#}"))),
            }
        }
        report.downloaded = downloaded.len();

        progress.update(0, downloaded.len(), "Importing into repository...");
        let paths: Vec<PathBuf> = downloaded.iter().map(|(_, p)| p.clone()).collect();
        let ingest = ingest::ingest_files(
            conn,
            cfg,
            paths,
            ingest::IngestOptions::MOVE.with_conflict(cfg.on_conflict),
            progress,
        )?;

        if delete_on_scope {
            let safe: std::collections::HashSet<&Path> = ingest
                .placed
                .iter()
                .map(|(input, _)| input.as_path())
                .chain(ingest.duplicates.iter().map(|(p, _)| p.as_path()))
                .collect();
            for (i, (remote, local)) in downloaded.iter().enumerate() {
                progress.update(
                    i + 1,
                    downloaded.len(),
                    &format!("Deleting {} from telescope", remote.name),
                );
                if !safe.contains(local.as_path()) {
                    continue; // not catalogued: keep it on the telescope
                }
                match self.transport.delete(&remote.path) {
                    Ok(()) => report.deleted += 1,
                    Err(e) => report
                        .failed
                        .push((remote.path.clone(), format!("delete: {e:#}"))),
                }
            }
        }
        report.ingest = ingest;
        Ok(report)
    }
}

#[derive(Debug, Default)]
pub struct ImportReport {
    pub downloaded: usize,
    pub deleted: usize,
    pub failed: Vec<(String, String)>,
    pub ingest: IngestReport,
}

impl ImportReport {
    pub fn summary(&self) -> String {
        format!(
            "{} downloaded, {}; {} deleted from telescope, {} failed",
            self.downloaded,
            self.ingest.summary(),
            self.deleted,
            self.failed.len()
        )
    }
}

// ---------------------------------------------------------------- discovery

#[derive(Clone)]
pub struct Found {
    pub telescope: &'static dyn Telescope,
    pub link: Link,
    pub label: String,
}

/// Mounted drives that look like a supported telescope connected over USB-C.
pub fn find_usb() -> Vec<Found> {
    let mut found = Vec::new();
    for root in mount_roots() {
        for t in all() {
            if t.detect_usb(&root) {
                found.push(Found {
                    telescope: *t,
                    link: Link::Usb(root.clone()),
                    label: format!("{} on {}", t.name(), root.display()),
                });
            }
        }
    }
    found
}

fn mount_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    #[cfg(target_os = "windows")]
    for letter in b'D'..=b'Z' {
        let p = PathBuf::from(format!("{}:\\", letter as char));
        if p.exists() {
            roots.push(p);
        }
    }
    #[cfg(target_os = "macos")]
    if let Ok(rd) = std::fs::read_dir("/Volumes") {
        roots.extend(rd.flatten().map(|e| e.path()));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let user = std::env::var("USER").unwrap_or_default();
        for base in [
            format!("/media/{user}"),
            format!("/run/media/{user}"),
            "/media".into(),
            "/mnt".into(),
        ] {
            if let Ok(rd) = std::fs::read_dir(&base) {
                roots.extend(rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()));
            }
        }
    }
    roots
}

fn port_open(ip: IpAddr, port: u16, timeout: Duration) -> bool {
    TcpStream::connect_timeout(&SocketAddr::new(ip, port), timeout).is_ok()
}

fn local_ipv4() -> Option<Ipv4Addr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:80").ok()?;
    match s.local_addr().ok()?.ip() {
        IpAddr::V4(v4) => Some(v4),
        _ => None,
    }
}

/// Look for a telescope on the network: its default host first, then a
/// parallel scan of the local /24 for its port.
pub fn find_network(
    cfg: &Config,
    t: &'static dyn Telescope,
    progress: &dyn Progress,
) -> Vec<Found> {
    let host = t.default_host(cfg);
    progress.update(0, 0, &format!("Trying {host}..."));
    if let Ok(addr) = transport::resolve(&host, t.port()) {
        if port_open(addr.ip(), t.port(), Duration::from_secs(2)) {
            return vec![Found {
                telescope: t,
                link: Link::Network(host.clone()),
                label: format!("{} at {host}", t.name()),
            }];
        }
    }
    let Some(me) = local_ipv4() else {
        return vec![];
    };
    let [a, b, c, _] = me.octets();
    progress.update(
        0,
        0,
        &format!("Scanning {a}.{b}.{c}.0/24 for {}...", t.name()),
    );
    let pool = match rayon::ThreadPoolBuilder::new().num_threads(96).build() {
        Ok(p) => p,
        Err(_) => return vec![],
    };
    let hits: Vec<IpAddr> = pool.install(|| {
        (1u8..=254)
            .into_par_iter()
            .map(|d| IpAddr::V4(Ipv4Addr::new(a, b, c, d)))
            .filter(|ip| {
                *ip != IpAddr::V4(me) && port_open(*ip, t.port(), Duration::from_millis(600))
            })
            .collect()
    });
    hits.into_iter()
        .filter_map(|ip| {
            let hostname = dns_lookup::lookup_addr(&ip).unwrap_or_default();
            t.identify_network(cfg, &ip.to_string(), &hostname)
                .then(|| {
                    let label = if hostname.is_empty() {
                        ip.to_string()
                    } else {
                        format!("{hostname} ({ip})")
                    };
                    Found {
                        telescope: t,
                        link: Link::Network(ip.to_string()),
                        label: format!("{} at {label}", t.name()),
                    }
                })
        })
        .collect()
}

/// Helper for modules: first candidate directory that exists on the transport.
pub(crate) fn first_existing(
    t: &mut dyn Transport,
    candidates: &[&'static str],
) -> Result<&'static str> {
    candidates
        .iter()
        .copied()
        .find(|c| t.exists(c))
        .ok_or_else(|| anyhow!("none of {candidates:?} found"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fits::{self, Value};
    use crate::ingest::tests::make_frame;
    use crate::progress::NoProgress;

    #[test]
    fn registry_lookup() {
        assert_eq!(find("Seestar").unwrap().id(), "seestar");
        assert_eq!(find("dwarf3").unwrap().id(), "dwarf");
        assert!(find("nope").is_none());
    }

    #[test]
    fn usb_seestar_import_with_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let usb = tmp.path().join("usb");
        let sub = usb.join("MyWorks/NGC 7000_sub");
        std::fs::create_dir_all(&sub).unwrap();
        let stacked_dir = usb.join("MyWorks/NGC 7000");
        std::fs::create_dir_all(&stacked_dir).unwrap();
        std::fs::write(
            stacked_dir.join("Stacked_2_NGC 7000_10.0s_IRCUT_20240801-221000.jpg"),
            b"jpg",
        )
        .unwrap();
        std::fs::write(
            stacked_dir.join("Stacked_2_NGC 7000_10.0s_IRCUT_20240801-221000_thn.jpg"),
            b"jpg",
        )
        .unwrap();
        std::fs::write(sub.join("Light_1.jpg"), b"jpg").unwrap();
        make_frame(
            &sub,
            "Light_1.fit",
            "Light",
            Some("wrong"),
            "2024-08-01T22:00:00",
            10.0,
            Some("IRCUT"),
            1.0,
        );
        make_frame(
            &sub,
            "Light_2.fit",
            "Light",
            Some("wrong"),
            "2024-08-01T22:00:10",
            10.0,
            Some("IRCUT"),
            2.0,
        );
        assert!(find("seestar").unwrap().detect_usb(&usb));

        let cfg = Config {
            repo: tmp.path().join("repo"),
            source: tmp.path().join("incoming"),
            ..Default::default()
        };
        let mut conn = crate::db::open(&tmp.path().join("t.db")).unwrap();
        let mut s = connect(&cfg, find("seestar").unwrap(), &Link::Usb(usb.clone())).unwrap();
        let files = s.scan(true).unwrap();
        // 2 subs + the stacked JPG; not Light_1.jpg or the _thn.jpg thumbnail.
        assert_eq!(files.len(), 3, "{files:?}");
        assert!(files
            .iter()
            .any(|f| f.kind == "stacked preview" && !f.name.contains("_thn")));
        let r = s
            .import(&mut conn, &cfg, &files, &cfg.source, true, &NoProgress)
            .unwrap();
        assert_eq!(r.ingest.registered, 2, "{r:?}");
        assert_eq!(r.deleted, 2);
        assert!(!sub.join("Light_1.fit").exists());
        assert!(
            sub.join("Light_1.jpg").exists(),
            "previews stay on the telescope"
        );
        let db_files = crate::db::all_files(&conn, false).unwrap();
        assert!(db_files
            .iter()
            .all(|f| f.object.as_deref() == Some("NGC 7000")));
        let h = fits::read_primary_header(Path::new(&db_files[0].name)).unwrap();
        assert_eq!(h.get("MOSAIC"), Some(&Value::Bool(false)));
    }

    #[test]
    fn usb_dwarf_scan_skips_previews() {
        let tmp = tempfile::tempdir().unwrap();
        let usb = tmp.path().join("usb");
        let raw = usb.join("Astronomy/DWARF_RAW_TELE_M 42_EXP_15_GAIN_80_2024-10-01-21-00-00-000");
        std::fs::create_dir_all(raw.join("Thumbnail")).unwrap();
        std::fs::create_dir_all(usb.join("Astronomy/CALI_FRAME/dark/cam_0")).unwrap();
        make_frame(
            &raw,
            "0001.fits",
            "Light",
            Some("x"),
            "2024-10-01T21:00:00",
            15.0,
            None,
            1.0,
        );
        for f in [
            "failed_0002.fits",
            "Thumbnail/0001.jpg",
            "stacked.jpg",
            "img_reference.png",
            "shotsInfo.json",
        ] {
            std::fs::write(raw.join(f), b"x").unwrap();
        }
        std::fs::write(
            usb.join("Astronomy/CALI_FRAME/dark/cam_0/dark_exp_15_gain_80_bin_1_-10.fits"),
            b"x",
        )
        .unwrap();
        let dwarf = find("dwarf").unwrap();
        assert!(dwarf.detect_usb(&usb));
        let mut s = connect(&Config::default(), dwarf, &Link::Usb(usb)).unwrap();
        let files = s.scan(true).unwrap();
        // 0001.fits, failed_0002.fits, shotsInfo.json, stacked.jpg and the
        // CALI_FRAME master; not the thumbnail or img_reference.png.
        assert_eq!(files.len(), 5, "{files:?}");
        assert!(files.iter().any(|f| f.kind == "session info"));
        let master = files.iter().find(|f| f.kind == "master").unwrap();
        assert_eq!(master.local_dir, "CALI_FRAME/dark/cam_0");
    }
}
