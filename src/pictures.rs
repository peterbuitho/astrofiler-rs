//! Pictures the user made from processed frames (JPG, PNG, TIFF): they are
//! filed with the frames, under their own names.

use crate::config::Config;
use crate::ingest::{self, IngestOptions, Placement};
use crate::{batch, util};
use std::path::{Path, PathBuf};

/// A picture the user saved from processed frames (JPG, PNG, TIFF), as
/// opposed to what a telescope writes next to its frames: thumbnails and a
/// preview of every sub-frame.
pub fn is_picture(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let image = [".jpg", ".jpeg", ".png", ".tif", ".tiff"]
        .iter()
        .any(|e| name.ends_with(e));
    let in_thumbnails = path
        .parent()
        .and_then(|d| d.file_name())
        .is_some_and(|d| d.to_string_lossy().to_lowercase().contains("thumbnail"));
    let telescope = name.contains("thumbnail")
        || name.contains("_thn.")
        || ["light_", "dark_", "flat_", "bias_", "img-", "failed_"]
            .iter()
            .any(|p| name.starts_with(p));
    // Stack previews are filed with their frames as sidecars.
    image && !in_thumbnails && !telescope && !util::is_sidecar(path)
}

fn pictures_under(dir: &Path, skip: &[PathBuf], out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            if !skip.contains(&p) {
                pictures_under(&p, skip, out);
            }
        } else if is_picture(&p) {
            out.push(p);
        }
    }
}

/// Where a picture goes when the catalogue no longer knows which frames came
/// from its folder (they were loaded from another path, or the catalogue was
/// regenerated): the folder the repository already has for the object its
/// folder is named after, next to a result of the same name if there is one.
fn by_folder_name(cfg: &Config, pic: &Path) -> Option<PathBuf> {
    let stem = pic.file_stem()?.to_string_lossy().to_lowercase();
    pic.ancestors().take(3).find_map(|from| {
        let object = crate::names::object_from_folders(from)?;
        let folder = crate::names::object_folder(&object, &cfg.object_names);
        ["Stacked", "Light"].iter().find_map(|top| {
            let dir = cfg.repo.join(top).join(&folder);
            if !dir.is_dir() {
                return None;
            }
            let twin = walkdir::WalkDir::new(&dir)
                .into_iter()
                .filter_map(|e| e.ok())
                .map(|e| e.into_path())
                .find(|p| {
                    util::is_supported_file(p)
                        && !util::is_sidecar(p)
                        && p.file_stem()
                            .is_some_and(|s| s.to_string_lossy().to_lowercase() == stem)
                });
            Some(
                twin.and_then(|t| t.parent().map(Path::to_path_buf))
                    .unwrap_or(dir),
            )
        })
    })
}

/// Move or copy the user's processed pictures under `src` to where the frames
/// of their folder were filed (`filed`: input -> place in the repository).
/// Returns how many were (or, in a dry run, would be) filed.
pub fn file(cfg: &Config, src: &Path, filed: &[(PathBuf, PathBuf)], opts: IngestOptions) -> usize {
    let skip: Vec<PathBuf> = ingest::MANAGED_DIRS
        .iter()
        .map(|d| cfg.repo.join(d))
        .collect();
    let mut pictures = Vec::new();
    pictures_under(src, &skip, &mut pictures);
    let stem = |p: &Path| p.file_stem().map(|s| s.to_string_lossy().to_lowercase());
    let top = |p: &Path| {
        p.strip_prefix(&cfg.repo)
            .ok()
            .and_then(|r| r.components().next())
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
    };
    let mut n = 0;
    for pic in &pictures {
        let pic = pic.as_path();
        let Some(dir) = pic.parent() else { continue };
        let beside: Vec<&(PathBuf, PathBuf)> = filed
            .iter()
            .filter(|(a, _)| a.parent() == Some(dir))
            .collect();
        // A preview of every sub-frame is the telescope's doing, not the
        // user's; one or two pictures named like a frame are the user's.
        let twin = beside.iter().find(|(a, _)| stem(a) == stem(&pic));
        let frame_twin = |p: &Path| {
            beside
                .iter()
                .any(|(a, b)| stem(a) == stem(p) && top(b).as_deref() != Some("Stacked"))
        };
        if frame_twin(&pic)
            && pictures
                .iter()
                .filter(|p| p.parent() == Some(dir) && frame_twin(p))
                .count()
                > 2
        {
            continue;
        }
        // Next to the result it was made from, else next to a stacked result
        // of the same folder, else in the object folder of the frames below,
        // else in the folder of the object its own folder is named after.
        let dest_dir = twin
            .or_else(|| {
                beside
                    .iter()
                    .find(|(_, b)| top(b).as_deref() == Some("Stacked"))
            })
            .and_then(|(_, b)| b.parent().map(Path::to_path_buf))
            .or_else(|| {
                let (_, b) = beside
                    .first()
                    .copied()
                    .or_else(|| filed.iter().find(|(a, _)| a.starts_with(dir)))?;
                let mut parts = b.strip_prefix(&cfg.repo).ok()?.components();
                Some(cfg.repo.join(parts.next()?).join(parts.next()?))
            })
            .or_else(|| by_folder_name(cfg, pic));
        let Some(dest_dir) = dest_dir else {
            log::info!(
                "{}: left, no frames of its folder were filed",
                pic.display()
            );
            continue;
        };
        let Some(name) = pic.file_name() else {
            continue;
        };
        let dest = dest_dir.join(name);
        if dest == pic {
            continue;
        }
        let same_size = |a: &Path, b: &Path| match (a.metadata(), b.metadata()) {
            (Ok(x), Ok(y)) => x.len() == y.len(),
            _ => false,
        };
        // Pictures keep their names: one that is taken stays where it is.
        if dest.exists() {
            if same_size(pic, &dest) {
                log::info!("{}: already at {}", pic.display(), dest.display());
            } else {
                log::warn!(
                    "{}: left, a different picture is already at {}",
                    pic.display(),
                    dest.display()
                );
            }
            continue;
        }
        if opts.dry_run {
            log::info!("plan: {} -> {}", pic.display(), dest.display());
            n += 1;
            continue;
        }
        let done = if opts.placement == Placement::Move {
            util::move_file(&pic, &dest)
        } else {
            util::copy_file(&pic, &dest).map(|_| ())
        };
        match done {
            Ok(()) => {
                log::info!("picture: {} -> {}", pic.display(), dest.display());
                n += 1;
            }
            Err(e) => log::warn!("{}: not filed ({e:#})", pic.display()),
        }
    }
    n
}

/// After frames moved to a renamed folder, the processed pictures filed next
/// to them go along, under their own names.
pub fn follow(cfg: &Config, moved: &[(PathBuf, PathBuf)]) {
    let mut dirs: std::collections::BTreeSet<(PathBuf, PathBuf)> = Default::default();
    for (old, new) in moved {
        // Folders pair up level by level only when the depth is unchanged.
        let levels = if old.components().count() == new.components().count() {
            4
        } else {
            1
        };
        for (a, b) in old.ancestors().zip(new.ancestors()).skip(1).take(levels) {
            if a == b {
                break;
            }
            dirs.insert((a.to_path_buf(), b.to_path_buf()));
        }
    }
    for (old, new) in dirs {
        let Ok(entries) = std::fs::read_dir(&old) else {
            continue;
        };
        for p in entries.flatten().map(|e| e.path()) {
            let Some(name) = p.file_name() else { continue };
            let to = new.join(name);
            if !p.is_file() || !is_picture(&p) {
                continue;
            }
            if to.exists() {
                log::warn!("{}: left, {} exists", p.display(), to.display());
            } else if let Err(e) = util::move_file(&p, &to) {
                log::warn!("{}: not moved ({e})", p.display());
            }
        }
    }
    for top in ["Light", "Stacked"] {
        batch::remove_empty_dirs(&cfg.repo.join(top));
    }
}
