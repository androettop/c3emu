//! General-purpose DMA controller @0x08020000: an ARM PL080-style DMAC (driver
//! 0x80738xxx, ISR 0x807384D0 on INTC line 20). The display driver uses it to push the
//! frame buffer into the LCDC data register (channel 7: src = RAM, dst = 0x08030004).
//!
//!   +0x004 IntTCStatus   +0x008 IntTCClear (W1C)   +0x00C IntErrorStatus
//!   +0x010 IntErrClr     +0x014 RawIntTCStatus     +0x01C EnbldChns
//!   +0x030 Configuration (bit 0 = DMAC enable)
//!   +0x100 + 0x20*n: SrcAddr, DestAddr, LLI, Control, Configuration
//! Control: [11:0] transfer size, [20:18] src width, [23:21] dst width, bit 26 src
//! increment, bit 27 dst increment, bit 31 terminal-count interrupt. Configuration: bit 0
//! channel enable, bit 15 ITC (TC interrupt mask). LLI items: {src, dst, next, control}.
//!
//! A transfer runs to completion when the channel is enabled (the bus is not timed).

use crate::uc::Uc;

use crate::machine::State;

pub const DMAC_BASE: u64 = 0x0802_0000;
pub const DMAC_SIZE: u64 = 0x1_0000;
pub const DMAC_IRQ: u32 = 20;
const INT_TC_STATUS: u64 = 0x004;
const INT_TC_CLEAR: u64 = 0x008;
const RAW_INT_TC: u64 = 0x014;
const ENABLED_CHANNELS: u64 = 0x01C;
const CHANNELS: u64 = 0x100;
const CH_STRIDE: u64 = 0x20;
const CH_CONFIG: u64 = 0x10;
const CFG_ENABLE: u32 = 1;
const CFG_ITC: u32 = 1 << 15;
const CTRL_TC_INT: u32 = 1 << 31;

#[derive(Default)]
pub struct DmacState {
    pub tc_status: u32,
    pub transfers: u64,
    pub bytes: u64,
}

pub fn irq_level(s: &State) -> u32 {
    if s.dmac.tc_status != 0 { 1 << DMAC_IRQ } else { 0 }
}

pub fn read(s: &mut State, addr: u64) -> Option<u32> {
    match addr.checked_sub(DMAC_BASE).filter(|&o| o < DMAC_SIZE)? {
        0x000 | INT_TC_STATUS | RAW_INT_TC => Some(s.dmac.tc_status),
        ENABLED_CHANNELS => Some(0),
        _ => None,
    }
}

pub fn write(uc: &mut Uc<'_>, s: &mut State, addr: u64, val: u32) {
    let Some(off) = addr.checked_sub(DMAC_BASE).filter(|&o| o < DMAC_SIZE) else { return };
    if off == INT_TC_CLEAR {
        s.dmac.tc_status &= !val;
        return;
    }
    if !(CHANNELS..CHANNELS + 8 * CH_STRIDE).contains(&off) || (off - CHANNELS) % CH_STRIDE != CH_CONFIG {
        return;
    }
    if val & CFG_ENABLE == 0 {
        return;
    }
    let ch = ((off - CHANNELS) / CH_STRIDE) as u32;
    let base = DMAC_BASE + CHANNELS + ch as u64 * CH_STRIDE;
    let mut item = [0u32; 4];
    for (i, w) in item.iter_mut().enumerate() {
        *w = s.backing.read(base + 4 * i as u64, 4);
    }
    let mut tc_irq = false;
    let mut items = 0;
    loop {
        let [src, dst, next, ctrl] = item;
        run_item(uc, s, src, dst, ctrl);
        tc_irq |= ctrl & CTRL_TC_INT != 0;
        items += 1;
        if next == 0 || items > 4096 {
            break;
        }
        let mut b = [0u8; 16];
        if uc.mem_read(next as u64 & !3, &mut b).is_err() {
            break;
        }
        for (i, w) in item.iter_mut().enumerate() {
            *w = u32::from_le_bytes(b[4 * i..4 * i + 4].try_into().unwrap());
        }
    }
    s.dmac.transfers += 1;
    // channel done: it disables itself
    s.backing.write(addr, 4, val & !CFG_ENABLE);
    if tc_irq && val & CFG_ITC != 0 {
        s.dmac.tc_status |= 1 << ch;
    }
    if s.dmac.transfers <= 64 {
        s.event(format!("dma ch{ch}: {items} items, tc irq {}", tc_irq && val & CFG_ITC != 0));
    }
}

fn run_item(uc: &mut Uc<'_>, s: &mut State, src: u32, dst: u32, ctrl: u32) {
    let n = (ctrl & 0xFFF) as u64;
    let sw = 1u64 << ((ctrl >> 18) & 7).min(2);
    let dw = 1u64 << ((ctrl >> 21) & 7).min(2);
    let (si, di) = (ctrl & (1 << 26) != 0, ctrl & (1 << 27) != 0);
    let total = n * sw;
    let mut data = vec![0u8; total as usize];
    if si {
        let _ = uc.mem_read(src as u64, &mut data);
    } else {
        for i in 0..n as usize {
            let mut b = vec![0u8; sw as usize];
            let _ = uc.mem_read(src as u64, &mut b);
            data[i * sw as usize..(i + 1) * sw as usize].copy_from_slice(&b);
        }
    }
    s.dmac.bytes += total;
    let to_io = crate::bus::WINDOWS.iter().any(|w| (w.0..w.0 + w.1).contains(&(dst as u64)));
    if !to_io {
        if di {
            let _ = uc.mem_write(dst as u64, &data);
        } else if let Some(last) = data.chunks(dw as usize).last() {
            let _ = uc.mem_write(dst as u64, last);
        }
        return;
    }
    let mut a = dst as u64;
    for c in data.chunks(dw as usize) {
        let mut v = 0u32;
        for (i, b) in c.iter().enumerate() {
            v |= (*b as u32) << (8 * i);
        }
        crate::lcd::write_dma(s, a, v, dw as usize);
        if di {
            a += dw;
        }
    }
}
