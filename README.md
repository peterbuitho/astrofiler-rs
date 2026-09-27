# AstroFiler-rs

Fast astronomical image filing and cataloguing, written in Rust. A port of the
file-management core of [AstroFiler](https://github.com/gordtulloch/astrofiler-gui)
by Gord Tulloch, focused on organising large FITS/XISF collections and importing
straight from smart telescopes.

Runs natively on **Windows, macOS (Apple Silicon) and Linux**, with a desktop app
and a command-line tool. No Python, no C libraries to install.

## What it does

- **Loads images from any folder**, whether that's an incoming folder, a USB drive,
  or years of existing files on a NAS. It reads the FITS headers, renames each file
  descriptively and files it into a tidy repository:

  ```
  Repository/
    Light/<Object>/<Telescope>/<Camera>/<YYYYMMDD>/M_31-RedCat_51-ASI2600MM-Ha-20241001210000-300.0s-1x1-t-10.0.fits
    Calibrate/<DARK|FLAT|BIAS|FLATDARK>/<Telescope>/<Camera>/
    Stacked/<Object>/<Telescope>/<Camera>/     (the telescopes' own stacked results)
    Masters/                                   (master calibration frames)
  ```

  You choose to **move**, **copy** (originals untouched) or **catalogue in place**,
  and a **dry run** shows exactly where every file would go first.
  If a different file already has the same name in the repository, you choose to
  **skip** (default), **overwrite** or **keep both** (`_001` suffix). Identical
  files are never copied twice, and empty or half-copied leftovers of an
  interrupted copy are always replaced.
- **FITS and XISF**: `.fits/.fit/.fts`, gzip-compressed FITS, `.zip` archives, and
  PixInsight `.xisf` (zlib/LZ4/Zstd, byte-shuffled), which is converted to FITS on import.
- **Smart telescopes**: ZWO **Seestar** and **DWARF**, over **Wi-Fi or USB-C**.
  FITS files are transferred together with the telescopes' stacked-result
  previews (Seestar `Stacked_*.jpg`, DWARF `stacked.jpg` / `stacked-*.png`) and
  DWARF's `shotsInfo.json`; per-frame previews and thumbnails are left behind. Files can
  optionally be deleted from the telescope, but only after they are safely catalogued.
- **Catalogue**: a SQLite database that uses the same schema as the original
  AstroFiler, so an existing `astrofiler.db` can be opened directly.
- **Sessions**: groups frames by object, night and filter (and calibration frames
  by matching settings).
- **Master frames**: registers existing masters, builds new ones from
  calibration sessions (parallel sigma-clipped stacking), validates checksums.
- **Batch management**: merge objects (e.g. "Andromeda" → "M 31"), bulk-edit
  OBJECT/FILTER/TELESCOP/INSTRUME/OBSERVER/NOTES in the catalogue, headers and
  file names, export sessions or selections, delete, verify, and regenerate the catalogue.
- **Duplicates**: SHA-256 based detection and removal.
- **Statistics**: integration time per object, filter, telescope and camera.
- **Header mappings**: normalise inconsistent header values on import.
- **Preview clean-up**: delete leftover telescope JPG/PNG previews and thumbnails
  from old backups (stacked-result previews are kept).

### Why Rust

Header parsing, SHA-256 hashing and stacking run in parallel on all CPU cores,
and all catalogue writes are batched into one transaction. On a 32-core machine,
a dry run over 2,000 Seestar/DWARF frames (24 GB) finishes in under a second
when the files are in the disk cache. On a NAS the network is the limit, not the program.

## Install

Download the archive for your platform from the
[Releases](../../releases) page and unpack it. It contains:

- `astrofiler-gui` (`astrofiler-gui.exe` on Windows): the desktop app
- `astrofiler`: the command-line tool (`astrofiler` with no arguments also opens the app)

On macOS, the first time you run it, right-click → Open (the binaries are not notarised).

To build from source (needs [Rust](https://rustup.rs)):

```sh
cargo install --git https://github.com/peterbuitho/astrofiler-rs
```

On Linux you need the usual GUI development packages (`libxkbcommon-dev libwayland-dev libgtk-3-dev`).
Add `--no-default-features` for a command-line-only build.

## Quick start

1. Open the app, go to **Settings**, and set the **Repository** (where organised
   files live) and the **Incoming folder**.
2. **Existing archive (e.g. on a NAS):** Images → *Load folder…* → pick the folder →
   tick *Dry run* to preview → then run it for real with *Copy* (safest) or *Move*.
3. **From a telescope:** Telescopes → plug in over USB-C (it's detected
   automatically) or pick Wi-Fi → *Connect & list files* → *Import selected*.
4. Sessions → *Create sessions*, then browse in **Images** and **Statistics**.

## Command line

```sh
astrofiler load --source /mnt/nas/astro --dry-run --plan plan.csv   # preview a reorganisation
astrofiler load --source /mnt/nas/astro --copy                      # organised copies, originals untouched
astrofiler load --source ~/incoming --on-conflict overwrite         # replace different files with the same name
astrofiler load                                                     # move files from the incoming folder
astrofiler load --source /mnt/nas/astro --no-move                   # catalogue in place (counts in stats)
astrofiler sync                                                     # catalogue files already in the repository

astrofiler find                          # telescopes connected over USB (add --network for Wi-Fi)
astrofiler import seestar --usb auto     # import from a Seestar on USB-C
astrofiler import dwarf --list           # list what's on a DWARF over Wi-Fi
astrofiler import seestar --delete       # import, then delete from the telescope

astrofiler sessions create
astrofiler masters create                # build masters for all calibration sessions
astrofiler masters register ~/old-masters --move
astrofiler merge "Andromeda" "M 31" --headers --refile
astrofiler set FILTER "L-Pro" --object "NGC 7000" --current "LP"
astrofiler export ~/stack-me --session <session-id> --by-object
astrofiler duplicates --remove
astrofiler verify --hash
astrofiler clean-previews /mnt/nas/astro --dry-run
astrofiler stats
astrofiler mapping add TELESCOP "S50_1a2b3c4d" "Seestar S50"
astrofiler header some-file.fits
astrofiler config set repo /mnt/nas/Repository
```

Run `astrofiler <command> --help` for all options.

## Configuration

Settings are stored in `astrofiler.ini` (`[DEFAULT]` section, compatible with the
original). It's looked up in this order:

1. `$ASTROFILER_CONFIG`
2. `./astrofiler.ini`, so running from an existing AstroFiler folder reuses its settings and `astrofiler.db`
3. `~/.config/astrofiler/astrofiler.ini` (Linux), `~/Library/Application Support/astrofiler/` (macOS),
   `%APPDATA%\astrofiler\` (Windows)

The database defaults to `astrofiler.db` beside the config file, or the user data
folder; override it with the `database` key, `--db` or `$ASTROFILER_DB_PATH`.

## Smart telescopes

| Telescope | Wi-Fi | USB-C | Files imported |
|-----------|-------|-------|----------------|
| ZWO Seestar | SMB share `EMMC Images` (guest) | drive with `MyWorks/` | `<target>_sub/*.fit`, mosaics, optional `Stacked_*.fit` + `.jpg` |
| DWARF | FTP (`192.168.88.1` in hotspot mode) | drive with `DWARF_RAW_*` (optionally under `Astronomy/`) | `DWARF_RAW_*/*.fits`, `shotsInfo.json`, optional `stacked-*.fits` + `stacked.jpg` / `stacked-*.png`, `CALI_FRAME`, `DWARF_DARK` |

Header fixes are applied automatically: the Seestar target and mosaic flag are
taken from the folder name, and DWARF files (which have no `IMAGETYP`) get their
frame type, camera (TELE/WIDE) and temperature filled in. DWARF's `shotsInfo.json` session summary is
kept too: it is filed next to that session's frames as
`<original folder name>_shotsInfo.json`. Stacked-result previews are filed next
to their stacked FITS, taking its new name when they share one (Seestar
`Stacked_*.jpg`, DWARF `stacked-*.png`), otherwise prefixed with the folder name. Frames DWARF marked
`failed_*` are imported too, so you can decide for yourself whether to use them.

If a telescope connects over USB as a *media device* (MTP) instead of a drive, it
won't appear as a folder. In that case copy the files off with your OS first, or use Wi-Fi.

### Adding a telescope

Each telescope is a self-contained module in `src/telescope/`. To add one (e.g.
DWARF Draco), create `src/telescope/<model>.rs` implementing the `Telescope`
trait (connection, USB detection, where the FITS files live, optional header
fixes) and add it to `all()` in `src/telescope/mod.rs`. See the module
documentation there. The CLI, GUI, discovery and import pipeline pick it up automatically.

## Differences from the original AstroFiler

Deliberately **not** included: light-frame calibration and auto-calibration,
cloud sync, SEP quality metrics, and telescopes other than Seestar and DWARF.

Improvements and fixes over the original:
- copy mode, dry runs with a CSV plan, and duplicate checks before anything is moved;
- DWARF files are recognised even outside the telescope's folder layout, and
  `failed_*` frames are imported instead of reported as errors;
- stacked results are kept apart from sub-frames;
- header mappings are actually applied on import;
- light sessions with interleaved filters group correctly;
- master flats keep their ADU scale.

## License

GPL-3.0-or-later, matching the original project's `LICENSE`. Based on
[AstroFiler](https://github.com/gordtulloch/astrofiler-gui) by Gord Tulloch.

App icon: the original AstroFiler telescope (telescope icon created by
[Freepik - Flaticon](https://www.flaticon.com/free-icons/telescope)) framed in a
rust-orange gear as a nod to Rust. It is not the Rust logo, and this project is
not affiliated with the Rust Foundation.
