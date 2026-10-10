// Track log browser / playback state — drives the TRACKLIST page and the
// MAP page's track-replay overlay.
//
// Flow: TRACKLIST page lists saved dates (`refresh_file_list` / `list_move`);
// the MAP-mode button loads the highlighted file and starts playback at
// point 0 (`select_and_play`); ZoomUp/Down then step through points
// (`step`) instead of zooming; the Page button ends playback (`stop`).
//
// SPDX-License-Identifier: MIT

use std::sync::{LazyLock, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::track_log::TrackPoint;

/// Cached list of available track dates, refreshed on entering the list page.
static FILE_LIST: LazyLock<Mutex<Vec<String>>> = LazyLock::new(|| Mutex::new(Vec::new()));
/// Highlighted row in the track-list page.
static LIST_SEL: AtomicUsize = AtomicUsize::new(0);

/// Points of the currently-loaded track (empty when nothing loaded).
static LOADED: LazyLock<Mutex<Vec<TrackPoint>>> = LazyLock::new(|| Mutex::new(Vec::new()));
/// Date string of the currently-loaded track.
static LOADED_NAME: LazyLock<Mutex<String>> = LazyLock::new(|| Mutex::new(String::new()));
/// Index into LOADED currently being displayed.
static POINT_IDX: AtomicUsize = AtomicUsize::new(0);
/// true while a loaded track is being replayed on the MAP page.
static PLAYING: AtomicBool = AtomicBool::new(false);

/// Deadline until which a freshly-selected track is shown zoomed out to fit
/// its whole route. `None` once that's elapsed (or playback never started).
static OVERVIEW_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);
/// Zoom level in use before playback started, restored when the overview ends.
static PRE_PLAYBACK_ZOOM: AtomicU8 = AtomicU8::new(15);
/// How long the whole-route overview stays on screen after selecting a track.
const OVERVIEW: Duration = Duration::from_secs(5);

/// Re-scan /emmc/tracks and refresh the cached file list (newest date
/// first). Call whenever the track-list page becomes visible.
pub fn refresh_file_list() {
    let mut files = crate::track_log::list_files();
    files.sort_by(|a, b| b.cmp(a));
    let len = files.len();
    *FILE_LIST.lock().unwrap() = files;
    if LIST_SEL.load(Ordering::Relaxed) >= len {
        LIST_SEL.store(len.saturating_sub(1), Ordering::Relaxed);
    }
}

/// Snapshot of the cached file list.
pub fn file_list() -> Vec<String> {
    FILE_LIST.lock().unwrap().clone()
}

pub fn list_selection() -> usize { LIST_SEL.load(Ordering::Relaxed) }

/// Move the list highlight by `delta` rows, clamped to the list bounds.
pub fn list_move(delta: i32) {
    let len = FILE_LIST.lock().unwrap().len();
    if len == 0 { return; }
    let cur = LIST_SEL.load(Ordering::Relaxed) as i32;
    LIST_SEL.store((cur + delta).clamp(0, len as i32 - 1) as usize, Ordering::Relaxed);
}

/// Load the highlighted file and start playback at point 0.
/// Returns false if the list is empty or the file has no usable points.
pub fn select_and_play() -> bool {
    let name = {
        let files = FILE_LIST.lock().unwrap();
        match files.get(LIST_SEL.load(Ordering::Relaxed)) {
            Some(n) => n.clone(),
            None => return false,
        }
    };
    let Some(points) = crate::track_log::load_file(&name) else { return false };
    if points.is_empty() { return false; }

    // Zoom out far enough that the whole route is visible, as an overview of
    // what's about to be replayed. `tick_overview` puts the zoom back after
    // OVERVIEW elapses; stepping to later points then keeps that scale.
    let (mut lat_min, mut lat_max) = (points[0].lat, points[0].lat);
    let (mut lon_min, mut lon_max) = (points[0].lon, points[0].lon);
    for p in &points {
        if p.lat < lat_min { lat_min = p.lat; }
        if p.lat > lat_max { lat_max = p.lat; }
        if p.lon < lon_min { lon_min = p.lon; }
        if p.lon > lon_max { lon_max = p.lon; }
    }
    let fit_zoom = crate::map_display::zoom_to_fit(
        points[0].lat, points[0].lon, lat_min, lat_max, lon_min, lon_max,
    );

    *LOADED.lock().unwrap() = points;
    *LOADED_NAME.lock().unwrap() = name;
    POINT_IDX.store(0, Ordering::Relaxed);
    PLAYING.store(true, Ordering::SeqCst);

    PRE_PLAYBACK_ZOOM.store(crate::map_display::get_zoom(), Ordering::Relaxed);
    *OVERVIEW_UNTIL.lock().unwrap() = Some(Instant::now() + OVERVIEW);
    crate::map_display::set_zoom(fit_zoom);
    true
}

/// End the initial whole-route overview once its time is up, restoring the
/// zoom that was in use before playback started. Called every display tick.
pub fn tick_overview() {
    let mut until = OVERVIEW_UNTIL.lock().unwrap();
    let Some(deadline) = *until else { return };
    if Instant::now() < deadline { return; }
    *until = None;
    drop(until);
    crate::map_display::set_zoom(PRE_PLAYBACK_ZOOM.load(Ordering::Relaxed));
}

/// true while a track is actively being replayed on the MAP page.
pub fn is_playing() -> bool { PLAYING.load(Ordering::Relaxed) }

/// Stop playback and return the MAP page to live GPS tracking.
pub fn stop() {
    PLAYING.store(false, Ordering::SeqCst);
    // If playback ended while the overview was still up, the zoom is still
    // the fitted one — put it back rather than leaving the live map there.
    if OVERVIEW_UNTIL.lock().unwrap().take().is_some() {
        crate::map_display::set_zoom(PRE_PLAYBACK_ZOOM.load(Ordering::Relaxed));
    }
}

pub fn point_count() -> usize { LOADED.lock().unwrap().len() }
pub fn point_index() -> usize { POINT_IDX.load(Ordering::Relaxed) }
pub fn loaded_name() -> String { LOADED_NAME.lock().unwrap().clone() }

/// Move the playback cursor by `delta` points, clamped to the track bounds.
pub fn step(delta: i32) {
    let len = LOADED.lock().unwrap().len();
    if len == 0 { return; }
    let cur = POINT_IDX.load(Ordering::Relaxed) as i32;
    POINT_IDX.store((cur + delta).clamp(0, len as i32 - 1) as usize, Ordering::Relaxed);
}

/// Clone of the point currently being displayed, if any.
pub fn current_point() -> Option<TrackPoint> {
    LOADED.lock().unwrap().get(POINT_IDX.load(Ordering::Relaxed)).cloned()
}

/// (lat, lon) pairs from point 0 up to and including the current index —
/// the part of the route already replayed. Sampled down to at most `max`
/// points: a day of logging at one point per 5 s runs to thousands, far more
/// than a 320 px-wide map can resolve, and drawing every segment over SPI is
/// slow enough to stall the redraw.
pub fn trail_up_to_current(max: usize) -> Vec<(f32, f32)> {
    let idx = POINT_IDX.load(Ordering::Relaxed);
    let pts = LOADED.lock().unwrap();
    let end = (idx + 1).min(pts.len());
    sample(&pts[..end], max)
}

/// (lat, lon) pairs spanning the whole loaded route, for drawing the full
/// track outline. Sampled the same way as `trail_up_to_current`.
pub fn route_outline(max: usize) -> Vec<(f32, f32)> {
    sample(&LOADED.lock().unwrap(), max)
}

/// (lat, lon) of the final point of the loaded track.
pub fn end_point() -> Option<(f64, f64)> {
    LOADED.lock().unwrap().last().map(|p| (p.lat, p.lon))
}

/// Take at most `max` evenly-spaced points from `pts`, always keeping the
/// first and last so the drawn line's endpoints stay exact.
fn sample(pts: &[TrackPoint], max: usize) -> Vec<(f32, f32)> {
    let n = pts.len();
    if n == 0 || max == 0 { return Vec::new(); }
    if n <= max || max == 1 {
        return pts.iter().map(|p| (p.lat as f32, p.lon as f32)).collect();
    }
    let mut out = Vec::with_capacity(max);
    for i in 0..max - 1 {
        let j = i * (n - 1) / (max - 1);
        out.push((pts[j].lat as f32, pts[j].lon as f32));
    }
    out.push((pts[n - 1].lat as f32, pts[n - 1].lon as f32));
    out
}
