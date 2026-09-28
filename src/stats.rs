//! Repository statistics (the original's Stats page).

use crate::db;
use anyhow::Result;
use rayon::prelude::*;
use rusqlite::Connection;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub total_files: usize,
    pub total_bytes: u64,
    pub light_files: usize,
    pub calibration_files: usize,
    pub sessions: usize,
    pub first_date: Option<String>,
    pub last_date: Option<String>,
    /// object -> (frames, integration seconds)
    pub by_object: Vec<(String, usize, f64)>,
    /// filter -> integration seconds (lights only)
    pub by_filter: Vec<(String, f64)>,
    pub by_telescope: Vec<(String, usize)>,
    pub by_instrument: Vec<(String, usize)>,
    pub by_type: Vec<(String, usize)>,
}

fn sorted<V: PartialOrd + Copy>(m: BTreeMap<String, V>) -> Vec<(String, V)> {
    let mut v: Vec<_> = m.into_iter().collect();
    v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    v
}

pub fn compute(conn: &Connection) -> Result<Stats> {
    let files = db::all_files(conn, false)?;
    let mut s = Stats {
        total_files: files.len(),
        ..Default::default()
    };
    s.total_bytes = files
        .par_iter()
        .filter_map(|f| std::fs::metadata(Path::new(&f.name)).ok())
        .map(|m| m.len())
        .sum();
    let mut obj: BTreeMap<String, (usize, f64)> = BTreeMap::new();
    let mut filt: BTreeMap<String, f64> = BTreeMap::new();
    let mut tel: BTreeMap<String, usize> = BTreeMap::new();
    let mut ins: BTreeMap<String, usize> = BTreeMap::new();
    let mut typ: BTreeMap<String, usize> = BTreeMap::new();
    for f in &files {
        let t = f.image_type.clone().unwrap_or_default();
        let is_light = t.contains("LIGHT");
        let exp: f64 = f
            .exptime
            .as_deref()
            .and_then(|e| e.trim().parse().ok())
            .unwrap_or(0.0);
        if is_light {
            s.light_files += 1;
            let e = obj
                .entry(f.object.clone().unwrap_or_else(|| "Unknown".into()))
                .or_default();
            e.0 += 1;
            e.1 += exp;
            *filt
                .entry(f.filter.clone().unwrap_or_else(|| "OSC".into()))
                .or_default() += exp;
        } else {
            s.calibration_files += 1;
        }
        *tel.entry(f.telescope.clone().unwrap_or_else(|| "Unknown".into()))
            .or_default() += 1;
        *ins.entry(f.instrument.clone().unwrap_or_else(|| "Unknown".into()))
            .or_default() += 1;
        *typ.entry(t).or_default() += 1;
        if let Some(d) = f
            .date
            .as_deref()
            .map(|d| d.chars().take(10).collect::<String>())
        {
            if s.first_date.as_ref().is_none_or(|x| &d < x) {
                s.first_date = Some(d.clone());
            }
            if s.last_date.as_ref().is_none_or(|x| &d > x) {
                s.last_date = Some(d);
            }
        }
    }
    let mut by_object: Vec<_> = obj.into_iter().map(|(k, (n, e))| (k, n, e)).collect();
    by_object.sort_by(|a, b| b.2.total_cmp(&a.2));
    s.by_object = by_object;
    s.by_filter = sorted(filt);
    s.by_telescope = sorted(tel);
    s.by_instrument = sorted(ins);
    s.by_type = sorted(typ);
    s.sessions = conn.query_row("SELECT count(*) FROM fitsSession", [], |r| {
        r.get::<_, i64>(0)
    })? as usize;
    Ok(s)
}

pub fn hours(seconds: f64) -> String {
    format!("{:.2} h", seconds / 3600.0)
}
