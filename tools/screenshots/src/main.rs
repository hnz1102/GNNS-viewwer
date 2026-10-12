//! Renders each TFT page (320x240) to a PNG on the host, using the firmware's
//! own drawing code (`src/display_pages.rs`) with stubbed data sources.
//!
//!   cargo run --release -- [OUT_DIR] [SCALE]     (defaults: ../../docs/images/tft, 2)
//!
//! The sample data is synthetic (Tokyo Tower, made-up satellites/quakes); the map
//! is a schematic stand-in for the Google tile the firmware would fetch.
#![allow(dead_code)]

#[path = "../../../src/display_pages.rs"]
mod display_pages;
#[path = "../../../src/track_view.rs"]
mod track_view;
mod gps_parser;
mod httpserver;
mod map_display;
mod quake;
mod track_log;
mod usb_host;

use std::convert::Infallible;
use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;

use embedded_graphics::{
    mono_font::{ascii::FONT_10X20, MonoTextStyle},
    pixelcolor::Rgb565,
    prelude::*,
    primitives::Rectangle,
};

use display_pages::*;
use gps_parser::{GpsData, SatInfo};

const W: u32 = 320;
const H: u32 = 240;

/// In-memory 320x240 RGB565 framebuffer standing in for the ST7789.
struct Fb(Vec<Rgb565>);

impl Fb {
    fn new() -> Self { Fb(vec![Rgb565::BLACK; (W * H) as usize]) }

    fn save(&self, path: &PathBuf, scale: u32) {
        let (w, h) = (W * scale, H * scale);
        let mut rgb = Vec::with_capacity((w * h * 3) as usize);
        for y in 0..h {
            for x in 0..w {
                let p = self.0[((y / scale) * W + x / scale) as usize];
                // Expand 5/6/5 to 8 bits with bit replication (full-range white).
                let (r, g, b) = (p.r(), p.g(), p.b());
                rgb.push((r << 3) | (r >> 2));
                rgb.push((g << 2) | (g >> 4));
                rgb.push((b << 3) | (b >> 2));
            }
        }
        let mut enc = png::Encoder::new(BufWriter::new(File::create(path).unwrap()), w, h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().unwrap().write_image_data(&rgb).unwrap();
        println!("wrote {} ({}x{})", path.display(), w, h);
    }
}

impl OriginDimensions for Fb {
    fn size(&self) -> Size { Size::new(W, H) }
}

impl DrawTarget for Fb {
    type Color = Rgb565;
    type Error = Infallible;
    fn draw_iter<I: IntoIterator<Item = Pixel<Rgb565>>>(&mut self, pixels: I) -> Result<(), Infallible> {
        for Pixel(p, c) in pixels {
            if p.x >= 0 && p.y >= 0 && (p.x as u32) < W && (p.y as u32) < H {
                self.0[(p.y as u32 * W + p.x as u32) as usize] = c;
            }
        }
        Ok(())
    }
    fn fill_solid(&mut self, area: &Rectangle, c: Rgb565) -> Result<(), Infallible> {
        self.fill_contiguous(area, core::iter::repeat(c))
    }
}

fn header_style() -> MonoTextStyle<'static, Rgb565> {
    MonoTextStyle::new(&FONT_10X20, Rgb565::CSS_LIGHT_GREEN)
}

/// What `display_task` does on a full redraw: clear, header, page, battery icon.
fn render(page: u8, batt: u8, body: impl FnOnce(&mut Fb)) -> Fb {
    let mut fb = Fb::new();
    draw_header(&mut fb, page, &header_style());
    body(&mut fb);
    draw_battery_icon(&mut fb, batt);
    fb
}

// ─── Sample data ─────────────────────────────────────────────────────────────

const HOME: (f64, f64) = (35.658584, 139.745433); // Tokyo Tower

fn sat(system: char, prn: u8, el: i16, az: u16, snr: u8, in_use: bool) -> SatInfo {
    SatInfo { system, prn, elevation: el, azimuth: az, snr, in_use }
}

fn sample_gps() -> GpsData {
    let sats = vec![
        sat('G', 2, 62, 40, 46, true),   sat('G', 5, 35, 110, 42, true),
        sat('G', 12, 18, 205, 38, true), sat('G', 13, 71, 300, 48, true),
        sat('G', 15, 8, 255, 27, false), sat('G', 20, 44, 160, 44, true),
        sat('G', 25, 27, 330, 40, true), sat('G', 29, 52, 75, 45, true),
        
        sat('A', 3, 55, 130, 44, true),  sat('A', 8, 30, 220, 41, true),
         sat('A', 21, 68, 350, 47, true),
        sat('A', 24, 9, 60, 29, false),
        sat('B', 6, 48, 195, 43, true),  sat('B', 13, 22, 90, 39, true),
        sat('B', 24, 63, 255, 46, true), 
        sat('B', 38, 5, 140, 0, false),
        sat('L', 3, 40, 25, 41, true),   sat('L', 10, 20, 235, 37, true),
         sat('L', 19, 14, 170, 30, false),
        sat('Q', 1, 78, 5, 49, true),    sat('Q', 2, 70, 120, 48, true),
        
    ];
    let used = sats.iter().filter(|s| s.in_use).count() as u8;
    GpsData {
        time_str: "03:21:47".into(),
        date_str: "2026/10/12".into(),
        lat_str: "35d39.5150N".into(),
        lon_str: "139d44.7260E".into(),
        lat_dd: HOME.0,
        lon_dd: HOME.1,
        fix_quality: 2,
        fix_mode: 3,
        num_sats: used,
        hdop: 0.62,
        pdop: 1.05,
        vdop: 0.84,
        altitude: 28.4,
        speed_kn: 0.1,
        course: 148.0,
        sats,
        valid: true,
    }
}

fn track_points(n: usize) -> Vec<track_log::TrackPoint> {
    // A loop around Shiba Park / Tokyo Tower, one point per 5 s.
    (0..n).map(|i| {
        let t = i as f64 / n as f64 * std::f64::consts::TAU;
        let wob = 1.0 + 0.25 * (3.0 * t).sin();
        track_log::TrackPoint {
            time_utc: format!("2026-10-10T{:02}:{:02}:{:02}Z", 1 + i * 5 / 3600, i * 5 / 60 % 60, i * 5 % 60),
            lat: HOME.0 + 0.0042 * t.sin() * wob,
            lon: HOME.1 + 0.0062 * t.cos() * wob - 0.0062,
            alt_m: 25.0 + 6.0 * (2.0 * t).sin() as f32,
            speed_kn: 2.4 + (t * 2.0).cos() as f32,
        }
    }).collect()
}

fn sample_quakes() -> Vec<quake::Quake> {
    let q = |time: &str, lat, lon, depth, mag, scale, ts: &str| quake::Quake {
        time: time.into(), name: String::new(), lat, lon, depth, mag, scale, tsunami: ts.into(),
    };
    vec![
        q("2026/10/12 02:48:00", 35.70, 140.85, 40, 4.6, 30, "None"),
        q("2026/10/11 23:15:00", 37.50, 141.60, 50, 5.4, 45, "NonEffective"),
        q("2026/10/11 17:02:00", 33.20, 131.70, 10, 3.9, 20, "None"),
        q("2026/10/11 09:31:00", 36.30, 139.90, 60, 3.3, 20, "None"),
        q("2026/10/10 21:40:00", 43.00, 144.30, 90, 5.0, 40, "None"),
        q("2026/10/10 06:12:00", 34.90, 138.20, 20, 3.1, 10, "None"),
    ]
}

fn setup() {
    gps_parser::set_data(sample_gps());
    gps_parser::set_location_name("Shiba-koen, Minato-ku, Tokyo");

    let status = httpserver::StatusInfo::new();
    *status.ip_address.lock().unwrap() = "192.168.1.42".into();
    *status.ssid.lock().unwrap() = "home-ap".into();
    *status.rssi.lock().unwrap() = -57;
    *status.battery_voltage.lock().unwrap() = 4.72;
    *status.chip_temp.lock().unwrap() = 41.5;
    set_display_aux(DisplayAux {
        status,
        com1_baud: "115200".into(),
        com1_tx: "GPIO43".into(),
        com1_rx: "GPIO44".into(),
    });
    httpserver::set_mem_info(httpserver::MemInfo {
        int_free: 118_000, int_min: 71_000, psram_free: 7_400_000, psram_min: 7_100_000,
    });
    usb_host::set_active_device_name("com1");

    track_log::add_file("2026-10-10", track_points(360));
    track_log::add_file("2026-10-09", track_points(200));
    track_log::add_file("2026-10-06", track_points(520));
    track_log::add_file("2026-10-03", track_points(90));
    track_log::add_file("2026-09-28", track_points(750));
}

fn main() {
    let mut args = std::env::args().skip(1);
    let out: PathBuf = args.next().map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/images/tft")));
    let scale: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(2).max(1);
    std::fs::create_dir_all(&out).unwrap();
    setup();
    let save = |name: &str, fb: Fb| fb.save(&out.join(name), scale);
    let batt = 4; // 4 of 5 segments

    // 1. GPS
    save("01-gps.png", render(PAGE_GPS, batt, |fb| {
        draw_gps_page(fb, true, &mut Vec::new(), &mut 0xFFFF_FFFF);
    }));

    // 2. Sky view
    save("02-skyview.png", render(PAGE_SKYVIEW, batt, |fb| {
        draw_skyview_page(fb, true, &mut 0xFFFF_FFFF, &mut Vec::new(), &mut Vec::new());
    }));

    // 3. Map (live): the trail is what the firmware accumulates while moving.
    map_display::set_view(HOME.0, HOME.1, 16);
    for i in 0..40 {
        // a short walk ending at the current position
        let t = 1.0 - i as f64 / 39.0;
        map_display::push_trail(HOME.0 + 0.0011 * t * (1.0 + 0.4 * (t * 5.0).sin()), HOME.1 - 0.0016 * t * t);
    }
    save("03-map.png", render(PAGE_MAP, batt, |fb| {
        draw_map_page(fb, true, &mut 0xFFFF_FFFF, &mut 0);
    }));

    // 3b. Same view on satellite imagery (the firmware's map-type toggle)
    map_display::set_map_type_satellite(true);
    map_display::set_view(HOME.0, HOME.1, 16);
    save("03b-map-satellite.png", render(PAGE_MAP, batt, |fb| {
        draw_map_page(fb, true, &mut 0xFFFF_FFFF, &mut 0);
    }));

    map_display::clear_trail();
    // 3c. Wide view (zoom <= 5): the ground point under each tracked satellite
    map_display::set_view(HOME.0, HOME.1, 3);
    save("03c-map-satellite-ground-points.png", render(PAGE_MAP, batt, |fb| {
        draw_map_page(fb, true, &mut 0xFFFF_FFFF, &mut 0);
    }));
    map_display::set_map_type_satellite(false);
    map_display::set_view(HOME.0, HOME.1, 16);
    map_display::clear_trail();

    // 4. Saved tracks
    track_view::refresh_file_list();
    track_view::list_move(1);
    save("04-tracklist.png", render(PAGE_TRACKLIST, batt, |fb| {
        draw_tracklist_page(fb, true, &mut Vec::new());
    }));

    // 4b. Track playback (map page while replaying the highlighted track)
    track_view::list_move(-1);
    assert!(track_view::select_and_play());
    track_view::step(150);
    let p = track_view::current_point().unwrap();
    map_display::set_view(p.lat, p.lon, 16);
    save("04b-track-playback.png", render(PAGE_MAP, batt, |fb| {
        draw_map_page(fb, true, &mut 0xFFFF_FFFF, &mut 0);
    }));
    track_view::stop();

    // 5. Serial terminal (COM1), fed raw NMEA/UBX-style text with ANSI colours.
    let com1 = SerialRxBuffer::new();
    com1.push_data(concat!(
        "\x1b[1;32mu-blox MAX-F10S\x1b[0m  FW SPG 6.00  \x1b[36m115200 8N1\x1b[0m\r\n",
        "\x1b[90m$GNRMC,032147.00,A,3539.5150,N,13944.7260,E,0.1,148.0,121026,,,D,V*6B\x1b[0m\r\n",
        "$GNGGA,032147.00,3539.5150,N,13944.7260,E,2,18,0.62,28.4,M,39.4,M,,0000*4E\r\n",
        "$GNGSA,A,3,02,05,13,20,25,29,12,15,,,,,1.05,0.62,0.84,1*0A\r\n",
        "$GNGSA,A,3,03,08,13,21,,,,,,,,,1.05,0.62,0.84,3*0B\r\n",
        "\x1b[33m$GPGSV,3,1,09,02,62,040,46,05,35,110,42,12,18,205,38,13,71,300,48*70\x1b[0m\r\n",
        "\x1b[33m$GPGSV,3,2,09,15,08,255,27,20,44,160,44,25,27,330,40,29,52,075,45*77\x1b[0m\r\n",
        "\x1b[35m$GAGSV,2,1,05,03,55,130,44,08,30,220,41,13,16,285,36,21,68,350,47*6C\x1b[0m\r\n",
        "$GNVTG,148.0,T,,M,0.1,N,0.2,K,D*2E\r\n",
        "$GNGLL,3539.5150,N,13944.7260,E,032147.00,A,D*7A\r\n",
        "\x1b[1;37;44m ZED/MAX terminal: type to send ASCII to the receiver \x1b[0m\r\n",
        "\x1b[32m$\x1b[0m \x1b[5m_\x1b[0m",
    ).as_bytes());
    save("05-com1.png", render(PAGE_COM1, batt, |fb| {
        let mut prev = Box::new([[Cell::blank(); TERM_COLS]; TERM_ROWS]);
        draw_terminal_page(fb, &com1, &mut prev, true);
    }));

    // 6. System info
    save("06-info.png", render(PAGE_INFO, batt, |fb| {
        draw_info_page(fb, true, &mut Vec::new());
    }));

    // 7. Earthquake list
    quake::set_quakes(sample_quakes(), 1);
    quake::set_selection(1);
    save("07-quake.png", render(PAGE_QUAKE, batt, |fb| {
        draw_quake_page(fb, true, &mut Vec::new());
    }));

    // 7b. Quake list with nothing received yet
    quake::set_quakes(Vec::new(), 0);
    save("07b-quake-waiting.png", render(PAGE_QUAKE, batt, |fb| {
        draw_quake_page(fb, true, &mut Vec::new());
    }));
    quake::set_quakes(sample_quakes(), 1);

    // 8. Map framed on a quake selected from the list
    let q = &sample_quakes()[1];
    let (clat, clon, zoom, _) = quake::fit_view(q.lat, q.lon);
    quake::set_focus(Some((clat, clon)));
    map_display::set_view(clat, clon, zoom);
    save("08-map-quake-focus.png", render(PAGE_MAP, batt, |fb| {
        draw_map_page(fb, true, &mut 0xFFFF_FFFF, &mut 0);
    }));
    quake::set_focus(None);

    // 9. New earthquake announced on the map page
    quake::set_new_quake(Some(q.clone()));
    quake::set_focus(Some((clat, clon)));
    save("09-map-new-quake.png", render(PAGE_MAP, batt, |fb| {
        draw_map_page(fb, true, &mut 0xFFFF_FFFF, &mut 0);
    }));
    quake::set_new_quake(None);
    quake::set_focus(None);

    // 10. Earthquake early warning takes over the map page
    let eew = quake::Eew {
        time: "2026/10/12 03:22:10".into(), name: String::new(),
        lat: 35.30, lon: 139.95, depth: 30, mag: 6.1, max_scale: 55,
        areas: String::new(),
    };
    let (clat, clon, zoom, _) = quake::eew_view(&eew);
    quake::set_eew(Some(eew));
    map_display::set_view(clat, clon, zoom);
    save("10-map-eew.png", render(PAGE_MAP, batt, |fb| {
        draw_map_page(fb, true, &mut 0xFFFF_FFFF, &mut 0);
    }));
    quake::set_eew(None);

    // 11. System message overlay
    let mut fb = Fb::new();
    draw_system_message(&mut fb, "FACTORY RESET", "Erasing NVS...\nPlease wait...", &header_style(), true);
    save("11-system-message.png", fb);

    // 12. No fix (cold start) on the GPS page and the map page
    gps_parser::set_data(GpsData { time_str: "03:18:02".into(), date_str: "2026/10/12".into(), ..GpsData::default() });
    gps_parser::set_location_name("");
    save("12-gps-no-fix.png", render(PAGE_GPS, 255, |fb| {
        draw_gps_page(fb, true, &mut Vec::new(), &mut 0xFFFF_FFFF);
    }));
}
