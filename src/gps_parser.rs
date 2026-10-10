// NMEA 0183 GPS/GNSS parser for u-blox MAX-F10S
// Parses sentences received on COM1 and maintains shared GPS state.
// SPDX-License-Identifier: MIT

use std::sync::{LazyLock, Mutex};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use chrono::{DateTime, Utc};
use esp_idf_svc::sys::{settimeofday, timeval};

// ─── Public types ─────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default)]
pub struct SatInfo {
    pub system: char,   // 'G'=GPS, 'A'=Galileo, 'B'=BeiDou, 'L'=GLONASS, 'Q'=QZSS
    pub prn: u8,
    pub elevation: i16, // degrees (0-90)
    pub azimuth: u16,   // degrees (0-359)
    pub snr: u8,        // dBHz (0 = not tracking)
    pub in_use: bool,
}

#[derive(Clone, Debug)]
pub struct GpsData {
    pub time_str: String,   // "HH:MM:SS" UTC
    pub date_str: String,   // "YYYY/MM/DD"
    pub lat_str: String,    // "35d44.5738N"
    pub lon_str: String,    // "139d32.3299E"
    pub lat_dd: f64,        // decimal degrees (NaN = no fix)
    pub lon_dd: f64,        // decimal degrees (NaN = no fix)
    pub fix_quality: u8,    // 0=no fix, 1=GPS, 2=DGPS, 4=RTK
    pub fix_mode: u8,       // 1=no fix, 2=2D, 3=3D
    pub num_sats: u8,       // satellites used (from GGA)
    pub hdop: f32,
    pub pdop: f32,
    pub vdop: f32,
    pub altitude: f32,      // metres MSL
    pub speed_kn: f32,      // knots
    pub course: f32,        // degrees true
    pub sats: Vec<SatInfo>,
    pub valid: bool,
}

impl Default for GpsData {
    fn default() -> Self {
        GpsData {
            time_str:    "--:--:--".into(),
            date_str:    "----/--/--".into(),
            lat_str:     "---".into(),
            lon_str:     "---".into(),
            lat_dd:      f64::NAN,
            lon_dd:      f64::NAN,
            fix_quality: 0,
            fix_mode:    1,
            num_sats:    0,
            hdop:        0.0,
            pdop:        0.0,
            vdop:        0.0,
            altitude:    0.0,
            speed_kn:    0.0,
            course:      0.0,
            sats:        Vec::new(),
            valid:       false,
        }
    }
}

// ─── State ────────────────────────────────────────────────────────────────────

static GPS_DATA: LazyLock<Mutex<GpsData>> =
    LazyLock::new(|| Mutex::new(GpsData::default()));

/// Byte accumulator for incomplete NMEA lines.
static LINE_BUF: LazyLock<Mutex<Vec<u8>>> =
    LazyLock::new(|| Mutex::new(Vec::with_capacity(128)));

/// Satellite data accumulated across multi-message GSV sequences.
static GSV_TEMP: LazyLock<Mutex<Vec<SatInfo>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

/// In-use PRN list from GSA sentences for the current epoch.
static GSA_USE: LazyLock<Mutex<Vec<(char, u8)>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

/// Reverse-geocoded place name for the current position (empty = not yet fetched).
static LOCATION_NAME: LazyLock<Mutex<String>> =
    LazyLock::new(|| Mutex::new(String::new()));

// ─── Public API ───────────────────────────────────────────────────────────────

/// Get a snapshot of the current GPS state.
pub fn get_data() -> GpsData {
    GPS_DATA.lock().unwrap().clone()
}

/// Get the current reverse-geocoded place name (empty string if not yet available).
pub fn get_location_name() -> String {
    LOCATION_NAME.lock().unwrap().clone()
}

/// Set the reverse-geocoded place name (called from geocoder background thread).
pub fn set_location_name(name: String) {
    *LOCATION_NAME.lock().unwrap() = name;
}

/// Feed raw bytes received from the GPS serial port into the parser.
pub fn feed(data: &[u8]) {
    scan_ubx_ack(data);
    let mut buf = LINE_BUF.lock().unwrap();
    for &b in data {
        match b {
            b'\n' => {
                let line = buf.clone();
                buf.clear();
                drop(buf);
                parse_sentence(&line);
                buf = LINE_BUF.lock().unwrap();
            }
            b'\r' => {}
            _ => {
                if buf.len() < 128 {
                    buf.push(b);
                } else {
                    buf.clear(); // guard against garbage input
                }
            }
        }
    }
}

/// Rolling window for detecting UBX binary responses (`ACK-ACK`/`ACK-NAK`,
/// `MON-VER`) that arrive interleaved with NMEA text on COM1. Diagnostic aid
/// for the `UBX-CFG-VALSET` messages sent by `usb_host::start`/`apply_timepulse_freq`.
static UBX_SCAN_BUF: LazyLock<Mutex<Vec<u8>>> = LazyLock::new(|| Mutex::new(Vec::with_capacity(128)));

fn cstr_trim(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

fn scan_ubx_ack(data: &[u8]) {
    let mut buf = UBX_SCAN_BUF.lock().unwrap();
    buf.extend_from_slice(data);
    let mut i = 0usize;
    loop {
        if i + 6 > buf.len() { break; }
        if buf[i] != 0xB5 || buf[i + 1] != 0x62 {
            i += 1;
            continue;
        }
        let class = buf[i + 2];
        let id = buf[i + 3];
        let len = buf[i + 4] as usize | ((buf[i + 5] as usize) << 8);
        let frame_len = 6 + len + 2;
        if i + frame_len > buf.len() {
            break; // full frame not received yet — wait for more data
        }
        let payload = &buf[i + 6..i + 6 + len];
        match (class, id) {
            (0x05, 0x00) | (0x05, 0x01) if len == 2 => {
                let kind = if id == 0x01 { "ACK" } else { "NAK" };
                log::info!("[GPS] UBX-ACK-{} for class=0x{:02X} id=0x{:02X}", kind, payload[0], payload[1]);
            }
            (0x0A, 0x04) if len >= 40 => {
                log::info!(
                    "[GPS] UBX-MON-VER sw='{}' hw='{}'",
                    cstr_trim(&payload[0..30]),
                    cstr_trim(&payload[30..40]),
                );
                let mut off = 40;
                while off + 30 <= payload.len() {
                    let ext = cstr_trim(&payload[off..off + 30]);
                    if !ext.is_empty() {
                        log::info!("[GPS] UBX-MON-VER ext='{}'", ext);
                    }
                    off += 30;
                }
            }
            _ => {}
        }
        i += frame_len;
    }
    // Drop fully-scanned bytes; keep only the unscanned tail, which may be
    // the start of a frame split across UART reads.
    buf.drain(0..i);
    // Guard against unbounded growth if a corrupt length field makes us wait
    // forever for a frame that will never complete.
    if buf.len() > 512 {
        buf.clear();
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

#[inline]
fn pf32(s: &str) -> f32 { s.parse().unwrap_or(0.0) }
#[inline]
fn pu8(s: &str) -> u8 { s.parse().unwrap_or(0) }
#[inline]
fn pu16(s: &str) -> u16 { s.parse().unwrap_or(0) }
#[inline]
fn pi16(s: &str) -> i16 { s.parse().unwrap_or(0) }

/// Convert NMEA coordinate + hemisphere to (decimal_degrees, display_string).
/// NMEA format: DDMM.MMMMM or DDDMM.MMMMM
/// Display format: "35d44.5738N"
fn nmea_coord(val: &str, hemi: &str) -> (f64, String) {
    if val.is_empty() {
        return (0.0, "---".into());
    }
    let dot = match val.find('.') {
        Some(p) if p >= 2 => p,
        _ => return (0.0, "---".into()),
    };
    let deg_end = dot - 2; // minutes always 2 digits before decimal point
    let deg: f64 = val[..deg_end].parse().unwrap_or(0.0);
    let min: f64 = val[deg_end..].parse().unwrap_or(0.0);
    let mut decimal = deg + min / 60.0;
    if hemi == "S" || hemi == "W" { decimal = -decimal; }

    // Display as "DDd MM.MMMM H"  (ASCII only, no degree symbol)
    let min_s = &val[deg_end..];
    let end = min_s.find('.').map(|p| (p + 5).min(min_s.len())).unwrap_or(min_s.len());
    let display = format!("{}d{}{}", deg as i64, &min_s[..end], hemi);
    (decimal, display)
}

fn fmt_time(t: &str) -> String {
    if t.len() < 6 { return "--:--:--".into(); }
    format!("{}:{}:{}", &t[0..2], &t[2..4], &t[4..6])
}

fn fmt_date(d: &str) -> String {
    // DDMMYY → YYYY/MM/DD
    if d.len() < 6 { return "----/--/--".into(); }
    let y: u32 = d[4..6].parse().unwrap_or(0);
    let year = if y >= 70 { 1900 + y } else { 2000 + y };
    format!("{}/{}/{}", year, &d[2..4], &d[0..2])
}

// ─── System time sync from GPS ───────────────────────────────────────────────
// Time-source priority for this device: GPS (below) is authoritative whenever
// it has a valid fix — RMC sentences arrive every ~1 s, so GPS re-asserts
// itself far more often than SNTP's periodic background resync in main.rs,
// which keeps GPS effectively dominant while locked. When GPS has no fix,
// system time is simply left alone: with the 32.768 kHz crystal RTC clock
// source enabled (sdkconfig.defaults), it free-runs accurately off the last
// value set by GPS or NTP. NTP (main.rs) covers the case where WiFi is up
// but GPS has no fix yet (e.g. cold start indoors).

/// Correction threshold below which we don't bother calling `settimeofday`
/// (avoids needless small jumps from serial/parse jitter on every RMC
/// sentence when the clock is already close enough).
const GPS_TIME_SYNC_THRESHOLD_MS: i64 = 500;

/// Set the system clock from a GPS RMC date+time (already-formatted
/// "YYYY/MM/DD" / "HH:MM:SS" strings, UTC). No-ops on unparsable or
/// implausibly old values (receiver hasn't resolved a fix yet) and on
/// corrections smaller than `GPS_TIME_SYNC_THRESHOLD_MS`.
fn sync_system_time_from_gps(date_str: &str, time_str: &str) {
    let combined = format!("{date_str} {time_str}");
    let Ok(naive) = chrono::NaiveDateTime::parse_from_str(&combined, "%Y/%m/%d %H:%M:%S") else { return; };
    let gps_time: DateTime<Utc> = naive.and_utc();
    let gps_unix_ms = gps_time.timestamp_millis();

    // 2024-01-01 00:00:00 UTC = 1_704_067_200 — guards against a receiver
    // that hasn't resolved a real fix yet (NMEA fields can read zero/garbage
    // immediately after cold start).
    if gps_unix_ms < 1_704_067_200_000 { return; }

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    if (gps_unix_ms - now_ms).abs() < GPS_TIME_SYNC_THRESHOLD_MS { return; }

    let tv = timeval {
        tv_sec:  (gps_unix_ms / 1000) as _,
        tv_usec: ((gps_unix_ms % 1000) * 1000) as _,
    };
    unsafe { settimeofday(&tv, core::ptr::null()); }
    log::info!("[GPS] System time synced from GPS: {}", gps_time.format("%Y-%m-%d %H:%M:%S UTC"));
}

// ─── Sentence dispatcher ──────────────────────────────────────────────────────

fn parse_sentence(line: &[u8]) {
    let s = match core::str::from_utf8(line) {
        Ok(s) => s.trim(),
        Err(_) => return,
    };
    if !s.starts_with('$') || s.len() < 6 { return; }
    // Strip checksum field
    let s = s.rfind('*').map(|p| &s[..p]).unwrap_or(s);
    let fields: Vec<&str> = s[1..].split(',').collect();
    if fields.is_empty() || fields[0].len() < 5 { return; }

    let talker = &fields[0][..2];  // e.g. "GN", "GP", "GA", "GB"
    match &fields[0][2..] {
        "GGA" => parse_gga(&fields),
        "RMC" => parse_rmc(&fields),
        "GSA" => parse_gsa(&fields),
        "GSV" => parse_gsv(talker, &fields),
        _ => {}
    }
}

// ─── GGA – fix data + epoch boundary ─────────────────────────────────────────
// $GNGGA,hhmmss.ss,DDMM.MMMMM,N,DDDMM.MMMMM,E,quality,numSV,HDOP,alt,M,...

fn parse_gga(f: &[&str]) {
    if f.len() < 10 { return; }
    let time      = fmt_time(f[1]);
    let (lat, ls) = nmea_coord(f[2], f[3]);
    let (lon, lo) = nmea_coord(f[4], f[5]);
    let fix_q     = pu8(f[6]);
    let nsv       = pu8(f[7]);
    let hdop      = pf32(f[8]);
    let alt       = pf32(f[9]);

    // Commit the previous epoch's satellite data (from GSV_TEMP) into GPS_DATA
    // before resetting.  This ensures sats are written exactly once per epoch,
    // eliminating per-constellation flicker.
    {
        let temp_snap: Vec<SatInfo> = GSV_TEMP.lock().unwrap().clone();
        if !temp_snap.is_empty() {
            let mut d = GPS_DATA.lock().unwrap();
            d.sats = temp_snap;
            d.sats.sort_by(|a, b| {
                b.in_use.cmp(&a.in_use).then(b.snr.cmp(&a.snr))
            });
        }
    }

    // Epoch boundary: reset GSA/GSV accumulators for the new epoch
    GSA_USE.lock().unwrap().clear();
    GSV_TEMP.lock().unwrap().clear();

    let mut d = GPS_DATA.lock().unwrap();
    d.time_str    = time;
    if !f[2].is_empty() { d.lat_str = ls; d.lat_dd = lat; }
    if !f[4].is_empty() { d.lon_str = lo; d.lon_dd = lon; }
    d.fix_quality = fix_q;
    d.num_sats    = nsv;
    d.hdop        = hdop;
    d.altitude    = alt;
    if fix_q > 0 { d.valid = true; }
}

// ─── RMC – recommended minimum ───────────────────────────────────────────────
// $GNRMC,hhmmss.ss,A,DDMM.MMMMM,N,DDDMM.MMMMM,E,speed,course,DDMMYY,...

fn parse_rmc(f: &[&str]) {
    if f.len() < 10 { return; }
    let time      = fmt_time(f[1]);
    let status    = f[2];
    let (lat, ls) = nmea_coord(f[3], f[4]);
    let (lon, lo) = nmea_coord(f[5], f[6]);
    let speed     = pf32(f[7]);
    let course    = pf32(f[8]);
    let date      = fmt_date(f[9]);

    if status == "A" {
        sync_system_time_from_gps(&date, &time);
    }

    let mut d = GPS_DATA.lock().unwrap();
    d.time_str = time;
    d.date_str = date;
    if status == "A" {
        if !f[3].is_empty() { let _ = lat; d.lat_str = ls; }
        if !f[5].is_empty() { let _ = lon; d.lon_str = lo; }
        d.speed_kn = speed;
        d.course   = course;
        d.valid    = true;
    }
}

// ─── GSA – DOP and active satellites ─────────────────────────────────────────
// $GNGSA,mode,fixType,sv1..sv12,PDOP,HDOP,VDOP,sysId
// sysId: 1=GPS 2=GLONASS 3=Galileo 4=BeiDou

fn parse_gsa(f: &[&str]) {
    if f.len() < 18 { return; }
    let fix_mode = pu8(f[2]);
    let pdop     = pf32(f[15]);
    let hdop     = pf32(f[16]);
    let vdop     = pf32(f[17]);
    let sys_id   = if f.len() > 18 { pu8(f[18]) } else { 0 };
    let sys_ch   = match sys_id { 1 => 'G', 2 => 'L', 3 => 'A', 4 => 'B', 5 => 'Q', _ => 'N' };

    // Accumulate in-use PRNs for this constellation
    {
        let mut use_set = GSA_USE.lock().unwrap();
        for i in 3..15usize {
            if let Some(&fv) = f.get(i) {
                if let Ok(prn) = fv.parse::<u8>() {
                    if prn > 0 { use_set.push((sys_ch, prn)); }
                }
            }
        }
    }

    let mut d = GPS_DATA.lock().unwrap();
    d.fix_mode = fix_mode;
    if pdop > 0.0 { d.pdop = pdop; }
    if hdop > 0.0 { d.hdop = hdop; }
    if vdop > 0.0 { d.vdop = vdop; }
}

// ─── UBX configuration builder ───────────────────────────────────────────────

/// TIMEPULSE1 output frequency in Hz (0 = disabled). Set from NVS config before
/// `usb_host::start()` so the GPS startup thread can read it.
static TIMEPULSE_FREQ_HZ: AtomicU32 = AtomicU32::new(1);

pub fn set_timepulse_freq_hz(freq: u32) {
    TIMEPULSE_FREQ_HZ.store(freq, Ordering::Relaxed);
}

pub fn get_timepulse_freq_hz() -> u32 {
    TIMEPULSE_FREQ_HZ.load(Ordering::Relaxed)
}

/// Build a `UBX-CFG-VALSET` message to configure TIMEPULSE1 (TP1) on the
/// u-blox MAX-F10S.  `freq_hz` = 0 disables TP1; any other value (up to
/// 10 MHz, the module's hardware limit — min. high/low time is 50 ns) sets a
/// square-wave output at that frequency using CFG-TP-FREQ_TP1 (Hz-based,
/// exact for any frequency) with a fixed 50 % duty ratio via CFG-TP-DUTY_TP1.
/// Applies to RAM + BBR layers only — the MAX-F10S here has no config flash,
/// and requesting the Flash layer makes the module NAK the whole VALSET.
/// Persistence across power cycles comes from re-sending at boot (ESP32 NVS).
/// Caller writes the returned bytes to UART1 (COM1).
pub fn build_ubx_timepulse_freq(freq_hz: u32) -> Vec<u8> {
    let mut kv: Vec<u8> = Vec::new();

    if freq_hz == 0 {
        // CFG-TP-TP1_ENA (0x10050007, L) = 0 — disable
        kv.extend_from_slice(&[0x07, 0x00, 0x05, 0x10, 0x00]);
    } else {
        // CFG-TP-TP1_ENA (0x10050007, L) = 1 — enable
        kv.extend_from_slice(&[0x07, 0x00, 0x05, 0x10, 0x01]);
        // CFG-TP-PULSE_DEF (0x20050023, E1) = 1 — interpret as FREQ, not PERIOD
        kv.extend_from_slice(&[0x23, 0x00, 0x05, 0x20, 0x01]);
        // CFG-TP-FREQ_TP1 (0x40050024, U4) — frequency in Hz (unlocked)
        kv.extend_from_slice(&[0x24, 0x00, 0x05, 0x40]);
        kv.extend_from_slice(&freq_hz.to_le_bytes());
        // CFG-TP-FREQ_LOCK_TP1 (0x40050025, U4) — frequency in Hz (locked)
        kv.extend_from_slice(&[0x25, 0x00, 0x05, 0x40]);
        kv.extend_from_slice(&freq_hz.to_le_bytes());
        // CFG-TP-PULSE_LENGTH_DEF (0x20050030, E1) = 0 — interpret as RATIO, not LENGTH
        kv.extend_from_slice(&[0x30, 0x00, 0x05, 0x20, 0x00]);
        // CFG-TP-DUTY_TP1 (0x5005002a, R8) — 50 % duty cycle (unlocked)
        kv.extend_from_slice(&[0x2a, 0x00, 0x05, 0x50]);
        kv.extend_from_slice(&50.0f64.to_le_bytes());
        // CFG-TP-DUTY_LOCK_TP1 (0x5005002b, R8) — 50 % duty cycle (locked)
        kv.extend_from_slice(&[0x2b, 0x00, 0x05, 0x50]);
        kv.extend_from_slice(&50.0f64.to_le_bytes());
    }

    // UBX-CFG-VALSET payload header: version=0x00, layers=0x03 (RAM+BBR).
    // Do NOT set the Flash bit: this module has no config flash and would
    // NAK the entire message.
    let header: &[u8] = &[0x00, 0x03, 0x00, 0x00];
    let payload_len = (header.len() + kv.len()) as u16;

    let mut msg = Vec::with_capacity(6 + payload_len as usize + 2);
    msg.extend_from_slice(&[0xB5, 0x62, 0x06, 0x8A]);
    msg.push((payload_len & 0xFF) as u8);
    msg.push((payload_len >> 8) as u8);
    msg.extend_from_slice(header);
    msg.extend_from_slice(&kv);

    let (mut ck_a, mut ck_b) = (0u8, 0u8);
    for &b in &msg[2..] {
        ck_a = ck_a.wrapping_add(b);
        ck_b = ck_b.wrapping_add(ck_a);
    }
    msg.push(ck_a);
    msg.push(ck_b);
    msg
}

/// Build a `UBX-CFG-VALSET` message that enables QZSS (L1C/A, L1S, L5)
/// in the RAM + BBR configuration layers of the u-blox MAX-F10S (no config
/// flash on this module — see `build_ubx_timepulse_freq`). Re-sent at every
/// boot.  Caller writes the returned bytes to UART1 (COM1).
pub fn build_ubx_qzss_enable() -> Vec<u8> {
    // Key-value pairs (4-byte key LE + 1-byte value for type L=bool)
    #[rustfmt::skip]
    let kv: &[u8] = &[
        0x24, 0x00, 0x31, 0x10, 0x01, // CFG-SIGNAL-QZSS_ENA    (0x10310024)
        0x12, 0x00, 0x31, 0x10, 0x01, // CFG-SIGNAL-QZSS_L1CA_ENA (0x10310012)
        0x14, 0x00, 0x31, 0x10, 0x01, // CFG-SIGNAL-QZSS_L1S_ENA  (0x10310014)
        0x17, 0x00, 0x31, 0x10, 0x01, // CFG-SIGNAL-QZSS_L5_ENA   (0x10310017)
    ];
    // UBX-CFG-VALSET payload header: version=0x00, layers=0x03 (RAM+BBR), reserved[2]
    let header: &[u8] = &[0x00, 0x03, 0x00, 0x00];
    let payload_len = (header.len() + kv.len()) as u16;

    // Frame: sync(2) + class(1) + id(1) + len(2) + payload + checksum(2)
    let mut msg = Vec::with_capacity(6 + payload_len as usize + 2);
    msg.extend_from_slice(&[0xB5, 0x62, 0x06, 0x8A]);
    msg.push((payload_len & 0xFF) as u8);
    msg.push((payload_len >> 8) as u8);
    msg.extend_from_slice(header);
    msg.extend_from_slice(kv);

    // Fletcher-8 checksum over bytes[2..] (Class through last payload byte)
    let (mut ck_a, mut ck_b) = (0u8, 0u8);
    for &b in &msg[2..] {
        ck_a = ck_a.wrapping_add(b);
        ck_b = ck_b.wrapping_add(ck_a);
    }
    msg.push(ck_a);
    msg.push(ck_b);
    msg
}

/// Build a `UBX-MON-VER` poll request to query the receiver's firmware and
/// protocol version. Response is decoded by `scan_ubx_ack` and logged.
/// Caller writes the returned bytes to UART1 (COM1).
pub fn build_ubx_mon_ver_poll() -> Vec<u8> {
    let mut msg: Vec<u8> = vec![0xB5, 0x62, 0x0A, 0x04, 0x00, 0x00];
    let (mut ck_a, mut ck_b) = (0u8, 0u8);
    for &b in &msg[2..] {
        ck_a = ck_a.wrapping_add(b);
        ck_b = ck_b.wrapping_add(ck_a);
    }
    msg.push(ck_a);
    msg.push(ck_b);
    msg
}

// ─── GSV – satellites in view ────────────────────────────────────────────────
// $G?GSV,totalMsg,msgNum,totalSV,{prn,el,az,snr}...[,signalId]*cs
// Groups of 4 fields per satellite, up to 4 groups per sentence.
// u-blox NMEA 4.11: an optional signalId field may follow the last group.

fn parse_gsv(talker: &str, f: &[&str]) {
    if f.len() < 4 { return; }
    let total_msg = pu8(f[1]);
    let msg_num   = pu8(f[2]);
    if msg_num == 0 || total_msg == 0 { return; }

    let system = match talker {
        "GP" => 'G',
        "GA" => 'A',
        "GB" => 'B',
        "GL" => 'L',
        "GQ" => 'Q',  // QZSS (NMEA 4.11+)
        _    => 'N',
    };

    // Parse satellite groups (4 fields each, starting at index 4)
    // `i + 3 < f.len()` ensures all 4 fields are present;
    // any trailing signalId field is naturally excluded.
    let mut i = 4usize;
    while i + 3 < f.len() {
        let prn  = pu8(f[i]);
        let elev = pi16(f[i + 1]);
        let azim = pu16(f[i + 2]);
        let snr  = pu8(f[i + 3]);
        if prn > 0 {
            let mut temp = GSV_TEMP.lock().unwrap();
            // Upsert: keep the highest SNR seen for this PRN (multi-frequency)
            if let Some(ex) = temp.iter_mut().find(|s| s.system == system && s.prn == prn) {
                if snr > ex.snr { ex.snr = snr; }
                if elev != 0 { ex.elevation = elev; }
                if azim != 0 { ex.azimuth = azim; }
            } else {
                temp.push(SatInfo { system, prn, elevation: elev, azimuth: azim, snr, in_use: false });
            }
        }
        i += 4;
    }

    // On last message of this constellation: update GSV_TEMP only.
    // GPS_DATA.sats is committed once per epoch at GGA time to avoid
    // multiple redraws per second (one commit per constellation would
    // cause 3 flashes per epoch).
    if msg_num == total_msg {
        // Mark in_use flags inside GSV_TEMP (no write to GPS_DATA yet)
        let use_set: Vec<(char, u8)> = GSA_USE.lock().unwrap().clone();
        let mut temp = GSV_TEMP.lock().unwrap();
        for sat in temp.iter_mut().filter(|s| s.system == system) {
            sat.in_use = use_set.iter().any(|&(sys, prn)| sys == sat.system && prn == sat.prn);
        }
    }
}
