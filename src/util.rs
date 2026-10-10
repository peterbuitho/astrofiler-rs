//! Small helpers shared across modules (ports of `core/utils.py`).

use anyhow::Result;
use std::path::{Path, PathBuf};

/// A folder in GNOME's network view (`/run/user/1000/gvfs/smb-share:server=
/// nas.local,share=photo/...`) rewritten to the same place under a kernel
/// SMB mount of that share (e.g. `/mnt/nas/photo/...`), when there is one.
/// GVFS goes through a FUSE helper: copies through it have failed part-way
/// and left empty files, and a dry-run load read 1.5x slower through it.
/// Other paths, and GVFS paths without a matching mount, are returned
/// unchanged.
pub fn prefer_kernel_mount(path: &Path) -> PathBuf {
    #[cfg(target_os = "linux")]
    if let Ok(mounts) = std::fs::read_to_string("/proc/mounts") {
        if let Some(p) = gvfs_to_mount(path, &mounts).filter(|p| p.exists()) {
            return p;
        }
    }
    path.to_path_buf()
}

/// Whether `path` is inside GNOME's network view.
pub fn is_gvfs(path: &Path) -> bool {
    parse_gvfs_smb(path).is_some()
}

/// (server, share, path inside the share) of a GVFS SMB path.
fn parse_gvfs_smb(path: &Path) -> Option<(String, String, PathBuf)> {
    let mut comps = path.components();
    let mut prefix = PathBuf::new();
    for c in comps.by_ref() {
        prefix.push(c);
        if c.as_os_str() == "gvfs" {
            break;
        }
    }
    if !prefix.starts_with("/run/user") && !prefix.to_string_lossy().contains(".gvfs") {
        return None;
    }
    let mount = comps.next()?.as_os_str().to_string_lossy().into_owned();
    let params = mount.strip_prefix("smb-share:")?;
    let mut server = None;
    let mut share = None;
    for kv in params.split(',') {
        match kv.split_once('=') {
            Some(("server", v)) => server = Some(percent_decode(v)),
            Some(("share", v)) => share = Some(percent_decode(v)),
            _ => {}
        }
    }
    Some((server?, share?, comps.as_path().to_path_buf()))
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Undo the octal escapes (`\040` for a space) of /proc/mounts.
fn unescape_mounts(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 4], 8) {
                out.push(v);
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn gvfs_to_mount(path: &Path, mounts: &str) -> Option<PathBuf> {
    let (server, share, rest) = parse_gvfs_smb(path)?;
    let host = |h: &str| {
        h.trim()
            .to_lowercase()
            .trim_end_matches(".local")
            .to_string()
    };
    for line in mounts.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 4 || !matches!(f[2], "cifs" | "smb3") {
            continue;
        }
        let source = unescape_mounts(f[0]).replace('\\', "/");
        let Some((m_server, m_path)) = source.trim_start_matches('/').split_once('/') else {
            continue;
        };
        let addr = f[3].split(',').find_map(|o| o.strip_prefix("addr="));
        if host(m_server) != host(&server) && addr != Some(server.as_str()) {
            continue;
        }
        // The mount may be of a folder inside the share (//nas/photo/Astro).
        let mut parts = m_path.split('/').filter(|p| !p.is_empty());
        if !parts.next().is_some_and(|s| s.eq_ignore_ascii_case(&share)) {
            continue;
        }
        let sub: PathBuf = parts.collect();
        if let Ok(inner) = rest.strip_prefix(&sub) {
            return Some(PathBuf::from(unescape_mounts(f[1])).join(inner));
        }
    }
    None
}

/// Whether `a` and `b` are on the same filesystem, so a file can be renamed
/// from one to the other. `b` may not exist yet (its nearest existing folder
/// counts). Unknown means no.
pub fn same_filesystem(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let dev = |p: &Path| {
            p.ancestors()
                .find_map(|q| std::fs::metadata(q).ok())
                .map(|m| m.dev())
        };
        matches!((dev(a), dev(b)), (Some(x), Some(y)) if x == y)
    }
    #[cfg(windows)]
    {
        // Same drive letter or network share.
        let volume = |p: &Path| {
            let q = p.ancestors().find_map(|q| q.canonicalize().ok())?;
            match q.components().next() {
                Some(std::path::Component::Prefix(pre)) => {
                    Some(pre.as_os_str().to_ascii_lowercase())
                }
                _ => None,
            }
        };
        matches!((volume(a), volume(b)), (Some(x), Some(y)) if x == y)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (a, b);
        false
    }
}

/// Whether `path` is on a network filesystem (SMB, NFS, GVFS, sshfs...).
pub fn is_network_path(path: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else {
            return false;
        };
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        // The mount holding the path is the longest mount point that prefixes it.
        let fstype = mounts
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split_whitespace().collect();
                (f.len() >= 3).then(|| (PathBuf::from(unescape_mounts(f[1])), f[2].to_string()))
            })
            .filter(|(mp, _)| path.starts_with(mp))
            .max_by_key(|(mp, _)| mp.as_os_str().len())
            .map(|(_, t)| t)
            .unwrap_or_default();
        matches!(
            fstype.as_str(),
            "cifs" | "smb3" | "smbfs" | "nfs" | "nfs4" | "9p" | "afs"
        ) || fstype.starts_with("fuse.gvfs")
            || fstype == "fuse.sshfs"
    }
    #[cfg(target_os = "macos")]
    {
        // `mount` lists "//user@nas/share on /Volumes/share (smbfs, ...)".
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let Ok(out) = std::process::Command::new("/sbin/mount").output() else {
            return false;
        };
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| {
                let (_, rest) = l.split_once(" on ")?;
                let (mp, kind) = rest.rsplit_once(" (")?;
                Some((PathBuf::from(mp), kind.split(',').next()?.to_string()))
            })
            .filter(|(mp, _)| path.starts_with(mp))
            .max_by_key(|(mp, _)| mp.as_os_str().len())
            .is_some_and(|(_, k)| matches!(k.as_str(), "smbfs" | "nfs" | "afpfs" | "webdav"))
    }
    #[cfg(windows)]
    {
        let s = path.to_string_lossy();
        s.starts_with("\\\\") && !s.starts_with("\\\\?\\") || s.starts_with("\\\\?\\UNC\\")
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = path;
        false
    }
}

/// How many files to read at once from these places. A NAS's disks slow
/// down when too many reads are in flight: a load of 40 frames took 16.9 s
/// with 32 at once and 7.6-9.0 s with 4. Local disks get every core.
pub fn io_threads(paths: &[&Path]) -> usize {
    if paths.iter().any(|p| is_network_path(p)) {
        4
    } else {
        rayon::current_num_threads()
    }
}

/// Run `f` on a thread pool sized by [`io_threads`] for `paths`.
pub fn with_io_pool<R: Send>(paths: &[&Path], f: impl FnOnce() -> R + Send) -> R {
    match rayon::ThreadPoolBuilder::new()
        .num_threads(io_threads(paths))
        .build()
    {
        Ok(pool) => pool.install(f),
        Err(_) => f(),
    }
}

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

/// Frame class used for repository folders and sessions.
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

/// Files at least this big are read and copied in parallel parts. A NAS
/// often serves one stream at a fraction of the link speed (seen: 17-60 MB/s
/// alone, 117 MB/s with four parts in flight on gigabit); local disks don't
/// mind either way.
const PARALLEL_MIN: u64 = 8 << 20;
const PARTS: u64 = 4;
/// Whole-file reads into memory stop here (checksums stream beyond it).
const IN_MEMORY_MAX: u64 = 512 << 20;

fn read_at(f: &std::fs::File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
    #[cfg(unix)]
    return std::os::unix::fs::FileExt::read_at(f, buf, offset);
    #[cfg(windows)]
    return std::os::windows::fs::FileExt::seek_read(f, buf, offset);
}

fn write_at(f: &std::fs::File, buf: &[u8], offset: u64) -> std::io::Result<usize> {
    #[cfg(unix)]
    return std::os::unix::fs::FileExt::write_at(f, buf, offset);
    #[cfg(windows)]
    return std::os::windows::fs::FileExt::seek_write(f, buf, offset);
}

fn read_exact_at(f: &std::fs::File, mut buf: &mut [u8], mut offset: u64) -> std::io::Result<()> {
    while !buf.is_empty() {
        match read_at(f, buf, offset)? {
            0 => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            n => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
        }
    }
    Ok(())
}

fn write_all_at(f: &std::fs::File, mut buf: &[u8], mut offset: u64) -> std::io::Result<()> {
    while !buf.is_empty() {
        match write_at(f, buf, offset)? {
            0 => return Err(std::io::ErrorKind::WriteZero.into()),
            n => {
                buf = &buf[n..];
                offset += n as u64;
            }
        }
    }
    Ok(())
}

/// Run `work(offset, len)` over `len` bytes split into [`PARTS`] ranges, one
/// thread each.
fn in_parts(
    len: u64,
    work: impl Fn(u64, u64) -> std::io::Result<()> + Sync,
) -> std::io::Result<()> {
    let part = len.div_ceil(PARTS);
    std::thread::scope(|s| {
        let work = &work;
        let threads: Vec<_> = (0..PARTS)
            .map(|i| i * part)
            .filter(|&off| off < len)
            .map(|off| s.spawn(move || work(off, part.min(len - off))))
            .collect();
        threads.into_iter().try_for_each(|t| {
            t.join()
                .unwrap_or_else(|_| Err(std::io::Error::other("read thread panicked")))
        })
    })
}

/// Read a whole file into memory, big files in parallel parts.
pub fn read_file(path: &Path) -> Result<Vec<u8>> {
    read_whole(&std::fs::File::open(path)?)
}

fn read_whole(f: &std::fs::File) -> Result<Vec<u8>> {
    use std::io::Read;
    let len = f.metadata()?.len();
    if !(PARALLEL_MIN..=IN_MEMORY_MAX).contains(&len) {
        let mut buf = Vec::with_capacity(len as usize);
        let mut f = f;
        f.read_to_end(&mut buf)?;
        return Ok(buf);
    }
    let mut buf = vec![0u8; len as usize];
    let part = len.div_ceil(PARTS) as usize;
    std::thread::scope(|s| {
        let threads: Vec<_> = buf
            .chunks_mut(part)
            .enumerate()
            .map(|(i, chunk)| s.spawn(move || read_exact_at(f, chunk, (i * part) as u64)))
            .collect();
        threads.into_iter().try_for_each(|t| {
            t.join()
                .unwrap_or_else(|_| Err(std::io::Error::other("read thread panicked")))
        })
    })?;
    Ok(buf)
}

/// Copy file contents only. Unlike `std::fs::copy` this never copies
/// permissions, which network filesystems such as GNOME's GVFS reject with
/// "Operation not supported". Big files are copied in parallel parts. A
/// partial destination is removed on failure.
pub fn copy_file(from: &Path, to: &Path) -> Result<u64> {
    use std::io::{Read, Write};
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let result = (|| -> Result<u64> {
        let input = std::fs::File::open(from)?;
        let len = input.metadata()?.len();
        let mut output = std::fs::File::create(to)?;
        if len >= PARALLEL_MIN {
            output.set_len(len)?;
            in_parts(len, |start, n| {
                let mut buf = vec![0u8; (4 << 20).min(n as usize)];
                let mut off = start;
                while off < start + n {
                    let k = buf.len().min((start + n - off) as usize);
                    read_exact_at(&input, &mut buf[..k], off)?;
                    write_all_at(&output, &buf[..k], off)?;
                    off += k as u64;
                }
                Ok(())
            })?;
            return Ok(len);
        }
        let mut input = input;
        let mut buf = vec![0u8; 4 << 20];
        let mut total = 0u64;
        loop {
            let n = input.read(&mut buf)?;
            if n == 0 {
                break;
            }
            output.write_all(&buf[..n])?;
            total += n as u64;
        }
        output.flush()?;
        Ok(total)
    })();
    if result.is_err() {
        std::fs::remove_file(to).ok();
    }
    result
}

/// Move a file, falling back to copy+delete across filesystems.
pub fn move_file(from: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    copy_file(from, to)?;
    std::fs::remove_file(from)?;
    Ok(())
}

pub fn sha256_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    if (PARALLEL_MIN..=IN_MEMORY_MAX).contains(&len) {
        return Ok(format!("{:x}", Sha256::digest(read_whole(&f)?)));
    }
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

/// Non-FITS files kept alongside the frames they belong to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidecarKind {
    /// DWARF `shotsInfo.json` session summary.
    SessionInfo,
    /// JPG/PNG render of a stacked result (Seestar `Stacked_*.jpg`, DWARF
    /// `stacked.jpg` / `stacked-*.png` / AstroWizard output).
    StackPreview,
}

/// Stacked-result previews worth keeping; thumbnails (`*_thn.jpg`,
/// `*thumbnail*`) are not.
pub fn is_stack_preview_name(name: &str) -> bool {
    let n = name.to_lowercase();
    let image = [".jpg", ".jpeg", ".png"].iter().any(|e| n.ends_with(e));
    image
        && !n.contains("thumbnail")
        && !n.contains("_thn.")
        && (n.starts_with("stacked") || n.contains("astrowizard"))
}

pub fn sidecar_kind(path: &Path) -> Option<SidecarKind> {
    let name = path.file_name()?.to_string_lossy();
    if name.eq_ignore_ascii_case("shotsInfo.json") {
        Some(SidecarKind::SessionInfo)
    } else if is_stack_preview_name(&name) {
        Some(SidecarKind::StackPreview)
    } else {
        None
    }
}

pub fn is_sidecar(path: &Path) -> bool {
    sidecar_kind(path).is_some()
}

/// File extensions the ingest pipeline understands.
pub fn is_supported_file(path: &Path) -> bool {
    if is_sidecar(path) {
        return true;
    }
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

/// Open the folder containing `path` in the file manager, with the file
/// highlighted where the file manager supports it.
pub fn show_in_folder(path: &Path) -> Result<()> {
    use std::process::Command;
    let dir = path.parent().unwrap_or(path).to_path_buf();
    if !path.exists() {
        anyhow::bail!("{} no longer exists", path.display());
    }
    #[cfg(target_os = "windows")]
    {
        let mut arg = std::ffi::OsString::from("/select,");
        arg.push(path);
        Command::new("explorer").arg(arg).spawn()?;
    }
    #[cfg(target_os = "macos")]
    Command::new("open").arg("-R").arg(path).spawn()?;
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // The freedesktop file-manager interface (Nautilus, Dolphin, Nemo…)
        // selects the file; fall back to just opening the folder.
        let uri = file_uri(path);
        std::thread::spawn(move || {
            let selected = Command::new("dbus-send")
                .args([
                    "--session",
                    "--print-reply",
                    "--dest=org.freedesktop.FileManager1",
                    "--type=method_call",
                    "/org/freedesktop/FileManager1",
                    "org.freedesktop.FileManager1.ShowItems",
                ])
                .arg(format!("array:string:{uri}"))
                .arg("string:")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            if !selected {
                let _ = Command::new("xdg-open").arg(&dir).spawn();
            }
        });
    }
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    let _ = dir;
    Ok(())
}

/// `file://` URI for an absolute path, percent-encoding anything but
/// unreserved characters and slashes.
pub fn file_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for b in path.to_string_lossy().bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn file_uris() {
        assert_eq!(
            super::file_uri(std::path::Path::new("/mnt/nas/M 76/Stacked_2,x.fit")),
            "file:///mnt/nas/M%2076/Stacked_2%2Cx.fit"
        );
    }

    use super::*;

    #[test]
    fn sanitizes_like_python() {
        assert_eq!(sanitize(" M 31 / core "), "M_31_core");
        assert_eq!(sanitize("a::b"), "a_b");
        assert_eq!(sanitize("  "), "Unknown");
    }

    #[test]
    fn copies_contents() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.bin");
        let b = dir.path().join("sub/b.bin");
        let data: Vec<u8> = (0..10_000_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&a, &data).unwrap();
        assert_eq!(copy_file(&a, &b).unwrap(), data.len() as u64);
        assert_eq!(std::fs::read(&b).unwrap(), data);
        assert!(copy_file(&dir.path().join("missing"), &dir.path().join("c")).is_err());
        assert!(
            !dir.path().join("c").exists(),
            "no partial file left behind"
        );
    }

    #[test]
    fn gvfs_paths_use_the_kernel_mount() {
        let mounts = "\
/dev/nvme0n1p2 / ext4 rw 0 0
//nas.local/photo /mnt/nas/photo cifs rw,vers=3.0,addr=192.168.1.10 0 0
//192.168.1.10/media /mnt/nas/my\\040media cifs rw,addr=192.168.1.10 0 0
//other/stuff/Sub\\040Dir /mnt/sub cifs rw,addr=10.0.0.9 0 0
";
        let g = |p: &str| gvfs_to_mount(Path::new(p), mounts);
        assert_eq!(
            g("/run/user/1000/gvfs/smb-share:server=nas.local,share=photo/Astro/M 76 Barbell Nebula XISF"),
            Some(PathBuf::from("/mnt/nas/photo/Astro/M 76 Barbell Nebula XISF"))
        );
        // Host names compare without case or ".local"; an IP matches addr=.
        assert_eq!(
            g("/run/user/1000/gvfs/smb-share:server=NAS,share=Photo/x"),
            Some(PathBuf::from("/mnt/nas/photo/x"))
        );
        assert_eq!(
            g("/run/user/1000/gvfs/smb-share:server=192.168.1.10,share=media"),
            Some(PathBuf::from("/mnt/nas/my media"))
        );
        assert_eq!(
            g("/run/user/1000/gvfs/smb-share:server=other,share=stuff/Sub Dir/a.fits"),
            Some(PathBuf::from("/mnt/sub/a.fits"))
        );
        assert_eq!(
            g("/run/user/1000/gvfs/smb-share:server=other,share=stuff/Elsewhere"),
            None
        );
        assert_eq!(
            g("/run/user/1000/gvfs/smb-share:server=nope,share=photo/x"),
            None
        );
        assert_eq!(g("/mnt/nas/photo/Astro"), None);
        assert!(is_gvfs(Path::new(
            "/run/user/1000/gvfs/smb-share:server=a,share=b/c"
        )));
        assert!(!is_gvfs(Path::new("/mnt/nas/photo")));
        assert_eq!(percent_decode("My%20Share"), "My Share");
    }

    #[test]
    fn big_files_in_parts() {
        use sha2::{Digest, Sha256};
        let dir = tempfile::tempdir().unwrap();
        // Above PARALLEL_MIN, and not a multiple of PARTS, so parts differ in size.
        for len in [PARALLEL_MIN as usize - 1, PARALLEL_MIN as usize + 12_345] {
            let a = dir.path().join(format!("a{len}.bin"));
            let data: Vec<u8> = (0..len).map(|i| (i * 7 % 253) as u8).collect();
            std::fs::write(&a, &data).unwrap();
            assert!(read_file(&a).unwrap() == data, "read_file {len}");
            let b = dir.path().join(format!("b{len}.bin"));
            assert_eq!(copy_file(&a, &b).unwrap(), len as u64);
            assert!(std::fs::read(&b).unwrap() == data, "copy_file {len}");
            assert_eq!(
                sha256_file(&a).unwrap(),
                format!("{:x}", Sha256::digest(&data)),
                "sha256_file {len}"
            );
        }
    }

    #[test]
    fn stack_previews() {
        for keep in [
            "Stacked_47_M 39_10.0s_IRCUT_20260927-033001.jpg",
            "stacked.jpg",
            "stacked-16_M 39_15s60_Astro_20260927-014732395.png",
            "LDN 935_30s60_Astro_20260808-AstroWizard.png",
        ] {
            assert!(is_stack_preview_name(keep), "{keep}");
        }
        for skip in [
            "Stacked_47_M 39_10.0s_IRCUT_20260927-033001_thn.jpg",
            "stacked_thumbnail.jpg",
            "img_reference.png",
            "img_stacked_counter.png",
            "Light_1.jpg",
            "stacked.fits",
        ] {
            assert!(!is_stack_preview_name(skip), "{skip}");
        }
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
#[cfg(all(test, target_os = "linux"))]
mod network_tests {
    #[test]
    fn local_paths_are_not_network() {
        assert!(!super::is_network_path(std::path::Path::new("/tmp")));
        assert!(!super::is_network_path(&std::env::temp_dir()));
    }
}
