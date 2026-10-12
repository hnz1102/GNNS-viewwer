//! Host stub of `usb_host`.
use std::sync::Mutex;
static ACTIVE: Mutex<&'static str> = Mutex::new("none");
pub fn active_device_name() -> &'static str { *ACTIVE.lock().unwrap() }
pub fn set_active_device_name(n: &'static str) { *ACTIVE.lock().unwrap() = n; }
