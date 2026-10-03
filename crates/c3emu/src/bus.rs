//! Peripheral bus: the SoC register windows are Unicorn MMIO regions (no memory hooks,
//! so ordinary RAM accesses keep the fast path). Each access is offered to the device
//! models; registers no device claims behave as RAM (kept in a sparse backing store),
//! optionally overridden by the JSON MMIO model.

use crate::fxhash::FxHashMap as HashMap;

use crate::uc::Uc;

use crate::layout::*;
use crate::machine::State;

/// MMIO windows: (base, size, name used in the touch log).
pub const WINDOWS: &[(u64, u64, &str)] = &[
    (crate::dmac::DMAC_BASE, crate::dmac::DMAC_SIZE, "dmac"),
    (LCDC_LO, LCDC_HI - LCDC_LO, "lcdc"),
    (SOC_IO_LO, SOC_IO_HI - SOC_IO_LO, "soc"),
    (PERIPH, PERIPH_SZ, "periph"),
];

/// Sparse byte store for registers no device models.
#[derive(Default)]
pub struct Backing {
    pages: HashMap<u64, Box<[u8; 4096]>>,
}

impl Backing {
    pub fn read(&self, addr: u64, size: usize) -> u32 {
        let mut v = 0u32;
        for i in 0..size.min(4) {
            let a = addr + i as u64;
            let b = self.pages.get(&(a & !0xFFF)).map_or(0, |p| p[(a & 0xFFF) as usize]);
            v |= (b as u32) << (8 * i);
        }
        v
    }

    pub fn write(&mut self, addr: u64, size: usize, val: u32) {
        for i in 0..size.min(4) {
            let a = addr + i as u64;
            let p = self.pages.entry(a & !0xFFF).or_insert_with(|| Box::new([0; 4096]));
            p[(a & 0xFFF) as usize] = (val >> (8 * i)) as u8;
        }
    }
}

pub fn read(uc: &mut Uc<'_>, s: &mut State, region: &'static str, addr: u64, size: usize) -> u32 {
    let stored = s.backing.read(addr, size);
    let v = crate::soc::read(s, addr)
        .or_else(|| crate::i2c::read(s, addr))
        .or_else(|| crate::lcd::read(s, addr))
        .or_else(|| crate::sim::read(s, addr))
        .or_else(|| crate::dmac::read(s, addr))
        .or_else(|| crate::keypad::read(s, addr))
        .or_else(|| crate::gptimer::read(s, addr))
        .or_else(|| crate::gpio::read(s, addr))
        .or_else(|| crate::rtc::read(s, addr))
        .or_else(|| s.mmio_model.contains_key(&addr).then(|| s.mmio_value(addr, stored)))
        .unwrap_or(stored);
    let v = if size < 4 { v & ((1u32 << (8 * size)) - 1) } else { v };
    log(uc, s, region, 'R', addr, size, v);
    v
}

pub fn write(uc: &mut Uc<'_>, s: &mut State, region: &'static str, addr: u64, size: usize, val: u32) {
    s.backing.write(addr, size, val);
    log(uc, s, region, 'W', addr, size, val);
    crate::soc::write(s, addr, val);
    crate::i2c::write(s, addr, val);
    crate::lcd::write(s, addr, val);
    crate::sim::write(s, addr, val);
    crate::dmac::write(uc, s, addr, val);
    crate::gptimer::write(s, addr, val);
    crate::gpio::write(s, addr, val);
    crate::dsp::write(uc, s, addr);
}

fn log(uc: &mut Uc<'_>, s: &mut State, region: &'static str, op: char, addr: u64, size: usize, v: u32) {
    let pc = uc.reg_read(crate::uc::RegisterARM::PC).unwrap_or(0);
    s.touch(region, op, addr, v as u64, pc);
    if s.iolog.iter().any(|&(lo, hi)| (lo..hi).contains(&addr)) {
        s.event(format!("io {op}{size} 0x{addr:08x} = 0x{v:x} pc=0x{pc:08x}"));
    }
}
