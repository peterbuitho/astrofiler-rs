use anyhow::{anyhow, bail, Result};
use astrofiler::batch::{self, EditOptions, ExportLayout};
use astrofiler::config::Config;
use astrofiler::progress::{BarProgress, NoProgress};
use astrofiler::telescope::{self, Link};
use astrofiler::{db, fits, ingest, logging, masters, sessions, stats, util, xisf};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "astrofiler",
    version,
    about = "Fast astronomical image filing and cataloguing (Rust port of AstroFiler)"
)]
struct Cli {
    /// Config file (default: ./astrofiler.ini, else ~/.config/astrofiler/astrofiler.ini)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Database file (overrides config)
    #[arg(long, global = true)]
    db: Option<PathBuf>,
    /// Verbose logging
    #[arg(short, long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Open the desktop app (default when no command is given)
    Gui,
    /// Load images from a folder (incoming, or an existing archive on a NAS):
    /// rename, file into the repository and catalogue them
    Load {
        /// Folder to load from (default: configured source)
        #[arg(long)]
        source: Option<PathBuf>,
        /// Put renamed copies in the repository and leave the originals untouched
        #[arg(long, conflicts_with = "no_move")]
        copy: bool,
        /// Catalogue files where they are (no renaming or moving)
        #[arg(long)]
        no_move: bool,
        /// Show where files would go without changing anything
        #[arg(long)]
        dry_run: bool,
        /// Write the full source -> destination plan to this CSV file
        #[arg(long)]
        plan: Option<PathBuf>,
        /// When a different file already has the destination name: skip,
        /// overwrite or keep-both (default: the on_conflict setting, "skip").
        /// Empty or partly copied files are always replaced.
        #[arg(long, value_parser = parse_conflict)]
        on_conflict: Option<ingest::OnConflict>,
    },
    /// Catalogue files already in the repository without moving them
    Sync,
    /// Rebuild the catalogue from scratch by rescanning the repository
    Regenerate,
    /// Find Seestar / DWARF telescopes on USB and on the network
    Find {
        /// Also scan the local network (slower)
        #[arg(long)]
        network: bool,
    },
    /// Import FITS files from a smart telescope (preview JPG/PNGs are skipped)
    Import {
        /// Telescope: seestar or dwarf
        scope: String,
        /// Wi-Fi host name or IP (default from config)
        #[arg(long, conflicts_with = "usb")]
        host: Option<String>,
        /// Folder where the telescope is mounted over USB-C ("auto" to detect)
        #[arg(long)]
        usb: Option<String>,
        /// Only list what would be imported
        #[arg(long)]
        list: bool,
        /// Delete files from the telescope after they are safely catalogued
        #[arg(long)]
        delete: bool,
        /// Skip Seestar Stacked_*.fit results
        #[arg(long)]
        no_stacked: bool,
        /// Download folder before filing (default: configured source)
        #[arg(long)]
        dest: Option<PathBuf>,
    },
    /// Session grouping
    Sessions {
        #[command(subcommand)]
        action: SessionCmd,
    },
    /// Master calibration frames
    Masters {
        #[command(subcommand)]
        action: MasterCmd,
    },
    /// Rename an object everywhere (e.g. "Andromeda" -> "M 31")
    Merge {
        from: String,
        to: String,
        /// Also rewrite OBJECT in the FITS headers
        #[arg(long)]
        headers: bool,
        /// Also rename and move files to match
        #[arg(long)]
        refile: bool,
    },
    /// Set a header field on many files at once
    Set {
        /// OBJECT, FILTER, TELESCOP, INSTRUME, OBSERVER or NOTES
        field: String,
        value: String,
        /// Select files whose OBJECT equals this
        #[arg(long)]
        object: Option<String>,
        /// Select files in this session
        #[arg(long)]
        session: Option<String>,
        /// Select files whose current value of FIELD equals this
        #[arg(long)]
        current: Option<String>,
        #[arg(long)]
        headers: bool,
        #[arg(long)]
        refile: bool,
    },
    /// Copy (or move) files out of the repository
    Export {
        dest: PathBuf,
        #[arg(long)]
        object: Option<String>,
        #[arg(long)]
        session: Option<String>,
        /// Arrange as <dest>/<object>/<filter>/
        #[arg(long)]
        by_object: bool,
        #[arg(long = "move")]
        move_files: bool,
    },
    /// List (or remove) duplicate files
    Duplicates {
        #[arg(long)]
        remove: bool,
    },
    /// Check catalogued files exist (and optionally match their hash)
    Verify {
        #[arg(long)]
        hash: bool,
        /// Remove catalogue entries for missing files
        #[arg(long)]
        prune: bool,
    },
    /// Delete JPG/PNG preview images (and empty Thumbnail folders) under a folder
    CleanPreviews {
        dir: PathBuf,
        /// Only show what would be deleted
        #[arg(long)]
        dry_run: bool,
    },
    /// Repository statistics
    Stats,
    /// Header value mappings applied on import
    Mapping {
        #[command(subcommand)]
        action: MappingCmd,
    },
    /// Print a FITS or XISF header
    Header { file: PathBuf },
    /// Convert an XISF file to FITS
    Convert {
        input: PathBuf,
        output: Option<PathBuf>,
    },
    /// Show or change configuration
    Config {
        #[command(subcommand)]
        action: Option<ConfigCmd>,
    },
}

#[derive(Subcommand)]
enum SessionCmd {
    /// Group unassigned files into sessions
    Create,
    /// List sessions
    List,
    /// Remove all sessions
    Clear,
}

#[derive(Subcommand)]
enum MasterCmd {
    List,
    /// Build masters for calibration sessions (all missing ones, or one session)
    Create {
        #[arg(long)]
        session: Option<String>,
    },
    /// Register existing master files from a folder
    Register {
        dir: PathBuf,
        /// Move them into <repo>/Masters
        #[arg(long = "move")]
        move_files: bool,
    },
    /// Check master files exist and match their checksum
    Validate,
    /// Forget masters whose files are gone
    Cleanup,
}

#[derive(Subcommand)]
enum MappingCmd {
    List,
    /// Map CARD value CURRENT to REPLACE (empty CURRENT = default for missing values)
    Add {
        card: String,
        current: String,
        replace: String,
    },
    Remove {
        id: i64,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    Show,
    /// Set a key: source, repo, save_modified_headers, external_viewer, theme,
    /// min_master_files, sigma_clip, database, seestar_host, seestar_username,
    /// seestar_password, dwarf_host, include_stacked, ui_scale (auto or e.g. 1.5)
    Set {
        key: String,
        value: String,
    },
}

fn main() {
    let cli = Cli::parse();
    let gui = matches!(cli.command, None | Some(Cmd::Gui));
    logging::init(cli.verbose, !gui || cli.verbose);
    if let Err(e) = run(cli) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let mut cfg = match &cli.config {
        Some(p) => Config::load_from(p)?,
        None => Config::load()?,
    };
    if let Some(p) = &cli.db {
        cfg.database = Some(p.clone());
    }
    let command = cli.command.unwrap_or(Cmd::Gui);
    if let Cmd::Gui = command {
        #[cfg(feature = "gui")]
        return astrofiler::gui::run(cfg);
        #[cfg(not(feature = "gui"))]
        bail!("built without the GUI; run `astrofiler --help` for commands");
    }
    if let Cmd::Config { action } = &command {
        return config_cmd(&mut cfg, action.as_ref());
    }
    if let Cmd::Header { file } = &command {
        let h = if file
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("xisf"))
        {
            xisf::read(file)?.header
        } else {
            fits::read_primary_header(file)?
        };
        for c in &h.cards {
            match &c.value {
                fits::Value::Commentary(t) => println!("{:<8} {t}", c.key),
                v => println!(
                    "{:<8}= {:<30} {}",
                    c.key,
                    v.to_py_string(),
                    if c.comment.is_empty() {
                        String::new()
                    } else {
                        format!("/ {}", c.comment)
                    }
                ),
            }
        }
        return Ok(());
    }
    if let Cmd::Convert { input, output } = &command {
        let out = output
            .clone()
            .unwrap_or_else(|| input.with_extension("fits"));
        xisf::convert_to_fits(input, &out)?;
        println!("wrote {}", out.display());
        return Ok(());
    }
    if let Cmd::CleanPreviews { dir, dry_run } = &command {
        let (files, bytes) = batch::clean_previews(dir, *dry_run)?;
        for f in files.iter().take(20) {
            println!("{}", f.display());
        }
        if files.len() > 20 {
            println!("... and {} more", files.len() - 20);
        }
        let verb = if *dry_run { "would delete" } else { "deleted" };
        println!(
            "{verb} {} preview files ({})",
            files.len(),
            util::human_size(bytes)
        );
        return Ok(());
    }
    if let Cmd::Find { network } = &command {
        let mut found = telescope::find_usb();
        if *network {
            for t in telescope::all() {
                found.extend(telescope::find_network(&cfg, *t, &NoProgress));
            }
        }
        if found.is_empty() {
            println!(
                "No telescopes found{}.",
                if *network {
                    ""
                } else {
                    " on USB (add --network to scan Wi-Fi)"
                }
            );
        }
        for f in found {
            println!("{}", f.label);
        }
        return Ok(());
    }

    let db_path = cfg.database_path();
    log::debug!("database: {}", db_path.display());
    let mut conn = db::open(&db_path)?;
    let bar = BarProgress::new();

    match command {
        Cmd::Load {
            source,
            copy,
            no_move,
            dry_run,
            plan,
            on_conflict,
        } => {
            let src = source.unwrap_or_else(|| cfg.source.clone());
            let placement = if no_move {
                ingest::Placement::InPlace
            } else if copy {
                ingest::Placement::Copy
            } else {
                ingest::Placement::Move
            };
            let t = std::time::Instant::now();
            let r = ingest::ingest_folder(
                &mut conn,
                &cfg,
                &src,
                ingest::IngestOptions {
                    placement,
                    dry_run,
                    on_conflict: on_conflict.unwrap_or(cfg.on_conflict),
                },
                &bar,
            )?;
            bar.finish();
            if dry_run {
                for (a, b) in r.placed.iter().take(40) {
                    println!("{}\n    -> {}", a.display(), b.display());
                }
                if r.placed.len() > 40 {
                    println!(
                        "... and {} more (use --plan FILE.csv for the full list)",
                        r.placed.len() - 40
                    );
                }
            }
            if let Some(p) = plan {
                r.write_plan_csv(&p)?;
                println!("plan written to {}", p.display());
            }
            print_ingest(&r);
            println!("done in {:.1}s", t.elapsed().as_secs_f64());
        }
        Cmd::Sync => {
            let r = ingest::ingest_folder(
                &mut conn,
                &cfg,
                &cfg.repo.clone(),
                ingest::IngestOptions::IN_PLACE,
                &bar,
            )?;
            bar.finish();
            print_ingest(&r);
        }
        Cmd::Regenerate => {
            let r = batch::regenerate(&mut conn, &cfg, &bar)?;
            bar.finish();
            print_ingest(&r);
        }
        Cmd::Import {
            scope,
            host,
            usb,
            list,
            delete,
            no_stacked,
            dest,
        } => {
            let known: Vec<&str> = telescope::all().iter().map(|t| t.id()).collect();
            let scope = telescope::find(&scope).ok_or_else(|| {
                anyhow!(
                    "unknown telescope '{scope}' (supported: {})",
                    known.join(", ")
                )
            })?;
            let link = match usb.as_deref() {
                Some("auto") => telescope::find_usb()
                    .into_iter()
                    .find(|f| f.telescope.id() == scope.id())
                    .map(|f| f.link)
                    .ok_or_else(|| anyhow!("no {} found on USB", scope.name()))?,
                Some(p) => Link::Usb(PathBuf::from(p)),
                None => Link::Network(host.unwrap_or_else(|| scope.default_host(&cfg))),
            };
            println!("Connecting to {} ({link})...", scope.name());
            let mut session = telescope::connect(&cfg, scope, &link)?;
            let files = session.scan(cfg.include_stacked && !no_stacked)?;
            let size: u64 = files.iter().map(|f| f.size).sum();
            println!("{} files ({}) found", files.len(), util::human_size(size));
            if list {
                for f in &files {
                    println!("  [{}] {} ({})", f.kind, f.path, util::human_size(f.size));
                }
                return Ok(());
            }
            let dest = dest.unwrap_or_else(|| cfg.source.clone());
            let r = session.import(&mut conn, &cfg, &files, &dest, delete, &bar)?;
            bar.finish();
            println!("{}", r.summary());
            for (p, e) in &r.failed {
                println!("  failed: {p}: {e}");
            }
            print_ingest_errors(&r.ingest);
        }
        Cmd::Sessions { action } => match action {
            SessionCmd::Create => {
                let r = sessions::create_all(&mut conn, &bar)?;
                bar.finish();
                println!(
                    "created {} sessions ({} light, {} bias, {} dark, {} flat, {} flat-dark)",
                    r.total(),
                    r.light,
                    r.bias,
                    r.dark,
                    r.flat,
                    r.flat_dark
                );
            }
            SessionCmd::List => {
                println!(
                    "{:<36}  {:<20} {:<10} {:<8} {:>7} {:>5}  Telescope / Camera",
                    "ID", "Object", "Date", "Filter", "Exp", "Files"
                );
                for s in db::all_sessions(&conn)? {
                    println!(
                        "{:<36}  {:<20} {:<10} {:<8} {:>7} {:>5}  {} / {}",
                        s.id,
                        s.object.unwrap_or_default(),
                        s.date.unwrap_or_default(),
                        s.filter.unwrap_or_default(),
                        s.exposure.unwrap_or_default(),
                        s.file_count,
                        s.telescope.unwrap_or_default(),
                        s.imager.unwrap_or_default()
                    );
                }
            }
            SessionCmd::Clear => println!("removed {} sessions", sessions::clear_all(&mut conn)?),
        },
        Cmd::Masters { action } => match action {
            MasterCmd::List => {
                for m in db::masters(&conn, false)? {
                    println!(
                        "{:<9} {:>3} frames  {}  {}",
                        m.master_type,
                        m.file_count,
                        if m.validated { "ok " } else { "?  " },
                        m.path
                    );
                }
            }
            MasterCmd::Create { session } => {
                if let Some(id) = session {
                    let m = masters::create_from_session(&conn, &cfg, &id, &bar)?;
                    bar.finish();
                    println!("created {}", m.path);
                } else {
                    let (made, errors) = masters::create_missing(&conn, &cfg, &bar)?;
                    bar.finish();
                    for m in &made {
                        println!("created {}", m.path);
                    }
                    for (s, e) in &errors {
                        println!("session {s}: {e}");
                    }
                    println!("{} masters created, {} failed", made.len(), errors.len());
                }
            }
            MasterCmd::Register { dir, move_files } => {
                let (n, errors) =
                    masters::register_folder(&mut conn, &cfg, &dir, move_files, &bar)?;
                bar.finish();
                println!("registered {n} masters");
                for (p, e) in errors {
                    println!("  {}: {e}", p.display());
                }
            }
            MasterCmd::Validate => {
                let r = masters::validate(&conn, &bar)?;
                bar.finish();
                println!(
                    "{} ok, {} missing, {} changed/corrupt",
                    r.ok,
                    r.missing.len(),
                    r.corrupt.len()
                );
                for p in r.missing.iter().chain(&r.corrupt) {
                    println!("  {p}");
                }
            }
            MasterCmd::Cleanup => println!(
                "removed {} missing masters",
                masters::cleanup_missing(&conn)?.len()
            ),
        },
        Cmd::Merge {
            from,
            to,
            headers,
            refile,
        } => {
            let r = batch::merge_objects(
                &mut conn,
                &cfg,
                &from,
                &to,
                EditOptions {
                    update_headers: headers,
                    refile,
                },
                &bar,
            )?;
            bar.finish();
            println!(
                "{} files updated, {} moved, {} errors",
                r.updated,
                r.moved,
                r.errors.len()
            );
            for (p, e) in r.errors {
                println!("  {p}: {e}");
            }
        }
        Cmd::Set {
            field,
            value,
            object,
            session,
            current,
            headers,
            refile,
        } => {
            let files = select_files(&conn, object.as_deref(), session.as_deref())?;
            let ids: Vec<String> = files
                .into_iter()
                .filter(|f| {
                    current
                        .as_ref()
                        .is_none_or(|c| field_value(f, &field).as_deref() == Some(c.as_str()))
                })
                .map(|f| f.id)
                .collect();
            let r = batch::set_field(
                &mut conn,
                &cfg,
                &ids,
                &field,
                &value,
                EditOptions {
                    update_headers: headers,
                    refile,
                },
                &bar,
            )?;
            bar.finish();
            println!(
                "{} files updated, {} moved, {} errors",
                r.updated,
                r.moved,
                r.errors.len()
            );
        }
        Cmd::Export {
            dest,
            object,
            session,
            by_object,
            move_files,
        } => {
            let ids: Vec<String> = select_files(&conn, object.as_deref(), session.as_deref())?
                .into_iter()
                .map(|f| f.id)
                .collect();
            let layout = if by_object {
                ExportLayout::ByObject
            } else {
                ExportLayout::Flat
            };
            let n = batch::export_files(&mut conn, &ids, &dest, layout, move_files, &bar)?;
            bar.finish();
            println!("exported {n} files to {}", dest.display());
        }
        Cmd::Duplicates { remove } => {
            let groups = batch::duplicate_groups(&conn)?;
            for g in &groups {
                println!("{}", g[0].hash.as_deref().unwrap_or(""));
                for f in g {
                    println!("  {}", f.name);
                }
            }
            println!("{} duplicate groups", groups.len());
            if remove && !groups.is_empty() {
                let (n, bytes) = batch::remove_duplicates(&mut conn)?;
                println!("removed {n} files, freed {}", util::human_size(bytes));
            }
        }
        Cmd::Verify { hash, prune } => {
            let r = batch::verify(&conn, hash, &bar)?;
            bar.finish();
            for f in &r.missing {
                println!("missing:  {}", f.name);
            }
            for f in &r.mismatched {
                println!("changed:  {}", f.name);
            }
            println!(
                "{} checked, {} missing, {} changed",
                r.checked,
                r.missing.len(),
                r.mismatched.len()
            );
            if prune && !r.missing.is_empty() {
                println!(
                    "removed {} catalogue entries",
                    batch::remove_missing(&mut conn, &NoProgress)?
                );
            }
        }
        Cmd::Stats => {
            let s = stats::compute(&conn)?;
            println!(
                "Files:        {} ({} lights, {} calibration)",
                s.total_files, s.light_files, s.calibration_files
            );
            println!("Size:         {}", util::human_size(s.total_bytes));
            println!("Sessions:     {}", s.sessions);
            println!("Masters:      {}", s.masters);
            println!(
                "Date range:   {} .. {}",
                s.first_date.unwrap_or_default(),
                s.last_date.unwrap_or_default()
            );
            println!("\nTop objects by integration:");
            for (o, n, e) in s.by_object.iter().take(15) {
                println!("  {o:<24} {n:>6} frames  {}", stats::hours(*e));
            }
            println!("\nIntegration by filter:");
            for (f, e) in &s.by_filter {
                println!("  {f:<24} {}", stats::hours(*e));
            }
            println!("\nTelescopes:");
            for (t, n) in &s.by_telescope {
                println!("  {t:<24} {n}");
            }
        }
        Cmd::Mapping { action } => match action {
            MappingCmd::List => {
                for m in db::mappings(&conn)? {
                    println!(
                        "{:>4}  {:<9} '{}' -> '{}'",
                        m.id,
                        m.card,
                        m.current.unwrap_or_default(),
                        m.replace.unwrap_or_default()
                    );
                }
            }
            MappingCmd::Add {
                card,
                current,
                replace,
            } => {
                db::add_mapping(&conn, &card, &current, &replace)?;
                println!("mapping saved");
            }
            MappingCmd::Remove { id } => db::remove_mapping(&conn, id)?,
        },
        Cmd::Gui
        | Cmd::Config { .. }
        | Cmd::Header { .. }
        | Cmd::Convert { .. }
        | Cmd::CleanPreviews { .. }
        | Cmd::Find { .. } => unreachable!(),
    }
    Ok(())
}

fn select_files(
    conn: &rusqlite::Connection,
    object: Option<&str>,
    session: Option<&str>,
) -> Result<Vec<db::FitsFile>> {
    match (object, session) {
        (Some(o), _) => db::files_where(
            conn,
            "fitsFileObject=?1 AND COALESCE(fitsFileSoftDelete,0)=0",
            &[&o],
        ),
        (None, Some(s)) => sessions::session_files(conn, s),
        (None, None) => bail!("select files with --object or --session"),
    }
}

fn field_value(f: &db::FitsFile, field: &str) -> Option<String> {
    match field.to_uppercase().as_str() {
        "OBJECT" => f.object.clone(),
        "FILTER" => f.filter.clone(),
        "TELESCOP" => f.telescope.clone(),
        "INSTRUME" => f.instrument.clone(),
        "OBSERVER" => f.observer.clone(),
        "NOTES" => f.notes.clone(),
        _ => None,
    }
}

fn parse_conflict(s: &str) -> Result<ingest::OnConflict, String> {
    ingest::OnConflict::parse(s).ok_or_else(|| "expected skip, overwrite or keep-both".into())
}

fn print_ingest(r: &ingest::IngestReport) {
    println!("{}", r.summary());
    for (p, existing) in r.duplicates.iter().take(10) {
        println!("  duplicate: {} (same as {existing})", p.display());
    }
    for (p, existing) in r.conflicts.iter().take(10) {
        println!(
            "  skipped: {} (a different file already exists at {}; use --on-conflict overwrite or keep-both)",
            p.display(),
            existing.display()
        );
    }
    print_ingest_errors(r);
}

fn print_ingest_errors(r: &ingest::IngestReport) {
    for (p, e) in r.errors.iter().take(25) {
        println!("  error: {}: {e}", p.display());
    }
    if r.errors.len() > 25 {
        println!(
            "  ... {} more errors (see log with -v)",
            r.errors.len() - 25
        );
    }
}

fn config_cmd(cfg: &mut Config, action: Option<&ConfigCmd>) -> Result<()> {
    match action {
        None | Some(ConfigCmd::Show) => {
            println!("config file:           {}", cfg.path.display());
            println!("database:              {}", cfg.database_path().display());
            println!("source:                {}", cfg.source.display());
            println!("repo:                  {}", cfg.repo.display());
            println!("save_modified_headers: {}", cfg.save_modified_headers);
            println!("external_viewer:       {}", cfg.external_viewer);
            println!("theme:                 {}", cfg.theme);
            println!("min_master_files:      {}", cfg.min_master_files);
            println!("sigma_clip:            {}", cfg.sigma_clip);
            println!("seestar_host:          {}", cfg.seestar_host);
            println!("seestar_username:      {}", cfg.seestar_username);
            println!("dwarf_host:            {}", cfg.dwarf_host);
            println!("include_stacked:       {}", cfg.include_stacked);
            println!("ui_scale:              {}", cfg.ui_scale);
            println!("on_conflict:           {}", cfg.on_conflict.key());
        }
        Some(ConfigCmd::Set { key, value }) => {
            let b = || matches!(value.to_lowercase().as_str(), "1" | "true" | "yes" | "on");
            match key.as_str() {
                "source" => cfg.source = value.into(),
                "repo" => cfg.repo = value.into(),
                "save_modified_headers" => cfg.save_modified_headers = b(),
                "external_viewer" => cfg.external_viewer = value.clone(),
                "theme" => cfg.theme = value.clone(),
                "min_master_files" => cfg.min_master_files = value.parse()?,
                "sigma_clip" => cfg.sigma_clip = value.parse()?,
                "database" => {
                    cfg.database =
                        Some(value.into()).filter(|p: &PathBuf| !p.as_os_str().is_empty())
                }
                "seestar_host" => cfg.seestar_host = value.clone(),
                "seestar_username" => cfg.seestar_username = value.clone(),
                "seestar_password" => cfg.seestar_password = value.clone(),
                "dwarf_host" => cfg.dwarf_host = value.clone(),
                "include_stacked" => cfg.include_stacked = b(),
                "ui_scale" => cfg.ui_scale = value.to_lowercase(),
                "on_conflict" => {
                    cfg.on_conflict = parse_conflict(value).map_err(anyhow::Error::msg)?
                }
                other => bail!("unknown key '{other}'"),
            }
            cfg.save()?;
            println!("saved {}", cfg.path.display());
        }
    }
    Ok(())
}
