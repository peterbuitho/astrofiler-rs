//! ZWO Seestar (S50 / S30).
//!
//! Wi-Fi: SMB share `EMMC Images` (guest/guest by default).
//! USB-C: shows up as a drive with the same `MyWorks` folder.
//!
//! Layout: `MyWorks/<target>_sub/Light_*.fit` (sub-frames),
//! `MyWorks/<target>_mosaic_sub/` (mosaic panels) and
//! `MyWorks/<target>/Stacked_*.fit` (the telescope's own stacks), each next to
//! JPG previews that are never transferred.

use super::transport::{is_fits, join, SmbTransport, Transport};
use super::{first_existing, RemoteFile, ScanOptions, Telescope};
use crate::config::Config;
use crate::fits::{self, Value};
use anyhow::Result;
use std::path::Path;

pub struct Seestar;

const ROOTS: &[&str] = &["MyWorks", "EMMC Images/MyWorks"];

impl Telescope for Seestar {
    fn id(&self) -> &'static str {
        "seestar"
    }
    fn name(&self) -> &'static str {
        "Seestar"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["s50", "s30", "zwo"]
    }
    fn port(&self) -> u16 {
        445
    }
    fn default_host(&self, cfg: &Config) -> String {
        cfg.seestar_host.clone()
    }
    fn connect(&self, cfg: &Config, host: &str) -> Result<Box<dyn Transport>> {
        Ok(Box::new(SmbTransport::connect(
            host,
            &cfg.seestar_username,
            &cfg.seestar_password,
            "EMMC Images",
        )?))
    }
    fn identify_network(&self, _cfg: &Config, _ip: &str, hostname: &str) -> bool {
        hostname.to_lowercase().contains("seestar")
    }
    fn detect_usb(&self, root: &Path) -> bool {
        ROOTS.iter().any(|r| root.join(r).is_dir())
    }

    fn scan(&self, t: &mut dyn Transport, opts: &ScanOptions) -> Result<Vec<RemoteFile>> {
        let base = first_existing(t, ROOTS)
            .map_err(|_| anyhow::anyhow!("no MyWorks folder found; is this a Seestar?"))?;
        let mut files = Vec::new();
        for dir in t.list(base)?.into_iter().filter(|e| e.is_dir) {
            let subs = dir.name.ends_with("_sub");
            if !subs && !opts.include_stacked {
                continue;
            }
            let folder_path = join(base, &dir.name);
            for f in t.list(&folder_path)? {
                // The target folder holds Stacked_*.fit plus its JPG render
                // (kept) and a *_thn.jpg thumbnail (skipped).
                let preview = !subs && crate::util::is_stack_preview_name(&f.name);
                let stacked_fits = !subs && is_fits(&f.name) && f.name.starts_with("Stacked_");
                if f.is_dir || !(preview || stacked_fits || (subs && is_fits(&f.name))) {
                    continue;
                }
                let kind = if preview {
                    "stacked preview"
                } else if subs {
                    "light"
                } else {
                    "stacked"
                };
                files.push(RemoteFile {
                    path: join(&folder_path, &f.name),
                    name: f.name,
                    size: f.size,
                    folder: dir.name.clone(),
                    local_dir: dir.name.clone(),
                    kind,
                });
            }
        }
        Ok(files)
    }

    /// The folder name is the target the user picked in the app; write it into
    /// OBJECT and flag mosaic panels.
    fn after_download(&self, local: &Path, file: &RemoteFile) -> Result<()> {
        let (object, mosaic) = if let Some(o) = file.folder.strip_suffix("_mosaic_sub") {
            (o, true)
        } else if let Some(o) = file.folder.strip_suffix("_sub") {
            (o, false)
        } else {
            return Ok(());
        };
        let mut h = fits::read_primary_header(local)?;
        if h.get_str("OBJECT").as_deref() == Some(object)
            && h.get("MOSAIC") == Some(&Value::Bool(mosaic))
        {
            return Ok(());
        }
        h.set("OBJECT", Value::Str(object.to_string()));
        h.set_with_comment("MOSAIC", Value::Bool(mosaic), "Part of a Seestar mosaic");
        fits::rewrite_primary_header(local, &h)
    }
}
