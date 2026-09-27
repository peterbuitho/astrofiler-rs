//! Logger that writes to stderr and keeps recent lines for the GUI log tab.

use log::{Level, LevelFilter, Log, Metadata, Record};
use std::collections::VecDeque;
use std::sync::Mutex;

const KEEP: usize = 5000;

static LINES: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());

struct Logger {
    level: LevelFilter,
    to_stderr: bool,
    file: Option<Mutex<std::fs::File>>,
}

/// `astrofiler.log` in the user data folder, restarted when it passes 5 MB.
pub fn log_path() -> Option<std::path::PathBuf> {
    dirs::data_dir().map(|d| d.join("astrofiler").join("astrofiler.log"))
}

fn open_log_file() -> Option<std::fs::File> {
    let path = log_path()?;
    std::fs::create_dir_all(path.parent()?).ok()?;
    let too_big = std::fs::metadata(&path)
        .map(|m| m.len() > 5 << 20)
        .unwrap_or(false);
    std::fs::OpenOptions::new()
        .create(true)
        .append(!too_big)
        .write(true)
        .truncate(too_big)
        .open(path)
        .ok()
}

impl Log for Logger {
    fn enabled(&self, m: &Metadata) -> bool {
        m.level() <= self.level && m.target().starts_with("astrofiler")
    }
    fn log(&self, r: &Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let line = format!(
            "{} {:<5} {}",
            chrono::Local::now().format("%H:%M:%S"),
            r.level(),
            r.args()
        );
        if self.to_stderr && (r.level() <= Level::Warn || self.level >= LevelFilter::Debug) {
            eprintln!("{line}");
        }
        if let Some(f) = &self.file {
            use std::io::Write;
            let date = chrono::Local::now().format("%Y-%m-%d");
            let _ = writeln!(f.lock().unwrap(), "{date} {line}");
        }
        let mut lines = LINES.lock().unwrap();
        if lines.len() >= KEEP {
            lines.pop_front();
        }
        lines.push_back(line);
    }
    fn flush(&self) {}
}

pub fn init(verbose: bool, to_stderr: bool) {
    let level = if verbose {
        LevelFilter::Debug
    } else {
        LevelFilter::Info
    };
    let file = open_log_file().map(Mutex::new);
    let _ = log::set_boxed_logger(Box::new(Logger {
        level,
        to_stderr,
        file,
    }));
    log::set_max_level(level);
}

/// Snapshot of recent log lines (oldest first).
pub fn recent() -> Vec<String> {
    LINES.lock().unwrap().iter().cloned().collect()
}

pub fn count() -> usize {
    LINES.lock().unwrap().len()
}

pub fn clear() {
    LINES.lock().unwrap().clear();
}
