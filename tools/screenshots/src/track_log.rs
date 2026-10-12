//! Host stub of `track_log`: an in-memory set of "saved" tracks.
use std::sync::{LazyLock, Mutex};

#[derive(Clone)]
pub struct TrackPoint {
    pub time_utc: String,
    pub lat: f64,
    pub lon: f64,
    pub alt_m: f32,
    pub speed_kn: f32,
}

static FILES: LazyLock<Mutex<Vec<(String, Vec<TrackPoint>)>>> = LazyLock::new(|| Mutex::new(Vec::new()));

pub fn add_file(name: &str, pts: Vec<TrackPoint>) { FILES.lock().unwrap().push((name.to_string(), pts)); }
pub fn list_files() -> Vec<String> { FILES.lock().unwrap().iter().map(|f| f.0.clone()).collect() }
pub fn load_file(name: &str) -> Option<Vec<TrackPoint>> {
    FILES.lock().unwrap().iter().find(|f| f.0 == name).map(|f| f.1.clone())
}
