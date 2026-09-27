//! Logger that writes to stderr and keeps recent lines for the GUI log tab.

use log::{Level, LevelFilter, Log, Metadata, Record};
use std::collections::VecDeque;
use std::sync::Mutex;

const KEEP: usize = 5000;

static LINES: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());

struct Logger {
    level: LevelFilter,
    to_stderr: bool,
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
    let _ = log::set_boxed_logger(Box::new(Logger { level, to_stderr }));
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
