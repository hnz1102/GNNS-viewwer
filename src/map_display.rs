// Static map image fetcher — Google Maps Static API
//
// Fetches a JPEG map centred on the current GPS position and decodes it to
// an RGB565 pixel buffer that the display thread can consume.
//
// Every successfully-downloaded tile is also saved as JPEG under
// /emmc/mapcache/ (when eMMC is mounted), keyed by map type + zoom + a
// ~500 m position grid cell. When WiFi/the API key is unavailable, or the
// live fetch fails, the nearest cached tile for the current position/zoom
// is loaded from disk instead — this is what lets the map keep displaying
// when WiFi can't connect. Storage/indexing/eviction (up to a 32 GiB
// budget) is handled by `crate::map_cache`.
//
// Requirements:
//   • Maps API key set (Web UI, default from cfg.toml)  (empty = live fetch disabled, cache still works)
//   • WiFi connected (for live fetch only)
//   • PSRAM strongly recommended (320×200 JPEG decode peak ~200 KB)
//
// SPDX-License-Identifier: MIT

use log::*;
use std::sync::{LazyLock, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};
use embedded_svc::http::client::Client;
use embedded_svc::http::{Headers, Method};
use embedded_svc::io::Read;
use esp_idf_svc::http::client::{Configuration, EspHttpConnection};

// ─── Map geometry ─────────────────────────────────────────────────────────────

/// Width of the map area in pixels (= display width).
pub const MAP_W: usize = 320;
/// Height of the map area in pixels (leaves 40px at bottom for GPS info bar).
pub const MAP_H: usize = 200;

// ─── Shared pixel buffer ──────────────────────────────────────────────────────

/// RGB565 pixel buffer, row-major, MAP_W × MAP_H elements.
/// Stored in heap (uses PSRAM automatically if configured in sdkconfig).
static MAP_PIXELS: LazyLock<Mutex<Option<Vec<u16>>>> =
    LazyLock::new(|| Mutex::new(None));

/// Monotonically incremented each time new pixels are stored.
/// The display thread compares against its cached value to detect changes.
static MAP_VERSION: AtomicU32 = AtomicU32::new(0);

static MAP_BUSY: AtomicBool = AtomicBool::new(false);
static LAST_MAP_SECS: AtomicU32 = AtomicU32::new(0);
static LAST_MAP_POS: LazyLock<Mutex<(f64, f64)>> =
    LazyLock::new(|| Mutex::new((f64::NAN, f64::NAN)));

/// Google Maps API key currently in effect. Seeded at boot from NVS (whose
/// default is the cfg.toml value) and updated live when it is saved from the
/// Web UI, so changing it needs no reboot or rebuild.
static API_KEY: Mutex<String> = Mutex::new(String::new());

/// Current Google Maps API key (empty if none is set).
pub fn api_key() -> String { API_KEY.lock().unwrap().clone() }

/// true if an API key is set, i.e. live map downloads are possible.
pub fn has_api_key() -> bool { !API_KEY.lock().unwrap().is_empty() }

/// Set the API key (surrounding whitespace is dropped). Returns true if it
/// changed, so the caller can force a refetch with the new key.
pub fn set_api_key(key: &str) -> bool {
    let key = key.trim();
    let mut cur = API_KEY.lock().unwrap();
    if *cur == key { return false; }
    *cur = key.to_string();
    true
}

/// Sender half of the persistent fetch worker's request queue — see
/// `start_fetch_worker()`. `None` until that's been called (or if its
/// initial thread spawn failed).
static FETCH_TX: Mutex<Option<Sender<(f64, f64)>>> = Mutex::new(None);

/// Current map zoom level (1–20). Default 15.
static MAP_ZOOM: AtomicU8 = AtomicU8::new(15);

/// true = satellite (aerial photo), false = roadmap.  Updated by httpserver on config load/save.
static MAP_TYPE_SAT: AtomicBool = AtomicBool::new(true);

/// Centre (lat, lon) of the map image that is currently in MAP_PIXELS.
/// Set only on a successful fetch so projection is always consistent.
static MAP_CENTER: LazyLock<Mutex<(f64, f64)>> =
    LazyLock::new(|| Mutex::new((f64::NAN, f64::NAN)));

/// Zoom level the pixels currently in MAP_PIXELS were fetched at. Lags
/// MAP_ZOOM from a zoom change until the refetch at the new level lands, so
/// anything projected onto the image that's actually on screen — markers,
/// the route, the scale bar — has to use this rather than `get_zoom()`,
/// otherwise it sits at the wrong place for a second or two after a zoom
/// change.
static MAP_TILE_ZOOM: AtomicU8 = AtomicU8::new(15);

/// GPS position trail ring buffer: up to TRAIL_MAX (lat f32, lon f32) pairs.
const TRAIL_MAX: usize = 100;
static TRAIL: LazyLock<Mutex<Vec<(f32, f32)>>> =
    LazyLock::new(|| Mutex::new(Vec::with_capacity(TRAIL_MAX + 1)));

// ─── Public API ───────────────────────────────────────────────────────────────

/// Returns the current map version counter.
pub fn version() -> u32 {
    MAP_VERSION.load(Ordering::Relaxed)
}

/// Returns true if pixel data is available (i.e. at least one fetch succeeded).
pub fn has_map() -> bool {
    MAP_PIXELS.lock().unwrap().is_some()
}

/// Returns true if a fetch is currently in progress.
pub fn is_busy() -> bool {
    MAP_BUSY.load(Ordering::Relaxed)
}

/// Returns the current zoom level.
pub fn get_zoom() -> u8 { MAP_ZOOM.load(Ordering::Relaxed) }

/// Zoom level the map image currently on screen was fetched at — see
/// `MAP_TILE_ZOOM`. Use this (not `get_zoom`) to project anything onto it.
pub fn get_tile_zoom() -> u8 { MAP_TILE_ZOOM.load(Ordering::Relaxed) }

/// Set whether the map type is satellite (true) or roadmap (false).
pub fn set_map_type_satellite(sat: bool) { MAP_TYPE_SAT.store(sat, Ordering::Relaxed); }

/// Toggle between satellite and roadmap. Returns the new state (true = satellite).
pub fn toggle_map_type() -> bool {
    let prev = MAP_TYPE_SAT.fetch_xor(true, Ordering::Relaxed);
    !prev // new value
}

/// Wall-clock floor between two "force an immediate refetch" resets, shared
/// by every caller of `reset_last_pos()` (zoom in/out, map-type toggle,
/// track-playback jumping to an off-screen point, ...). Without this, a
/// rapid burst of calls — a zoom button held and auto-repeating every
/// ~300 ms, someone mashing the map-type toggle, or fast track-playback
/// scrubbing — each cancel map_display's own 500 m/60 s cooldown in turn,
/// driving the TLS+JPEG fetch pipeline harder than this chip can sustain
/// (observed as recurring "esp-aes: Failed to allocate memory" and
/// "pthread: Failed to create task!" under load). A burst still ends up
/// fetching the *last* requested position/zoom/type once the floor passes —
/// it just stops trying to fetch after every single intermediate step.
static LAST_FORCE_RESET_SECS: AtomicU32 = AtomicU32::new(0);
const FORCE_RESET_MIN_INTERVAL_SECS: u32 = 3;

/// Unix-seconds deadline set by `trigger()` whenever a fetch attempt fails
/// because the system couldn't even spawn a thread for it (genuine resource
/// exhaustion, not just "hasn't moved/waited long enough"). `reset_last_pos`
/// must not be allowed to cancel this the way it cancels the normal 60 s
/// cooldown: forcing a fetch is exactly what led to the exhaustion, so
/// honoring another force request during this window just retries into the
/// same failure every few seconds — observed as "retry after 5 min" firing
/// every few seconds instead of actually every 5 minutes — instead of
/// giving the system real time to recover.
static HARD_BACKOFF_UNTIL_SECS: AtomicU32 = AtomicU32::new(0);

/// Reset cached position and cooldown so the next trigger() call forces an
/// immediate new map fetch (used after zoom or map-type changes). Rate
/// limited — see `LAST_FORCE_RESET_SECS` — so repeated calls in quick
/// succession collapse into a single forced refetch, and suppressed
/// entirely during a `HARD_BACKOFF_UNTIL_SECS` window.
pub fn reset_last_pos() {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as u32).unwrap_or(0);
    if now < HARD_BACKOFF_UNTIL_SECS.load(Ordering::Relaxed) {
        return;
    }
    if now.saturating_sub(LAST_FORCE_RESET_SECS.load(Ordering::Relaxed)) < FORCE_RESET_MIN_INTERVAL_SECS {
        return;
    }
    LAST_FORCE_RESET_SECS.store(now, Ordering::Relaxed);
    *LAST_MAP_POS.lock().unwrap() = (f64::NAN, f64::NAN);
    LAST_MAP_SECS.store(0, Ordering::Relaxed);
}

/// Returns the centre (lat, lon) of the currently displayed map image.
/// Returns (NaN, NaN) if no map has been fetched yet.
pub fn get_map_center() -> (f64, f64) {
    *MAP_CENTER.lock().unwrap()
}

/// Push a new GPS fix onto the trail ring buffer.
/// Skips duplicate points that are within ~5 m of the last stored point.
pub fn push_trail(lat: f64, lon: f64) {
    let mut t = TRAIL.lock().unwrap();
    if let Some(&(ll, lo)) = t.last() {
        if (lat as f32 - ll).abs() < 0.00005 && (lon as f32 - lo).abs() < 0.00005 {
            return;
        }
    }
    if t.len() >= TRAIL_MAX { t.remove(0); }
    t.push((lat as f32, lon as f32));
}

/// Returns the number of trail points currently stored (cheap, no clone).
pub fn trail_len() -> usize {
    TRAIL.lock().unwrap().len()
}

/// Returns a snapshot clone of all trail points (oldest first).
pub fn get_trail() -> Vec<(f32, f32)> {
    TRAIL.lock().unwrap().clone()
}

/// Zoom in (increase zoom level, max 20). Resets position to force a new fetch.
pub fn zoom_in() {
    let z = MAP_ZOOM.load(Ordering::Relaxed);
    if z < 20 {
        MAP_ZOOM.store(z + 1, Ordering::Relaxed);
        // Force refetch at new zoom level (rate limited — see reset_last_pos)
        reset_last_pos();
        info!("[MAP] zoom in -> {}", z + 1);
    }
}

/// Zoom out (decrease zoom level, min 1). Resets position to force a new fetch.
pub fn zoom_out() {
    let z = MAP_ZOOM.load(Ordering::Relaxed);
    if z > 1 {
        MAP_ZOOM.store(z - 1, Ordering::Relaxed);
        reset_last_pos();
        info!("[MAP] zoom out -> {}", z - 1);
    }
}

/// Set the zoom level directly (clamped to 1–20), forcing a new fetch.
pub fn set_zoom(z: u8) {
    let z = z.clamp(1, 20);
    if MAP_ZOOM.swap(z, Ordering::Relaxed) != z {
        reset_last_pos();
    }
    info!("[MAP] zoom set -> {}", z);
}

/// Highest zoom level (most detail) at which the whole lat/lon bounding box
/// is still visible on a map image centred at (`center_lat`, `center_lon`).
///
/// The viewport is centred on that point rather than on the middle of the
/// box, so what actually has to fit is twice the largest deviation from the
/// centre — fitting the raw box would leave half the route off screen when
/// the centre sits at one end of it, as it does on a track's first point.
pub fn zoom_to_fit(
    center_lat: f64, center_lon: f64,
    lat_min: f64, lat_max: f64,
    lon_min: f64, lon_max: f64,
) -> u8 {
    /// Fraction of the tile used, so the route isn't drawn hard against the
    /// edges — this also absorbs the map area being slightly shorter on
    /// screen (179 px) than the tile is tall (MAP_H).
    const USABLE: f64 = 0.85;

    let merc = |lat: f64| -> f64 {
        let l = lat.clamp(-85.0, 85.0).to_radians();
        (l / 2.0 + core::f64::consts::FRAC_PI_4).tan().ln()
    };

    let half_lon = (lon_max - center_lon).abs().max((center_lon - lon_min).abs());
    let mc = merc(center_lat);
    let half_merc = (merc(lat_max) - mc).abs().max((mc - merc(lat_min)).abs());
    let lon_span  = (2.0 * half_lon).max(1e-9);
    let merc_span = (2.0 * half_merc).max(1e-9);

    for z in (1..=20u8).rev() {
        let world = 256.0 * (1u64 << z) as f64;
        let w_px = lon_span * world / 360.0;
        let h_px = merc_span * world / (2.0 * core::f64::consts::PI);
        if w_px <= MAP_W as f64 * USABLE && h_px <= MAP_H as f64 * USABLE {
            return z;
        }
    }
    1
}

/// Call the closure with a reference to the pixel slice, if available.
/// Avoids cloning the large buffer.
pub fn with_pixels<F: FnOnce(&[u16])>(f: F) {
    if let Some(ref px) = *MAP_PIXELS.lock().unwrap() {
        f(px.as_slice());
    }
}

/// Spawn the persistent background worker that performs map tile
/// fetch/cache-lookup requests sent by `trigger()`. Call once at startup,
/// before anything can call `trigger()`.
///
/// A single long-lived worker (rather than a fresh thread spawned per
/// fetch, as this used to work) avoids the repeated create/destroy cycles
/// that were fragmenting internal DRAM badly enough to eventually fail
/// "pthread: Failed to create task!" outright under sustained use (e.g.
/// fast track-playback scrubbing). Its stack stays in internal DRAM
/// deliberately (NOT PSRAM): a PSRAM stack was tried here and in the JPEG
/// decode workers to relieve internal-DRAM pressure instead, but produced a
/// hard crash (Guru Meditation "Double exception", EXCVADDR 0x0, inside a
/// memcpy with the FreeRTOS kernel lock in a register) right as JPEG
/// decoding started — Xtensa's window overflow/underflow register-spill
/// handling for a PSRAM-resident stack is a known rough edge on the S3, and
/// ESP-IDF's CONFIG_SPIRAM_ALLOW_STACK_EXTERNAL_MEMORY trampoline doesn't
/// fully cover every nested-exception path.
pub fn start_fetch_worker() {
    let (tx, rx) = mpsc::channel::<(f64, f64)>();
    let r = thread::Builder::new()
        .name("map_fetch".into())
        .stack_size(24576)
        .spawn(move || {
            for (lat, lon) in rx {
                let have_key = has_api_key();
                let wifi_up  = crate::wifi::get_rssi() != 0;
                if have_key && wifi_up {
                    // Free the quake WebSocket's TLS memory for the duration of this fetch.
                    crate::quake::release_ws_for_map();
                }
                let zoom = MAP_ZOOM.load(Ordering::Relaxed);
                let sat  = MAP_TYPE_SAT.load(Ordering::Relaxed);

                let px = if have_key && wifi_up {
                    fetch_and_decode(lat, lon, zoom, sat).or_else(|| {
                        info!("[MAP] live fetch failed, trying disk cache");
                        load_cached_tile(lat, lon, zoom, sat)
                    })
                } else {
                    load_cached_tile(lat, lon, zoom, sat)
                };

                match px {
                    Some(px) => {
                        *MAP_PIXELS.lock().unwrap() = Some(px);
                        *MAP_CENTER.lock().unwrap() = (lat, lon);
                        MAP_TILE_ZOOM.store(zoom, Ordering::Relaxed);
                        MAP_VERSION.fetch_add(1, Ordering::Relaxed);
                        info!("[MAP] pixel buffer updated ({}×{})", MAP_W, MAP_H);
                    }
                    None => {
                        // Fetch failed: reset position, but keep LAST_MAP_SECS
                        // so we wait the full 60 s cooldown before retrying.
                        *LAST_MAP_POS.lock().unwrap() = (f64::NAN, f64::NAN);
                    }
                }
                MAP_BUSY.store(false, Ordering::Relaxed);
            }
        });

    match r {
        Ok(_) => *FETCH_TX.lock().unwrap() = Some(tx),
        Err(e) => warn!("[MAP] failed to start fetch worker thread ({:?}) — map fetching disabled this boot", e),
    }
}

/// Trigger a background map fetch/cache-lookup when:
///   - no fetch is already running
///   - position moved > ~500 m (0.005°) or first fix
///   - at most once per 60 s
///
/// When `maps_api_key` is configured and WiFi is connected, fetches a fresh
/// tile live and caches it to eMMC. Otherwise (or if the live fetch fails),
/// falls back to the nearest cached tile on eMMC for the current
/// position/zoom/maptype, so the map keeps displaying while offline.
pub fn trigger(lat: f64, lon: f64) {
    let have_key = has_api_key();
    if !have_key && !crate::emmc::is_mounted() { return; } // nothing we could show either way

    // Atomically acquire the busy flag; bail out if already set.
    // compare_exchange prevents the race between check and set that would
    // otherwise allow two callers (display thread + httpd) to both queue a
    // fetch simultaneously.
    if MAP_BUSY.compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed).is_err() {
        return;
    }
    // From here we "own" MAP_BUSY=true.  Every early-return must release it.

    let now = SystemTime::now().duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as u32).unwrap_or(0);
    if now.saturating_sub(LAST_MAP_SECS.load(Ordering::Relaxed)) < 60 {
        MAP_BUSY.store(false, Ordering::Relaxed);
        return;
    }

    let (ll, lo) = *LAST_MAP_POS.lock().unwrap();
    let first  = ll.is_nan();
    let moved  = (lat - ll).abs() > 0.005 || (lon - lo).abs() > 0.005;
    if !first && !moved {
        MAP_BUSY.store(false, Ordering::Relaxed);
        return;
    }

    *LAST_MAP_POS.lock().unwrap() = (lat, lon);
    LAST_MAP_SECS.store(now, Ordering::Relaxed);

    // Hand off to the persistent fetch worker (see start_fetch_worker) — a
    // channel send, not a thread spawn, so this can't itself fail from
    // task-creation memory pressure the way a per-fetch spawn used to.
    let sent = FETCH_TX.lock().unwrap().as_ref()
        .map(|tx| tx.send((lat, lon)).is_ok())
        .unwrap_or(false);

    if !sent {
        // Worker was never started (start_fetch_worker's spawn failed at
        // boot) or has since died: back off 5 minutes before trying again.
        // HARD_BACKOFF_UNTIL_SECS additionally stops reset_last_pos() from
        // cancelling this backoff early (see its doc comment).
        let backoff = now.saturating_add(300);
        warn!("[MAP] fetch worker unavailable, retry after 5 min");
        MAP_BUSY.store(false, Ordering::Relaxed);
        *LAST_MAP_POS.lock().unwrap() = (f64::NAN, f64::NAN);
        LAST_MAP_SECS.store(backoff, Ordering::Relaxed);
        HARD_BACKOFF_UNTIL_SECS.store(backoff, Ordering::Relaxed);
    }
}

// ─── Fetch + decode pipeline ──────────────────────────────────────────────────

/// `zoom`/`sat` are passed in rather than re-read from the globals so the
/// tile that lands in MAP_PIXELS is guaranteed to match the zoom recorded in
/// MAP_TILE_ZOOM, even if the user changes either mid-fetch.
fn fetch_and_decode(lat: f64, lon: f64, zoom: u8, sat: bool) -> Option<Vec<u16>> {
    let api_key = api_key();

    // Google Maps Static API: JPEG output, road map
    let url = format!(
        "https://maps.googleapis.com/maps/api/staticmap\
         ?center={:.6},{:.6}&zoom={}&size={}x{}&format=jpg&maptype={}&key={}",
        lat, lon, zoom, MAP_W, MAP_H,
        if sat { "satellite" } else { "roadmap" },
        api_key
    );
    info!("[MAP] fetching {}×{} JPEG for ({:.4},{:.4})", MAP_W, MAP_H, lat, lon);

    let jpeg = http_get_jpeg(&url)?;
    info!("[MAP] downloaded {} bytes", jpeg.len());

    // Decode then drop the raw JPEG bytes immediately to free heap before storing RGB565
    let result = decode_jpeg_to_rgb565(&jpeg);
    // Only cache once we know the bytes are actually a valid, complete JPEG —
    // caching on download alone let a truncated/corrupt response poison the
    // disk cache permanently (every later fallback load would hit the same
    // decode failure forever, since nothing ever overwrites a bad cache file
    // with a good one unless a fresh live fetch happens to succeed here).
    if result.is_some() {
        save_cached_tile(lat, lon, zoom, sat, &jpeg);
    }
    drop(jpeg);
    result
}

// ─── Disk tile cache (eMMC) ────────────────────────────────────────────────────
//
// Storage, indexing (O(1) lookup instead of directory scans), and eviction
// live in `crate::map_cache` — see that module for the design rationale.

/// Save a freshly-downloaded JPEG tile to the disk cache. Best-effort: a
/// write failure only logs a warning, it never fails the live fetch.
fn save_cached_tile(lat: f64, lon: f64, zoom: u8, sat: bool, jpeg: &[u8]) {
    if !crate::map_cache::is_ready() { return; }

    let (glat, glon) = crate::map_cache::grid_key(lat, lon);
    let path = crate::map_cache::tile_path(zoom, sat, glat, glon);
    if let Some(dir) = std::path::Path::new(&path).parent() {
        if std::fs::create_dir_all(dir).is_err() { return; }
    }
    match std::fs::write(&path, jpeg) {
        Ok(()) => {
            info!("[MAP] cached tile -> {}", path);
            crate::map_cache::record_saved(zoom, sat, glat, glon, jpeg.len() as u32);
        }
        Err(e) => warn!("[MAP] cache write failed ({}): {:?}", path, e),
    }
}

/// Find and decode the nearest cached tile for the given position/zoom/maptype.
/// Returns `None` if the cache index isn't ready yet or nothing is within
/// `map_cache::MAX_SEARCH_RADIUS` of the current position.
fn load_cached_tile(lat: f64, lon: f64, zoom: u8, sat: bool) -> Option<Vec<u16>> {
    let (glat, glon) = crate::map_cache::grid_key(lat, lon);
    let path = crate::map_cache::find_nearest(zoom, sat, glat, glon, crate::map_cache::MAX_SEARCH_RADIUS)?;
    let jpeg = std::fs::read(&path).ok()?;
    info!("[MAP] loaded cached tile <- {}", path);
    decode_jpeg_to_rgb565(&jpeg)
}

/// Download the URL and return the response body.  
/// Caps at 256 KB to prevent heap overflow.
fn http_get_jpeg(url: &str) -> Option<Vec<u8>> {
    let cfg = Configuration {
        crt_bundle_attach: Some(esp_idf_sys::esp_crt_bundle_attach),
        use_global_ca_store: false,
        buffer_size:    Some(2048),
        buffer_size_tx: Some(512),
        // mbedTLS now runs off PSRAM (see sdkconfig.defaults'
        // MBEDTLS_EXTERNAL_MEM_ALLOC) to stop internal-DRAM fragmentation
        // from crashing the handshake outright; the tradeoff is slower
        // crypto, with full fetches sometimes taking 10-15 s instead of the
        // 1-2 s seen when mbedTLS used internal RAM. The previous unset
        // (short, ESP-IDF default ~5-7 s) timeout was cutting those off
        // mid-handshake/mid-read, which is worse than just waiting.
        timeout: Some(std::time::Duration::from_secs(20)),
        ..Default::default()
    };

    let conn = EspHttpConnection::new(&cfg)
        .map_err(|e| info!("[MAP] conn: {:?}", e)).ok()?;
    let mut client = Client::wrap(conn);

    let headers = [
        ("User-Agent", "satellite-viewer/1.0"),
        ("Accept",     "image/jpeg"),
    ];

    let req = client.request(Method::Get, url, &headers)
        .map_err(|e| info!("[MAP] req: {:?}", e)).ok()?;
    let mut resp = req.submit()
        .map_err(|e| info!("[MAP] submit: {:?}", e)).ok()?;
    let expected_len = resp.content_len();

    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 512];
    loop {
        match Read::read(&mut resp, &mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.len() > 128 * 1024 { break; }
            }
            // A read error mid-transfer (e.g. a dropped TLS connection) used
            // to fall through and hand back whatever partial bytes had
            // already been buffered as if it were a complete response —
            // that truncated JPEG would then fail to decode and, worse, get
            // written to the disk cache (see fetch_and_decode), poisoning
            // it permanently. Treat it as a hard failure instead.
            Err(e) => {
                warn!("[MAP] read error after {} bytes: {:?}", buf.len(), e);
                return None;
            }
        }
    }

    // Cross-check against Content-Length when the server sent one: a
    // connection that closes cleanly (Ok(0)) before the advertised length is
    // reached is a truncated response too, not a valid short one.
    if let Some(expected) = expected_len {
        if buf.len() as u64 != expected {
            warn!("[MAP] truncated response: got {} of {} bytes", buf.len(), expected);
            return None;
        }
    }

    if buf.is_empty() { info!("[MAP] empty response"); None } else { Some(buf) }
}

/// Decode JPEG bytes to a packed RGB565 Vec of exactly MAP_W × MAP_H elements.
fn decode_jpeg_to_rgb565(jpeg: &[u8]) -> Option<Vec<u16>> {
    // For images above 128×128 px (every map tile here), jpeg-decoder spawns
    // one OS thread per colour component (up to 3) to decode rows in
    // parallel, and gives them no explicit stack size — so their stacks come
    // from CONFIG_PTHREAD_TASK_STACK_SIZE_DEFAULT (see sdkconfig.defaults for
    // why that Kconfig value is the only thing that can size them, and why
    // it's set low). Trying to size them from here with
    // ThreadSpawnConfiguration does NOT work: ESP-IDF's pthread_create
    // overrides the per-thread config with the attr that Rust's
    // thread::Builder always passes.
    let mut dec = jpeg_decoder::Decoder::new(std::io::Cursor::new(jpeg));
    let rgb_bytes = dec.decode()
        .map_err(|e| info!("[MAP] jpeg decode: {:?}", e)).ok()?;
    let info = dec.info()?;

    let src_w = info.width  as usize;
    let src_h = info.height as usize;
    let bpp   = match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => 3,
        jpeg_decoder::PixelFormat::L8    => 1,
        _                                => return None,
    };
    info!("[MAP] decoded {}×{} bpp={}", src_w, src_h, bpp);

    let mut out: Vec<u16> = Vec::with_capacity(MAP_W * MAP_H);

    for y in 0..MAP_H {
        for x in 0..MAP_W {
            let px = if y < src_h && x < src_w {
                let i = (y * src_w + x) * bpp;
                if i + bpp <= rgb_bytes.len() {
                    let (r, g, b) = if bpp == 3 {
                        (rgb_bytes[i] as u16, rgb_bytes[i+1] as u16, rgb_bytes[i+2] as u16)
                    } else {
                        let l = rgb_bytes[i] as u16;
                        (l, l, l)
                    };
                    // Pack to RGB565: RRRRRGGGGGGBBBBB
                    ((r & 0xF8) << 8) | ((g & 0xFC) << 3) | (b >> 3)
                } else { 0 }
            } else { 0 };
            out.push(px);
        }
    }

    Some(out)
}
