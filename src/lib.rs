//! AstroFiler-rs: fast astronomical image filing and cataloguing.
//!
//! A Rust port of the core of [AstroFiler](https://github.com/gordtulloch/astrofiler-gui):
//! repository ingest (FITS, XISF, zip, gzip), a SQLite catalogue compatible with the
//! original, sessions, batch management, duplicates, statistics and
//! Seestar / DWARF import over Wi-Fi or USB-C.

pub mod batch;
pub mod config;
pub mod db;
pub mod fits;
pub mod ingest;
pub mod logging;
pub mod names;
pub mod nick;
pub mod pictures;
pub mod progress;
pub mod sessions;
pub mod stats;
pub mod telescope;
pub mod util;
pub mod xisf;

#[cfg(feature = "gui")]
pub mod gui;
