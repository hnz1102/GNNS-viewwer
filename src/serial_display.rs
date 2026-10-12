use std::thread;
use log::info;
use esp_idf_hal::{gpio::*, prelude::*, spi, delay::FreeRtos};
use esp_idf_hal::spi::config::MODE_0;
use esp_idf_hal::spi::Dma;
use display_interface_spi::SPIInterface;
use mipidsi::{Builder, models::ST7789, options::{Orientation, Rotation}};
use embedded_graphics::{
    mono_font::{ascii::FONT_10X20, MonoTextStyle},
    pixelcolor::Rgb565,
    prelude::*,
};

pub use crate::display_pages::*;

/// Start the display thread. Call once from main after peripherals init.
pub fn start_display_thread(
    spi_periph: spi::SPI3,
    sclk: Gpio48,
    sdo: Gpio45,
    cs: Gpio40,
    dc_pin: Gpio39,
    rst_pin: Gpio38,
    com1_buf: SerialRxBuffer,
) {
    thread::Builder::new()
        .name("display".into())
        .stack_size(24576)
        .spawn(move || display_task(spi_periph, sclk, sdo, cs, dc_pin, rst_pin, com1_buf))
        .expect("spawn display thread");
}

fn display_task(
    spi_periph: spi::SPI3,
    sclk: Gpio48,
    sdo: Gpio45,
    cs: Gpio40,
    dc_pin: Gpio39,
    rst_pin: Gpio38,
    com1_buf: SerialRxBuffer,
) {
    let dc = PinDriver::output(dc_pin).unwrap();
    let rst = PinDriver::output(rst_pin).unwrap();

    let spi_config = spi::SpiConfig::new().baudrate(40.MHz().into()).data_mode(MODE_0);
    let spi_driver_config = spi::SpiDriverConfig {
        dma: Dma::Disabled,
        ..Default::default()
    };
    let sdi_not_used: Option<Gpio2> = None;

    let spi_driver = spi::SpiDriver::new(
        spi_periph, sclk, sdo, sdi_not_used, &spi_driver_config,
    ).unwrap();
    let spi_device = spi::SpiDeviceDriver::new(spi_driver, Some(cs), &spi_config).unwrap();

    let mut delay = FreeRtos;
    let di = SPIInterface::new(spi_device, dc);
    let orientation = Orientation::new().rotate(Rotation::Deg90);
    let mut display = Builder::new(ST7789, di)
        .reset_pin(rst)
        .display_size(240, 320)
        .invert_colors(mipidsi::options::ColorInversion::Inverted)
        .color_order(mipidsi::options::ColorOrder::Rgb)
        .orientation(orientation)
        .init(&mut delay)
        .unwrap();

    info!("Display initialized (320x240 multi-page)");

    let header_style = MonoTextStyle::new(&FONT_10X20, Rgb565::CSS_LIGHT_GREEN);

    // Initial clear
    display.clear(Rgb565::BLACK).unwrap();

    // Single prev-cell snapshot for the currently viewed terminal page (heap)
    let mut prev_cells = Box::new([[Cell::blank(); TERM_COLS]; TERM_ROWS]);
    let mut force_full_redraw = true;
    let mut last_page = current_page();
    // For info page: track previous content to avoid flicker
    let mut prev_info_lines: Vec<String> = Vec::new();
    // For GPS page: track previous text lines and satellite chart signature
    let mut prev_gps_lines: Vec<String> = Vec::new();
    let mut prev_gps_chart_sig: u32 = 0xFFFF_FFFF;
    // For sky-view page: track change signature, previous dot positions, right-panel lines
    let mut prev_sky_sig: u32 = 0xFFFF_FFFF;
    let mut prev_sky_sats: Vec<(i32, i32)> = Vec::new();
    let mut prev_sky_panel: Vec<String> = Vec::new();
    let mut prev_map_sig: u32 = 0xFFFF_FFFF;
    let mut prev_trail_len: usize = 0;
    // For track-list page: track previous rendered rows
    let mut prev_track_lines: Vec<String> = Vec::new();
    let mut prev_quake_lines: Vec<String> = Vec::new();
    // Track previous system message to detect changes
    let mut prev_system_message: Option<(String, String)> = None;
    // Track previous track-playback state to force a redraw on start/stop
    let mut last_playing = crate::track_view::is_playing();
    let mut last_eew_ver = crate::quake::eew_version();
    let mut last_newq_ver = crate::quake::newq_version();
    let mut batt_level: u8 = BATT_NONE;

    // Draw initial header
    draw_header(&mut display, last_page, &header_style);

    loop {
        // ── Page change detection ─────────────────────────────────────
        let page = current_page();
        if page != last_page {
            last_page = page;
            force_full_redraw = true;
            // Reset diff snapshots when switching pages
            *prev_cells = [[Cell::blank(); TERM_COLS]; TERM_ROWS];
            prev_gps_lines.clear();
            prev_gps_chart_sig = 0xFFFF_FFFF;
            prev_sky_sig = 0xFFFF_FFFF;
            prev_sky_sats.clear();
            prev_sky_panel.clear();
            prev_map_sig = 0xFFFF_FFFF;
            prev_trail_len = 0;
            prev_track_lines.clear();
            prev_quake_lines.clear();
            if page == PAGE_TRACKLIST {
                crate::track_view::refresh_file_list();
            }
        }

        // Drops the newly-selected track's whole-route overview back to the
        // tracking zoom once its few seconds are up.
        crate::track_view::tick_overview();

        // ── Track-playback start/stop detection ─────────────────────────
        // Playback can start/stop without a page change (both live and
        // replayed views render on PAGE_MAP), so force a redraw here too.
        // An EEW appearing/ending changes the header and the whole map overlay.
        let eew_ver = crate::quake::eew_version();
        let newq_ver = crate::quake::newq_version();
        if eew_ver != last_eew_ver || newq_ver != last_newq_ver {
            last_eew_ver = eew_ver;
            last_newq_ver = newq_ver;
            force_full_redraw = true;
        }
        let playing_now = crate::track_view::is_playing();
        if playing_now != last_playing {
            last_playing = playing_now;
            force_full_redraw = true;
            prev_map_sig = 0xFFFF_FFFF;
            prev_trail_len = 0;
        }

        // ── Check for system message overlay ──────────────────────────
        let sys_msg = SYSTEM_MESSAGE.lock().unwrap().clone();
        
        if let Some((title, text)) = sys_msg.clone() {
            // Check if system message changed
            let msg_changed = match &prev_system_message {
                Some((prev_title, prev_text)) => prev_title != &title || prev_text != &text,
                None => true, // First time showing system message
            };
            
            // System message is active — draw it, force clear only if changed
            draw_system_message(&mut display, &title, &text, &header_style, msg_changed);
            prev_system_message = Some((title, text));
        } else {
            // Normal display mode
            // If we just exited system message mode, force redraw
            if prev_system_message.is_some() {
                force_full_redraw = true;
                prev_system_message = None;
            }
            
            if force_full_redraw {
                display.clear(Rgb565::BLACK).unwrap();
                draw_header(&mut display, page, &header_style);
                prev_info_lines.clear();
                prev_gps_lines.clear();
            }

            match page {
                PAGE_GPS => {
                    draw_gps_page(&mut display, force_full_redraw, &mut prev_gps_lines, &mut prev_gps_chart_sig);
                }
                PAGE_SKYVIEW => {
                    draw_skyview_page(&mut display, force_full_redraw, &mut prev_sky_sig,
                                      &mut prev_sky_sats, &mut prev_sky_panel);
                }
                PAGE_COM1 => {
                    draw_terminal_page(&mut display, &com1_buf, &mut prev_cells, force_full_redraw);
                }
                PAGE_INFO => {
                    draw_info_page(&mut display, force_full_redraw, &mut prev_info_lines);
                }
                PAGE_MAP => {
                    draw_map_page(&mut display, force_full_redraw, &mut prev_map_sig, &mut prev_trail_len);
                }
                PAGE_TRACKLIST => {
                    draw_tracklist_page(&mut display, force_full_redraw, &mut prev_track_lines);
                }
                PAGE_QUAKE => {
                    draw_quake_page(&mut display, force_full_redraw, &mut prev_quake_lines);
                }
                _ => {}
            }
        }

        // ── Battery icon (top-right of header) ─────────────────────────
        // Redrawn on a full redraw (the header was just cleared) or when the
        // level changes; skipped while a system message owns the header.
        let batt_v = battery_voltage();
        let new_level = next_battery_level(batt_level, batt_v);
        if sys_msg.is_none() && (force_full_redraw || new_level != batt_level) {
            draw_battery_icon(&mut display, new_level);
        }
        batt_level = new_level;

        force_full_redraw = false;
        thread::sleep(std::time::Duration::from_millis(50));
    }
}

