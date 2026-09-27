//! Small helpers shared across modules (ports of `core/utils.py`).

use anyhow::Result;
use std::path::{Path, PathBuf};

/// Normalise a path to forward slashes, as the original stores paths.
pub fn normalize_path(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// Replace characters that are invalid in file names with underscores.
pub fn sanitize(name: &str) -> String {
    let mut s: String = name
        .trim()
        .chars()
        .map(|c| {
            if " \\/:*?\"<>|\t\n\r".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    while s.contains("__") {
        s = s.replace("__", "_");
    }
    let s = s.trim_matches('_').to_string();
    if s.is_empty() {
        "Unknown".into()
    } else {
        s
    }
}

pub fn normalize_image_type(t: &str) -> String {
    t.chars()
        .filter(|c| c.is_alphanumeric())
        .collect::<String>()
        .to_uppercase()
}

pub fn is_flat_dark(t: &str) -> bool {
    let n = normalize_image_type(t);
    n.contains("FLATDARK") || n.contains("DARKFLAT")
}

pub fn is_dark(t: &str) -> bool {
    normalize_image_type(t).contains("DARK") && !is_flat_dark(t)
}

pub fn is_flat(t: &str) -> bool {
    normalize_image_type(t).contains("FLAT") && !is_flat_dark(t)
}

pub fn is_bias(t: &str) -> bool {
    normalize_image_type(t).contains("BIAS")
}

pub fn is_light(t: &str) -> bool {
    normalize_image_type(t).contains("LIGHT")
}

/// Calibration frame class used for repository folders and master types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrameKind {
    Light,
    Bias,
    Dark,
    Flat,
    FlatDark,
}

impl FrameKind {
    pub fn classify(image_type: &str) -> Option<Self> {
        if is_light(image_type) {
            Some(Self::Light)
        } else if is_flat_dark(image_type) {
            Some(Self::FlatDark)
        } else if is_dark(image_type) {
            Some(Self::Dark)
        } else if is_flat(image_type) {
            Some(Self::Flat)
        } else if is_bias(image_type) {
            Some(Self::Bias)
        } else {
            None
        }
    }

    /// Object name the original assigns to calibration frames and sessions.
    pub fn object_name(self) -> &'static str {
        match self {
            Self::Light => "Light",
            Self::Bias => "Bias",
            Self::Dark => "Dark",
            Self::Flat => "Flat",
            Self::FlatDark => "FlatDark",
        }
    }

    /// Master type string stored in the `Masters` table.
    pub fn master_type(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Bias => "bias",
            Self::Dark => "dark",
            Self::Flat => "flat",
            Self::FlatDark => "flatdark",
        }
    }

    pub fn from_master_type(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "bias" => Some(Self::Bias),
            "dark" => Some(Self::Dark),
            "flat" => Some(Self::Flat),
            "flatdark" => Some(Self::FlatDark),
            _ => None,
        }
    }
}

/// Numeric comparison of exposure strings with a small tolerance.
pub fn exposures_match(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => match (a.trim().parse::<f64>(), b.trim().parse::<f64>()) {
            (Ok(x), Ok(y)) => (x - y).abs() <= 1e-3,
            _ => a == b,
        },
        (a, b) => a == b,
    }
}

/// `path` if free, else `stem_001.ext`, `stem_002.ext`, ...
pub fn unique_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let stem = path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let parent = path.parent().unwrap_or(Path::new("."));
    (1..)
        .map(|i| parent.join(format!("{stem}_{i:03}{ext}")))
        .find(|p| !p.exists())
        .unwrap()
}

/// Move a file, falling back to copy+delete across filesystems.
pub fn move_file(from: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to)?;
    std::fs::remove_file(from)?;
    Ok(())
}

pub fn sha256_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn md5_file(path: &Path) -> Result<String> {
    use md5::{Digest, Md5};
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Md5::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", UNITS[i])
}

/// File extensions the ingest pipeline understands.
pub fn is_supported_file(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let name = name.strip_suffix(".gz").unwrap_or(&name);
    [".fits", ".fit", ".fts", ".xisf", ".zip"]
        .iter()
        .any(|e| name.ends_with(e))
}

pub fn is_fits_name(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let name = name.strip_suffix(".gz").unwrap_or(&name);
    [".fits", ".fit", ".fts"].iter().any(|e| name.ends_with(e))
}

/// Open a file with the configured external viewer, or the OS default app.
pub fn open_external(viewer: &str, path: &Path) -> Result<()> {
    use std::process::Command;
    if !viewer.trim().is_empty() {
        Command::new(viewer.trim()).arg(path).spawn()?;
        return Ok(());
    }
    #[cfg(target_os = "windows")]
    Command::new("cmd")
        .args(["/C", "start", ""])
        .arg(path)
        .spawn()?;
    #[cfg(target_os = "macos")]
    Command::new("open").arg(path).spawn()?;
    #[cfg(all(unix, not(target_os = "macos")))]
    Command::new("xdg-open").arg(path).spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_like_python() {
        assert_eq!(sanitize(" M 31 / core "), "M_31_core");
        assert_eq!(sanitize("a::b"), "a_b");
        assert_eq!(sanitize("  "), "Unknown");
    }

    #[test]
    fn classifies_frames() {
        assert_eq!(FrameKind::classify("Light Frame"), Some(FrameKind::Light));
        assert_eq!(FrameKind::classify("Dark Flat"), Some(FrameKind::FlatDark));
        assert_eq!(FrameKind::classify("FLAT"), Some(FrameKind::Flat));
        assert_eq!(FrameKind::classify("Dark Frame"), Some(FrameKind::Dark));
        assert_eq!(FrameKind::classify("Bias Frame"), Some(FrameKind::Bias));
        assert_eq!(FrameKind::classify("MASTERDARK"), Some(FrameKind::Dark));
        assert!(exposures_match(Some("10"), Some("10.0")));
    }
}
