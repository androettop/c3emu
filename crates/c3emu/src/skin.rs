//! The phone around the screen, as in device simulators: the C3-00 shell image (with
//! a transparent window for the display), drawn at any window size, and the areas of
//! its drawn keys (clickable). The web page uses the same geometry (www/app.js).

/// The shell (803 x 1605, RGBA) and the display window in it.
pub const SHELL_PNG: &[u8] = include_bytes!("../assets/shell.png");
pub const SHELL_W: usize = 803;
pub const SHELL_H: usize = 1605;
/// Display window: x, y, width, height (4:3, the 320 x 240 panel).
pub const SCREEN: (f32, f32, f32, f32) = (56.0, 221.0, 690.0, 517.0);

/// Clickable keys drawn on the shell: (x, y, w, h) in shell pixels, keypad index
/// (row * 8 + column; 255 = red key).
pub fn hotspots() -> Vec<(f32, f32, f32, f32, u8)> {
    let mut v = vec![
        (100.0, 856.0, 115.0, 42.0, 29), (585.0, 856.0, 115.0, 42.0, 43), // soft keys (short lines)
        (36.0, 912.0, 224.0, 56.0, 38), (536.0, 912.0, 232.0, 56.0, 52),  // contacts, messaging
        (118.0, 984.0, 84.0, 48.0, 30), (602.0, 984.0, 84.0, 48.0, 255),  // call, end / power
        (340.0, 880.0, 120.0, 115.0, 60),                                  // D-pad centre
        (330.0, 845.0, 140.0, 35.0, 56), (330.0, 997.0, 140.0, 42.0, 57), // up, down
        (298.0, 880.0, 42.0, 115.0, 58), (460.0, 880.0, 42.0, 115.0, 59), // left, right
    ];
    const QWERTY: [(f32, f32, [u8; 10]); 4] = [
        (1058.0, 92.0, [0, 1, 2, 3, 4, 5, 6, 24, 25, 26]),
        (1150.0, 112.0, [8, 9, 10, 11, 12, 13, 14, 32, 33, 34]),
        (1262.0, 110.0, [16, 17, 18, 19, 20, 21, 22, 45, 28, 42]),
        (1372.0, 122.0, [50, 35, 48, 49, 49, 49, 49, 40, 41, 27]),
    ];
    for (y, h, keys) in QWERTY {
        let mut c = 0;
        while c < 10 {
            let span = if keys[c] == 49 { 4 } else { 1 };
            v.push((42.0 + c as f32 * 72.3, y, 72.3 * span as f32, h, keys[c]));
            c += span;
        }
    }
    v
}

/// The shell scaled for one window size.
struct Layout {
    win: (usize, usize),
    scale: f32,
    ox: f32,
    oy: f32,
    /// Premultiplied 0x00RRGGBB and alpha per window pixel.
    rgb: Vec<u32>,
    alpha: Vec<u8>,
}

pub struct Skin {
    rgba: Vec<u8>,
    layout: Option<Layout>,
    pub background: u32,
    hot: Vec<(f32, f32, f32, f32, u8)>,
}

impl Skin {
    pub fn new() -> Result<Skin, String> {
        let dec = png::Decoder::new(std::io::Cursor::new(SHELL_PNG));
        let mut r = dec.read_info().map_err(|e| e.to_string())?;
        let mut buf = vec![0; r.output_buffer_size().ok_or("shell.png: size")?];
        let info = r.next_frame(&mut buf).map_err(|e| e.to_string())?;
        if info.color_type != png::ColorType::Rgba || info.width as usize != SHELL_W {
            return Err("shell.png: expected 803 px wide RGBA".into());
        }
        buf.truncate(info.buffer_size());
        Ok(Skin { rgba: buf, layout: None, background: 0x1b1d22, hot: hotspots() })
    }

    fn layout(&mut self, w: usize, h: usize) -> &Layout {
        if self.layout.as_ref().is_none_or(|l| l.win != (w, h)) {
            let scale = (w as f32 / SHELL_W as f32).min(h as f32 / SHELL_H as f32);
            let ox = (w as f32 - SHELL_W as f32 * scale) / 2.0;
            let oy = (h as f32 - SHELL_H as f32 * scale) / 2.0;
            let mut rgb = vec![0u32; w * h];
            let mut alpha = vec![0u8; w * h];
            // area-averaged downscale / bilinear upscale of the premultiplied shell
            let step = 1.0 / scale;
            let taps = step.ceil().max(1.0) as usize;
            for y in 0..h {
                for x in 0..w {
                    let sx0 = (x as f32 - ox) * step;
                    let sy0 = (y as f32 - oy) * step;
                    if sx0 < 0.0 || sy0 < 0.0 || sx0 >= SHELL_W as f32 || sy0 >= SHELL_H as f32 {
                        continue;
                    }
                    let (mut r, mut g, mut b, mut a) = (0f32, 0f32, 0f32, 0f32);
                    let mut n = 0f32;
                    for ty in 0..taps {
                        for tx in 0..taps {
                            let sx = (sx0 + (tx as f32 + 0.5) * step / taps as f32) as usize;
                            let sy = (sy0 + (ty as f32 + 0.5) * step / taps as f32) as usize;
                            if sx >= SHELL_W || sy >= SHELL_H {
                                continue;
                            }
                            let p = &self.rgba[(sy * SHELL_W + sx) * 4..][..4];
                            let pa = p[3] as f32 / 255.0;
                            r += p[0] as f32 * pa;
                            g += p[1] as f32 * pa;
                            b += p[2] as f32 * pa;
                            a += pa;
                            n += 1.0;
                        }
                    }
                    if n == 0.0 {
                        continue;
                    }
                    let i = y * w + x;
                    rgb[i] = ((r / n) as u32) << 16 | ((g / n) as u32) << 8 | (b / n) as u32;
                    alpha[i] = (a / n * 255.0).round() as u8;
                }
            }
            self.layout = Some(Layout { win: (w, h), scale, ox, oy, rgb, alpha });
        }
        self.layout.as_ref().unwrap()
    }

    /// Draw the phone (shell over the 320 x 240 RGB display `gram`) into `buf` (w x h,
    /// 0x00RRGGBB); `pressed`: highlight that key's area.
    pub fn render(&mut self, gram: &[u8], w: usize, h: usize, buf: &mut Vec<u32>, pressed: Option<u8>) {
        let bg = self.background;
        let hot: Vec<_> = self.hot.iter().filter(|k| Some(k.4) == pressed).copied().collect();
        let l = self.layout(w, h);
        buf.resize(w * h, 0);
        let (sx, sy, sw, sh) = SCREEN;
        let (x0, y0) = (l.ox + sx * l.scale, l.oy + sy * l.scale);
        let (fx, fy) = (320.0 / (sw * l.scale), 240.0 / (sh * l.scale));
        let px = |gx: f32, gy: f32| -> (f32, f32, f32) {
            // bilinear sample of the panel
            let gx = (gx - 0.5).clamp(0.0, 319.0);
            let gy = (gy - 0.5).clamp(0.0, 239.0);
            let (ix, iy) = (gx as usize, gy as usize);
            let (ax, ay) = (gx - ix as f32, gy - iy as f32);
            let at = |x: usize, y: usize, c: usize| gram[(y.min(239) * 320 + x.min(319)) * 3 + c] as f32;
            let mut o = [0f32; 3];
            for (c, v) in o.iter_mut().enumerate() {
                let top = at(ix, iy, c) * (1.0 - ax) + at(ix + 1, iy, c) * ax;
                let bot = at(ix, iy + 1, c) * (1.0 - ax) + at(ix + 1, iy + 1, c) * ax;
                *v = top * (1.0 - ay) + bot * ay;
            }
            (o[0], o[1], o[2])
        };
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                let a = l.alpha[i];
                if a == 255 {
                    buf[i] = l.rgb[i];
                    continue;
                }
                let (fxp, fyp) = (x as f32 + 0.5 - x0, y as f32 + 0.5 - y0);
                let under = if fxp >= 0.0 && fyp >= 0.0 && fxp < sw * l.scale && fyp < sh * l.scale {
                    px(fxp * fx, fyp * fy)
                } else {
                    (((bg >> 16) & 255) as f32, ((bg >> 8) & 255) as f32, (bg & 255) as f32)
                };
                let k = 1.0 - a as f32 / 255.0;
                let s = l.rgb[i];
                let r = ((s >> 16) & 255) as f32 + under.0 * k;
                let g = ((s >> 8) & 255) as f32 + under.1 * k;
                let b = (s & 255) as f32 + under.2 * k;
                buf[i] = (r.min(255.0) as u32) << 16 | (g.min(255.0) as u32) << 8 | b.min(255.0) as u32;
            }
        }
        // pressed key: a light tint over its area
        for (hx, hy, hw, hh, _) in hot {
            let xa = (l.ox + hx * l.scale) as usize;
            let ya = (l.oy + hy * l.scale) as usize;
            let xb = ((l.ox + (hx + hw) * l.scale) as usize).min(w);
            let yb = ((l.oy + (hy + hh) * l.scale) as usize).min(h);
            for y in ya..yb {
                for x in xa..xb {
                    let p = buf[y * w + x];
                    let mix = |c: u32| (c + (255 - c) / 4).min(255);
                    buf[y * w + x] = mix((p >> 16) & 255) << 16 | mix((p >> 8) & 255) << 8 | mix(p & 255);
                }
            }
        }
    }

    /// The key under window position (x, y), for the last rendered size.
    pub fn hit(&self, x: f32, y: f32) -> Option<u8> {
        let l = self.layout.as_ref()?;
        let (sx, sy) = ((x - l.ox) / l.scale, (y - l.oy) / l.scale);
        self.hot.iter().find(|k| sx >= k.0 && sx < k.0 + k.2 && sy >= k.1 && sy < k.1 + k.3).map(|k| k.4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_and_hit() {
        let mut s = Skin::new().unwrap();
        let gram: Vec<u8> = (0..320 * 240).flat_map(|i| [(i % 320) as u8, (i / 320) as u8, 128]).collect();
        for (w, h) in [(442usize, 883usize), (900, 700)] {
            let mut buf = Vec::new();
            s.render(&gram, w, h, &mut buf, Some(43));
            if let Ok(dir) = std::env::var("SKIN_OUT") {
                let rgb: Vec<u8> = buf.iter().flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8]).collect();
                let f = std::fs::File::create(format!("{dir}/skin_{w}x{h}.png")).unwrap();
                let mut e = png::Encoder::new(f, w as u32, h as u32);
                e.set_color(png::ColorType::Rgb);
                e.write_header().unwrap().write_image_data(&rgb).unwrap();
            }
        }
        // window 900 x 700: scale 700/1605, centred horizontally
        let sc = 700.0 / 1605.0;
        let ox = (900.0 - 803.0 * sc) / 2.0;
        assert_eq!(s.hit(ox + 150.0 * sc, 877.0 * sc), Some(29)); // left soft key line
        assert_eq!(s.hit(ox + 100.0 * sc, 940.0 * sc), Some(38)); // contacts bar
        assert_eq!(s.hit(ox + 400.0 * sc, 937.0 * sc), Some(60)); // OK
        assert_eq!(s.hit(ox + 400.0 * sc, 400.0 * sc), None); // screen
    }
}
