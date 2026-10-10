// HTTP Config Server for SSH-Serial-Bridge
// Web UI for device configuration with NVS persistence
// SPDX-License-Identifier: MIT
// Copyright (c) 2025 Hiroshi Nakajima

use log::*;
use std::sync::{Arc, Mutex};
use std::collections::HashMap;
use esp_idf_svc::io::{Read, Write};
use embedded_svc::http::Headers;
use esp_idf_svc::http::server::{Configuration, EspHttpServer};
use esp_idf_svc::http::Method;
use esp_idf_sys::*;
use std::ffi::CString;

// ─── NVS namespace ────────────────────────────────────────────────────────────

const NVS_NS: &str = "usbotg_cfg";

// ─── Config struct ────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct NvsConfig {
    pub wifi_ssid:     String,
    pub wifi_psk:      String,
    pub ip_mode:       String,
    pub ip_address:    String,
    pub subnet_mask:   String,
    pub gateway:       String,
    pub dns:           String,
    pub syslog_server: String,
    pub syslog_enable: String,
    pub syslog_host_name: String,
    pub syslog_app_name: String,
    pub ssh_user:      String,
    pub ssh_password:  String,
    pub com1_tx_pin:   String,
    pub com1_rx_pin:   String,
    pub com1_baud:     String,
    pub display_enable: String,
    pub display_port:  String,
    pub emmc_enable:   String,
    pub wps_enable:    String,
    pub ntp_server1:   String,
    pub ntp_server2:   String,
    pub ntp_server3:   String,
    pub ntp_server4:   String,
    pub map_type:      String,
    pub timepulse_freq: String,
    pub maps_api_key:  String,
    pub quake_enable:  String,
}

// ─── NVS helpers ─────────────────────────────────────────────────────────────

fn nvs_read(key: &str) -> Option<String> {
    let ns = CString::new(NVS_NS).ok()?;
    let k  = CString::new(key).ok()?;
    unsafe {
        let mut handle: nvs_handle_t = 0;
        if nvs_open(ns.as_ptr(), nvs_open_mode_t_NVS_READONLY, &mut handle) != ESP_OK {
            return None;
        }
        // Get required length (pass null for out_value)
        let mut len: usize = 0;
        let ret = nvs_get_str(handle, k.as_ptr(), core::ptr::null_mut::<u8>(), &mut len);
        if ret != ESP_OK || len == 0 {
            nvs_close(handle);
            return None;
        }
        let mut buf = vec![0u8; len];
        let ret = nvs_get_str(handle, k.as_ptr(), buf.as_mut_ptr() as *mut u8, &mut len);
        nvs_close(handle);
        if ret != ESP_OK {
            return None;
        }
        while buf.last() == Some(&0) {
            buf.pop();
        }
        String::from_utf8(buf).ok()
    }
}

fn nvs_write_all(cfg: &NvsConfig) -> bool {
    let ns = match CString::new(NVS_NS) {
        Ok(s) => s,
        Err(_) => return false,
    };
    unsafe {
        let mut handle: nvs_handle_t = 0;
        if nvs_open(ns.as_ptr(), nvs_open_mode_t_NVS_READWRITE, &mut handle) != ESP_OK {
            return false;
        }
        let pairs: &[(&str, &str)] = &[
            ("wifi_ssid",     &cfg.wifi_ssid),
            ("wifi_psk",      &cfg.wifi_psk),
            ("ip_mode",       &cfg.ip_mode),
            ("ip_address",    &cfg.ip_address),
            ("subnet_mask",   &cfg.subnet_mask),
            ("gateway",       &cfg.gateway),
            ("dns",           &cfg.dns),
            ("syslog_server", &cfg.syslog_server),
            ("syslog_enable", &cfg.syslog_enable),
            ("syslog_hostname",    &cfg.syslog_host_name),
            ("syslog_app_name", &cfg.syslog_app_name),
            ("ssh_user",      &cfg.ssh_user),
            ("ssh_password",  &cfg.ssh_password),
            ("com1_tx_pin",   &cfg.com1_tx_pin),
            ("com1_rx_pin",   &cfg.com1_rx_pin),
            ("com1_baud",     &cfg.com1_baud),
            ("display_enable", &cfg.display_enable),
            ("display_port",  &cfg.display_port),
            ("wps_enable",    &cfg.wps_enable),
            ("ntp_server1",   &cfg.ntp_server1),
            ("ntp_server2",   &cfg.ntp_server2),
            ("ntp_server3",   &cfg.ntp_server3),
            ("ntp_server4",   &cfg.ntp_server4),
            ("map_type",      &cfg.map_type),
            ("timepulse_freq", &cfg.timepulse_freq),
            ("maps_api_key", &cfg.maps_api_key),
        ];
        let mut ok = true;
        for (key, val) in pairs {
            match (CString::new(*key), CString::new(*val)) {
                (Ok(k), Ok(v)) => {
                    if nvs_set_str(handle, k.as_ptr(), v.as_ptr()) != ESP_OK {
                        warn!("NVS: failed to write key '{}'", key);
                        ok = false;
                    }
                }
                _ => {
                    warn!("NVS: CString conversion failed for key '{}'", key);
                    ok = false;
                }
            }
        }
        nvs_commit(handle);
        nvs_close(handle);
        ok
    }
}

fn nvs_erase_namespace() -> bool {
    let ns = match CString::new(NVS_NS) {
        Ok(s) => s,
        Err(_) => return false,
    };
    unsafe {
        let mut handle: nvs_handle_t = 0;
        if nvs_open(ns.as_ptr(), nvs_open_mode_t_NVS_READWRITE, &mut handle) != ESP_OK {
            return false;
        }
        let ok = nvs_erase_all(handle) == ESP_OK;
        nvs_commit(handle);
        nvs_close(handle);
        ok
    }
}

/// Write only wifi_ssid and wifi_psk to NVS (called after a successful WPS handshake).
pub fn nvs_write_wifi_creds(ssid: &str, psk: &str) -> bool {
    let ns = match CString::new(NVS_NS) {
        Ok(s) => s,
        Err(_) => return false,
    };
    unsafe {
        let mut handle: nvs_handle_t = 0;
        if nvs_open(ns.as_ptr(), nvs_open_mode_t_NVS_READWRITE, &mut handle) != ESP_OK {
            return false;
        }
        let mut ok = true;
        for (key, val) in [("wifi_ssid", ssid), ("wifi_psk", psk)] {
            match (CString::new(key), CString::new(val)) {
                (Ok(k), Ok(v)) => {
                    if nvs_set_str(handle, k.as_ptr(), v.as_ptr()) != ESP_OK {
                        warn!("NVS: failed to write key '{}'", key);
                        ok = false;
                    }
                }
                _ => { ok = false; }
            }
        }
        nvs_commit(handle);
        nvs_close(handle);
        ok
    }
}

/// Erase all NVS settings and restart the device (returns to cfg.toml defaults on next boot).
pub fn factory_reset() -> ! {
    info!("Factory reset: erasing NVS namespace '{}'...", NVS_NS);
    nvs_erase_namespace();
    info!("NVS erased. Restarting device...");
    unsafe { esp_idf_sys::esp_restart() }
}

/// Load config from NVS, falling back to the provided defaults for any missing keys.
pub fn load_config(defaults: NvsConfig) -> NvsConfig {
    fn or(nvs_val: Option<String>, default: &str) -> String {
        match nvs_val {
            Some(s) if !s.is_empty() => s,
            _ => default.to_string(),
        }
    }
    info!("Loading config from NVS (namespace: {})", NVS_NS);
    NvsConfig {
        wifi_ssid:     or(nvs_read("wifi_ssid"),     &defaults.wifi_ssid),
        wifi_psk:      or(nvs_read("wifi_psk"),      &defaults.wifi_psk),
        ip_mode:       or(nvs_read("ip_mode"),       &defaults.ip_mode),
        ip_address:    or(nvs_read("ip_address"),    &defaults.ip_address),
        subnet_mask:   or(nvs_read("subnet_mask"),   &defaults.subnet_mask),
        gateway:       or(nvs_read("gateway"),       &defaults.gateway),
        dns:           or(nvs_read("dns"),           &defaults.dns),
        syslog_server: or(nvs_read("syslog_server"), &defaults.syslog_server),
        syslog_enable: or(nvs_read("syslog_enable"), &defaults.syslog_enable),
        syslog_host_name: or(nvs_read("syslog_hostname"), &defaults.syslog_host_name),
        syslog_app_name: or(nvs_read("syslog_app_name"), &defaults.syslog_app_name),
        ssh_user:      or(nvs_read("ssh_user"),      &defaults.ssh_user),
        ssh_password:  or(nvs_read("ssh_password"),  &defaults.ssh_password),
        com1_tx_pin:   or(nvs_read("com1_tx_pin"),   &defaults.com1_tx_pin),
        com1_rx_pin:   or(nvs_read("com1_rx_pin"),   &defaults.com1_rx_pin),
        com1_baud:     or(nvs_read("com1_baud"),     &defaults.com1_baud),
        display_enable: or(nvs_read("display_enable"), &defaults.display_enable),
        display_port:  or(nvs_read("display_port"),  &defaults.display_port),
        emmc_enable:   or(nvs_read("emmc_enable"),   &defaults.emmc_enable),
        wps_enable:    or(nvs_read("wps_enable"),    &defaults.wps_enable),
        ntp_server1:   or(nvs_read("ntp_server1"),   &defaults.ntp_server1),
        ntp_server2:   or(nvs_read("ntp_server2"),   &defaults.ntp_server2),
        ntp_server3:   or(nvs_read("ntp_server3"),   &defaults.ntp_server3),
        ntp_server4:   or(nvs_read("ntp_server4"),   &defaults.ntp_server4),
        map_type:      or(nvs_read("map_type"),      &defaults.map_type),
        timepulse_freq: or(nvs_read("timepulse_freq"), &defaults.timepulse_freq),
        maps_api_key:  or(nvs_read("maps_api_key"), &defaults.maps_api_key),
        quake_enable:  or(nvs_read("quake_enable"), &defaults.quake_enable),
    }
}

// ─── Session management ───────────────────────────────────────────────────────

fn generate_token() -> String {
    (0..4)
        .map(|_| format!("{:08x}", unsafe { esp_random() }))
        .collect()
}

fn get_session_from_cookie_header(cookie_header: Option<&str>) -> Option<String> {
    let h = cookie_header?;
    for part in h.split(';') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix("session=") {
            return Some(v.to_string());
        }
    }
    None
}

fn is_auth(cookie_header: Option<&str>, session: &Arc<Mutex<Option<String>>>) -> bool {
    let guard = session.lock().unwrap();
    if let Some(token) = guard.as_ref() {
        return get_session_from_cookie_header(cookie_header)
            .as_deref() == Some(token.as_str());
    }
    false
}

// ─── URL decode / form body parse ────────────────────────────────────────────

fn url_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => {
                let hex = core::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("");
                if let Ok(c) = u8::from_str_radix(hex, 16) {
                    out.push(c as char);
                    i += 3;
                    continue;
                }
                out.push('%');
                i += 1;
            }
            c => {
                out.push(c as char);
                i += 1;
            }
        }
    }
    out
}

fn parse_form(body: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pair in body.split('&') {
        if let Some(eq) = pair.find('=') {
            map.insert(url_decode(&pair[..eq]), url_decode(&pair[eq + 1..]));
        }
    }
    map
}

// ─── Shared state ─────────────────────────────────────────────────────────────

/// Runtime status shared between the main loop and the HTTP server.
#[derive(Clone)]
pub struct StatusInfo {
    pub ip_address:     Arc<Mutex<String>>,
    pub ssid:           Arc<Mutex<String>>,
    pub rssi:           Arc<Mutex<i32>>,
    pub battery_voltage: Arc<Mutex<f32>>,
    pub chip_temp:      Arc<Mutex<f32>>,
}

impl StatusInfo {
    pub fn new() -> Self {
        Self {
            ip_address:      Arc::new(Mutex::new("--".to_string())),
            ssid:            Arc::new(Mutex::new("--".to_string())),
            rssi:            Arc::new(Mutex::new(0)),
            battery_voltage: Arc::new(Mutex::new(0.0)),
            chip_temp:       Arc::new(Mutex::new(0.0)),
        }
    }
    pub fn set_wifi(&self, ip: &str, ssid: &str, rssi: i32) {
        *self.ip_address.lock().unwrap() = ip.to_string();
        *self.ssid.lock().unwrap()       = ssid.to_string();
        *self.rssi.lock().unwrap()       = rssi;
    }
    pub fn set_rssi(&self, rssi: i32) {
        *self.rssi.lock().unwrap() = rssi;
    }
    pub fn set_battery_voltage(&self, voltage: f32) {
        *self.battery_voltage.lock().unwrap() = voltage;
    }
    pub fn set_chip_temp(&self, temp: f32) {
        *self.chip_temp.lock().unwrap() = temp;
    }
}

#[derive(Clone)]
pub struct ConfigState {
    pub config:   Arc<Mutex<NvsConfig>>,
    pub defaults: Arc<NvsConfig>,
    pub status:   StatusInfo,
    session:      Arc<Mutex<Option<String>>>,
}

impl ConfigState {
    pub fn new(config: NvsConfig, defaults: NvsConfig, status: StatusInfo) -> Self {
        Self {
            config:   Arc::new(Mutex::new(config)),
            defaults: Arc::new(defaults),
            status,
            session:  Arc::new(Mutex::new(None)),
        }
    }
}

// ─── HTML helpers ─────────────────────────────────────────────────────────────

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[allow(dead_code)]
fn sel(current: &str, value: &str) -> &'static str {
    if current == value { "selected" } else { "" }
}

fn chk(current: &str, value: &str) -> &'static str {
    if current == value { "checked" } else { "" }
}

// ─── Login page ───────────────────────────────────────────────────────────────

const LOGIN_HTML: &str = concat!(
    r#"<!DOCTYPE html><html lang="ja"><head>"#,
    r#"<meta charset="UTF-8"><meta name="viewport" content="width=device-width,initial-scale=1">"#,
    r#"<title>SSH-Serial-Bridge Login</title><style>"#,
    r#"*{box-sizing:border-box}"#,
    r#"body{font-family:Arial,sans-serif;background:#1a1a2e;display:flex;justify-content:center;"#,
    r#"align-items:center;height:100vh;margin:0}"#,
    r#".box{background:#16213e;padding:40px;border-radius:12px;"#,
    r#"box-shadow:0 4px 20px rgba(0,0,0,.5);width:320px}"#,
    r#"h1{color:#e94560;text-align:center;margin:0 0 24px;font-size:1.4em}"#,
    r#"label{color:#a8b2d8;font-size:.9em;display:block;margin:12px 0 4px}"#,
    r#"input{width:100%;padding:10px;border:1px solid #304070;border-radius:6px;"#,
    r#"background:#0f3460;color:#e2e8f0;font-size:1em}"#,
    r#"input:focus{outline:none;border-color:#e94560}"#,
    r#"button{width:100%;margin-top:20px;padding:12px;background:#e94560;color:#fff;"#,
    r#"border:none;border-radius:6px;font-size:1em;font-weight:bold;cursor:pointer}"#,
    r#"button:hover{background:#c73652}"#,
    r#"#err{color:#f87171;text-align:center;margin-top:12px;font-size:.9em;display:none}"#,
    r#"</style></head><body>"#,
    r#"<div class="box"><h1>Satellite Viewer Config</h1>"#,
    r#"<form method="POST" action="/login">"#,
    r#"<label>Username</label>"#,
    r#"<input type="text" name="username" autocomplete="username" required>"#,
    r#"<label>Password</label>"#,
    r#"<input type="password" name="password" autocomplete="current-password" required>"#,
    r#"<button type="submit">Login</button>"#,
    r#"</form>"#,
    r#"<div id="err">Username or password is incorrect</div></div>"#,
    r#"<script>if(location.search.includes('error=1'))"#,
    r#"document.getElementById('err').style.display='block';</script>"#,
    r#"</body></html>"#
);

// ─── Config page builder ──────────────────────────────────────────────────────

fn build_config_html(cfg: &NvsConfig) -> String {
    format!(
        r#"<!DOCTYPE html><html lang="ja"><head>
<meta charset="UTF-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Satellite Viewer Config</title>
<style>
*{{box-sizing:border-box;margin:0;padding:0}}
body{{font-family:'Segoe UI',Arial,sans-serif;background:#111827;color:#e2e8f0;min-height:100vh}}
.page{{max-width:780px;margin:0 auto;padding:16px}}
h1{{color:#f87171;text-align:center;font-size:1.4em;padding:18px 0 14px;letter-spacing:.05em}}
/* Section headers with icon strip */
.section-title{{display:flex;align-items:center;gap:8px;color:#93c5fd;font-size:.82em;
  font-weight:700;text-transform:uppercase;letter-spacing:.08em;
  margin:0 0 12px;padding-bottom:6px;border-bottom:1px solid #1e3a5f}}
/* Cards */
.card{{background:#1e2d45;border-radius:10px;padding:16px 18px;margin-bottom:10px;
  border:1px solid #243b55;box-shadow:0 2px 8px rgba(0,0,0,.35)}}
/* Grid: 2-col on wide, 1-col on narrow */
.grid{{display:grid;grid-template-columns:repeat(auto-fill,minmax(200px,1fr));gap:10px 14px;margin-bottom:4px}}
.grid-3{{display:grid;grid-template-columns:repeat(3,1fr);gap:10px 14px}}
@media(max-width:520px){{.grid-3{{grid-template-columns:1fr 1fr}}}}
/* Field */
.field label{{font-size:.76em;font-weight:600;color:#94a3b8;display:block;
  margin-bottom:4px;letter-spacing:.03em}}
.field input,.field select{{
  width:100%;padding:8px 10px;background:#0f172a;border:1px solid #2d4a6a;
  border-radius:6px;color:#e2e8f0;font-size:.9em;transition:border-color .2s}}
.field input:focus,.field select:focus{{outline:none;border-color:#60a5fa;
  box-shadow:0 0 0 2px rgba(96,165,250,.15)}}
/* Radio toggle group */
.toggle-group{{display:inline-flex;background:#0f172a;border:1px solid #2d4a6a;
  border-radius:8px;overflow:hidden;margin-bottom:10px}}
.toggle-group label{{padding:7px 18px;font-size:.85em;font-weight:600;cursor:pointer;
  color:#94a3b8;transition:background .2s,color .2s;user-select:none}}
.toggle-group input[type=radio]{{display:none}}
.toggle-group input[type=radio]:checked+span{{background:#1d4ed8;color:#fff;
  border-radius:7px;display:block;padding:7px 18px;margin:-7px -18px}}
/* Indent block shown/hidden based on IP mode */
.indent{{padding-left:4px;border-left:2px solid #1d4ed8;margin-top:6px}}
/* Status rows */
.stat-grid{{display:grid;grid-template-columns:auto 1fr;gap:6px 16px;align-items:center}}
.stat-key{{font-size:.78em;font-weight:600;color:#64748b;text-transform:uppercase;
  letter-spacing:.06em;white-space:nowrap}}
.stat-val{{font-size:.9em;color:#7dd3fc;font-weight:600}}
.badge{{display:inline-block;padding:2px 9px;border-radius:20px;font-size:.8em;font-weight:700}}
.badge-ok{{background:#14532d;color:#4ade80;border:1px solid #166534}}
.badge-warn{{background:#451a03;color:#fb923c;border:1px solid #7c2d12}}
.badge-err{{background:#4c0519;color:#fb7185;border:1px solid #881337}}
/* Message bar */
#msg{{padding:10px 14px;border-radius:7px;margin-bottom:10px;font-size:.88em;display:none;
  border-left:4px solid transparent}}
#msg.ok{{background:#052e16;border-color:#16a34a;color:#4ade80}}
#msg.err{{background:#350005;border-color:#dc2626;color:#fca5a5}}
/* Action buttons */
.actions{{display:flex;flex-wrap:wrap;gap:8px}}
.btn{{flex:1;min-width:110px;padding:11px 8px;border:none;border-radius:7px;
  font-size:.9em;font-weight:700;cursor:pointer;letter-spacing:.02em;transition:filter .2s}}
.btn:hover{{filter:brightness(1.15)}}
.btn-save{{background:#1d4ed8;color:#fff}}
.btn-reset{{background:#b45309;color:#fff}}
.btn-reboot{{background:#be123c;color:#fff}}
.btn-danger{{background:#991b1b;color:#fff}}
.btn-gpio{{background:#0f766e;color:#fff;text-decoration:none;
  display:flex;align-items:center;justify-content:center;border-radius:7px;
  font-weight:700;font-size:.9em;flex:1;min-width:110px;border:none;cursor:pointer;letter-spacing:.02em}}
.btn-gpio:hover{{filter:brightness(1.15)}}
.btn-logout{{background:#374151;color:#e2e8f0;text-decoration:none;
  display:flex;align-items:center;justify-content:center;border-radius:7px;
  font-weight:700;font-size:.9em;flex:1;min-width:110px}}
/* Divider */
hr{{border:none;border-top:1px solid #1e3a5f;margin:10px 0}}
</style></head><body>
<div class="page">
<h1>&#x1F6F0; Satellite Viewer Config</h1>
<div id="msg"></div>

<!-- Status -->
<div class="card">
  <div class="section-title">&#x1F4F6; Device Status</div>
  <div id="status-card" class="stat-grid">
    <span class="stat-key">Loading</span><span class="stat-val">...</span>
  </div>
</div>

<!-- Recent earthquakes (P2P地震情報); shown only when quake_enable = "true" -->
<div class="card" id="quake-box" style="display:none">
  <div class="section-title">&#x1F30B; Recent Earthquakes</div>
  <div id="eew-banner" style="display:none;background:#7f1d1d;color:#fff;padding:10px;border-radius:6px;margin-bottom:8px;font-weight:600"></div>
  <div id="quake-card" style="overflow-x:auto">Loading...</div>
  <div style="font-size:11px;color:#64748b;margin-top:6px">出典: 気象庁の情報を利用 / P2P地震情報 (<a href="https://www.p2pquake.net/" target="_blank" rel="noopener">p2pquake.net</a>) &mdash; Source: JMA via P2PQuake</div>
</div>

<form id="cfg">

<!-- Actions -->
<div class="card">
  <div class="actions">
    <button type="button" class="btn btn-save" onclick="saveConfig()">&#x1F4BE; Save to NVS</button>
    <button type="button" class="btn btn-reset" onclick="resetDefaults()">&#x21BA; Reset Defaults</button>
    <button type="button" class="btn btn-reboot" onclick="reboot()">&#x1F501; Reboot</button>
    <a href="/terminal" class="btn-gpio" style="background:#7c3aed">&#x1F4BB; Terminal</a>
    <a href="/skyview" class="btn-gpio" style="background:#1e40af">&#x1F4E1; Sky View</a>
    <a href="/gpsstatus" class="btn-gpio" style="background:#065f46">&#x1F6F0; GPS Status</a>
    <a href="/map" class="btn-gpio" style="background:#164e63">&#x1F4CD; Map</a>
    <a href="/tracks" class="btn-gpio" style="background:#713f12">&#x1F9ED; Tracks</a>
    <a href="/logout" class="btn-logout">&#x1F6AA; Logout</a>
    <button type="button" class="btn" style="background:#0e7490" onclick="showBootLog()">&#x1F4DC; Boot Log</button>
  </div>
</div>

<!-- Storage (eMMC) -->
<div class="card">
  <div class="section-title">&#x1F5C4;&#xFE0F; Storage (eMMC)</div>
  <div class="actions">
    <button type="button" class="btn btn-danger" onclick="clearMapCache()">&#x1F5FA;&#xFE0F; Clear Map Cache</button>
    <button type="button" class="btn btn-danger" onclick="clearTrackLog()">&#x1F4CD; Clear Track Log</button>
    <button type="button" class="btn btn-danger" onclick="formatEmmc()">&#x26A0;&#xFE0F; Format eMMC</button>
  </div>
</div>

<!-- Boot Log Modal -->
<div id="boot-log-overlay" style="display:none;position:fixed;inset:0;background:rgba(0,0,0,.7);z-index:100;align-items:center;justify-content:center">
  <div style="background:#1e2d45;border:1px solid #243b55;border-radius:12px;padding:20px 24px;max-width:480px;width:90%;max-height:80vh;overflow-y:auto;box-shadow:0 8px 32px rgba(0,0,0,.6)">
    <div style="display:flex;justify-content:space-between;align-items:center;margin-bottom:14px">
      <span style="color:#93c5fd;font-weight:700;font-size:.95em;text-transform:uppercase;letter-spacing:.08em">&#x1F4DC; Boot Log</span>
      <button onclick="closeBootLog()" style="background:none;border:none;color:#94a3b8;font-size:1.4em;cursor:pointer;line-height:1">&times;</button>
    </div>
    <div id="boot-log-body" style="font-size:.88em;color:#e2e8f0;line-height:1.7">
      <span style="color:#64748b">Loading...</span>
    </div>
  </div>
</div>

<!-- WiFi -->
<div class="card">
  <div class="section-title">&#x1F4F6; WiFi</div>
  <div class="grid">
    <div class="field"><label>SSID</label>
      <input type="text" name="wifi_ssid" value="{wifi_ssid}" maxlength="32" autocomplete="off"></div>
    <div class="field"><label>Password</label>
      <input type="password" name="wifi_psk" value="{wifi_psk}" maxlength="64" autocomplete="off"></div>
  </div>
  <hr>
  <div style="font-size:.8em;color:#64748b;margin-bottom:6px">WPS (PBC)</div>
  <div class="toggle-group">
    <label><input type="radio" name="wps_enable" value="true" {wps_true}><span>Enable</span></label>
    <label><input type="radio" name="wps_enable" value="false" {wps_false}><span>Disable</span></label>
  </div>
  <p style="font-size:.78em;color:#aaa;margin:.25em 0 0;">When WPS is enabled and SSID is empty, WPS PBC is triggered on next boot.</p>
</div>

<!-- Network -->
<div class="card">
  <div class="section-title">&#x1F310; Network</div>
  <div class="toggle-group">
    <label><input type="radio" name="ip_mode" value="dhcp" {dhcp_chk} onchange="ipModeChange()"><span>DHCP</span></label>
    <label><input type="radio" name="ip_mode" value="static" {static_chk} onchange="ipModeChange()"><span>Static IP</span></label>
  </div>
  <div class="indent" id="static-fields">
    <div class="grid">
      <div class="field"><label>IP Address</label>
        <input type="text" name="ip_address" value="{ip_address}" placeholder="192.168.1.100"></div>
      <div class="field"><label>Subnet Mask</label>
        <input type="text" name="subnet_mask" value="{subnet_mask}" placeholder="255.255.255.0"></div>
      <div class="field"><label>Gateway</label>
        <input type="text" name="gateway" value="{gateway}" placeholder="192.168.1.1"></div>
      <div class="field"><label>DNS Server</label>
        <input type="text" name="dns" value="{dns}" placeholder="192.168.1.1"></div>
    </div>
  </div>
</div>

<!-- Syslog -->
<div class="card">
  <div class="section-title">&#x1F4CB; Syslog</div>
  <div class="grid">
    <div class="field" style="grid-column:1/-1"><label>Server (host:port)</label>
      <input type="text" name="syslog_server" value="{syslog_server}" placeholder="192.168.1.1:514"></div>
    <div class="field"><label>Host Name</label>
      <input type="text" name="syslog_hostname" value="{syslog_host_name}" placeholder="esp32"></div>
    <div class="field"><label>App Name</label>
      <input type="text" name="syslog_app_name" value="{syslog_app_name}" placeholder="app"></div>
  </div>
  <hr>
  <div style="font-size:.8em;color:#64748b;margin-bottom:6px">Enable Syslog</div>
  <div class="toggle-group">
    <label><input type="radio" name="syslog_enable" value="true" {syslog_true}><span>Enabled</span></label>
    <label><input type="radio" name="syslog_enable" value="false" {syslog_false}><span>Disabled</span></label>
  </div>
</div>

<!-- Auth -->
<div class="card">
  <div class="section-title">&#x1F510; Authentication (SSH &amp; Web)</div>
  <div class="grid">
    <div class="field"><label>Username</label>
      <input type="text" name="ssh_user" value="{ssh_user}" maxlength="64" autocomplete="username"></div>
    <div class="field"><label>Password</label>
      <input type="password" name="ssh_password" value="{ssh_password}" maxlength="64" autocomplete="new-password"></div>
  </div>
</div>

<!-- Serial Ports -->
<div class="card">
  <div class="section-title">&#x1F50C; Serial Ports (UART)</div>
  <div style="font-size:.78em;font-weight:600;color:#475569;text-transform:uppercase;
    letter-spacing:.06em;margin-bottom:6px">COM1 / UART1</div>
  <div class="grid-3">
    <div class="field"><label>TX Pin</label>
      <input type="number" name="com1_tx_pin" value="{com1_tx_pin}" min="0" max="48"></div>
    <div class="field"><label>RX Pin</label>
      <input type="number" name="com1_rx_pin" value="{com1_rx_pin}" min="0" max="48"></div>
    <div class="field"><label>Baud Rate</label>
      <input type="number" name="com1_baud" value="{com1_baud}" min="300" max="921600"></div>
  </div>
</div>



<!-- Display Port -->
<div class="card">
  <div class="section-title">&#x1F4FA; Display Serial Port</div>
  <div class="grid-3">
    <div class="field"><label>Monitor Port</label>
      <select name="display_port">
        <option value="com1" {dp_com1}>COM1</option>
      </select></div>
  </div>
</div>



<!-- NTP Servers -->
<div class="card">
  <div class="section-title">&#x1F552; NTP Servers</div>
  <div class="grid">
    <div class="field"><label>NTP Server 1 (Primary)</label>
      <input type="text" name="ntp_server1" value="{ntp_server1}" placeholder="time.google.com"></div>
    <div class="field"><label>NTP Server 2</label>
      <input type="text" name="ntp_server2" value="{ntp_server2}" placeholder="time.google.com"></div>
    <div class="field"><label>NTP Server 3</label>
      <input type="text" name="ntp_server3" value="{ntp_server3}" placeholder="time.cloudflare.com"></div>
    <div class="field"><label>NTP Server 4</label>
      <input type="text" name="ntp_server4" value="{ntp_server4}" placeholder="ntp.nict.jp"></div>
  </div>
</div>

<!-- Map Settings -->
<div class="card">
  <div class="section-title">&#x1F5FA; Map Settings</div>
  <div class="grid">
    <div class="field"><label>Map Type</label>
      <select name="map_type">
        <option value="satellite" {mt_sat}>Satellite (aerial photo)</option>
        <option value="roadmap"   {mt_road}>Roadmap</option>
      </select></div>
    <div class="field"><label>Google Maps API Key</label>
      <input type="password" name="maps_api_key" value="{maps_api_key}" maxlength="128" autocomplete="off" placeholder="Maps Static API key"></div>
  </div>
  <p style="font-size:.78em;color:#aaa;margin:.4em 0 0">Needs the <b>Maps Static API</b> enabled on the key. Applies immediately on Save. Leaving it empty uses the cfg.toml default. The Web UI map pages load images from Google directly in your browser, so a logged-in browser receives this key.</p>
</div>

<!-- GPS TIMEPULSE -->
<div class="card">
  <div class="section-title">&#x1F4E1; GPS TIMEPULSE (u-blox MAX-F10S)</div>
  <div class="grid">
    <div class="field"><label>TIMEPULSE1 Frequency (Hz)</label>
      <select name="timepulse_freq">
        <option value="0"        {tp_0}>Disabled</option>
        <option value="1"        {tp_1}>1 Hz (standard PPS)</option>
        <option value="4"        {tp_4}>4 Hz</option>
        <option value="10"       {tp_10}>10 Hz</option>
        <option value="100"      {tp_100}>100 Hz</option>
        <option value="1000"     {tp_1000}>1000 Hz (1 kHz)</option>
        <option value="10000"    {tp_10000}>10 kHz</option>
        <option value="100000"   {tp_100000}>100 kHz</option>
        <option value="1000000"  {tp_1000000}>1 MHz</option>
        <option value="10000000" {tp_10000000}>10 MHz</option>
      </select>
    </div>
  </div>
  <p style="font-size:.78em;color:#aaa;margin:.4em 0 0">Applies immediately on Save. Stored in ESP32 NVS and re-applied to the GPS module at every boot.</p>
</div>


</form>
</div>
<script>
// Show/hide static IP fields based on ip_mode radio
function ipModeChange() {{
  var isStatic = document.querySelector('input[name=ip_mode]:checked').value === 'static';
  document.getElementById('static-fields').style.display = isStatic ? 'block' : 'none';
}}
// Apply on load
ipModeChange();
function showMsg(text, ok) {{
  var d = document.getElementById('msg');
  d.textContent = text;
  d.className = ok ? 'ok' : 'err';
  d.style.display = 'block';
  d.scrollIntoView({{behavior:'smooth'}});
}}
function saveConfig() {{
  var fd = new FormData(document.getElementById('cfg'));
  var body = new URLSearchParams(fd).toString();
  fetch('/api/config', {{
    method: 'POST',
    headers: {{'Content-Type': 'application/x-www-form-urlencoded'}},
    body: body
  }})
  .then(r => r.text())
  .then(t => showMsg(t, true))
  .catch(e => showMsg('Save failed: ' + e, false));
}}
function reboot() {{
  if (!confirm('Reboot the device?')) return;
  fetch('/api/reboot', {{method: 'POST'}})
  .then(() => {{
    showMsg('Rebooting... redirecting to login in 3 seconds.', true);
    setTimeout(() => {{ location.href = '/login'; }}, 3000);
  }})
  .catch(e => showMsg('Reboot request failed: ' + e, false));
}}
function resetDefaults() {{
  if (!confirm('Reset all settings to compile-time defaults and reload the page?')) return;
  fetch('/api/reset', {{method: 'POST'}})
  .then(r => r.text())
  .then(() => {{ location.reload(); }})
  .catch(e => showMsg('Reset failed: ' + e, false));
}}
function clearMapCache() {{
  if (!confirm('Delete the entire cached map tile set from eMMC? This cannot be undone.')) return;
  fetch('/api/storage/clear_mapcache', {{method: 'POST'}})
  .then(r => r.text())
  .then(t => showMsg(t, true))
  .catch(e => showMsg('Clear map cache failed: ' + e, false));
}}
function clearTrackLog() {{
  if (!confirm('Delete all GPS track log files from eMMC? This cannot be undone.')) return;
  fetch('/api/storage/clear_tracklog', {{method: 'POST'}})
  .then(r => r.text())
  .then(t => showMsg(t, true))
  .catch(e => showMsg('Clear track log failed: ' + e, false));
}}
function formatEmmc() {{
  if (!confirm('Format the ENTIRE eMMC card? This erases the map cache, track logs, and any other data on it. This cannot be undone.')) return;
  if (!confirm('Really sure? This is your last chance to cancel.')) return;
  fetch('/api/storage/format_emmc', {{method: 'POST'}})
  .then(r => r.text())
  .then(t => showMsg(t, true))
  .catch(e => showMsg('Format eMMC failed: ' + e, false));
}}
function rssiBar(rssi) {{
  if (rssi >= -55) return '<span class="badge badge-ok">Excellent (' + rssi + ' dBm)</span>';
  if (rssi >= -70) return '<span class="badge badge-ok">Good (' + rssi + ' dBm)</span>';
  if (rssi >= -80) return '<span class="badge badge-warn">Fair (' + rssi + ' dBm)</span>';
  return '<span class="badge badge-err">Weak (' + rssi + ' dBm)</span>';
}}
function updateStatus() {{
  fetch('/api/status')
  .then(r => r.json())
  .then(d => {{
    document.getElementById('status-card').innerHTML =
      '<span class="stat-key">IP Address</span><span class="stat-val">' + d.ip + '</span>' +
      '<span class="stat-key">WiFi SSID</span><span class="stat-val">' + d.ssid + '</span>' +
      '<span class="stat-key">RSSI</span><span class="stat-val">' + rssiBar(d.rssi) + '</span>' +
      '<span class="stat-key">Battery Voltage</span><span class="stat-val"><span class="badge badge-ok">' + d.battery_voltage.toFixed(2) + ' V</span></span>' +
      '<span class="stat-key">Chip Temp</span><span class="stat-val"><span class="badge ' + (d.chip_temp < 70 ? 'badge-ok' : d.chip_temp < 85 ? 'badge-warn' : 'badge-err') + '">' + d.chip_temp.toFixed(1) + ' °C</span></span>' +
      '<span class="stat-key">Free Heap (Internal)</span><span class="stat-val"><span class="badge badge-ok">' + (d.heap_int_free/1024).toFixed(0) + ' KB</span> <small>min ' + (d.heap_int_min/1024).toFixed(0) + ' KB</small></span>' +
      '<span class="stat-key">Free Heap (PSRAM)</span><span class="stat-val"><span class="badge badge-ok">' + (d.heap_psram_free/1024).toFixed(0) + ' KB</span> <small>min ' + (d.heap_psram_min/1024).toFixed(0) + ' KB</small></span>' +
      '<span class="stat-key">Display Port</span><span class="stat-val"><span class="badge badge-ok">' + d.display_port + '</span></span>';
  }})
  .catch(() => {{}});
}}
updateStatus();
setInterval(updateStatus, 3000);
function updateQuakes() {{
  fetch('/api/quakes')
  .then(r => r.json())
  .then(d => {{
    document.getElementById('quake-box').style.display = d.enabled ? '' : 'none';
    if (!d.enabled) return;
    var b = document.getElementById('eew-banner');
    if (d.eew) {{
      b.style.display = 'block';
      b.textContent = '\u26A0 \u7DCA\u6025\u5730\u9707\u901F\u5831 ' + d.eew.time + ' ' + d.eew.name +
        ' M' + (d.eew.mag >= 0 ? d.eew.mag.toFixed(1) : '?') + ' \u6700\u5927\u4E88\u6E2C\u9707\u5EA6' + d.eew.intensity +
        (d.eew.areas ? ' / ' + d.eew.areas : '') +
        (d.eew.view && d.eew.view.dist_km !== null ? ' / ' + d.eew.view.dist_km + ' km' : '');
    if (d.eew.view) {{
      var a = document.createElement('a');
      a.href = '/map?lat=' + d.eew.view.lat + '&lon=' + d.eew.view.lon + '&zoom=' + d.eew.view.zoom;
      a.textContent = ' \u25B6 MAP';
      a.style.color = '#fde047';
      b.appendChild(a);
    }}
    }} else {{ b.style.display = 'none'; }}
    if (!d.quakes.length) {{
      document.getElementById('quake-card').textContent = d.ready ? 'No events' : 'Waiting for data...';
      return;
    }}
    var h = '<table style="width:100%;border-collapse:collapse;font-size:13px">' +
      '<tr style="text-align:left;color:#64748b"><th>Time (JST)</th><th>Epicenter</th><th>M</th><th>Int.</th><th>Depth</th><th>Tsunami</th></tr>';
    d.quakes.forEach(function(q) {{
      var sev = q.scale >= 45 || q.mag >= 6 ? 'badge-err' : (q.scale >= 30 || q.mag >= 4.5 ? 'badge-warn' : 'badge-ok');
      var depth = q.depth < 0 ? '?' : (q.depth === 0 ? 'Very shallow' : q.depth + ' km');
      var link = '/map?lat=' + q.lat + '&lon=' + q.lon + '&zoom=' + (q.mag >= 6 ? 6 : 8);
      h += '<tr><td>' + q.time + '</td>' +
        '<td><a href="' + link + '">' + q.name + '</a></td>' +
        '<td><span class="badge ' + sev + '">' + (q.mag >= 0 ? q.mag.toFixed(1) : '?') + '</span></td>' +
        '<td>' + q.intensity + '</td><td>' + depth + '</td><td>' + q.tsunami + '</td></tr>';
    }});
    document.getElementById('quake-card').innerHTML = h + '</table>';
  }})
  .catch(() => {{}});
}}
updateQuakes();
setInterval(updateQuakes, 5000);
function showBootLog() {{
  var overlay = document.getElementById('boot-log-overlay');
  overlay.style.display = 'flex';
  document.getElementById('boot-log-body').innerHTML = '<span style="color:#64748b">Loading...</span>';
  fetch('/api/boot_log')
  .then(r => r.json())
  .then(d => {{
    if (!d.entries || d.entries.length === 0) {{
      document.getElementById('boot-log-body').innerHTML = '<span style="color:#64748b">No entries yet.</span>';
      return;
    }}
    var reasons = {{
      'PowerOn':     ['badge-ok',   '&#x1F50C; PowerOn'],
      'Software':    ['badge-ok',   '&#x1F501; Software'],
      'Panic/Crash': ['badge-err',  '&#x1F4A5; Panic/Crash'],
      'IntWatchdog': ['badge-err',  '&#x231B; IntWatchdog'],
      'TaskWatchdog':['badge-warn', '&#x231B; TaskWatchdog'],
      'Watchdog':    ['badge-warn', '&#x231B; Watchdog'],
      'Brownout':    ['badge-err',  '&#x26A1; Brownout'],
      'DeepSleep':   ['badge-ok',   '&#x1F4A4; DeepSleep'],
      'ExtReset':    ['badge-ok',   '&#x1F504; ExtReset'],
      'SDIO':        ['badge-warn', '&#x1F4BE; SDIO'],
    }};
    var html = '<table style="width:100%;border-collapse:collapse">' +
      '<tr><th style="text-align:left;color:#475569;font-size:.75em;padding-bottom:6px;border-bottom:1px solid #243b55">#</th>' +
      '<th style="text-align:left;color:#475569;font-size:.75em;padding-bottom:6px;border-bottom:1px solid #243b55;padding-left:10px">Reset Reason</th></tr>';
    d.entries.forEach(function(e) {{
      var m = e.match(/^#(\d+)\s+(.+)$/);
      if (!m) return;
      var num = m[1], reason = m[2].trim();
      var info = reasons[reason] || ['badge-warn', reason];
      html += '<tr><td style="padding:5px 0;color:#64748b;font-size:.85em;vertical-align:top">' + num + '</td>' +
        '<td style="padding:5px 0 5px 10px"><span class="badge ' + info[0] + '">' + info[1] + '</span></td></tr>';
    }});
    html += '</table>';
    document.getElementById('boot-log-body').innerHTML = html;
  }})
  .catch(e => {{
    document.getElementById('boot-log-body').innerHTML = '<span style="color:#fca5a5">Failed to load: ' + e + '</span>';
  }});
}}
function closeBootLog() {{
  document.getElementById('boot-log-overlay').style.display = 'none';
}}
document.getElementById('boot-log-overlay').addEventListener('click', function(e) {{
  if (e.target === this) closeBootLog();
}});
</script>
</body></html>"#,
        wifi_ssid     = esc(&cfg.wifi_ssid),
        wifi_psk      = esc(&cfg.wifi_psk),
        wps_true      = chk(&cfg.wps_enable, "true"),
        wps_false     = chk(&cfg.wps_enable, "false"),
        dhcp_chk      = chk(&cfg.ip_mode, "dhcp"),
        static_chk    = chk(&cfg.ip_mode, "static"),
        ip_address    = esc(&cfg.ip_address),
        subnet_mask   = esc(&cfg.subnet_mask),
        gateway       = esc(&cfg.gateway),
        dns           = esc(&cfg.dns),
        syslog_server = esc(&cfg.syslog_server),
        syslog_true   = chk(&cfg.syslog_enable, "true"),
        syslog_false  = chk(&cfg.syslog_enable, "false"),
        syslog_host_name = esc(&cfg.syslog_host_name),
        syslog_app_name  = esc(&cfg.syslog_app_name),
        ssh_user      = esc(&cfg.ssh_user),
        ssh_password  = esc(&cfg.ssh_password),
        com1_tx_pin   = esc(&cfg.com1_tx_pin),
        com1_rx_pin   = esc(&cfg.com1_rx_pin),
        com1_baud     = esc(&cfg.com1_baud),
        dp_com1       = if cfg.display_port == "com1" { "selected" } else { "" },
        ntp_server1   = esc(&cfg.ntp_server1),
        ntp_server2   = esc(&cfg.ntp_server2),
        ntp_server3   = esc(&cfg.ntp_server3),
        ntp_server4   = esc(&cfg.ntp_server4),
        maps_api_key  = esc(&cfg.maps_api_key),
        mt_sat        = if cfg.map_type == "satellite" { "selected" } else { "" },
        mt_road       = if cfg.map_type == "roadmap"   { "selected" } else { "" },
        tp_0          = if cfg.timepulse_freq == "0"    { "selected" } else { "" },
        tp_1          = if cfg.timepulse_freq == "1"    { "selected" } else { "" },
        tp_4          = if cfg.timepulse_freq == "4"    { "selected" } else { "" },
        tp_10         = if cfg.timepulse_freq == "10"   { "selected" } else { "" },
        tp_100        = if cfg.timepulse_freq == "100"  { "selected" } else { "" },
        tp_1000       = if cfg.timepulse_freq == "1000"     { "selected" } else { "" },
        tp_10000      = if cfg.timepulse_freq == "10000"    { "selected" } else { "" },
        tp_100000     = if cfg.timepulse_freq == "100000"   { "selected" } else { "" },
        tp_1000000    = if cfg.timepulse_freq == "1000000"  { "selected" } else { "" },
        tp_10000000   = if cfg.timepulse_freq == "10000000" { "selected" } else { "" },
    )
}

// ─── HTTP server ──────────────────────────────────────────────────────────────

// ─── HTTP server ──────────────────────────────────────────────────────────────

/// Value of `key` in the query string of a request URI, URL-decoded.
fn query_param(uri: &str, key: &str) -> Option<String> {
    let q = uri.split_once('?')?.1;
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| url_decode(v))
    })
}

pub fn start_http_server(state: ConfigState) -> anyhow::Result<EspHttpServer<'static>> {
    // 32 is esp-idf-svc's default and the handlers below were already close to it.
    let config = Configuration { max_uri_handlers: 40, ..Default::default() };
    let mut server = EspHttpServer::new(&config)?;

    // ── GET /login ──────────────────────────────────────────────────────────
    server.fn_handler("/login", Method::Get, |request| {
        let mut response = request.into_ok_response()?;
        response.write_all(LOGIN_HTML.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── POST /login ─────────────────────────────────────────────────────────
    let state_login = state.clone();
    server.fn_handler("/login", Method::Post, move |mut request| {
        let len = (request.content_len().unwrap_or(0) as usize).min(512);
        let mut buf = vec![0u8; len];
        request.read_exact(&mut buf).unwrap_or(());
        let body = String::from_utf8_lossy(&buf);
        let form = parse_form(&body);

        let cfg = state_login.config.lock().unwrap();
        let valid = form.get("username").map(String::as_str) == Some(cfg.ssh_user.as_str())
            && form.get("password").map(String::as_str) == Some(cfg.ssh_password.as_str());
        drop(cfg);

        if valid {
            let token = generate_token();
            *state_login.session.lock().unwrap() = Some(token.clone());
            let cookie = format!(
                "session={}; Path=/; HttpOnly; SameSite=Strict",
                token
            );
            let headers = [("Set-Cookie", cookie.as_str()), ("Location", "/")];
            request.into_response(302, Some("Found"), &headers)?.write_all(b"")?;
        } else {
            info!("HTTP: failed login attempt");
            let headers = [("Location", "/login?error=1")];
            request.into_response(302, Some("Found"), &headers)?.write_all(b"")?;
        }
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET /logout ─────────────────────────────────────────────────────────
    let state_logout = state.clone();
    server.fn_handler("/logout", Method::Get, move |request| {
        *state_logout.session.lock().unwrap() = None;
        let headers = [
            ("Set-Cookie", "session=; Path=/; Max-Age=0"),
            ("Location", "/login"),
        ];
        request.into_response(302, Some("Found"), &headers)?.write_all(b"")?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET / — config page (requires auth) ─────────────────────────────────
    let state_main = state.clone();
    server.fn_handler("/", Method::Get, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_main.session) {
            let headers = [("Location", "/login")];
            request.into_response(302, Some("Found"), &headers)?.write_all(b"")?;
            return Ok::<(), anyhow::Error>(());
        }
        let cfg = state_main.config.lock().unwrap().clone();
        let html = build_config_html(&cfg);
        let headers = [("Content-Type", "text/html; charset=UTF-8")];
        let mut response = request.into_response(200, Some("OK"), &headers)?;
        response.write_all(html.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── POST /api/config — save config to NVS (requires auth) ───────────────
    let state_cfg = state.clone();
    server.fn_handler("/api/config", Method::Post, move |mut request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_cfg.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        let len = (request.content_len().unwrap_or(0) as usize).min(4096);
        let mut buf = vec![0u8; len];
        request.read_exact(&mut buf).unwrap_or(());
        let body = String::from_utf8_lossy(&buf);
        let form = parse_form(&body);

        let mut cfg = state_cfg.config.lock().unwrap();
        macro_rules! update {
            ($field:ident, $key:expr) => {
                if let Some(v) = form.get($key) {
                    cfg.$field = v.clone();
                }
            };
        }
        update!(wifi_ssid,     "wifi_ssid");
        update!(wifi_psk,      "wifi_psk");
        update!(ip_mode,       "ip_mode");
        update!(ip_address,    "ip_address");
        update!(subnet_mask,   "subnet_mask");
        update!(gateway,       "gateway");
        update!(dns,           "dns");
        update!(syslog_server, "syslog_server");
        update!(syslog_enable, "syslog_enable");
        update!(syslog_host_name, "syslog_hostname");
        update!(syslog_app_name, "syslog_app_name");
        update!(ssh_user,      "ssh_user");
        update!(ssh_password,  "ssh_password");
        update!(com1_tx_pin,   "com1_tx_pin");
        update!(com1_rx_pin,   "com1_rx_pin");
        update!(com1_baud,     "com1_baud");
        update!(display_port,  "display_port");
        update!(wps_enable,    "wps_enable");
        update!(ntp_server1,   "ntp_server1");
        update!(ntp_server2,   "ntp_server2");
        update!(ntp_server3,   "ntp_server3");
        update!(ntp_server4,   "ntp_server4");
        update!(map_type,      "map_type");
        update!(timepulse_freq, "timepulse_freq");
        update!(maps_api_key,  "maps_api_key");
        cfg.maps_api_key = cfg.maps_api_key.trim().to_string();

        // Apply map type change immediately (no reboot needed)
        crate::map_display::set_map_type_satellite(cfg.map_type == "satellite");

        // Apply Maps API key change immediately (no reboot needed), and
        // refetch the map with it.
        if crate::map_display::set_api_key(&cfg.maps_api_key) {
            crate::map_display::reset_last_pos();
        }

        // Apply TIMEPULSE1 frequency change immediately (no reboot needed)
        crate::usb_host::apply_timepulse_freq(cfg.timepulse_freq.parse().unwrap_or(1));

        // Apply display port change immediately (no reboot needed)
        crate::usb_host::set_display_port(&cfg.display_port);

        // Apply syslog hostname/app_name changes immediately (no reboot needed)
        if let Err(e) = crate::syslogger::update_logger_config(&cfg.syslog_host_name, &cfg.syslog_app_name) {
            warn!("Failed to update syslog config: {}", e);
        }

        let ok = nvs_write_all(&cfg);
        drop(cfg);

        if ok {
            info!("HTTP: config saved to NVS");
            let headers = [("Content-Type", "text/plain; charset=UTF-8")];
            request.into_response(200, Some("OK"), &headers)?
                .write_all("Settings saved. Reboot to apply.".as_bytes())?;
        } else {
            warn!("HTTP: failed to save config to NVS");
            request.into_status_response(500)?
                .write_all(b"NVS write error")?;
        }
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET /api/status — device/wifi/usb status (requires auth) ────────────
    let state_stat = state.clone();
    server.fn_handler("/api/status", Method::Get, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_stat.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        let ip   = state_stat.status.ip_address.lock().unwrap().clone();
        let ssid = state_stat.status.ssid.lock().unwrap().clone();
        let rssi = *state_stat.status.rssi.lock().unwrap();
        let battery_voltage = *state_stat.status.battery_voltage.lock().unwrap();
        let chip_temp = *state_stat.status.chip_temp.lock().unwrap();
        let mem = mem_info();
        let json = format!(
            r#"{{"ip":"{ip}","ssid":"{ssid}","rssi":{rssi},"battery_voltage":{battery_voltage:.2},"chip_temp":{chip_temp:.1},"heap_int_free":{hif},"heap_int_min":{him},"heap_psram_free":{hpf},"heap_psram_min":{hpm},"display_port":"{display_port}"}}"#,
            hif = mem.int_free, him = mem.int_min, hpf = mem.psram_free, hpm = mem.psram_min,
            ip              = esc(&ip),
            ssid            = esc(&ssid),
            rssi            = rssi,
            battery_voltage = battery_voltage,
            chip_temp       = chip_temp,
            display_port = crate::usb_host::display_port_name(),
        );
        let headers = [("Content-Type", "application/json")];
        request.into_response(200, Some("OK"), &headers)?
            .write_all(json.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── POST /api/reset — erase NVS and restore defaults (requires auth) ────
    let state_rst = state.clone();
    server.fn_handler("/api/reset", Method::Post, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_rst.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        nvs_erase_namespace();
        let defaults = (*state_rst.defaults).clone();
        *state_rst.config.lock().unwrap() = defaults;
        info!("HTTP: config reset to defaults");
        let headers = [("Content-Type", "text/plain; charset=UTF-8")];
        request.into_response(200, Some("OK"), &headers)?
            .write_all(b"Reset to defaults. NVS erased.")?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── POST /api/reboot (requires auth) ────────────────────────────────────
    let state_rb = state.clone();
    server.fn_handler("/api/reboot", Method::Post, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_rb.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        // Clear session so the login page is shown after reconnect
        *state_rb.session.lock().unwrap() = None;
        request.into_ok_response()?.write_all(b"Rebooting...")?;
        // Use Builder with explicit stack size — bare std::thread::spawn can
        // silently fail on ESP-IDF when default stack is too small.
        let spawned = std::thread::Builder::new()
            .name("reboot".into())
            .stack_size(2048)
            .spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(500));
                unsafe { esp_restart(); }
            });
        if spawned.is_err() {
            // Fallback: reboot directly if thread spawn failed
            std::thread::sleep(std::time::Duration::from_millis(200));
            unsafe { esp_restart(); }
        }
        Ok::<(), anyhow::Error>(())
    })?;

    // ── POST /api/storage/clear_mapcache — wipe map tile cache (requires auth) ──
    let state_cmc = state.clone();
    server.fn_handler("/api/storage/clear_mapcache", Method::Post, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_cmc.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        info!("HTTP: map cache clear requested");
        let _ = std::thread::Builder::new().stack_size(8192).spawn(crate::map_cache::clear);
        let headers = [("Content-Type", "text/plain; charset=UTF-8")];
        request.into_response(200, Some("OK"), &headers)?
            .write_all(b"Map cache clear started.")?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── POST /api/storage/clear_tracklog — wipe GPS track logs (requires auth) ──
    let state_ctl = state.clone();
    server.fn_handler("/api/storage/clear_tracklog", Method::Post, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_ctl.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        info!("HTTP: track log clear requested");
        let _ = std::thread::Builder::new().stack_size(4096).spawn(crate::track_log::clear);
        let headers = [("Content-Type", "text/plain; charset=UTF-8")];
        request.into_response(200, Some("OK"), &headers)?
            .write_all(b"Track logs cleared.")?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── POST /api/storage/format_emmc — full eMMC reformat, wipes everything
    // (requires auth) ─────────────────────────────────────────────────────────
    let state_fmt = state.clone();
    server.fn_handler("/api/storage/format_emmc", Method::Post, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_fmt.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        if !crate::emmc::is_mounted() {
            request.into_status_response(409)?.write_all(b"eMMC not mounted")?;
            return Ok::<(), anyhow::Error>(());
        }
        info!("HTTP: eMMC format requested");
        let _ = std::thread::Builder::new().stack_size(8192).spawn(|| {
            // Reformats in place via the existing card handle (no SDMMC
            // host/slot re-init — see emmc::format() for why that matters).
            // Wipes the map cache + track logs along with everything else,
            // so re-create a fresh empty cache index afterwards.
            if crate::emmc::format() {
                crate::map_cache::clear();
            }
        });
        let headers = [("Content-Type", "text/plain; charset=UTF-8")];
        request.into_response(200, Some("OK"), &headers)?
            .write_all(b"eMMC format started. This may take a few seconds.")?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET /terminal — serial terminal page (requires auth) ─────────────────
    let state_term = state.clone();
    server.fn_handler("/terminal", Method::Get, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_term.session) {
            request.into_response(302, Some("Found"), &[("Location", "/login")])?
                .write_all(b"")?;
            return Ok::<(), anyhow::Error>(());
        }
        let html = build_terminal_html();
        let headers = [("Content-Type", "text/html; charset=utf-8")];
        request.into_response(200, Some("OK"), &headers)?
            .write_all(html.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET /static/xterm.js — serve embedded xterm.js ───────────────────────
    server.fn_handler("/static/xterm.js", Method::Get, |request| {
        let headers = [
            ("Content-Type", "application/javascript"),
            ("Cache-Control", "public, max-age=86400"),
        ];
        request.into_response(200, Some("OK"), &headers)?
            .write_all(include_bytes!("../static/xterm.min.js"))?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET /static/xterm.css — serve embedded xterm.css ─────────────────────
    server.fn_handler("/static/xterm.css", Method::Get, |request| {
        let headers = [
            ("Content-Type", "text/css"),
            ("Cache-Control", "public, max-age=86400"),
        ];
        request.into_response(200, Some("OK"), &headers)?
            .write_all(include_bytes!("../static/xterm.min.css"))?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET /static/xterm-addon-fit.js — serve embedded fit addon ───────────
    server.fn_handler("/static/xterm-addon-fit.js", Method::Get, |request| {
        let headers = [
            ("Content-Type", "application/javascript"),
            ("Cache-Control", "public, max-age=86400"),
        ];
        request.into_response(200, Some("OK"), &headers)?
            .write_all(include_bytes!("../static/xterm-addon-fit.min.js"))?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET /api/boot_log — boot history from NVS (requires auth) ───────────
    let state_bl = state.clone();
    server.fn_handler("/api/boot_log", Method::Get, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_bl.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        let log = crate::boot_log::read_log();
        let entries: Vec<&str> = if log.is_empty() { Vec::new() } else { log.lines().collect() };
        let json_entries = entries.iter()
            .map(|e| format!("\"{}\"", e.replace('"', "\\\"")))
            .collect::<Vec<_>>()
            .join(",");
        let json = format!("{{\"entries\":[{}]}}", json_entries);
        let headers = [("Content-Type", "application/json")];
        request.into_response(200, Some("OK"), &headers)?
            .write_all(json.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET /api/mapinfo — map metadata + trail (requires auth) ─────────────
    let state_mi = state.clone();
    server.fn_handler("/api/mapinfo", Method::Get, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_mi.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        let (clat, clon) = crate::map_display::get_map_center();
        let center_valid = !clat.is_nan() && !clon.is_nan();
        let clat_s = if center_valid { clat } else { 0.0_f64 };
        let clon_s = if center_valid { clon } else { 0.0_f64 };
        let zoom    = crate::map_display::get_zoom();
        let has_map = crate::map_display::has_map();
        let ver     = crate::map_display::version();
        let trail   = crate::map_display::get_trail();
        let gps     = crate::gps_parser::get_data();
        let cur_lat = if gps.lat_dd.is_nan() { 0.0_f64 } else { gps.lat_dd };
        let cur_lon = if gps.lon_dd.is_nan() { 0.0_f64 } else { gps.lon_dd };
        let cur_valid = !gps.lat_dd.is_nan() && gps.fix_quality > 0;
        // Accumulate trail and trigger a background map fetch whenever the WebUI polls
        if cur_valid {
            crate::map_display::push_trail(gps.lat_dd, gps.lon_dd);
            crate::map_display::trigger(gps.lat_dd, gps.lon_dd);
        }
        let trail_json: String = trail.iter()
            .map(|&(la, lo)| format!("[{:.6},{:.6}]", la, lo))
            .collect::<Vec<_>>()
            .join(",");
        let sats_json: String = gps.sats.iter()
            .filter(|s| s.snr > 0 && s.elevation >= 0)
            .map(|s| format!(
                r#"{{"s":"{sys}","p":{prn},"e":{el},"a":{az},"q":{snr},"u":{use_}}}"#,
                sys  = s.system,
                prn  = s.prn,
                el   = s.elevation,
                az   = s.azimuth,
                snr  = s.snr,
                use_ = s.in_use,
            ))
            .collect::<Vec<_>>()
            .join(",");
        let json = format!(
            r#"{{"has_map":{hm},"version":{ver},"center_lat":{clat:.6},"center_lon":{clon:.6},"center_valid":{cv},"zoom":{zoom},"cur_lat":{cur_lat:.6},"cur_lon":{cur_lon:.6},"cur_valid":{cur_valid},"trail":[{trail}],"sats":[{sats}]}}"#,
            hm        = has_map,
            ver       = ver,
            clat      = clat_s,
            clon      = clon_s,
            cv        = center_valid,
            zoom      = zoom,
            cur_lat   = cur_lat,
            cur_lon   = cur_lon,
            cur_valid = cur_valid,
            trail     = trail_json,
            sats      = sats_json,
        );
        let headers = [("Content-Type", "application/json")];
        request.into_response(200, Some("OK"), &headers)?
            .write_all(json.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET /api/map/pixels — raw RGB565 image data (requires auth) ──────────
    let state_px = state.clone();
    server.fn_handler("/api/map/pixels", Method::Get, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_px.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        let mut bytes: Vec<u8> = Vec::new();
        crate::map_display::with_pixels(|px| {
            bytes.reserve(px.len() * 2);
            for &v in px {
                bytes.extend_from_slice(&v.to_le_bytes());
            }
        });
        if bytes.is_empty() {
            request.into_status_response(204)?.write_all(b"")?;
        } else {
            let headers = [("Content-Type", "application/octet-stream")];
            let mut resp = request.into_response(200, Some("OK"), &headers)?;
            // Send in 4 KB chunks to avoid ENOBUFS on the TCP send buffer
            for chunk in bytes.chunks(4096) {
                resp.write_all(chunk)?;
            }
        }
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET /api/mapkey — Google Maps API key + map type for browser use ──────
    let state_mk = state.clone();
    server.fn_handler("/api/mapkey", Method::Get, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_mk.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        let api_key  = crate::map_display::api_key();
        let map_type = state_mk.config.lock().unwrap().map_type.clone();
        let json = format!("{{{:?}:{:?},{:?}:{:?}}}", "api_key", api_key, "map_type", map_type);
        let headers = [("Content-Type", "application/json")];
        request.into_response(200, Some("OK"), &headers)?
            .write_all(json.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET /map — map view page (requires auth) ──────────────────────────────
    let state_map = state.clone();
    server.fn_handler("/map", Method::Get, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_map.session) {
            request.into_response(302, Some("Found"), &[("Location", "/login")])?
                .write_all(b"")?;
            return Ok::<(), anyhow::Error>(());
        }
        let html = build_map_html();
        let headers = [("Content-Type", "text/html; charset=utf-8")];
        request.into_response(200, Some("OK"), &headers)?
            .write_all(html.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── POST /api/map/zoomin — zoom in one level (requires auth) ─────────────
    let state_zi = state.clone();
    server.fn_handler("/api/map/zoomin", Method::Post, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_zi.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        crate::map_display::zoom_in();
        let zoom = crate::map_display::get_zoom();
        let key = "zoom";
        let headers = [("Content-Type", "application/json")];
        let json = format!("{{{:?}:{}}}", key, zoom);
        request.into_response(200, Some("OK"), &headers)?
            .write_all(json.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── POST /api/map/zoomout — zoom out one level (requires auth) ───────────
    let state_zo = state.clone();
    server.fn_handler("/api/map/zoomout", Method::Post, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_zo.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        crate::map_display::zoom_out();
        let zoom = crate::map_display::get_zoom();
        let key = "zoom";
        let headers = [("Content-Type", "application/json")];
        let json = format!("{{{:?}:{}}}", key, zoom);
        request.into_response(200, Some("OK"), &headers)?
            .write_all(json.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── POST /api/map/toggletype — toggle satellite/roadmap (requires auth) ──
    let state_tt = state.clone();
    server.fn_handler("/api/map/toggletype", Method::Post, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_tt.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        let is_sat = crate::map_display::toggle_map_type();
        crate::map_display::reset_last_pos();
        let map_type = if is_sat { "satellite" } else { "roadmap" };
        // Persist to NVS and config mutex
        {
            let mut cfg = state_tt.config.lock().unwrap();
            cfg.map_type = map_type.to_string();
            nvs_write_all(&cfg);
        }
        let headers = [("Content-Type", "application/json")];
        let json = format!("{{{:?}:{:?}}}", "map_type", map_type);
        request.into_response(200, Some("OK"), &headers)?
            .write_all(json.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET /api/gps — GPS & satellite data (requires auth) ─────────────────
    let state_gps = state.clone();
    server.fn_handler("/api/gps", Method::Get, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_gps.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        let d = crate::gps_parser::get_data();
        let location = crate::gps_parser::get_location_name();
        let sats_json: String = d.sats.iter().map(|s| {
            format!(
                r#"{{"sys":"{}","prn":{},"el":{},"az":{},"snr":{},"use":{}}}"#,
                s.system, s.prn, s.elevation, s.azimuth, s.snr, s.in_use
            )
        }).collect::<Vec<_>>().join(",");
        let lat_dd = if d.lat_dd.is_nan() { 0.0_f64 } else { d.lat_dd };
        let lon_dd = if d.lon_dd.is_nan() { 0.0_f64 } else { d.lon_dd };
        let json = format!(
            r#"{{"time":"{time}","date":"{date}","lat":"{lat}","lon":"{lon}","lat_dd":{lat_dd:.6},"lon_dd":{lon_dd:.6},"fix_quality":{fq},"fix_mode":{fm},"num_sats":{ns},"hdop":{hdop:.1},"pdop":{pdop:.1},"vdop":{vdop:.1},"alt":{alt:.1},"speed":{spd:.2},"course":{crs:.1},"valid":{valid},"location":"{loc}","sats":[{sats}]}}"#,
            time  = d.time_str,
            date  = d.date_str,
            lat   = esc(&d.lat_str),
            lon   = esc(&d.lon_str),
            lat_dd = lat_dd,
            lon_dd = lon_dd,
            fq    = d.fix_quality,
            fm    = d.fix_mode,
            ns    = d.num_sats,
            hdop  = d.hdop,
            pdop  = d.pdop,
            vdop  = d.vdop,
            alt   = d.altitude,
            spd   = d.speed_kn,
            crs   = d.course,
            valid = d.valid,
            loc   = esc(&location),
            sats  = sats_json,
        );
        let headers = [("Content-Type", "application/json")];
        request.into_response(200, Some("OK"), &headers)?
            .write_all(json.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET /tracks — recorded track viewer page (requires auth) ─────────────
    let state_tp = state.clone();
    server.fn_handler("/tracks", Method::Get, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_tp.session) {
            request.into_response(302, Some("Found"), &[("Location", "/login")])?
                .write_all(b"")?;
            return Ok::<(), anyhow::Error>(());
        }
        let html = build_tracks_html();
        let headers = [("Content-Type", "text/html; charset=utf-8")];
        request.into_response(200, Some("OK"), &headers)?
            .write_all(html.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET /api/quakes — recent earthquakes from P2P地震情報 (requires auth) ─
    let state_qk = state.clone();
    server.fn_handler("/api/quakes", Method::Get, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_qk.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        let items: Vec<String> = crate::quake::get().iter().map(|q| format!(
            r#"{{"time":{time},"name":{name},"lat":{lat:.3},"lon":{lon:.3},"depth":{depth},"mag":{mag:.1},"scale":{scale},"intensity":{inten},"tsunami":{tsu}}}"#,
            time  = serde_json::to_string(&q.time).unwrap_or_default(),
            name  = serde_json::to_string(&q.name).unwrap_or_default(),
            lat = q.lat, lon = q.lon, depth = q.depth, mag = q.mag, scale = q.scale,
            inten = serde_json::to_string(q.intensity_jp()).unwrap_or_default(),
            tsu   = serde_json::to_string(&q.tsunami).unwrap_or_default(),
        )).collect();
        let eew = match crate::quake::eew() {
            Some(e) => {
                // Map framing both the epicenter and this unit (same as the TFT)
                let (vl, vo, vz, dist) = crate::quake::eew_view(&e);
                let view = if vl.is_nan() { "null".to_string() } else { format!(
                    r#"{{"lat":{:.4},"lon":{:.4},"zoom":{},"dist_km":{}}}"#, vl, vo, vz,
                    dist.map(|d| format!("{:.0}", d)).unwrap_or_else(|| "null".into())) };
                format!(
                r#"{{"time":{t},"name":{n},"mag":{m:.1},"depth":{d},"intensity":{i},"areas":{a},"view":{v}}}"#,
                t = serde_json::to_string(&e.time).unwrap_or_default(),
                n = serde_json::to_string(&e.name).unwrap_or_default(),
                m = e.mag, d = e.depth,
                i = serde_json::to_string(e.intensity_jp()).unwrap_or_default(),
                a = serde_json::to_string(&e.areas).unwrap_or_default(),
                v = view)
            }
            None => "null".to_string(),
        };
        let json = format!(r#"{{"enabled":{},"ready":{},"eew":{},"quakes":[{}]}}"#,
            crate::quake::enabled(), crate::quake::last_ok_secs() != 0, eew, items.join(","));
        let headers = [("Content-Type", "application/json; charset=utf-8"), ("Cache-Control", "no-store")];
        request.into_response(200, Some("OK"), &headers)?
            .write_all(json.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET /api/tracks — list of recorded track dates (requires auth) ───────
    let state_tl = state.clone();
    server.fn_handler("/api/tracks", Method::Get, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_tl.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        let mut files: Vec<String> = crate::track_log::list_files()
            .into_iter()
            .filter(|d| crate::track_log::path_for(d).is_some())
            .collect();
        files.sort_by(|a, b| b.cmp(a)); // newest first
        let list = files.iter().map(|d| format!("\"{}\"", d)).collect::<Vec<_>>().join(",");
        let json = format!("{{\"files\":[{}]}}", list);
        let headers = [("Content-Type", "application/json"), ("Cache-Control", "no-store")];
        request.into_response(200, Some("OK"), &headers)?
            .write_all(json.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET /api/track?date=YYYY-MM-DD — one day's points (requires auth) ────
    // Streamed straight from the CSV: a day logged every 5 s is up to ~17 000
    // points, too many to build as one JSON string in RAM alongside everything
    // else the device is doing. Each point is [ "HH:MM:SS", lat, lon, alt_m, speed_kn ].
    let state_tk = state.clone();
    server.fn_handler("/api/track", Method::Get, move |request| {
        use std::io::BufRead;
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_tk.session) {
            request.into_status_response(401)?.write_all(b"Unauthorized")?;
            return Ok::<(), anyhow::Error>(());
        }
        let date = query_param(request.uri(), "date").unwrap_or_default();
        let Some(path) = crate::track_log::path_for(&date) else {
            request.into_status_response(400)?.write_all(b"Bad date")?;
            return Ok::<(), anyhow::Error>(());
        };
        let Ok(file) = std::fs::File::open(&path) else {
            request.into_status_response(404)?.write_all(b"Not found")?;
            return Ok::<(), anyhow::Error>(());
        };

        let headers = [("Content-Type", "application/json"), ("Cache-Control", "no-store")];
        let mut resp = request.into_response(200, Some("OK"), &headers)?;
        resp.write_all(format!("{{\"date\":\"{}\",\"pts\":[", date).as_bytes())?;

        let mut out = String::with_capacity(4096);
        let mut first = true;
        // skip(1): header row
        for line in std::io::BufReader::with_capacity(1024, file).lines().skip(1).map_while(Result::ok) {
            let cols: Vec<&str> = line.split(',').collect();
            if cols.len() < 5 { continue; }
            let (Ok(lat), Ok(lon)) = (cols[1].parse::<f64>(), cols[2].parse::<f64>()) else { continue };
            if !lat.is_finite() || !lon.is_finite() { continue; }
            let fin = |s: &str| s.parse::<f32>().ok().filter(|v| v.is_finite()).unwrap_or(0.0);
            let t = cols[0].get(11..19)
                .filter(|t| t.bytes().all(|c| c.is_ascii_digit() || c == b':'))
                .unwrap_or("--:--:--");
            if !first { out.push(','); }
            first = false;
            out.push_str(&format!("[\"{}\",{:.6},{:.6},{:.1},{:.1}]", t, lat, lon, fin(cols[3]), fin(cols[4])));
            if out.len() >= 3500 {
                resp.write_all(out.as_bytes())?;
                out.clear();
            }
        }
        out.push_str("]}");
        resp.write_all(out.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET /skyview — satellite sky view page (requires auth) ───────────────
    let state_sv = state.clone();
    server.fn_handler("/skyview", Method::Get, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_sv.session) {
            request.into_response(302, Some("Found"), &[("Location", "/login")])?
                .write_all(b"")?;
            return Ok::<(), anyhow::Error>(());
        }
        let html = build_skyview_html();
        let headers = [("Content-Type", "text/html; charset=utf-8")];
        request.into_response(200, Some("OK"), &headers)?
            .write_all(html.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── GET /gpsstatus — GPS status page (requires auth) ─────────────────────
    let state_gs = state.clone();
    server.fn_handler("/gpsstatus", Method::Get, move |request| {
        let cookie_hdr = request.header("Cookie").map(str::to_owned);
        if !is_auth(cookie_hdr.as_deref(), &state_gs.session) {
            request.into_response(302, Some("Found"), &[("Location", "/login")])?
                .write_all(b"")?;
            return Ok::<(), anyhow::Error>(());
        }
        let html = build_gpsstatus_html();
        let headers = [("Content-Type", "text/html; charset=utf-8")];
        request.into_response(200, Some("OK"), &headers)?
            .write_all(html.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // ── WebSocket /ws/serial — bidirectional serial bridge ──────────────────
    let _state_ws = state.clone();
    server.ws_handler("/ws/serial", move |ws| {
        // ── Connection open ─────────────────────────────────────────────
        if ws.is_new() {
            let fd = ws.session();
            // Port name will be sent as first text frame from client
            info!("[WS] new serial connection fd={}", fd);
            return Ok::<(), anyhow::Error>(());
        }
        // ── Connection closed ───────────────────────────────────────────
        if ws.is_closed() {
            let fd = ws.session();
            crate::usb_host::ws_remove_sender("com1", fd);
            info!("[WS] serial connection closed fd={}", fd);
            return Ok::<(), anyhow::Error>(());
        }
        // ── Verify auth via session cookie ──────────────────────────────
        // (WebSocket auth is checked once at open; data frames trust the connection)

        // ── Receive data ────────────────────────────────────────────────
        // Phase 1: get frame metadata
        let (frame_type, len) = ws.recv(&mut [])?;
        if len == 0 {
            return Ok(());
        }
        let mut buf = vec![0u8; len];
        ws.recv(&mut buf)?;

        match frame_type {
            embedded_svc::ws::FrameType::Text(_) => {
                // Text frame = control message (JSON): {"cmd":"init","port":"com1"}
                let msg = String::from_utf8_lossy(&buf);
                if let Some(port) = parse_ws_port(&msg) {
                    let fd = ws.session();
                    let sender = ws.create_detached_sender()?;
                    // Remove from previous port lists first
                    crate::usb_host::ws_remove_sender("com1", fd);
                    // Register for new port
                    crate::usb_host::ws_register_sender(&port, fd, sender);
                }
            }
            embedded_svc::ws::FrameType::Binary(_) => {
                // Binary frame = serial TX data from browser
                // Determine which port this fd is registered on
                let fd = ws.session();
                let port = ws_fd_port(fd);
                if !port.is_empty() {
                    crate::usb_host::serial_write(&port, &buf);
                }
            }
            _ => {}
        }
        Ok::<(), anyhow::Error>(())
    })?;

    info!("HTTP config server started on port 80");
    Ok(server)
}

// ─── WebSocket helpers ───────────────────────────────────────────────────────

/// Parse port name from WS init message like {"cmd":"init","port":"com1"}
fn parse_ws_port(msg: &str) -> Option<String> {
    // Simple parse — avoid pulling in serde_json
    if let Some(idx) = msg.find("\"port\"") {
        let rest = &msg[idx + 6..];
        if let Some(start) = rest.find('"') {
            let rest = &rest[start + 1..];
            if let Some(end) = rest.find('"') {
                let port = &rest[..end];
                match port {
                    "com1" => return Some(port.to_string()),
                    _ => {}
                }
            }
        }
    }
    None
}

/// Look up which port a WebSocket fd is registered on.
/// Delegates to ws_port_for_fd which uses WS_FD_SET — a separate lock from
/// WS_SENDERS_COM1 — to avoid deadlocking against the ws_send thread.
fn ws_fd_port(fd: i32) -> String {
    crate::usb_host::ws_port_for_fd(fd).to_string()
}

// ─── Terminal HTML page ──────────────────────────────────────────────────────

fn build_terminal_html() -> String {
    r#"<!DOCTYPE html><html lang="ja"><head>
<meta charset="UTF-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>SSH-Serial-Bridge Terminal</title>
<link rel="stylesheet" href="/static/xterm.css">
<style>
*{box-sizing:border-box;margin:0;padding:0}
body{background:#111827;color:#e2e8f0;font-family:'Segoe UI',Arial,sans-serif;height:100vh;display:flex;flex-direction:column}
.toolbar{display:flex;align-items:center;gap:10px;padding:8px 14px;background:#1e2d45;
  border-bottom:1px solid #243b55;flex-shrink:0}
.toolbar h1{font-size:1em;color:#93c5fd;letter-spacing:.05em;margin-right:auto}
.toolbar select,.toolbar button{padding:6px 14px;border-radius:6px;border:1px solid #2d4a6a;
  background:#0f172a;color:#e2e8f0;font-size:.85em;cursor:pointer}
.toolbar select:focus,.toolbar button:focus{outline:none;border-color:#60a5fa}
.toolbar button:hover{filter:brightness(1.2)}
.btn-connect{background:#166534;border-color:#16a34a;font-weight:700}
.btn-connect.connected{background:#991b1b;border-color:#dc2626}
.btn-back{background:#374151;text-decoration:none;color:#e2e8f0;display:inline-flex;
  align-items:center;padding:6px 14px;border-radius:6px;font-size:.85em;font-weight:600}
.status{font-size:.78em;padding:3px 10px;border-radius:12px;font-weight:700}
.status.ok{background:#14532d;color:#4ade80}.status.off{background:#4c0519;color:#fb7185}
#terminal-container{flex:1;padding:4px;overflow:hidden}
</style></head><body>
<div class="toolbar">
  <h1>&#x1F4BB; Serial Terminal</h1>
  <label style="font-size:.8em;color:#94a3b8">Port:</label>
  <select id="port-select">
    <option value="com1">COM1</option>
  </select>
  <button id="btn-conn" class="toolbar btn-connect" onclick="toggleConnection()">Connect</button>
  <span id="ws-status" class="status off">Disconnected</span>
  <a href="/" class="btn-back">&#x2190; Config</a>
</div>
<div id="terminal-container"></div>
<script src="/static/xterm.js"></script>
<script src="/static/xterm-addon-fit.js"></script>
<script>
const term = new Terminal({
  cursorBlink: true,
  fontSize: 14,
  theme: {
    background: '#0f172a',
    foreground: '#e2e8f0',
    cursor: '#60a5fa',
    selectionBackground: '#334155'
  }
});
const fitAddon = new FitAddon.FitAddon();
term.loadAddon(fitAddon);
term.open(document.getElementById('terminal-container'));
fitAddon.fit();
window.addEventListener('resize', () => fitAddon.fit());

let ws = null;
const portSel = document.getElementById('port-select');
const btnConn = document.getElementById('btn-conn');
const wsStatus = document.getElementById('ws-status');

function toggleConnection() {
  if (ws && ws.readyState === WebSocket.OPEN) {
    ws.close();
  } else {
    connect();
  }
}

function connect() {
  const port = portSel.value;
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  ws = new WebSocket(proto + '//' + location.host + '/ws/serial');
  ws.binaryType = 'arraybuffer';

  ws.onopen = () => {
    wsStatus.textContent = 'Connected (' + port.toUpperCase() + ')';
    wsStatus.className = 'status ok';
    btnConn.textContent = 'Disconnect';
    btnConn.classList.add('connected');
    portSel.disabled = true;
    // Send init message with port selection
    ws.send(JSON.stringify({cmd: 'init', port: port}));
    term.focus();
  };

  ws.onmessage = (ev) => {
    if (ev.data instanceof ArrayBuffer) {
      term.write(new Uint8Array(ev.data));
    } else {
      term.write(ev.data);
    }
  };

  ws.onclose = () => {
    wsStatus.textContent = 'Disconnected';
    wsStatus.className = 'status off';
    btnConn.textContent = 'Connect';
    btnConn.classList.remove('connected');
    portSel.disabled = false;
    ws = null;
  };

  ws.onerror = (err) => {
    console.error('WS error:', err);
    term.write('\r\n\x1b[31m[WebSocket error]\x1b[0m\r\n');
  };
}

// Terminal input → WebSocket (binary)
term.onData((data) => {
  if (ws && ws.readyState === WebSocket.OPEN) {
    const encoder = new TextEncoder();
    ws.send(encoder.encode(data));
  }
});
</script>
</body></html>"#.to_string()
}

// ─── Satellite Sky View HTML page ────────────────────────────────────────────

fn build_skyview_html() -> String {
r##"<!DOCTYPE html><html lang="ja"><head>
<meta charset="UTF-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Sky View</title>
<style>*{box-sizing:border-box;margin:0;padding:0}
body{background:#111827;color:#e2e8f0;font-family:'Segoe UI',Arial,sans-serif;display:flex;flex-direction:column;min-height:100vh}
.tb{display:flex;align-items:center;gap:10px;padding:8px 14px;background:#1e2d45;border-bottom:1px solid #243b55;flex-shrink:0}
.tb h1{font-size:1em;color:#93c5fd;margin-right:auto}
.bb{background:#374151;text-decoration:none;color:#e2e8f0;display:inline-flex;align-items:center;padding:6px 14px;border-radius:6px;font-size:.85em;font-weight:600}
.ct{display:flex;flex-direction:column;align-items:center;padding:12px;gap:10px;flex:1}
.lg{display:flex;gap:10px;flex-wrap:wrap;justify-content:center;font-size:.8em}
.li{display:flex;align-items:center;gap:4px}.ld{width:9px;height:9px;border-radius:50%}
.il{font-size:.78em;color:#64748b;text-align:center}
.bdg{padding:2px 10px;border-radius:20px;font-size:.78em;font-weight:700}</style></head><body>
<div class="tb">
  <h1>&#x1F4E1; Satellite Sky View</h1>
  <span id="fix" class="bdg" style="background:#4c0519;color:#fb7185">No Fix</span>
  <a href="/" class="bb">&#x2190; Config</a></div>
<div class="ct">
  <canvas id="sky" width="280" height="280" style="background:#0a1628;border-radius:50%"></canvas>
  <div class="lg">
    <div class="li"><div class="ld" style="background:#4ade80"></div><span>GPS</span></div>
    <div class="li"><div class="ld" style="background:#60a5fa"></div><span>Galileo</span></div>
    <div class="li"><div class="ld" style="background:#facc15"></div><span>BeiDou</span></div>
    <div class="li"><div class="ld" style="background:#fb923c"></div><span>GLONASS</span></div>
    <div class="li"><div class="ld" style="background:#e879f9"></div><span>QZSS</span></div>
  </div>
  <div id="il" class="il">Loading...</div>
  <div id="bars"></div>
</div>
<script>
const C={G:'#4ade80',A:'#60a5fa',B:'#facc15',L:'#fb923c',Q:'#e879f9'};
const cv=document.getElementById('sky'),cx=cv.getContext('2d');
const W=cv.width,H=cv.height,X=W/2,Y=H/2,R=W/2-14;
function draw(sats){
  cx.clearRect(0,0,W,H);
  cx.strokeStyle='#1e3a5f';cx.lineWidth=1;
  [1/3,2/3,1].forEach(f=>{cx.beginPath();cx.arc(X,Y,R*f,0,2*Math.PI);cx.stroke();});
  'NESW'.split('').forEach((d,i)=>{
    const a=i*Math.PI/2-Math.PI/2;
    cx.beginPath();cx.moveTo(X,Y);cx.lineTo(X+R*Math.cos(a),Y+R*Math.sin(a));
    cx.strokeStyle='#1e3a5f';cx.stroke();
    cx.fillStyle='#475569';cx.font='bold 10px sans-serif';cx.textAlign='center';cx.textBaseline='middle';
    cx.fillText(d,X+(R+10)*Math.cos(a),Y+(R+10)*Math.sin(a));
  });
  cx.fillStyle='#334155';cx.font='8px sans-serif';cx.textAlign='left';
  cx.fillText('30\u00b0',X+R*2/3+2,Y-2);cx.fillText('60\u00b0',X+R/3+2,Y-2);
  sats.forEach(s=>{
    const er=(90-s.el)/90,ar=s.az*Math.PI/180-Math.PI/2;
    const sx=X+R*er*Math.cos(ar),sy=Y+R*er*Math.sin(ar);
    const col=C[s.sys]||'#94a3b8';
    cx.beginPath();cx.arc(sx,sy,s.use?7:5,0,2*Math.PI);
    cx.fillStyle=s.use?col:col+'55';cx.fill();
    if(s.use){cx.strokeStyle='#fff';cx.lineWidth=1.5;cx.stroke();}
    cx.fillStyle='#000';cx.font='bold 7px sans-serif';cx.textAlign='center';cx.textBaseline='middle';
    cx.fillText(s.prn,sx,sy);
  });
}
function bars(sats){
  const v=sats.filter(s=>s.snr>0).sort((a,b)=>b.snr-a.snr);
  if(!v.length){document.getElementById('bars').innerHTML='';return;}
  const bw=9,gap=2,mh=44,tot=v.length*(bw+gap);
  let h='<svg width="'+tot+'" height="'+(mh+14)+'">';
  v.forEach((s,i)=>{
    const col=C[s.sys]||'#94a3b8',hh=Math.max(2,Math.round(s.snr/50*mh)),x=i*(bw+gap);
    h+='<rect x="'+x+'" y="'+(mh-hh)+'" width="'+bw+'" height="'+hh+'" fill="'+(s.use?col:col+'70')+'"/>';
    h+='<text x="'+(x+bw/2)+'" y="'+(mh+11)+'" text-anchor="middle" fill="#64748b" font-size="7">'+s.prn+'</text>';
  });
  document.getElementById('bars').innerHTML=h+'</svg>';
}
function refresh(){
  fetch('/api/gps').then(r=>r.json()).then(d=>{
    draw(d.sats);bars(d.sats);
    const fb=document.getElementById('fix');
    fb.textContent=d.valid?('Fix '+d.fix_mode+'D ('+d.num_sats+')'):('No Fix');
    fb.style.background=d.valid?'#14532d':'#4c0519';
    fb.style.color=d.valid?'#4ade80':'#fb7185';
    document.getElementById('il').textContent=
      d.time+' UTC  '+d.lat+'  '+d.lon+'  Alt:'+d.alt.toFixed(1)+'m';
  }).catch(()=>{});
}
refresh();setInterval(refresh,2000);
</script></body></html>"##.to_string()
}

// ─── GPS Status HTML page ─────────────────────────────────────────────────────

fn build_gpsstatus_html() -> String {
r##"<!DOCTYPE html><html lang="ja"><head>
<meta charset="UTF-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>GPS Status</title>
<style>*{box-sizing:border-box;margin:0;padding:0}
body{background:#111827;color:#e2e8f0;font-family:'Segoe UI',Arial,sans-serif;min-height:100vh}
.tb{display:flex;align-items:center;gap:10px;padding:8px 14px;background:#1e2d45;border-bottom:1px solid #243b55}
.tb h1{font-size:1em;color:#93c5fd;margin-right:auto}
.bb{background:#374151;text-decoration:none;color:#e2e8f0;display:inline-flex;align-items:center;padding:6px 14px;border-radius:6px;font-size:.85em;font-weight:600}
.pg{max-width:680px;margin:0 auto;padding:12px}
.card{background:#1e2d45;border-radius:10px;padding:14px 16px;margin-bottom:10px;border:1px solid #243b55}
.st{display:flex;align-items:center;gap:8px;color:#93c5fd;font-size:.8em;font-weight:700;text-transform:uppercase;letter-spacing:.06em;margin-bottom:10px;padding-bottom:6px;border-bottom:1px solid #1e3a5f}
.sg{display:grid;grid-template-columns:auto 1fr;gap:4px 14px}
.sk{font-size:.75em;font-weight:600;color:#64748b;text-transform:uppercase;letter-spacing:.05em;white-space:nowrap}
.sv{font-size:.88em;color:#7dd3fc;font-weight:600}
.bdg{display:inline-block;padding:2px 9px;border-radius:20px;font-size:.8em;font-weight:700}
.ok{background:#14532d;color:#4ade80;border:1px solid #166534}
.wa{background:#451a03;color:#fb923c;border:1px solid #7c2d12}
.er{background:#4c0519;color:#fb7185;border:1px solid #881337}
table{width:100%;border-collapse:collapse;font-size:.82em}
th{text-align:left;color:#475569;font-size:.75em;padding:4px 6px;border-bottom:1px solid #243b55}
td{padding:4px 6px;border-bottom:1px solid #1e3a5f;color:#cbd5e1}
.sb{display:inline-block;height:7px;border-radius:3px;vertical-align:middle}</style></head><body>
<div class="tb">
  <h1>&#x1F6F0; GPS Status</h1>
  <a href="/" class="bb">&#x2190; Config</a></div>
<div class="pg">
  <div class="card">
    <div class="st">&#x1F4CD; Position &amp; Fix</div>
    <div id="pos" class="sg"><span class="sk">Loading</span><span class="sv">...</span></div>
  </div>
  <div class="card">
    <div class="st">&#x1F4E1; Satellites</div>
    <div id="satsum" style="display:flex;gap:8px;flex-wrap:wrap;margin-bottom:10px;font-size:.82em"></div>
    <table><thead><tr>
      <th>Sys</th><th>PRN</th><th>El</th><th>Az</th><th>SNR (dBHz)</th><th>Use</th>
    </tr></thead><tbody id="sattbl"></tbody></table>
  </div>
</div>
<script>
const C={G:'#4ade80',A:'#60a5fa',B:'#facc15',L:'#fb923c',Q:'#e879f9'};
const SN={G:'GPS',A:'Galileo',B:'BeiDou',L:'GLONASS',Q:'QZSS'};
const SA={G:'GPS',A:'GAL',B:'BDS',L:'GLO',Q:'QZS'};
function bdg(t,c){return '<span class="bdg '+c+'">'+t+'</span>';}
function fq(q,m){
  if(!q)return bdg('No Fix','er');
  return m===3?bdg('3D Fix','ok'):m===2?bdg('2D Fix','wa'):bdg('Fix','ok');
}
function refresh(){
  fetch('/api/gps').then(r=>r.json()).then(d=>{
    var loc=d.location?('<br><span style="color:#94a3b8;font-size:.85em">'+d.location+'</span>'):'';
    document.getElementById('pos').innerHTML=
      '<span class="sk">Fix</span><span class="sv">'+fq(d.fix_quality,d.fix_mode)+'</span>'+
      '<span class="sk">Time UTC</span><span class="sv">'+d.date+' '+d.time+'</span>'+
      '<span class="sk">Latitude</span><span class="sv">'+d.lat+'</span>'+
      '<span class="sk">Longitude</span><span class="sv">'+d.lon+loc+'</span>'+
      '<span class="sk">Altitude</span><span class="sv">'+d.alt.toFixed(1)+' m</span>'+
      '<span class="sk">Speed</span><span class="sv">'+(d.speed*1.852).toFixed(1)+' km/h ('+d.speed.toFixed(1)+' kn)</span>'+
      '<span class="sk">Course</span><span class="sv">'+d.course.toFixed(1)+'\u00b0</span>'+
      '<span class="sk">H/P/VDOP</span><span class="sv">'+d.hdop.toFixed(1)+' / '+d.pdop.toFixed(1)+' / '+d.vdop.toFixed(1)+'</span>'+
      '<span class="sk">Sats Used</span><span class="sv">'+d.num_sats+'</span>';
    var cnt={};
    d.sats.forEach(s=>{if(!cnt[s.sys])cnt[s.sys]={t:0,u:0};cnt[s.sys].t++;if(s.use)cnt[s.sys].u++;});
    document.getElementById('satsum').innerHTML=Object.keys(cnt).map(k=>
      '<span style="display:inline-flex;align-items:center;gap:4px;padding:3px 8px;background:#0f172a;border-radius:6px;border:1px solid #1e3a5f">'+
      '<span style="width:8px;height:8px;border-radius:50%;background:'+C[k]+';display:inline-block"></span>'+
      '<span style="color:#94a3b8;font-size:.78em">'+(SN[k]||k)+'</span>'+
      '<span style="color:#7dd3fc;font-weight:700">'+cnt[k].u+'/'+cnt[k].t+'</span></span>'
    ).join('');
    document.getElementById('sattbl').innerHTML=d.sats.slice().sort((a,b)=>b.snr-a.snr).map(s=>{
      const col=C[s.sys]||'#94a3b8';
      const w=Math.max(2,Math.round(s.snr/50*60));
      return '<tr style="opacity:'+(s.snr>0||s.use?1:.45)+'">'
        +'<td><span style="color:'+col+';font-weight:700">'+(SA[s.sys]||s.sys)+'</span></td>'
        +'<td>'+s.prn+'</td><td>'+s.el+'&deg;</td><td>'+s.az+'&deg;</td>'
        +'<td><span class="sb" style="width:'+w+'px;background:'+col+'"></span> '+s.snr+'</td>'
        +'<td>'+(s.use?bdg('\u2714','ok'):'')+'</td></tr>';
    }).join('');
  }).catch(()=>{});
}
refresh();setInterval(refresh,2000);
</script>
</body></html>"##.to_string()
}

// ─── Map View HTML page ───────────────────────────────────────────────────────

fn build_map_html() -> String {
r##"<!DOCTYPE html><html lang="ja"><head>
<meta charset="UTF-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Map View</title>
<style>*{box-sizing:border-box;margin:0;padding:0}
body{background:#111827;color:#e2e8f0;font-family:'Segoe UI',Arial,sans-serif;display:flex;flex-direction:column;min-height:100vh}
.tb{display:flex;align-items:center;gap:10px;padding:8px 14px;background:#1e2d45;border-bottom:1px solid #243b55;flex-shrink:0;flex-wrap:wrap}
.tb h1{font-size:1em;color:#93c5fd;margin-right:auto}
.bb{background:#374151;text-decoration:none;color:#e2e8f0;display:inline-flex;align-items:center;padding:6px 14px;border-radius:6px;font-size:.85em;font-weight:600}
.bdg{padding:2px 10px;border-radius:20px;font-size:.78em;font-weight:700}
.ct{display:flex;flex-direction:column;align-items:center;padding:12px;gap:8px;flex:1}
.mw{position:relative;display:block;line-height:0;max-width:640px;width:100%;min-height:240px;background:#0a1628;border-radius:6px;overflow:hidden}
.mw img{display:block;width:100%;height:auto}
#ov{position:absolute;top:0;left:0;width:100%;height:100%;pointer-events:none}
#nm{position:absolute;top:50%;left:50%;transform:translate(-50%,-50%);color:#64748b;font-size:.9em;text-align:center;line-height:1.6;pointer-events:none}
.il{font-size:.78em;color:#64748b;text-align:center;padding:0 8px}
.zb{background:#0f172a;border:1px solid #2d4a6a;color:#e2e8f0;padding:5px 14px;border-radius:6px;font-size:1.1em;cursor:pointer;font-weight:700;line-height:1}
.zb:hover{background:#1e3a5f;border-color:#60a5fa}
.tb{display:flex;align-items:center;gap:10px;padding:8px 14px;background:#1e2d45;border-bottom:1px solid #243b55;flex-shrink:0;flex-wrap:wrap}
.zb.sat{border-color:#f59e0b;color:#fcd34d}
.zb.road{border-color:#6ee7b7;color:#6ee7b7}
</style></head><body>
<div class="tb">
  <h1>&#x1F4CD; Map View</h1>
  <button class="zb" onclick="doZoom(-1)">&#x2212;</button>
  <span id="fix" class="bdg" style="background:#1e3a5f;color:#93c5fd">Zoom --</span>
  <button class="zb" onclick="doZoom(1)">+</button>
  <button class="zb" id="tbt" onclick="doToggleType()">&#x1F6F0; Sat</button>
  <a href="/map" id="back" class="bb" style="display:none">&#x1F4CD; Live</a>
  <a href="/tracks" class="bb">&#x1F9ED; Tracks</a>
  <a href="/" class="bb">&#x2190; Config</a>
</div>
<div class="ct">
  <div class="mw">
    <img id="mi" alt="">
    <canvas id="ov" width="640" height="480"></canvas>
    <div id="nm">Initializing...</div>
  </div>
  <div id="il" class="il">Loading...</div>
</div>
<script>
const MAP_W=640,MAP_H=480;
let apiKey='',mapType='satellite',mi={},lastUrl='',mapCenter=null;
// Optional focus (?lat=&lon=&zoom=): view an arbitrary point, e.g. an earthquake
// epicenter. Browser-side only — the device's own map/zoom state is untouched.
const qp=new URLSearchParams(location.search);
let focus=null;
if(qp.has('lat')&&qp.has('lon')){
  const fl=parseFloat(qp.get('lat')),fo=parseFloat(qp.get('lon'));
  if(isFinite(fl)&&isFinite(fo))focus={lat:fl,lon:fo,zoom:Math.min(16,Math.max(2,parseInt(qp.get('zoom'))||8))};
}
let quakes=[],eew=null,qTick=0;
const img=document.getElementById('mi');
const oc=document.getElementById('ov'),octx=oc.getContext('2d');
const nm=document.getElementById('nm');

async function init(){
  try{
    const d=await fetch('/api/mapkey').then(r=>r.json());
    apiKey=d.api_key||'';mapType=d.map_type||'satellite';
    updateTypeBtn();
  }catch(e){}
  refresh();setInterval(refresh,3000);
}

function updateTypeBtn(){
  const b=document.getElementById('tbt');
  if(mapType==='satellite'){b.textContent='\u{1F6F0} Sat';b.className='zb sat';}
  else{b.textContent='\u{1F5FA} Road';b.className='zb road';}
}

// Returns the best available map center:
// prefer ESP32's stored center (center_valid); fall back to current GPS
// position rounded to ~100 m grid to avoid reloading on every GPS jitter.
function getMapCenter(){
  if(focus)return{lat:focus.lat,lon:focus.lon,zoom:focus.zoom};
  if(mi.center_valid)return{lat:mi.center_lat,lon:mi.center_lon,zoom:mi.zoom};
  if(mi.cur_valid)return{
    lat:parseFloat(mi.cur_lat.toFixed(3)),
    lon:parseFloat(mi.cur_lon.toFixed(3)),
    zoom:mi.zoom};
  return null;
}

function mapUrl(){
  const c=getMapCenter();
  if(!apiKey||!c)return null;
  return'https://maps.googleapis.com/maps/api/staticmap'+
    '?center='+c.lat.toFixed(6)+','+c.lon.toFixed(6)+
    '&zoom='+c.zoom+'&size='+MAP_W+'x'+MAP_H+
    '&format=jpg&maptype='+mapType+'&key='+apiKey;
}

function toPx(lat,lon){
  if(!mapCenter)return null;
  const w=256*Math.pow(2,mapCenter.zoom);
  const lr=lat*Math.PI/180,cr=mapCenter.lat*Math.PI/180;
  const mp=Math.log(Math.tan(lr/2+Math.PI/4));
  const cm=Math.log(Math.tan(cr/2+Math.PI/4));
  const dx=(lon-mapCenter.lon)*w/360;
  const dy=-(mp-cm)*w/(2*Math.PI);
  const px=Math.round(MAP_W/2+dx),py=Math.round(MAP_H/2+dy);
  if(px<-20||px>MAP_W+20||py<-20||py>MAP_H+20)return null;
  return[px,py];
}

function drawOv(){
  octx.clearRect(0,0,oc.width,oc.height);
  const trail=mi.trail||[],n=trail.length;
  if(n>=2&&mapCenter){
    for(let i=0;i<n-1;i++){
      const p0=toPx(trail[i][0],trail[i][1]);
      const p1=toPx(trail[i+1][0],trail[i+1][1]);
      if(!p0||!p1)continue;
      const t=i/(n-1);
      const gv=Math.round((12+t*51)*255/63);
      const bv=Math.round((6+t*25)*255/31);
      octx.strokeStyle='rgb(0,'+gv+','+bv+')';
      octx.lineWidth=3;octx.beginPath();
      octx.moveTo(p0[0],p0[1]);octx.lineTo(p1[0],p1[1]);octx.stroke();
    }
    const lp=toPx(trail[n-1][0],trail[n-1][1]);
    if(lp){octx.fillStyle='#7fffd4';octx.beginPath();octx.arc(lp[0],lp[1],5,0,2*Math.PI);octx.fill();}
  }
  if(mi.cur_valid&&mapCenter){
    const cp=toPx(mi.cur_lat,mi.cur_lon);
    if(cp){
      octx.strokeStyle='#ef4444';octx.lineWidth=3;
      octx.beginPath();
      octx.moveTo(cp[0]-12,cp[1]);octx.lineTo(cp[0]+12,cp[1]);
      octx.moveTo(cp[0],cp[1]-12);octx.lineTo(cp[0],cp[1]+12);
      octx.stroke();
    }
  }
  // Earthquake epicenters (P2P地震情報) and an active EEW
  if(mapCenter){
    const SC={10:'#fde047',20:'#fde047',30:'#fb923c',40:'#fb923c',45:'#ef4444',50:'#ef4444',55:'#e879f9',60:'#e879f9',70:'#e879f9'};
    octx.font='bold 11px monospace';
    for(const q of quakes){
      const p=toPx(q.lat,q.lon);
      if(!p)continue;
      const m=q.mag>0?q.mag:1;
      const r=Math.min(14,Math.max(4,2+m*1.6));
      const col=SC[q.scale]||(m>=5?'#ef4444':m>=3?'#fb923c':'#fde047');
      octx.lineWidth=2.5;octx.strokeStyle=col;
      octx.beginPath();octx.arc(p[0],p[1],r,0,2*Math.PI);octx.stroke();
      if(focus&&Math.abs(q.lat-focus.lat)<0.0005&&Math.abs(q.lon-focus.lon)<0.0005){
        octx.strokeStyle='#ffffff';octx.lineWidth=1.5;
        octx.beginPath();octx.arc(p[0],p[1],r+4,0,2*Math.PI);octx.stroke();
      }
      const lab=(q.mag>0?'M'+q.mag.toFixed(1):'')+(q.scale>0?' I'+q.intensity:'');
      if(lab){
        octx.lineWidth=3;octx.strokeStyle='#000000';octx.strokeText(lab,p[0]+r+3,p[1]+4);
        octx.fillStyle='#ffffff';octx.fillText(lab,p[0]+r+3,p[1]+4);
      }
    }
  }
  // Sub-satellite ground points: drawn only when zoomed out (zoom <= 5)
  if(mapCenter&&mapCenter.zoom<=5&&mi.cur_valid&&mi.sats&&mi.sats.length){
    const oLat=mi.cur_lat*Math.PI/180;
    const oLon=mi.cur_lon*Math.PI/180;
    const H={'G':20200,'A':23222,'B':21528,'L':19100,'Q':42164};
    const COL={'G':'#00e000','A':'#4488ff','B':'#ffcc00','L':'#ff66ff','Q':'#ff8800'};
    const R=6371;
    octx.font='bold 10px monospace';
    for(const s of mi.sats){
      if(s.q<=0||s.e<0)continue;
      const h=H[s.s]||20200;
      const el=s.e*Math.PI/180;
      const az=s.a*Math.PI/180;
      const nadir=Math.asin(R*Math.cos(el)/(R+h));
      const central=Math.PI/2-el-nadir;
      const sLat=Math.asin(Math.sin(oLat)*Math.cos(central)
                          +Math.cos(oLat)*Math.sin(central)*Math.cos(az));
      const sLon=oLon+Math.atan2(Math.sin(az)*Math.sin(central)*Math.cos(oLat),
                                  Math.cos(central)-Math.sin(oLat)*Math.sin(sLat));
      const p=toPx(sLat*180/Math.PI,sLon*180/Math.PI);
      if(!p)continue;
      const col=COL[s.s]||'#aaaaaa';
      const r=s.u?5:4;
      octx.beginPath();octx.arc(p[0],p[1],r,0,2*Math.PI);
      if(s.u){octx.fillStyle=col;octx.fill();}
      else{octx.strokeStyle=col;octx.lineWidth=1.5;octx.stroke();}
      octx.fillStyle='#ffffff';
      octx.fillText(s.s+s.p,p[0]+6,p[1]+4);
    }
  }
}

img.onload=function(){
  nm.style.display='none';
  drawOv();
};
img.onerror=function(){
  nm.textContent='Map image load failed\n(check API key or network)';
  nm.style.display='block';
};

async function loadQuakes(){
  try{
    const q=await fetch('/api/quakes').then(r=>r.json());
    quakes=q.quakes||[];eew=q.eew||null;
    drawOv();
  }catch(e){}
}

async function refresh(){
  if(qTick++%4===0)loadQuakes();
  try{
    const d=await fetch('/api/mapinfo').then(r=>r.json());
    mi=d;
    const url=mapUrl();
    if(url){
      if(url!==lastUrl){
        mapCenter=getMapCenter();
        img.src=url;lastUrl=url;
      }
      drawOv(); // always redraw overlay (position/trail may have moved)
    }else{
      nm.style.display='block';
      nm.textContent=!apiKey?'Maps API key not set (Config page > Map Settings)':
        (d.cur_valid?'Fetching map...':'No GPS fix');
    }
    const fb=document.getElementById('fix');
    fb.textContent=d.center_valid?'Zoom '+d.zoom:'No Map';
    fb.style.background=d.cur_valid?'#14532d':'#1e3a5f';
    fb.style.color=d.cur_valid?'#4ade80':'#93c5fd';
    const lat=d.cur_valid?d.cur_lat.toFixed(5)+'\u00b0':'---';
    const lon=d.cur_valid?d.cur_lon.toFixed(5)+'\u00b0':'---';
    document.getElementById('il').textContent=focus?
      'Epicenter view: '+focus.lat.toFixed(3)+'N  '+focus.lon.toFixed(3)+'E  |  Zoom: '+focus.zoom+
        '  |  Quakes: '+quakes.length+(eew?'  |  EEW ACTIVE':'')+'  |  ':
      lat+'N  '+lon+'E  |  Zoom: '+d.zoom+'  |  Trail: '+(d.trail?d.trail.length:0)+' pts  |  Quakes: '+quakes.length+(eew?'  |  EEW ACTIVE':'');
    if(focus){
      const a=document.getElementById('back');
      if(a)a.style.display='inline-flex';
      fb.textContent='Zoom '+focus.zoom;
    }
  }catch(e){}
}

async function doZoom(dir){
  if(focus){
    focus.zoom=Math.min(16,Math.max(2,focus.zoom+dir));
    lastUrl='';document.getElementById('fix').textContent='Zoom '+focus.zoom;
    refresh();return;
  }
  try{
    const r=await fetch(dir>0?'/api/map/zoomin':'/api/map/zoomout',{method:'POST'});
    const d=await r.json();
    mi.zoom=d.zoom;lastUrl='';
    document.getElementById('fix').textContent='Zoom '+d.zoom;
    refresh();
  }catch(e){}
}

async function doToggleType(){
  try{
    const r=await fetch('/api/map/toggletype',{method:'POST'});
    const d=await r.json();
    mapType=d.map_type;lastUrl='';
    updateTypeBtn();
    refresh();
  }catch(e){}
}

init();
</script></body></html>"##.to_string()
}

// ─── Track Viewer HTML page ───────────────────────────────────────────────────
//
// Runs entirely in the browser: it fetches the track list and one day's points
// from /api/tracks and /api/track, and draws the Google Static Maps image
// itself (same as the Map page). It deliberately does not touch the device's
// own track-playback state, so browsing here never changes what the LCD shows.

fn build_tracks_html() -> String {
r##"<!DOCTYPE html><html lang="ja"><head>
<meta charset="UTF-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Track Viewer</title>
<style>*{box-sizing:border-box;margin:0;padding:0}
body{background:#111827;color:#e2e8f0;font-family:'Segoe UI',Arial,sans-serif;display:flex;flex-direction:column;min-height:100vh}
.tb{display:flex;align-items:center;gap:10px;padding:8px 14px;background:#1e2d45;border-bottom:1px solid #243b55;flex-shrink:0;flex-wrap:wrap}
.tb h1{font-size:1em;color:#93c5fd;margin-right:auto}
.bb{background:#374151;text-decoration:none;color:#e2e8f0;display:inline-flex;align-items:center;padding:6px 14px;border-radius:6px;font-size:.85em;font-weight:600}
.ct{display:flex;flex-direction:column;align-items:center;padding:12px;gap:8px;flex:1}
.mw{position:relative;display:block;line-height:0;max-width:640px;width:100%;min-height:240px;background:#0a1628;border-radius:6px;overflow:hidden}
.mw img{display:block;width:100%;height:auto}
#ov{position:absolute;top:0;left:0;width:100%;height:100%;pointer-events:none}
#nm{position:absolute;top:50%;left:50%;transform:translate(-50%,-50%);color:#64748b;font-size:.9em;text-align:center;line-height:1.6;pointer-events:none}
.il{font-size:.8em;color:#94a3b8;text-align:center;padding:0 8px;line-height:1.6}
.cr{display:flex;align-items:center;gap:6px;flex-wrap:wrap;justify-content:center;max-width:640px;width:100%}
.zb{background:#0f172a;border:1px solid #2d4a6a;color:#e2e8f0;padding:5px 12px;border-radius:6px;font-size:1em;cursor:pointer;font-weight:700;line-height:1.2}
.zb:hover{background:#1e3a5f;border-color:#60a5fa}
.zb.on{background:#1e3a5f;border-color:#60a5fa;color:#93c5fd}
.zb.sat{border-color:#f59e0b;color:#fcd34d}
.zb.road{border-color:#6ee7b7;color:#6ee7b7}
select{background:#0f172a;border:1px solid #2d4a6a;color:#e2e8f0;padding:5px 8px;border-radius:6px;font-size:.9em}
input[type=range]{flex:1;min-width:140px;accent-color:#38bdf8}
.lg{font-size:.72em;color:#64748b}
.lg b{display:inline-block;width:14px;height:4px;vertical-align:middle;margin:0 3px 0 8px;border-radius:2px}
</style></head><body>
<div class="tb">
  <h1>&#x1F9ED; Track Viewer</h1>
  <select id="ds" onchange="loadTrack(this.value)"></select>
  <button class="zb sat" id="tbt" onclick="toggleType()">&#x1F6F0; Sat</button>
  <a href="/map" class="bb">&#x1F4CD; Map</a>
  <a href="/" class="bb">&#x2190; Config</a>
</div>
<div class="ct">
  <div class="cr">
    <button class="zb on" id="mo" onclick="setMode('over')">&#x1F5FA; Overview</button>
    <button class="zb" id="mf" onclick="setMode('follow')">&#x1F3AF; Follow</button>
    <button class="zb" onclick="doZoom(-1)">&#x2212;</button>
    <button class="zb" onclick="doZoom(1)">+</button>
  </div>
  <div class="mw">
    <img id="mi" alt="">
    <canvas id="ov" width="640" height="480"></canvas>
    <div id="nm">Loading...</div>
  </div>
  <div class="cr">
    <button class="zb" onclick="step(-10)" title="-10">&#x23EE;</button>
    <button class="zb" onclick="step(-1)" title="-1">&#x25C0;</button>
    <button class="zb" id="pl" onclick="togglePlay()">&#x25B6; Play</button>
    <button class="zb" onclick="step(1)" title="+1">&#x25B6;</button>
    <button class="zb" onclick="step(10)" title="+10">&#x23ED;</button>
    <select id="sp" onchange="speed=+this.value">
      <option value="1">1 pt/s</option><option value="2">2 pt/s</option>
      <option value="5" selected>5 pt/s</option><option value="10">10 pt/s</option>
      <option value="20">20 pt/s</option>
    </select>
  </div>
  <div class="cr"><input type="range" id="sl" min="0" max="0" value="0" oninput="seek(+this.value)"></div>
  <div id="il" class="il">&nbsp;</div>
  <div class="lg"><b style="background:#facc15"></b>Route<b style="background:#22d3ee"></b>Replayed<b style="background:#ef4444"></b>Current<b style="background:#4ade80"></b>S<b style="background:#e879f9"></b>E</div>
</div>
<script>
const MAP_W=640,MAP_H=480,MAX_DRAW=1500;
let apiKey='',mapType='satellite',date='',pts=[],idx=0,mode='over',zoomF=16;
let overView=null,view=null,pendingView=null,imgView=null,lastUrl='';
let timer=null,speed=5,acc=0,bbox=null;
const img=document.getElementById('mi');
const oc=document.getElementById('ov'),ctx=oc.getContext('2d');
const nm=document.getElementById('nm');

function merc(lat){const l=Math.max(-85,Math.min(85,lat))*Math.PI/180;return Math.log(Math.tan(l/2+Math.PI/4));}
function unmerc(m){return Math.atan(Math.sinh(m))*180/Math.PI;}
// Pixel position of (lat,lon) on a MAP_W x MAP_H image centred on v.
function project(lat,lon,v){
  const w=256*Math.pow(2,v.zoom);
  return[MAP_W/2+(lon-v.lon)*w/360, MAP_H/2-(merc(lat)-merc(v.lat))*w/(2*Math.PI)];
}

// View (centre + zoom) that fits the whole route.
function fitView(b){
  const lat=unmerc((merc(b.latMin)+merc(b.latMax))/2),lon=(b.lonMin+b.lonMax)/2;
  const ls=Math.max(b.lonMax-b.lonMin,1e-9),ms=Math.max(merc(b.latMax)-merc(b.latMin),1e-9);
  let z=1;
  for(let t=18;t>=1;t--){
    const w=256*Math.pow(2,t);
    if(ls*w/360<=MAP_W*.85&&ms*w/(2*Math.PI)<=MAP_H*.85){z=t;break;}
  }
  return{lat:lat,lon:lon,zoom:z};
}

function mapUrl(v){
  return'https://maps.googleapis.com/maps/api/staticmap?center='+v.lat.toFixed(6)+','+v.lon.toFixed(6)+
    '&zoom='+v.zoom+'&size='+MAP_W+'x'+MAP_H+'&format=jpg&maptype='+mapType+'&key='+apiKey;
}

function loadImg(){
  if(!view)return;
  if(!apiKey){imgView=view;nm.style.display='block';nm.textContent='Maps API key not set (Config page > Map Settings)\n(route shown without map)';return;}
  const u=mapUrl(view);
  if(u===lastUrl)return;
  lastUrl=u;pendingView=view;img.src=u;
}
// Overlays are always drawn against the view the *loaded* image was fetched
// for, not the requested one — otherwise they sit in the wrong place while a
// new image is still downloading.
img.onload=function(){nm.style.display='none';imgView=pendingView;draw();};
img.onerror=function(){nm.style.display='block';nm.textContent='Map image load failed\n(check API key or network)';};

function polyline(a,b,color,width){
  const n=b-a+1;if(n<2||!imgView)return;
  const st=Math.max(1,Math.floor(n/MAX_DRAW));
  ctx.strokeStyle=color;ctx.lineWidth=width;ctx.lineJoin='round';ctx.beginPath();
  let i=a,first=true;
  for(;i<=b;i+=st){const p=project(pts[i][1],pts[i][2],imgView);first?ctx.moveTo(p[0],p[1]):ctx.lineTo(p[0],p[1]);first=false;}
  if(i-st!==b){const p=project(pts[b][1],pts[b][2],imgView);ctx.lineTo(p[0],p[1]);}
  ctx.stroke();
}
function dot(i,color,r,label){
  const p=project(pts[i][1],pts[i][2],imgView);
  ctx.fillStyle=color;ctx.beginPath();ctx.arc(p[0],p[1],r,0,2*Math.PI);ctx.fill();
  ctx.strokeStyle='#0b1220';ctx.lineWidth=1.5;ctx.stroke();
  if(label){ctx.font='bold 13px monospace';ctx.fillStyle='#fff';ctx.strokeStyle='#000';ctx.lineWidth=3;
    ctx.strokeText(label,p[0]+r+3,p[1]+4);ctx.fillText(label,p[0]+r+3,p[1]+4);}
}
function draw(){
  ctx.clearRect(0,0,oc.width,oc.height);
  if(!pts.length||!imgView)return;
  const last=pts.length-1;
  polyline(0,last,'#facc15',3);
  polyline(0,idx,'#22d3ee',4);
  dot(0,'#4ade80',5,'S');
  dot(last,'#e879f9',6,'E');
  const p=project(pts[idx][1],pts[idx][2],imgView);
  ctx.strokeStyle='#ef4444';ctx.lineWidth=3;ctx.beginPath();
  ctx.moveTo(p[0]-12,p[1]);ctx.lineTo(p[0]+12,p[1]);ctx.moveTo(p[0],p[1]-12);ctx.lineTo(p[0],p[1]+12);ctx.stroke();
}

function update(){
  if(!pts.length)return;
  const p=pts[idx];
  if(mode==='follow'){
    // Re-centre only once the point nears the edge, so stepping through
    // nearby points doesn't fetch a new image every time.
    let need=!view||view===overView;
    if(!need){const q=project(p[1],p[2],view);
      need=q[0]<MAP_W*.15||q[0]>MAP_W*.85||q[1]<MAP_H*.15||q[1]>MAP_H*.85;}
    if(need)view={lat:p[1],lon:p[2],zoom:zoomF};
  }else view=overView;
  loadImg();draw();info();
  document.getElementById('sl').value=idx;
}
function info(){
  const p=pts[idx];
  document.getElementById('il').innerHTML=
    date+' &nbsp; PT '+(idx+1)+'/'+pts.length+' &nbsp; '+p[0]+' UTC<br>'+
    p[1].toFixed(5)+'°, '+p[2].toFixed(5)+'° &nbsp; '+p[3].toFixed(0)+' m &nbsp; '+
    p[4].toFixed(1)+' kn ('+(p[4]*1.852).toFixed(1)+' km/h)';
}

function setMode(m){
  mode=m;view=null;
  document.getElementById('mo').className='zb'+(m==='over'?' on':'');
  document.getElementById('mf').className='zb'+(m==='follow'?' on':'');
  update();
}
function doZoom(d){
  if(mode==='over'){zoomF=overView?overView.zoom:zoomF;}
  zoomF=Math.max(1,Math.min(20,zoomF+d));
  mode='follow';view=null;
  document.getElementById('mo').className='zb';document.getElementById('mf').className='zb on';
  update();
}
function step(d){idx=Math.max(0,Math.min(pts.length-1,idx+d));update();}
function seek(i){idx=i;update();}
function togglePlay(){
  if(timer){stopPlay();return;}
  if(!pts.length)return;
  if(idx>=pts.length-1)idx=0;
  if(mode!=='follow')setMode('follow');
  document.getElementById('pl').innerHTML='&#x23F8; Pause';
  acc=0;
  timer=setInterval(function(){
    acc+=speed/10;const n=Math.floor(acc);acc-=n;
    if(n>0){idx=Math.min(pts.length-1,idx+n);update();}
    if(idx>=pts.length-1)stopPlay();
  },100);
}
function stopPlay(){
  if(timer){clearInterval(timer);timer=null;}
  document.getElementById('pl').innerHTML='&#x25B6; Play';
}
function toggleType(){
  mapType=mapType==='satellite'?'roadmap':'satellite';updateTypeBtn();lastUrl='';update();
}
function updateTypeBtn(){
  const b=document.getElementById('tbt');
  if(mapType==='satellite'){b.textContent='\u{1F6F0} Sat';b.className='zb sat';}
  else{b.textContent='\u{1F5FA} Road';b.className='zb road';}
}

async function loadTrack(d){
  stopPlay();date=d;pts=[];idx=0;view=overView=imgView=null;lastUrl='';
  ctx.clearRect(0,0,oc.width,oc.height);img.removeAttribute('src');
  nm.style.display='block';nm.textContent='Loading track...';
  document.getElementById('il').innerHTML='&nbsp;';
  try{
    const r=await fetch('/api/track?date='+encodeURIComponent(d));
    if(!r.ok)throw new Error(r.status);
    pts=(await r.json()).pts||[];
  }catch(e){nm.textContent='Failed to load track';return;}
  if(!pts.length){nm.textContent='No points in this track';return;}
  bbox={latMin:90,latMax:-90,lonMin:180,lonMax:-180};
  for(const p of pts){
    if(p[1]<bbox.latMin)bbox.latMin=p[1];if(p[1]>bbox.latMax)bbox.latMax=p[1];
    if(p[2]<bbox.lonMin)bbox.lonMin=p[2];if(p[2]>bbox.lonMax)bbox.lonMax=p[2];
  }
  overView=fitView(bbox);
  const sl=document.getElementById('sl');sl.max=pts.length-1;sl.value=0;
  setMode('over');
}

document.addEventListener('keydown',function(e){
  if(e.target.tagName==='SELECT')return;
  const k=e.key;
  if(k==='ArrowRight'){step(e.shiftKey?10:1);e.preventDefault();}
  else if(k==='ArrowLeft'){step(e.shiftKey?-10:-1);e.preventDefault();}
  else if(k===' '){togglePlay();e.preventDefault();}
});

async function init(){
  try{const d=await fetch('/api/mapkey').then(r=>r.json());apiKey=d.api_key||'';mapType=d.map_type||'satellite';}catch(e){}
  updateTypeBtn();
  let files=[];
  try{files=(await fetch('/api/tracks').then(r=>r.json())).files||[];}catch(e){}
  const ds=document.getElementById('ds');
  if(!files.length){nm.textContent='No saved tracks\n(needs eMMC and a GPS fix)';ds.style.display='none';return;}
  ds.innerHTML=files.map(f=>'<option>'+f+'</option>').join('');
  loadTrack(files[0]);
}
init();
</script></body></html>"##.to_string()
}

/// Heap usage snapshot: (free, min-ever-free) in bytes for internal RAM and PSRAM.
#[derive(Clone, Copy, Default)]
pub struct MemInfo {
    pub int_free: u32,
    pub int_min:  u32,
    pub psram_free: u32,
    pub psram_min:  u32,
}

pub fn mem_info() -> MemInfo {
    use esp_idf_sys::{heap_caps_get_free_size, heap_caps_get_minimum_free_size,
                      MALLOC_CAP_INTERNAL, MALLOC_CAP_SPIRAM};
    unsafe {
        MemInfo {
            int_free:   heap_caps_get_free_size(MALLOC_CAP_INTERNAL) as u32,
            int_min:    heap_caps_get_minimum_free_size(MALLOC_CAP_INTERNAL) as u32,
            psram_free: heap_caps_get_free_size(MALLOC_CAP_SPIRAM) as u32,
            psram_min:  heap_caps_get_minimum_free_size(MALLOC_CAP_SPIRAM) as u32,
        }
    }
}
