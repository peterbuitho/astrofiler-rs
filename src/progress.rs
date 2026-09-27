//! Progress reporting shared by the CLI (terminal bar) and GUI (job panel).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

pub trait Progress: Sync {
    fn update(&self, done: usize, total: usize, message: &str);
    fn cancelled(&self) -> bool {
        false
    }
}

pub struct NoProgress;
impl Progress for NoProgress {
    fn update(&self, _: usize, _: usize, _: &str) {}
}

/// Terminal progress bar for CLI commands.
pub struct BarProgress {
    bar: indicatif::ProgressBar,
}

impl BarProgress {
    pub fn new() -> Self {
        let bar = indicatif::ProgressBar::new(0);
        bar.set_style(
            indicatif::ProgressStyle::with_template(
                "{spinner} [{elapsed_precise}] {bar:40.cyan/blue} {pos}/{len} {wide_msg}",
            )
            .unwrap()
            .progress_chars("=> "),
        );
        BarProgress { bar }
    }
    pub fn finish(&self) {
        self.bar.finish_and_clear();
    }
}

impl Default for BarProgress {
    fn default() -> Self {
        Self::new()
    }
}

impl Progress for BarProgress {
    fn update(&self, done: usize, total: usize, message: &str) {
        self.bar.set_length(total as u64);
        self.bar.set_position(done as u64);
        self.bar.set_message(message.to_string());
    }
}

/// Shared state polled by the GUI while a background job runs.
#[derive(Default)]
pub struct JobState {
    pub status: Mutex<(usize, usize, String)>,
    pub cancel: AtomicBool,
    pub done: AtomicBool,
    pub result: Mutex<Option<Result<String, String>>>,
}

impl JobState {
    pub fn snapshot(&self) -> (usize, usize, String) {
        self.status.lock().unwrap().clone()
    }
    pub fn finish(&self, result: Result<String, String>) {
        *self.result.lock().unwrap() = Some(result);
        self.done.store(true, Ordering::SeqCst);
    }
}

impl Progress for JobState {
    fn update(&self, done: usize, total: usize, message: &str) {
        *self.status.lock().unwrap() = (done, total, message.to_string());
    }
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}
