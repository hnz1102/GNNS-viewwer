// GPS trail / track log — appends fix records to a daily CSV file on eMMC.
//
// Call `trigger(&gps)` on every valid GPS fix (mirrors geocoder::trigger).
// Rate-limited to one row per MIN_LOG_INTERVAL_SECS to bound eMMC write wear
// and file growth. Each UTC day gets its own file under /emmc/tracks/; the
// header row is (re-)written whenever that day's file doesn't exist yet,
// which also naturally handles the midnight rollover to a new file.
//
// SPDX-License-Identifier: MIT

use log::*;
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use chrono::{DateTime, Utc};

use crate::gps_parser::GpsData;

const TRACK_DIR: &str = "/emmc/tracks";
const MIN_LOG_INTERVAL_SECS: u32 = 5;

static LAST_LOG_SECS: AtomicU32 = AtomicU32::new(0);

/// Log one GPS fix if eMMC is mounted, the fix is valid, and at least
/// `MIN_LOG_INTERVAL_SECS` have passed since the last logged row.
pub fn trigger(gps: &GpsData) {
    if !crate::emmc::is_mounted() { return; }
    if gps.fix_quality == 0 || gps.lat_dd.is_nan() || gps.lon_dd.is_nan() { return; }

    let now = SystemTime::now().duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as u32).unwrap_or(0);
    if now.saturating_sub(LAST_LOG_SECS.load(Ordering::Relaxed)) < MIN_LOG_INTERVAL_SECS {
        return;
    }
    LAST_LOG_SECS.store(now, Ordering::Relaxed);

    if std::fs::create_dir_all(TRACK_DIR).is_err() { return; }

    let dt: DateTime<Utc> = SystemTime::now().into();
    let path = format!("{}/{}.csv", TRACK_DIR, dt.format("%Y-%m-%d"));
    let is_new_file = !std::path::Path::new(&path).exists();

    let mut file = match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(f) => f,
        Err(e) => {
            warn!("[TRACK] open failed ({}): {:?}", path, e);
            return;
        }
    };

    if is_new_file {
        let _ = writeln!(file, "time_utc,lat,lon,alt_m,speed_kn,course_deg,fix_quality,num_sats,hdop");
    }

    let _ = writeln!(
        file,
        "{},{:.6},{:.6},{:.1},{:.1},{:.1},{},{},{:.1}",
        dt.format("%Y-%m-%dT%H:%M:%SZ"),
        gps.lat_dd, gps.lon_dd, gps.altitude, gps.speed_kn, gps.course,
        gps.fix_quality, gps.num_sats, gps.hdop,
    );
}

/// Delete every track log file under /emmc/tracks/.
pub fn clear() {
    let Ok(entries) = std::fs::read_dir(TRACK_DIR) else { return };
    let mut removed = 0u32;
    for entry in entries.flatten() {
        if std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    info!("[TRACK] cleared {} log file(s)", removed);
}

// ─── Track browsing / playback support ─────────────────────────────────────

/// One parsed row from a track CSV file.
#[derive(Clone)]
pub struct TrackPoint {
    /// "%Y-%m-%dT%H:%M:%SZ" as logged.
    pub time_utc: String,
    pub lat: f64,
    pub lon: f64,
    pub alt_m: f32,
    pub speed_kn: f32,
}

/// Path of the log file for `date`, or `None` unless `date` is exactly
/// `YYYY-MM-DD`. Anything that reaches the filesystem from outside (a URL
/// query parameter) must go through this so it can't name another path.
pub fn path_for(date: &str) -> Option<String> {
    let b = date.as_bytes();
    let ok = b.len() == 10
        && b.iter().enumerate().all(|(i, &c)| {
            if i == 4 || i == 7 { c == b'-' } else { c.is_ascii_digit() }
        });
    ok.then(|| format!("{}/{}.csv", TRACK_DIR, date))
}

/// List the dates (filenames without ".csv") of every saved track log,
/// unsorted. Empty if eMMC isn't mounted or nothing has been logged yet.
pub fn list_files() -> Vec<String> {
    if !crate::emmc::is_mounted() { return Vec::new(); }
    let Ok(entries) = std::fs::read_dir(TRACK_DIR) else { return Vec::new(); };
    entries.flatten()
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("csv"))
        .filter_map(|e| e.path().file_stem().and_then(|s| s.to_str()).map(str::to_string))
        .collect()
}

/// Load and parse a track log file by date (e.g. "2026-09-25").
/// Returns `None` if the file can't be read; malformed rows are skipped.
pub fn load_file(date: &str) -> Option<Vec<TrackPoint>> {
    let path = format!("{}/{}.csv", TRACK_DIR, date);
    let content = std::fs::read_to_string(&path).ok()?;
    let mut points = Vec::new();
    for line in content.lines().skip(1) {
        let cols: Vec<&str> = line.split(',').collect();
        if cols.len() < 5 { continue; }
        let (Ok(lat), Ok(lon)) = (cols[1].parse::<f64>(), cols[2].parse::<f64>()) else { continue };
        points.push(TrackPoint {
            time_utc: cols[0].to_string(),
            lat,
            lon,
            alt_m: cols[3].parse().unwrap_or(0.0),
            speed_kn: cols[4].parse().unwrap_or(0.0),
        });
    }
    Some(points)
}
