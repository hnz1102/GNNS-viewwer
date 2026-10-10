// Reverse geocoding via Nominatim (OpenStreetMap)
// Converts decimal-degree lat/lon → place name string for display.
// Runs in a dedicated background thread; WiFi + a synced system clock (GPS or NTP) required.
// SPDX-License-Identifier: MIT

use log::*;
use std::sync::{LazyLock, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use embedded_svc::http::client::Client;
use embedded_svc::http::Method;
use embedded_svc::io::Read;
use esp_idf_svc::http::client::{Configuration, EspHttpConnection};
use serde::Deserialize;

// ─── Persistent trigger state ─────────────────────────────────────────────────

/// True while a geocoder thread is running (prevents parallel requests).
static GEO_BUSY: AtomicBool = AtomicBool::new(false);

/// Last position that was successfully geocoded (lat, lon).
/// NaN = never geocoded.
static LAST_GEO_POS: LazyLock<Mutex<(f64, f64)>> =
    LazyLock::new(|| Mutex::new((f64::NAN, f64::NAN)));

/// Unix timestamp (seconds) when trigger() last spawned a thread.
/// Used to rate-limit calls since trigger() may be called on every UART chunk.
static LAST_TRIGGER_SECS: AtomicU32 = AtomicU32::new(0);

// ─── Nominatim response types ─────────────────────────────────────────────────

#[derive(Deserialize)]
struct NominatimAddress {
    suburb:        Option<String>,
    neighbourhood: Option<String>,
    city_district: Option<String>,
    city:          Option<String>,
    town:          Option<String>,
    village:       Option<String>,
    state:         Option<String>,
    country:       Option<String>,
}

#[derive(Deserialize)]
struct NominatimResponse {
    address: Option<NominatimAddress>,
}

// ─── Public API ───────────────────────────────────────────────────────────────

/// Call this each time a valid GPS fix arrives (e.g. after each GGA parse).
/// Spawns a background geocoder thread only when:
///   - position moved > ~111 m (0.001° ≈ 111 m), or first fix ever
///   - no geocoder thread is already running
///   - WiFi is connected (RSSI ≠ 0)
///   - system time is synced, via GPS or NTP (system time > 2024-01-01)
pub fn trigger(lat: f64, lon: f64) {
    // Quick bail if a request is already in progress
    if GEO_BUSY.load(Ordering::Relaxed) {
        return;
    }

    // Rate-limit: trigger() is called on every UART chunk; check at most once per 5 s
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0);
    let last = LAST_TRIGGER_SECS.load(Ordering::Relaxed);
    if now_secs.saturating_sub(last) < 5 {
        return;
    }
    LAST_TRIGGER_SECS.store(now_secs, Ordering::Relaxed);

    let (last_lat, last_lon) = *LAST_GEO_POS.lock().unwrap();
    let first  = last_lat.is_nan();
    let moved  = (lat - last_lat).abs() > 0.001 || (lon - last_lon).abs() > 0.001;

    if !first && !moved {
        return;
    }

    // システム時刻が同期済みか確認（TLS 証明書の有効期限チェックに必要）。GPS
    // またはNTPのいずれかで同期されていればOK（gps_parser::sync_system_time_from_gps）。
    // 2024-01-01 00:00:00 UTC = 1_704_067_200
    let time_ok = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as u32 > 1_704_067_200u32)
        .unwrap_or(false);
    if !time_ok {
        info!("[GEO] System time not synced yet, skipping");
        return;
    }

    // WiFi 接続確認
    if crate::wifi::get_rssi() == 0 {
        info!("[GEO] WiFi not connected, skipping");
        return;
    }

    *LAST_GEO_POS.lock().unwrap() = (lat, lon);
    GEO_BUSY.store(true, Ordering::Relaxed);

    // ジオコーダスレッド: TLS + JSON 処理用に 20 KB スタックを確保
    let result = thread::Builder::new()
        .stack_size(20480)
        .spawn(move || {
            if let Some(name) = reverse_geocode(lat, lon) {
                crate::gps_parser::set_location_name(name);
            } else {
                // 失敗時はリセット → 次回 GGA で再試行
                *LAST_GEO_POS.lock().unwrap() = (f64::NAN, f64::NAN);
            }
            GEO_BUSY.store(false, Ordering::Relaxed);
        });
    if result.is_err() {
        // スレッド生成失敗: フラグをリセットして次回再試行できるようにする
        warn!("[GEO] thread spawn failed, will retry");
        GEO_BUSY.store(false, Ordering::Relaxed);
        *LAST_GEO_POS.lock().unwrap() = (f64::NAN, f64::NAN);
        LAST_TRIGGER_SECS.store(0, Ordering::Relaxed);
    }
}

// ─── Nominatim HTTP request ───────────────────────────────────────────────────

/// 逆ジオコーディング (Nominatim / OpenStreetMap)
/// lat/lon (DD形式) → 地名文字列 (最大 20 文字 ASCII)
/// 失敗時は None を返す。TLS メモリ確保失敗に備え 10 秒待ちで最大 3 回リトライ。
pub fn reverse_geocode(lat: f64, lon: f64) -> Option<String> {
    for attempt in 0..3u32 {
        if attempt > 0 {
            info!("[GEO] retry #{}", attempt);
            thread::sleep(Duration::from_secs(10));
        }
        if let Some(result) = try_geocode(lat, lon) {
            return Some(result);
        }
    }
    info!("[GEO] all attempts failed");
    None
}

fn try_geocode(lat: f64, lon: f64) -> Option<String> {
    unsafe {
        let free = esp_idf_sys::esp_get_free_heap_size();
        let min  = esp_idf_sys::esp_get_minimum_free_heap_size();
        info!("[GEO] heap free={} min_ever={}", free, min);
    }

    let url = format!(
        "https://nominatim.openstreetmap.org/reverse\
         ?lat={:.6}&lon={:.6}&format=json&zoom=14&accept-language=en",
        lat, lon
    );
    info!("[GEO] {}", url);

    let config = Configuration {
        crt_bundle_attach: Some(esp_idf_sys::esp_crt_bundle_attach),
        use_global_ca_store: false,
        buffer_size:    Some(2048),
        buffer_size_tx: Some(512),
        ..Default::default()
    };

    let connection = EspHttpConnection::new(&config)
        .map_err(|e| info!("[GEO] connection: {:?}", e))
        .ok()?;

    let mut client = Client::wrap(connection);

    let headers = [
        ("User-Agent", "satellite-viewer/1.0"),
        ("Accept",     "application/json"),
    ];

    let request = client
        .request(Method::Get, &url, &headers)
        .map_err(|e| info!("[GEO] request: {:?}", e))
        .ok()?;

    let mut response = request
        .submit()
        .map_err(|e| info!("[GEO] submit: {:?}", e))
        .ok()?;

    // レスポンス本文を読み込む
    let mut body = [0u8; 2048];
    let mut total = 0usize;
    loop {
        match Read::read(&mut response, &mut body[total..]) {
            Ok(0) => break,
            Ok(n) => {
                total += n;
                if total >= body.len() { break; }
            }
            Err(_) => break,
        }
    }

    if total == 0 {
        info!("[GEO] empty response");
        return None;
    }

    let text = core::str::from_utf8(&body[..total]).ok()?;
    info!("[GEO] snippet: {}", &text[..text.len().min(120)]);

    let parsed: NominatimResponse = serde_json::from_str(text)
        .map_err(|e| info!("[GEO] json: {:?}", e))
        .ok()?;

    let addr = parsed.address?;

    // 地名の組み立て: suburb(丁目/区) → city(市) → state(都道府県) → country(国)
    // city と state が同じ場合（例: 東京都=東京市相当）は state を省略する
    let area    = addr.suburb.or(addr.neighbourhood).or(addr.city_district).unwrap_or_default();
    let city    = addr.city.or(addr.town).or(addr.village).unwrap_or_default();
    let state   = addr.state.unwrap_or_default();
    let country = addr.country.unwrap_or_default();

    let mut parts: Vec<&str> = Vec::new();
    if !area.is_empty()    { parts.push(&area); }
    if !city.is_empty()    { parts.push(&city); }
    // state が city と重複する場合は省略（例: Tokyo / Tokyo）
    if !state.is_empty() && state != city { parts.push(&state); }
    if !country.is_empty() { parts.push(&country); }
    if parts.is_empty()    { parts.push("Unknown"); }

    let name = parts.join(",");

    // FONT_6X12 (6 px/char), 320 px 幅: "Loc: "(5 chars) を除くと最大 48 文字使用可能
    // 余裕を持って 40 文字に制限する
    let result: String = name.chars().take(40).collect();
    info!("[GEO] => {}", result);
    Some(result)
}
