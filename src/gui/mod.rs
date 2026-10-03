//! Desktop GUI (egui): downloads from smart telescopes into a local folder
//! and hands finished folders to the web version, which keeps the catalogue.

mod picker;

use crate::batch;
use crate::config::Config;
use crate::logging;
use crate::progress::{JobState, Progress};
use crate::telescope::{self, Found, Link, RemoteFile};
use crate::util;
use anyhow::Result;
use eframe::egui::{self, Align2, Color32, RichText};
use egui_extras::{Column, TableBuilder};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Telescopes,
    Inbox,
    Config,
    Log,
}

const TABS: &[(Tab, &str)] = &[
    (Tab::Telescopes, "Telescopes"),
    (Tab::Inbox, "Send to inbox"),
    (Tab::Config, "Settings"),
    (Tab::Log, "Log"),
];

/// The task that moves folders to the inbox; the web version opens when it
/// has finished.
const MOVE_TO_INBOX: &str = "Move to inbox";

struct Job {
    name: String,
    state: Arc<JobState>,
}

/// Something the user must confirm before it happens.
enum Confirm {
    DownloadWithDelete,
    /// Whole telescope folders (remote paths) and single files to delete.
    DeleteFromTelescope {
        folders: Vec<String>,
        files: Vec<RemoteFile>,
    },
    QuitDuringJob,
}

/// A line of the telescope file list: a folder, or a file of an expanded folder.
#[derive(Clone, Copy)]
enum ScopeRow {
    /// Index into the folder groups.
    Folder(usize),
    /// Index into the scanned files.
    File(usize),
}

struct ScopeUi {
    telescope: usize,
    usb: bool,
    host: String,
    usb_path: String,
    found: Arc<Mutex<Vec<Found>>>,
    files: Arc<Mutex<Vec<RemoteFile>>>,
    selected: Vec<bool>,
    /// Telescope folders whose files are shown in the list.
    expanded: HashSet<String>,
    /// Folder last ticked, for shift-click ranges.
    anchor: Option<usize>,
    include_stacked: bool,
    delete_after: bool,
    dest: String,
}

/// A folder (or loose file) in the download folder.
#[derive(Clone)]
struct LocalEntry {
    path: PathBuf,
    name: String,
    files: usize,
    bytes: u64,
}

pub struct App {
    cfg: Config,
    allow_close: bool,
    /// Zoom last applied from the interface-size setting (None = not yet).
    applied_zoom: Option<f32>,
    /// "Auto" interface size, worked out once the monitor size is known.
    auto_zoom: Option<f32>,
    tab: Tab,
    status: String,
    jobs: Vec<Job>,
    confirm: Option<Confirm>,
    picker: picker::FolderPicker,
    scope: ScopeUi,
    /// What is in the download folder, and which of it is ticked.
    local: Arc<Mutex<Vec<LocalEntry>>>,
    local_sel: HashSet<PathBuf>,
    /// Folder `local` was listed from (None = not listed yet).
    local_dir: Option<String>,
    cfg_edit: Config,
}

impl App {
    fn new(cc: &eframe::CreationContext, cfg: Config) -> Self {
        cc.egui_ctx.style_mut(interaction_style);
        cc.egui_ctx.set_visuals(if cfg.theme == "light" {
            egui::Visuals::light()
        } else {
            egui::Visuals::dark()
        });
        let mut app = App {
            allow_close: false,
            applied_zoom: None,
            auto_zoom: None,
            tab: Tab::Telescopes,
            status: String::new(),
            jobs: Vec::new(),
            confirm: None,
            picker: Default::default(),
            scope: ScopeUi {
                telescope: 0,
                usb: true,
                host: telescope::all()[0].default_host(&cfg),
                usb_path: String::new(),
                found: Arc::new(Mutex::new(vec![])),
                files: Arc::new(Mutex::new(vec![])),
                selected: vec![],
                expanded: HashSet::new(),
                anchor: None,
                include_stacked: cfg.include_stacked,
                delete_after: false,
                dest: cfg.source.to_string_lossy().into(),
            },
            local: Arc::new(Mutex::new(vec![])),
            local_sel: HashSet::new(),
            local_dir: None,
            cfg_edit: cfg.clone(),
            cfg,
        };
        // Offer USB telescopes straight away.
        let usb = telescope::find_usb();
        if let Some(f) = usb.first() {
            app.status = format!("Found {}", f.label);
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

    /// Run a task on a background thread.
    fn spawn<F>(&mut self, name: &str, work: F)
    where
        F: FnOnce(&Config, &JobState) -> Result<String> + Send + 'static,
    {
        if self.jobs.iter().any(|j| j.name == name) {
            self.status = format!("{name} is already running");
            return;
        }
        let state = Arc::new(JobState::default());
        let st = state.clone();
        let cfg = self.cfg.clone();
        let label = name.to_string();
        std::thread::spawn(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&cfg, &st)))
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
        });
    }

    fn poll_jobs(&mut self, ctx: &egui::Context) {
        let (finished, running): (Vec<Job>, Vec<Job>) = std::mem::take(&mut self.jobs)
            .into_iter()
            .partition(|j| j.state.done.load(Ordering::SeqCst));
        self.jobs = running;
        for job in finished {
            let result = job.state.result.lock().unwrap().take();
            let ok = matches!(result, Some(Ok(_)));
            self.status = match result {
                Some(Ok(msg)) => format!("{}: {msg}", job.name),
                Some(Err(e)) => format!("{} failed: {e}", job.name),
                None => String::new(),
            };
            if job.name.ends_with(" telescope") {
                let n = self.scope.files.lock().unwrap().len();
                self.scope.selected = vec![false; n];
                self.scope.anchor = None;
            }
            // Downloads and moves change what is in the download folder.
            self.local_dir = None;
            if job.name == MOVE_TO_INBOX && ok {
                self.local_sel.clear();
                // The files are in the inbox: on to loading them.
                self.open_web("/load");
            }
        }
        if !self.jobs.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    /// Open a page of the web version in the browser.
    fn open_web(&mut self, page: &str) {
        let base = self.cfg.web_url.trim().trim_end_matches('/');
        if base.is_empty() {
            self.status = format!(
                "{} Set the web version's address in Settings to open it from here.",
                self.status
            )
            .trim()
            .to_string();
            return;
        }
        let url = if base.contains("://") {
            format!("{base}{page}")
        } else {
            format!("http://{base}{page}")
        };
        if let Err(e) = util::open_external("", Path::new(&url)) {
            self.status = format!("Could not open {url}: {e:#}");
        }
    }

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
        self.apply_ui_scale(ctx);
        if ctx.input(|i| i.viewport().close_requested())
            && !self.jobs.is_empty()
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
                ui.separator();
                if ui
                    .button("🌐 Open web version")
                    .on_hover_text("The catalogue, in your browser")
                    .clicked()
                {
                    self.status.clear();
                    self.open_web("/");
                }
            });
            ui.add_space(2.0);
        });

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            for job in &self.jobs {
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
                });
            }
            if self.jobs.is_empty() {
                ui.label(&self.status);
            }
        });

        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Telescopes => self.telescopes_tab(ui),
            Tab::Inbox => self.inbox_tab(ui, ctx),
            Tab::Config => {
                egui::ScrollArea::vertical().show(ui, |ui| self.config_tab(ui, ctx));
            }
            Tab::Log => log_tab(ui),
        });

        self.dialogs(ctx);
    }
}

fn interaction_style(s: &mut egui::Style) {
    // Labels must not grab clicks, or table rows can't be selected.
    s.interaction.selectable_labels = false;
    // Panel dividers are easier to catch (the default is 5 px either side).
    s.interaction.resize_grab_radius_side = 8.0;
}

impl App {
    // ------------------------------------------------------------ Telescopes

    fn telescopes_tab(&mut self, ui: &mut egui::Ui) {
        let scopes = telescope::all();
        ui.heading("Download from a smart telescope");
        ui.label("FITS files and DWARF session info (shotsInfo.json) are transferred — the telescope's JPG/PNG previews and thumbnails are skipped. They are saved in the telescope's own folders.");
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
                self.spawn("Network scan", move |cfg, p| {
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
                        RichText::new("Delete from telescope after download")
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
                self.spawn("Scanning telescope", move |cfg, p| {
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
            self.scope.selected = vec![false; files.len()];
        }
        if files.is_empty() {
            return;
        }
        ui.separator();
        // One row per folder, as laid out on the telescope; its files are
        // listed underneath when expanded.
        let mut groups: Vec<(&str, Vec<usize>)> = Vec::new();
        let mut by_dir: HashMap<&str, usize> = HashMap::new();
        for (i, f) in files.iter().enumerate() {
            let dir = f.local_dir.as_str();
            let g = *by_dir.entry(dir).or_insert_with(|| {
                groups.push((dir, Vec::new()));
                groups.len() - 1
            });
            groups[g].1.push(i);
        }
        let mut rows: Vec<ScopeRow> = Vec::new();
        for (g, (dir, idx)) in groups.iter().enumerate() {
            rows.push(ScopeRow::Folder(g));
            if self.scope.expanded.contains(*dir) {
                rows.extend(idx.iter().map(|&i| ScopeRow::File(i)));
            }
        }

        let sel_n = self.scope.selected.iter().filter(|s| **s).count();
        let sel_folders = groups
            .iter()
            .filter(|(_, idx)| idx.iter().any(|&i| self.scope.selected[i]))
            .count();
        let sel_size: u64 = files
            .iter()
            .zip(&self.scope.selected)
            .filter(|(_, s)| **s)
            .map(|(f, _)| f.size)
            .sum();
        ui.horizontal_wrapped(|ui| {
            ui.label(format!(
                "{sel_folders} of {} folders, {sel_n} of {} files selected ({})",
                groups.len(),
                files.len(),
                util::human_size(sel_size)
            ));
            if ui.button("All").clicked() {
                self.scope.selected.iter_mut().for_each(|s| *s = true);
            }
            if ui.button("None").clicked() {
                self.scope.selected.iter_mut().for_each(|s| *s = false);
            }
            if ui.button("Expand all").clicked() {
                self.scope.expanded = groups.iter().map(|(d, _)| d.to_string()).collect();
            }
            if ui.button("Collapse all").clicked() {
                self.scope.expanded.clear();
            }
            if ui
                .add_enabled(
                    sel_n > 0,
                    egui::Button::new(RichText::new("⬇ Download selected").strong()),
                )
                .clicked()
            {
                if self.scope.delete_after {
                    self.confirm = Some(Confirm::DownloadWithDelete);
                } else {
                    self.start_download(false);
                }
            }
            if ui
                .add_enabled(
                    sel_n > 0,
                    egui::Button::new(
                        RichText::new("🗑 Delete selected").color(Color32::from_rgb(230, 120, 60)),
                    ),
                )
                .on_hover_text("Delete from the telescope without downloading")
                .clicked()
            {
                // A fully ticked folder goes as a whole, previews included.
                let mut folders = Vec::new();
                let mut single = Vec::new();
                for (_, idx) in &groups {
                    let picked: Vec<usize> = idx
                        .iter()
                        .copied()
                        .filter(|&i| self.scope.selected[i])
                        .collect();
                    let dir = files[idx[0]].path.rsplit_once('/').map(|(d, _)| d);
                    match dir {
                        Some(d) if picked.len() == idx.len() => folders.push(d.to_string()),
                        _ => single.extend(picked.into_iter().map(|i| files[i].clone())),
                    }
                }
                self.confirm = Some(Confirm::DeleteFromTelescope {
                    folders,
                    files: single,
                });
            }
            ui.weak("Shift-click a folder's checkbox to tick a range.");
        });
        TableBuilder::new(ui)
            .striped(true)
            .column(Column::exact(24.0))
            .column(
                Column::initial(460.0)
                    .at_least(160.0)
                    .clip(true)
                    .resizable(true),
            )
            .column(Column::initial(80.0).clip(true))
            .column(Column::initial(80.0).clip(true))
            .column(Column::remainder().clip(true))
            .header(20.0, |mut h| {
                for l in ["", "Folder / file", "Files", "Size", "Kind"] {
                    h.col(|ui| {
                        ui.strong(l);
                    });
                }
            })
            .body(|body| {
                body.rows(20.0, rows.len(), |mut row| match rows[row.index()] {
                    ScopeRow::Folder(g) => {
                        let (dir, idx) = &groups[g];
                        let n_sel = idx.iter().filter(|&&i| self.scope.selected[i]).count();
                        row.col(|ui| {
                            let mut on = n_sel == idx.len();
                            let partly = n_sel > 0 && !on;
                            if ui
                                .add(egui::Checkbox::without_text(&mut on).indeterminate(partly))
                                .clicked()
                            {
                                let range = match self.scope.anchor {
                                    Some(a) if ui.input(|i| i.modifiers.shift) => {
                                        let a = a.min(groups.len() - 1);
                                        a.min(g)..=a.max(g)
                                    }
                                    _ => g..=g,
                                };
                                for (_, idx) in &groups[range] {
                                    for &i in idx {
                                        self.scope.selected[i] = on;
                                    }
                                }
                                self.scope.anchor = Some(g);
                            }
                        });
                        row.col(|ui| {
                            let open = self.scope.expanded.contains(*dir);
                            let arrow = if open { "⏷" } else { "⏵" };
                            let label = RichText::new(format!("{arrow} 📁 {dir}")).strong();
                            if ui.selectable_label(false, label).clicked() {
                                if open {
                                    self.scope.expanded.remove(*dir);
                                } else {
                                    self.scope.expanded.insert(dir.to_string());
                                }
                            }
                        });
                        row.col(|ui| {
                            ui.label(if n_sel == idx.len() {
                                idx.len().to_string()
                            } else {
                                format!("{n_sel} / {}", idx.len())
                            });
                        });
                        row.col(|ui| {
                            let size: u64 = idx.iter().map(|&i| files[i].size).sum();
                            ui.label(util::human_size(size));
                        });
                        row.col(|ui| {
                            let mut kinds: Vec<&str> = Vec::new();
                            for &i in idx {
                                if !kinds.contains(&files[i].kind) {
                                    kinds.push(files[i].kind);
                                }
                            }
                            ui.label(kinds.join(", "));
                        });
                    }
                    ScopeRow::File(i) => {
                        let f = &files[i];
                        row.col(|ui| {
                            ui.checkbox(&mut self.scope.selected[i], "");
                        });
                        row.col(|ui| {
                            ui.horizontal(|ui| {
                                ui.add_space(34.0);
                                ui.label(&f.name);
                            });
                        });
                        row.col(|_| {});
                        row.col(|ui| {
                            ui.label(util::human_size(f.size));
                        });
                        row.col(|ui| {
                            ui.label(f.kind);
                        });
                    }
                });
            });
    }

    fn start_download(&mut self, delete: bool) {
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
        let stacked = self.scope.include_stacked;
        self.spawn("Downloading from telescope", move |cfg, p| {
            let mut s = telescope::connect(cfg, t, &link)?;
            let r = s.download(&files, &dest, delete, p)?;
            for (path, e) in &r.failed {
                log::warn!("{path}: {e}");
            }
            // Rescan so the list reflects what is left on the telescope.
            if let Ok(left) = s.scan(stacked) {
                *slot.lock().unwrap() = left;
            }
            Ok(r.summary())
        });
    }

    fn start_delete(&mut self, folders: Vec<String>, files: Vec<RemoteFile>) {
        let scopes = telescope::all();
        let t = scopes[self.scope.telescope.min(scopes.len() - 1)];
        let link = if self.scope.usb {
            Link::Usb(PathBuf::from(&self.scope.usb_path))
        } else {
            Link::Network(self.scope.host.clone())
        };
        let stacked = self.scope.include_stacked;
        let slot = self.scope.files.clone();
        self.spawn("Deleting from telescope", move |cfg, p| {
            let mut s = telescope::connect(cfg, t, &link)?;
            let r = s.delete(&folders, &files, p);
            for (path, e) in &r.failed {
                log::warn!("{path}: {e}");
            }
            // Rescan so the list reflects what is left on the telescope.
            if let Ok(left) = s.scan(stacked) {
                *slot.lock().unwrap() = left;
            }
            Ok(r.summary())
        });
    }

    // ------------------------------------------------------------ Inbox

    /// List the download folder in the background.
    fn list_local(&mut self, ctx: &egui::Context) {
        let dir = self.scope.dest.clone();
        self.local_dir = Some(dir.clone());
        let (slot, ctx) = (self.local.clone(), ctx.clone());
        std::thread::spawn(move || {
            let mut entries: Vec<LocalEntry> = std::fs::read_dir(&dir)
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    !p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with('.'))
                })
                .filter_map(|path| {
                    let (mut files, mut bytes) = (0, 0);
                    for f in walkdir::WalkDir::new(&path)
                        .into_iter()
                        .filter_map(|e| e.ok())
                        .filter(|e| e.file_type().is_file())
                    {
                        files += 1;
                        bytes += f.metadata().map(|m| m.len()).unwrap_or(0);
                    }
                    Some(LocalEntry {
                        name: path.file_name()?.to_string_lossy().into_owned(),
                        path,
                        files,
                        bytes,
                    })
                })
                .collect();
            entries.sort_by_key(|e| e.name.to_lowercase());
            *slot.lock().unwrap() = entries;
            ctx.request_repaint();
        });
    }

    fn inbox_tab(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        if self.local_dir.as_deref() != Some(self.scope.dest.as_str()) {
            self.list_local(ctx);
        }
        let entries = self.local.lock().unwrap().clone();
        self.local_sel
            .retain(|p| entries.iter().any(|e| &e.path == p));
        let inbox = self.cfg.inbox.clone();
        let inbox_set = !inbox.as_os_str().is_empty();

        ui.heading("Send to the web version");
        ui.label(format!(
            "What is in the download folder {}. Move the folders you are done with to the inbox, \
             where the web version files them into the repository.",
            self.scope.dest
        ));
        if inbox_set {
            ui.label(format!("Inbox: {}", inbox.display()));
        } else {
            ui.label(
                RichText::new("Set the inbox folder in Settings first.")
                    .color(Color32::from_rgb(230, 150, 60)),
            );
        }
        ui.add_space(6.0);

        let bytes: u64 = entries
            .iter()
            .filter(|e| self.local_sel.contains(&e.path))
            .map(|e| e.bytes)
            .sum();
        ui.horizontal_wrapped(|ui| {
            ui.label(format!(
                "{} of {} selected ({})",
                self.local_sel.len(),
                entries.len(),
                util::human_size(bytes)
            ));
            if ui.button("All").clicked() {
                self.local_sel = entries.iter().map(|e| e.path.clone()).collect();
            }
            if ui.button("None").clicked() {
                self.local_sel.clear();
            }
            if ui.button("Refresh").clicked() {
                self.list_local(ctx);
            }
            if ui
                .add_enabled(
                    inbox_set && !self.local_sel.is_empty(),
                    egui::Button::new(RichText::new("📤 Move selected to inbox").strong()),
                )
                .on_hover_text("Then opens the web version's Load page")
                .clicked()
            {
                let picked: Vec<PathBuf> = entries
                    .iter()
                    .filter(|e| self.local_sel.contains(&e.path))
                    .map(|e| e.path.clone())
                    .collect();
                self.spawn(MOVE_TO_INBOX, move |cfg, p| {
                    let r = batch::move_to_inbox(&picked, &cfg.inbox, p)?;
                    for f in &r.skipped {
                        log::warn!("Not moved: {} (the inbox already has it)", f.display());
                    }
                    for (f, e) in &r.errors {
                        log::warn!("Not moved: {} ({e})", f.display());
                    }
                    Ok(r.summary())
                });
            }
        });
        ui.separator();

        TableBuilder::new(ui)
            .striped(true)
            .column(Column::auto())
            .column(Column::initial(520.0).clip(true))
            .column(Column::initial(70.0))
            .column(Column::remainder())
            .header(20.0, |mut h| {
                for l in ["", "Folder", "Files", "Size"] {
                    h.col(|ui| {
                        ui.strong(l);
                    });
                }
            })
            .body(|body| {
                body.rows(20.0, entries.len(), |mut row| {
                    let e = &entries[row.index()];
                    row.col(|ui| {
                        let mut on = self.local_sel.contains(&e.path);
                        if ui.checkbox(&mut on, "").changed() {
                            if on {
                                self.local_sel.insert(e.path.clone());
                            } else {
                                self.local_sel.remove(&e.path);
                            }
                        }
                    });
                    row.col(|ui| {
                        ui.label(&e.name);
                    });
                    row.col(|ui| {
                        ui.label(e.files.to_string());
                    });
                    row.col(|ui| {
                        ui.label(util::human_size(e.bytes));
                    });
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
                    "cfg_source",
                    "Download folder",
                    "Where telescope downloads are saved on this computer",
                    &mut c.source,
                );
                path_row(
                    ui,
                    "cfg_inbox",
                    "Inbox folder",
                    "The web version's incoming folder as this computer sees it, e.g. on the mounted NAS share",
                    &mut c.inbox,
                );
                ui.label("Web version")
                    .on_hover_text("Address of the web version that keeps the catalogue");
                ui.add(
                    egui::TextEdit::singleline(&mut c.web_url)
                        .hint_text("e.g. http://nas:8080")
                        .desired_width(420.0),
                );
                ui.end_row();
                ui.label("Interface size");
                ui.horizontal(|ui| {
                    let label = |v: &str| match v.parse::<f32>() {
                        Ok(z) => format!("{:.0}%", z * 100.0),
                        Err(_) => format!("Auto ({:.0}%)", auto * 100.0),
                    };
                    egui::ComboBox::from_id_salt("ui_scale")
                        .selected_text(label(&c.ui_scale))
                        .show_ui(ui, |ui| {
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
                    "Download the telescopes' own stacked results by default",
                );
                ui.end_row();
            });
        ui.add_space(8.0);
        ui.label(format!("Config file: {}", self.cfg.path.display()));
        if ui.button("💾 Save settings").clicked() {
            match self.cfg_edit.save() {
                Ok(()) => {
                    // The download folder follows the setting unless it was changed by hand.
                    if self.scope.dest == self.cfg.source.to_string_lossy() {
                        self.scope.dest = self.cfg_edit.source.to_string_lossy().into();
                    }
                    self.cfg = self.cfg_edit.clone();
                    ctx.style_mut(interaction_style);
                    ctx.set_visuals(if self.cfg.theme == "light" {
                        egui::Visuals::light()
                    } else {
                        egui::Visuals::dark()
                    });
                    self.scope.include_stacked = self.cfg.include_stacked;
                    self.status = "Settings saved".into();
                }
                Err(e) => self.status = format!("Could not save settings: {e:#}"),
            }
        }
    }

    // ------------------------------------------------------------ Dialogs

    fn dialogs(&mut self, ctx: &egui::Context) {
        let Some(confirm) = &self.confirm else { return };
        let text = match confirm {
            Confirm::DownloadWithDelete => "Files will be DELETED from the telescope once they are downloaded completely. Continue?".to_string(),
            Confirm::DeleteFromTelescope { folders, files } => {
                let mut what = Vec::new();
                if !folders.is_empty() {
                    what.push(format!(
                        "{} folder(s) with everything in them (previews, thumbnails and files not listed here)",
                        folders.len()
                    ));
                }
                if !files.is_empty() {
                    what.push(format!("{} file(s)", files.len()));
                }
                format!(
                    "Permanently DELETE {} from the telescope WITHOUT downloading? This cannot be undone.",
                    what.join(" and ")
                )
            }
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
            Some(true) => match self.confirm.take().unwrap() {
                Confirm::DownloadWithDelete => self.start_download(true),
                Confirm::DeleteFromTelescope { folders, files } => {
                    self.start_delete(folders, files)
                }
                Confirm::QuitDuringJob => {
                    for job in &self.jobs {
                        job.state.cancel.store(true, Ordering::SeqCst);
                    }
                    self.allow_close = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            },
            Some(false) => self.confirm = None,
            None => {}
        }
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
