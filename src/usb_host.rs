//! Serial bridge module.
//!
//! Owns the COM1 UART (used for the u-blox GPS receiver) and the SSH bridge
//! that exposes it over `ssh <host> console com1`.  Received data is mirrored
//! to the on-device display and to browser WebSocket terminals, and forwarded
//! via `log::info!()` (→ syslog when enabled).

use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use esp_idf_sys::*;
use log::{info, warn};
use std::thread;
use std::time::Duration;
use crate::gpio_ctrl::GpioPwmState;

// ─── Shared state ───────────────────────────────────────────────────────────

/// Guard: ssh_bridge has already been initialised.
static SSH_BRIDGE_STARTED: AtomicBool = AtomicBool::new(false);
/// Shared GPIO state — written by SSH callbacks, read by main loop.
static GPIO_PWM_STATE: std::sync::Mutex<Option<GpioPwmState>> = std::sync::Mutex::new(None);
/// COM1 display RX buffer — always fed regardless of which page is shown.
static COM1_DISP_BUF: std::sync::Mutex<Option<crate::serial_display::SerialRxBuffer>> = std::sync::Mutex::new(None);

// ─── WebSocket sender registry (for the browser serial terminal) ────────────
use esp_idf_svc::http::server::ws::EspHttpWsDetachedSender;
use embedded_svc::ws::FrameType;
use std::collections::{HashSet, VecDeque};

/// List of WS senders for the COM1 terminal.  Each entry is (session_fd, sender).
/// Senders whose `is_closed()` returns true are pruned on each broadcast.
pub static WS_SENDERS_COM1: std::sync::Mutex<Vec<(i32, EspHttpWsDetachedSender)>> = std::sync::Mutex::new(Vec::new());

/// Set of fds currently subscribed to the COM1 terminal, tracked separately
/// from WS_SENDERS_COM1 so the httpd handler thread can look up a fd's port
/// without risking a deadlock against the ws_send thread (which holds
/// WS_SENDERS_COM1 while calling sender.send(), which may itself block on httpd).
static WS_FD_SET: std::sync::LazyLock<std::sync::Mutex<HashSet<i32>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashSet::new()));

/// Ring buffer for COM1 WS data (RX path writes here, sender thread drains).
const WS_RING_CAP: usize = 65536;
static WS_RING_COM1: std::sync::Mutex<VecDeque<u8>> = std::sync::Mutex::new(VecDeque::new());

/// Register a WebSocket sender for the COM1 terminal.
pub fn ws_register_sender(port_name: &str, fd: i32, sender: EspHttpWsDetachedSender) {
    if port_name != "com1" { return; }
    WS_SENDERS_COM1.lock().unwrap().push((fd, sender));
    if let Ok(mut set) = WS_FD_SET.lock() {
        set.insert(fd);
    }
    info!("[WS] registered sender fd={} for port {}", fd, port_name);
}

/// Remove a WebSocket sender for the COM1 terminal.
pub fn ws_remove_sender(port_name: &str, fd: i32) {
    if port_name != "com1" { return; }
    WS_SENDERS_COM1.lock().unwrap().retain(|(f, _)| *f != fd);
    if let Ok(mut set) = WS_FD_SET.lock() {
        set.remove(&fd);
    }
    info!("[WS] removed sender fd={} for port {}", fd, port_name);
}

/// Return the port name for a WebSocket fd ("com1" or "").
/// Uses the lock-free WS_FD_SET so the httpd handler thread can call this
/// without risk of deadlocking against ws_send thread which holds WS_SENDERS_COM1.
pub fn ws_port_for_fd(fd: i32) -> &'static str {
    if let Ok(set) = WS_FD_SET.lock() {
        if set.contains(&fd) {
            return "com1";
        }
    }
    ""
}

/// Queue serial RX data for WebSocket broadcast (non-blocking).
/// Called from uart_rx_thread — must never block on httpd.
fn ws_enqueue(data: &[u8]) {
    if let Ok(mut ring) = WS_RING_COM1.try_lock() {
        for &b in data {
            if ring.len() >= WS_RING_CAP {
                // Drop oldest bytes on overflow
                ring.pop_front();
            }
            ring.push_back(b);
        }
    }
    // If lock is contended, silently drop — sender thread holds it briefly.
}

/// Drain the COM1 ring buffer and broadcast to its WS senders.
/// Called from the dedicated ws_sender_thread only.
fn ws_drain_and_send() {
    // Drain ring buffer first (short lock)
    let data: Vec<u8> = {
        if let Ok(mut ring) = WS_RING_COM1.try_lock() {
            let v: Vec<u8> = ring.drain(..).collect();
            v
        } else {
            return;
        }
    };
    if data.is_empty() {
        return;
    }
    // Send to WS clients (may block on httpd — that's fine here, we're on our own thread)
    let mut list = WS_SENDERS_COM1.lock().unwrap();
    list.retain_mut(|(fd, sender): &mut (i32, EspHttpWsDetachedSender)| {
        if sender.is_closed() {
            info!("[WS] pruning closed sender fd={}", fd);
            return false;
        }
        if let Err(e) = sender.send(FrameType::Binary(false), &data) {
            warn!("[WS] send error fd={}: {:?}", fd, e);
            return false;
        }
        true
    });
}

/// Background thread: drains the COM1 WS ring buffer and sends to clients.
/// Also drains the TX queue (browser→serial). Runs ~200 Hz so latency is ≤5 ms.
pub fn start_ws_sender_thread() {
    thread::Builder::new()
        .name("ws_send".into())
        .stack_size(6144)
        .spawn(move || {
            loop {
                ws_drain_and_send();
                ws_drain_tx();
                thread::sleep(Duration::from_millis(5));
            }
        })
        .expect("spawn ws_sender thread");
}

/// TX queue for WebSocket→serial direction.
/// The httpd thread enqueues data here (non-blocking); the ws_sender_thread drains it.
static WS_TX_COM1: std::sync::Mutex<VecDeque<u8>> = std::sync::Mutex::new(VecDeque::new());

/// Write data to COM1 (for WebSocket TX direction: browser → serial).
/// Non-blocking: enqueues data for the ws_sender_thread to drain.
pub fn serial_write(port_name: &str, data: &[u8]) {
    if port_name != "com1" { return; }
    if let Ok(mut q) = WS_TX_COM1.try_lock() {
        for &b in data {
            if q.len() >= WS_RING_CAP {
                q.pop_front();
            }
            q.push_back(b);
        }
    }
}

/// Actually write queued TX data to COM1. Called from ws_sender_thread.
fn ws_drain_tx() {
    let data: Vec<u8> = {
        if let Ok(mut q) = WS_TX_COM1.try_lock() {
            q.drain(..).collect()
        } else {
            return;
        }
    };
    if data.is_empty() {
        return;
    }
    if UART1_READY.load(Ordering::Relaxed) {
        unsafe { uart_write_bytes(1, data.as_ptr() as *const c_void, data.len()); }
    }
}

/// Register the COM1 SerialRxBuffer for the display.
pub fn set_display_rx_bufs(com1: crate::serial_display::SerialRxBuffer) {
    *COM1_DISP_BUF.lock().unwrap() = Some(com1);
}

/// Set which port the display should monitor (also switches display page).
pub fn set_display_port(name: &str) {
    if name == "com1" {
        crate::serial_display::set_page(crate::serial_display::PAGE_COM1);
    }
}

/// Apply a new TIMEPULSE1 output frequency to the u-blox MAX-F10S over UART1
/// (COM1) immediately, without requiring a reboot.
pub fn apply_timepulse_freq(freq_hz: u32) {
    crate::gps_parser::set_timepulse_freq_hz(freq_hz);
    if UART1_READY.load(Ordering::Relaxed) {
        let msg = crate::gps_parser::build_ubx_timepulse_freq(freq_hz);
        unsafe {
            uart_write_bytes(1, msg.as_ptr() as *const c_void, msg.len());
        }
        log::info!("[GPS] UBX-CFG-VALSET TIMEPULSE1 sent ({} bytes, {} Hz)", msg.len(), freq_hz);
    }
}

/// Get the name of the port currently shown on the display.
pub fn display_port_name() -> &'static str {
    "com1"
}

// ─── SSH bridge FFI ───────────────────────────────────────────────────────────

extern "C" {
    fn ssh_bridge_init(port: u16,
                       username: *const core::ffi::c_char,
                       password: *const core::ffi::c_char) -> i32;
    fn ssh_bridge_serial_rx(data: *const u8, len: usize);
}

// ─── C→Rust logging (so C logs appear in syslog) ─────────────────────────────

#[no_mangle]
pub extern "C" fn ssh_log_info(msg: *const core::ffi::c_char) {
    let s = unsafe { std::ffi::CStr::from_ptr(msg) }.to_str().unwrap_or("?");
    log::info!("[SSH_BRIDGE] {}", s);
}

#[no_mangle]
pub extern "C" fn ssh_log_warn(msg: *const core::ffi::c_char) {
    let s = unsafe { std::ffi::CStr::from_ptr(msg) }.to_str().unwrap_or("?");
    log::warn!("[SSH_BRIDGE] {}", s);
}

/// Log hex dump of data direction + content (called from C bridge loop).
#[no_mangle]
pub extern "C" fn ssh_log_hex(dir: *const core::ffi::c_char, data: *const u8, len: usize) {
    // let d = unsafe { std::ffi::CStr::from_ptr(dir) }.to_str().unwrap_or("?");
    // let slice = unsafe { core::slice::from_raw_parts(data, len.min(32)) };
    // let hex: String = slice.iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(" ");
    // log::info!("[BRIDGE] {} len={} [{}]", d, len, hex);
    let _ = (dir, data, len);
}

// ─── Device constants and active device tracking ──────────────────────────────

const DEVICE_NONE: u8 = 0;
const DEVICE_COM1: u8 = 2;

/// Which serial device is currently bridged to the active SSH session.
static ACTIVE_DEVICE: AtomicU8 = AtomicU8::new(DEVICE_NONE);

/// Last device selection error reason (NUL-terminated C string in static storage).
static LAST_SELECT_ERR: std::sync::Mutex<&'static [u8]> = std::sync::Mutex::new(b"Unknown device\0");

/// Track whether the UART port was successfully initialised.
static UART1_READY: AtomicBool = AtomicBool::new(false);

// ─── Device write callbacks (SSH client → serial device TX) ──────────────────

/// SSH client → UART1 TX.
extern "C" fn com1_write_cb(data: *const u8, len: usize) {
    // log::info!("[TX] com1 {} bytes", len);
    unsafe {
        uart_write_bytes(1, data as *const core::ffi::c_void, len);
    }
}

// ─── Device selection (called from ssh_bridge.c) ─────────────────────────────

/// Activate the requested serial device for the current SSH session.
/// Returns the device's TX write callback, or NULL if the name is unknown.
#[no_mangle]
pub extern "C" fn ssh_select_device(
    name: *const core::ffi::c_char,
) -> Option<extern "C" fn(*const u8, usize)> {
    let name_str = unsafe { std::ffi::CStr::from_ptr(name) }
        .to_str()
        .unwrap_or("");
    match name_str {
        "com1" => {
            if UART1_READY.load(Ordering::Relaxed) {
                ACTIVE_DEVICE.store(DEVICE_COM1, Ordering::SeqCst);
                log::info!("[SSH] device selected: com1 (UART1)");
                Some(com1_write_cb)
            } else {
                log::warn!("[SSH] com1 requested but UART1 not initialised");
                *LAST_SELECT_ERR.lock().unwrap() = b"com1 (UART1) not initialised\0";
                None
            }
        }
        other => {
            log::warn!("[SSH] unknown device: '{}'", other);
            *LAST_SELECT_ERR.lock().unwrap() = b"Unknown device. Available: com1\0";
            None
        }
    }
}

/// Deactivate the current serial device when the SSH session ends.
#[no_mangle]
pub extern "C" fn ssh_release_device(_name: *const core::ffi::c_char) {
    ACTIVE_DEVICE.store(DEVICE_NONE, Ordering::SeqCst);
    log::info!("[SSH] device released");
}

/// Set one GPIO output from SSH command.
/// `idx` is 0-based: 0-5 = GPIO4-9 (power 1-6), `on_off` 1=ON 0=OFF.
/// Returns 0 on success, -1 if no state is registered.
#[no_mangle]
pub extern "C" fn ssh_gpio_set(idx: i32, on_off: i32) -> i32 {
    if idx < 0 || idx > 5 {
        log::warn!("[SSH] ssh_gpio_set: idx {} out of range", idx);
        return -1;
    }
    let guard = GPIO_PWM_STATE.lock().unwrap();
    if let Some(ref state) = *guard {
        state.set_gpio(idx as usize, on_off != 0);
        let pin = crate::gpio_ctrl::GPIO_PIN_MAP[idx as usize];
        log::info!("[SSH] GPIO{} (power {}) -> {}",
            pin, idx + 1, if on_off != 0 { "ON" } else { "OFF" });
        0
    } else {
        log::warn!("[SSH] ssh_gpio_set: GPIO/PWM state not initialised");
        -1
    }
}

/// Return the last device‐selection error as a C string pointer.
#[no_mangle]
pub extern "C" fn ssh_device_error() -> *const u8 {
    let err = LAST_SELECT_ERR.lock().unwrap();
    err.as_ptr()
}

// ─── UART driver helpers ──────────────────────────────────────────────────────

/// Initialise a UART port with 8N1 framing and the given baud rate / GPIO pins.
/// Returns `true` on success.
unsafe fn uart_init(port_num: i32, tx_pin: i32, rx_pin: i32, baud_rate: u32) -> bool {
    let cfg = uart_config_t {
        baud_rate: baud_rate as i32,
        data_bits: uart_word_length_t_UART_DATA_8_BITS,
        parity:    uart_parity_t_UART_PARITY_DISABLE,
        stop_bits: uart_stop_bits_t_UART_STOP_BITS_1,
        flow_ctrl: uart_hw_flowcontrol_t_UART_HW_FLOWCTRL_DISABLE,
        rx_flow_ctrl_thresh: 0,
        ..core::mem::zeroed()
    };
    let rc = uart_param_config(port_num as u32, &cfg);
    if rc != 0 {
        log::warn!("[UART{}] uart_param_config failed: {}", port_num, rc);
        return false;
    }
    // -1 = UART_PIN_NO_CHANGE; no RTS/CTS needed
    let rc = uart_set_pin(port_num as u32, tx_pin, rx_pin, -1, -1);
    if rc != 0 {
        log::warn!("[UART{}] uart_set_pin failed: {}", port_num, rc);
        return false;
    }
    // Enable internal pull-up on TX and RX lines so they idle high when
    // no external device is connected (prevents noise / framing errors).
    gpio_pullup_en(tx_pin);
    gpio_pullup_en(rx_pin);
    // RX buffer 8192 B, no TX buffer (direct write), no event queue
    let rc = uart_driver_install(port_num as u32, 8192, 0, 0, core::ptr::null_mut(), 0);
    if rc != 0 {
        log::warn!("[UART{}] uart_driver_install failed: {}", port_num, rc);
        return false;
    }
    log::info!("[UART{}] init ok  tx={} rx={} baud={}", port_num, tx_pin, rx_pin, baud_rate);
    true
}

/// UART RX forwarding thread.
///
/// Reads from `port_num` with a 10-tick timeout.  When `ACTIVE_DEVICE ==
/// device_id`, the received bytes are forwarded to the SSH bridge ring buffer.
fn uart_rx_thread(port_num: i32, device_id: u8) {
    let mut buf = [0u8; 1024];
    loop {
        let n = unsafe {
            uart_read_bytes(
                port_num as u32,
                buf.as_mut_ptr() as *mut core::ffi::c_void,
                buf.len() as u32,
                1,  // ticks; ~10 ms at 100 Hz FreeRTOS tick rate
            )
        };
        if n > 0 {
            // Forward to SSH bridge only when this is the active SSH device
            if ACTIVE_DEVICE.load(Ordering::Relaxed) == device_id {
                // log::info!("[RX] uart{} {} bytes", port_num, n);
                unsafe { ssh_bridge_serial_rx(buf.as_ptr(), n as usize); }
            }
            // Mirror to display buffer (always, regardless of page)
            if let Some(ref disp_buf) = *COM1_DISP_BUF.lock().unwrap() {
                disp_buf.push_data(&buf[..n as usize]);
            }
            // Feed COM1 data to the NMEA GPS parser
            if device_id == DEVICE_COM1 {
                crate::gps_parser::feed(&buf[..n as usize]);
                // Trigger reverse geocoding when a valid fix is available
                let d = crate::gps_parser::get_data();
                if d.fix_quality > 0 && !d.lat_dd.is_nan() {
                    crate::geocoder::trigger(d.lat_dd, d.lon_dd);
                    crate::track_log::trigger(&d);
                }
            }
            // Broadcast to WebSocket terminals
            ws_enqueue(&buf[..n as usize]);
        }
    }
}

// ─── Public status accessors ─────────────────────────────────────────────────

/// Returns the name of the currently active bridged device.
pub fn active_device_name() -> &'static str {
    match ACTIVE_DEVICE.load(Ordering::Relaxed) {
        DEVICE_COM1 => "com1",
        _           => "none",
    }
}

// ─── Public entry point ───────────────────────────────────────────────────────

pub fn start(
    username: &'static str,
    password: &'static str,
    com1_tx: i32, com1_rx: i32, com1_baud: u32,
    gpio_pwm: GpioPwmState,
) {
    // Store GPIO/PWM state so SSH callbacks can reach it.
    *GPIO_PWM_STATE.lock().unwrap() = Some(gpio_pwm);
    // Initialise UART1 and spawn its RX forwarding thread.
    if unsafe { uart_init(1, com1_tx, com1_rx, com1_baud) } {
        UART1_READY.store(true, Ordering::Relaxed);
        thread::Builder::new()
            .name("uart1_rx".into())
            .stack_size(8192)
            .spawn(move || uart_rx_thread(1, DEVICE_COM1))
            .expect("spawn uart1_rx");
        // Send UBX-CFG-VALSET to enable QZSS and set the TIMEPULSE1 frequency
        // on the u-blox MAX-F10S. Sent 1 s after init to let the module
        // finish its startup sequence.
        let mon_ver_msg = crate::gps_parser::build_ubx_mon_ver_poll();
        let qzss_msg = crate::gps_parser::build_ubx_qzss_enable();
        let tp_msg = crate::gps_parser::build_ubx_timepulse_freq(crate::gps_parser::get_timepulse_freq_hz());
        thread::Builder::new()
            .name("gps_cfg".into())
            .stack_size(4096)
            .spawn(move || {
                thread::sleep(Duration::from_millis(1000));
                // Query firmware/protocol version first — diagnostic aid to
                // determine whether this module actually supports UBX-CFG-VALSET.
                unsafe {
                    uart_write_bytes(
                        1,
                        mon_ver_msg.as_ptr() as *const core::ffi::c_void,
                        mon_ver_msg.len(),
                    );
                }
                log::info!("[GPS] UBX-MON-VER poll sent ({} bytes)", mon_ver_msg.len());
                thread::sleep(Duration::from_millis(500));
                unsafe {
                    uart_write_bytes(
                        1,
                        qzss_msg.as_ptr() as *const core::ffi::c_void,
                        qzss_msg.len(),
                    );
                }
                log::info!("[GPS] UBX-CFG-VALSET QZSS_ENA sent ({} bytes)", qzss_msg.len());
                // Wait for the QZSS ACK/NAK to arrive before sending the next
                // VALSET, so the two responses are distinguishable in the log.
                thread::sleep(Duration::from_millis(500));
                unsafe {
                    uart_write_bytes(
                        1,
                        tp_msg.as_ptr() as *const core::ffi::c_void,
                        tp_msg.len(),
                    );
                }
                log::info!("[GPS] UBX-CFG-VALSET TIMEPULSE1 sent ({} bytes)", tp_msg.len());
            })
            .expect("spawn gps_cfg");
    }

    // Initialise the SSH bridge (listening on port 22).
    if !SSH_BRIDGE_STARTED.swap(true, Ordering::SeqCst) {
        log::info!("[SSH] Initialising SSH bridge...");
        let user_cstr = std::ffi::CString::new(username)
            .unwrap_or_else(|_| std::ffi::CString::new("admin").unwrap());
        let pass_cstr = std::ffi::CString::new(password)
            .unwrap_or_else(|_| std::ffi::CString::new("esp32").unwrap());
        unsafe {
            let rc = ssh_bridge_init(22, user_cstr.as_ptr(), pass_cstr.as_ptr());
            log::info!("[SSH] SSH bridge init returned rc={}", rc);
        }
        // The C code stores the raw pointer; keep the memory alive forever.
        std::mem::forget(user_cstr);
        std::mem::forget(pass_cstr);
    }
}
