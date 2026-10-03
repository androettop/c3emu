//! Capture of the OS's own debug/trace messages. The firmware's printf-style logging
//! functions are hooked (pass-through: the OS code still runs) and the message is
//! formatted on the host from the guest's format string and arguments.
//!
//! (address, index of the format argument) — chosen by call count in MCUSW:
//!   0x8083511C (fmt, ...)       5130 call sites
//!   0x80827F1C (id, fmt, ...)   1955 (the OS filters by a per-module enable bitmap)
//!   0x8082D40C (id, fmt, ...)   1029
//!   0x803A7B60 (fmt, ...)        309 (MTC / startup messages)
//!   0x80841630 (fmt, ...)            (energy management: BATTMGR / charger)
//!   0x807D6CF8 (fmt, ...)            (glue_audio)

use crate::uc::{RegisterARM, Uc};

use crate::machine::{Machine, State};
use crate::mmu::{read_virt, virt_word};

const LOGGERS: &[(u64, usize)] = &[(0x8083_511C, 0), (0x8082_7F1C, 1), (0x8082_D40C, 1), (0x803A_7B60, 0), (0x8084_1630, 0), (0x807D_6CF8, 0)];
pub const MAX_LINES: usize = 4000;

fn cstr(uc: &Uc<'_>, va: u32, max: usize) -> Option<String> {
    let mut out = Vec::new();
    let mut buf = [0u8; 64];
    let mut a = va;
    while out.len() < max {
        if !read_virt(uc, a, &mut buf) {
            break;
        }
        match buf.iter().position(|&c| c == 0) {
            Some(n) => {
                out.extend_from_slice(&buf[..n]);
                return Some(out.iter().map(|&c| if c == b'\n' || (0x20..0x7f).contains(&c) { c as char } else { '.' }).collect());
            }
            None => out.extend_from_slice(&buf),
        }
        a += 64;
    }
    if out.is_empty() { None } else { Some(out.iter().map(|&c| c as char).collect()) }
}

/// Argument `i` of the call (r0-r3, then the stack).
fn arg(uc: &Uc<'_>, i: usize) -> u32 {
    if i < 4 {
        uc.reg_read(i32::from(RegisterARM::R0) + i as i32).unwrap_or(0) as u32
    } else {
        let sp = uc.reg_read(RegisterARM::SP).unwrap_or(0) as u32;
        virt_word(uc, sp + 4 * (i as u32 - 4)).unwrap_or(0)
    }
}

/// Minimal printf: flags 0/-, width, l/h modifiers, d i u x X p c s %.
pub fn format(uc: &Uc<'_>, fmt: &str, mut next: usize) -> String {
    let mut out = String::new();
    let mut it = fmt.chars().peekable();
    while let Some(c) = it.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let mut zero = false;
        let mut left = false;
        let mut width = 0usize;
        while let Some(&f) = it.peek() {
            match f {
                '0' if width == 0 => zero = true,
                '-' => left = true,
                '0'..='9' => width = width * 10 + f.to_digit(10).unwrap() as usize,
                'l' | 'h' | '.' => {}
                _ => break,
            }
            it.next();
        }
        let conv = it.next().unwrap_or('%');
        let s = match conv {
            '%' => "%".to_string(),
            'd' | 'i' => { let v = arg(uc, next) as i32; next += 1; v.to_string() }
            'u' => { let v = arg(uc, next); next += 1; v.to_string() }
            'x' | 'p' => { let v = arg(uc, next); next += 1; format!("{v:x}") }
            'X' => { let v = arg(uc, next); next += 1; format!("{v:X}") }
            'c' => { let v = arg(uc, next); next += 1; ((v as u8) as char).to_string() }
            's' => { let v = arg(uc, next); next += 1; cstr(uc, v, 128).unwrap_or_else(|| "(?)".into()) }
            other => format!("%{other}"),
        };
        let pad = width.saturating_sub(s.len());
        if left {
            out.push_str(&s);
            out.extend(std::iter::repeat_n(' ', pad));
        } else {
            out.extend(std::iter::repeat_n(if zero { '0' } else { ' ' }, pad));
            out.push_str(&s);
        }
    }
    out.trim_end_matches(['\n', '\r']).to_string()
}

pub fn install(m: &mut Machine, echo: bool) {
    for &(addr, fmt_idx) in LOGGERS {
        m.hle_fn(addr, "oslog", Box::new(move |uc: &mut Uc<'_>, s: &mut State| {
            let fmt_ptr = arg(uc, fmt_idx);
            if let Some(fmt) = cstr(uc, fmt_ptr, 256) {
                let msg = format(uc, &fmt, fmt_idx + 1);
                if echo {
                    println!("[os {:>10}] {msg}", s.icount);
                }
                if s.oslog.len() == MAX_LINES {
                    s.oslog.pop_front();
                }
                s.oslog.push_back((s.icount, msg));
                s.oslog_total += 1;
            }
            None // pass-through: the OS's logger still runs
        }));
    }
}
