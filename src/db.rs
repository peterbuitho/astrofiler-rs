//! SQLite catalogue. The schema matches the tables the original Python
//! application creates (via peewee), so an existing `astrofiler.db` can be
//! opened directly and the two applications can share one database.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension, Row};
use std::path::Path;

const FITS_FILE_COLUMNS: &[(&str, &str)] = &[
    ("fitsFileName", "TEXT"),
    ("fitsFileDate", "DATE"),
    ("fitsFileCalibrated", "INTEGER"),
    ("fitsFileType", "TEXT"),
    ("fitsFileStacked", "INTEGER"),
    ("fitsFileObject", "TEXT"),
    ("fitsFileExpTime", "TEXT"),
    ("fitsFileXBinning", "TEXT"),
    ("fitsFileYBinning", "TEXT"),
    ("fitsFileCCDTemp", "TEXT"),
    ("fitsFileTelescop", "TEXT"),
    ("fitsFileInstrument", "TEXT"),
    ("fitsFileGain", "TEXT"),
    ("fitsFileOffset", "TEXT"),
    ("fitsFileFilter", "TEXT"),
    ("fitsFileObserver", "TEXT"),
    ("fitsFileNotes", "TEXT"),
    ("fitsFileHash", "TEXT"),
    ("fitsFileSession", "TEXT"),
    ("fitsFileCloudURL", "TEXT"),
    ("fitsFileSoftDelete", "INTEGER DEFAULT 0"),
    ("fitsFileCalibrationDate", "DATETIME"),
    ("fitsFileOriginalFile", "TEXT"),
    ("fitsFileOriginalCloudURL", "TEXT"),
    ("fitsFileAvgFWHMArcsec", "REAL"),
    ("fitsFileAvgEccentricity", "REAL"),
    ("fitsFileAvgHFRArcsec", "REAL"),
    ("fitsFileImageSNR", "REAL"),
    ("fitsFileStarCount", "INTEGER"),
    ("fitsFileImageScale", "REAL"),
];

const FITS_SESSION_COLUMNS: &[(&str, &str)] = &[
    ("fitsSessionObjectName", "TEXT"),
    ("fitsSessionDate", "DATE"),
    ("fitsSessionTelescope", "TEXT"),
    ("fitsSessionImager", "TEXT"),
    ("fitsSessionExposure", "TEXT"),
    ("fitsSessionBinningX", "TEXT"),
    ("fitsSessionBinningY", "TEXT"),
    ("fitsSessionCCDTemp", "TEXT"),
    ("fitsSessionGain", "TEXT"),
    ("fitsSessionOffset", "TEXT"),
    ("fitsSessionFilter", "TEXT"),
    ("fitsBiasSession", "TEXT"),
    ("fitsDarkSession", "TEXT"),
    ("fitsFlatSession", "TEXT"),
    ("is_auto_calibration", "INTEGER DEFAULT 0"),
    ("auto_calibration_dark_session_id", "TEXT"),
    ("auto_calibration_flat_session_id", "TEXT"),
    ("auto_calibration_bias_session_id", "TEXT"),
    ("fitsSessionAvgFWHMArcsec", "REAL"),
    ("fitsSessionAvgEccentricity", "REAL"),
    ("fitsSessionAvgHFRArcsec", "REAL"),
    ("fitsSessionImageSNR", "REAL"),
    ("fitsSessionStarCount", "INTEGER"),
    ("fitsSessionImageScale", "REAL"),
];

const MASTERS_COLUMNS: &[(&str, &str)] = &[
    ("master_id", "TEXT NOT NULL"),
    ("master_type", "TEXT NOT NULL"),
    ("master_path", "TEXT NOT NULL"),
    ("creation_date", "DATETIME NOT NULL"),
    ("telescope", "TEXT"),
    ("instrument", "TEXT"),
    ("exposure_time", "TEXT"),
    ("binning_x", "TEXT"),
    ("binning_y", "TEXT"),
    ("ccd_temp", "TEXT"),
    ("gain", "TEXT"),
    ("offset", "TEXT"),
    ("filter_name", "TEXT"),
    ("source_session_id", "TEXT"),
    ("file_count", "INTEGER NOT NULL DEFAULT 0"),
    ("quality_score", "REAL"),
    ("file_size", "INTEGER"),
    ("hash_value", "TEXT"),
    ("cloud_url", "TEXT"),
    ("is_validated", "INTEGER NOT NULL DEFAULT 0"),
    ("validation_date", "DATETIME"),
    ("notes", "TEXT"),
    ("soft_delete", "INTEGER NOT NULL DEFAULT 0"),
];

/// Migrations the Python app tracks with peewee-migrate. Recording them in a
/// fresh database stops the Python app from trying to re-create our tables.
const PY_MIGRATIONS: &[&str] = &[
    "001_initial_schema",
    "002_add_master_calibration_fields",
    "003_add_mapping_table",
    "004_remove_is_default_from_mapping",
    "005_recreate_mapping_table_without_is_default",
    "006_add_cloud_url_field",
    "007_add_calibration_fields",
    "007_create_masters_table",
    "008_add_session_autocal_fields",
    "008_remove_master_fields",
    "009_add_observer_notes_fields",
    "009_add_quality_metrics_fields",
    "010_add_masters_table",
    "010_add_session_quality_fields",
    "011_add_performance_indexes",
];

pub fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "wal")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    init_schema(&conn)?;
    Ok(conn)
}

fn create_table(conn: &Connection, table: &str, pk: &str, cols: &[(&str, &str)]) -> Result<()> {
    let body: Vec<String> = cols.iter().map(|(n, t)| format!("\"{n}\" {t}")).collect();
    conn.execute(
        &format!(
            "CREATE TABLE IF NOT EXISTS \"{table}\" ({pk}, {})",
            body.join(", ")
        ),
        [],
    )?;
    // Add any columns an older database is missing.
    let existing: Vec<String> = conn
        .prepare(&format!("PRAGMA table_info(\"{table}\")"))?
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<Result<_, _>>()?;
    for (name, ty) in cols {
        if !existing.iter().any(|e| e == name) {
            let ty = ty.replace(" NOT NULL", "");
            conn.execute(
                &format!("ALTER TABLE \"{table}\" ADD COLUMN \"{name}\" {ty}"),
                [],
            )?;
        }
    }
    Ok(())
}

fn init_schema(conn: &Connection) -> Result<()> {
    let fresh: bool = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='fitsFile'",
        [],
        |r| r.get::<_, i64>(0),
    )? == 0;
    create_table(
        conn,
        "fitsFile",
        "\"fitsFileId\" TEXT NOT NULL PRIMARY KEY",
        FITS_FILE_COLUMNS,
    )?;
    create_table(
        conn,
        "fitsSession",
        "\"fitsSessionId\" TEXT NOT NULL PRIMARY KEY",
        FITS_SESSION_COLUMNS,
    )?;
    create_table(
        conn,
        "Masters",
        "\"id\" INTEGER NOT NULL PRIMARY KEY",
        MASTERS_COLUMNS,
    )?;
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS "Mapping" ("id" INTEGER NOT NULL PRIMARY KEY, "card" VARCHAR(20) NOT NULL,
            "current" VARCHAR(255), "replace" VARCHAR(255));
        CREATE UNIQUE INDEX IF NOT EXISTS "masters_master_id" ON "Masters" ("master_id");
        CREATE INDEX IF NOT EXISTS "idx_af_file_hash" ON "fitsFile" ("fitsFileHash");
        CREATE INDEX IF NOT EXISTS "idx_af_file_session" ON "fitsFile" ("fitsFileSession");
        CREATE INDEX IF NOT EXISTS "idx_af_file_object" ON "fitsFile" ("fitsFileObject");
        CREATE INDEX IF NOT EXISTS "idx_af_session_object" ON "fitsSession" ("fitsSessionObjectName");
        CREATE TABLE IF NOT EXISTS "migratehistory" ("id" INTEGER NOT NULL PRIMARY KEY,
            "name" VARCHAR(255) NOT NULL, "migrated_at" DATETIME NOT NULL);
        "#,
    )?;
    if fresh {
        let now = now_str();
        for m in PY_MIGRATIONS {
            conn.execute(
                "INSERT INTO migratehistory (name, migrated_at) VALUES (?1, ?2)",
                params![m, now],
            )?;
        }
    }
    Ok(())
}

pub fn now_str() -> String {
    chrono::Local::now()
        .format("%Y-%m-%d %H:%M:%S%.6f")
        .to_string()
}

#[derive(Debug, Clone, Default)]
pub struct FitsFile {
    pub id: String,
    pub name: String,
    pub date: Option<String>,
    pub image_type: Option<String>,
    pub object: Option<String>,
    pub exptime: Option<String>,
    pub xbin: Option<String>,
    pub ybin: Option<String>,
    pub ccd_temp: Option<String>,
    pub telescope: Option<String>,
    pub instrument: Option<String>,
    pub gain: Option<String>,
    pub offset: Option<String>,
    pub filter: Option<String>,
    pub observer: Option<String>,
    pub notes: Option<String>,
    pub hash: Option<String>,
    pub session: Option<String>,
    pub calibrated: bool,
    pub soft_delete: bool,
    /// A stacked result (e.g. Seestar `Stacked_*.fit`) rather than a sub-frame.
    pub stacked: bool,
}

pub const FILE_SELECT: &str = "SELECT fitsFileId, fitsFileName, fitsFileDate, fitsFileType, fitsFileObject, \
    fitsFileExpTime, fitsFileXBinning, fitsFileYBinning, fitsFileCCDTemp, fitsFileTelescop, fitsFileInstrument, \
    fitsFileGain, fitsFileOffset, fitsFileFilter, fitsFileObserver, fitsFileNotes, fitsFileHash, fitsFileSession, \
    COALESCE(fitsFileCalibrated,0), COALESCE(fitsFileSoftDelete,0), COALESCE(fitsFileStacked,0) FROM fitsFile";

/// Read a column that the Python app may have stored as TEXT, INTEGER or REAL.
fn loose_text(row: &Row, idx: usize) -> rusqlite::Result<Option<String>> {
    use rusqlite::types::ValueRef;
    Ok(match row.get_ref(idx)? {
        ValueRef::Null => None,
        ValueRef::Integer(i) => Some(i.to_string()),
        ValueRef::Real(f) => Some(crate::fits::py_float_repr(f)),
        ValueRef::Text(t) | ValueRef::Blob(t) => Some(String::from_utf8_lossy(t).into_owned()),
    })
}

fn loose_bool(row: &Row, idx: usize) -> rusqlite::Result<bool> {
    Ok(loose_text(row, idx)?.is_some_and(|s| s == "1" || s.eq_ignore_ascii_case("true")))
}

impl FitsFile {
    pub fn from_row(r: &Row) -> rusqlite::Result<Self> {
        Ok(FitsFile {
            id: r.get(0)?,
            name: loose_text(r, 1)?.unwrap_or_default(),
            date: loose_text(r, 2)?,
            image_type: loose_text(r, 3)?,
            object: loose_text(r, 4)?,
            exptime: loose_text(r, 5)?,
            xbin: loose_text(r, 6)?,
            ybin: loose_text(r, 7)?,
            ccd_temp: loose_text(r, 8)?,
            telescope: loose_text(r, 9)?,
            instrument: loose_text(r, 10)?,
            gain: loose_text(r, 11)?,
            offset: loose_text(r, 12)?,
            filter: loose_text(r, 13)?,
            observer: loose_text(r, 14)?,
            notes: loose_text(r, 15)?,
            hash: loose_text(r, 16)?,
            session: loose_text(r, 17)?,
            calibrated: loose_bool(r, 18)?,
            soft_delete: loose_bool(r, 19)?,
            stacked: loose_bool(r, 20)?,
        })
    }

    pub fn insert(&self, conn: &Connection) -> Result<()> {
        conn.execute(
            "INSERT INTO fitsFile (fitsFileId, fitsFileName, fitsFileDate, fitsFileType, fitsFileObject, \
             fitsFileExpTime, fitsFileXBinning, fitsFileYBinning, fitsFileCCDTemp, fitsFileTelescop, \
             fitsFileInstrument, fitsFileGain, fitsFileOffset, fitsFileFilter, fitsFileObserver, fitsFileNotes, \
             fitsFileHash, fitsFileSession, fitsFileCalibrated, fitsFileSoftDelete, fitsFileStacked) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,0,?20)",
            params![
                self.id, self.name, self.date, self.image_type, self.object, self.exptime, self.xbin, self.ybin,
                self.ccd_temp, self.telescope, self.instrument, self.gain, self.offset, self.filter, self.observer,
                self.notes, self.hash, self.session, self.calibrated as i64, self.stacked as i64
            ],
        )?;
        Ok(())
    }

    pub fn file_name(&self) -> String {
        Path::new(&self.name)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

pub fn all_files(conn: &Connection, include_deleted: bool) -> Result<Vec<FitsFile>> {
    let sql = if include_deleted {
        format!("{FILE_SELECT} ORDER BY fitsFileObject, fitsFileDate")
    } else {
        format!("{FILE_SELECT} WHERE COALESCE(fitsFileSoftDelete,0)=0 ORDER BY fitsFileObject, fitsFileDate")
    };
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([], FitsFile::from_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn files_where(
    conn: &Connection,
    clause: &str,
    args: &[&dyn rusqlite::ToSql],
) -> Result<Vec<FitsFile>> {
    let mut stmt = conn.prepare(&format!("{FILE_SELECT} WHERE {clause}"))?;
    let rows = stmt
        .query_map(args, FitsFile::from_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn file_by_id(conn: &Connection, id: &str) -> Result<Option<FitsFile>> {
    Ok(conn
        .query_row(
            &format!("{FILE_SELECT} WHERE fitsFileId=?1"),
            [id],
            FitsFile::from_row,
        )
        .optional()?)
}

pub fn hash_exists(conn: &Connection, hash: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT fitsFileName FROM fitsFile WHERE fitsFileHash=?1 LIMIT 1",
            [hash],
            |r| r.get(0),
        )
        .optional()?)
}

#[derive(Debug, Clone, Default)]
pub struct Session {
    pub id: String,
    pub object: Option<String>,
    pub date: Option<String>,
    pub telescope: Option<String>,
    pub imager: Option<String>,
    pub exposure: Option<String>,
    pub xbin: Option<String>,
    pub ybin: Option<String>,
    pub ccd_temp: Option<String>,
    pub gain: Option<String>,
    pub offset: Option<String>,
    pub filter: Option<String>,
    pub file_count: i64,
}

impl Session {
    pub fn is_calibration(&self) -> bool {
        matches!(
            self.object.as_deref(),
            Some("Bias" | "Dark" | "Flat" | "FlatDark")
        )
    }

    pub fn insert(&self, conn: &Connection) -> Result<()> {
        conn.execute(
            "INSERT INTO fitsSession (fitsSessionId, fitsSessionObjectName, fitsSessionDate, fitsSessionTelescope, \
             fitsSessionImager, fitsSessionExposure, fitsSessionBinningX, fitsSessionBinningY, fitsSessionCCDTemp, \
             fitsSessionGain, fitsSessionOffset, fitsSessionFilter, is_auto_calibration) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,0)",
            params![
                self.id, self.object, self.date, self.telescope, self.imager, self.exposure, self.xbin, self.ybin,
                self.ccd_temp, self.gain, self.offset, self.filter
            ],
        )?;
        Ok(())
    }
}

pub fn all_sessions(conn: &Connection) -> Result<Vec<Session>> {
    let mut stmt = conn.prepare(
        "SELECT s.fitsSessionId, s.fitsSessionObjectName, s.fitsSessionDate, s.fitsSessionTelescope, \
         s.fitsSessionImager, s.fitsSessionExposure, s.fitsSessionBinningX, s.fitsSessionBinningY, \
         s.fitsSessionCCDTemp, s.fitsSessionGain, s.fitsSessionOffset, s.fitsSessionFilter, \
         (SELECT count(*) FROM fitsFile f WHERE f.fitsFileSession = s.fitsSessionId) \
         FROM fitsSession s ORDER BY s.fitsSessionDate DESC, s.fitsSessionObjectName",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(Session {
                id: r.get(0)?,
                object: loose_text(r, 1)?,
                date: loose_text(r, 2)?,
                telescope: loose_text(r, 3)?,
                imager: loose_text(r, 4)?,
                exposure: loose_text(r, 5)?,
                xbin: loose_text(r, 6)?,
                ybin: loose_text(r, 7)?,
                ccd_temp: loose_text(r, 8)?,
                gain: loose_text(r, 9)?,
                offset: loose_text(r, 10)?,
                filter: loose_text(r, 11)?,
                file_count: r.get(12)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[derive(Debug, Clone, Default)]
pub struct Master {
    pub id: i64,
    pub master_id: String,
    pub master_type: String,
    pub path: String,
    pub creation_date: String,
    pub telescope: Option<String>,
    pub instrument: Option<String>,
    pub exposure: Option<String>,
    pub xbin: Option<String>,
    pub ybin: Option<String>,
    pub ccd_temp: Option<String>,
    pub gain: Option<String>,
    pub offset: Option<String>,
    pub filter: Option<String>,
    pub source_session: Option<String>,
    pub file_count: i64,
    pub file_size: Option<i64>,
    pub hash: Option<String>,
    pub validated: bool,
    pub soft_delete: bool,
}

const MASTER_SELECT: &str = "SELECT id, master_id, master_type, master_path, creation_date, telescope, instrument, \
    exposure_time, binning_x, binning_y, ccd_temp, gain, offset, filter_name, source_session_id, file_count, \
    file_size, hash_value, is_validated, soft_delete FROM Masters";

impl Master {
    fn from_row(r: &Row) -> rusqlite::Result<Self> {
        Ok(Master {
            id: r.get(0)?,
            master_id: loose_text(r, 1)?.unwrap_or_default(),
            master_type: loose_text(r, 2)?.unwrap_or_default(),
            path: loose_text(r, 3)?.unwrap_or_default(),
            creation_date: loose_text(r, 4)?.unwrap_or_default(),
            telescope: loose_text(r, 5)?,
            instrument: loose_text(r, 6)?,
            exposure: loose_text(r, 7)?,
            xbin: loose_text(r, 8)?,
            ybin: loose_text(r, 9)?,
            ccd_temp: loose_text(r, 10)?,
            gain: loose_text(r, 11)?,
            offset: loose_text(r, 12)?,
            filter: loose_text(r, 13)?,
            source_session: loose_text(r, 14)?,
            file_count: r.get::<_, Option<i64>>(15)?.unwrap_or(0),
            file_size: r.get(16)?,
            hash: loose_text(r, 17)?,
            validated: loose_bool(r, 18)?,
            soft_delete: loose_bool(r, 19)?,
        })
    }

    pub fn insert(&self, conn: &Connection) -> Result<()> {
        conn.execute(
            "INSERT INTO Masters (master_id, master_type, master_path, creation_date, telescope, instrument, \
             exposure_time, binning_x, binning_y, ccd_temp, gain, offset, filter_name, source_session_id, \
             file_count, file_size, hash_value, is_validated, validation_date, soft_delete) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,0)",
            params![
                self.master_id, self.master_type, self.path, self.creation_date, self.telescope, self.instrument,
                self.exposure, self.xbin, self.ybin, self.ccd_temp, self.gain, self.offset, self.filter,
                self.source_session, self.file_count, self.file_size, self.hash, self.validated as i64,
                if self.validated { Some(now_str()) } else { None }
            ],
        )?;
        Ok(())
    }
}

pub fn masters(conn: &Connection, include_deleted: bool) -> Result<Vec<Master>> {
    let sql = if include_deleted {
        format!("{MASTER_SELECT} ORDER BY creation_date DESC")
    } else {
        format!("{MASTER_SELECT} WHERE COALESCE(soft_delete,0)=0 ORDER BY creation_date DESC")
    };
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([], Master::from_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[derive(Debug, Clone, Default)]
pub struct Mapping {
    pub id: i64,
    pub card: String,
    pub current: Option<String>,
    pub replace: Option<String>,
}

pub fn mappings(conn: &Connection) -> Result<Vec<Mapping>> {
    let mut stmt =
        conn.prepare("SELECT id, card, current, replace FROM Mapping ORDER BY card, current")?;
    let rows = stmt
        .query_map([], |r| {
            Ok(Mapping {
                id: r.get(0)?,
                card: r.get(1)?,
                current: r.get(2)?,
                replace: r.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn add_mapping(conn: &Connection, card: &str, current: &str, replace: &str) -> Result<()> {
    let card = card.trim().to_uppercase();
    let current = current.trim();
    let updated = conn.execute(
        "UPDATE Mapping SET replace=?3 WHERE card=?1 AND COALESCE(current,'')=?2",
        params![card, current, replace],
    )?;
    if updated == 0 {
        conn.execute(
            "INSERT INTO Mapping (card, current, replace) VALUES (?1, ?2, ?3)",
            params![
                card,
                if current.is_empty() {
                    None
                } else {
                    Some(current)
                },
                replace
            ],
        )?;
    }
    Ok(())
}

pub fn remove_mapping(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM Mapping WHERE id=?1", [id])?;
    Ok(())
}
