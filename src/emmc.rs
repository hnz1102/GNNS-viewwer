// eMMC storage — 8-bit SDMMC, mounted as exFAT/FAT32 at /emmc via esp_vfs_fat_sdmmc_mount.
//
// Ported from the eMMC Flash Manager reference project (../emmc), which pins the
// same ESP-IDF version (v5.4.2) so the sdmmc_host_t struct layout matches exactly.
//
// Once mounted, files under /emmc are reachable through ordinary std::fs calls.
// Used by map_display (offline map tile cache) and track_log (GPS trail log).
//
// exFAT support (FF_FS_EXFAT) is not exposed via ESP-IDF Kconfig in this version;
// it is patched directly into components/fatfs/src/ffconf.h by setup_components.sh.
//
// SPDX-License-Identifier: MIT

use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

// ─── eMMC pin configuration (8-bit bus) ────────────────────────────────────────

const SDMMC_FREQ_KHZ: i32 = 40000; // 40 MHz — conservative default for eMMC over long traces
const SLOT_WIDTH: u8 = 8;
const PIN_CLK: i32 = 46;
const PIN_CMD: i32 = 3;
const PIN_D0: i32 = 47;
const PIN_D1: i32 = 21;
const PIN_D2: i32 = 10;
const PIN_D3: i32 = 9;
const PIN_D4: i32 = 17;
const PIN_D5: i32 = 18;
const PIN_D6: i32 = 8;
const PIN_D7: i32 = 7;

const SDMMC_HOST_FLAG_8BIT: u32 = 1 << 2;

/// VFS mount point, null-terminated for the C API.
const MOUNT_POINT_C: &[u8] = b"/emmc\0";
/// VFS mount point as a plain path, for std::fs use.
pub const MOUNT_POINT: &str = "/emmc";

static MOUNTED: AtomicBool = AtomicBool::new(false);
/// Card handle from the successful mount, kept so `format()` can reformat
/// in place via `esp_vfs_fat_sdcard_format` without re-touching the SDMMC
/// host/slot config (that init/deinit dance is what caused the earlier
/// boot-crash — reformatting an already-mounted card avoids it entirely).
static CARD: AtomicPtr<esp_idf_sys::sdmmc_card_t> = AtomicPtr::new(std::ptr::null_mut());

/// Returns true once `mount()` has completed successfully.
pub fn is_mounted() -> bool {
    MOUNTED.load(Ordering::Relaxed)
}

fn build_host() -> esp_idf_sys::sdmmc_host_t {
    esp_idf_sys::sdmmc_host_t {
        flags: SDMMC_HOST_FLAG_8BIT,
        slot: 0,
        max_freq_khz: SDMMC_FREQ_KHZ,
        io_voltage: 3.3,
        driver_strength: esp_idf_sys::sdmmc_driver_strength_t_SDMMC_DRIVER_STRENGTH_B,
        current_limit: esp_idf_sys::sdmmc_current_limit_t_SDMMC_CURRENT_LIMIT_200MA,
        init: Some(esp_idf_sys::sdmmc_host_init),
        set_bus_width: Some(esp_idf_sys::sdmmc_host_set_bus_width),
        get_bus_width: Some(esp_idf_sys::sdmmc_host_get_slot_width),
        set_bus_ddr_mode: Some(esp_idf_sys::sdmmc_host_set_bus_ddr_mode),
        set_card_clk: Some(esp_idf_sys::sdmmc_host_set_card_clk),
        set_cclk_always_on: Some(esp_idf_sys::sdmmc_host_set_cclk_always_on),
        do_transaction: Some(esp_idf_sys::sdmmc_host_do_transaction),
        io_int_enable: Some(esp_idf_sys::sdmmc_host_io_int_enable),
        io_int_wait: Some(esp_idf_sys::sdmmc_host_io_int_wait),
        command_timeout_ms: 30000,
        get_real_freq: Some(esp_idf_sys::sdmmc_host_get_real_freq),
        __bindgen_anon_1: esp_idf_sys::sdmmc_host_t__bindgen_ty_1 {
            deinit: Some(esp_idf_sys::sdmmc_host_deinit),
        },
        input_delay_phase: 0,
        set_input_delay: None,
        dma_aligned_buffer: std::ptr::null_mut(),
        pwr_ctrl_handle: std::ptr::null_mut(),
        get_dma_info: Some(esp_idf_sys::sdmmc_host_get_dma_info),
        is_slot_set_to_uhs1: Some(esp_idf_sys::sdmmc_host_is_slot_set_to_uhs1),
    }
}

fn build_slot_config() -> esp_idf_sys::sdmmc_slot_config_t {
    esp_idf_sys::sdmmc_slot_config_t {
        width: SLOT_WIDTH,
        clk: PIN_CLK,
        cmd: PIN_CMD,
        d0: PIN_D0,
        d1: PIN_D1,
        d2: PIN_D2,
        d3: PIN_D3,
        d4: PIN_D4,
        d5: PIN_D5,
        d6: PIN_D6,
        d7: PIN_D7,
        __bindgen_anon_1: esp_idf_sys::sdmmc_slot_config_t__bindgen_ty_1 { gpio_cd: -1 },
        __bindgen_anon_2: esp_idf_sys::sdmmc_slot_config_t__bindgen_ty_2 { gpio_wp: -1 },
        flags: 1, // SDMMC_SLOT_FLAG_INTERNAL_PULLUP
    }
}

/// Mount the eMMC card as a FAT/exFAT filesystem at `/emmc`.
///
/// Auto-formats the card only when it has no recognisable FAT/exFAT/MBR
/// signature at all (ESP-IDF's `f_mount` returns `FR_NO_FILESYSTEM` /
/// `FR_INT_ERR`) — never on other errors (disk I/O errors, timeouts, etc),
/// so a card with real data is not touched just because of a transient bus
/// error. This also matters operationally: on this IDF version, leaving a
/// genuinely blank card unformatted exercises `esp_vfs_fat_sdmmc_mount`'s
/// failure/cleanup path, which was observed to leave the SDMMC host in a
/// state that crashes the WiFi task ~10s later (reproduced: disabling the
/// mount() call removed the crash; root cause not fully bisected further,
/// but auto-formatting on first boot avoids ever hitting that path).
pub fn mount() -> bool {
    let host = build_host();
    let slot_config = build_slot_config();

    let mount_config = esp_idf_sys::esp_vfs_fat_sdmmc_mount_config_t {
        format_if_mount_failed: true,
        max_files: 6,
        allocation_unit_size: 32 * 1024,
        disk_status_check_enable: false,
        use_one_fat: false,
    };

    let mut card: *mut esp_idf_sys::sdmmc_card_t = std::ptr::null_mut();
    let rc = unsafe {
        esp_idf_sys::esp_vfs_fat_sdmmc_mount(
            MOUNT_POINT_C.as_ptr() as *const _,
            &host,
            &slot_config as *const _ as *const core::ffi::c_void,
            &mount_config,
            &mut card,
        )
    };

    if rc != esp_idf_sys::ESP_OK as i32 {
        // `mount()` runs before the Rust `log` crate backend (EspLogger/syslog)
        // is installed, so info!()/warn!() here are silently dropped — use
        // println!() (raw UART, always active) so this is actually visible.
        println!("[eMMC] mount failed: 0x{:x} (is the card present and FAT/exFAT formatted?)", rc);
        return false;
    }

    // Log capacity for visibility in the boot log (see println!() note above).
    unsafe {
        if !card.is_null() {
            let sectors = (*card).csd.capacity as u64;
            let sector_size = (*card).csd.sector_size as u64;
            let gb = (sectors * sector_size) as f64 / (1024.0 * 1024.0 * 1024.0);
            println!("[eMMC] mounted at {} ({:.2} GiB, 8-bit @ {} kHz)", MOUNT_POINT, gb, SDMMC_FREQ_KHZ);
        } else {
            println!("[eMMC] mounted at {}", MOUNT_POINT);
        }
    }

    CARD.store(card, Ordering::Relaxed);
    MOUNTED.store(true, Ordering::Relaxed);
    true
}

/// Reformat the card in place (wipes everything) and mount it back, using
/// the existing card handle from `mount()` — does not touch the SDMMC
/// host/slot config. Must only be called after a successful `mount()`.
///
/// Blocks for the duration of the format (writing new FAT/exFAT tables) —
/// callers driving this from an HTTP handler etc should run it on a
/// background thread rather than await it inline.
pub fn format() -> bool {
    let card = CARD.load(Ordering::Relaxed);
    if card.is_null() {
        println!("[eMMC] format: not mounted");
        return false;
    }
    let rc = unsafe {
        esp_idf_sys::esp_vfs_fat_sdcard_format(MOUNT_POINT_C.as_ptr() as *const _, card)
    };
    if rc != esp_idf_sys::ESP_OK as i32 {
        println!("[eMMC] format failed: 0x{:x}", rc);
        return false;
    }
    println!("[eMMC] format complete, remounted at {}", MOUNT_POINT);
    true
}
