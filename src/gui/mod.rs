//! Desktop GUI (egui). Long operations run on background threads with their
//! own database connection; the UI polls a shared [`JobState`] for progress.

mod picker;
mod preview;

use crate::batch::{self, EditOptions, ExportLayout};
use crate::config::Config;
use crate::db::{self, FitsFile, Mapping, Master, Session};
use crate::ingest::{self, IngestOptions, OnConflict, Placement};
use crate::progress::{JobState, Progress};
use crate::stats::{self, Stats};
use crate::telescope::{self, Found, Link, RemoteFile};
use crate::util::{self, FrameKind};
use crate::{logging, masters, names, sessions};
use anyhow::Result;
use eframe::egui::{self, Align2, Color32, RichText};
use egui_extras::{Column, TableBuilder};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

pub fn run(cfg: Config) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: {
            // The app id ties the window to astrofiler.desktop on Wayland; the
            // icon is used on X11, Windows and macOS.
            let vb = egui::ViewportBuilder::default()
                .with_inner_size([1400.0, 880.0])
                .with_min_inner_size([900.0, 600.0])
                .with_title("AstroFiler")
                .with_app_id("astrofiler");
            match eframe::icon_data::from_png_bytes(include_bytes!("../../assets/astrofiler.png")) {
                Ok(icon) => vb.with_icon(icon),
                Err(_) => vb,
            }
        },
        ..Default::default()
    };
    eframe::run_native(
        "AstroFiler",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc, cfg)))),
    )
    .map_err(|e| anyhow::anyhow!("GUI error: {e}"))
}

/// What a double-click or the right-click menu does with an image row.
#[derive(Clone, Copy)]
enum RowAction {
    ShowFolder,
    OpenViewer,
    CopyPath,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Images,
    Telescopes,
    Sessions,
    Masters,
    Batch,
    Duplicates,
    Mappings,
    Stats,
    Config,
    Log,
}

const TABS: &[(Tab, &str)] = &[
    (Tab::Images, "Images"),
    (Tab::Telescopes, "Telescopes"),
    (Tab::Sessions, "Sessions"),
    (Tab::Masters, "Masters"),
    (Tab::Batch, "Batch"),
    (Tab::Duplicates, "Duplicates"),
    (Tab::Mappings, "Mappings"),
    (Tab::Stats, "Statistics"),
    (Tab::Config, "Settings"),
    (Tab::Log, "Log"),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum TypeFilter {
    All,
    Light,
    Calibration,
    Stacked,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortKey {
    Object,
    Date,
    Type,
    Filter,
    Exposure,
    Telescope,
}

struct Job {
    name: String,
    state: Arc<JobState>,
    /// Changes the catalogue. Only one such task runs at a time (SQLite has a
    /// single writer); read-only tasks run alongside it.
    writes: bool,
}

type Work = Box<dyn FnOnce(&mut rusqlite::Connection, &Config, &JobState) -> Result<String> + Send>;

/// A catalogue-changing task waiting for the running one to finish.
struct Queued {
    name: String,
    work: Work,
}

/// Something the user must confirm before it happens.
enum Confirm {
    DeleteFiles { ids: Vec<String>, from_disk: bool },
    RemoveDuplicates,
    ClearSessions,
    ImportWithDelete,
    QuitDuringJob,
}

#[derive(Default)]
struct EditDialog {
    open: bool,
    field: usize,
    value: String,
    headers: bool,
    refile: bool,
}

struct LoadDialog {
    open: bool,
    folder: String,
    placement: Placement,
    dry_run: bool,
    on_conflict: OnConflict,
}

struct ScopeUi {
    telescope: usize,
    usb: bool,
    host: String,
    usb_path: String,
    found: Arc<Mutex<Vec<Found>>>,
    files: Arc<Mutex<Vec<RemoteFile>>>,
    selected: Vec<bool>,
    include_stacked: bool,
    delete_after: bool,
    dest: String,
}

pub struct App {
    cfg: Config,
    last_refresh: std::time::Instant,
    allow_close: bool,
    /// Zoom last applied from the interface-size setting (None = not yet).
    applied_zoom: Option<f32>,
    /// "Auto" interface size, worked out once the monitor size is known.
    auto_zoom: Option<f32>,
    db_path: PathBuf,
    tab: Tab,
    status: String,
    jobs: Vec<Job>,
    queued: VecDeque<Queued>,
    confirm: Option<Confirm>,

    files: Vec<FitsFile>,
    filtered: Vec<usize>,
    filter_key: (String, TypeFilter, SortKey, bool, usize),
    search: String,
    type_filter: TypeFilter,
    sort: SortKey,
    sort_desc: bool,
    selected: HashSet<String>,
    anchor: Option<usize>,
    edit: EditDialog,
    load: LoadDialog,

    preview_for: Option<String>,
    preview_tex: Option<egui::TextureHandle>,
    preview_info: String,
    preview_rx: Option<mpsc::Receiver<(String, Result<preview::Preview, String>)>>,
    picker: picker::FolderPicker,

    sessions: Vec<Session>,
    session_sel: Option<String>,
    session_files: Vec<FitsFile>,
    masters: Vec<Master>,
    dup_groups: Vec<Vec<FitsFile>>,
    mappings: Vec<Mapping>,
    new_map: (String, String, String),
    stats: Arc<Mutex<Option<Stats>>>,
    merge: (String, String, bool, bool),
    export_layout_by_object: bool,
    scope: ScopeUi,
    clean_dir: String,
    cfg_edit: Config,
    /// Files still filed under an older layout (see `batch::layout_plan`).
    old_layout: Arc<Mutex<usize>>,
    /// Object and common name being added in the settings.
    new_object_name: (String, String),
}

impl App {
    fn new(cc: &eframe::CreationContext, cfg: Config) -> Self {
        // Labels must not grab clicks, or table rows can't be selected.
        cc.egui_ctx
            .style_mut(|s| s.interaction.selectable_labels = false);
        cc.egui_ctx.set_visuals(if cfg.theme == "light" {
            egui::Visuals::light()
        } else {
            egui::Visuals::dark()
        });
        let db_path = cfg.database_path();
        let mut app = App {
            last_refresh: std::time::Instant::now(),
            allow_close: false,
            applied_zoom: None,
            auto_zoom: None,
            db_path,
            tab: Tab::Images,
            status: String::new(),
            jobs: Vec::new(),
            queued: VecDeque::new(),
            confirm: None,
            files: vec![],
            filtered: vec![],
            filter_key: (
                String::from("\u{0}"),
                TypeFilter::All,
                SortKey::Date,
                false,
                usize::MAX,
            ),
            search: String::new(),
            type_filter: TypeFilter::All,
            sort: SortKey::Date,
            sort_desc: true,
            selected: HashSet::new(),
            anchor: None,
            edit: EditDialog::default(),
            load: LoadDialog {
                open: false,
                folder: cfg.source.to_string_lossy().into(),
                placement: Placement::Copy,
                dry_run: false,
                on_conflict: cfg.on_conflict,
            },
            preview_for: None,
            preview_tex: None,
            preview_info: String::new(),
            preview_rx: None,
            picker: Default::default(),
            sessions: vec![],
            session_sel: None,
            session_files: vec![],
            masters: vec![],
            dup_groups: vec![],
            mappings: vec![],
            new_map: ("TELESCOP".into(), String::new(), String::new()),
            stats: Arc::new(Mutex::new(None)),
            merge: (String::new(), String::new(), true, true),
            export_layout_by_object: true,
            scope: ScopeUi {
                telescope: 0,
                usb: true,
                host: telescope::all()[0].default_host(&cfg),
                usb_path: String::new(),
                found: Arc::new(Mutex::new(vec![])),
                files: Arc::new(Mutex::new(vec![])),
                selected: vec![],
                include_stacked: cfg.include_stacked,
                delete_after: false,
                dest: cfg.source.to_string_lossy().into(),
            },
            clean_dir: String::new(),
            cfg_edit: cfg.clone(),
            old_layout: Arc::new(Mutex::new(0)),
            new_object_name: Default::default(),
            cfg,
        };
        app.reload();
        app.check_layout(&cc.egui_ctx);
        // Offer USB telescopes straight away.
        let usb = telescope::find_usb();
        if let Some(f) = usb.first() {
            app.status = format!("Found {} — see the Telescopes tab", f.label);
            if let Link::Usb(p) = &f.link {
                app.scope.telescope = telescope::all()
                    .iter()
                    .position(|t| t.id() == f.telescope.id())
                    .unwrap_or(0);
                app.scope.usb_path = p.to_string_lossy().into();
            }
        }
        *app.scope.found.lock().unwrap() = usb;
        app
    }

    /// Count, in the background, files filed under an older layout.
    fn check_layout(&self, ctx: &egui::Context) {
        let (cfg, db_path) = (self.cfg.clone(), self.db_path.clone());
        let (count, ctx) = (self.old_layout.clone(), ctx.clone());
        std::thread::spawn(move || {
            let n = db::open(&db_path)
                .and_then(|conn| batch::layout_plan(&conn, &cfg))
                .map(|plan| plan.len())
                .unwrap_or(0);
            *count.lock().unwrap() = n;
            ctx.request_repaint();
        });
    }

    fn layout_banner(&mut self, ctx: &egui::Context) {
        let n = *self.old_layout.lock().unwrap();
        if n == 0 || self.job_active("Update folders") {
            return;
        }
        egui::TopBottomPanel::top("old_layout").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(format!(
                    "{n} files in the repository are filed the old way: object folders without \
                     the object's name, Seestar serial-number folders or renamed stacked results."
                ));
                if ui.button("Move them to the current layout").clicked() {
                    let count = self.old_layout.clone();
                    self.spawn("Update folders", move |conn, cfg, p| {
                        let r = batch::migrate_layout(conn, cfg, false, p)?;
                        for (f, e) in &r.errors {
                            log::warn!("Not moved: {} ({e})", f.display());
                        }
                        *count.lock().unwrap() = r.errors.len();
                        Ok(format!(
                            "{} files moved to the current layout{}",
                            r.moved.len(),
                            if r.errors.is_empty() {
                                String::new()
                            } else {
                                format!(", {} not moved (see Log)", r.errors.len())
                            }
                        ))
                    });
                }
            });
        });
    }

    fn conn(&self) -> Result<rusqlite::Connection> {
        db::open(&self.db_path)
    }

    fn reload(&mut self) {
        let r = (|| -> Result<()> {
            let conn = self.conn()?;
            self.files = db::all_files(&conn, false)?;
            self.sessions = db::all_sessions(&conn)?;
            self.masters = db::masters(&conn, false)?;
            self.mappings = db::mappings(&conn)?;
            self.dup_groups = batch::duplicate_groups(&conn)?;
            if let Some(id) = &self.session_sel {
                self.session_files = sessions::session_files(&conn, id)?;
            }
            Ok(())
        })();
        if let Err(e) = r {
            self.status = format!("Database error: {e:#}");
        }
        let ids: HashSet<&str> = self.files.iter().map(|f| f.id.as_str()).collect();
        self.selected.retain(|s| ids.contains(s.as_str()));
        self.filter_key.4 = usize::MAX; // force re-filter
        *self.stats.lock().unwrap() = None;
    }

    /// Run a task that changes the catalogue on a background thread with its
    /// own DB connection. It waits in a queue while another such task runs.
    fn spawn<F>(&mut self, name: &str, work: F)
    where
        F: FnOnce(&mut rusqlite::Connection, &Config, &JobState) -> Result<String> + Send + 'static,
    {
        self.submit(name, true, Box::new(work));
    }

    /// Run a task that only reads the catalogue; it starts straight away.
    fn spawn_read<F>(&mut self, name: &str, work: F)
    where
        F: FnOnce(&mut rusqlite::Connection, &Config, &JobState) -> Result<String> + Send + 'static,
    {
        self.submit(name, false, Box::new(work));
    }

    fn submit(&mut self, name: &str, writes: bool, work: Work) {
        if self.job_active(name) {
            self.status = format!("{name} is already running");
            return;
        }
        if writes && self.jobs.iter().any(|j| j.writes) {
            self.status = format!("{name} will start when the current task finishes");
            self.queued.push_back(Queued {
                name: name.to_string(),
                work,
            });
            return;
        }
        self.start(name, writes, work);
    }

    fn job_active(&self, name: &str) -> bool {
        self.jobs.iter().any(|j| j.name == name) || self.queued.iter().any(|q| q.name == name)
    }

    fn start(&mut self, name: &str, writes: bool, work: Work) {
        let state = Arc::new(JobState::default());
        let st = state.clone();
        let cfg = self.cfg.clone();
        let db_path = self.db_path.clone();
        let label = name.to_string();
        std::thread::spawn(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                db::open(&db_path).and_then(|mut conn| work(&mut conn, &cfg, &st))
            }))
            .unwrap_or_else(|panic| {
                let msg = panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "unknown error".into());
                Err(anyhow::anyhow!("internal error (please report): {msg}"))
            });
            if let Err(e) = &r {
                log::error!("{label}: {e:#}");
            }
            st.finish(r.map_err(|e| format!("{e:#}")));
        });
        self.jobs.push(Job {
            name: name.to_string(),
            state,
            writes,
        });
    }

    fn poll_jobs(&mut self, ctx: &egui::Context) {
        let (finished, running): (Vec<Job>, Vec<Job>) = std::mem::take(&mut self.jobs)
            .into_iter()
            .partition(|j| j.state.done.load(Ordering::SeqCst));
        self.jobs = running;
        let mut reload = false;
        for job in finished {
            let result = job.state.result.lock().unwrap().take();
            self.status = match result {
                Some(Ok(msg)) => format!("{}: {msg}", job.name),
                Some(Err(e)) => format!("{} failed: {e}", job.name),
                None => String::new(),
            };
            // Stats jobs only read; reloading would clear the result and loop.
            reload |= job.name != "Statistics";
            if job.name == "Scanning telescope" {
                let n = self.scope.files.lock().unwrap().len();
                self.scope.selected = vec![true; n];
            }
        }
        if reload {
            self.reload();
        }
        if !self.jobs.iter().any(|j| j.writes) {
            if let Some(q) = self.queued.pop_front() {
                self.start(&q.name, true, q.work);
            }
        }
        if self.jobs.is_empty() {
            return;
        }
        // Loads commit in batches, so show new files as they arrive.
        if self.jobs.iter().any(|j| j.writes)
            && self.last_refresh.elapsed() > Duration::from_secs(3)
        {
            self.last_refresh = std::time::Instant::now();
            if let Ok(files) = self.conn().and_then(|c| db::all_files(&c, false)) {
                if files.len() != self.files.len() {
                    self.files = files;
                    self.filter_key.4 = usize::MAX;
                    *self.stats.lock().unwrap() = None;
                }
            }
        }
        ctx.request_repaint_after(Duration::from_millis(100));
    }

    fn refilter(&mut self) {
        let key = (
            self.search.clone(),
            self.type_filter,
            self.sort,
            self.sort_desc,
            self.files.len(),
        );
        if key == self.filter_key {
            return;
        }
        self.filter_key = key;
        let q = self.search.to_lowercase();
        let mut terms: Vec<&str> = q.split_whitespace().collect();
        // Common names, looked up once per object rather than once per file.
        let objects: HashSet<&str> = self
            .files
            .iter()
            .filter_map(|f| f.object.as_deref())
            .collect();
        let common: HashMap<&str, String> = objects
            .into_iter()
            .map(|o| {
                (
                    o,
                    names::common_name(o, &self.cfg.object_names).unwrap_or_default(),
                )
            })
            .collect();
        // A search starting with an object ("M 76", "m76", "NGC 7000 Ha")
        // shows exactly that object; matching "m" and "76" as separate words
        // would also find every file with 76 in its time or temperature.
        let object_keys: HashSet<String> = common.keys().map(|o| names::key(o)).collect();
        let mut object = None;
        for n in (1..=terms.len()).rev() {
            let k = names::key(&terms[..n].join(" "));
            if object_keys.contains(&k) {
                object = Some(k);
                terms.drain(..n);
                break;
            }
        }
        let mut idx: Vec<usize> = self
            .files
            .iter()
            .enumerate()
            .filter(|(_, f)| {
                let kind = FrameKind::classify(f.image_type.as_deref().unwrap_or(""));
                let type_ok = match self.type_filter {
                    TypeFilter::All => true,
                    TypeFilter::Light => kind == Some(FrameKind::Light) && !f.stacked,
                    TypeFilter::Calibration => !matches!(kind, Some(FrameKind::Light) | None),
                    TypeFilter::Stacked => f.stacked,
                };
                let object_ok = object
                    .as_ref()
                    .is_none_or(|k| names::key(f.object.as_deref().unwrap_or("")) == *k);
                type_ok && object_ok && {
                    let object = f.object.as_deref().unwrap_or("");
                    let hay = format!(
                        "{} {} {} {} {} {} {}",
                        object,
                        common.get(object).map(String::as_str).unwrap_or(""),
                        f.filter.as_deref().unwrap_or(""),
                        f.telescope.as_deref().unwrap_or(""),
                        f.instrument.as_deref().unwrap_or(""),
                        f.date.as_deref().unwrap_or(""),
                        f.name
                    )
                    .to_lowercase();
                    terms.iter().all(|t| hay.contains(t))
                }
            })
            .map(|(i, _)| i)
            .collect();
        let files = &self.files;
        let exp = |f: &FitsFile| {
            f.exptime
                .as_deref()
                .and_then(|e| e.parse::<f64>().ok())
                .unwrap_or(0.0)
        };
        idx.sort_by(|&a, &b| {
            let (x, y) = (&files[a], &files[b]);
            let o = match self.sort {
                SortKey::Object => x.object.cmp(&y.object).then(x.date.cmp(&y.date)),
                SortKey::Date => x.date.cmp(&y.date),
                SortKey::Type => x.image_type.cmp(&y.image_type).then(x.date.cmp(&y.date)),
                SortKey::Filter => x.filter.cmp(&y.filter).then(x.date.cmp(&y.date)),
                SortKey::Exposure => exp(x).total_cmp(&exp(y)),
                SortKey::Telescope => x.telescope.cmp(&y.telescope).then(x.date.cmp(&y.date)),
            };
            if self.sort_desc {
                o.reverse()
            } else {
                o
            }
        });
        self.filtered = idx;
    }

    fn request_preview(&mut self, f: &FitsFile) {
        if self.preview_for.as_deref() == Some(&f.id) {
            return;
        }
        self.preview_for = Some(f.id.clone());
        self.preview_info = "Loading preview...".into();
        let (tx, rx) = mpsc::channel();
        let path = PathBuf::from(&f.name);
        let id = f.id.clone();
        std::thread::spawn(move || {
            let r = preview::render(&path, 1024).map_err(|e| format!("{e:#}"));
            let _ = tx.send((id, r));
        });
        self.preview_rx = Some(rx);
    }

    fn poll_preview(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.preview_rx else { return };
        match rx.try_recv() {
            Ok((id, r)) => {
                if self.preview_for.as_deref() == Some(&id) {
                    match r {
                        Ok(p) => {
                            self.preview_tex = Some(ctx.load_texture(
                                "preview",
                                p.image,
                                egui::TextureOptions::LINEAR,
                            ));
                            self.preview_info = p.info;
                        }
                        Err(e) => {
                            self.preview_tex = None;
                            self.preview_info = format!("No preview: {e}");
                        }
                    }
                }
                self.preview_rx = None;
            }
            Err(mpsc::TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(50)),
            Err(_) => self.preview_rx = None,
        }
    }

    /// Zoom picked by "Auto": the desktop scaling, raised so text stays
    /// readable on high-resolution screens (4K at 100% -> 150%).
    /// Computed once and cached: the monitor size egui reports is in points,
    /// which change with the zoom itself.
    fn auto_zoom(&mut self, ctx: &egui::Context) -> f32 {
        if let Some(z) = self.auto_zoom {
            return z;
        }
        let native = ctx.native_pixels_per_point().unwrap_or(1.0);
        let Some(monitor) = ctx.input(|i| i.viewport().monitor_size) else {
            return 1.0; // not known yet; try again next frame
        };
        // Points x current pixels-per-point = physical pixels, whatever the zoom.
        let physical_height = monitor.y * ctx.pixels_per_point();
        let wanted = ((physical_height / 1440.0) * 4.0).round() / 4.0;
        let zoom = (wanted.max(native) / native).clamp(1.0, 3.0);
        log::info!(
            "screen {:.0}px high, desktop scaling {:.0}%: auto interface size {:.0}%",
            physical_height,
            native * 100.0,
            zoom * 100.0
        );
        self.auto_zoom = Some(zoom);
        zoom
    }

    fn wanted_zoom(&mut self, ctx: &egui::Context) -> f32 {
        match self.cfg_edit.ui_scale.parse::<f32>() {
            Ok(z) => z.clamp(0.75, 3.0),
            Err(_) => self.auto_zoom(ctx),
        }
    }

    /// Apply the interface-size setting when it (or the screen) changes;
    /// Ctrl +/- zooming in between is left alone.
    fn apply_ui_scale(&mut self, ctx: &egui::Context) {
        let zoom = self.wanted_zoom(ctx);
        if self.applied_zoom == Some(zoom) {
            return;
        }
        let first = self.applied_zoom.is_none();
        ctx.set_zoom_factor(zoom);
        self.applied_zoom = Some(zoom);
        if first && zoom > 1.0 {
            // Grow the window with the interface, within the screen.
            let mut size = egui::vec2(1400.0, 880.0) * zoom;
            if let Some(m) = ctx.input(|i| i.viewport().monitor_size) {
                size = size.min(m * 0.9);
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_jobs(ctx);
        self.poll_preview(ctx);
        self.apply_ui_scale(ctx);
        if ctx.input(|i| i.viewport().close_requested())
            && !(self.jobs.is_empty() && self.queued.is_empty())
            && !self.allow_close
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm = Some(Confirm::QuitDuringJob);
        }

        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.heading(RichText::new("🔭 AstroFiler").strong());
                ui.separator();
                for (tab, label) in TABS {
                    if ui.selectable_label(self.tab == *tab, *label).clicked() {
                        self.tab = *tab;
                    }
                }
            });
            ui.add_space(2.0);
        });

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            let mut unqueue = None;
            for (i, job) in self.jobs.iter().enumerate() {
                let last = i + 1 == self.jobs.len();
                ui.horizontal(|ui| {
                    let (done, total, msg) = job.state.snapshot();
                    ui.spinner();
                    ui.label(RichText::new(&job.name).strong());
                    let frac = if total > 0 {
                        done as f32 / total as f32
                    } else {
                        0.0
                    };
                    ui.add(
                        egui::ProgressBar::new(frac)
                            .desired_width(260.0)
                            .text(if total > 0 {
                                format!("{done}/{total}")
                            } else {
                                String::new()
                            }),
                    );
                    ui.label(msg);
                    if ui.button("Cancel").clicked() {
                        job.state.cancel.store(true, Ordering::SeqCst);
                    }
                    if last && self.queued.is_empty() {
                        self.status_counts(ui);
                    }
                });
            }
            if !self.queued.is_empty() {
                ui.horizontal(|ui| {
                    ui.label("Waiting:");
                    for (i, q) in self.queued.iter().enumerate() {
                        ui.label(&q.name);
                        if ui.small_button("✖").on_hover_text("Don't run").clicked() {
                            unqueue = Some(i);
                        }
                    }
                    self.status_counts(ui);
                });
            }
            if let Some(i) = unqueue {
                self.queued.remove(i);
            }
            if self.jobs.is_empty() && self.queued.is_empty() {
                ui.horizontal(|ui| {
                    ui.label(&self.status);
                    self.status_counts(ui);
                });
            }
        });

        self.layout_banner(ctx);
        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Images => self.images_tab(ui, ctx),
            Tab::Telescopes => self.telescopes_tab(ui),
            Tab::Sessions => self.sessions_tab(ui),
            Tab::Masters => self.masters_tab(ui),
            Tab::Batch => self.batch_tab(ui),
            Tab::Duplicates => self.duplicates_tab(ui),
            Tab::Mappings => self.mappings_tab(ui),
            Tab::Stats => self.stats_tab(ui),
            Tab::Config => {
                egui::ScrollArea::vertical().show(ui, |ui| self.config_tab(ui, ctx));
            }
            Tab::Log => log_tab(ui),
        });

        self.dialogs(ctx);
    }
}

fn opt(s: &Option<String>) -> &str {
    s.as_deref().unwrap_or("")
}

impl App {
    // ------------------------------------------------------------ Images

    fn images_tab(&mut self, ui: &mut egui::Ui, _ctx: &egui::Context) {
        ui.horizontal_wrapped(|ui| {
            if ui
                .button("📥 Load folder…")
                .on_hover_text(
                    "Load images from an incoming folder or an existing archive (e.g. on your NAS)",
                )
                .clicked()
            {
                self.load.open = true;
            }
            if ui
                .button("🔄 Sync repository")
                .on_hover_text("Catalogue files already in the repository without moving them")
                .clicked()
            {
                self.spawn("Sync repository", |conn, cfg, p| {
                    Ok(
                        ingest::ingest_folder(conn, cfg, &cfg.repo, IngestOptions::IN_PLACE, p)?
                            .summary(),
                    )
                });
            }
            if ui.button("Refresh").clicked() {
                self.reload();
            }
            ui.separator();
            ui.add(
                egui::TextEdit::singleline(&mut self.search)
                    .hint_text("Search: M 76, M 76 LP, Barbell, 2026-09-27…")
                    .desired_width(260.0),
            );
            for (f, label) in [
                (TypeFilter::All, "All"),
                (TypeFilter::Light, "Lights"),
                (TypeFilter::Calibration, "Calibration"),
                (TypeFilter::Stacked, "Stacked"),
            ] {
                ui.selectable_value(&mut self.type_filter, f, label);
            }
        });
        self.refilter();

        ui.horizontal(|ui| {
            let n = self.selected.len();
            ui.label(format!("{} shown, {n} selected", self.filtered.len()));
            ui.add_enabled_ui(n > 0, |ui| {
                if ui.button("✏ Edit field…").clicked() {
                    self.edit.open = true;
                }
                if ui.button("📤 Export…").clicked() {
                    self.picker.open("export", "", ui.ctx());
                }
                if let Some(dir) = self.picker.take("export") {
                    {
                        let ids: Vec<String> = self.selected.iter().cloned().collect();
                        let layout = if self.export_layout_by_object {
                            ExportLayout::ByObject
                        } else {
                            ExportLayout::Flat
                        };
                        self.spawn_read("Export", move |conn, _, p| {
                            Ok(format!(
                                "{} files exported",
                                batch::export_files(conn, &ids, Path::new(&dir), layout, false, p)?
                            ))
                        });
                    }
                }
                if ui.button("🗑 Remove from catalogue").clicked() {
                    self.confirm = Some(Confirm::DeleteFiles {
                        ids: self.selected.iter().cloned().collect(),
                        from_disk: false,
                    });
                }
                if ui.button("🗑 Delete files").clicked() {
                    self.confirm = Some(Confirm::DeleteFiles {
                        ids: self.selected.iter().cloned().collect(),
                        from_disk: true,
                    });
                }
                if ui.button("Clear selection").clicked() {
                    self.selected.clear();
                }
            });
            if ui.button("Select all shown").clicked() {
                for &i in &self.filtered {
                    self.selected.insert(self.files[i].id.clone());
                }
            }
        });
        ui.separator();

        let full_width = ui.available_width();
        egui::SidePanel::right("preview")
            .resizable(true)
            .default_width((full_width * 0.3).clamp(240.0, 420.0))
            .min_width(200.0)
            .max_width(full_width * 0.45)
            .show_inside(ui, |ui| {
                ui.heading("Preview");
                if let Some(id) = self.preview_for.clone() {
                    if let Some(f) = self.files.iter().find(|f| f.id == id) {
                        ui.label(RichText::new(f.file_name()).small());
                        ui.horizontal(|ui| {
                            if ui.button("Open in viewer").clicked() {
                                if let Err(e) = util::open_external(
                                    &self.cfg.external_viewer,
                                    Path::new(&f.name),
                                ) {
                                    self.status = format!("Could not open viewer: {e}");
                                }
                            }
                            if ui.button("Show folder").clicked() {
                                if let Err(e) = util::show_in_folder(Path::new(&f.name)) {
                                    self.status = format!("Could not open folder: {e:#}");
                                }
                            }
                        });
                    }
                }
                ui.label(&self.preview_info);
                if let Some(tex) = &self.preview_tex {
                    let avail = ui.available_size();
                    ui.add(
                        egui::Image::from_texture(tex)
                            .max_size(avail)
                            .maintain_aspect_ratio(true),
                    );
                }
            });

        let mut clicked: Option<usize> = None;
        let mut row_action: Option<(usize, RowAction)> = None;
        let mut sort_click: Option<SortKey> = None;
        let n = self.filtered.len();
        // The scroll area clips the table to the space left of the preview
        // and scrolls sideways when the columns don't fit.
        egui::ScrollArea::horizontal()
            .id_salt("images_hscroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                TableBuilder::new(ui)
                    .striped(true)
                    .resizable(true)
                    .sense(egui::Sense::click())
                    .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                    .column(Column::initial(150.0).clip(true).at_least(60.0))
                    .column(Column::initial(90.0).clip(true))
                    .column(Column::initial(140.0).clip(true))
                    .column(Column::initial(60.0).clip(true))
                    .column(Column::initial(55.0).clip(true))
                    .column(Column::initial(40.0).clip(true))
                    .column(Column::initial(50.0).clip(true))
                    .column(Column::initial(130.0).clip(true))
                    .column(Column::remainder().clip(true).at_least(100.0))
                    .header(22.0, |mut h| {
                        let mut head =
                            |h: &mut egui_extras::TableRow, label: &str, key: Option<SortKey>| {
                                h.col(|ui| {
                                    let mut text = label.to_string();
                                    if key == Some(self.sort) {
                                        text.push_str(if self.sort_desc { " ⏷" } else { " ⏶" });
                                    }
                                    if ui
                                        .add(
                                            egui::Label::new(RichText::new(text).strong())
                                                .sense(egui::Sense::click()),
                                        )
                                        .clicked()
                                    {
                                        sort_click = key;
                                    }
                                });
                            };
                        head(&mut h, "Object", Some(SortKey::Object));
                        head(&mut h, "Type", Some(SortKey::Type));
                        head(&mut h, "Date", Some(SortKey::Date));
                        head(&mut h, "Filter", Some(SortKey::Filter));
                        head(&mut h, "Exp (s)", Some(SortKey::Exposure));
                        head(&mut h, "Bin", None);
                        head(&mut h, "Temp", None);
                        head(&mut h, "Telescope / camera", Some(SortKey::Telescope));
                        head(&mut h, "File", None);
                    })
                    .body(|body| {
                        body.rows(20.0, n, |mut row| {
                            let i = row.index();
                            let f = &self.files[self.filtered[i]];
                            row.set_selected(self.selected.contains(&f.id));
                            let typ = if f.stacked {
                                "STACKED".to_string()
                            } else {
                                opt(&f.image_type).to_string()
                            };
                            row.col(|ui| {
                                ui.label(opt(&f.object));
                            });
                            row.col(|ui| {
                                ui.label(typ);
                            });
                            row.col(|ui| {
                                ui.label(
                                    opt(&f.date)
                                        .replace('T', " ")
                                        .chars()
                                        .take(19)
                                        .collect::<String>(),
                                );
                            });
                            row.col(|ui| {
                                ui.label(opt(&f.filter));
                            });
                            row.col(|ui| {
                                ui.label(opt(&f.exptime));
                            });
                            row.col(|ui| {
                                ui.label(format!("{}x{}", opt(&f.xbin), opt(&f.ybin)));
                            });
                            row.col(|ui| {
                                ui.label(opt(&f.ccd_temp));
                            });
                            row.col(|ui| {
                                ui.label(format!("{} / {}", opt(&f.telescope), opt(&f.instrument)));
                            });
                            row.col(|ui| {
                                ui.label(f.file_name()).on_hover_text(&f.name);
                            });
                            let resp = row.response();
                            if resp.clicked() {
                                clicked = Some(i);
                            }
                            // Right-clicking an unselected row selects it, as
                            // in a file manager.
                            if resp.secondary_clicked() && !self.selected.contains(&f.id) {
                                clicked = Some(i);
                            }
                            if resp.double_clicked() {
                                row_action = Some((i, RowAction::ShowFolder));
                            }
                            resp.context_menu(|ui| {
                                for (action, label) in [
                                    (RowAction::ShowFolder, "🗁 Open containing folder"),
                                    (RowAction::OpenViewer, "🖼 Open in viewer"),
                                    (RowAction::CopyPath, "📋 Copy path"),
                                ] {
                                    if ui.button(label).clicked() {
                                        row_action = Some((i, action));
                                        ui.close_menu();
                                    }
                                }
                            });
                        });
                    });
            });

        if let Some(k) = sort_click {
            if self.sort == k {
                self.sort_desc = !self.sort_desc;
            } else {
                self.sort = k;
                self.sort_desc = k == SortKey::Date;
            }
        }
        if let Some(i) = clicked {
            let mods = ui.input(|inp| inp.modifiers);
            let id = self.files[self.filtered[i]].id.clone();
            if mods.shift {
                let a = self.anchor.unwrap_or(i);
                for j in a.min(i)..=a.max(i) {
                    self.selected
                        .insert(self.files[self.filtered[j]].id.clone());
                }
            } else if mods.command {
                if !self.selected.remove(&id) {
                    self.selected.insert(id);
                }
                self.anchor = Some(i);
            } else {
                self.selected.clear();
                self.selected.insert(id);
                self.anchor = Some(i);
            }
            let f = self.files[self.filtered[i]].clone();
            self.request_preview(&f);
        }
        if let Some((i, action)) = row_action {
            let path = PathBuf::from(&self.files[self.filtered[i]].name);
            let r = match action {
                RowAction::ShowFolder => util::show_in_folder(&path),
                RowAction::OpenViewer => util::open_external(&self.cfg.external_viewer, &path),
                RowAction::CopyPath => {
                    ui.ctx().copy_text(path.to_string_lossy().into_owned());
                    self.status = format!("Copied {}", path.display());
                    Ok(())
                }
            };
            if let Err(e) = r {
                self.status = format!("Could not open {}: {e:#}", path.display());
            }
        }
    }

    // ------------------------------------------------------------ Telescopes

    fn telescopes_tab(&mut self, ui: &mut egui::Ui) {
        let scopes = telescope::all();
        ui.heading("Import from a smart telescope");
        ui.label("FITS files and DWARF session info (shotsInfo.json) are transferred — the telescope's JPG/PNG previews and thumbnails are skipped.");
        ui.add_space(6.0);

        let found = self.scope.found.lock().unwrap().clone();
        ui.horizontal_wrapped(|ui| {
            ui.label("Detected:");
            if found.is_empty() {
                ui.weak("nothing yet");
            }
            for f in &found {
                if ui.button(&f.label).clicked() {
                    self.scope.telescope = scopes
                        .iter()
                        .position(|t| t.id() == f.telescope.id())
                        .unwrap_or(0);
                    match &f.link {
                        Link::Usb(p) => {
                            self.scope.usb = true;
                            self.scope.usb_path = p.to_string_lossy().into();
                        }
                        Link::Network(h) => {
                            self.scope.usb = false;
                            self.scope.host = h.clone();
                        }
                    }
                }
            }
            if ui.button("🔌 Detect USB").clicked() {
                *self.scope.found.lock().unwrap() = telescope::find_usb();
            }
            if ui.button("📡 Scan Wi-Fi").clicked() {
                let slot = self.scope.found.clone();
                self.spawn_read("Network scan", move |_, cfg, p| {
                    let mut all = telescope::find_usb();
                    for t in telescope::all() {
                        all.extend(telescope::find_network(cfg, *t, p));
                    }
                    let n = all.len();
                    *slot.lock().unwrap() = all;
                    Ok(format!("{n} telescope(s) found"))
                });
            }
        });
        ui.separator();

        egui::Grid::new("scope_grid")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                ui.label("Telescope");
                ui.horizontal(|ui| {
                    for (i, t) in scopes.iter().enumerate() {
                        if ui
                            .selectable_value(&mut self.scope.telescope, i, t.name())
                            .clicked()
                        {
                            self.scope.host = t.default_host(&self.cfg);
                        }
                    }
                });
                ui.end_row();
                ui.label("Connection");
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.scope.usb, false, "Wi-Fi");
                    ui.selectable_value(&mut self.scope.usb, true, "USB-C");
                });
                ui.end_row();
                if self.scope.usb {
                    ui.label("Telescope drive");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.scope.usb_path)
                                .desired_width(360.0),
                        );
                        if ui.button("Browse…").clicked() {
                            self.picker.open("usb", &self.scope.usb_path, ui.ctx());
                        }
                        if let Some(p) = self.picker.take("usb") {
                            self.scope.usb_path = p;
                        }
                    });
                } else {
                    ui.label("Host / IP");
                    ui.add(egui::TextEdit::singleline(&mut self.scope.host).desired_width(220.0));
                }
                ui.end_row();
                ui.label("Download to");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.scope.dest).desired_width(360.0));
                    if ui.button("Browse…").clicked() {
                        self.picker.open("dest", &self.scope.dest, ui.ctx());
                    }
                    if let Some(p) = self.picker.take("dest") {
                        self.scope.dest = p;
                    }
                });
                ui.end_row();
                ui.label("Options");
                ui.vertical(|ui| {
                    ui.checkbox(
                        &mut self.scope.include_stacked,
                        "Include the telescope's own stacked results",
                    );
                    ui.checkbox(
                        &mut self.scope.delete_after,
                        RichText::new("Delete from telescope after safe import")
                            .color(Color32::from_rgb(230, 150, 60)),
                    );
                });
                ui.end_row();
            });

        let link = if self.scope.usb {
            Link::Usb(PathBuf::from(&self.scope.usb_path))
        } else {
            Link::Network(self.scope.host.clone())
        };
        let t = scopes[self.scope.telescope.min(scopes.len() - 1)];
        ui.horizontal(|ui| {
            if ui.button(format!("🔍 Connect & list files")).clicked() {
                let slot = self.scope.files.clone();
                let link = link.clone();
                let stacked = self.scope.include_stacked;
                self.spawn_read("Scanning telescope", move |_, cfg, p| {
                    p.update(0, 0, &format!("Connecting to {} ({link})…", t.name()));
                    let mut s = telescope::connect(cfg, t, &link)?;
                    let files = s.scan(stacked)?;
                    let n = files.len();
                    let size: u64 = files.iter().map(|f| f.size).sum();
                    *slot.lock().unwrap() = files;
                    Ok(format!("{n} files ({})", util::human_size(size)))
                });
            }
        });

        let files = self.scope.files.lock().unwrap().clone();
        if self.scope.selected.len() != files.len() {
            self.scope.selected = vec![true; files.len()];
        }
        if files.is_empty() {
            return;
        }
        ui.separator();
        let sel_n = self.scope.selected.iter().filter(|s| **s).count();
        let sel_size: u64 = files
            .iter()
            .zip(&self.scope.selected)
            .filter(|(_, s)| **s)
            .map(|(f, _)| f.size)
            .sum();
        ui.horizontal(|ui| {
            ui.label(format!(
                "{sel_n} of {} selected ({})",
                files.len(),
                util::human_size(sel_size)
            ));
            if ui.button("All").clicked() {
                self.scope.selected.iter_mut().for_each(|s| *s = true);
            }
            if ui.button("None").clicked() {
                self.scope.selected.iter_mut().for_each(|s| *s = false);
            }
            if ui
                .add_enabled(
                    sel_n > 0,
                    egui::Button::new(RichText::new("⬇ Import selected").strong()),
                )
                .clicked()
            {
                if self.scope.delete_after {
                    self.confirm = Some(Confirm::ImportWithDelete);
                } else {
                    self.start_import(false);
                }
            }
        });
        TableBuilder::new(ui)
            .striped(true)
            .column(Column::exact(24.0))
            .column(Column::initial(90.0).clip(true))
            .column(Column::initial(260.0).clip(true))
            .column(Column::initial(80.0).clip(true))
            .column(Column::remainder().clip(true))
            .header(20.0, |mut h| {
                for l in ["", "Kind", "Folder", "Size", "File"] {
                    h.col(|ui| {
                        ui.strong(l);
                    });
                }
            })
            .body(|body| {
                body.rows(20.0, files.len(), |mut row| {
                    let i = row.index();
                    let f = &files[i];
                    row.col(|ui| {
                        ui.checkbox(&mut self.scope.selected[i], "");
                    });
                    row.col(|ui| {
                        ui.label(f.kind);
                    });
                    row.col(|ui| {
                        ui.label(&f.folder);
                    });
                    row.col(|ui| {
                        ui.label(util::human_size(f.size));
                    });
                    row.col(|ui| {
                        ui.label(&f.name);
                    });
                });
            });
    }

    fn start_import(&mut self, delete: bool) {
        let files: Vec<RemoteFile> = self
            .scope
            .files
            .lock()
            .unwrap()
            .iter()
            .zip(&self.scope.selected)
            .filter(|(_, s)| **s)
            .map(|(f, _)| f.clone())
            .collect();
        let scopes = telescope::all();
        let t = scopes[self.scope.telescope.min(scopes.len() - 1)];
        let link = if self.scope.usb {
            Link::Usb(PathBuf::from(&self.scope.usb_path))
        } else {
            Link::Network(self.scope.host.clone())
        };
        let dest = PathBuf::from(&self.scope.dest);
        let slot = self.scope.files.clone();
        self.spawn("Importing from telescope", move |conn, cfg, p| {
            let mut s = telescope::connect(cfg, t, &link)?;
            let r = s.import(conn, cfg, &files, &dest, delete, p)?;
            for (path, e) in &r.failed {
                log::warn!("{path}: {e}");
            }
            // Rescan so the list reflects what is left on the telescope.
            if let Ok(left) = s.scan(true) {
                *slot.lock().unwrap() = left;
            }
            Ok(r.summary())
        });
    }

    // ------------------------------------------------------------ Sessions

    fn sessions_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui
                .button("➕ Create sessions")
                .on_hover_text("Group unassigned files by object, night and filter")
                .clicked()
            {
                self.spawn("Create sessions", |conn, _, p| {
                    let r = sessions::create_all(conn, p)?;
                    Ok(format!(
                        "{} sessions created ({} light, {} calibration)",
                        r.total(),
                        r.light,
                        r.total() - r.light
                    ))
                });
            }
            if ui.button("Clear all sessions").clicked() {
                self.confirm = Some(Confirm::ClearSessions);
            }
            if let Some(id) = self.session_sel.clone() {
                ui.separator();
                let sess = self.sessions.iter().find(|s| s.id == id).cloned();
                if let Some(s) = sess {
                    if s.is_calibration() && ui.button("⭐ Create master from session").clicked() {
                        let sid = s.id.clone();
                        self.spawn("Create master", move |conn, cfg, p| {
                            Ok(format!(
                                "created {}",
                                masters::create_from_session(conn, cfg, &sid, p)?.path
                            ))
                        });
                    }
                    if ui.button("📤 Export session…").clicked() {
                        self.picker.open("export_session", "", ui.ctx());
                    }
                    if let Some(dir) = self.picker.take("export_session") {
                        {
                            let ids: Vec<String> =
                                self.session_files.iter().map(|f| f.id.clone()).collect();
                            self.spawn_read("Export session", move |conn, _, p| {
                                Ok(format!(
                                    "{} files exported",
                                    batch::export_files(
                                        conn,
                                        &ids,
                                        Path::new(&dir),
                                        ExportLayout::ByObject,
                                        false,
                                        p
                                    )?
                                ))
                            });
                        }
                    }
                    if ui.button("Select files in Images").clicked() {
                        self.selected = self.session_files.iter().map(|f| f.id.clone()).collect();
                        self.tab = Tab::Images;
                    }
                }
            }
        });
        ui.separator();
        let mut pick: Option<String> = None;
        let avail = ui.available_height();
        ui.push_id("sessions_table", |ui| {
            TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                .sense(egui::Sense::click())
                .max_scroll_height(avail * 0.55)
                .column(Column::initial(180.0).clip(true))
                .column(Column::initial(95.0).clip(true))
                .column(Column::initial(70.0).clip(true))
                .column(Column::initial(60.0).clip(true))
                .column(Column::initial(50.0).clip(true))
                .column(Column::initial(55.0).clip(true))
                .column(Column::remainder().clip(true))
                .header(20.0, |mut h| {
                    for l in [
                        "Object",
                        "Date",
                        "Filter",
                        "Exp",
                        "Files",
                        "Temp",
                        "Telescope / camera",
                    ] {
                        h.col(|ui| {
                            ui.strong(l);
                        });
                    }
                })
                .body(|body| {
                    body.rows(20.0, self.sessions.len(), |mut row| {
                        let s = &self.sessions[row.index()];
                        row.set_selected(self.session_sel.as_deref() == Some(&s.id));
                        row.col(|ui| {
                            let name = opt(&s.object);
                            if s.is_calibration() {
                                ui.label(RichText::new(name).italics());
                            } else {
                                ui.label(name);
                            }
                        });
                        row.col(|ui| {
                            ui.label(opt(&s.date));
                        });
                        row.col(|ui| {
                            ui.label(opt(&s.filter));
                        });
                        row.col(|ui| {
                            ui.label(opt(&s.exposure));
                        });
                        row.col(|ui| {
                            ui.label(s.file_count.to_string());
                        });
                        row.col(|ui| {
                            ui.label(opt(&s.ccd_temp));
                        });
                        row.col(|ui| {
                            ui.label(format!("{} / {}", opt(&s.telescope), opt(&s.imager)));
                        });
                        if row.response().clicked() {
                            pick = Some(s.id.clone());
                        }
                    });
                });
        });
        if let Some(id) = pick {
            self.session_files = self
                .conn()
                .and_then(|c| sessions::session_files(&c, &id))
                .unwrap_or_default();
            self.session_sel = Some(id);
        }
        ui.separator();
        ui.label(format!(
            "{} files in selected session",
            self.session_files.len()
        ));
        ui.push_id("session_files", |ui| {
            TableBuilder::new(ui)
                .striped(true)
                .column(Column::initial(160.0).clip(true))
                .column(Column::remainder().clip(true))
                .header(20.0, |mut h| {
                    h.col(|ui| {
                        ui.strong("Date");
                    });
                    h.col(|ui| {
                        ui.strong("File");
                    });
                })
                .body(|body| {
                    body.rows(18.0, self.session_files.len(), |mut row| {
                        let f = &self.session_files[row.index()];
                        row.col(|ui| {
                            ui.label(opt(&f.date));
                        });
                        row.col(|ui| {
                            ui.label(&f.name);
                        });
                    });
                });
        });
    }

    // ------------------------------------------------------------ Masters

    fn masters_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            if ui
                .button("⭐ Create missing masters")
                .on_hover_text("Stack every calibration session that has no master yet")
                .clicked()
            {
                self.spawn("Create masters", |conn, cfg, p| {
                    let (made, errs) = masters::create_missing(conn, cfg, p)?;
                    for (s, e) in &errs {
                        log::warn!("session {s}: {e}");
                    }
                    Ok(format!("{} created, {} failed", made.len(), errs.len()))
                });
            }
            if ui.button("📂 Register masters from folder…").clicked() {
                self.picker.open("masters", "", ui.ctx());
            }
            if let Some(dir) = self.picker.take("masters") {
                {
                    self.spawn("Register masters", move |conn, cfg, p| {
                        let (n, errs) =
                            masters::register_folder(conn, cfg, Path::new(&dir), false, p)?;
                        Ok(format!("{n} registered, {} errors", errs.len()))
                    });
                }
            }
            if ui.button("✔ Validate").clicked() {
                self.spawn("Validate masters", |conn, _, p| {
                    let r = masters::validate(conn, p)?;
                    Ok(format!(
                        "{} ok, {} missing, {} changed",
                        r.ok,
                        r.missing.len(),
                        r.corrupt.len()
                    ))
                });
            }
            if ui.button("Clean up missing").clicked() {
                let r = self.conn().and_then(|c| masters::cleanup_missing(&c));
                self.status = match r {
                    Ok(v) => format!("{} missing masters removed", v.len()),
                    Err(e) => format!("{e:#}"),
                };
                self.reload();
            }
        });
        ui.separator();
        let mut remove: Option<i64> = None;
        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .column(Column::initial(70.0).clip(true))
            .column(Column::initial(60.0).clip(true))
            .column(Column::initial(60.0).clip(true))
            .column(Column::initial(70.0).clip(true))
            .column(Column::initial(170.0).clip(true))
            .column(Column::initial(40.0).clip(true))
            .column(Column::remainder().clip(true))
            .column(Column::exact(30.0))
            .header(20.0, |mut h| {
                for l in [
                    "Type",
                    "Frames",
                    "Exp",
                    "Filter",
                    "Telescope / camera",
                    "OK",
                    "File",
                    "",
                ] {
                    h.col(|ui| {
                        ui.strong(l);
                    });
                }
            })
            .body(|body| {
                body.rows(20.0, self.masters.len(), |mut row| {
                    let m = &self.masters[row.index()];
                    row.col(|ui| {
                        ui.label(&m.master_type);
                    });
                    row.col(|ui| {
                        ui.label(m.file_count.to_string());
                    });
                    row.col(|ui| {
                        ui.label(opt(&m.exposure));
                    });
                    row.col(|ui| {
                        ui.label(opt(&m.filter));
                    });
                    row.col(|ui| {
                        ui.label(format!("{} / {}", opt(&m.telescope), opt(&m.instrument)));
                    });
                    row.col(|ui| {
                        ui.label(if m.validated { "✔" } else { "?" });
                    });
                    row.col(|ui| {
                        ui.label(&m.path);
                    });
                    row.col(|ui| {
                        if ui
                            .small_button("✖")
                            .on_hover_text("Remove from catalogue (keeps the file)")
                            .clicked()
                        {
                            remove = Some(m.id);
                        }
                    });
                });
            });
        if let Some(id) = remove {
            if let Err(e) = self.conn().and_then(|c| masters::delete(&c, id, false)) {
                self.status = format!("{e:#}");
            }
            self.reload();
        }
    }

    // ------------------------------------------------------------ Batch

    fn batch_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Merge objects");
        ui.label("Rename an object everywhere, e.g. \"Andromeda\" -> \"M 31\".");
        let objects: Vec<String> = {
            let mut v: Vec<String> = self
                .files
                .iter()
                .filter_map(|f| f.object.clone())
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            v.sort();
            v
        };
        ui.horizontal(|ui| {
            ui.label("From");
            egui::ComboBox::from_id_salt("merge_from")
                .selected_text(&self.merge.0)
                .width(200.0)
                .show_ui(ui, |ui| {
                    for o in &objects {
                        ui.selectable_value(&mut self.merge.0, o.clone(), o);
                    }
                });
            ui.label("to");
            ui.add(egui::TextEdit::singleline(&mut self.merge.1).desired_width(200.0));
        });
        ui.checkbox(&mut self.merge.2, "Also rewrite OBJECT in the FITS headers");
        ui.checkbox(&mut self.merge.3, "Also rename and re-file the files");
        let count = self
            .files
            .iter()
            .filter(|f| f.object.as_deref() == Some(self.merge.0.as_str()))
            .count();
        if ui
            .add_enabled(
                count > 0 && !self.merge.1.trim().is_empty(),
                egui::Button::new(format!("Merge {count} files")),
            )
            .clicked()
        {
            let (from, to) = (self.merge.0.clone(), self.merge.1.trim().to_string());
            let opts = EditOptions {
                update_headers: self.merge.2,
                refile: self.merge.3,
            };
            self.spawn("Merge objects", move |conn, cfg, p| {
                let r = batch::merge_objects(conn, cfg, &from, &to, opts, p)?;
                Ok(format!(
                    "{} updated, {} moved, {} errors",
                    r.updated,
                    r.moved,
                    r.errors.len()
                ))
            });
        }
        ui.separator();

        ui.heading("Repository maintenance");
        ui.horizontal_wrapped(|ui| {
            if ui
                .button("Verify files")
                .on_hover_text("Check every catalogued file still exists")
                .clicked()
            {
                self.spawn_read("Verify", |conn, _, p| {
                    let r = batch::verify(conn, false, p)?;
                    Ok(format!(
                        "{} checked, {} missing",
                        r.checked,
                        r.missing.len()
                    ))
                });
            }
            if ui
                .button("Verify checksums")
                .on_hover_text("Re-hash every file (slower)")
                .clicked()
            {
                self.spawn_read("Verify checksums", |conn, _, p| {
                    let r = batch::verify(conn, true, p)?;
                    for f in &r.mismatched {
                        log::warn!("changed: {}", f.name);
                    }
                    Ok(format!(
                        "{} checked, {} missing, {} changed",
                        r.checked,
                        r.missing.len(),
                        r.mismatched.len()
                    ))
                });
            }
            if ui.button("Forget missing files").clicked() {
                self.spawn("Forget missing", |conn, _, p| {
                    Ok(format!(
                        "{} entries removed",
                        batch::remove_missing(conn, p)?
                    ))
                });
            }
            if ui.button("Remove empty folders").clicked() {
                self.status = format!(
                    "{} empty folders removed",
                    batch::remove_empty_dirs(&self.cfg.repo)
                );
            }
            if ui
                .button("Regenerate catalogue")
                .on_hover_text("Rebuild the catalogue by rescanning the repository")
                .clicked()
            {
                self.spawn("Regenerate", |conn, cfg, p| {
                    Ok(batch::regenerate(conn, cfg, p)?.summary())
                });
            }
        });
        ui.checkbox(
            &mut self.export_layout_by_object,
            "Export into <object>/<filter> folders",
        );
        ui.separator();

        ui.heading("Clean up telescope previews");
        ui.label("Delete JPG/PNG previews and empty Thumbnail folders under a folder (e.g. an old backup). FITS and JSON files are kept.");
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.clean_dir).desired_width(360.0));
            if ui.button("Browse…").clicked() {
                self.picker.open("clean", &self.clean_dir, ui.ctx());
            }
            if let Some(p) = self.picker.take("clean") {
                self.clean_dir = p;
            }
            if ui.button("Preview").clicked() {
                match batch::clean_previews(Path::new(&self.clean_dir), true) {
                    Ok((f, b)) => {
                        self.status = format!(
                            "{} preview files ({}) would be deleted",
                            f.len(),
                            util::human_size(b)
                        )
                    }
                    Err(e) => self.status = format!("{e:#}"),
                }
            }
            if ui
                .button(RichText::new("Delete previews").color(Color32::from_rgb(230, 150, 60)))
                .clicked()
            {
                let dir = self.clean_dir.clone();
                self.spawn_read("Clean previews", move |_, _, _| {
                    let (f, b) = batch::clean_previews(Path::new(&dir), false)?;
                    Ok(format!(
                        "{} files deleted, {} freed",
                        f.len(),
                        util::human_size(b)
                    ))
                });
            }
        });
    }

    // ------------------------------------------------------------ Duplicates

    fn duplicates_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(format!(
                "{} groups of identical files (same SHA-256)",
                self.dup_groups.len()
            ));
            if ui
                .add_enabled(
                    !self.dup_groups.is_empty(),
                    egui::Button::new("Delete duplicates, keep one of each"),
                )
                .clicked()
            {
                self.confirm = Some(Confirm::RemoveDuplicates);
            }
        });
        ui.separator();
        egui::ScrollArea::vertical().show(ui, |ui| {
            for g in &self.dup_groups {
                ui.collapsing(format!("{} × {}", g.len(), g[0].file_name()), |ui| {
                    for (i, f) in g.iter().enumerate() {
                        ui.label(format!(
                            "{} {}",
                            if i == 0 { "keep  " } else { "delete" },
                            f.name
                        ));
                    }
                });
            }
        });
    }

    // ------------------------------------------------------------ Mappings

    fn mappings_tab(&mut self, ui: &mut egui::Ui) {
        ui.label("Header values are rewritten on import. Leave \"current\" empty to fill in a missing or Unknown value.");
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("map_card")
                .selected_text(&self.new_map.0)
                .show_ui(ui, |ui| {
                    for c in [
                        "TELESCOP", "INSTRUME", "OBSERVER", "OBJECT", "FILTER", "NOTES",
                    ] {
                        ui.selectable_value(&mut self.new_map.0, c.to_string(), c);
                    }
                });
            ui.add(
                egui::TextEdit::singleline(&mut self.new_map.1)
                    .hint_text("current value")
                    .desired_width(180.0),
            );
            ui.label("->");
            ui.add(
                egui::TextEdit::singleline(&mut self.new_map.2)
                    .hint_text("replacement")
                    .desired_width(180.0),
            );
            if ui.button("Add").clicked() && !self.new_map.2.trim().is_empty() {
                let r = self.conn().and_then(|c| {
                    db::add_mapping(&c, &self.new_map.0, &self.new_map.1, self.new_map.2.trim())
                });
                if let Err(e) = r {
                    self.status = format!("{e:#}");
                }
                self.new_map.1.clear();
                self.new_map.2.clear();
                self.reload();
            }
        });
        ui.separator();
        let mut remove = None;
        egui::Grid::new("mappings")
            .striped(true)
            .num_columns(4)
            .show(ui, |ui| {
                for m in &self.mappings {
                    ui.label(&m.card);
                    ui.label(m.current.as_deref().unwrap_or("(missing / Unknown)"));
                    ui.label(format!("-> {}", m.replace.as_deref().unwrap_or("")));
                    if ui.small_button("✖").clicked() {
                        remove = Some(m.id);
                    }
                    ui.end_row();
                }
            });
        if let Some(id) = remove {
            let _ = self.conn().and_then(|c| db::remove_mapping(&c, id));
            self.reload();
        }
    }

    // ------------------------------------------------------------ Stats

    fn status_counts(&self, ui: &mut egui::Ui) {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(format!(
                "{} files · {} sessions · {} masters",
                self.files.len(),
                self.sessions.len(),
                self.masters.len()
            ));
        });
    }

    fn stats_tab(&mut self, ui: &mut egui::Ui) {
        let snapshot = self.stats.lock().unwrap().clone();
        let Some(s) = snapshot else {
            if !self.job_active("Statistics") {
                let slot = self.stats.clone();
                self.spawn_read("Statistics", move |conn, _, _| {
                    *slot.lock().unwrap() = Some(stats::compute(conn)?);
                    Ok("updated".into())
                });
            }
            ui.spinner();
            return;
        };
        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new("summary")
                .num_columns(2)
                .spacing([24.0, 4.0])
                .show(ui, |ui| {
                    for (k, v) in [
                        (
                            "Files",
                            format!(
                                "{} ({} lights, {} calibration)",
                                s.total_files, s.light_files, s.calibration_files
                            ),
                        ),
                        ("Size on disk", util::human_size(s.total_bytes)),
                        ("Sessions", s.sessions.to_string()),
                        ("Masters", s.masters.to_string()),
                        (
                            "Date range",
                            format!(
                                "{} -> {}",
                                s.first_date.clone().unwrap_or_default(),
                                s.last_date.clone().unwrap_or_default()
                            ),
                        ),
                        (
                            "Total integration",
                            stats::hours(s.by_object.iter().map(|o| o.2).sum()),
                        ),
                    ] {
                        ui.strong(k);
                        ui.label(v);
                        ui.end_row();
                    }
                });
            ui.add_space(10.0);
            ui.columns(2, |cols| {
                bar_list(
                    &mut cols[0],
                    "Integration by object",
                    s.by_object
                        .iter()
                        .take(25)
                        .map(|(o, n, e)| {
                            let label = match names::common_name(o, &self.cfg.object_names) {
                                Some(name) => format!("{o} {name} ({n})"),
                                None => format!("{o} ({n})"),
                            };
                            (label, *e)
                        })
                        .collect(),
                    true,
                );
                bar_list(
                    &mut cols[1],
                    "Integration by filter",
                    s.by_filter.iter().map(|(f, e)| (f.clone(), *e)).collect(),
                    true,
                );
                cols[1].add_space(10.0);
                bar_list(
                    &mut cols[1],
                    "Frames by telescope",
                    s.by_telescope
                        .iter()
                        .map(|(t, n)| (t.clone(), *n as f64))
                        .collect(),
                    false,
                );
                cols[1].add_space(10.0);
                bar_list(
                    &mut cols[1],
                    "Frames by camera",
                    s.by_instrument
                        .iter()
                        .map(|(t, n)| (t.clone(), *n as f64))
                        .collect(),
                    false,
                );
            });
        });
    }

    // ------------------------------------------------------------ Config

    fn config_tab(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let auto = self.auto_zoom(ctx);
        let c = &mut self.cfg_edit;
        let picker = &mut self.picker;
        egui::Grid::new("cfg")
            .num_columns(3)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                let mut path_row = |ui: &mut egui::Ui,
                                    key: &'static str,
                                    label: &str,
                                    hint: &str,
                                    p: &mut PathBuf| {
                    ui.label(label).on_hover_text(hint);
                    let mut s = p.to_string_lossy().to_string();
                    if ui
                        .add(egui::TextEdit::singleline(&mut s).desired_width(420.0))
                        .changed()
                    {
                        *p = PathBuf::from(&s);
                    }
                    if ui.button("Browse…").clicked() {
                        picker.open(key, &s, ui.ctx());
                    }
                    if let Some(x) = picker.take(key) {
                        *p = PathBuf::from(x);
                    }
                    ui.end_row();
                };
                path_row(
                    ui,
                    "cfg_repo",
                    "Repository",
                    "Where organised files are kept",
                    &mut c.repo,
                );
                path_row(
                    ui,
                    "cfg_source",
                    "Incoming folder",
                    "Default folder for Load and telescope downloads",
                    &mut c.source,
                );
                ui.label("External viewer");
                ui.add(
                    egui::TextEdit::singleline(&mut c.external_viewer)
                        .hint_text("e.g. siril (empty = system default)")
                        .desired_width(420.0),
                );
                ui.end_row();
                ui.label("Save fixed headers");
                ui.checkbox(
                    &mut c.save_modified_headers,
                    "Write normalised headers into the filed FITS files",
                );
                ui.end_row();
                ui.label("Interface size");
            ui.horizontal(|ui| {
                let label = |v: &str| match v.parse::<f32>() {
                    Ok(z) => format!("{:.0}%", z * 100.0),
                    Err(_) => format!("Auto ({:.0}%)", auto * 100.0),
                };
                egui::ComboBox::from_id_salt("ui_scale").selected_text(label(&c.ui_scale)).show_ui(ui, |ui| {
                    for v in ["auto", "1", "1.25", "1.5", "1.75", "2", "2.5"] {
                        ui.selectable_value(&mut c.ui_scale, v.to_string(), label(v));
                    }
                });
                ui.weak("Auto follows your screen resolution and desktop scaling. Ctrl +/- zooms, Ctrl 0 resets.");
            });
            ui.end_row();
            ui.label("Theme");
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut c.theme, "dark".to_string(), "Dark");
                    ui.selectable_value(&mut c.theme, "light".to_string(), "Light");
                });
                ui.end_row();
                ui.label("Masters");
                ui.horizontal(|ui| {
                    ui.label("min frames");
                    ui.add(egui::DragValue::new(&mut c.min_master_files).range(2..=500));
                    ui.label("sigma clip κ");
                    ui.add(
                        egui::DragValue::new(&mut c.sigma_clip)
                            .range(1.0..=10.0)
                            .speed(0.1),
                    );
                });
                ui.end_row();
                ui.label("Seestar Wi-Fi");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut c.seestar_host).desired_width(160.0));
                    ui.label("user");
                    ui.add(egui::TextEdit::singleline(&mut c.seestar_username).desired_width(80.0));
                    ui.label("password");
                    ui.add(
                        egui::TextEdit::singleline(&mut c.seestar_password)
                            .password(true)
                            .desired_width(80.0),
                    );
                });
                ui.end_row();
                ui.label("DWARF Wi-Fi");
                ui.add(egui::TextEdit::singleline(&mut c.dwarf_host).desired_width(160.0));
                ui.end_row();
                ui.label("Stacked results");
                ui.checkbox(
                    &mut c.include_stacked,
                    "Import the telescopes' own stacked results by default",
                );
                ui.end_row();
                ui.label("Name conflicts");
                egui::ComboBox::from_id_salt("on_conflict")
                    .selected_text(c.on_conflict.label())
                    .show_ui(ui, |ui| {
                        for v in OnConflict::ALL {
                            ui.selectable_value(&mut c.on_conflict, v, v.label());
                        }
                    });
                ui.end_row();
            });
        ui.add_space(8.0);
        ui.collapsing("Object names in folder names", |ui| {
            ui.label(
                "Well-known objects get their common name in the folder name, e.g. \
                 Light/M_76_Barbell_Nebula. Add names here, or change a built-in one; \
                 leave the name empty to use just the catalogue number.",
            );
            let mut remove = None;
            egui::Grid::new("object_names")
                .num_columns(3)
                .spacing([12.0, 4.0])
                .show(ui, |ui| {
                    for (object, name) in c.object_names.iter_mut() {
                        ui.label(object.as_str());
                        ui.add(egui::TextEdit::singleline(name).desired_width(260.0));
                        if ui.small_button("✖").on_hover_text("Remove").clicked() {
                            remove = Some(object.clone());
                        }
                        ui.end_row();
                    }
                    let (object, name) = &mut self.new_object_name;
                    ui.add(
                        egui::TextEdit::singleline(object)
                            .hint_text("M 76")
                            .desired_width(100.0),
                    );
                    ui.add(
                        egui::TextEdit::singleline(name)
                            .hint_text("Barbell Nebula")
                            .desired_width(260.0),
                    );
                    if ui
                        .add_enabled(!object.trim().is_empty(), egui::Button::new("Add"))
                        .clicked()
                    {
                        c.object_names
                            .insert(object.trim().to_string(), name.trim().to_string());
                        object.clear();
                        name.clear();
                    }
                    ui.end_row();
                });
            if let Some(o) = remove {
                c.object_names.remove(&o);
            }
            let (object, _) = &self.new_object_name;
            if !object.trim().is_empty() {
                let current = crate::names::common_name(object, &Default::default());
                ui.weak(match current {
                    Some(n) => format!("Built-in name: {n}"),
                    None => "No built-in name".to_string(),
                });
            }
            ui.weak("Save the settings, then use the bar at the top to rename existing folders.");
        });
        ui.add_space(8.0);
        ui.label(format!("Config file: {}", self.cfg.path.display()));
        ui.label(format!("Database: {}", self.db_path.display()));
        if ui.button("💾 Save settings").clicked() {
            match self.cfg_edit.save() {
                Ok(()) => {
                    self.cfg = self.cfg_edit.clone();
                    self.check_layout(ctx);
                    ctx.style_mut(|s| s.interaction.selectable_labels = false);
                    ctx.set_visuals(if self.cfg.theme == "light" {
                        egui::Visuals::light()
                    } else {
                        egui::Visuals::dark()
                    });
                    self.scope.include_stacked = self.cfg.include_stacked;
                    self.load.on_conflict = self.cfg.on_conflict;
                    self.status = "Settings saved".into();
                }
                Err(e) => self.status = format!("Could not save settings: {e:#}"),
            }
        }
    }

    // ------------------------------------------------------------ Dialogs

    fn dialogs(&mut self, ctx: &egui::Context) {
        if self.load.open {
            let mut open = true;
            egui::Window::new("Load images")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.label(
                        "Folder with FITS / XISF files (incoming folder, NAS archive, USB drive…)",
                    );
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.load.folder).desired_width(380.0),
                        );
                        if ui.button("Browse…").clicked() {
                            self.picker.open("load", &self.load.folder, ui.ctx());
                        }
                        if let Some(p) = self.picker.take("load") {
                            self.load.folder = p;
                        }
                    });
                    let also = ingest::seestar_companions(Path::new(&self.load.folder));
                    if !also.is_empty() {
                        let names: Vec<String> = also
                            .iter()
                            .map(|p| p.file_name().unwrap_or_default().to_string_lossy().into_owned())
                            .collect();
                        ui.label(
                            RichText::new(format!("Also loads: {}", names.join(", ")))
                                .small()
                                .weak(),
                        );
                    }
                    ui.add_space(6.0);
                    ui.radio_value(
                        &mut self.load.placement,
                        Placement::Copy,
                        "Copy into the repository, keep originals untouched",
                    );
                    ui.radio_value(
                        &mut self.load.placement,
                        Placement::Move,
                        "Move into the repository (rename & organise)",
                    );
                    ui.radio_value(
                        &mut self.load.placement,
                        Placement::InPlace,
                        "Catalogue in place (no renaming or moving)",
                    );
                    ui.add_space(6.0);
                    ui.label("If a different file already has the same name in the repository:");
                    ui.horizontal(|ui| {
                        for c in OnConflict::ALL {
                            ui.radio_value(&mut self.load.on_conflict, c, c.label());
                        }
                    });
                    ui.weak("Identical files are never copied twice, and empty or half-copied leftovers are always replaced.");
                    ui.add_space(6.0);
                    ui.checkbox(
                        &mut self.load.dry_run,
                        "Dry run — only show what would happen (plan is written to the log)",
                    );
                    ui.add_space(6.0);
                    if ui.button(RichText::new("Start").strong()).clicked() {
                        let src = PathBuf::from(&self.load.folder);
                        let opts = IngestOptions {
                            placement: self.load.placement,
                            dry_run: self.load.dry_run,
                            on_conflict: self.load.on_conflict,
                        };
                        self.submit(
                            if opts.dry_run { "Dry run" } else { "Load" },
                            !opts.dry_run,
                            Box::new(move |conn, cfg, p| {
                                let r = ingest::ingest_folder(conn, cfg, &src, opts, p)?;
                                if opts.dry_run {
                                    for (a, b) in &r.placed {
                                        log::info!("plan: {} -> {}", a.display(), b.display());
                                    }
                                }
                                for (a, e) in &r.errors {
                                    log::warn!("{}: {e}", a.display());
                                }
                                for (a, b) in &r.conflicts {
                                    log::warn!(
                                        "{}: skipped, a different file already exists at {}",
                                        a.display(),
                                        b.display()
                                    );
                                }
                                Ok(r.summary())
                            }),
                        );
                        self.load.open = false;
                    }
                });
            if !open {
                self.load.open = false;
            }
        }

        if self.edit.open {
            let fields: Vec<&str> = batch::EDITABLE.iter().map(|e| e.0).collect();
            let mut open = true;
            egui::Window::new(format!("Edit {} files", self.selected.len()))
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        egui::ComboBox::from_id_salt("edit_field")
                            .selected_text(fields[self.edit.field])
                            .show_ui(ui, |ui| {
                                for (i, f) in fields.iter().enumerate() {
                                    ui.selectable_value(&mut self.edit.field, i, *f);
                                }
                            });
                        ui.label("=");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.edit.value).desired_width(220.0),
                        );
                    });
                    ui.checkbox(
                        &mut self.edit.headers,
                        "Also write it into the FITS headers",
                    );
                    ui.checkbox(&mut self.edit.refile, "Rename and re-file to match");
                    if ui
                        .add_enabled(
                            !self.edit.value.trim().is_empty(),
                            egui::Button::new("Apply"),
                        )
                        .clicked()
                    {
                        let ids: Vec<String> = self.selected.iter().cloned().collect();
                        let field = fields[self.edit.field].to_string();
                        let value = self.edit.value.trim().to_string();
                        let opts = EditOptions {
                            update_headers: self.edit.headers,
                            refile: self.edit.refile,
                        };
                        self.spawn("Edit files", move |conn, cfg, p| {
                            let r = batch::set_field(conn, cfg, &ids, &field, &value, opts, p)?;
                            Ok(format!(
                                "{} updated, {} moved, {} errors",
                                r.updated,
                                r.moved,
                                r.errors.len()
                            ))
                        });
                        self.edit.open = false;
                    }
                });
            if !open {
                self.edit.open = false;
            }
        }

        let Some(confirm) = &self.confirm else { return };
        let text = match confirm {
            Confirm::DeleteFiles { ids, from_disk: true } => format!("Permanently delete {} files from disk?", ids.len()),
            Confirm::DeleteFiles { ids, from_disk: false } => format!("Remove {} files from the catalogue? (files stay on disk)", ids.len()),
            Confirm::RemoveDuplicates => {
                let n: usize = self.dup_groups.iter().map(|g| g.len() - 1).sum();
                format!("Delete {n} duplicate files from disk, keeping one copy of each?")
            }
            Confirm::ClearSessions => "Remove all sessions? Files are kept; you can re-create sessions any time.".to_string(),
            Confirm::ImportWithDelete => "Files will be DELETED from the telescope after they are safely catalogued. Continue?".to_string(),
            Confirm::QuitDuringJob => "A task is still running. Quit anyway? Work finished so far is kept.".to_string(),
        };
        let mut answer = None;
        egui::Window::new("Please confirm")
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(text);
                ui.horizontal(|ui| {
                    if ui
                        .button(RichText::new("Yes").color(Color32::from_rgb(230, 120, 60)))
                        .clicked()
                    {
                        answer = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        answer = Some(false);
                    }
                });
            });
        match answer {
            Some(true) => {
                let c = self.confirm.take().unwrap();
                match c {
                    Confirm::DeleteFiles { ids, from_disk } => {
                        self.spawn("Delete", move |conn, _, _| {
                            let (n, errs) = batch::delete_files(conn, &ids, from_disk)?;
                            Ok(format!("{n} removed, {} errors", errs.len()))
                        });
                        self.selected.clear();
                    }
                    Confirm::RemoveDuplicates => self.spawn("Remove duplicates", |conn, _, _| {
                        let (n, b) = batch::remove_duplicates(conn)?;
                        Ok(format!("{n} files removed, {} freed", util::human_size(b)))
                    }),
                    Confirm::ClearSessions => {
                        let r = self.conn().and_then(|mut c| sessions::clear_all(&mut c));
                        self.status = match r {
                            Ok(n) => format!("{n} sessions removed"),
                            Err(e) => format!("{e:#}"),
                        };
                        self.session_sel = None;
                        self.session_files.clear();
                        self.reload();
                    }
                    Confirm::ImportWithDelete => self.start_import(true),
                    Confirm::QuitDuringJob => {
                        self.queued.clear();
                        for job in &self.jobs {
                            job.state.cancel.store(true, Ordering::SeqCst);
                        }
                        self.allow_close = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
            }
            Some(false) => self.confirm = None,
            None => {}
        }
    }
}

fn bar_list(ui: &mut egui::Ui, title: &str, rows: Vec<(String, f64)>, as_hours: bool) {
    ui.strong(title);
    let max = rows.iter().map(|r| r.1).fold(0.0, f64::max).max(1e-9);
    let accent = ui.visuals().selection.bg_fill;
    for (label, v) in rows {
        ui.horizontal(|ui| {
            ui.allocate_ui_with_layout(
                egui::vec2(220.0, 18.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.set_width(220.0);
                    ui.add(egui::Label::new(&label).truncate())
                        .on_hover_text(&label);
                },
            );
            let width = 220.0;
            let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 14.0), egui::Sense::hover());
            let w = (v / max) as f32 * width;
            ui.painter().rect_filled(
                egui::Rect::from_min_size(rect.min, egui::vec2(w.max(1.0), rect.height())),
                3.0,
                accent,
            );
            ui.label(if as_hours {
                stats::hours(v)
            } else {
                format!("{v:.0}")
            });
        });
    }
}

fn log_tab(ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.label(format!("{} lines", logging::count()));
        if ui.button("Clear").clicked() {
            logging::clear();
        }
    });
    ui.separator();
    let lines = logging::recent();
    egui::ScrollArea::vertical()
        .stick_to_bottom(true)
        .auto_shrink([false; 2])
        .show_rows(ui, 16.0, lines.len(), |ui, range| {
            for l in &lines[range] {
                let color = if l.contains("ERROR") {
                    Some(Color32::from_rgb(230, 90, 90))
                } else if l.contains("WARN") {
                    Some(Color32::from_rgb(230, 170, 60))
                } else {
                    None
                };
                let t = RichText::new(l).monospace();
                ui.label(if let Some(c) = color { t.color(c) } else { t });
            }
        });
}
