//! The main display: a MIPI-DCS smart panel behind the display controller (LCDC)
//! @0x08030000 (src/display_chipset_api.c).
//!
//! LCDC registers (from the driver @0x808A3E00..0x808A3F30):
//!   +0x00 W  DCS command byte          +0x04 W  parameter / pixel byte
//!   +0x08 W  read request (0x100)      +0x04 R  read data
//!   +0x18 W  bus / chip select         +0x1C R  status (bit31 idle, bit30 FIFO full,
//!                                                bit21 read data valid)
//! The OS initializes the panel with 0x11 (sleep out), 0x26 (gamma), 0x36 (MADCTL),
//! 0x3A = 6 (18 bpp), 0x2B rows 0..239, 0x2A columns 0..319, then 0x2C (memory write)
//! followed by 3 bytes per pixel (R, G, B, 6 significant bits each).
//!
//! This model keeps the panel's GRAM (320 x 240) and can dump it as a PNG.

use crate::machine::State;

pub const WIDTH: usize = 320;
pub const HEIGHT: usize = 240;
const LCDC: u64 = 0x0803_0000;
const REG_CMD: u64 = 0x00;
const REG_DATA: u64 = 0x04;

pub struct Panel {
    /// RGB888 GRAM.
    pub gram: Vec<u8>,
    cmd: u8,
    params: Vec<u8>,
    col: (usize, usize),
    row: (usize, usize),
    /// Write cursor (x, y) and partial pixel bytes.
    x: usize,
    y: usize,
    pix: Vec<u8>,
    bpp_bytes: usize,
    pub pixels_written: u64,
    /// Memory-write commands (0x2C) = frame starts.
    pub frames: u64,
    /// Completed frames whose content differed from / matched the previous one
    /// (hash of GRAM taken when the next frame starts).
    pub frames_changed: u64,
    pub frames_same: u64,
    last_hash: u64,
    /// icount when the last changed frame was detected.
    pub last_change_at: u64,
    pub icount_hint: u64,
    /// Bytes the panel returns on reads (+0x04) for the last read command.
    resp: std::collections::VecDeque<u8>,
    /// Save every changed frame as a PNG here (--frames).
    pub dump_dir: Option<String>,
}

/// Panel IDs answered to "read ID1/2/3" (0xDA/0xDB/0xDC). The driver (0x803746D2) only
/// checks that each is not 0, 0xFF or the command itself ("panel present"); the real
/// values are unknown, these are provisional.
pub const PANEL_IDS: [u8; 3] = [0x45, 0x4B, 0x04];

impl Default for Panel {
    fn default() -> Self {
        Self::new()
    }
}

impl Panel {
    pub fn new() -> Self {
        Panel {
            gram: vec![0; WIDTH * HEIGHT * 3],
            cmd: 0,
            params: Vec::new(),
            col: (0, WIDTH - 1),
            row: (0, HEIGHT - 1),
            x: 0,
            y: 0,
            pix: Vec::new(),
            bpp_bytes: 3,
            pixels_written: 0,
            frames: 0,
            frames_changed: 0,
            frames_same: 0,
            last_hash: 0,
            last_change_at: 0,
            icount_hint: 0,
            resp: Default::default(),
            dump_dir: None,
        }
    }

    fn command(&mut self, c: u8) {
        self.cmd = c;
        self.params.clear();
        self.pix.clear();
        self.resp.clear();
        if let 0xDA..=0xDC = c {
            // first byte read is a dummy, then the ID
            self.resp.extend([0, PANEL_IDS[(c - 0xDA) as usize]]);
        }
        match c {
            0x2C => {
                self.x = self.col.0;
                self.y = self.row.0;
                if self.frames > 0 {
                    self.finish_frame();
                }
                self.frames += 1;
            }
            0x3C => {} // memory write continue: keep the cursor
            _ => {}
        }
    }

    fn data(&mut self, b: u8) {
        match self.cmd {
            0x2C | 0x3C => self.pixel_byte(b),
            _ => {
                self.params.push(b);
                let p = &self.params;
                match (self.cmd, p.len()) {
                    (0x2A, 4) => self.col = (u16::from_be_bytes([p[0], p[1]]) as usize,
                                             u16::from_be_bytes([p[2], p[3]]) as usize),
                    (0x2B, 4) => self.row = (u16::from_be_bytes([p[0], p[1]]) as usize,
                                             u16::from_be_bytes([p[2], p[3]]) as usize),
                    // COLMOD: 6 = 18 bpp (3 bytes/pixel), 5 = 16 bpp (2 bytes/pixel)
                    (0x3A, 1) => self.bpp_bytes = if p[0] & 7 == 5 { 2 } else { 3 },
                    _ => {}
                }
            }
        }
    }

    fn pixel_byte(&mut self, b: u8) {
        self.pix.push(b);
        if self.pix.len() < self.bpp_bytes {
            return;
        }
        let (r, g, bl) = if self.bpp_bytes == 2 {
            let v = u16::from_be_bytes([self.pix[0], self.pix[1]]);
            (((v >> 11) << 3) as u8, (((v >> 5) & 0x3F) << 2) as u8, ((v & 0x1F) << 3) as u8)
        } else {
            (self.pix[0] & 0xFC, self.pix[1] & 0xFC, self.pix[2] & 0xFC)
        };
        self.pix.clear();
        if self.x < WIDTH && self.y < HEIGHT {
            let i = (self.y * WIDTH + self.x) * 3;
            self.gram[i..i + 3].copy_from_slice(&[r, g, bl]);
        }
        self.pixels_written += 1;
        self.x += 1;
        if self.x > self.col.1 {
            self.x = self.col.0;
            self.y += 1;
            if self.y > self.row.1 {
                self.y = self.row.0;
            }
        }
    }

    /// Classify the frame just completed as changed / identical to the previous one.
    fn finish_frame(&mut self) {
        // FNV-1a over the GRAM
        let mut h: u64 = 0xCBF2_9CE4_8422_2325;
        for &b in &self.gram {
            h = (h ^ b as u64).wrapping_mul(0x0100_0000_01B3);
        }
        if h == self.last_hash {
            self.frames_same += 1;
        } else {
            self.frames_changed += 1;
            self.last_change_at = self.icount_hint;
            if let Some(dir) = &self.dump_dir {
                let path = format!("{dir}/{:04}_{}M.png", self.frames_changed, self.icount_hint / 1_000_000);
                let _ = self.save_png(&path);
            }
        }
        self.last_hash = h;
    }

    pub fn save_png(&self, path: &str) -> Result<(), String> {
        let f = std::fs::File::create(path).map_err(|e| format!("{path}: {e}"))?;
        let mut enc = png::Encoder::new(std::io::BufWriter::new(f), WIDTH as u32, HEIGHT as u32);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().map_err(|e| e.to_string())?;
        w.write_image_data(&self.gram).map_err(|e| e.to_string())
    }
}

/// Bus read: data returned by the panel.
pub fn read(s: &mut State, addr: u64) -> Option<u32> {
    if addr == LCDC + REG_DATA {
        return Some(s.panel.resp.pop_front().unwrap_or(0) as u32);
    }
    None
}

/// Bus write: DCS command / data bytes.
pub fn write(s: &mut State, addr: u64, val: u32) {
    match addr.wrapping_sub(LCDC) {
        REG_CMD => {
            s.panel.icount_hint = s.icount;
            s.panel.command(val as u8)
        }
        REG_DATA => s.panel.data(val as u8),
        _ => {}
    }
}

/// LCDC data register written by the DMA controller: in DMA mode (LCDC +0x18 bit 31)
/// each 32-bit bus word is one xRGB8888 pixel (the OS frame buffer format), which the
/// LCDC sends to the panel in its configured pixel format.
pub fn write_dma(s: &mut State, addr: u64, val: u32, width: usize) {
    if addr.wrapping_sub(LCDC) != REG_DATA || width != 4 {
        return write(s, addr, val);
    }
    let (r, g, b) = ((val >> 16) as u8, (val >> 8) as u8, val as u8);
    if s.panel.bpp_bytes == 2 {
        let v = ((r as u16 >> 3) << 11) | ((g as u16 >> 2) << 5) | (b as u16 >> 3);
        s.panel.data((v >> 8) as u8);
        s.panel.data(v as u8);
    } else {
        s.panel.data(r);
        s.panel.data(g);
        s.panel.data(b);
    }
}
