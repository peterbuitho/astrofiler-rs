# AstroFiler-rs Codebase Audit

- **Repository:** `astrofiler-rs`
- **Audited commit:** `8260106` (`v0.8.8`, pulled from `3e07244..8260106`)
- **Audit date:** 2026-11-26
- **Scope:** Full static audit of all Rust sources (`src/`, ~13,735 LOC across 21 files), CI workflow (`.github/workflows/ci.yml`), packaging script (`scripts/macos-dmg.sh`), and `Cargo.toml` / `Cargo.lock`.

---

## Executive Summary

AstroFiler-rs is a well-structured, readable Rust codebase with **zero `unsafe` blocks**, solid unit test coverage on core workflows, and thoughtful domain engineering (NAS-aware I/O throttling, SQLite compatibility with Python AstroFiler, FITS/XISF header rewriting, and smart-telescope support).

Automated checks on `v0.8.8`:

| Check | Result |
|---|---|
| `cargo test --all-features` | **Pass** — 65 passed, 0 failed (2 dead-code warnings on non-Linux) |
| `cargo test --no-default-features` | **Pass** — 65 passed, 0 failed |
| `cargo fmt --check` | **Pass** — clean |
| `cargo clippy --all-targets --all-features` | **15 warnings** across 5 files |
| `unsafe` usage in `src/` | **0 blocks** |

The audit identified **22 findings** (6 High, 12 Medium, 4 Low) covering path traversal, Windows command injection, data-loss race conditions, file-handle leaks, parser panics/OOM vectors, a broken GUI input introduced in `v0.8.8`, and catalogue/session edge cases.

### Findings Summary

| ID | Severity | Area | Title |
|---|---|---|---|
| [H-1](#h-1-path-traversal-via-unsanitized-and-dot-fitsxisf-header-values) | High | Security / Ingest | Path traversal via unsanitized and `..` FITS/XISF header values |
| [H-2](#h-2-utilcopy_file-deletes-pre-existing-target-on-source-open-failure-and-truncates-self-copies) | High | Data Loss | `util::copy_file` deletes pre-existing target on source-open failure & truncates self-copies |
| [H-3](#h-3-os-command-injection-on-windows-in-open_external-and-unvalidated-web_url-scheme) | High | Security | OS command injection on Windows in `open_external` & unvalidated `web_url` scheme |
| [H-4](#h-4-data-loss-race-condition-and-missing-db-transaction-in-batchexport_files) | High | Data Loss / Concurrency | Data-loss race condition & missing DB transaction in `batch::export_files` |
| [H-5](#h-5-premature-archive-deletion-extraction-race-and-unbounded-decompression-in-ingestunpack) | High | Data Loss / DoS | Premature archive deletion, extraction race & unbounded decompression in `ingest::unpack` |
| [H-6](#h-6-open-file-handle-held-during-renameoverwrite-in-fitsrewrite_primary_header) | High | Cross-Platform / Reliability | Open file handle held during rename/overwrite in `fits::rewrite_primary_header` |
| [M-1](#m-1-broken-inbox-excludes-text-input-in-desktop-gui-settings) | Medium | GUI Bug (`v0.8.8`) | Broken "Inbox excludes" text input in desktop GUI Settings |
| [M-2](#m-2-move_to_inbox-matches-ancestor-directories-against-inbox_exclude-and-prunes-excluded-subdirectories) | Medium | Correctness (`v0.8.8`) | `move_to_inbox` matches ancestor dirs against `inbox_exclude` & prunes excluded subdirs |
| [M-3](#m-3-trailing-slash-breaks-sqlite-prefix-queries-in-layout_plan-and-ingest_folder) | Medium | Correctness | Trailing slash breaks SQLite prefix queries in `layout_plan` and `ingest_folder` |
| [M-4](#m-4-fits-card-truncation-corrupts-long-string-headers-and-continue-escaped-quotes) | Medium | Data Integrity | FITS card truncation corrupts long string headers & `CONTINUE` escaped quotes |
| [M-5](#m-5-integer-overflow-oom-and-utf-8-slice-panic-vectors-in-fits-xisf-and-dwarf-parsers) | Medium | Reliability / DoS | Integer overflow, OOM, and UTF-8 slice panic vectors in FITS, XISF, and DWARF parsers |
| [M-6](#m-6-stale-checksum-stored-in-catalogue-when-header-rewrite-fails) | Medium | Data Integrity | Stale checksum stored in catalogue when header rewrite fails |
| [M-7](#m-7-duplicate-removal-compares-raw-path-strings-and-discards-deletion-errors) | Medium | Data Loss | `remove_duplicates` compares raw path strings & discards deletion errors |
| [M-8](#m-8-uncanonicalized-repo-exclusions-and-premature-forget_unfiled-in-ingest_folder) | Medium | Data Integrity | Uncanonicalized repo exclusions & premature `forget_unfiled` (missing `Archive`) |
| [M-9](#m-9-cli-silently-ignores---session-when---object-is-also-supplied-and-drops-set-errors) | Medium | CLI Bug | CLI silently ignores `--session` when `--object` is supplied & omits `set` error details |
| [M-10](#m-10-session-sync-stats-double-counting-and-soft-delete-inconsistencies) | Medium | Correctness | Session sync gaps, `stats::compute` double-counting, and soft-delete inconsistencies |
| [M-11](#m-11-telescope-transport-timeouts-path-validation-and-gui-folder-deletion-hazard) | Medium | Reliability / Data Loss | Missing SMB timeouts, `LocalTransport` path validation, `.part` handling, and GUI delete hazard |
| [M-12](#m-12-clean_previews-deletes-users-processed-jpgpng-pictures) | Medium | Data Loss / UX | `clean_previews` deletes user-processed JPG/PNG pictures |
| [L-1](#l-1-non-atomic-config-writes-and-plaintext-password-permissions) | Low | Security / Reliability | Non-atomic config writes and plaintext password file permissions |
| [L-2](#l-2-mutex-poison-panics-and-per-frame-cloning-in-gui-and-logger) | Low | Performance / Robustness | Mutex `.unwrap()` poison panics, per-frame cloning, and `list_local` race in GUI |
| [L-3](#l-3-symlink-cycle-size-only-dedup-and-over-broad-ancestor-moves-in-picturesrs) | Low | Robustness | Symlink cycle, size-only dedup, and over-broad ancestor moves in `pictures.rs` |
| [L-4](#l-4-compilerclippy-warnings-ci-hardening-and-maintenance-items) | Low | Quality / CI | Compiler/Clippy warnings, CI permissions/pinning, and minor maintenance items |

---

## High-Severity Findings

### H-1. Path traversal via unsanitized and `..` FITS/XISF header values

- **Locations:** `src/util.rs:244-265`, `src/ingest.rs:1558-1636`, `src/batch.rs:586-587`
- **Details:**
  1. `util::sanitize()` replaces `" \\/:*?\"<>|\t\n\r"` with `_`, but **does not reject `.` or `..`**, ASCII control characters (`\0`–`\x1f`), trailing dots/spaces (invalid on Windows/SMB), or Windows reserved device names (`CON`, `NUL`, `AUX`, `PRN`, `COM1`–`9`, `LPT1`–`9`). Thus `sanitize("..")` returns `".."`.
  2. In `ingest::destination()` (`src/ingest.rs:1579-1623`), `exposure` (`EXPTIME`/`EXPOSURE`), `xbin` (`XBINNING`), `ybin` (`YBINNING`), and `temp` (`CCD-TEMP`) are formatted directly into the filename **without calling `sanitize()`**. A FITS/XISF file with a string card such as `CCD-TEMP = '/../../evil'` or `OBJECT = '..'`, `TELESCOP = '..'`, `INSTRUME = '..'` can escape `cfg.repo` during filing or `batch::export_files` (`ExportLayout::ByObject`).
- **Recommendation:**
  - Harden `util::sanitize()` to replace control characters, strip trailing `.`/spaces, map `.`/`..` and Windows reserved names to safe values (e.g. `"Unknown"` or prefixed `_`), and apply `sanitize()` to `exposure`, `xbin`, `ybin`, and `temp` as well.
  - Assert that the computed `dest_dir` and `dest_dir.join(&new_name)` stay strictly within `cfg.repo` (and `dest` in `export_files`).

---

### H-2. `util::copy_file` deletes pre-existing target on source-open failure and truncates self-copies

- **Location:** `src/util.rs:470-512`
- **Details:**
  ```rust
  let result = (|| -> Result<u64> {
      let input = std::fs::File::open(from)?;
      let len = input.metadata()?.len();
      let mut output = std::fs::File::create(to)?;
      ...
  })();
  if result.is_err() {
      std::fs::remove_file(to).ok();
  }
  ```
  1. If `File::open(from)` or `input.metadata()` fails **before** `to` is created, `result.is_err()` is still `true` and `std::fs::remove_file(to)` **deletes any pre-existing file at `to`**.
  2. If `from` and `to` resolve to the same underlying file (via symlink, hardlink, or case-insensitive filesystem like macOS APFS/Windows NTFS), `File::create(to)` truncates the file to 0 bytes before reading, destroying the original (and for files `>= 8 MiB`, `read_exact_at` fails with `UnexpectedEof` and line 509 deletes the file completely).
  3. On the parallel path (`len >= 8 MiB`), `output.flush()` is skipped and partial writes on failure leave the pre-existing `to` clobbered rather than intact.
- **Recommendation:**
  - Check whether `from` and `to` refer to the same file (e.g. using `same_file::is_same_file`, already present in `Cargo.lock`) before creating `to`.
  - Track whether `to` was created by this call (or write to a unique sibling temp file and rename atomically), so an error opening `from` never removes an existing `to`.

---

### H-3. OS command injection on Windows in `open_external` and unvalidated `web_url` scheme

- **Locations:** `src/util.rs:619-635`, `src/gui/mod.rs:258-277`
- **Details:**
  1. On Windows, `util::open_external()` spawns:
     ```rust
     Command::new("cmd").args(["/C", "start", ""]).arg(path).spawn()?;
     ```
     Passing a path or URL through `cmd /C` causes `cmd.exe` to interpret shell metacharacters (`&`, `|`, `^`, `%VAR%`, `>`, `<`). A malicious filename (from a telescope or archive) or crafted `web_url` can execute arbitrary OS commands on Windows.
  2. `App::open_web()` only checks `base.contains("://")`, allowing non-HTTP schemes (e.g. `file://`, `ms-msdt:`, etc.) to be passed directly to the OS launcher.
- **Recommendation:**
  - On Windows, avoid `cmd /C` (use `ShellExecuteW` via Win32 FFI / `open` crate, or `explorer.exe` / `rundll32 url.dll,FileProtocolHandler`).
  - In `App::open_web()`, validate that the final URL starts with `http://` or `https://` and contains no control characters.

---

### H-4. Data-loss race condition and missing DB transaction in `batch::export_files`

- **Location:** `src/batch.rs:554-625`
- **Details:**
  1. Inside `files.par_iter()` (`src/batch.rs:573-604`), worker threads concurrently call `util::unique_path(&dir.join(...))` and then `move_file`/`copy_file`. When two exported files share a filename (common with `ExportLayout::Flat` or multiple `stacked.fits`), two threads can observe the same free path and overwrite each other. With `--move`, one file is **permanently lost**.
  2. Unlike `set_field` and `migrate_layout` (which explicitly take `TransactionBehavior::Immediate` before moving files), `export_files(..., move_files=true)` moves all files on disk first and updates SQLite one row at a time afterwards without a transaction. A locked DB or error leaves moved files unrecorded at their new paths.
- **Recommendation:**
  - Pre-assign unique destination paths sequentially before entering `par_iter()` (maintaining a `HashSet<PathBuf>` of claimed destinations, as `ingest` does for dry runs).
  - When `move_files` is `true`, acquire an `Immediate` transaction before moving files (or batch the DB updates inside a transaction checked up front).

---

### H-5. Premature archive deletion, extraction race, and unbounded decompression in `ingest::unpack`

- **Locations:** `src/ingest.rs:1159-1217`, `src/fits.rs:482-496`, `src/xisf.rs:512-540`
- **Details:**
  1. In `Move` mode, `ingest::unpack()` deletes `.zip` (`src/ingest.rs:1194-1196`) and `.gz` (`src/ingest.rs:1211-1213`) inputs **immediately during unpacking**, before `prepare()` or `file_prepared()` succeeds. For `.zip` archives, only `util::is_fits_name()` entries are extracted — any `.xisf`, `shotsInfo.json`, or picture files inside the `.zip` are **permanently deleted** along with the archive.
  2. `unpack()` runs inside `chunk.par_iter()`. When `moving == true`, `scratch()` calls `util::unique_path()` in the source directory (`src/ingest.rs:1166`). Two `.zip` files in the same folder containing identically named FITS files race on `unique_path` and overwrite each other.
  3. Decompression in `unpack()` (`.zip` / `.gz`), `fits::FitsFile::open`/`from_bytes` (`GzDecoder::read_to_end`), and `xisf::decompress` (`ZlibDecoder::read_to_end`, `Vec::with_capacity(size)` from untrusted XML) has no upper size bound, allowing corrupt or crafted files to exhaust memory or disk.
- **Recommendation:**
  - Extract into the per-run unique `.astrofiler-work-*` directory (even in `Move` mode) and delete the original `.zip`/`.gz` only after all extracted entries are committed (and for `.zip`, only when all archive entries were handled or explicitly warning if non-FITS entries remain).
  - Cap decompressed output sizes (e.g., via `Read::take(max_bytes)`) and validate `size` in `xisf::decompress` against `shape.len() * item_size` and a sane ceiling before allocating.

---

### H-6. Open file handle held during rename/overwrite in `fits::rewrite_primary_header`

- **Locations:** `src/fits.rs:763-790` (also temp cleanup in `src/fits.rs:701-728`, `src/xisf.rs:331-343`, `src/ingest.rs:1141-1156`)
- **Details:**
  ```rust
  let mut file = File::open(path)?;
  let (_, old_len) = read_header(&mut BufReader::new(&mut file))?...;
  let new_bytes = header.to_bytes();
  if new_bytes.len() as u64 == old_len {
      let mut f = std::fs::OpenOptions::new().write(true).open(path)?;
      ...
  }
  ...
  std::fs::rename(&tmp, path)?;
  ```
  `file` stays open until `rewrite_primary_header` returns. On Windows and some SMB/CIFS mounts, renaming `&tmp` over `path` while `path` still has an open handle fails with `ERROR_ACCESS_DENIED` / sharing violation.
  Additionally:
  - Failed writes to `.astrofiler-tmp` in `fits::write_image`, `fits::rewrite_primary_header`, `xisf::rewrite_header`, and `ingest::replace_file` never clean up the temp file.
  - `replace_file` (`src/ingest.rs:1141-1156`) moves `from` to `&tmp` first when `moving == true`; if the subsequent rename over `to` fails, `from` is gone and the file is stranded as `.astrofiler-tmp` (which `collect_files` ignores).
- **Recommendation:**
  - Scope `File::open(path)` inside a block so `file` is dropped immediately after `read_header`.
  - Clean up temp files on error (e.g. via a small RAII guard) and include a random/PID suffix in temp names to avoid collisions.
  - In `replace_file`, prefer copying to `&tmp`, renaming over `to`, and removing `from` only after `to` is in place (or restoring on failure).

---

## Medium-Severity Findings

### M-1. Broken "Inbox excludes" text input in desktop GUI Settings

- **Location:** `src/gui/mod.rs:1082-1095`
- **Details:**
  ```rust
  let mut s = c.inbox_exclude.join(", ");
  if ui.add(egui::TextEdit::singleline(&mut s).desired_width(420.0)).changed() {
      c.inbox_exclude = s
          .split(',')
          .map(|n| n.trim().to_string())
          .filter(|n| !n.is_empty())
          .collect();
  }
  ```
  Because `s` is rebuilt from `c.inbox_exclude.join(", ")` every frame and `.changed()` strips trailing commas and whitespace immediately, typing `,` after `suspect_subs` snaps back to `"suspect_subs"` on the next frame. Users cannot type a comma-separated list interactively in Settings.
- **Recommendation:** Store the raw `inbox_exclude_text: String` buffer on `App` (initialized from `cfg.inbox_exclude.join(", ")`) and parse it into `cfg_edit.inbox_exclude` when saving settings.

---

### M-2. `move_to_inbox` matches ancestor directories against `inbox_exclude` and prunes excluded subdirectories

- **Location:** `src/batch.rs:848-926`
- **Details:**
  1. `excluded_path` inspects `e.path().components()`, which is the full path from filesystem root. If an ancestor directory *above* `entry` happens to share a name with an entry in `inbox_exclude`, every file under `entry` is silently skipped without being reported in `report.excluded`.
  2. `prune_except` (`src/batch.rs:910-926`) filters only `!excluded_name(e.file_name())` on each directory's leaf name, so **empty subdirectories inside an excluded folder** (e.g. `suspect_subs/empty/`) are deleted despite the contract that excluded folders and everything inside them stay untouched.
  3. `move_to_inbox` does not reject the case where `inbox` is inside one of `entries` (which would walk `inbox` itself and try to move its files into a nested copy).
- **Recommendation:**
  - Compute `excluded_path` on `e.path().strip_prefix(entry)` only.
  - In `prune_except`, use `WalkDir::new(root).into_iter().filter_entry(|e| !excluded_name(e.file_name()))` (or check ancestors relative to `root`) so excluded trees are never descended into.
  - Check canonicalized paths to ensure `inbox` is not inside any `entry`.

---

### M-3. Trailing slash breaks SQLite prefix queries in `layout_plan` and `ingest_folder`

- **Locations:** `src/batch.rs:285-286`, `src/ingest.rs:367-372`, `src/ingest.rs:490-495`
- **Details:**
  `Path::new("/mnt/nas/repo/").to_string_lossy()` preserves trailing slashes on Unix. `format!("{}/", util::normalize_path(repo))` produces `"/mnt/nas/repo//"`, causing `substr(fitsFileName, 1, length(?1)) = ?1` to match **zero rows**. (By contrast, `forget_unfiled` at `src/ingest.rs:392` correctly uses `.trim_end_matches('/')`.)
  - If `repo` in `astrofiler.ini` ends with `/`, `migrate-layout` misses every catalogue record for object-name/restack planning.
  - If `--source /path/to/in/` is passed with a trailing slash, `ingest_folder` fails to match earlier-loaded frames for pictures.
- **Recommendation:** Extract a shared helper `util::path_prefix(p: &Path) -> String` that trims trailing `/` before appending `/` (handling root `/` safely).

---

### M-4. FITS card truncation corrupts long string headers and `CONTINUE` escaped quotes

- **Locations:** `src/fits.rs:213-254`, `src/fits.rs:376-398`
- **Details:**
  1. `format_card()` formats `Value::Str(v)` as `KEY     = '...'` and then truncates the whole card to 80 bytes (`bytes[..n]`). For strings longer than 68 characters (fewer when containing `'` escaped as `''`), the closing quote `'` is cut off, writing an **invalid FITS card** (and losing the trailing text).
  2. Keys longer than 8 characters are not truncated/rejected, shifting `= ` past columns 9–10 and turning value cards into commentary cards on re-read.
  3. `merge_continue_cards()` splits on the first `'` (`t.strip_prefix('\'').and_then(|t| t.split('\'').next())`), corrupting any `CONTINUE` string containing escaped quotes `''`.
- **Recommendation:**
  - Either emit FITS `CONTINUE` cards for long strings or cap the string value length so the closing `'` always fits within column 80.
  - Enforce 8-char uppercase FITS keyword formatting in `format_card`.
  - Reuse the escaped-quote parser from `parse_card` in `merge_continue_cards`.

---

### M-5. Integer overflow, OOM, and UTF-8 slice panic vectors in FITS, XISF, and DWARF parsers

- **Locations:** `src/fits.rs:165-188, 561-577, 623`, `src/xisf.rs:380-430, 470-540`, `src/telescope/dwarf.rs:367-369, 393`
- **Details:**
  - **FITS (`src/fits.rs`):**
    - `Header::naxes()` iterates `1..=NAXIS` with no upper bound (a corrupt `NAXIS=1000000000` hangs/OOMs; FITS standard caps `NAXIS` at 999).
    - `data_len()` and `read_rows()` perform unchecked multiplications (`product()`, `nrows * width`, `count * bpp`).
    - `decode_pixels()` silently returns an empty `Vec` for unsupported `BITPIX` instead of returning an error.
  - **XISF (`src/xisf.rs`):**
    - `block_bytes()` computes `pos + len` with unchecked addition (overflow panic on crafted `location="attachment:..."`) and slices hex `&text[i..i + 2]` (panics on odd length or non-ASCII multi-byte UTF-8 chars).
    - `read()` computes `shape.len() * item_size` with unchecked multiplication, and `decompress()` trusts the XML-supplied `size` for `Vec::with_capacity(size)` / `zstd::bulk::decompress`.
  - **DWARF (`src/telescope/dwarf.rs:367-369, 393`):**
    - `is_date` checks `t.len() >= 10 && t.as_bytes()[4] == b'-'` and line 393 slices `&t[..10]`. If bytes `5..10` contain multi-byte UTF-8 characters (e.g. `2026-ééééé`), byte index 10 can land inside a UTF-8 code point and **panic at runtime**.
- **Recommendation:** Use checked arithmetic (`checked_mul`, `checked_add`), cap `NAXIS <= 999`, validate `BITPIX` explicitly, validate ASCII/even-length hex before slicing, and use `t.get(..10)` (or `chars().take(10)`) in `RawFolder::parse`.

---

### M-6. Stale checksum stored in catalogue when header rewrite fails

- **Location:** `src/ingest.rs:1044-1059`, `src/ingest.rs:1353-1357`
- **Details:**
  When `save_modified_headers` is enabled, `prepare()` computes `hash = fits::sha256_with_header(&st.path, &header)?` (the hash the file *will* have after rewriting). In `file_prepared()`, if `fits::rewrite_primary_header(&dest, &header)` fails (for example, on a digitally signed XISF file or transient I/O error), only a warning is logged, yet `file_record()` still inserts the post-rewrite `hash`. Subsequent `astrofiler verify --hash` runs then report the file as corrupted.
- **Recommendation:** If `rewrite_primary_header` returns `Err`, recompute `util::sha256_file(&dest)` (or fall back to the original file hash) before inserting the database row.

---

### M-7. Duplicate removal compares raw path strings and discards deletion errors

- **Location:** `src/batch.rs:519-542`, `src/batch.rs:1096-1152`
- **Details:**
  1. `remove_duplicates()` guards against deleting the kept copy via string equality `f.name == group[keep].name` (`src/batch.rs:1118, 1133`). If two catalogue rows point to the same underlying file via different path spellings (symlinks, `/var` vs `/private/var`, case differences on macOS/Windows, or hardlinks), `remove_duplicates` will **unlink the only copy on disk**.
  2. `freed` is summed *before* calling `delete_files`, and the error list returned by `delete_files` (`let (n, _) = ...`) is silently discarded — reporting freed space for files that failed to delete and hiding deletion failures.
  3. `delete_files()` (`src/batch.rs:524`) uses a `Deferred` transaction (`conn.transaction()`); if the database is locked at commit time, files on disk have already been deleted.
- **Recommendation:**
  - Compare file identity with `same_file::is_same_file` (falling back to canonicalized path comparison).
  - Use `TransactionBehavior::Immediate` in `delete_files`, compute `freed` only for files actually removed, and log/propagate deletion errors.

---

### M-8. Uncanonicalized repo exclusions and premature `forget_unfiled` in `ingest_folder`

- **Locations:** `src/ingest.rs:254-267, 317-336, 391-406`
- **Details:**
  1. `ingest_folder` builds `excluded` using raw `cfg.repo.join(d)` and `collect_files` checks `e.path() == x` without canonicalizing either `source` or `cfg.repo` (unlike `managed_dir` at line 273). When `source` is relative (e.g. `.`) or passes through a symlink (such as `/var` -> `/private/var` on macOS), `excluded` fails to match and `collect_files` walks into the repository's own managed folders.
  2. `forget_unfiled()` (`src/ingest.rs:391-406`):
     - Excludes `Light`, `Stacked`, and `Calibrate`, but **omits `Archive`** (which is in `MANAGED_DIRS`).
     - Deletes rows from `fitsFile` **before** `ingest_files()` runs. If ingestion is cancelled or fails partway through, those in-place catalogued rows are already permanently gone.
  3. `seestar_companions()` (`src/ingest.rs:343-352, 381-385`) adds sibling `<base>` / `<base>_sub` / `<base>_mosaic_sub` folders without checking `managed_dir()` on those companions.
- **Recommendation:** Canonicalize paths for `excluded` and `managed_dir` checks (including on Seestar companions), include `Archive` in `forget_unfiled`, and delete/replace old in-place rows per-file inside `file_prepared` when each file is actually moved rather than up front.

---

### M-9. CLI silently ignores `--session` when `--object` is also supplied and drops `set` errors

- **Locations:** `src/main.rs:591-596`, `src/main.rs:739-753`
- **Details:**
  1. In `select_files()`:
     ```rust
     match (object, session) {
         (Some(o), _) => db::files_where(conn, "fitsFileObject=?1 AND COALESCE(fitsFileSoftDelete,0)=0", &[&o]),
         (None, Some(s)) => sessions::session_files(conn, s),
         (None, None) => bail!("select files with --object or --session"),
     }
     ```
     Running `astrofiler set ... --object "M 31" --session <id>` or `astrofiler export ... --object "M 31" --session <id>` silently ignores `--session` and operates on **every file of `"M 31"` across all sessions**.
  2. `Cmd::Set` (`src/main.rs:591-596`) prints `r.errors.len()` but never prints the error messages (unlike `Cmd::Merge` at lines 555–557).
- **Recommendation:** Either filter by **both** `fitsFileObject=?1 AND fitsFileSession=?2` when both flags are given, or mark them `conflicts_with` in `clap`. Print `r.errors` in `Cmd::Set`.

---

### M-10. Session sync, stats double-counting, and soft-delete inconsistencies

- **Locations:** `src/batch.rs:62, 226-242`, `src/sessions.rs:11-16, 111-185, 210-228`, `src/stats.rs:28-32, 50-95`, `src/db.rs:345-353, 395-423`
- **Details:**
  1. **Stale `fitsSession` on `set_field`:** `batch::EDITABLE` maps each editable card to its `fitsSession` column, and `column_for()` returns it, but `set_field()` discards it (`let (column, _) = column_for(card)?;`). Editing `FILTER`, `TELESCOP`, or `INSTRUME` never updates `fitsSession`. In `merge_objects`, `UPDATE fitsSession` runs outside `set_field`'s transaction (even if `set_field` partially errored), and `pictures::follow` is never called when `merge_objects` refiles files (stranding user pictures in the old object folder).
  2. **`stats::compute` double-counts stacked results & float edge cases:** Stacked results (`fitsFileStacked = 1`, whose `EXPTIME` holds total stack integration time) are counted alongside sub-frames in `light_files`, `by_object`, and `by_filter`, doubling reported integration hours. `is_light = t.contains("LIGHT")` is case-sensitive (unlike `util::is_light`), and `sorted()` uses `partial_cmp().unwrap_or(Equal)` while `exp` does not guard against `NaN`/non-finite values.
  3. **Session creation edge cases (`src/sessions.rs`):**
     - `create_all` never merges unassigned files into existing matching sessions — loading a second batch from the same night creates duplicate sessions.
     - `day()` uses calendar date (`YYYY-MM-DD`), splitting sessions that cross midnight into two sessions.
     - Light sessions group by `(object, day, filter)` without `telescope`/`instrument`, merging frames from different rigs on the same night into one session.
     - `sessions::delete` executes `UPDATE` and `DELETE` outside a transaction.
  4. **Soft-delete inconsistency:** `db::hash_exists`, `db::all_sessions` (file-count subquery), `sessions::session_files`, and `batch::merge_objects` do not filter `COALESCE(fitsFileSoftDelete, 0) = 0`.
- **Recommendation:** Filter `fitsFileStacked = 0` (or report stacks separately) and validate `exp.is_finite() && exp >= 0.0` in `stats::compute`; update `fitsSession` and call `pictures::follow` consistently in `batch`; filter `COALESCE(fitsFileSoftDelete, 0) = 0` consistently across DB queries; wrap `sessions::delete` in a transaction.

---

### M-11. Telescope transport timeouts, path validation, and GUI folder-deletion hazard

- **Locations:** `src/telescope/transport.rs:58-157`, `src/telescope/mod.rs:219-255, 347-407, 525-592`, `src/gui/mod.rs:638-657`
- **Details:**
  1. **No SMB timeouts (`src/telescope/transport.rs:98-157`):** Unlike `FtpTransport` (10s connect, 60s read timeout), `SmbTransport` uses no `tokio::time::timeout` on connect, list, download, or delete. A dropped Wi-Fi telescope hangs the worker thread indefinitely.
  2. **Unchecked `LocalTransport` paths & `is_safe_name(".")`:** `LocalTransport` passes `path`/`dir` straight to `self.root.join(...)`, where an absolute path replaces `self.root` and `..` escapes it. `is_safe_name()` (`src/telescope/mod.rs:387`) rejects `""` and `".."` but allows `"."`.
  3. **`.part` cleanup & clobbering on truncated download (`src/telescope/mod.rs:231-247`):** Failed downloads leave `<name>.part` behind; size verification runs *after* `rename(&tmp, &local)`, which overwrites any pre-existing valid local file before deleting it.
  4. **GUI "Delete selected" wipes unlisted stacked results (`src/gui/mod.rs:638-657`):** When `"Include the telescope's own stacked results"` is unchecked, scanning a DWARF folder lists only sub-frames. Selecting all listed files triggers `picked.len() == idx.len()` and calls `remove_tree` on the entire remote folder — **permanently deleting unlisted `stacked-*.fits` and previews**.
  5. **`find_network` cancellation & offline hotspot (`src/telescope/mod.rs:525-592`):** Ignores `progress.cancelled()`, spawns 96 threads for the `/24` scan, and `local_ipv4()` relies on connecting a UDP socket to `8.8.8.8:80` (which can fail when connected only to a telescope hotspot with no default route).
- **Recommendation:**
  - Wrap `SmbTransport` async operations in `tokio::time::timeout`.
  - Validate that `LocalTransport` paths stay inside `self.root`, and reject `"."` in `is_safe_name`.
  - Check `tmp` size *before* renaming to `local`, and delete `tmp` on error.
  - Only offer whole-folder `remove_tree` deletion when `include_stacked` is enabled (or explicitly delete only the ticked files when stacked results are hidden).
  - Check `progress.cancelled()` in `find_network`.

---

### M-12. `clean_previews` deletes user's processed JPG/PNG pictures

- **Locations:** `src/batch.rs:1025-1033`, `src/main.rs:296-329`
- **Details:**
  `batch::is_preview()` matches **every** `.jpg`, `.jpeg`, and `.png` file except `util::is_stack_preview_name()`, regardless of whether it is a telescope preview or a user-processed picture (`pictures::is_picture()`). While `main.rs` prompts `[y/N]`, it only prints the first 20 filenames (`... and N more`), making it easy to accidentally wipe user-processed JPG/PNG images when running `clean-previews` on an archive or repository (especially since the README describes it as deleting "leftover telescope JPG/PNG previews and thumbnails").
- **Recommendation:** Exclude `pictures::is_picture(p)` by default in `clean_previews` (or explicitly flag non-telescope pictures separately in the confirmation prompt).

---

## Low-Severity Findings

### L-1. Non-atomic config writes and plaintext password permissions

- **Location:** `src/config.rs:181-237`, `src/main.rs:841`
- **Details:**
  - `Config::save()` writes directly via `ini.write_to_file(&self.path)` without atomic temp-file + rename; an interruption can truncate `astrofiler.ini`.
  - `seestar_password` is stored in plaintext with default umask permissions (world/group-readable on many Unix systems), and `astrofiler config set seestar_password <pw>` exposes the password in shell history and process arguments.
- **Recommendation:** Write to a sibling temp file, set `0o600` permissions on Unix, and atomically rename over `self.path`.

---

### L-2. Mutex poison panics and per-frame cloning in GUI and logger

- **Locations:** `src/logging.rs:57-93`, `src/progress.rs:64-76`, `src/gui/mod.rs:425, 557, 859-907, 1246`
- **Details:**
  - `logging.rs`, `progress.rs`, and `gui/mod.rs` call `.lock().unwrap()` (accounting for most of the 32 non-test `.unwrap()` calls in `src/`). If any thread panics while holding the logger mutex, every subsequent `log::*!` call will panic.
  - `telescopes_tab`, `inbox_tab`, and `log_tab` clone entire `Vec<RemoteFile>`, `Vec<LocalEntry>`, and up to 5,000 `String` log lines **every rendered frame**. `telescopes_tab` also sorts dates and formats local timestamps for every folder row every frame.
  - `list_local()` spawns detached threads without a generation counter, so rapid refreshes or folder switches can finish out of order and overwrite `self.local` with stale entries.
- **Recommendation:** Use `.lock().unwrap_or_else(|e| e.into_inner())` for the logger/UI mutexes, keep an `Arc` or generation counter in `App` to avoid per-frame clones and stale `list_local` overwrites, and precompute folder summaries when scan results arrive.

---

### L-3. Symlink cycle, size-only dedup, and over-broad ancestor moves in `pictures.rs`

- **Location:** `src/pictures.rs:33-47, 156-172, 196-228`
- **Details:**
  - `pictures_under()` uses `p.is_dir()`, which follows symlinks and will recurse infinitely on a symlink loop (contrast `nick::names_under`, which uses `e.file_type().is_ok_and(|t| t.is_dir())`).
  - `pictures::file()` considers two pictures identical solely by equal byte length (`same_size`), which can falsely match uncompressed TIFFs of identical resolution and leaves source pictures behind in `Move` mode (blocking empty-folder cleanup).
  - `pictures::follow()` pairs ancestor directories up to 4 levels up (`skip(1).take(4)`) and moves pictures from all of them, even if other frames remain in those parent directories.
- **Recommendation:** Use `e.file_type()` in `pictures_under`, compare content/SHA-256 when sizes match in `pictures::file`, and check whether a directory still contains frames (as `batch::move_sidecars` does) before moving pictures out of shared ancestors.

---

### L-4. Compiler/Clippy warnings, CI hardening, and maintenance items

- **Locations:** `src/util.rs:74, 92`, `src/fits.rs:362, 597-620, 635`, `src/names.rs:279, 308`, `src/nick.rs:80`, `src/pictures.rs:108, 114, 179, 181`, `src/gui/mod.rs:541`, `src/xisf.rs:248-252`, `src/main.rs:633, 650`, `.github/workflows/ci.yml`
- **Details:**
  - **Compiler warnings (non-Linux):** `unescape_mounts` and `gvfs_to_mount` in `src/util.rs` trigger `dead_code` warnings on macOS/Windows; gate them with `#[cfg(any(target_os = "linux", test))]`.
  - **Clippy warnings (15 total):** `needless_borrow` in `src/pictures.rs` (4), `chunks_exact_to_as_chunks` in `src/fits.rs` (6), `len_without_is_empty` on `ImageShape` (`src/fits.rs:635`), `useless_borrows_in_formatting` (`src/names.rs:308`), `useless_format` (`src/gui/mod.rs:541`), plus the 2 `dead_code` warnings above.
  - **CI workflow (`.github/workflows/ci.yml`):**
    - Sets `permissions: contents: write` at the workflow top level instead of scoping it only to a dedicated release job.
    - Uses mutable action tags (`@v7`, `@v3`, `@v2`) instead of commit SHAs.
    - Does not run `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, or `cargo test --no-default-features`.
  - **Minor code items:**
    - `Cmd::Verify --prune` (`src/main.rs:633, 650`) verifies the entire repository twice instead of reusing `r.missing`.
    - `designation()` is duplicated between `src/names.rs:279` and `src/nick.rs:80`.
    - `xisf::plan_rewrite()` (`src/xisf.rs:248-252`) silently skips updating an empty `<Property ...></Property>` element that has neither a `value` attribute nor an existing text node.
    - `Cargo.lock` resolves two major versions of `lz4_flex` (`0.11.6` and `0.14.0`) and `sha2` (`0.10.9` and `0.11.0`) due to version mismatch with `smb2`.
    - Local working tree notes: `.gitignore` has an uncommitted local addition (`+.DS_Store`), and `dist/` contains an untracked older build (`AstroFiler-0.8.2-arm64.dmg` and `AstroFiler.app`).

---

## Resolution (`v0.8.9`)

All 22 findings above were addressed in `v0.8.9` with regression unit tests, zero compiler/Clippy warnings (`-D warnings`), and hardened CI:

- **High (`H-1`–`H-6`):** Hardened `util::sanitize` (`.`, `..`, control chars, trailing dots, Windows reserved names) and sanitized numeric FITS keywords + repository containment check in `ingest::destination`; fixed `util::copy_file` self-copy truncation and target deletion on source-open error; replaced Windows `cmd /C start` with `rundll32 url.dll,FileProtocolHandler` and enforced `http://`/`https://` in `App::open_web`; pre-assigned unique paths sequentially and added an `Immediate` transaction in `batch::export_files`; extracted archives into unique work dirs with bounded decompression and deferred deletion (preserving mixed-content `.zip` archives); closed the read handle before write/rename in `fits::rewrite_primary_header` and added unique temp paths with error cleanup across FITS/XISF/ingest.
- **Medium (`M-1`–`M-12`):** Added dedicated `inbox_exclude_text` buffer in GUI Settings; scoped `move_to_inbox` / `prune_except` exclusion checks to paths relative to each moved entry and guarded against `inbox` inside `entry`; added `util::path_prefix` for trailing-slash safety; fixed FITS long-string card truncation and `CONTINUE` escaped quotes; added checked arithmetic, `NAXIS <= 999`, `BITPIX` validation, hex/decompression bounds, and ASCII-safe date slicing in FITS/XISF/DWARF; recomputed hash when header rewrite fails; used file-identity checks (`util::same_file`) and `Immediate` transactions in duplicate/file deletion; canonicalized repo exclusions and added `Archive` to `forget_unfiled`; supported combined `--object` + `--session` and error printing in `main.rs`; synced `fitsSession` on `set_field`, called `pictures::follow` in `merge_objects`, avoided double-counting stacked frames in `stats::compute`, and filtered soft-deleted rows consistently; added SMB timeouts, `LocalTransport::safe_join`, size-check-before-rename on downloads, cancellation in `find_network`, and `include_stacked` guard on whole-folder GUI deletes; added explicit picture warnings in `clean-previews`.
- **Low (`L-1`–`L-4`):** Made `Config::save` atomic with `0o600` permissions on Unix; replaced `.lock().unwrap()` with poison-safe locking and added a generation counter to `list_local`; avoided symlink recursion and size-only dedup in `pictures.rs`; resolved all 15 Clippy and 2 dead-code warnings, deduplicated `split_designation`, removed duplicate `verify` scan on `--prune`, handled empty XISF `<Property>` elements, and hardened `.github/workflows/ci.yml` with scoped permissions, separate `release` job, `fmt`, `clippy -D warnings`, and `--no-default-features` tests.

## Prioritized Remediation Order

1. **Immediate (Security & Data Loss):** Fix `H-1` (sanitize `..` & numeric FITS fields), `H-2` (`util::copy_file` error-cleanup & self-copy), `H-3` (Windows `open_external` & URL scheme validation), `H-4` (`export_files` `unique_path` race & DB lock), and `H-6` (`fits::rewrite_primary_header` file handle drop).
2. **Next Release (`v0.8.9`):** Fix `M-1` (GUI `inbox_exclude` text editing), `M-2` (`move_to_inbox` ancestor exclude & `prune_except`), `M-3` (trailing-slash SQLite prefix helper), `H-5` (`unpack` archive safety & decompression caps), and `M-11` (SMB timeouts & GUI delete-with-hidden-stacks guard).
3. **Hardening & Hygiene:** Address `M-4` through `M-10`, `M-12`, and `L-1` through `L-4`, and add `clippy`, `fmt --check`, and `--no-default-features` to CI.
