// Disk-backed, indexed cache for map tiles on eMMC.
//
// Problem this solves: a flat directory of cached tiles doesn't scale to a
// large cache budget — both a directory listing (for "find nearest tile" or
// pruning) AND FatFs's own directory scan on file create (duplicate-name
// check) are O(n) in the number of cached tiles. At a 32 GiB budget and an
// observed ~15 KB average tile size, that's up to ~2.2M tiles: O(n) scans
// are no longer acceptable at that scale.
//
// Design:
//   - A fixed-capacity open-addressing hash table (`index.bin`) maps
//     (zoom, maptype, grid_lat, grid_lon) -> (size, mtime) with O(1)-average
//     lookup/insert/delete via direct seeks, no directory scan involved.
//   - Actual tile bytes are still individual files (easy to inspect/debug
//     over SSH/SFTP), but sharded across 256 subdirectories by hash so any
//     single directory stays small enough for FatFs's own linear directory
//     scan to stay cheap even at millions of total tiles.
//   - Total cache size is tracked in RAM (rebuilt by one sequential index
//     scan at boot) and enforced by evicting the oldest entries, found via a
//     two-pass day-bucketed histogram scan (bounded ~32 KB of RAM) rather
//     than sorting every entry.
//   - Creating + zero-filling the index on first boot (or scanning it to
//     rebuild the running total on later boots) runs on a background
//     thread so it never blocks boot or WiFi — a lesson learned from an
//     earlier eMMC-related boot-time reboot loop in this project.
//
// SPDX-License-Identifier: MIT

use log::*;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

pub const CACHE_DIR: &str = "/emmc/mapcache";
const INDEX_PATH: &str = "/emmc/mapcache/index.bin";

/// Hash table capacity (power of two). 4,194,304 slots x 20-byte records =
/// 80 MiB fixed index size. Sized so that even at the 32 GiB cap with tiles
/// as small as ~8 KB (well below the observed ~15 KB average), the table's
/// load factor stays under ~0.65 and doesn't fill up before the byte budget
/// does.
const CAPACITY: u64 = 1 << 22;
const RECORD_SIZE: u64 = 20;
/// Safety bound on open-addressing probe length. Real probe chains stay
/// short at our target load factor; this only guards against pathological
/// clustering turning a lookup into a full-table scan.
const MAX_PROBE: u64 = 10_000;

/// Cache size cap. Enforced by evicting the oldest entries (by mtime) once
/// exceeded.
pub const CACHE_MAX_BYTES: u64 = 32 * 1024 * 1024 * 1024;
/// When over budget, evict down to this fraction of the cap so eviction
/// doesn't re-trigger on every subsequent save.
const EVICT_TARGET_FRAC: f64 = 0.90;

/// Grid resolution: 1 / 0.005° — matches the move-trigger threshold in
/// `map_display::trigger()`.
const GRID_SCALE: f64 = 200.0;
/// Max Chebyshev grid distance `find_nearest` will search (~0.2° ≈ 22 km).
pub const MAX_SEARCH_RADIUS: i64 = 40;

/// Number of tile subdirectories (`/emmc/mapcache/xx/`), chosen by hash so
/// any single directory's entry count stays FAT/exFAT-friendly even with
/// millions of tiles overall.
const SHARD_COUNT: u64 = 256;

/// Fixed day-0 reference for eviction's histogram buckets (2024-01-01
/// 00:00:00 UTC, same epoch used elsewhere in this project e.g.
/// geocoder.rs's NTP-sync check). Avoids needing an extra scan pass just to
/// find the oldest mtime present.
const EPOCH_DAY0_SECS: u64 = 1_704_067_200;
const SECS_PER_DAY: u64 = 86_400;
const EVICT_BUCKETS: usize = 4096; // ~11 years of daily buckets

const ST_EMPTY: u8 = 0;
const ST_OCCUPIED: u8 = 1;
const ST_TOMBSTONE: u8 = 2;

#[derive(Clone, Copy)]
struct Record {
    state: u8,
    zoom: u8,
    sat: u8,
    glat: i32,
    glon: i32,
    size: u32,
    mtime: u32,
}

impl Record {
    fn empty() -> Self {
        Record { state: ST_EMPTY, zoom: 0, sat: 0, glat: 0, glon: 0, size: 0, mtime: 0 }
    }

    fn to_bytes(self) -> [u8; RECORD_SIZE as usize] {
        let mut b = [0u8; RECORD_SIZE as usize];
        b[0] = self.state;
        b[1] = self.zoom;
        b[2] = self.sat;
        b[4..8].copy_from_slice(&self.glat.to_le_bytes());
        b[8..12].copy_from_slice(&self.glon.to_le_bytes());
        b[12..16].copy_from_slice(&self.size.to_le_bytes());
        b[16..20].copy_from_slice(&self.mtime.to_le_bytes());
        b
    }

    fn from_bytes(b: &[u8]) -> Self {
        Record {
            state: b[0],
            zoom: b[1],
            sat: b[2],
            glat: i32::from_le_bytes(b[4..8].try_into().unwrap()),
            glon: i32::from_le_bytes(b[8..12].try_into().unwrap()),
            size: u32::from_le_bytes(b[12..16].try_into().unwrap()),
            mtime: u32::from_le_bytes(b[16..20].try_into().unwrap()),
        }
    }

    fn matches(&self, zoom: u8, sat: bool, glat: i32, glon: i32) -> bool {
        self.state == ST_OCCUPIED
            && self.zoom == zoom
            && self.sat == sat as u8
            && self.glat == glat
            && self.glon == glon
    }
}

fn hash_key(zoom: u8, sat: bool, glat: i32, glon: i32) -> u64 {
    let mut h: u64 = (zoom as u64)
        ^ ((sat as u64) << 8)
        ^ ((glat as u32 as u64) << 16)
        ^ ((glon as u32 as u64) << 48);
    // splitmix64 finalizer for good avalanche/bit mixing.
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58476d1ce4e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d049bb133111eb);
    h ^= h >> 31;
    h
}

/// Round (lat, lon) to the ~500 m grid cell used as the index key.
pub fn grid_key(lat: f64, lon: f64) -> (i32, i32) {
    ((lat * GRID_SCALE).round() as i32, (lon * GRID_SCALE).round() as i32)
}

/// Path of the tile file for a given key (sharded subdirectory + filename).
pub fn tile_path(zoom: u8, sat: bool, glat: i32, glon: i32) -> String {
    let shard = hash_key(zoom, sat, glat, glon) % SHARD_COUNT;
    format!(
        "{}/{:02x}/{}_{}_{}_{}.jpg",
        CACHE_DIR, shard, if sat { "s" } else { "r" }, zoom, glat, glon
    )
}

// ─── Shared state ───────────────────────────────────────────────────────────

struct IndexState {
    file: File,
    total_bytes: u64,
}

static STATE: LazyLock<Mutex<Option<IndexState>>> = LazyLock::new(|| Mutex::new(None));
static READY: AtomicBool = AtomicBool::new(false);

/// True once the index has been created/loaded and is safe to use. `false`
/// during the (background, one-time) setup — callers should treat that
/// simply as "no cache available yet", not an error.
pub fn is_ready() -> bool {
    READY.load(Ordering::Relaxed)
}

/// Kick off (on a background thread) creating the index file if it doesn't
/// exist yet — zero-filling all `CAPACITY` slots, a one-time ~80 MiB
/// sequential write — or, if it already exists, scanning it once to rebuild
/// the in-RAM running `total_bytes` counter. Call once at boot after the
/// eMMC is mounted.
///
/// Runs at a low FreeRTOS priority: the first-boot zero-fill is heavy
/// sustained eMMC I/O, and observed in practice to noticeably delay the
/// *main* task's own progress (WiFi bring-up, NTP, etc.) even though this
/// thread never blocks on anything main() touches — plain CPU/bus
/// contention. A low priority (well below WiFi's, and below the default
/// pthread priority) keeps this from competing with boot-critical work.
pub fn init() {
    if !crate::emmc::is_mounted() { return; }

    // Only `priority` is carried by the spawn configuration — ESP-IDF's
    // pthread_create overrides its stack_size with the attr that Rust's
    // thread::Builder always passes, so the size has to be set on the
    // Builder itself to take effect.
    let prev = esp_idf_hal::task::thread::ThreadSpawnConfiguration::get();
    let low_prio = esp_idf_hal::task::thread::ThreadSpawnConfiguration {
        priority: 3,
        ..Default::default()
    };
    if low_prio.set().is_err() {
        warn!("[MAPCACHE] failed to set low-priority thread config, using default");
    }

    let r = std::thread::Builder::new()
        .stack_size(8192)
        .spawn(|| match init_inner() {
            Ok(total_bytes) => {
                READY.store(true, Ordering::Relaxed);
                info!("[MAPCACHE] index ready ({} bytes cached)", total_bytes);
            }
            Err(e) => warn!("[MAPCACHE] index init failed: {:?}", e),
        });

    // Restore whatever spawn config was active before, so this doesn't leak
    // into unrelated std::thread::spawn() calls made later from this thread.
    let restore = prev.unwrap_or_default();
    let _ = restore.set();

    if let Err(e) = r {
        warn!("[MAPCACHE] failed to spawn index init thread: {:?}", e);
    }
}

/// Wipe the entire map tile cache (index file + every cached tile,
/// including any leftover from the old flat-directory layout) and start a
/// fresh empty index. Synchronous and can take a while on a heavily
/// populated cache (FAT/exFAT has no fast "truncate directory" operation,
/// each file has to be individually unlinked) — call from a background
/// thread when driving this from an HTTP handler.
pub fn clear() {
    READY.store(false, Ordering::Relaxed);
    *STATE.lock().unwrap() = None; // drop the open index file handle first
    match std::fs::remove_dir_all(CACHE_DIR) {
        Ok(()) => info!("[MAPCACHE] cache directory cleared"),
        Err(e) => warn!("[MAPCACHE] clear: remove_dir_all failed: {:?}", e),
    }
    init();
}

fn init_inner() -> std::io::Result<u64> {
    std::fs::create_dir_all(CACHE_DIR)?;
    let expected_len = CAPACITY * RECORD_SIZE;

    let mut file = OpenOptions::new().read(true).write(true).create(true).open(INDEX_PATH)?;
    let existing_len = file.metadata()?.len();

    let total_bytes = if existing_len != expected_len {
        // (Re)create from scratch: zero-fill every slot. FAT/exFAT don't
        // support sparse files — extending via seek+write leaves *undefined*
        // content, not zeros — and ST_EMPTY == 0 relies on genuinely-zeroed
        // bytes to correctly terminate probe searches, so every byte must
        // actually be written, not just the file length extended.
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        let zeros = vec![0u8; 64 * 1024];
        let mut written = 0u64;
        while written < expected_len {
            let n = (expected_len - written).min(zeros.len() as u64) as usize;
            file.write_all(&zeros[..n])?;
            written += n as u64;
            // Explicit yield between chunks (~1280 total): belt-and-suspenders
            // alongside this thread's low priority, so boot-critical work
            // (WiFi, NTP) reliably gets scheduled during this long write.
            std::thread::yield_now();
        }
        file.flush()?;
        0
    } else {
        sum_occupied_bytes(&mut file)?
    };

    *STATE.lock().unwrap() = Some(IndexState { file, total_bytes });
    Ok(total_bytes)
}

/// Sequentially scan the whole index, calling `f(record)` for every
/// occupied slot. Used for both the boot-time total_bytes rebuild and
/// eviction's histogram passes — bounded RAM regardless of entry count.
fn scan_occupied<F: FnMut(Record)>(file: &mut File, mut f: F) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(0))?;
    let mut buf = vec![0u8; (RECORD_SIZE as usize) * 4096]; // 80 KB chunks
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 { break; }
        let mut off = 0;
        while off + RECORD_SIZE as usize <= n {
            let rec = Record::from_bytes(&buf[off..off + RECORD_SIZE as usize]);
            if rec.state == ST_OCCUPIED {
                f(rec);
            }
            off += RECORD_SIZE as usize;
        }
    }
    Ok(())
}

fn sum_occupied_bytes(file: &mut File) -> std::io::Result<u64> {
    let mut total = 0u64;
    scan_occupied(file, |rec| total += rec.size as u64)?;
    Ok(total)
}

// ─── Slot probing (open addressing, linear probe, tombstone reuse) ───────────

fn read_record(file: &mut File, slot: u64) -> std::io::Result<Record> {
    file.seek(SeekFrom::Start(slot * RECORD_SIZE))?;
    let mut buf = [0u8; RECORD_SIZE as usize];
    file.read_exact(&mut buf)?;
    Ok(Record::from_bytes(&buf))
}

fn write_record(file: &mut File, slot: u64, rec: Record) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(slot * RECORD_SIZE))?;
    file.write_all(&rec.to_bytes())
}

/// Look up the exact (zoom, sat, glat, glon) key. Probing stops as soon as
/// a genuinely-empty (never-used) slot is hit, since insertion always fills
/// the first empty-or-tombstone slot on its probe path — so an empty slot
/// reliably means "not present" rather than "keep looking".
fn find_slot(file: &mut File, zoom: u8, sat: bool, glat: i32, glon: i32) -> std::io::Result<Option<u64>> {
    let start = hash_key(zoom, sat, glat, glon) % CAPACITY;
    for i in 0..MAX_PROBE.min(CAPACITY) {
        let slot = (start + i) % CAPACITY;
        let rec = read_record(file, slot)?;
        if rec.state == ST_EMPTY { return Ok(None); }
        if rec.matches(zoom, sat, glat, glon) { return Ok(Some(slot)); }
    }
    Ok(None)
}

/// Insert or update. Returns the previous record's size (0 if newly
/// inserted) so the caller can adjust the running `total_bytes` correctly.
fn upsert(file: &mut File, zoom: u8, sat: bool, glat: i32, glon: i32, size: u32, mtime: u32) -> std::io::Result<u32> {
    let start = hash_key(zoom, sat, glat, glon) % CAPACITY;
    let mut first_free: Option<u64> = None;
    let new_rec = Record { state: ST_OCCUPIED, zoom, sat: sat as u8, glat, glon, size, mtime };

    for i in 0..MAX_PROBE.min(CAPACITY) {
        let slot = (start + i) % CAPACITY;
        let rec = read_record(file, slot)?;
        if rec.matches(zoom, sat, glat, glon) {
            write_record(file, slot, new_rec)?;
            return Ok(rec.size);
        }
        if rec.state == ST_EMPTY {
            write_record(file, first_free.unwrap_or(slot), new_rec)?;
            return Ok(0);
        }
        if rec.state == ST_TOMBSTONE && first_free.is_none() {
            first_free = Some(slot);
        }
    }
    // Probe limit hit without finding a free slot (extremely unlikely given
    // CAPACITY sizing) — reuse a tombstone if we saw one, otherwise drop
    // the insert rather than corrupt state or loop forever.
    if let Some(slot) = first_free {
        write_record(file, slot, new_rec)?;
    }
    Ok(0)
}

// ─── Public API ─────────────────────────────────────────────────────────────

/// Record a tile that the caller has already written to `tile_path(...)`,
/// updating the running total and evicting old entries if now over budget.
pub fn record_saved(zoom: u8, sat: bool, glat: i32, glon: i32, size: u32) {
    if !is_ready() { return; }
    let mtime = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0);

    let (over, total) = {
        let mut guard = STATE.lock().unwrap();
        let Some(state) = guard.as_mut() else { return };
        let old_size = match upsert(&mut state.file, zoom, sat, glat, glon, size, mtime) {
            Ok(s) => s,
            Err(e) => { warn!("[MAPCACHE] index upsert failed: {:?}", e); return; }
        };
        state.total_bytes = state.total_bytes.saturating_sub(old_size as u64).saturating_add(size as u64);
        (state.total_bytes > CACHE_MAX_BYTES, state.total_bytes)
    };

    if over {
        info!("[MAPCACHE] over budget ({} bytes), evicting oldest entries", total);
        evict();
    }
}

/// Ring-expansion nearest-neighbour search: checks the exact cell first,
/// then each successively wider ring (Chebyshev distance) up to
/// `max_radius`, returning the tile path of the first hit found. Bounded to
/// O(cells checked) index lookups — each an O(1) seek — instead of a
/// directory scan.
pub fn find_nearest(zoom: u8, sat: bool, glat: i32, glon: i32, max_radius: i64) -> Option<String> {
    if !is_ready() { return None; }
    let mut guard = STATE.lock().unwrap();
    let state = guard.as_mut()?;

    for r in 0..=max_radius {
        for (dx, dy) in ring_offsets(r) {
            let (g2lat, g2lon) = (glat + dx as i32, glon + dy as i32);
            if let Ok(Some(_)) = find_slot(&mut state.file, zoom, sat, g2lat, g2lon) {
                return Some(tile_path(zoom, sat, g2lat, g2lon));
            }
        }
    }
    None
}

/// Offsets `(dx, dy)` forming the perimeter of the `(2r+1)x(2r+1)` square
/// centred on the origin, i.e. all cells at Chebyshev distance exactly `r`.
fn ring_offsets(r: i64) -> Vec<(i64, i64)> {
    if r == 0 { return vec![(0, 0)]; }
    let mut v = Vec::with_capacity((8 * r) as usize);
    for dx in -r..=r {
        v.push((dx, -r));
        v.push((dx, r));
    }
    for dy in (-r + 1)..r {
        v.push((-r, dy));
        v.push((r, dy));
    }
    v
}

/// Evict the oldest entries until back down to `EVICT_TARGET_FRAC` of the
/// cap. Uses a two-pass day-bucketed histogram (bounded ~32 KB of RAM
/// regardless of entry count) to find a cutoff mtime, then deletes every
/// entry at or before that day — avoids needing to sort millions of entries
/// in RAM just to find the oldest ones.
fn evict() {
    let mut guard = STATE.lock().unwrap();
    let Some(state) = guard.as_mut() else { return };

    let target_bytes = (CACHE_MAX_BYTES as f64 * EVICT_TARGET_FRAC) as u64;
    let need_to_evict = state.total_bytes.saturating_sub(target_bytes);
    if need_to_evict == 0 { return; }

    // Pass 1: histogram of bytes per day bucket.
    let mut hist = vec![0u64; EVICT_BUCKETS];
    let day_of = |mtime: u32| -> usize {
        (((mtime as u64).saturating_sub(EPOCH_DAY0_SECS)) / SECS_PER_DAY)
            .min((EVICT_BUCKETS - 1) as u64) as usize
    };
    if let Err(e) = scan_occupied(&mut state.file, |rec| hist[day_of(rec.mtime)] += rec.size as u64) {
        warn!("[MAPCACHE] evict: histogram scan failed: {:?}", e);
        return;
    }

    // Find the smallest cutoff day such that evicting everything at or
    // before it frees at least `need_to_evict` bytes.
    let mut acc = 0u64;
    let mut cutoff_day = EVICT_BUCKETS - 1;
    for (day, &bytes) in hist.iter().enumerate() {
        acc += bytes;
        if acc >= need_to_evict {
            cutoff_day = day;
            break;
        }
    }

    // Pass 2a: sequential scan collecting the slots to tombstone. Deleting
    // the tile file here is safe (a different file, doesn't touch the
    // index's read cursor), but writing the tombstone back into index.bin
    // is deferred to 2b — write_record() seeks internally, which would
    // otherwise corrupt this sequential read's position.
    let mut to_tombstone: Vec<(u64, u32)> = Vec::new(); // (slot, size)
    let file = &mut state.file;
    if let Err(e) = file.seek(SeekFrom::Start(0)) {
        warn!("[MAPCACHE] evict: seek failed: {:?}", e);
        return;
    }
    let mut buf = vec![0u8; (RECORD_SIZE as usize) * 4096];
    let mut slot: u64 = 0;
    loop {
        let n = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => { warn!("[MAPCACHE] evict: scan failed: {:?}", e); break; }
        };
        let mut off = 0;
        while off + RECORD_SIZE as usize <= n {
            let rec = Record::from_bytes(&buf[off..off + RECORD_SIZE as usize]);
            if rec.state == ST_OCCUPIED && day_of(rec.mtime) <= cutoff_day {
                let path = tile_path(rec.zoom, rec.sat != 0, rec.glat, rec.glon);
                let _ = std::fs::remove_file(&path);
                to_tombstone.push((slot, rec.size));
            }
            off += RECORD_SIZE as usize;
            slot += 1;
        }
    }

    // Pass 2b: now safe to seek freely — write the tombstones.
    let mut freed = 0u64;
    let mut removed = 0u32;
    for (slot, size) in to_tombstone {
        if write_record(file, slot, Record { state: ST_TOMBSTONE, ..Record::empty() }).is_ok() {
            freed += size as u64;
            removed += 1;
        }
    }

    state.total_bytes = state.total_bytes.saturating_sub(freed);
    info!("[MAPCACHE] evicted {} tiles, freed {} bytes (now {} bytes)", removed, freed, state.total_bytes);
}
