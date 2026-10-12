//! Host stub of `gps_parser`: the same public data types, state set by the scenarios.
use std::sync::{LazyLock, Mutex};

#[derive(Clone, Debug, Default)]
pub struct SatInfo {
    pub system: char,
    pub prn: u8,
    pub elevation: i16,
    pub azimuth: u16,
    pub snr: u8,
    pub in_use: bool,
}

#[derive(Clone, Debug)]
pub struct GpsData {
    pub time_str: String,
    pub date_str: String,
    pub lat_str: String,
    pub lon_str: String,
    pub lat_dd: f64,
    pub lon_dd: f64,
    pub fix_quality: u8,
    pub fix_mode: u8,
    pub num_sats: u8,
    pub hdop: f32,
    pub pdop: f32,
    pub vdop: f32,
    pub altitude: f32,
    pub speed_kn: f32,
    pub course: f32,
    pub sats: Vec<SatInfo>,
    pub valid: bool,
}

impl Default for GpsData {
    fn default() -> Self {
        GpsData {
            time_str: "--:--:--".into(), date_str: "----/--/--".into(),
            lat_str: "---".into(), lon_str: "---".into(),
            lat_dd: f64::NAN, lon_dd: f64::NAN,
            fix_quality: 0, fix_mode: 1, num_sats: 0,
            hdop: 0.0, pdop: 0.0, vdop: 0.0,
            altitude: 0.0, speed_kn: 0.0, course: 0.0,
            sats: Vec::new(), valid: false,
        }
    }
}

static GPS_DATA: LazyLock<Mutex<GpsData>> = LazyLock::new(|| Mutex::new(GpsData::default()));
static LOCATION_NAME: Mutex<String> = Mutex::new(String::new());

pub fn get_data() -> GpsData { GPS_DATA.lock().unwrap().clone() }
pub fn set_data(d: GpsData) { *GPS_DATA.lock().unwrap() = d; }
pub fn get_location_name() -> String { LOCATION_NAME.lock().unwrap().clone() }
pub fn set_location_name(name: &str) { *LOCATION_NAME.lock().unwrap() = name.to_string(); }
