//! Host stub of `quake`: same types and view-framing maths as the firmware;
//! the feed (P2P WebSocket/HTTP) is replaced by setters used by the scenarios.
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicUsize, AtomicBool, Ordering};

#[derive(Clone, Debug)]
pub struct Quake {
    pub time: String,
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    pub depth: i32,
    pub mag: f32,
    pub scale: i32,
    pub tsunami: String,
}

fn scale_str(scale: i32) -> &'static str {
    match scale {
        10 => "1", 20 => "2", 30 => "3", 40 => "4", 45 => "5-", 50 => "5+",
        55 => "6-", 60 => "6+", 70 => "7", _ => "?",
    }
}

impl Quake {
    pub fn intensity(&self) -> &'static str { scale_str(self.scale) }
}

#[derive(Clone, Debug)]
pub struct Eew {
    pub time: String,
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    pub depth: i32,
    pub mag: f32,
    pub max_scale: i32,
    pub areas: String,
}

impl Eew {
    pub fn intensity(&self) -> &'static str { scale_str(self.max_scale) }
}

static QUAKES: Mutex<Vec<Quake>> = Mutex::new(Vec::new());
static SEL: AtomicUsize = AtomicUsize::new(0);
static FOCUS: Mutex<Option<(f64, f64)>> = Mutex::new(None);
static LAST_OK_SECS: AtomicU32 = AtomicU32::new(0);
static ENABLED: AtomicBool = AtomicBool::new(true);
static NEWQ: Mutex<Option<Quake>> = Mutex::new(None);
static EEW: Mutex<Option<Eew>> = Mutex::new(None);
static VERSION: AtomicU32 = AtomicU32::new(0);

pub fn enabled() -> bool { ENABLED.load(Ordering::Relaxed) }
pub fn get() -> Vec<Quake> { QUAKES.lock().unwrap().clone() }
pub fn version() -> u32 { VERSION.load(Ordering::Relaxed) }
pub fn last_ok_secs() -> u32 { LAST_OK_SECS.load(Ordering::Relaxed) }
pub fn selection() -> usize { SEL.load(Ordering::Relaxed) }
pub fn focus() -> Option<(f64, f64)> { *FOCUS.lock().unwrap() }
pub fn new_quake() -> Option<Quake> { NEWQ.lock().unwrap().clone() }
pub fn newq_version() -> u32 { 0 }
pub fn eew() -> Option<Eew> { EEW.lock().unwrap().clone() }
pub fn eew_version() -> u32 { 0 }

pub fn distance_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = p2 - p1;
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    6371.0 * 2.0 * a.sqrt().asin()
}

pub fn own_position() -> Option<(f64, f64)> {
    let g = crate::gps_parser::get_data();
    if g.fix_quality > 0 && !g.lat_dd.is_nan() && !g.lon_dd.is_nan() { Some((g.lat_dd, g.lon_dd)) } else { None }
}

pub fn eew_view(e: &Eew) -> (f64, f64, u8, Option<f64>) { fit_view(e.lat, e.lon) }

pub fn fit_view(lat: f64, lon: f64) -> (f64, f64, u8, Option<f64>) {
    let own = own_position();
    match (own, lat.is_nan()) {
        (Some((ol, oo)), false) => {
            let (clat, clon) = ((ol + lat) / 2.0, (oo + lon) / 2.0);
            let z = crate::map_display::zoom_to_fit(
                clat, clon, ol.min(lat), ol.max(lat), oo.min(lon), oo.max(lon));
            (clat, clon, z.min(14), Some(distance_km(ol, oo, lat, lon)))
        }
        (None, false) => (lat, lon, 8, None),
        (Some((ol, oo)), true) => (ol, oo, 8, None),
        (None, true) => (f64::NAN, f64::NAN, 8, None),
    }
}

// ── host-only ────────────────────────────────────────────────────────────────

pub fn set_enabled(on: bool) { ENABLED.store(on, Ordering::Relaxed); }
pub fn set_quakes(q: Vec<Quake>, last_ok: u32) {
    *QUAKES.lock().unwrap() = q;
    LAST_OK_SECS.store(last_ok, Ordering::Relaxed);
}
pub fn set_selection(i: usize) { SEL.store(i, Ordering::Relaxed); }
pub fn set_focus(f: Option<(f64, f64)>) { *FOCUS.lock().unwrap() = f; }
pub fn set_new_quake(q: Option<Quake>) { *NEWQ.lock().unwrap() = q; }
pub fn set_eew(e: Option<Eew>) { *EEW.lock().unwrap() = e; }
