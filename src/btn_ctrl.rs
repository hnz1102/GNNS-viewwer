// Button control thread
//
// GPIO0 (BOOT button, active-low, internal pull-up):
//   Short press  (< 3 s)  : cycle display page — or, while replaying a saved
//                           track on the MAP page, end playback instead
//   Long press   (>= 10 s) : factory reset to cfg.toml defaults
//
// GPIO4/GPIO5 (active-low, internal pull-up) are context-sensitive:
//   On the track-list page      : move the highlight (GPIO4 = up, GPIO5 = down)
//   On MAP page during playback : step the replayed point (+1 / -1 on the
//                                 initial press, +10 / -10 once auto-repeat
//                                 kicks in from holding the button)
//   Otherwise                   : MAP zoom in / out
//   Buttons debounced at 50 ms, with 400 ms initial delay then 300 ms repeat.
//
// GPIO6 (active-low, internal pull-up) is also context-sensitive:
//   On the quake page      : show the highlighted epicenter on the MAP page
//   On the track-list page : load the highlighted track and start playback
//   Otherwise               : MAP type toggle (satellite ↔ roadmap)

use std::thread;
use std::time::{Duration, Instant};
use log::info;
use esp_idf_hal::gpio::{Gpio0, Gpio4, Gpio5, Gpio6, PinDriver, Pull};

const FACTORY_RESET_PRESS_MS: u64 = 10000;
const ZOOM_REPEAT_DELAY_MS: u64 = 400;
const ZOOM_REPEAT_MS: u64 = 300;

/// GPIO4 (UP, +) action: move the list highlight up, step playback forward, or zoom in —
/// whichever applies to the currently displayed page. During playback,
/// `repeating` (true once the button is auto-repeating, false on the
/// initial press edge) steps 10 points at a time instead of 1, so holding
/// the button scrubs through a long track quickly.
fn step_plus(repeating: bool) {
    if crate::serial_display::current_page() == crate::serial_display::PAGE_QUAKE {
        crate::quake::move_sel(-1);
    } else if crate::serial_display::current_page() == crate::serial_display::PAGE_TRACKLIST {
        crate::track_view::list_move(-1);
    } else if crate::track_view::is_playing() {
        crate::track_view::step(if repeating { 10 } else { 1 });
    } else {
        crate::map_display::zoom_in();
    }
}

/// GPIO5 action: mirror of [`step_plus`] in the opposite direction.
fn step_minus(repeating: bool) {
    if crate::serial_display::current_page() == crate::serial_display::PAGE_QUAKE {
        crate::quake::move_sel(1);
    } else if crate::serial_display::current_page() == crate::serial_display::PAGE_TRACKLIST {
        crate::track_view::list_move(1);
    } else if crate::track_view::is_playing() {
        crate::track_view::step(if repeating { -10 } else { -1 });
    } else {
        crate::map_display::zoom_out();
    }
}

pub fn start_button_thread(btn_pin: Gpio0, zoom_in_pin: Gpio4, zoom_out_pin: Gpio5, map_type_pin: Gpio6) {
    thread::Builder::new()
        .name("btn_ctrl".into())
        .stack_size(4096)
        .spawn(move || button_task(btn_pin, zoom_in_pin, zoom_out_pin, map_type_pin))
        .expect("spawn btn_ctrl thread");
}

fn button_task(btn_pin: Gpio0, zoom_in_pin: Gpio4, zoom_out_pin: Gpio5, map_type_pin: Gpio6) {
    let mut btn = PinDriver::input(btn_pin).unwrap();
    btn.set_pull(Pull::Up).unwrap();
    let mut btn_zin = PinDriver::input(zoom_in_pin).unwrap();
    btn_zin.set_pull(Pull::Up).unwrap();
    let mut btn_zout = PinDriver::input(zoom_out_pin).unwrap();
    btn_zout.set_pull(Pull::Up).unwrap();
    let mut btn_type = PinDriver::input(map_type_pin).unwrap();
    btn_type.set_pull(Pull::Up).unwrap();

    let mut btn_last = true;
    let mut press_start: Option<Instant> = None;
    let mut factory_reset_triggered = false;
    let mut display_updated = false;

    let mut zin_last  = true;
    let mut zout_last = true;
    let mut zin_next_repeat:  Option<Instant> = None;
    let mut zout_next_repeat: Option<Instant> = None;
    let mut type_last = true;

    loop {
        // GPIO0: page cycle / factory reset
        let btn_now = btn.is_high();
        if !btn_now && btn_last {
            press_start = Some(Instant::now());
            factory_reset_triggered = false;
            display_updated = false;
        } else if !btn_now {
            if let Some(start) = press_start {
                let elapsed = start.elapsed().as_millis() as u64;
                if elapsed >= 6000 && !display_updated {
                    let rem = ((FACTORY_RESET_PRESS_MS - elapsed.min(FACTORY_RESET_PRESS_MS)) / 1000) as i32;
                    if rem > 0 {
                        crate::serial_display::show_system_message("FACTORY RESET",
                            &format!("Resetting to defaults\nin {} seconds...", rem));
                        display_updated = true;
                    }
                }
                if !factory_reset_triggered && elapsed >= FACTORY_RESET_PRESS_MS {
                    info!("GPIO0 long press -- factory reset");
                    crate::serial_display::show_system_message("FACTORY RESET", "Erasing NVS...\nPlease wait...");
                    thread::sleep(Duration::from_millis(500));
                    crate::httpserver::factory_reset();
                }
            }
        } else if btn_now && !btn_last {
            if !factory_reset_triggered {
                if let Some(start) = press_start.take() {
                    let elapsed = start.elapsed().as_millis() as u64;
                    if crate::track_view::is_playing() {
                        info!("GPIO0 short press ({}ms) -- end track playback", elapsed);
                        crate::track_view::stop();
                    } else {
                        info!("GPIO0 short press ({}ms) -- next display page", elapsed);
                        crate::quake::clear_focus();
                        crate::serial_display::next_page();
                    }
                }
            }
            if display_updated { crate::serial_display::clear_system_message(); }
            press_start = None;
            factory_reset_triggered = false;
            display_updated = false;
        }
        btn_last = btn_now;

        // GPIO4: zoom in / list-select up / step playback forward
        let zin_now = btn_zin.is_high();
        if !zin_now && zin_last {
            step_plus(false);
            zin_next_repeat = Some(Instant::now() + Duration::from_millis(ZOOM_REPEAT_DELAY_MS));
        } else if !zin_now {
            if zin_next_repeat.map(|t| Instant::now() >= t).unwrap_or(false) {
                step_plus(true);
                zin_next_repeat = Some(Instant::now() + Duration::from_millis(ZOOM_REPEAT_MS));
            }
        } else {
            zin_next_repeat = None;
        }
        zin_last = zin_now;

        // GPIO5: zoom out / list-select down / step playback backward
        let zout_now = btn_zout.is_high();
        if !zout_now && zout_last {
            step_minus(false);
            zout_next_repeat = Some(Instant::now() + Duration::from_millis(ZOOM_REPEAT_DELAY_MS));
        } else if !zout_now {
            if zout_next_repeat.map(|t| Instant::now() >= t).unwrap_or(false) {
                step_minus(true);
                zout_next_repeat = Some(Instant::now() + Duration::from_millis(ZOOM_REPEAT_MS));
            }
        } else {
            zout_next_repeat = None;
        }
        zout_last = zout_now;

        // GPIO6: on the track-list page, load the highlighted track and
        // start playback; otherwise map type toggle (satellite ↔ roadmap).
        let type_now = btn_type.is_high();
        if !type_now && type_last {
            if crate::serial_display::current_page() == crate::serial_display::PAGE_QUAKE {
                if crate::quake::focus_selected() {
                    info!("GPIO6 press -- showing selected epicenter on MAP");
                    crate::serial_display::set_page(crate::serial_display::PAGE_MAP);
                }
            } else if crate::serial_display::current_page() == crate::serial_display::PAGE_TRACKLIST {
                if crate::track_view::select_and_play() {
                    info!("GPIO6 press -- track selected, starting playback");
                    crate::serial_display::set_page(crate::serial_display::PAGE_MAP);
                } else {
                    info!("GPIO6 press -- no track available to play");
                }
            } else {
                let is_sat = crate::map_display::toggle_map_type();
                info!("GPIO6 press -- map type toggled to {}",
                      if is_sat { "satellite" } else { "roadmap" });
                // Force a new map fetch by resetting the cached position
                crate::map_display::reset_last_pos();
            }
        }
        type_last = type_now;

        thread::sleep(Duration::from_millis(50));
    }
}
