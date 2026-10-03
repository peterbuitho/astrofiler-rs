//! Configuration, stored in an `astrofiler.ini` file compatible with the
//! original Python application (keys live in the `[DEFAULT]` section).

use anyhow::Result;
use ini::Ini;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Config {
    /// Folder new images are loaded from.
    pub source: PathBuf,
    /// Root of the organised repository.
    pub repo: PathBuf,
    /// Write normalised headers back into the FITS files.
    pub save_modified_headers: bool,
    /// Command used to open images externally (e.g. `siril`, `ds9`).
    pub external_viewer: String,
    /// `dark` or `light`.
    pub theme: String,
    /// Explicit database location (optional).
    pub database: Option<PathBuf>,
    /// Seestar Wi-Fi host name / IP and SMB login.
    pub seestar_host: String,
    pub seestar_username: String,
    pub seestar_password: String,
    /// DWARF Wi-Fi host name / IP (FTP, anonymous).
    pub dwarf_host: String,
    /// Also import Seestar `Stacked_*.fit` results.
    pub include_stacked: bool,
    /// Interface size: "auto" or a factor relative to the desktop scaling (e.g. "1.5").
    pub ui_scale: String,
    /// What to do when a different file already has a filed file's name.
    pub on_conflict: crate::ingest::OnConflict,
    /// Common names for objects, added to their folder names; they take
    /// precedence over the built-in list (`[object_names]` section).
    pub object_names: std::collections::BTreeMap<String, String>,
    /// Address of the web version that keeps the catalogue (e.g. on a NAS).
    pub web_url: String,
    /// The web version's incoming folder as this machine sees it (e.g. the
    /// mounted share); finished downloads are moved there.
    pub inbox: PathBuf,
    /// Where this config was loaded from / will be saved to.
    pub path: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            source: PathBuf::from("."),
            repo: PathBuf::from("."),
            save_modified_headers: false,
            external_viewer: String::new(),
            theme: "dark".into(),
            database: None,
            seestar_host: "seestar.local".into(),
            seestar_username: "guest".into(),
            seestar_password: "guest".into(),
            dwarf_host: "192.168.88.1".into(),
            include_stacked: true,
            ui_scale: "auto".into(),
            on_conflict: Default::default(),
            object_names: Default::default(),
            web_url: String::new(),
            inbox: PathBuf::new(),
            path: default_config_path(),
        }
    }
}

fn default_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("astrofiler")
        .join("astrofiler.ini")
}

/// Config lookup order: `$ASTROFILER_CONFIG`, `./astrofiler.ini` (the original
/// app's location), then `~/.config/astrofiler/astrofiler.ini`.
pub fn locate_config() -> PathBuf {
    if let Ok(p) = std::env::var("ASTROFILER_CONFIG") {
        return PathBuf::from(p);
    }
    let local = PathBuf::from("astrofiler.ini");
    if local.exists() {
        return local;
    }
    default_config_path()
}

fn parse_bool(s: &str) -> bool {
    matches!(
        s.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

impl Config {
    pub fn load() -> Result<Self> {
        Self::load_from(&locate_config())
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let mut cfg = Config {
            path: path.to_path_buf(),
            ..Default::default()
        };
        if !path.exists() {
            return Ok(cfg);
        }
        let ini = Ini::load_from_file(path)?;
        let get = |key: &str| -> Option<String> {
            ini.section(Some("DEFAULT"))
                .and_then(|s| s.get(key))
                .or_else(|| ini.general_section().get(key))
                .map(|s| s.trim().to_string())
        };
        if let Some(v) = get("source") {
            cfg.source = PathBuf::from(v);
        }
        if let Some(v) = get("repo") {
            cfg.repo = PathBuf::from(v);
        }
        if let Some(v) = get("save_modified_headers") {
            cfg.save_modified_headers = parse_bool(&v);
        }
        if let Some(v) = get("external_viewer").or_else(|| get("fits_viewer")) {
            cfg.external_viewer = v;
        }
        if let Some(v) = get("theme") {
            cfg.theme = v.to_ascii_lowercase();
        }
        if let Some(v) = get("seestar_host").filter(|v| !v.is_empty()) {
            cfg.seestar_host = v;
        }
        if let Some(v) = get("seestar_username").filter(|v| !v.is_empty()) {
            cfg.seestar_username = v;
        }
        if let Some(v) = get("seestar_password") {
            cfg.seestar_password = v;
        }
        if let Some(v) = get("dwarf_host").filter(|v| !v.is_empty()) {
            cfg.dwarf_host = v;
        }
        if let Some(v) = get("include_stacked") {
            cfg.include_stacked = parse_bool(&v);
        }
        if let Some(v) = get("ui_scale").filter(|v| !v.is_empty()) {
            cfg.ui_scale = v.to_ascii_lowercase();
        }
        if let Some(c) = get("on_conflict").and_then(|v| crate::ingest::OnConflict::parse(&v)) {
            cfg.on_conflict = c;
        }
        if let Some(v) = get("web_url") {
            cfg.web_url = v;
        }
        if let Some(v) = get("inbox") {
            cfg.inbox = PathBuf::from(v);
        }
        if let Some(sec) = ini.section(Some("object_names")) {
            cfg.object_names = sec
                .iter()
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                .filter(|(k, _)| !k.is_empty())
                .collect();
        }
        cfg.database = get("database").filter(|v| !v.is_empty()).map(PathBuf::from);
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        // Preserve keys we don't know about (e.g. ones used by the Python app).
        let mut ini = if self.path.exists() {
            Ini::load_from_file(&self.path)?
        } else {
            Ini::new()
        };
        ini.with_section(Some("DEFAULT"))
            .set("source", self.source.to_string_lossy())
            .set("repo", self.repo.to_string_lossy())
            .set(
                "save_modified_headers",
                if self.save_modified_headers {
                    "True"
                } else {
                    "False"
                },
            )
            .set("ui_scale", self.ui_scale.clone())
            .set("on_conflict", self.on_conflict.key())
            .set("external_viewer", self.external_viewer.clone())
            .set("theme", self.theme.clone())
            .set("seestar_host", self.seestar_host.clone())
            .set("seestar_username", self.seestar_username.clone())
            .set("seestar_password", self.seestar_password.clone())
            .set("dwarf_host", self.dwarf_host.clone())
            .set("web_url", self.web_url.clone())
            .set("inbox", self.inbox.to_string_lossy())
            .set(
                "include_stacked",
                if self.include_stacked {
                    "True"
                } else {
                    "False"
                },
            )
            .set(
                "database",
                self.database
                    .as_ref()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default(),
            );
        ini.delete(Some("object_names"));
        for (object, name) in &self.object_names {
            ini.with_section(Some("object_names"))
                .set(object.clone(), name.clone());
        }
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        ini.write_to_file(&self.path)?;
        Ok(())
    }

    /// Database lookup order: `$ASTROFILER_DB_PATH`, the `database` key, an
    /// `astrofiler.db` next to the config file, then the user data directory.
    pub fn database_path(&self) -> PathBuf {
        if let Ok(p) = std::env::var("ASTROFILER_DB_PATH") {
            return PathBuf::from(p);
        }
        if let Some(p) = &self.database {
            return p.clone();
        }
        let beside = self
            .path
            .parent()
            .map(|p| p.join("astrofiler.db"))
            .unwrap_or_else(|| "astrofiler.db".into());
        if beside.exists() || self.path == Path::new("astrofiler.ini") {
            return beside;
        }
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("astrofiler")
            .join("astrofiler.db")
    }
}
