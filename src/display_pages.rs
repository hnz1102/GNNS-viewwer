//! Page rendering for the TFT (320x240 RGB565). Everything here is generic
//! over `DrawTarget`, with no ESP-IDF dependency, so `tools/screenshots`
//! can compile this file on the host and render each page to a PNG.
use std::sync::{Arc, Mutex};
use core::sync::atomic::{AtomicU8, Ordering};
use embedded_graphics::{
    mono_font::{ascii::FONT_5X8, ascii::FONT_6X12, ascii::FONT_10X20, MonoTextStyle, MonoTextStyleBuilder},
    pixelcolor::Rgb565,
    text::Text,
    geometry::{Point, Size},
    primitives::{Circle, Line, Rectangle, PrimitiveStyle, PrimitiveStyleBuilder},
    prelude::*,
};

/// Terminal grid dimensions (320x240 display, 6x12 font).
/// Reserve top 20px for header → 220px / 12 = 18 rows.
pub(crate) const TERM_COLS: usize = 53;
pub(crate) const TERM_ROWS: usize = 18;
pub(crate) const CHAR_W: i32 = 6;
pub(crate) const CHAR_H: i32 = 12;
/// Header height in pixels.
pub(crate) const HEADER_H: i32 = 20;

// ─── Display page management ─────────────────────────────────────────────
// Page order: GPS(0) → SKYVIEW(1) → MAP(2) → TRACKLIST(3) → COM1(4) → INFO(5) → QUAKE(6) → GPS ...
pub const PAGE_GPS:       u8 = 0;
pub const PAGE_SKYVIEW:   u8 = 1;
pub const PAGE_MAP:       u8 = 2;
pub const PAGE_TRACKLIST: u8 = 3;
pub const PAGE_COM1:      u8 = 4;
pub const PAGE_INFO:      u8 = 5;
pub const PAGE_QUAKE:     u8 = 6;
pub(crate) const PAGE_COUNT: u8 = 7;

pub(crate) static CURRENT_PAGE: AtomicU8 = AtomicU8::new(PAGE_GPS);

pub fn next_page() {
    let cur = CURRENT_PAGE.load(Ordering::Relaxed);
    CURRENT_PAGE.store((cur + 1) % PAGE_COUNT, Ordering::SeqCst);
}

pub fn set_page(page: u8) {
    if page < PAGE_COUNT {
        CURRENT_PAGE.store(page, Ordering::SeqCst);
    }
}

pub fn current_page() -> u8 {
    CURRENT_PAGE.load(Ordering::Relaxed)
}

// ─── System message overlay (for important notifications) ────────────────
pub(crate) static SYSTEM_MESSAGE: Mutex<Option<(String, String)>> = Mutex::new(None);

/// Display a system message overlay (title + text).
pub fn show_system_message(title: &str, text: &str) {
    *SYSTEM_MESSAGE.lock().unwrap() = Some((title.to_string(), text.to_string()));
}

/// Clear the system message overlay.
pub fn clear_system_message() {
    *SYSTEM_MESSAGE.lock().unwrap() = None;
}

// ─── Shared info for Info page (set from main after init) ────────────────
pub struct DisplayAux {
    pub status: crate::httpserver::StatusInfo,
    pub com1_baud: String,
    pub com1_tx: String,
    pub com1_rx: String,
}

pub(crate) static DISPLAY_AUX: Mutex<Option<DisplayAux>> = Mutex::new(None);

pub fn set_display_aux(aux: DisplayAux) {
    *DISPLAY_AUX.lock().unwrap() = Some(aux);
}

// ─── ANSI color palette (standard 8 + bright 8) ──────────────────────────
pub(crate) const ANSI_COLORS: [Rgb565; 16] = [
    Rgb565::new(0, 0, 0),          // 0 black
    Rgb565::new(21, 0, 0),         // 1 red       (170,0,0)
    Rgb565::new(0, 42, 0),         // 2 green     (0,170,0)
    Rgb565::new(21, 42, 0),        // 3 yellow    (170,170,0)
    Rgb565::new(0, 0, 21),         // 4 blue      (0,0,170)
    Rgb565::new(21, 0, 21),        // 5 magenta   (170,0,170)
    Rgb565::new(0, 42, 21),        // 6 cyan      (0,170,170)
    Rgb565::new(21, 42, 21),       // 7 white     (170,170,170)
    Rgb565::new(10, 21, 10),       // 8 bright black (85,85,85)
    Rgb565::new(31, 0, 0),         // 9 bright red
    Rgb565::new(0, 63, 0),         // 10 bright green
    Rgb565::new(31, 63, 0),        // 11 bright yellow
    Rgb565::new(0, 0, 31),         // 12 bright blue
    Rgb565::new(31, 0, 31),        // 13 bright magenta
    Rgb565::new(0, 63, 31),        // 14 bright cyan
    Rgb565::new(31, 63, 31),       // 15 bright white
];

pub(crate) const DEFAULT_FG: Rgb565 = ANSI_COLORS[7];  // white
pub(crate) const DEFAULT_BG: Rgb565 = ANSI_COLORS[0];  // black

// ─── Terminal cell ────────────────────────────────────────────────────────
#[derive(Copy, Clone)]
pub(crate) struct Cell {
    ch: char,
    fg: Rgb565,
    bg: Rgb565,
}

impl Cell {
    pub(crate) fn blank() -> Self {
        Self { ch: ' ', fg: DEFAULT_FG, bg: DEFAULT_BG }
    }
}

// ─── ANSI parser state machine ───────────────────────────────────────────
#[derive(PartialEq)]
pub(crate) enum ParseState {
    Normal,
    Esc,        // got ESC
    Csi,        // got ESC[
    OscOrOther, // got ESC] or other; eat until ST
}

pub(crate) struct AnsiTerminal {
    cells: [[Cell; TERM_COLS]; TERM_ROWS],
    cursor_row: usize,
    cursor_col: usize,
    saved_row: usize,
    saved_col: usize,
    fg: Rgb565,
    bg: Rgb565,
    bold: bool,
    parse_state: ParseState,
    csi_params: Vec<u8>,
    dirty: bool,
}

impl AnsiTerminal {
    fn new() -> Self {
        Self {
            cells: [[Cell::blank(); TERM_COLS]; TERM_ROWS],
            cursor_row: 0,
            cursor_col: 0,
            saved_row: 0,
            saved_col: 0,
            fg: DEFAULT_FG,
            bg: DEFAULT_BG,
            bold: false,
            parse_state: ParseState::Normal,
            csi_params: Vec::new(),
            dirty: true,
        }
    }

    fn clear_all(&mut self) {
        for r in 0..TERM_ROWS {
            for c in 0..TERM_COLS {
                self.cells[r][c] = Cell::blank();
            }
        }
        self.dirty = true;
    }

    /// Scroll the terminal up by one line.
    fn scroll_up(&mut self) {
        for r in 1..TERM_ROWS {
            self.cells[r - 1] = self.cells[r];
        }
        self.cells[TERM_ROWS - 1] = [Cell::blank(); TERM_COLS];
        self.dirty = true;
    }

    /// Put a character at cursor position and advance.
    fn put_char(&mut self, ch: char) {
        if self.cursor_col >= TERM_COLS {
            // auto-wrap
            self.cursor_col = 0;
            self.cursor_row += 1;
        }
        if self.cursor_row >= TERM_ROWS {
            self.scroll_up();
            self.cursor_row = TERM_ROWS - 1;
        }
        self.cells[self.cursor_row][self.cursor_col] = Cell {
            ch,
            fg: self.fg,
            bg: self.bg,
        };
        self.cursor_col += 1;
        self.dirty = true;
    }

    /// Feed raw bytes from serial into the ANSI parser.
    fn feed(&mut self, data: &[u8]) {
        for &b in data {
            match self.parse_state {
                ParseState::Normal => {
                    match b {
                        0x1B => self.parse_state = ParseState::Esc,
                        b'\n' => {
                            self.cursor_row += 1;
                            if self.cursor_row >= TERM_ROWS {
                                self.scroll_up();
                                self.cursor_row = TERM_ROWS - 1;
                            }
                            self.dirty = true;
                        }
                        b'\r' => {
                            self.cursor_col = 0;
                            self.dirty = true;
                        }
                        b'\x08' => {
                            // backspace
                            if self.cursor_col > 0 {
                                self.cursor_col -= 1;
                            }
                            self.dirty = true;
                        }
                        b'\t' => {
                            // tab: advance to next 8-col stop
                            let next = (self.cursor_col + 8) & !7;
                            let next = if next > TERM_COLS { TERM_COLS } else { next };
                            while self.cursor_col < next {
                                self.put_char(' ');
                            }
                        }
                        0x00..=0x1F => {} // ignore other control chars
                        _ => self.put_char(b as char),
                    }
                }
                ParseState::Esc => {
                    match b {
                        b'[' => {
                            self.csi_params.clear();
                            self.parse_state = ParseState::Csi;
                        }
                        b']' | b'(' | b')' | b'#' => {
                            self.parse_state = ParseState::OscOrOther;
                        }
                        b'7' => {
                            // save cursor
                            self.saved_row = self.cursor_row;
                            self.saved_col = self.cursor_col;
                            self.parse_state = ParseState::Normal;
                        }
                        b'8' => {
                            // restore cursor
                            self.cursor_row = self.saved_row;
                            self.cursor_col = self.saved_col;
                            self.parse_state = ParseState::Normal;
                        }
                        b'c' => {
                            // full reset
                            self.clear_all();
                            self.cursor_row = 0;
                            self.cursor_col = 0;
                            self.fg = DEFAULT_FG;
                            self.bg = DEFAULT_BG;
                            self.bold = false;
                            self.parse_state = ParseState::Normal;
                        }
                        b'D' => {
                            // index (scroll up)
                            self.cursor_row += 1;
                            if self.cursor_row >= TERM_ROWS {
                                self.scroll_up();
                                self.cursor_row = TERM_ROWS - 1;
                            }
                            self.parse_state = ParseState::Normal;
                        }
                        b'M' => {
                            // reverse index (scroll down)
                            if self.cursor_row == 0 {
                                // scroll down: move rows 0..N-2 to 1..N-1
                                for r in (1..TERM_ROWS).rev() {
                                    self.cells[r] = self.cells[r - 1];
                                }
                                self.cells[0] = [Cell::blank(); TERM_COLS];
                            } else {
                                self.cursor_row -= 1;
                            }
                            self.dirty = true;
                            self.parse_state = ParseState::Normal;
                        }
                        _ => self.parse_state = ParseState::Normal,
                    }
                }
                ParseState::Csi => {
                    if b >= b'0' && b <= b'9' || b == b';' || b == b'?' {
                        self.csi_params.push(b);
                    } else {
                        // b is the final byte (command)
                        self.handle_csi(b);
                        self.parse_state = ParseState::Normal;
                    }
                }
                ParseState::OscOrOther => {
                    // eat until BEL or ST (ESC\)
                    if b == 0x07 || b == b'\\' || (b >= 0x40 && b <= 0x7E && b != b'[') {
                        self.parse_state = ParseState::Normal;
                    }
                }
            }
        }
    }

    /// Parse CSI parameter string into a list of u16 values.
    fn parse_params(&self) -> Vec<u16> {
        if self.csi_params.is_empty() {
            return Vec::new();
        }
        // Filter out '?' prefix
        let s: Vec<u8> = self.csi_params.iter()
            .filter(|&&b| b != b'?')
            .cloned()
            .collect();
        let s = std::str::from_utf8(&s).unwrap_or("");
        s.split(';')
            .map(|p| p.parse::<u16>().unwrap_or(0))
            .collect()
    }

    fn handle_csi(&mut self, cmd: u8) {
        let params = self.parse_params();
        let p1 = *params.first().unwrap_or(&0);
        let p2 = *params.get(1).unwrap_or(&0);

        match cmd {
            b'A' => {
                // CUU – Cursor Up
                let n = if p1 == 0 { 1 } else { p1 as usize };
                self.cursor_row = self.cursor_row.saturating_sub(n);
                self.dirty = true;
            }
            b'B' => {
                // CUD – Cursor Down
                let n = if p1 == 0 { 1 } else { p1 as usize };
                self.cursor_row = (self.cursor_row + n).min(TERM_ROWS - 1);
                self.dirty = true;
            }
            b'C' => {
                // CUF – Cursor Forward
                let n = if p1 == 0 { 1 } else { p1 as usize };
                self.cursor_col = (self.cursor_col + n).min(TERM_COLS - 1);
                self.dirty = true;
            }
            b'D' => {
                // CUB – Cursor Backward
                let n = if p1 == 0 { 1 } else { p1 as usize };
                self.cursor_col = self.cursor_col.saturating_sub(n);
                self.dirty = true;
            }
            b'H' | b'f' => {
                // CUP – Cursor Position  ESC[row;colH
                let row = if p1 == 0 { 1 } else { p1 as usize };
                let col = if p2 == 0 { 1 } else { p2 as usize };
                self.cursor_row = (row - 1).min(TERM_ROWS - 1);
                self.cursor_col = (col - 1).min(TERM_COLS - 1);
                self.dirty = true;
            }
            b'J' => {
                // ED – Erase in Display
                match p1 {
                    0 => {
                        // erase from cursor to end
                        for c in self.cursor_col..TERM_COLS {
                            self.cells[self.cursor_row][c] = Cell::blank();
                        }
                        for r in (self.cursor_row + 1)..TERM_ROWS {
                            self.cells[r] = [Cell::blank(); TERM_COLS];
                        }
                    }
                    1 => {
                        // erase from start to cursor
                        for r in 0..self.cursor_row {
                            self.cells[r] = [Cell::blank(); TERM_COLS];
                        }
                        for c in 0..=self.cursor_col.min(TERM_COLS - 1) {
                            self.cells[self.cursor_row][c] = Cell::blank();
                        }
                    }
                    2 | 3 => {
                        // erase entire screen
                        self.clear_all();
                    }
                    _ => {}
                }
                self.dirty = true;
            }
            b'K' => {
                // EL – Erase in Line
                match p1 {
                    0 => {
                        for c in self.cursor_col..TERM_COLS {
                            self.cells[self.cursor_row][c] = Cell::blank();
                        }
                    }
                    1 => {
                        for c in 0..=self.cursor_col.min(TERM_COLS - 1) {
                            self.cells[self.cursor_row][c] = Cell::blank();
                        }
                    }
                    2 => {
                        self.cells[self.cursor_row] = [Cell::blank(); TERM_COLS];
                    }
                    _ => {}
                }
                self.dirty = true;
            }
            b'G' => {
                // CHA – Cursor Horizontal Absolute
                let col = if p1 == 0 { 1 } else { p1 as usize };
                self.cursor_col = (col - 1).min(TERM_COLS - 1);
                self.dirty = true;
            }
            b'd' => {
                // VPA – Vertical Position Absolute
                let row = if p1 == 0 { 1 } else { p1 as usize };
                self.cursor_row = (row - 1).min(TERM_ROWS - 1);
                self.dirty = true;
            }
            b'S' => {
                // SU – Scroll Up
                let n = if p1 == 0 { 1 } else { p1 as usize };
                for _ in 0..n { self.scroll_up(); }
            }
            b'T' => {
                // SD – Scroll Down
                let n = if p1 == 0 { 1 } else { p1 as usize };
                for _ in 0..n {
                    for r in (1..TERM_ROWS).rev() {
                        self.cells[r] = self.cells[r - 1];
                    }
                    self.cells[0] = [Cell::blank(); TERM_COLS];
                }
                self.dirty = true;
            }
            b'L' => {
                // IL – Insert Lines
                let n = if p1 == 0 { 1 } else { p1 as usize };
                let n = n.min(TERM_ROWS - self.cursor_row);
                for _ in 0..n {
                    for r in (self.cursor_row + 1..TERM_ROWS).rev() {
                        self.cells[r] = self.cells[r - 1];
                    }
                    self.cells[self.cursor_row] = [Cell::blank(); TERM_COLS];
                }
                self.dirty = true;
            }
            b'M' => {
                // DL – Delete Lines
                let n = if p1 == 0 { 1 } else { p1 as usize };
                let n = n.min(TERM_ROWS - self.cursor_row);
                for _ in 0..n {
                    for r in self.cursor_row..(TERM_ROWS - 1) {
                        self.cells[r] = self.cells[r + 1];
                    }
                    self.cells[TERM_ROWS - 1] = [Cell::blank(); TERM_COLS];
                }
                self.dirty = true;
            }
            b'P' => {
                // DCH – Delete Characters
                let n = if p1 == 0 { 1 } else { p1 as usize };
                let n = n.min(TERM_COLS - self.cursor_col);
                for c in self.cursor_col..(TERM_COLS - n) {
                    self.cells[self.cursor_row][c] = self.cells[self.cursor_row][c + n];
                }
                for c in (TERM_COLS - n)..TERM_COLS {
                    self.cells[self.cursor_row][c] = Cell::blank();
                }
                self.dirty = true;
            }
            b'@' => {
                // ICH – Insert Characters
                let n = if p1 == 0 { 1 } else { p1 as usize };
                let n = n.min(TERM_COLS - self.cursor_col);
                for c in (self.cursor_col + n..TERM_COLS).rev() {
                    self.cells[self.cursor_row][c] = self.cells[self.cursor_row][c - n];
                }
                for c in self.cursor_col..(self.cursor_col + n) {
                    self.cells[self.cursor_row][c] = Cell::blank();
                }
                self.dirty = true;
            }
            b'X' => {
                // ECH – Erase Characters
                let n = if p1 == 0 { 1 } else { p1 as usize };
                for c in self.cursor_col..(self.cursor_col + n).min(TERM_COLS) {
                    self.cells[self.cursor_row][c] = Cell::blank();
                }
                self.dirty = true;
            }
            b'm' => {
                // SGR – Select Graphic Rendition
                if params.is_empty() {
                    self.sgr_reset();
                } else {
                    let mut i = 0;
                    while i < params.len() {
                        match params[i] {
                            0 => self.sgr_reset(),
                            1 => self.bold = true,
                            22 => self.bold = false,
                            7 => {
                                // reverse video
                                std::mem::swap(&mut self.fg, &mut self.bg);
                            }
                            27 => {
                                // un-reverse
                                std::mem::swap(&mut self.fg, &mut self.bg);
                            }
                            // Foreground standard
                            30..=37 => {
                                let idx = (params[i] - 30) as usize;
                                self.fg = if self.bold { ANSI_COLORS[idx + 8] } else { ANSI_COLORS[idx] };
                            }
                            39 => self.fg = DEFAULT_FG,
                            // Background standard
                            40..=47 => {
                                let idx = (params[i] - 40) as usize;
                                self.bg = ANSI_COLORS[idx];
                            }
                            49 => self.bg = DEFAULT_BG,
                            // Foreground bright
                            90..=97 => {
                                let idx = (params[i] - 90 + 8) as usize;
                                self.fg = ANSI_COLORS[idx];
                            }
                            // Background bright
                            100..=107 => {
                                let idx = (params[i] - 100 + 8) as usize;
                                self.bg = ANSI_COLORS[idx];
                            }
                            // 256-color mode: 38;5;n or 48;5;n
                            38 => {
                                if i + 2 < params.len() && params[i + 1] == 5 {
                                    self.fg = color256_to_rgb565(params[i + 2]);
                                    i += 2;
                                }
                            }
                            48 => {
                                if i + 2 < params.len() && params[i + 1] == 5 {
                                    self.bg = color256_to_rgb565(params[i + 2]);
                                    i += 2;
                                }
                            }
                            _ => {} // ignore unsupported
                        }
                        i += 1;
                    }
                }
                // SGR doesn't mark positional dirty, but color changes
                // will appear on next characters written.
            }
            b's' => {
                // SCP – Save Cursor Position
                self.saved_row = self.cursor_row;
                self.saved_col = self.cursor_col;
            }
            b'u' => {
                // RCP – Restore Cursor Position
                self.cursor_row = self.saved_row;
                self.cursor_col = self.saved_col;
                self.dirty = true;
            }
            b'h' | b'l' | b'r' | b'n' | b'c' => {
                // mode set/reset, scroll region, device status — ignore
            }
            _ => {} // unknown CSI
        }
    }

    fn sgr_reset(&mut self) {
        self.fg = DEFAULT_FG;
        self.bg = DEFAULT_BG;
        self.bold = false;
    }
}

/// Map 256-color index to Rgb565.
pub(crate) fn color256_to_rgb565(n: u16) -> Rgb565 {
    if n < 16 {
        return ANSI_COLORS[n as usize];
    }
    if n < 232 {
        // 6x6x6 color cube: 16 + 36*r + 6*g + b  (r,g,b ∈ 0..5)
        let idx = (n - 16) as u32;
        let b_val = (idx % 6) as u8;
        let g_val = ((idx / 6) % 6) as u8;
        let r_val = ((idx / 36) % 6) as u8;
        // Scale 0..5 to 5-bit (0..31) for R/B and 6-bit (0..63) for G
        let r5 = (r_val as u16 * 31 + 2) / 5;
        let g6 = (g_val as u16 * 63 + 2) / 5;
        let b5 = (b_val as u16 * 31 + 2) / 5;
        Rgb565::new(r5 as u8, g6 as u8, b5 as u8)
    } else {
        // grayscale ramp: 232..255 → shades 8,18,28,...238
        let shade = ((n - 232) * 10 + 8) as u16;
        let r5 = (shade * 31 + 127) / 255;
        let g6 = (shade * 63 + 127) / 255;
        let b5 = (shade * 31 + 127) / 255;
        Rgb565::new(r5 as u8, g6 as u8, b5 as u8)
    }
}

// ─── Public shared buffer (thin wrapper around AnsiTerminal) ─────────────
#[derive(Clone)]
pub struct SerialRxBuffer {
    inner: Arc<Mutex<AnsiTerminal>>,
}

impl SerialRxBuffer {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(AnsiTerminal::new())),
        }
    }

    /// Push raw bytes from serial into the ANSI terminal emulator.
    pub fn push_data(&self, data: &[u8]) {
        let mut term = self.inner.lock().unwrap();
        term.feed(data);
    }
}

// ─── Battery icon ────────────────────────────────────────────────────────

/// Battery level: `BATT_NONE` = no reading, `BATT_USB` = externally powered
/// (voltage >= 4.5 V), 0..=5 = number of lit segments.
pub(crate) const BATT_NONE: u8 = 255;
pub(crate) const BATT_USB: u8 = 254;
pub(crate) const USB_ENTER_V: f32 = 4.50;
pub(crate) const USB_EXIT_V: f32 = 4.45;
pub(crate) const BATT_SEGS: u8 = 5;

pub(crate) fn battery_voltage() -> f32 {
    DISPLAY_AUX.lock().unwrap().as_ref()
        .map(|aux| *aux.status.battery_voltage.lock().unwrap())
        .unwrap_or(0.0)
}

/// Single-cell Li-ion thresholds with hysteresis (same steps as wakepcbox),
/// so the icon doesn't flicker when the voltage hovers near a boundary.
pub(crate) fn next_battery_level(prev: u8, v: f32) -> u8 {
    const UP:   [f32; 5] = [3.75, 3.85, 3.95, 4.00, 4.05]; // level k -> k+1
    const DOWN: [f32; 5] = [3.70, 3.80, 3.90, 3.95, 4.00]; // level k+1 -> k
    if v < 2.0 { return BATT_NONE; }          // no battery / not measured
    if prev == BATT_USB {
        if v >= USB_EXIT_V { return BATT_USB; }
        return UP.iter().filter(|&&t| v > t).count() as u8;
    }
    if v >= USB_ENTER_V { return BATT_USB; }
    if prev == BATT_NONE || prev > BATT_SEGS {
        // First reading: pick the level directly
        return UP.iter().filter(|&&t| v > t).count() as u8;
    }
    let mut lvl = prev;
    if (lvl as usize) < UP.len() && v > UP[lvl as usize] { lvl += 1; }
    else if lvl > 0 && v < DOWN[lvl as usize - 1] { lvl -= 1; }
    lvl
}

pub(crate) fn draw_battery_icon<D: DrawTarget<Color = Rgb565>>(display: &mut D, level: u8) {
    const X: i32 = 288;
    const Y: i32 = 4;
    const BODY_W: u32 = 26;
    const BODY_H: u32 = 12;
    // Clear the icon area (header background)
    let _ = Rectangle::new(Point::new(X - 2, 0), Size::new(320 - (X as u32 - 2), HEADER_H as u32))
        .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
        .draw(display);
    if level == BATT_NONE { return; }
    if level == BATT_USB {
        let style = MonoTextStyle::new(&FONT_6X12, Rgb565::CSS_CYAN);
        let _ = Text::new("USB", Point::new(X + 4, Y + 10), style).draw(display);
        return;
    }
    let color = match level {
        0 | 1 => Rgb565::CSS_RED,
        2     => Rgb565::CSS_ORANGE,
        _     => Rgb565::CSS_LIGHT_GREEN,
    };
    // Body outline + terminal tip
    let _ = Rectangle::new(Point::new(X, Y), Size::new(BODY_W, BODY_H))
        .into_styled(PrimitiveStyle::with_stroke(Rgb565::WHITE, 1))
        .draw(display);
    let _ = Rectangle::new(Point::new(X + BODY_W as i32, Y + 3), Size::new(2, BODY_H - 6))
        .into_styled(PrimitiveStyle::with_fill(Rgb565::WHITE))
        .draw(display);
    // Segments (4 px wide, 1 px gap)
    for i in 0..level.min(BATT_SEGS) as i32 {
        let _ = Rectangle::new(Point::new(X + 2 + i * 5, Y + 2), Size::new(4, BODY_H - 4))
            .into_styled(PrimitiveStyle::with_fill(color))
            .draw(display);
    }
}

// ─── System message overlay drawing ──────────────────────────────────────

pub(crate) fn draw_system_message<D: DrawTarget<Color = Rgb565>>(
    display: &mut D,
    title: &str,
    text: &str,
    header_style: &MonoTextStyle<Rgb565>,
    force_clear: bool,
) {
    // Only full clear and redraw header on first display or when message changes
    if force_clear {
        let _ = display.clear(Rgb565::BLACK);
        
        // Draw title in header
        let _ = Text::new(title, Point::new(4, 16), *header_style).draw(display);
        let _ = Rectangle::new(Point::new(0, HEADER_H), Size::new(320, 1))
            .into_styled(PrimitiveStyle::with_fill(Rgb565::CSS_DARK_GREEN))
            .draw(display);
        
        // Clear message area once
        let _ = Rectangle::new(Point::new(0, HEADER_H + 1), Size::new(320, 240 - HEADER_H as u32 - 1))
            .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
            .draw(display);
    }
    
    // Draw message text with background color to overwrite old text without clearing
    let msg_style = MonoTextStyleBuilder::new()
        .font(&FONT_10X20)
        .text_color(Rgb565::YELLOW)
        .background_color(Rgb565::BLACK)
        .build();
    
    // Draw text lines - background color ensures old text is overwritten
    let y_start = 80;
    let lines: Vec<&str> = text.lines().collect();
    
    // Draw actual lines
    for (i, line) in lines.iter().enumerate() {
        let y = y_start + (i as i32 * 24);
        // Pad line to full width to ensure complete overwrite of previous text
        let padded = format!("{:<40}", line);  // Pad to ~40 chars (covers ~240px with 6px font width)
        let _ = Text::new(&padded, Point::new(10, y), msg_style).draw(display);
    }
    
    // Clear any remaining lines that were used before but not now (if not force_clear)
    if !force_clear && lines.len() < 6 {
        for i in lines.len()..6 {
            let y = y_start + (i as i32 * 24);
            let blank = format!("{:<40}", "");
            let _ = Text::new(&blank, Point::new(10, y), msg_style).draw(display);
        }
    }
}

// ─── Header drawing ──────────────────────────────────────────────────────

pub(crate) fn draw_header<D: DrawTarget<Color = Rgb565>>(
    display: &mut D,
    page: u8,
    style: &MonoTextStyle<Rgb565>,
) {
    let page_num = page + 1;
    let title = match page {
        PAGE_GPS       => format!("GNSS [COM1]   {}/{}", page_num, PAGE_COUNT),
        PAGE_SKYVIEW   => format!("SKY VIEW      {}/{}", page_num, PAGE_COUNT),
        PAGE_COM1      => format!("SERIAL [COM1] {}/{}", page_num, PAGE_COUNT),
        PAGE_INFO      => format!("SYSTEM INFO   {}/{}", page_num, PAGE_COUNT),
        PAGE_MAP       => if crate::track_view::is_playing() {
            format!("TRACK PLAYBACK {}/{}", page_num, PAGE_COUNT)
        } else {
            format!("MAP           {}/{}", page_num, PAGE_COUNT)
        },
        PAGE_TRACKLIST => format!("SAVED TRACKS  {}/{}", page_num, PAGE_COUNT),
        PAGE_QUAKE     => format!("EARTHQUAKE    {}/{}", page_num, PAGE_COUNT),
        _              => format!("PAGE {}        {}/{}", page, page_num, PAGE_COUNT),
    };
    let _ = Rectangle::new(Point::new(0, 0), Size::new(320, HEADER_H as u32))
        .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
        .draw(display);
    // An active EEW takes over the MAP header.
    let eew = if page == PAGE_MAP { crate::quake::eew() } else { None };
    if let Some(e) = eew {
        let red = MonoTextStyle::new(&FONT_10X20, Rgb565::RED);
        let t = format!("!! EEW M{:.1} I{} !!", e.mag.max(0.0), e.intensity());
        let _ = Text::new(&t, Point::new(4, 16), red).draw(display);
    } else if let (PAGE_MAP, Some(q)) = (page, crate::quake::new_quake()) {
        let yellow = MonoTextStyle::new(&FONT_10X20, Rgb565::YELLOW);
        let t = format!("NEW QUAKE M{:.1} I{}", q.mag.max(0.0), q.intensity());
        let _ = Text::new(&t, Point::new(4, 16), yellow).draw(display);
    } else {
        let _ = Text::new(&title, Point::new(4, 16), *style).draw(display);
    }
    let _ = Rectangle::new(Point::new(0, HEADER_H), Size::new(320, 1))
        .into_styled(PrimitiveStyle::with_fill(Rgb565::CSS_DARK_GREEN))
        .draw(display);
}

// ─── Button legend ───────────────────────────────────────────────────────
//
// Same look on every page so the on-screen hint matches the physical buttons:
// B0 (page) = yellow round button, GPIO4 = blue "+UP", GPIO5 = blue "-DOWN",
// B6 = red round button. Round buttons carry a short label of what they do
// on the current page.

pub(crate) enum Legend {
    /// B0 — yellow round button (page switch / exit).
    Page(&'static str),
    /// GPIO4 — blue "+UP".
    Up,
    /// GPIO5 — blue "-DOWN".
    Down,
    /// B6 — red round button.
    Action(&'static str),
}

const LEGEND_BLUE: Rgb565 = Rgb565::new(8, 28, 31);

/// Draws `items` left to right starting at `x`; `baseline` is the text baseline
/// (FONT_6X12). Returns the x just past the last item.
pub(crate) fn draw_legend<D: DrawTarget<Color = Rgb565>>(
    display: &mut D, x: i32, baseline: i32, items: &[Legend],
) -> i32 {
    let mut x = x;
    for item in items {
        let (color, label, round) = match item {
            Legend::Page(l)   => (Rgb565::YELLOW, *l, true),
            Legend::Action(l) => (Rgb565::RED, *l, true),
            Legend::Up        => (LEGEND_BLUE, "+UP", false),
            Legend::Down      => (LEGEND_BLUE, "-DOWN", false),
        };
        if round {
            let _ = Circle::new(Point::new(x, baseline - 9), 9)
                .into_styled(PrimitiveStyle::with_fill(color)).draw(display);
            x += 12;
        }
        let style = MonoTextStyle::new(&FONT_6X12, color);
        let _ = Text::new(label, Point::new(x, baseline), style).draw(display);
        x += label.len() as i32 * CHAR_W + 10;
    }
    x
}

// ─── Map page ─────────────────────────────────────────────────────────────
//
// Shows a Google Maps Static API JPEG centred on the current GPS position.
// Top 200px = map image; bottom 40px = GPS lat/lon + location name bar.
// Requires a Maps API key (Web UI / cfg.toml) and WiFi connectivity.

/// Web Mercator projection: (lat, lon) → screen pixel, for a map image of
/// `crate::map_display::MAP_W`×(y_bot−y_top) centred at (clat, clon) at the
/// given zoom. Returns `None` if `clat` is NaN (no map yet) or the point
/// falls outside the visible image.
pub(crate) fn project_to_map(clat: f64, clon: f64, zoom: u8, lat: f64, lon: f64, y_top: i32, y_bot: i32) -> Option<(i32, i32)> {
    if clat.is_nan() { return None; }
    let world = 256.0_f64 * ((1u64 << zoom) as f64);
    let lat_r  = lat.to_radians();
    let clat_r = clat.to_radians();
    let merc_pt  = (lat_r  / 2.0 + core::f64::consts::FRAC_PI_4).tan().ln();
    let merc_cen = (clat_r / 2.0 + core::f64::consts::FRAC_PI_4).tan().ln();
    let dx = (lon - clon) * world / 360.0;
    let dy = -(merc_pt - merc_cen) * world / (2.0 * core::f64::consts::PI);
    let px = (crate::map_display::MAP_W as f64 / 2.0 + dx).round() as i32;
    let py = y_top + ((y_bot - y_top) as f64 / 2.0 + dy).round() as i32;
    if px >= 0 && px < crate::map_display::MAP_W as i32 && py >= y_top && py < y_bot {
        Some((px, py))
    } else {
        None
    }
}

/// (lat, lon) of the last point for which track playback forced an
/// immediate map refetch (see the off-screen check in `draw_map_page`).
/// Bounds forced refetches to one attempt per distinct point: `draw_map_page`
/// runs every ~50 ms display tick regardless of whether anything changed, so
/// without this, a fetch that keeps failing would be re-forced on every
/// single tick. (`map_display::reset_last_pos()` additionally rate-limits
/// itself, shared with every other caller — zoom, map-type toggle, etc. —
/// so a rapid run of distinct off-screen points during fast scrubbing still
/// collapses into fetching only the last one once that floor passes.)
pub(crate) static LAST_FORCED_PLAYBACK_POS: Mutex<(f64, f64)> = Mutex::new((f64::NAN, f64::NAN));

/// Cap on how many points of a replayed track get drawn as line segments.
/// The map is only 320 px wide, so more than this can't be resolved anyway,
/// while a full day's track (thousands of points) drawn segment by segment
/// over SPI takes long enough to visibly stall the redraw.
pub(crate) const ROUTE_DRAW_MAX: usize = 240;

pub(crate) fn draw_map_page<D: DrawTarget<Color = Rgb565>>(
    display: &mut D,
    force: bool,
    prev_sig: &mut u32,
    prev_trail_len: &mut usize,
) {
    const MAP_Y_TOP: i32 = HEADER_H + 1;
    const MAP_Y_BOT: i32 = 200;           // map occupies rows 21–199
    const INFO_Y:    i32 = MAP_Y_BOT;     // info bar rows 200–239

    let playing = crate::track_view::is_playing();
    let gps = crate::gps_parser::get_data();
    let track_pt = if playing { crate::track_view::current_point() } else { None };

    // Position shown on the map: the live GPS fix, or the point currently
    // selected during track playback.
    // Epicenter chosen on the quake page, if any (centres the map there).
    let quake_focus = if playing { None } else { crate::quake::focus() };
    let (cur_lat, cur_lon, cur_valid) = if let Some(ref p) = track_pt {
        (p.lat, p.lon, true)
    } else if let Some((fl, fo)) = quake_focus {
        (fl, fo, true)
    } else if !playing {
        (gps.lat_dd, gps.lon_dd, gps.fix_quality > 0 && !gps.lat_dd.is_nan())
    } else {
        (f64::NAN, f64::NAN, false)
    };

    // Where this unit actually is (marked with the crosshair), as opposed to the
    // point the map is centred on, which a quake focus moves elsewhere.
    let (own_lat, own_lon, own_valid) = if let Some(ref p) = track_pt {
        (p.lat, p.lon, true)
    } else if !playing {
        (gps.lat_dd, gps.lon_dd, gps.fix_quality > 0 && !gps.lat_dd.is_nan())
    } else {
        (f64::NAN, f64::NAN, false)
    };

    if quake_focus.is_some() {
        crate::map_display::trigger(cur_lat, cur_lon);
    } else if !playing && cur_valid {
        // Push current GPS fix onto the trail (map_display deduplicates ~5 m)
        crate::map_display::push_trail(cur_lat, cur_lon);
        crate::map_display::trigger(cur_lat, cur_lon);
    } else if playing && cur_valid {
        // trigger()'s live-tracking heuristics (only refetch after ~500 m of
        // movement, at most once every 60 s) are wrong for playback: the
        // ZoomUp/Down step can jump between points far apart in space while
        // being milliseconds apart in wall-clock time, so a point can walk
        // straight off the currently displayed tile without either
        // threshold tripping, leaving the map stuck on stale imagery.
        // Instead, force an immediate refetch whenever the replayed point
        // has left the map image that's actually on screen right now.
        let off_screen = !crate::map_display::has_map() || {
            let (clat, clon) = crate::map_display::get_map_center();
            let zoom = crate::map_display::get_tile_zoom();
            project_to_map(clat, clon, zoom, cur_lat, cur_lon, MAP_Y_TOP, MAP_Y_BOT).is_none()
        };
        if off_screen {
            let mut last_pos = LAST_FORCED_PLAYBACK_POS.lock().unwrap();
            if *last_pos != (cur_lat, cur_lon) {
                *last_pos = (cur_lat, cur_lon);
                drop(last_pos);
                crate::map_display::reset_last_pos();
            }
        }
        crate::map_display::trigger(cur_lat, cur_lon);
    } else if !playing && !cur_valid {
        // No GNSS fix and no quake focus: keep refreshing the map around the point
        // it is already showing, so zoom / map-type changes still take effect
        // (without this they were silently ignored until a fix arrived).
        let (clat, clon) = crate::map_display::get_map_center();
        if !clat.is_nan() {
            crate::map_display::trigger(clat, clon);
        }
    }

    let map_ver = crate::map_display::version();
    let has     = crate::map_display::has_map();
    let busy    = crate::map_display::is_busy();
    let t_len   = if playing { crate::track_view::point_index() + 1 } else { crate::map_display::trail_len() };

    // ── Change detection ─────────────────────────────────────────────────────
    // Include satellite azimuth/elevation in sig so sub-satellite dots update
    // when satellites move (only relevant at low zoom, and not while replaying
    // a historical track — today's sky doesn't match the replayed point).
    let sat_sig: u32 = if !playing && crate::map_display::get_tile_zoom() <= 5 {
        gps.sats.iter().fold(0u32, |acc, s| {
            acc.wrapping_add(s.azimuth as u32)
               .wrapping_add((s.elevation as u32).wrapping_mul(360))
        })
    } else { 0 };
    let map_sig = map_ver
        .wrapping_mul(31)
        .wrapping_add(gps.fix_quality as u32)
        .wrapping_add(if busy { 1 } else { 0 })
        .wrapping_add(sat_sig.wrapping_mul(7))
        .wrapping_add(crate::quake::version().wrapping_mul(13))
        .wrapping_add(crate::quake::eew_version().wrapping_mul(17))
        .wrapping_add(crate::quake::newq_version().wrapping_mul(19));

    let old_trail_len = *prev_trail_len;
    // During playback the cursor can move backward, which needs a full
    // repaint (there's no way to selectively erase already-drawn trail
    // segments), so any index change there forces a full redraw.
    let map_changed = force || map_sig != *prev_sig || (playing && t_len != old_trail_len);
    let trail_grew  = !playing && t_len > old_trail_len;

    if !map_changed && !trail_grew { return; }

    *prev_sig       = map_sig;
    *prev_trail_len = t_len;

    // ── Map background (only on full redraw) ─────────────────────────────────
    if map_changed {
        if force || !has {
            let _ = Rectangle::new(
                Point::new(0, MAP_Y_TOP),
                Size::new(320, (MAP_Y_BOT - MAP_Y_TOP) as u32),
            ).into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK)).draw(display);
        }

        let s_dim = MonoTextStyleBuilder::new().font(&FONT_6X12)
            .text_color(Rgb565::new(0, 25, 12)).build();

        if !has {
            let msg = if !crate::map_display::has_api_key() {
                "Set Maps API key in Web UI"
            } else if !playing && gps.fix_quality == 0 {
                "Waiting for GNSS fix..."
            } else if busy {
                "Fetching map..."
            } else {
                "Map unavailable"
            };
            let _ = Text::new(msg, Point::new(4, MAP_Y_TOP + 90), s_dim).draw(display);
        } else {
            // Draw stored RGB565 pixels
            let map_rect = Rectangle::new(
                Point::new(0, MAP_Y_TOP),
                Size::new(crate::map_display::MAP_W as u32,
                          (MAP_Y_BOT - MAP_Y_TOP) as u32),
            );
            crate::map_display::with_pixels(|pixels| {
                let _ = display.fill_contiguous(
                    &map_rect,
                    pixels.iter().map(|&p| Rgb565::new(
                        ((p >> 11) & 0x1F) as u8,
                        ((p >> 5)  & 0x3F) as u8,
                        (p & 0x1F) as u8,
                    )),
                );
            });
        }
    }

    // ── Trail overlay + crosshair ─────────────────────────────────────────────
    if has {
        let (clat, clon) = crate::map_display::get_map_center();
        // The zoom the displayed image was fetched at, not the current
        // setting — they differ while a refetch after a zoom change is in
        // flight, and everything below is drawn onto that image.
        let zoom         = crate::map_display::get_tile_zoom();
        let trail = if playing {
            crate::track_view::trail_up_to_current(ROUTE_DRAW_MAX)
        } else {
            crate::map_display::get_trail()
        };

        // Web Mercator projection: lat/lon → screen pixel.
        // clat/clon/zoom are Copy so the closure captures by value.
        let to_px = |lat: f32, lon: f32| -> Option<(i32, i32)> {
            project_to_map(clat, clon, zoom, lat as f64, lon as f64, MAP_Y_TOP, MAP_Y_BOT)
        };

        // ── Whole-route outline + end marker (playback only) ──────────────
        // Drawn underneath the replayed portion so the full track and where
        // it ends are visible from the very first point — that first screen
        // is zoomed out to fit the whole route (see track_view's
        // select_and_play), which is only useful if the route is on it.
        if playing && map_changed {
            // Bright yellow at 2 px: a dim thin line disappears completely
            // against satellite imagery, and yellow stays distinct from the
            // cyan replayed portion drawn over it.
            let route_style = PrimitiveStyle::with_stroke(Rgb565::new(31, 63, 0), 2);
            let mut prev: Option<(i32, i32)> = None;
            for &(la, lo) in crate::track_view::route_outline(ROUTE_DRAW_MAX).iter() {
                let cur = to_px(la, lo);
                // Both ends on screen, and not already the same pixel.
                if let (Some(a), Some(b)) = (prev, cur) {
                    if a != b {
                        let _ = Line::new(Point::new(a.0, a.1), Point::new(b.0, b.1))
                            .into_styled(route_style).draw(display);
                    }
                }
                prev = cur;
            }

            if let Some((elat, elon)) = crate::track_view::end_point() {
                if let Some((ex, ey)) = to_px(elat as f32, elon as f32) {
                    let _ = Circle::with_center(Point::new(ex, ey), 5)
                        .into_styled(PrimitiveStyle::with_fill(Rgb565::new(31, 0, 31)))
                        .draw(display);
                    let s_end = MonoTextStyleBuilder::new().font(&FONT_5X8)
                        .text_color(Rgb565::WHITE).build();
                    let _ = Text::new("E", Point::new(ex + 5, ey + 3), s_end).draw(display);
                }
            }
        }

        // Draw trail as connected lines with colour gradient (oldest=dim → newest=bright).
        // Full redraw: all segments.  Trail-only: from last-drawn point to new tail.
        let n = trail.len();
        let draw_from = if map_changed { 0 } else { old_trail_len.saturating_sub(1) };

        if n >= 2 {
            let segs = (n - 1) as f32;
            for i in draw_from..n.saturating_sub(1) {
                let &(lat0, lon0) = &trail[i];
                let &(lat1, lon1) = &trail[i + 1];
                if let (Some((x0, y0)), Some((x1, y1))) = (to_px(lat0, lon0), to_px(lat1, lon1)) {
                    // t: 0.0 = oldest segment, 1.0 = newest
                    let t = if segs > 0.0 { i as f32 / segs } else { 1.0 };
                    // Gradient: mid cyan → bright cyan. The dark end used to
                    // start near-black, which vanished against satellite
                    // imagery — and during playback it is drawn over the
                    // yellow route outline, so a dark oldest segment made the
                    // replayed part look *less* visible than the rest.
                    let g = (34.0 + t * 29.0) as u8; // 34..63
                    let b = (16.0 + t * 15.0) as u8; // 16..31
                    let _ = Line::new(Point::new(x0, y0), Point::new(x1, y1))
                        .into_styled(PrimitiveStyle::with_stroke(Rgb565::new(0, g, b), 2))
                        .draw(display);
                }
            }
        }
        // Bright dot at the latest position
        if let Some(&(lat, lon)) = trail.last() {
            if let Some((px, py)) = to_px(lat, lon) {
                let _ = Circle::with_center(Point::new(px, py), 3)
                    .into_styled(PrimitiveStyle::with_fill(Rgb565::new(31, 63, 31)))
                    .draw(display);
            }
        }

        // Recent earthquake epicenters that fall inside the visible image.
        if map_changed {
            let s_q = MonoTextStyleBuilder::new().font(&FONT_5X8)
                .text_color(Rgb565::WHITE).build();
            let newq = crate::quake::new_quake();
            for q in crate::quake::get().iter() {
                if let Some((qx, qy)) = to_px(q.lat as f32, q.lon as f32) {
                    let m = if q.mag > 0.0 { q.mag } else { 1.0 };
                    let diam = (4.0 + m * 2.0).clamp(6.0, 24.0) as u32;
                    // Colour by max seismic intensity (falls back to magnitude).
                    let col = if q.scale >= 55 { Rgb565::new(31, 0, 31) }       // 6- and up: magenta
                              else if q.scale >= 45 || m >= 5.0 { Rgb565::RED }  // 5- / 5+
                              else if q.scale >= 30 || m >= 3.0 { Rgb565::CSS_ORANGE }
                              else { Rgb565::YELLOW };
                    let _ = Circle::with_center(Point::new(qx, qy), diam)
                        .into_styled(PrimitiveStyleBuilder::new()
                            .stroke_color(col).stroke_width(2).build())
                        .draw(display);
                    // The newly received earthquake gets an extra white ring.
                    if newq.as_ref().map(|n| n.time == q.time && n.lat == q.lat && n.lon == q.lon).unwrap_or(false) {
                        let _ = Circle::with_center(Point::new(qx, qy), diam + 10)
                            .into_styled(PrimitiveStyle::with_stroke(Rgb565::WHITE, 1))
                            .draw(display);
                    }
                    // Label: magnitude and max seismic intensity, e.g. "M3.5 I3" / "M5.2 I5-"
                    let label = match (q.mag > 0.0, q.scale > 0) {
                        (true, true)  => format!("M{:.1} I{}", q.mag, q.intensity()),
                        (true, false) => format!("M{:.1}", q.mag),
                        (false, true) => format!("I{}", q.intensity()),
                        _ => String::new(),
                    };
                    if !label.is_empty() {
                        let _ = Text::new(&label,
                            Point::new(qx + diam as i32 / 2 + 2, qy + 3), s_q).draw(display);
                    }
                }
            }
            // Active EEW: forecast epicenter drawn as a large red X.
            if let Some(e) = crate::quake::eew() {
                if !e.lat.is_nan() {
                    if let Some((ex, ey)) = to_px(e.lat as f32, e.lon as f32) {
                        let xs = PrimitiveStyle::with_stroke(Rgb565::RED, 3);
                        let _ = Line::new(Point::new(ex - 9, ey - 9), Point::new(ex + 9, ey + 9)).into_styled(xs).draw(display);
                        let _ = Line::new(Point::new(ex - 9, ey + 9), Point::new(ex + 9, ey - 9)).into_styled(xs).draw(display);
                        let _ = Text::new(&format!("EEW I{}", e.intensity()),
                            Point::new(ex + 12, ey + 3), s_q).draw(display);
                        // Line from the epicenter to this unit
                        if own_valid {
                            if let Some((ox, oy)) = to_px(own_lat as f32, own_lon as f32) {
                                let _ = Line::new(Point::new(ex, ey), Point::new(ox, oy))
                                    .into_styled(PrimitiveStyle::with_stroke(Rgb565::new(31, 20, 0), 1))
                                    .draw(display);
                            }
                        }
                    }
                }
            }
        }

        // Crosshair at the current position (live GPS fix, or the point
        // being replayed) projected on the map. Drawn only on full redraws
        // to avoid ghost marks.
        if map_changed {
            // No GPS fix yet: draw a neutral centre placeholder. Otherwise,
            // only draw when the position actually falls on the map image
            // that's on screen right now — if a refetch is pending or has
            // failed (stale/mismatched tile, e.g. during fast track-playback
            // scrubbing or a flaky connection), to_px() returns None, and
            // falling back to a fixed screen-centre point would draw a
            // crosshair that doesn't correspond to the real position at all,
            // making the map and the marker look misaligned.
            let crosshair_px = if own_valid {
                to_px(own_lat as f32, own_lon as f32)
            } else if quake_focus.is_some() {
                None // map is centred elsewhere: a centre placeholder would mislead
            } else {
                Some((160, MAP_Y_TOP + (MAP_Y_BOT - MAP_Y_TOP) / 2))
            };
            if let Some((cx, cy)) = crosshair_px {
                let cs = PrimitiveStyle::with_stroke(Rgb565::RED, 2);
                let _ = Line::new(Point::new(cx - 8, cy), Point::new(cx + 8, cy))
                    .into_styled(cs).draw(display);
                let _ = Line::new(Point::new(cx, cy - 8), Point::new(cx, cy + 8))
                    .into_styled(cs).draw(display);
            }

            // ── Scale bar (bottom-right corner of map) ────────────────────
            // Web Mercator: metres-per-pixel at current centre latitude
            let lat_cos = if clat.is_nan() { 1.0f64 } else { clat.to_radians().cos() };
            let mpp = (2.0 * core::f64::consts::PI * 6_378_137.0 * lat_cos)
                      / (256.0 * (1u64 << zoom) as f64);

            // Nearest "nice" distance to a target of ~64 px
            const NICE_M: &[f64] = &[
                1.0, 2.0, 5.0, 10.0, 20.0, 50.0,
                100.0, 200.0, 500.0,
                1_000.0, 2_000.0, 5_000.0, 10_000.0,
                20_000.0, 50_000.0, 100_000.0, 200_000.0, 500_000.0,
            ];
            let target_m = mpp * 64.0;
            let nice_m = NICE_M.iter().copied().min_by(|&a, &b| {
                (a - target_m).abs().partial_cmp(&(b - target_m).abs())
                    .unwrap_or(core::cmp::Ordering::Equal)
            }).unwrap_or(1_000.0);
            let bar_px = ((nice_m / mpp).round() as i32).clamp(8, 240);

            // Human-readable label: "500 m" / "5 km"
            let label = if nice_m < 1_000.0 {
                format!("{} m", nice_m as u32)
            } else {
                format!("{} km", (nice_m / 1_000.0) as u32)
            };
            let label_px = label.len() as i32 * 5; // FONT_5X8 = 5 px wide

            // Bar positioned flush to right edge, bar line at y = MAP_Y_BOT-8
            let bar_y  = MAP_Y_BOT - 8;            // = 192
            let bar_x1 = (316 - bar_px).max(4);
            let bar_x2 = bar_x1 + bar_px;

            // Dark background box covering label + bar
            let bg_x = (bar_x1 - 2).max(0);
            let bg_w = (bar_x2 - bg_x + 2).max(label_px + 4) as u32;
            let _ = Rectangle::new(
                Point::new(bg_x, bar_y - 12),
                Size::new(bg_w, 17),
            ).into_styled(PrimitiveStyle::with_fill(Rgb565::new(2, 4, 2))).draw(display);

            // Bar line + end ticks
            let wh = PrimitiveStyle::with_stroke(Rgb565::WHITE, 1);
            let _ = Line::new(Point::new(bar_x1, bar_y), Point::new(bar_x2, bar_y))
                .into_styled(wh).draw(display);
            let _ = Line::new(Point::new(bar_x1, bar_y - 3), Point::new(bar_x1, bar_y + 2))
                .into_styled(wh).draw(display);
            let _ = Line::new(Point::new(bar_x2, bar_y - 3), Point::new(bar_x2, bar_y + 2))
                .into_styled(wh).draw(display);

            // Centred label above the bar
            let label_x = (bar_x1 + (bar_px - label_px) / 2).max(0);
            let s_sc = MonoTextStyleBuilder::new().font(&FONT_5X8)
                .text_color(Rgb565::WHITE).build();
            let _ = Text::new(&label, Point::new(label_x, bar_y - 4), s_sc).draw(display);

            // ── Sub-satellite ground points (zoom ≤ 5 only) ──────────────
            // For each tracked satellite, compute the point on Earth's surface
            // directly below it using azimuth/elevation and orbital altitude.
            // Skipped during track playback: today's sky doesn't match the
            // satellite geometry at the time the replayed point was logged.
            if !playing && zoom <= 5 && gps.fix_quality > 0 && !gps.lat_dd.is_nan() {
                let obs_lat = gps.lat_dd.to_radians();
                let obs_lon = gps.lon_dd.to_radians();
                let s_prn = MonoTextStyleBuilder::new().font(&FONT_5X8)
                    .text_color(Rgb565::WHITE).build();

                for sat in gps.sats.iter().filter(|s| s.snr > 0 && s.elevation >= 0) {
                    // Orbital altitude by constellation
                    let h_km: f64 = match sat.system {
                        'G' => 20_200.0,
                        'A' => 23_222.0,
                        'B' => 21_528.0,
                        'L' => 19_100.0,
                        _   => 20_200.0,
                    };
                    const R: f64 = 6_371.0;
                    let el = (sat.elevation as f64).to_radians();
                    let az = (sat.azimuth   as f64).to_radians();
                    // Nadir angle at Earth centre
                    let nadir   = (R * el.cos() / (R + h_km)).asin();
                    // Earth central angle (great-circle arc observer→sub-sat point)
                    let central = core::f64::consts::FRAC_PI_2 - el - nadir;
                    // Sub-satellite lat/lon via spherical trig
                    let sub_lat_r = (obs_lat.sin() * central.cos()
                                   + obs_lat.cos() * central.sin() * az.cos()).asin();
                    let sub_lon_r = obs_lon
                        + (az.sin() * central.sin() * obs_lat.cos())
                          .atan2(central.cos() - obs_lat.sin() * sub_lat_r.sin());

                    let color = match sat.system {
                        'G' => Rgb565::new(0,  50,  0),   // green  (GPS)
                        'A' => Rgb565::new(0,  20, 31),   // blue   (Galileo)
                        'B' => Rgb565::new(31, 30,  0),   // yellow (BeiDou)
                        'L' => Rgb565::new(20,  0, 20),   // magenta(GLONASS)
                        _   => Rgb565::new(15, 30, 15),
                    };

                    if let Some((px, py)) = to_px(
                        sub_lat_r.to_degrees() as f32,
                        sub_lon_r.to_degrees() as f32,
                    ) {
                        let fill = if sat.in_use {
                            PrimitiveStyle::with_fill(color)
                        } else {
                            PrimitiveStyle::with_stroke(color, 1)
                        };
                        let _ = Circle::with_center(Point::new(px, py), if sat.in_use { 4 } else { 3 })
                            .into_styled(fill).draw(display);
                        // PRN label (system prefix + number)
                        let label = format!("{}{}", sat.system, sat.prn);
                        let _ = Text::new(&label, Point::new(px + 5, py + 3), s_prn).draw(display);
                    }
                }
            }
        }
    }

    // ── Info bar (bottom 40 px) — refreshed on every update ──────────────────
    let _ = Rectangle::new(
        Point::new(0, INFO_Y),
        Size::new(320, (240 - INFO_Y) as u32),
    ).into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK)).draw(display);

    let s_lbl = MonoTextStyleBuilder::new().font(&FONT_6X12)
        .text_color(Rgb565::new(0, 50, 31)).build();
    let s_val = MonoTextStyleBuilder::new().font(&FONT_6X12)
        .text_color(Rgb565::YELLOW).build();

    let zoom = crate::map_display::get_zoom();
    // Info bar rows (40 px): line 1, line 2, then the button legend on its own row.
    const LEGEND_Y: i32 = INFO_Y + 35;

    if let Some(e) = crate::quake::eew() {
        // Active EEW: summary instead of the usual position line.
        let s_alert = MonoTextStyleBuilder::new().font(&FONT_6X12)
            .text_color(Rgb565::RED).build();
        let (_, _, _, dist) = crate::quake::eew_view(&e);
        let mag = if e.mag >= 0.0 { format!("M{:.1}", e.mag) } else { "M?".into() };
        let dep = if e.depth >= 0 { format!("{}km", e.depth) } else { "?km".into() };
        let d = dist.map(|d| format!("  {:.0}km away", d)).unwrap_or_default();
        let l1 = format!("EEW {} Imax{} dep{}{}", mag, e.intensity(), dep, d);
        let l2 = if e.lat.is_nan() { "Epicenter unknown".to_string() } else {
            format!("Epicenter {:.2}{} {:.2}{}",
                e.lat.abs(), if e.lat >= 0.0 { 'N' } else { 'S' },
                e.lon.abs(), if e.lon >= 0.0 { 'E' } else { 'W' }) };
        let _ = Text::new(&l1, Point::new(2, INFO_Y + 9), s_alert).draw(display);
        let _ = Text::new(&l2, Point::new(2, INFO_Y + 21), s_lbl).draw(display);
        draw_legend(display, 2, LEGEND_Y, &[Legend::Page("EXIT")]);
    } else if let Some(q) = crate::quake::new_quake() {
        // Newly received earthquake: its details instead of the position line.
        let s_new = MonoTextStyleBuilder::new().font(&FONT_6X12)
            .text_color(Rgb565::YELLOW).build();
        let (_, _, _, dist) = crate::quake::fit_view(q.lat, q.lon);
        let t = q.time.get(5..16).unwrap_or(&q.time);
        let mag = if q.mag >= 0.0 { format!("M{:.1}", q.mag) } else { "M?".into() };
        let dep = match q.depth { d if d < 0 => "?km".to_string(), 0 => "0km".to_string(), d => format!("{}km", d) };
        let d = dist.map(|d| format!(" {:.0}km away", d)).unwrap_or_default();
        let l1 = format!("{} {} I{} dep{}{}", t, mag, q.intensity(), dep, d);
        let l2 = format!("Epicenter {:.2}{} {:.2}{}",
            q.lat.abs(), if q.lat >= 0.0 { 'N' } else { 'S' },
            q.lon.abs(), if q.lon >= 0.0 { 'E' } else { 'W' });
        let _ = Text::new(&l1, Point::new(2, INFO_Y + 9), s_new).draw(display);
        let _ = Text::new(&l2, Point::new(2, INFO_Y + 21), s_lbl).draw(display);
        draw_legend(display, 2, LEGEND_Y, &[Legend::Page("EXIT")]);
    } else if let Some(pt) = track_pt {
        let (lat_s, lon_s) = (format_dm(pt.lat, 'N', 'S'), format_dm(pt.lon, 'E', 'W'));
        let lat_lon = format!("{} {}  Z:{}", lat_s, lon_s, zoom);
        let _ = Text::new(&lat_lon, Point::new(2, INFO_Y + 9), s_val).draw(display);

        let idx = crate::track_view::point_index();
        let total = crate::track_view::point_count();
        let name = crate::track_view::loaded_name();
        let time_s = pt.time_utc.get(11..19).unwrap_or("--:--:--");
        let info_text = format!("{} PT {}/{} {} {:.0}m {:.1}kn",
            name, idx + 1, total, time_s, pt.alt_m, pt.speed_kn);
        let _ = Text::new(&info_text,      Point::new(2,   INFO_Y + 21), s_lbl).draw(display);
        draw_legend(display, 2, LEGEND_Y, &[
            Legend::Page("EXIT"), Legend::Up, Legend::Down, Legend::Action("TYPE")]);
    } else {
        let lat_lon = format!("{} {}  Z:{}", gps.lat_str, gps.lon_str, zoom);
        let _ = Text::new(&lat_lon, Point::new(2, INFO_Y + 9), s_val).draw(display);

        let loc = crate::gps_parser::get_location_name();
        let loc_text = if loc.is_empty() {
            if busy { "Fetching...".into() } else { "---".into() }
        } else { loc };
        let _ = Text::new(&loc_text,       Point::new(2,   INFO_Y + 21), s_lbl).draw(display);
        draw_legend(display, 2, LEGEND_Y, &[
            Legend::Page("PAGE"), Legend::Up, Legend::Down, Legend::Action("TYPE")]);
    }
}

/// Format decimal degrees as "DDd MM.MMMM H" (matches the live NMEA display
/// style), e.g. `format_dm(35.7429, 'N', 'S')` → "35d44.5738N".
pub(crate) fn format_dm(dd: f64, pos_hemi: char, neg_hemi: char) -> String {
    let hemi = if dd < 0.0 { neg_hemi } else { pos_hemi };
    let abs = dd.abs();
    let deg = abs.floor() as i64;
    let min = (abs - deg as f64) * 60.0;
    format!("{}d{:.4}{}", deg, min, hemi)
}

// ─── Track-list page ─────────────────────────────────────────────────────
//
// Lists saved track log dates. ZoomUp/Down (GPIO4/5) move the highlight,
// the MAP-mode button (GPIO6) loads the highlighted track and switches to
// PAGE_MAP to replay it — see btn_ctrl.rs.

pub(crate) fn draw_tracklist_page<D: DrawTarget<Color = Rgb565>>(
    display: &mut D,
    force: bool,
    prev_lines: &mut Vec<String>,
) {
    let files = crate::track_view::file_list();
    let sel = crate::track_view::list_selection();

    let hdr_style = MonoTextStyleBuilder::new().font(&FONT_6X12)
        .text_color(Rgb565::CSS_LIGHT_GREEN).build();
    let sel_style = MonoTextStyleBuilder::new().font(&FONT_6X12)
        .text_color(Rgb565::BLACK).background_color(Rgb565::YELLOW).build();
    let item_style = MonoTextStyleBuilder::new().font(&FONT_6X12)
        .text_color(Rgb565::WHITE).build();

    // Reserve the header row + a trailing hint row; the rest lists files.
    let max_rows = TERM_ROWS.saturating_sub(2);

    let mut rows: Vec<(String, bool)> = Vec::new();
    rows.push((format!("Saved Tracks: {} file(s)", files.len()), false));
    if files.is_empty() {
        rows.push(("(no track logs found)".into(), false));
    } else {
        for (i, name) in files.iter().enumerate().take(max_rows.saturating_sub(1)) {
            rows.push((name.clone(), i == sel));
        }
    }

    let text_strs: Vec<String> = rows.iter()
        .map(|(t, is_sel)| format!("{}{}", if *is_sel { '>' } else { ' ' }, t))
        .collect();

    for (i, line) in text_strs.iter().enumerate() {
        if i >= TERM_ROWS { break; }
        let changed = force || i >= prev_lines.len() || prev_lines[i] != *line;
        if !changed { continue; }

        let y = HEADER_H + 1 + (i as i32) * CHAR_H;
        let _ = Rectangle::new(Point::new(0, y), Size::new(320, CHAR_H as u32))
            .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK)).draw(display);

        let style = if i == 0 { hdr_style } else if rows[i].1 { sel_style } else { item_style };
        let _ = Text::new(line, Point::new(2, y + 10), style).draw(display);
    }
    *prev_lines = text_strs;

    // Hint line always drawn on force (rarely changes otherwise).
    if force {
        let y = HEADER_H + 1 + (TERM_ROWS as i32 - 1) * CHAR_H;
        let _ = Rectangle::new(Point::new(0, y), Size::new(320, CHAR_H as u32))
            .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK)).draw(display);
        draw_legend(display, 2, y + 10, &[
            Legend::Page("PAGE"), Legend::Up, Legend::Down, Legend::Action("PLAY")]);
    }
}

// ─── Info page ───────────────────────────────────────────────────────────

pub(crate) fn draw_info_page<D: DrawTarget<Color = Rgb565>>(
    display: &mut D,
    force: bool,
    prev_lines: &mut Vec<String>,
) {
    // Color styles
    let section_style = MonoTextStyleBuilder::new()
        .font(&FONT_6X12)
        .text_color(Rgb565::CSS_LIGHT_GREEN)
        .build();
    let label_style = MonoTextStyleBuilder::new()
        .font(&FONT_6X12)
        .text_color(Rgb565::new(0, 50, 31))  // cyan
        .build();
    let val_style = MonoTextStyleBuilder::new()
        .font(&FONT_6X12)
        .text_color(Rgb565::WHITE)
        .build();
    let val_yellow = MonoTextStyleBuilder::new()
        .font(&FONT_6X12)
        .text_color(Rgb565::YELLOW)
        .build();
    let val_on = MonoTextStyleBuilder::new()
        .font(&FONT_6X12)
        .text_color(Rgb565::GREEN)
        .build();
    let val_off = MonoTextStyleBuilder::new()
        .font(&FONT_6X12)
        .text_color(Rgb565::new(21, 0, 0))  // dim red
        .build();
    let val_disabled = MonoTextStyleBuilder::new()
        .font(&FONT_6X12)
        .text_color(Rgb565::new(10, 21, 10))  // gray
        .build();

    // Collect data
    let (ip, ssid, rssi, battery, chip_temp) = {
        let guard = DISPLAY_AUX.lock().unwrap();
        if let Some(ref aux) = *guard {
            let ip = aux.status.ip_address.lock().unwrap().clone();
            let ssid = aux.status.ssid.lock().unwrap().clone();
            let rssi = *aux.status.rssi.lock().unwrap();
            let battery = *aux.status.battery_voltage.lock().unwrap();
            let chip_temp = *aux.status.chip_temp.lock().unwrap();
            (ip, ssid, rssi, battery, chip_temp)
        } else {
            ("--".into(), "--".into(), 0i32, 0.0f32, 0.0f32)
        }
    };

    let active = crate::usb_host::active_device_name();

    let (c1b, c1t, c1r) = {
        let guard = DISPLAY_AUX.lock().unwrap();
        if let Some(ref aux) = *guard {
            (aux.com1_baud.clone(), aux.com1_tx.clone(), aux.com1_rx.clone())
        } else {
            ("--".into(), "--".into(), "--".into())
        }
    };


    // Build colored segments per line: Vec<Vec<(&str or String, style)>>
    // We use a simple serialization: join all text into one string per line
    // for change-detection, then draw each segment separately.

    // Each line: Vec<(text, style_index)>
    // style_index: 0=section, 1=label, 2=val, 3=yellow, 4=on, 5=off, 6=disabled
    type Seg = (String, u8);
    let mut rows: Vec<Vec<Seg>> = Vec::new();

    // ── WiFi / Network ───────────────────
    rows.push(vec![("-- Network --".into(), 0)]);
    rows.push(vec![("IP: ".into(), 1), (ip.clone(), 3)]);
    rows.push(vec![
        ("WiFi: ".into(), 1), (ssid.clone(), 2),
        ("  RSSI: ".into(), 1), (format!("{}dBm", rssi), if rssi > -60 { 4 } else if rssi > -80 { 3 } else { 5 }),
    ]);
    rows.push(vec![
        ("Battery: ".into(), 1), (format!("{:.2}V", battery), 3),
    ]);
    rows.push(vec![
        ("ChipTemp: ".into(), 1),
        (format!("{:.1}C", chip_temp),
         if chip_temp < 70.0 { 4 } else if chip_temp < 85.0 { 3 } else { 5 }),
    ]);

    // ── Memory ───────────────────────────
    let mem = crate::httpserver::mem_info();
    rows.push(vec![("-- Memory --".into(), 0)]);
    rows.push(vec![
        ("Int:   ".into(), 1), (format!("{}KB", mem.int_free / 1024), 4),
        (" min ".into(), 1), (format!("{}KB", mem.int_min / 1024), 2),
    ]);
    rows.push(vec![
        ("PSRAM: ".into(), 1), (format!("{}KB", mem.psram_free / 1024), 4),
        (" min ".into(), 1), (format!("{}KB", mem.psram_min / 1024), 2),
    ]);

    // ── Device ───────────────────────────
    rows.push(vec![("-- Device --".into(), 0)]);
    rows.push(vec![("Active: ".into(), 1), (active.to_string(), 3)]);

    // ── Serial Ports ─────────────────────
    rows.push(vec![("-- Serial --".into(), 0)]);
    rows.push(vec![
        ("COM1 ".into(), 1), (format!("{}bps", c1b), 2),
        (" TX:".into(), 1), (c1t.clone(), 3),
        (" RX:".into(), 1), (c1r.clone(), 3),
    ]);

    // ── Firmware ─────────────────────────
    rows.push(vec![("-- Firmware --".into(), 0)]);
    rows.push(vec![("Ver:   ".into(), 1), (crate::httpserver::FW_VERSION.to_string(), 3)]);
    rows.push(vec![("Build: ".into(), 1), (crate::httpserver::FW_BUILD.to_string(), 2)]);

    // ── Change detection for text rows ───────────────────────────────────────
    let text_strs: Vec<String> = rows.iter().map(|segs| {
        segs.iter().map(|(t, _)| t.as_str()).collect::<String>()
    }).collect();

    for (i, segs) in rows.iter().enumerate() {
        if i >= TERM_ROWS { break; }
        let changed = force || i >= prev_lines.len() || prev_lines[i] != text_strs[i];
        if !changed { continue; }

        let y = HEADER_H + 1 + (i as i32) * CHAR_H;

        // Clear only x=0..215; right side (x>=215) is reserved for the legend.
        let _ = Rectangle::new(Point::new(0, y), Size::new(215, CHAR_H as u32))
            .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
            .draw(display);

        if segs.len() == 1 && segs[0].1 == 0 {
            let _ = Rectangle::new(Point::new(0, y), Size::new(320, 1))
                .into_styled(PrimitiveStyle::with_fill(Rgb565::new(0, 15, 0)))
                .draw(display);
        }

        let mut x = 2;
        for (text, sty) in segs {
            let style = match sty {
                0 => section_style,
                1 => label_style,
                3 => val_yellow,
                4 => val_on,
                5 => val_off,
                6 => val_disabled,
                _ => val_style,
            };
            let _ = Text::new(text, Point::new(x, y + 10), style).draw(display);
            x += text.len() as i32 * CHAR_W;
        }
    }
    *prev_lines = text_strs;
}

// ─── Earthquake page ─────────────────────────────────────────────────────
//
// Recent earthquakes from the P2P地震情報 API (see quake.rs). Two lines per
// event (time/M/intensity/depth, then epicenter coordinates); the font is
// ASCII-only, so the Japanese hypocenter name is shown on the Web UI only.
// ZoomUp/Down move the highlight, the MAP-mode button (GPIO6) centres the
// MAP page on the highlighted epicenter — see btn_ctrl.rs.

pub(crate) fn draw_quake_page<D: DrawTarget<Color = Rgb565>>(
    display: &mut D,
    force: bool,
    prev_lines: &mut Vec<String>,
) {
    let quakes = crate::quake::get();
    let sel = crate::quake::selection();

    let st = |c: Rgb565| MonoTextStyleBuilder::new().font(&FONT_6X12).text_color(c).build();
    let hdr_style = st(Rgb565::CSS_LIGHT_GREEN);
    let dim_style = st(Rgb565::new(10, 21, 10));

    // (text, row style: 0=header 1=dim 2=normal 3=orange 4=red, selected)
    let mut rows: Vec<(String, u8, bool)> = Vec::new();
    rows.push((format!("Recent quakes: {} (JMA via P2P)", quakes.len()), 0, false));
    if quakes.is_empty() {
        let msg = if !crate::quake::enabled() { "(disabled: set quake_enable in cfg.toml)" }
                  else if crate::quake::last_ok_secs() == 0 { "(waiting for data...)" } else { "(no events)" };
        rows.push((msg.into(), 1, false));
    }
    for (i, q) in quakes.iter().enumerate() {
        let sev = if q.scale >= 45 || q.mag >= 6.0 { 4 }
                  else if q.scale >= 30 || q.mag >= 4.5 { 3 } else { 2 };
        let t = q.time.get(5..16).unwrap_or(&q.time); // "10/10 10:20"
        let mag = if q.mag >= 0.0 { format!("M{:.1}", q.mag) } else { "M?".into() };
        let dep = match q.depth { d if d < 0 => "?km".to_string(), 0 => "0km".to_string(), d => format!("{}km", d) };
        let tsu = if q.tsunami == "Watch" || q.tsunami == "Warning" { " TSUNAMI" } else { "" };
        rows.push((format!("{} {} I{} {}{}", t, mag, q.intensity(), dep, tsu), sev, i == sel));
        rows.push((format!("   {:.2}{} {:.2}{}",
            q.lat.abs(), if q.lat >= 0.0 { 'N' } else { 'S' },
            q.lon.abs(), if q.lon >= 0.0 { 'E' } else { 'W' }), 1, i == sel));
    }

    let text_strs: Vec<String> = rows.iter()
        .map(|(t, _, s)| format!("{}{}", if *s { '>' } else { ' ' }, t))
        .collect();

    for (i, line) in text_strs.iter().enumerate() {
        if i >= TERM_ROWS - 1 { break; }
        let changed = force || i >= prev_lines.len() || prev_lines[i] != *line;
        if !changed { continue; }
        let y = HEADER_H + 1 + (i as i32) * CHAR_H;
        let _ = Rectangle::new(Point::new(0, y), Size::new(320, CHAR_H as u32))
            .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK)).draw(display);
        let style = match rows[i].1 {
            0 => hdr_style,
            1 => dim_style,
            3 => st(Rgb565::CSS_ORANGE),
            4 => st(Rgb565::RED),
            _ => st(Rgb565::WHITE),
        };
        let _ = Text::new(line, Point::new(2, y + 10), style).draw(display);
    }
    // Clear rows left over from a longer previous list.
    for i in text_strs.len()..prev_lines.len().min(TERM_ROWS - 1) {
        let y = HEADER_H + 1 + (i as i32) * CHAR_H;
        let _ = Rectangle::new(Point::new(0, y), Size::new(320, CHAR_H as u32))
            .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK)).draw(display);
    }
    *prev_lines = text_strs;

    if force {
        let y = HEADER_H + 1 + (TERM_ROWS as i32 - 1) * CHAR_H;
        let _ = Rectangle::new(Point::new(0, y), Size::new(320, CHAR_H as u32))
            .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK)).draw(display);
        draw_legend(display, 2, y + 10, &[
            Legend::Page("PAGE"), Legend::Up, Legend::Down, Legend::Action("MAP")]);
    }
}

// ─── GPS page ────────────────────────────────────────────────────────────────
//
// Top 9 text rows: GPS fix info + satellite count + lat/lon/location.
// Bottom: satellite SNR bar chart (up to 20 sats).

pub(crate) fn draw_gps_page<D: DrawTarget<Color = Rgb565>>(
    display: &mut D,
    force: bool,
    prev_lines: &mut Vec<String>,
    prev_chart_sig: &mut u32,
) {
    let gps = crate::gps_parser::get_data();

    // ── Constellation colour helper ────────────────────────────────────────────────
    // Defined early – used for legend and bar chart.
    // G=GPS(USA)  A=Galileo(EU)  B=BeiDou(CN)  L=GLONASS(RU)  Q=QZSS(JPN)
    let sys_color = |system: char, in_use: bool| -> Rgb565 {
        match (system, in_use) {
            ('G', true)  => Rgb565::new(0,  40,  0),
            ('G', false) => Rgb565::new(0,  10,  0),
            ('A', true)  => Rgb565::new(0,  22, 22),
            ('A', false) => Rgb565::new(0,   7,  7),
            ('B', true)  => Rgb565::new(28, 35,  0),
            ('B', false) => Rgb565::new(9,  11,  0),
            ('L', true)  => Rgb565::new(28,  0, 17),
            ('L', false) => Rgb565::new(9,   0,  5),
            ('Q', true)  => Rgb565::new(20,  0, 20),  // magenta (QZSS)
            ('Q', false) => Rgb565::new(6,   0,  6),
            (_,   true)  => Rgb565::new(20, 20, 20),
            _            => Rgb565::new(5,   5,  5),
        }
    };

    // ── Text styles ──────────────────────────────────────────────────────────────────
    let section_style = MonoTextStyleBuilder::new()
        .font(&FONT_6X12).text_color(Rgb565::CSS_LIGHT_GREEN).build();
    let label_style = MonoTextStyleBuilder::new()
        .font(&FONT_6X12).text_color(Rgb565::new(0, 50, 31)).build();
    let val_yellow = MonoTextStyleBuilder::new()
        .font(&FONT_6X12).text_color(Rgb565::YELLOW).build();
    let val_on = MonoTextStyleBuilder::new()
        .font(&FONT_6X12).text_color(Rgb565::GREEN).build();
    let val_off = MonoTextStyleBuilder::new()
        .font(&FONT_6X12).text_color(Rgb565::new(21, 0, 0)).build();

    // ── Build rows ──────────────────────────────────────────────────────────────────────
    type Seg = (String, u8);
    let mut rows: Vec<Vec<Seg>> = Vec::new();

    let fix_str = match gps.fix_quality {
        0 => "No fix",
        1 => "GNSS fix",
        2 => "DGNSS fix",
        _ => "Fix",
    };
    let sats_used: Vec<_> = gps.sats.iter().filter(|s| s.in_use).collect();
    rows.push(vec![
        ("Fix: ".into(), 1),
        (fix_str.into(), if gps.fix_quality > 0 { 4 } else { 5 }),
        ("  Sats: ".into(), 1),
        (format!("{}/{}", sats_used.len(), gps.sats.len()), 3),
    ]);
    rows.push(vec![
        ("Lat: ".into(), 1), (gps.lat_str.clone(), 3),
    ]);
    rows.push(vec![
        ("Lon: ".into(), 1), (gps.lon_str.clone(), 3),
    ]);
    let loc = crate::gps_parser::get_location_name();
    let loc_display: String = if loc.is_empty() { "--".into() }
                              else { loc.chars().take(30).collect() };
    rows.push(vec![
        ("Loc: ".into(), 1), (loc_display, 3),
    ]);
    let alt_str = format!("{:.1}m", gps.altitude);
    let spd_str = format!("{:.1}kn", gps.speed_kn);
    rows.push(vec![
        ("Alt: ".into(), 1), (alt_str, 3),
        ("  Spd: ".into(), 1), (spd_str, 3),
    ]);
    rows.push(vec![
        ("Time: ".into(), 1), (gps.time_str.clone(), 3),
        ("  Date: ".into(), 1), (gps.date_str.clone(), 3),
    ]);
    let hdop_str = format!("{:.1}", gps.hdop);
    let mode_str = match gps.fix_mode {
        2 => "2D".into(),
        3 => "3D".into(),
        _ => "--".into(),
    };
    rows.push(vec![
        ("HDOP: ".into(), 1), (hdop_str, 3),
        ("  Mode: ".into(), 1), (mode_str, 3),
    ]);

    // Change detection for text rows
    let text_strs: Vec<String> = rows.iter().map(|segs| {
        segs.iter().map(|(t, _)| t.as_str()).collect::<String>()
    }).collect();

    for (i, segs) in rows.iter().enumerate() {
        if i >= TERM_ROWS { break; }
        let changed = force || i >= prev_lines.len() || prev_lines[i] != text_strs[i];
        if !changed { continue; }

        let y = HEADER_H + 1 + (i as i32) * CHAR_H;

        // Clear only x=0..214 to preserve the legend area (x>=215).
        let _ = Rectangle::new(Point::new(0, y), Size::new(215, CHAR_H as u32))
            .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
            .draw(display);

        let mut x = 2;
        for (text, sty) in segs {
            let style = match sty {
                1 => label_style,
                3 => val_yellow,
                4 => val_on,
                5 => val_off,
                _ => section_style,
            };
            let _ = Text::new(text, Point::new(x, y + 10), style).draw(display);
            x += text.len() as i32 * CHAR_W;
        }
    }
    *prev_lines = text_strs;

    // ── Legend (upper-right, x>=215) ───────────────────────────────────────────────
    // Redrawn every frame (not gated on sig) so it survives any text-row clear.
    // Background re-cleared only on force (page switch / init).
    const LEG_X: i32 = 216;
    const LEG_Y: i32 = HEADER_H + 3;  // 23 px from top
    const LEG_PITCH: i32 = 11;        // row pitch in pixels
    let leg_entries: [(&str, Rgb565, &str); 5] = [
        ("G", sys_color('G', true), "GPS /USA"),
        ("E", sys_color('A', true), "GAL /EU "),
        ("C", sys_color('B', true), "BDS /CHN"),
        ("R", sys_color('L', true), "GLO /RUS"),
        ("J", sys_color('Q', true), "QZS /JPN"),
    ];
    if force {
        let _ = Rectangle::new(
            Point::new(LEG_X - 1, LEG_Y - 1),
            Size::new(104, (5 * LEG_PITCH + 2) as u32),
        )
        .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
        .draw(display);
    }
    let leg_black = MonoTextStyleBuilder::new()
        .font(&FONT_5X8).text_color(Rgb565::BLACK).build();
    for (i, (letter, color, name)) in leg_entries.iter().enumerate() {
        let ly = LEG_Y + i as i32 * LEG_PITCH;
        let _ = Rectangle::new(Point::new(LEG_X, ly), Size::new(8, 8))
            .into_styled(PrimitiveStyle::with_fill(*color))
            .draw(display);
        let _ = Text::new(letter, Point::new(LEG_X + 2, ly + 7), leg_black).draw(display);
        let leg_style = MonoTextStyleBuilder::new()
            .font(&FONT_5X8).text_color(*color).build();
        let _ = Text::new(name, Point::new(LEG_X + 10, ly + 7), leg_style).draw(display);
    }

    // ── Chart change detection ────────────────────────────────────────────
    let sig: u32 = gps.sats.iter().fold(
        (gps.fix_quality as u32).wrapping_mul(0x9e37),
        |acc, s| acc.wrapping_add((s.prn as u32).wrapping_mul(31)
                                  .wrapping_add(s.snr as u32)
                                  .wrapping_add(if s.in_use { 0x100 } else { 0 })),
    );

    if !force && sig == *prev_chart_sig {
        return;
    }
    *prev_chart_sig = sig;

    const CHART_Y_TOP: i32 = HEADER_H + 1 + 9 * CHAR_H; // 129 (9 text rows)
    const CHART_Y_BOT: i32 = 240;
    const LABEL_H: i32 = CHAR_H;                          // 12 px for PRN label
    const BAR_AREA_H: i32 = CHART_Y_BOT - CHART_Y_TOP - LABEL_H; // 111 px
    const BAR_SLOT_W: i32 = 16;  // px per satellite slot
    const BAR_W: i32 = 12;       // actual bar width (centred in slot)
    const BAR_X_OFF: i32 = 2;    // left offset within slot
    const SNR_MAX: u32 = 50;     // dBHz reference for 100% bar height

    // Clear chart area (y=129..240)
    let _ = Rectangle::new(
        Point::new(0, CHART_Y_TOP),
        Size::new(320, (CHART_Y_BOT - CHART_Y_TOP) as u32),
    )
    .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
    .draw(display);

    for (idx, sat) in gps.sats.iter().take(20).enumerate() {
        let x = idx as i32 * BAR_SLOT_W;

        let bar_color = sys_color(sat.system, sat.in_use);

        // Draw SNR bar
        let snr_u = sat.snr as u32;
        let bar_h = ((snr_u * BAR_AREA_H as u32) / SNR_MAX)
            .min(BAR_AREA_H as u32) as i32;
        if bar_h > 0 {
            let bar_y = CHART_Y_BOT - LABEL_H - bar_h;
            let _ = Rectangle::new(
                Point::new(x + BAR_X_OFF, bar_y),
                Size::new(BAR_W as u32, bar_h as u32),
            )
            .into_styled(PrimitiveStyle::with_fill(bar_color))
            .draw(display);
        }

        // ── PRN label: constellation-colour background, inverted text ───────────────────
        // in-use  → bright background + BLACK PRN number
        // visible → dim   background + dark-grey PRN number
        let label_fg = if sat.in_use { Rgb565::BLACK } else { Rgb565::new(14, 26, 14) };
        let _ = Rectangle::new(
            Point::new(x, CHART_Y_BOT - LABEL_H),
            Size::new((BAR_SLOT_W - 1) as u32, LABEL_H as u32),
        )
        .into_styled(PrimitiveStyle::with_fill(bar_color))
        .draw(display);
        // 2-digit PRN with FONT_6X12 (12px wide in 15px slot, centred at x+1)
        let s_prn = MonoTextStyleBuilder::new()
            .font(&FONT_6X12).text_color(label_fg).build();
        let _ = Text::new(&format!("{:02}", sat.prn),
                          Point::new(x + 1, CHART_Y_BOT - 2), s_prn).draw(display);
    }

}
// ─── Terminal page ────────────────────────────────────────────────────────────

pub(crate) fn draw_terminal_page<D: DrawTarget<Color = Rgb565>>(
    display: &mut D,
    rx_buf: &SerialRxBuffer,
    prev_cells: &mut [[Cell; TERM_COLS]; TERM_ROWS],
    force: bool,
) {
    let (should_redraw, snapshot) = {
        let mut term = rx_buf.inner.lock().unwrap();
        let dirty = term.dirty;
        if dirty { term.dirty = false; }
        (dirty || force, term.cells)
    };

    if !should_redraw {
        return;
    }

    let mut char_buf = [0u8; 4];

    for row in 0..TERM_ROWS {
        for col in 0..TERM_COLS {
            let cell = snapshot[row][col];
            let prev = prev_cells[row][col];
            if !force
                && cell.ch == prev.ch
                && cell.fg == prev.fg
                && cell.bg == prev.bg
            {
                continue;
            }

            let px = col as i32 * CHAR_W;
            let py = HEADER_H + 1 + row as i32 * CHAR_H;

            let _ = Rectangle::new(
                Point::new(px, py),
                Size::new(CHAR_W as u32, CHAR_H as u32),
            )
            .into_styled(PrimitiveStyle::with_fill(cell.bg))
            .draw(display);

            if cell.ch != ' ' {
                let s = cell.ch.encode_utf8(&mut char_buf);
                let style = MonoTextStyleBuilder::new()
                    .font(&FONT_6X12)
                    .text_color(cell.fg)
                    .build();
                let _ = Text::new(s, Point::new(px, py + 10), style).draw(display);
            }

            prev_cells[row][col] = cell;
        }
    }
}

// ─── Sky-view page ────────────────────────────────────────────────────────────
//
// Polar plot of satellite positions.
// Layout (320×240, 20 px header → usable y 21..240 = 219 px):
//   Left:  polar circle, diameter 210 px, centre (110, 130)
//          North=up, East=right; elevation rings at 0°/30°/60°
//   Right: x 222..320 — constellation legend + fix status
//
// Satellite dot size: 6×6 px; colour by constellation (bright=in-use, dim=visible).
// PRN label (2 chars) drawn to the right of each dot when space allows.
//
// Simple integer-only trig: sin/cos look-up via fixed-point approximation.

/// Integer sin for angle in degrees × 1000 (returns value × 1000).
pub(crate) fn isin(deg: i32) -> i32 {
    // Use a 91-entry quarter-wave lookup table
    const T: [i32; 91] = [
        0,17,35,52,70,87,105,122,139,156,174,191,208,225,242,259,276,292,309,326,
        342,358,374,391,407,423,438,454,469,485,500,515,530,545,559,574,588,602,616,629,
        643,656,669,682,695,707,719,731,743,755,766,777,788,799,809,819,829,839,848,857,
        866,875,883,891,899,906,914,921,927,934,940,946,951,956,961,966,970,974,978,982,
        985,988,990,993,995,996,998,999,999,1000,1000,
    ];
    let d = ((deg % 360) + 360) % 360;
    let (q, r) = (d / 90, d % 90);
    let v = T[r as usize];
    match q {
        0 =>  v,
        1 =>  T[90 - r as usize],
        2 => -v,
        _ => -T[90 - r as usize],
    }
}

#[inline]
pub(crate) fn icos(deg: i32) -> i32 { isin(deg + 90) }

/// Convert (azimuth°, elevation°) → pixel (x, y) on the polar plot.
/// cx, cy = centre; r_max = radius for elevation 0°.
pub(crate) fn sky_xy(az: u16, el: i16, cx: i32, cy: i32, r_max: i32) -> (i32, i32) {
    let el_clamped = el.max(0).min(90) as i32;
    // distance from centre: full radius at el=0, zero at el=90
    let r = r_max * (90 - el_clamped) / 90;
    let az_i = az as i32;
    // azimuth 0=North(up), clockwise → screen: x=cx+r*sin(az), y=cy-r*cos(az)
    let px = cx + r * isin(az_i) / 1000;
    let py = cy - r * icos(az_i) / 1000;
    (px, py)
}

pub(crate) fn draw_skyview_page<D: DrawTarget<Color = Rgb565>>(
    display: &mut D,
    force: bool,
    prev_sig: &mut u32,
    prev_sats: &mut Vec<(i32, i32)>,
    prev_panel: &mut Vec<String>,
) {
    let gps = crate::gps_parser::get_data();

    // ── Change signature ──────────────────────────────────────────────
    let sig: u32 = gps.sats.iter().fold(0u32, |acc, s| {
        acc.wrapping_add(
            (s.prn as u32).wrapping_mul(31)
                .wrapping_add((s.azimuth as u32) << 8)
                .wrapping_add(s.elevation as u32)
                .wrapping_add((s.in_use as u32) << 24)
                .wrapping_add((s.system as u32) << 16),
        )
    }).wrapping_add(gps.fix_mode as u32);

    if !force && sig == *prev_sig {
        return;
    }
    *prev_sig = sig;

    // ── Geometry constants ────────────────────────────────────────────
    const CX: i32 = 110;
    const CY: i32 = 130;
    const R:  i32 = 105;
    const DOT_DIAM: u32 = 8;  // dot diameter (radius 4)

    // ── Static background (rings + labels) — only on force ────────────
    if force {
        let _ = Rectangle::new(
            Point::new(0, HEADER_H + 1),
            Size::new(320, (240 - HEADER_H - 1) as u32),
        ).into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK)).draw(display);

        skyview_draw_bg(display, CX, CY, R);

        // Right-panel header and static key
        const PANEL_X: i32 = 222;
        let s_hdr = MonoTextStyleBuilder::new()
            .font(&FONT_6X12).text_color(Rgb565::CSS_LIGHT_GREEN).build();
        let _ = Text::new("STATUS", Point::new(PANEL_X, HEADER_H + 12), s_hdr).draw(display);

        // key_y_start: after 11 status lines (Fix…UTC) + 4 px gap
        let key_y_start = HEADER_H + 24 + 11 * 13 + 4;
        let key_items: &[(Rgb565, &str)] = &[
            (Rgb565::new(0, 60, 0),   "GP"),
            (Rgb565::new(0, 40, 31),  "GA"),
            (Rgb565::new(31, 50, 0),  "GB"),
            (Rgb565::new(31, 0, 25),  "GL"),
            (Rgb565::new(31, 0, 31),  "QZ"),
        ];
        for (i, (col, name)) in key_items.iter().enumerate() {
            let ky = key_y_start + i as i32 * 11;  // 11 px spacing to fit 5 items
            let _ = Circle::with_center(Point::new(PANEL_X + 3, ky - 3), 6)
                .into_styled(PrimitiveStyle::with_fill(*col))
                .draw(display);
            let s_k = MonoTextStyleBuilder::new()
                .font(&FONT_6X12).text_color(*col).build();
            let _ = Text::new(name, Point::new(PANEL_X + 10, ky), s_k).draw(display);
        }
    }

    // ── Erase old satellite dots individually ─────────────────────────
    for &(px, py) in prev_sats.iter() {
        // Erase dot + label area with a small black rectangle
        let ex = (px - 14).max(0);
        let ey = (py - 10).max(HEADER_H + 1);
        let ew = ((px + 26).min(220) - ex).max(0) as u32;
        let eh = ((py + 14).min(240) - ey).max(0) as u32;
        if ew > 0 && eh > 0 {
            let _ = Rectangle::new(Point::new(ex, ey), Size::new(ew, eh))
                .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
                .draw(display);
        }
    }

    // Redraw rings after erasing (restores any ring pixels that were erased)
    if !prev_sats.is_empty() || force {
        skyview_draw_rings(display, CX, CY, R);
    }

    // ── Draw new satellite dots ───────────────────────────────────────
    let mut new_sats: Vec<(i32, i32)> = Vec::new();

    for sat in &gps.sats {
        let (px, py) = sky_xy(sat.azimuth, sat.elevation, CX, CY, R);

        let (fill_col, outline_col) = if sat.in_use {
            let fc = match sat.system {
                'G' => Rgb565::new(0, 60, 0),
                'A' => Rgb565::new(0, 40, 31),
                'B' => Rgb565::new(31, 50, 0),
                'L' => Rgb565::new(31, 0, 25),
                _   => Rgb565::new(30, 30, 30),
            };
            (fc, Rgb565::WHITE)
        } else {
            let fc = match sat.system {
                'G' => Rgb565::new(0, 18, 0),
                'A' => Rgb565::new(0, 12, 10),
                'B' => Rgb565::new(10, 15, 0),
                'L' => Rgb565::new(10, 0, 8),
                _   => Rgb565::new(8, 8, 8),
            };
            (fc, Rgb565::new(8, 16, 8))
        };

        let _ = Circle::with_center(Point::new(px, py), DOT_DIAM)
            .into_styled(PrimitiveStyle::with_fill(fill_col))
            .draw(display);
        let _ = Circle::with_center(Point::new(px, py), DOT_DIAM)
            .into_styled(PrimitiveStyleBuilder::new()
                .stroke_color(outline_col).stroke_width(1).build())
            .draw(display);

        let lx = if px + 18 < CX + R + 10 { px + 5 } else { px - 12 };
        let ly = if py - 6 > HEADER_H + 1 { py - 2 } else { py + 10 };
        let prn_col = if sat.in_use { Rgb565::WHITE } else { Rgb565::new(10, 20, 10) };
        let prn_style = MonoTextStyleBuilder::new()
            .font(&FONT_6X12).text_color(prn_col).build();
        let _ = Text::new(&format!("{:02}", sat.prn), Point::new(lx, ly), prn_style).draw(display);

        new_sats.push((px, py));
    }
    *prev_sats = new_sats;

    // ── Right panel: differential value update ────────────────────────
    const PANEL_X: i32 = 222;

    let fix_str = match gps.fix_mode {
        3 => "3D Fix", 2 => "2D Fix", _ => "No Fix",
    };
    let fix_col = match gps.fix_mode {
        3 => Rgb565::GREEN, 2 => Rgb565::YELLOW, _ => Rgb565::new(21, 0, 0),
    };

    let n_used  = gps.sats.iter().filter(|s| s.in_use).count();
    let n_total = gps.sats.len();
    let n_gps   = gps.sats.iter().filter(|s| s.system == 'G').count();
    let n_gal   = gps.sats.iter().filter(|s| s.system == 'A').count();
    let n_bds   = gps.sats.iter().filter(|s| s.system == 'B').count();
    let n_glo   = gps.sats.iter().filter(|s| s.system == 'L').count();
    let n_qzs   = gps.sats.iter().filter(|s| s.system == 'Q').count();

    let sat_str  = format!("{}/{}", n_used, n_total);
    let gps_str  = format!("{}", n_gps);
    let gal_str  = format!("{}", n_gal);
    let bds_str  = format!("{}", n_bds);
    let glo_str  = format!("{}", n_glo);
    let qzs_str  = format!("{}", n_qzs);
    let hdop_str = format!("{:.1}", gps.hdop);
    let vdop_str = format!("{:.1}", gps.vdop);
    let alt_str  = format!("{:.0}m", gps.altitude);

    let lines: &[(&str, &str, Rgb565)] = &[
        ("Fix",  fix_str,        fix_col),
        ("Sats", &sat_str,       Rgb565::YELLOW),
        ("GPS",  &gps_str,       Rgb565::new(0, 60, 0)),
        ("GAL",  &gal_str,       Rgb565::new(0, 40, 31)),
        ("BDS",  &bds_str,       Rgb565::new(31, 50, 0)),
        ("GLO",  &glo_str,       Rgb565::new(31, 0, 25)),
        ("QZS",  &qzs_str,       Rgb565::new(31, 0, 31)),
        ("HDOP", &hdop_str,      Rgb565::WHITE),
        ("VDOP", &vdop_str,      Rgb565::WHITE),
        ("Alt",  &alt_str,       Rgb565::WHITE),
        ("UTC",  &gps.time_str,  Rgb565::new(10, 30, 20)),
    ];

    for (i, (label, value, col)) in lines.iter().enumerate() {
        let y = HEADER_H + 24 + i as i32 * 13;
        let text_key = format!("{}:{}", label, value);
        let changed = force || i >= prev_panel.len() || prev_panel[i] != text_key;
        if !changed { continue; }
        // Clear value area only
        let _ = Rectangle::new(Point::new(PANEL_X, y - 10), Size::new(98, 12))
            .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
            .draw(display);
        let s_lbl = MonoTextStyleBuilder::new()
            .font(&FONT_6X12).text_color(Rgb565::new(0, 40, 20)).build();
        let s_v = MonoTextStyleBuilder::new()
            .font(&FONT_6X12).text_color(*col).build();
        let _ = Text::new(label, Point::new(PANEL_X, y), s_lbl).draw(display);
        let _ = Text::new(value, Point::new(PANEL_X + 24, y), s_v).draw(display);
    }

    // Update prev_panel
    let new_panel: Vec<String> = lines.iter()
        .map(|(l, v, _)| format!("{}:{}", l, v))
        .collect();
    *prev_panel = new_panel;
}

/// Draw the static sky-view background: rings, cross-hair, cardinal labels.
pub(crate) fn skyview_draw_bg<D: DrawTarget<Color = Rgb565>>(
    display: &mut D,
    cx: i32, cy: i32, r: i32,
) {
    skyview_draw_rings(display, cx, cy, r);

    // Centre dot
    let _ = Circle::with_center(Point::new(cx, cy), 4)
        .into_styled(PrimitiveStyle::with_fill(Rgb565::new(5, 10, 5)))
        .draw(display);

    // Cardinal labels — placed just inside the outer ring to avoid clipping
    // N: above centre, S: below (but within 240px), E/W: left/right inside ring
    let lbl = MonoTextStyleBuilder::new()
        .font(&FONT_6X12).text_color(Rgb565::new(10, 20, 10)).build();
    let _ = Text::new("N", Point::new(cx - 3, HEADER_H + 1 + 12), lbl).draw(display);
    let _ = Text::new("S", Point::new(cx - 3, cy + r - 2), lbl).draw(display);
    let _ = Text::new("E", Point::new(cx + r - 10, cy + 4), lbl).draw(display);
    let _ = Text::new("W", Point::new(cx - r + 4, cy + 4), lbl).draw(display);

    // Elevation labels on east axis
    let r30 = r * 2 / 3;
    let r60 = r / 3;
    let elev_lbl = MonoTextStyleBuilder::new()
        .font(&FONT_6X12).text_color(Rgb565::new(5, 10, 5)).build();
    let _ = Text::new("30", Point::new(cx + r30 + 2, cy + 4), elev_lbl).draw(display);
    let _ = Text::new("60", Point::new(cx + r60 + 2, cy + 4), elev_lbl).draw(display);
}

/// Draw only the elevation rings (called after erasing dots to restore ring pixels).
pub(crate) fn skyview_draw_rings<D: DrawTarget<Color = Rgb565>>(
    display: &mut D,
    cx: i32, cy: i32, r: i32,
) {
    let ring_stroke = PrimitiveStyleBuilder::new()
        .stroke_color(Rgb565::new(5, 10, 5))
        .stroke_width(1)
        .build();
    let r30 = r * 2 / 3;
    let r60 = r / 3;
    let _ = Circle::with_center(Point::new(cx, cy), (r  * 2) as u32).into_styled(ring_stroke).draw(display);
    let _ = Circle::with_center(Point::new(cx, cy), (r30 * 2) as u32).into_styled(ring_stroke).draw(display);
    let _ = Circle::with_center(Point::new(cx, cy), (r60 * 2) as u32).into_styled(ring_stroke).draw(display);
}
