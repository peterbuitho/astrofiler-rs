//! Session grouping (port of `SessionProcessor`). Light frames are grouped by
//! object, night and filter; calibration frames by the settings that must match
//! for them to be stacked into a master.

use crate::db::{self, FitsFile, Session};
use crate::progress::Progress;
use crate::util::exposures_match;
use anyhow::Result;
use rusqlite::{params, Connection};

fn day(f: &FitsFile) -> String {
    f.date
        .as_deref()
        .map(|d| d.chars().take(10).collect())
        .unwrap_or_default()
}

fn exp_num(f: &FitsFile) -> f64 {
    f.exptime
        .as_deref()
        .and_then(|e| e.trim().parse().ok())
        .unwrap_or(0.0)
}

fn cal_key(
    f: &FitsFile,
) -> (
    Option<String>,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
) {
    (
        f.telescope.clone(),
        f.instrument.clone(),
        day(f),
        f.xbin.clone(),
        f.ybin.clone(),
    )
}

fn by_key_exposure_date(a: &FitsFile, b: &FitsFile) -> std::cmp::Ordering {
    cal_key(a)
        .cmp(&cal_key(b))
        .then(exp_num(a).total_cmp(&exp_num(b)))
        .then(a.date.cmp(&b.date))
}

fn session_from(f: &FitsFile, object: &str) -> Session {
    Session {
        id: uuid::Uuid::new_v4().to_string(),
        object: Some(object.to_string()),
        date: Some(day(f)).filter(|d| !d.is_empty()),
        telescope: f.telescope.clone(),
        imager: f.instrument.clone(),
        exposure: f.exptime.clone(),
        xbin: f.xbin.clone(),
        ybin: f.ybin.clone(),
        ccd_temp: f.ccd_temp.clone(),
        gain: f.gain.clone(),
        offset: f.offset.clone(),
        filter: f.filter.clone(),
        file_count: 0,
    }
}

/// Group already-sorted files: a new session starts whenever `same` says the
/// file doesn't belong with the previous one.
fn group(
    tx: &Connection,
    files: &[FitsFile],
    object_of: impl Fn(&FitsFile) -> String,
    same: impl Fn(&FitsFile, &FitsFile) -> bool,
) -> Result<usize> {
    let mut created = 0;
    let mut current: Option<(usize, String)> = None;
    let mut update = tx.prepare("UPDATE fitsFile SET fitsFileSession=?1 WHERE fitsFileId=?2")?;
    for (i, f) in files.iter().enumerate() {
        let start_new = match &current {
            None => true,
            Some((j, _)) => !same(&files[*j], f),
        };
        if start_new {
            let s = session_from(f, &object_of(f));
            s.insert(tx)?;
            created += 1;
            current = Some((i, s.id));
        }
        update.execute(params![current.as_ref().unwrap().1, f.id])?;
    }
    Ok(created)
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SessionReport {
    pub light: usize,
    pub bias: usize,
    pub dark: usize,
    pub flat: usize,
    pub flat_dark: usize,
}

impl SessionReport {
    pub fn total(&self) -> usize {
        self.light + self.bias + self.dark + self.flat + self.flat_dark
    }
}

/// Create sessions for every file not yet assigned to one.
pub fn create_all(conn: &mut Connection, progress: &dyn Progress) -> Result<SessionReport> {
    let unassigned = "fitsFileSession IS NULL AND COALESCE(fitsFileSoftDelete,0)=0 AND COALESCE(fitsFileStacked,0)=0";
    let tx = conn.transaction()?;
    let mut r = SessionReport::default();

    progress.update(0, 5, "Grouping light frames...");
    let mut lights = db::files_where(
        &tx,
        &format!("{unassigned} AND fitsFileType LIKE '%LIGHT%'"),
        &[],
    )?;
    lights.sort_by(|a, b| {
        (&a.object, day(a), &a.filter, &a.date).cmp(&(&b.object, day(b), &b.filter, &b.date))
    });
    r.light = group(
        &tx,
        &lights,
        |f| f.object.clone().unwrap_or_default(),
        |a, b| a.object == b.object && day(a) == day(b) && a.filter == b.filter,
    )?;

    let key = cal_key;
    let same_base = |a: &FitsFile, b: &FitsFile| key(a) == key(b);

    progress.update(1, 5, "Grouping bias frames...");
    let mut biases = db::files_where(
        &tx,
        &format!("{unassigned} AND fitsFileType LIKE '%BIAS%'"),
        &[],
    )?;
    biases.sort_by(|a, b| (key(a), &a.date).cmp(&(key(b), &b.date)));
    r.bias = group(&tx, &biases, |_| "Bias".into(), same_base)?;

    progress.update(2, 5, "Grouping dark frames...");
    let mut darks = db::files_where(
        &tx,
        &format!("{unassigned} AND fitsFileType LIKE '%DARK%' AND fitsFileType NOT LIKE '%FLAT%'"),
        &[],
    )?;
    darks.sort_by(by_key_exposure_date);
    r.dark = group(
        &tx,
        &darks,
        |_| "Dark".into(),
        |a, b| same_base(a, b) && a.exptime == b.exptime,
    )?;

    progress.update(3, 5, "Grouping flat frames...");
    let mut flats = db::files_where(
        &tx,
        &format!("{unassigned} AND fitsFileType LIKE '%FLAT%' AND fitsFileType NOT LIKE '%DARK%'"),
        &[],
    )?;
    flats.sort_by(|a, b| (key(a), &a.filter, &a.date).cmp(&(key(b), &b.filter, &b.date)));
    r.flat = group(
        &tx,
        &flats,
        |_| "Flat".into(),
        |a, b| same_base(a, b) && a.filter == b.filter,
    )?;

    progress.update(4, 5, "Grouping flat-dark frames...");
    let mut fdarks = db::files_where(
        &tx,
        &format!("{unassigned} AND fitsFileType LIKE '%FLAT%' AND fitsFileType LIKE '%DARK%'"),
        &[],
    )?;
    fdarks.sort_by(by_key_exposure_date);
    r.flat_dark = group(
        &tx,
        &fdarks,
        |_| "FlatDark".into(),
        |a, b| same_base(a, b) && exposures_match(a.exptime.as_deref(), b.exptime.as_deref()),
    )?;

    tx.commit()?;
    progress.update(5, 5, "Done");
    log::info!(
        "Created {} sessions ({} light, {} bias, {} dark, {} flat, {} flat-dark)",
        r.total(),
        r.light,
        r.bias,
        r.dark,
        r.flat,
        r.flat_dark
    );
    Ok(r)
}

/// Remove all sessions and unassign their files.
pub fn clear_all(conn: &mut Connection) -> Result<usize> {
    let tx = conn.transaction()?;
    tx.execute("UPDATE fitsFile SET fitsFileSession=NULL", [])?;
    let n = tx.execute("DELETE FROM fitsSession", [])?;
    tx.commit()?;
    Ok(n)
}

/// Remove one session and unassign its files.
pub fn delete(conn: &Connection, session_id: &str) -> Result<()> {
    conn.execute(
        "UPDATE fitsFile SET fitsFileSession=NULL WHERE fitsFileSession=?1",
        [session_id],
    )?;
    conn.execute(
        "DELETE FROM fitsSession WHERE fitsSessionId=?1",
        [session_id],
    )?;
    Ok(())
}

pub fn session_files(conn: &Connection, session_id: &str) -> Result<Vec<FitsFile>> {
    db::files_where(
        conn,
        "fitsFileSession=?1 ORDER BY fitsFileDate",
        &[&session_id],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::ingest::tests::make_frame;

    #[test]
    fn groups_by_object_night_filter() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("in");
        std::fs::create_dir_all(&src).unwrap();
        // Interleaved filters on one night must still give one session per filter.
        make_frame(
            &src,
            "1.fits",
            "Light",
            Some("M31"),
            "2024-10-01T21:00:00",
            60.0,
            Some("R"),
            1.0,
        );
        make_frame(
            &src,
            "2.fits",
            "Light",
            Some("M31"),
            "2024-10-01T21:01:00",
            60.0,
            Some("G"),
            2.0,
        );
        make_frame(
            &src,
            "3.fits",
            "Light",
            Some("M31"),
            "2024-10-01T21:02:00",
            60.0,
            Some("R"),
            3.0,
        );
        make_frame(
            &src,
            "4.fits",
            "Light",
            Some("M31"),
            "2024-10-02T21:02:00",
            60.0,
            Some("R"),
            4.0,
        );
        make_frame(
            &src,
            "5.fits",
            "Flat",
            None,
            "2024-10-01T08:00:00",
            1.0,
            Some("R"),
            5.0,
        );
        make_frame(
            &src,
            "6.fits",
            "Flat",
            None,
            "2024-10-01T08:01:00",
            1.0,
            Some("R"),
            6.0,
        );
        let cfg = Config {
            repo: tmp.path().join("repo"),
            ..Default::default()
        };
        let mut conn = db::open(&tmp.path().join("t.db")).unwrap();
        crate::ingest::ingest_folder(
            &mut conn,
            &cfg,
            &src,
            crate::ingest::IngestOptions::MOVE,
            &crate::progress::NoProgress,
        )
        .unwrap();
        let r = create_all(&mut conn, &crate::progress::NoProgress).unwrap();
        assert_eq!(r.light, 3);
        assert_eq!(r.flat, 1);
        let again = create_all(&mut conn, &crate::progress::NoProgress).unwrap();
        assert_eq!(again.total(), 0);
    }
}
